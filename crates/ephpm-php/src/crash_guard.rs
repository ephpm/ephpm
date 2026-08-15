//! Stack-overflow crash containment for the single-process ZTS model.
//!
//! Off unless `[php] crash_containment = true` **and** `[php] fpm_engine =
//! "pool"` (see [`set_enabled`]). When off, [`run_guarded`] is never called, no
//! guard is ever armed, and the fatal-signal handler's recovery hook is not
//! even registered — a crash kills the process exactly as it always has.
//!
//! # The problem
//!
//! ePHPm runs every tenant's PHP in one process on ZTS threads. A single
//! hostile (or buggy) tenant can crash the **whole** process — taking every
//! other tenant down — with a deep object-graph free: building a ~50 000-node
//! plain-object linked list and dropping it recurses in C
//! (`zend_object_std_dtor` <-> `zend_objects_store_del`), overflows the thread
//! stack, and the resulting SIGSEGV aborts the process. No `disable_functions`
//! or ini reaches it, because the trigger is ordinary object creation + scope
//! exit and the fault is in the C runtime below the VM's stack guard. (ePHPm
//! disables that guard anyway — `EG(max_allowed_stack_size) = 0` — and even
//! enabled it would not fire, because a plain-object destructor cascade passes
//! no VM/`zend_call_function` checkpoint.)
//!
//! # The mechanism
//!
//! This module contains exactly that fault class and nothing else:
//!
//! 1. [`run_guarded`] establishes a per-thread `sigsetjmp` recovery point (in
//!    the pure-libc C shim `crash_guard.c`) and runs the request inside it.
//! 2. The process fatal-signal handler ([`crate`]'s sibling in the `ephpm`
//!    binary, `src/fatal_signal.rs`) calls [`recover_hook`] — i.e. the C
//!    `ephpm_guard_try_recover` — first thing for every SIGSEGV/SIGBUS. If the
//!    current thread is inside a guarded region **and** the faulting address is
//!    at/just below this thread's own stack low bound (a stack overflow), it
//!    `siglongjmp`s back into [`run_guarded`], which returns [`Guarded::Contained`].
//!    Otherwise it returns and the handler proceeds to its normal
//!    die-with-diagnostic path (a genuine fatal still kills the process).
//!
//! # The soundness boundary — stated precisely
//!
//! Recovering from SIGSEGV is undefined behaviour in general. This is sound for
//! **stack overflow only**, and only because that fault exhausts a
//! *thread-local* resource: the ePHPm crash it targets frees a plain-object
//! graph entirely within thread-local Zend state (the per-thread ZMM heap and
//! `EG(objects_store)`), and the ZMM allocates its chunks with `mmap`, not glibc
//! `malloc`, so no process-global allocator lock is held at the fault instant.
//! The rest of the process is intact.
//!
//! It is **NOT** sound for heap corruption / wild writes: a bad write may have
//! already corrupted another thread's or the shared allocator's memory, so
//! continuing is unsafe. Those produce the same SIGSEGV, and are told apart by
//! the fault address (see `crash_guard.c`): a wild write lands nowhere near this
//! thread's stack and is therefore **not** contained.
//!
//! # After containment: the thread is poisoned
//!
//! When [`run_guarded`] returns [`Guarded::Contained`] the recovery skipped the
//! rest of `zend_object_std_dtor`, so the thread's ZMM free lists and
//! `EG(objects_store)` are mid-mutation and inconsistent. That state is confined
//! to this one thread, but the thread must never run PHP again and must never be
//! torn down through `php_request_shutdown` / `ts_free_thread` (both walk the
//! poisoned heap and would re-crash).
//!
//! [`crate::PhpRuntime::execute`] therefore does three things on containment:
//! marks the thread poisoned (so it refuses further PHP), disarms the
//! [`crate`]-internal TLS retirement guard (so thread exit does **not** run
//! `ts_free_thread`), and latches a process-wide flag that makes
//! [`crate::PhpRuntime::shutdown`] skip `php_embed_shutdown()` — module
//! shutdown's `ts_free_id()` would walk the abandoned entry and abort. It
//! surfaces the request as [`crate::PhpError::Contained`].
//!
//! Actually *retiring* the OS thread is the caller's job, and is why
//! containment is gated on the dedicated FPM pool: `ephpm-server`'s
//! `fpm_pool::thread_main` sees `PhpError::Contained`, exits its job loop
//! **without** PHP teardown, and the pool spawns a replacement. On tokio's
//! shared `spawn_blocking` pool no such primitive exists — a poisoned thread
//! would stay in rotation failing every later request, which is why that
//! combination is refused at startup.

use std::sync::atomic::{AtomicBool, Ordering};

/// Master switch. `false` until the server explicitly turns containment on for
/// this process, which it does only for the one supported configuration
/// (`[php] crash_containment = true` + `fpm_engine = "pool"`).
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Turn crash containment on or off for this process.
///
/// Call once at startup, on the single-threaded path, **before** any PHP
/// request runs — and only after registering the recovery hook with the
/// fatal-signal handler, since arming a guard with no hook installed would let
/// a contained-class fault fall through to the die path.
///
/// This is the only thing that makes [`run_guarded`] establish a recovery
/// region; leaving it `false` keeps the request path byte-identical to a build
/// without this module.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Release);
}

/// Whether crash containment is on for this process. Read once per PHP request,
/// so a relaxed load (a single non-atomic-cost instruction on every target
/// ePHPm ships) is deliberate.
#[must_use]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

#[cfg(unix)]
mod imp {
    use std::os::raw::c_void;

    unsafe extern "C" {
        /// Run `work(ctx)` inside a stack-overflow recovery region. Returns 0
        /// on normal completion, 1 if a stack overflow was contained.
        fn ephpm_guard_run(work: extern "C" fn(*mut c_void), ctx: *mut c_void) -> i32;

        /// Classify a SIGSEGV/SIGBUS fault; recover (does not return) if it is a
        /// contained stack overflow, else return 0. Async-signal-safe.
        fn ephpm_guard_try_recover(fault_addr: usize) -> i32;

        /// Test-only: whether the current thread is inside a guarded region.
        #[cfg(test)]
        fn ephpm_guard_is_armed() -> i32;
    }

    /// Result of [`run_guarded`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Guarded {
        /// `work` ran to completion normally.
        Completed,
        /// A stack overflow was contained; `work` did **not** finish and the
        /// calling thread's PHP context is poisoned (see the module docs).
        Contained,
    }

    /// Run `work` inside the per-thread stack-overflow recovery region.
    ///
    /// On [`Guarded::Contained`], `work` was abandoned mid-flight by a
    /// `siglongjmp` out of the signal handler: any Rust value created **inside**
    /// `work` is leaked (its destructor does not run), which is acceptable for a
    /// contained crash. Keep `work` minimal — in ePHPm it wraps only the single
    /// `ephpm_execute_request` FFI call, so the leak is the abandoned Zend
    /// request, not Rust state.
    pub fn run_guarded<F: FnOnce()>(work: F) -> Guarded {
        // The closure is moved into an `Option` so the trampoline can take it by
        // value exactly once. A raw pointer to it crosses the C boundary.
        let mut slot: Option<F> = Some(work);

        extern "C" fn trampoline<F: FnOnce()>(ctx: *mut c_void) {
            // SAFETY: `ctx` is the `&mut Option<F>` handed to `ephpm_guard_run`
            // below; it is live for the whole call and touched by no other
            // thread. `take()` runs the closure at most once.
            let slot = unsafe { &mut *(ctx.cast::<Option<F>>()) };
            if let Some(work) = slot.take() {
                work();
            }
        }

        // SAFETY: `trampoline::<F>` matches the `void(*)(void*)` C signature and
        // `slot` outlives the call. `ephpm_guard_run` either returns normally or
        // (on containment) via `siglongjmp` back to its own `sigsetjmp`, which
        // unwinds only C frames it owns — never across this Rust frame.
        let ret = unsafe {
            ephpm_guard_run(trampoline::<F>, std::ptr::from_mut(&mut slot).cast::<c_void>())
        };
        if ret == 0 { Guarded::Completed } else { Guarded::Contained }
    }

    /// The async-signal-safe fault-classification hook to register with the
    /// process fatal-signal handler.
    ///
    /// The returned function takes a faulting address (`siginfo_t.si_addr`) and,
    /// if the current thread is inside [`run_guarded`] and the fault is a stack
    /// overflow, does **not** return (it recovers). Otherwise it returns and the
    /// caller must fall through to its normal fatal handling. Wire it once at
    /// startup, from the binary crate, into `fatal_signal::set_recover_hook`.
    #[must_use]
    pub fn recover_hook() -> unsafe extern "C" fn(usize) -> i32 {
        ephpm_guard_try_recover
    }

    #[cfg(test)]
    #[allow(clippy::undocumented_unsafe_blocks)]
    mod tests {
        use std::os::raw::c_void;
        use std::sync::atomic::{AtomicU32, Ordering};

        use libc::{c_int, siginfo_t};

        use super::*;

        // ── A self-contained test handler ────────────────────────────────────
        //
        // In these unit tests there is no `fatal_signal` handler (that lives in
        // the `ephpm` binary crate), so the test installs its own minimal
        // SIGSEGV/SIGBUS handler that does exactly what the real one will:
        //   - call the recover hook first (which does not return on a contained
        //     stack overflow);
        //   - if it returns, restore SIG_DFL and re-raise (a genuine fatal dies,
        //     preserving wait status) — mirroring fatal_signal::die.

        /// How many times the test handler was entered (per process).
        static HANDLER_HITS: AtomicU32 = AtomicU32::new(0);

        extern "C" fn test_handler(sig: c_int, info: *mut siginfo_t, _ctx: *mut c_void) {
            HANDLER_HITS.fetch_add(1, Ordering::SeqCst);
            if !info.is_null() {
                // si_addr is the faulting address (SA_SIGINFO).
                let addr = unsafe { (*info).si_addr() } as usize;
                // Recover if this is a contained stack overflow; does not return
                // in that case.
                let _ = unsafe { recover_hook()(addr) };
            }
            // Not contained: revert to default disposition and re-raise so the
            // (forked) process dies with the real signal, exactly like the
            // production die path.
            unsafe {
                let mut dfl: libc::sigaction = std::mem::zeroed();
                libc::sigemptyset(&raw mut dfl.sa_mask);
                dfl.sa_sigaction = libc::SIG_DFL;
                libc::sigaction(sig, &raw const dfl, std::ptr::null_mut());
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&raw mut set);
                libc::sigaddset(&raw mut set, sig);
                libc::pthread_sigmask(libc::SIG_UNBLOCK, &raw const set, std::ptr::null_mut());
                libc::raise(sig);
            }
        }

        /// Give the current thread a 32 KiB alternate signal stack (required to
        /// run a handler for a stack-overflow fault, where the normal stack is
        /// exhausted) and install `test_handler` for SIGSEGV + SIGBUS.
        fn install_test_handler() {
            unsafe {
                let size = 32 * 1024;
                let sp = libc::mmap(
                    std::ptr::null_mut(),
                    size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                );
                assert_ne!(sp, libc::MAP_FAILED, "altstack mmap failed");
                let ss = libc::stack_t { ss_sp: sp, ss_flags: 0, ss_size: size };
                assert_eq!(
                    libc::sigaltstack(&raw const ss, std::ptr::null_mut()),
                    0,
                    "sigaltstack failed"
                );

                let mut act: libc::sigaction = std::mem::zeroed();
                act.sa_sigaction = test_handler as *const () as usize;
                act.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
                libc::sigemptyset(&raw mut act.sa_mask);
                for sig in [libc::SIGSEGV, libc::SIGBUS] {
                    assert_eq!(
                        libc::sigaction(sig, &raw const act, std::ptr::null_mut()),
                        0,
                        "sigaction failed"
                    );
                }
            }
        }

        /// Recurse forever, touching a 1 KiB frame each level so the compiler
        /// cannot elide the frames or tail-call it away — guarantees a real
        /// stack-overflow guard-page fault. The unbounded recursion is the whole
        /// point (it overflows the stack), so the lint is expected.
        #[inline(never)]
        #[allow(unconditional_recursion)]
        fn blow_the_stack(depth: u64) -> u64 {
            let mut frame = [0u8; 1024];
            let idx = usize::try_from(depth % 1024).unwrap_or(0);
            // Volatile touch keeps the frame live and defeats tail-call opt.
            // `depth.to_le_bytes()[0]` is the low byte with no truncating cast.
            unsafe {
                std::ptr::write_volatile(&raw mut frame[idx], depth.to_le_bytes()[0]);
            }
            let deeper = blow_the_stack(depth.wrapping_add(1));
            // Use both results so nothing is dead.
            deeper.wrapping_add(u64::from(frame[0]))
        }

        // ── Test 1: the mechanism itself, in-process ─────────────────────────
        //
        // A real stack overflow inside run_guarded is contained, the thread
        // survives, and — because in stub mode there is no PHP heap to poison —
        // the SAME thread can run another guarded region afterward. This proves
        // the pure signal/setjmp/longjmp mechanism independent of libphp.
        #[test]
        fn stack_overflow_is_contained_and_thread_survives() {
            // Run on a dedicated std::thread with a known 8 MiB stack so the
            // overflow is bounded and reproducible.
            let handle = std::thread::Builder::new()
                .name("crashguard-test".into())
                .stack_size(8 * 1024 * 1024)
                .spawn(|| {
                    install_test_handler();

                    assert_eq!(unsafe { ephpm_guard_is_armed() }, 0, "should start disarmed");

                    let outcome = run_guarded(|| {
                        // Never returns normally: overflows the stack.
                        std::hint::black_box(blow_the_stack(std::hint::black_box(0)));
                    });
                    assert_eq!(outcome, Guarded::Contained, "overflow must be contained");
                    assert_eq!(
                        unsafe { ephpm_guard_is_armed() },
                        0,
                        "guard must be disarmed after recovery"
                    );

                    // The thread is alive and usable: a normal guarded region
                    // now completes.
                    let ran = std::cell::Cell::new(false);
                    let outcome2 = run_guarded(|| ran.set(true));
                    assert_eq!(outcome2, Guarded::Completed);
                    assert!(ran.get(), "post-recovery guarded work must run");

                    // And a SECOND overflow on the same thread is contained too
                    // (no one-shot latch left armed).
                    let outcome3 = run_guarded(|| {
                        std::hint::black_box(blow_the_stack(std::hint::black_box(0)));
                    });
                    assert_eq!(outcome3, Guarded::Contained, "second overflow must contain");

                    "survived"
                })
                .expect("spawn");

            assert_eq!(handle.join().expect("thread must not panic/abort"), "survived");
        }

        // ── Test 2: a normal (non-crashing) guarded region completes ─────────
        #[test]
        fn normal_work_completes_without_containment() {
            let n = std::cell::Cell::new(0u32);
            for _ in 0..1000 {
                let outcome = run_guarded(|| n.set(n.get() + 1));
                assert_eq!(outcome, Guarded::Completed);
            }
            assert_eq!(n.get(), 1000);
            assert_eq!(unsafe { ephpm_guard_is_armed() }, 0);
        }

        // ── Test 3: a wild write is NOT contained (it re-raises and dies) ────
        //
        // Forked child so the death is observable without killing the test
        // runner, mirroring fatal_signal's own fault-injection harness. A null
        // dereference inside a guarded region faults far from the stack, so the
        // classifier refuses to contain it; the test handler then re-raises and
        // the child must die with SIGSEGV.
        #[test]
        fn wild_write_inside_guard_is_not_contained() {
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0, "fork failed");

            if pid == 0 {
                // Child. Suppress core dumps, install the handler, fault.
                unsafe {
                    let no_core = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
                    libc::setrlimit(libc::RLIMIT_CORE, &raw const no_core);
                }
                install_test_handler();
                let _ = run_guarded(|| unsafe {
                    // Wild write through null — a genuine fatal, not an overflow.
                    std::ptr::write_volatile(std::ptr::null_mut::<u32>(), 0x41);
                });
                // Must not reach here: either the fault killed us, or (bug) it
                // was wrongly contained and run_guarded returned.
                unsafe { libc::_exit(97) };
            }

            let mut status: c_int = 0;
            unsafe { libc::waitpid(pid, &raw mut status, 0) };
            assert!(
                libc::WIFSIGNALED(status),
                "child should have died from a signal, not exited (was it wrongly contained?)"
            );
            assert_eq!(
                libc::WTERMSIG(status),
                libc::SIGSEGV,
                "a wild write must NOT be contained — it must re-raise and die with SIGSEGV"
            );
        }
    }
}

#[cfg(unix)]
pub use imp::{Guarded, recover_hook, run_guarded};

// ── Windows: no signals, no containment (single-process ZTS is Unix-only) ────

/// Result of [`run_guarded`] (Windows stub).
#[cfg(not(unix))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guarded {
    /// `work` ran to completion.
    Completed,
    /// Never produced on Windows.
    Contained,
}

/// Run `work` with no containment (Windows delivers faults as SEH exceptions,
/// not signals; the mechanism does not apply). Always [`Guarded::Completed`].
#[cfg(not(unix))]
pub fn run_guarded<F: FnOnce()>(work: F) -> Guarded {
    work();
    Guarded::Completed
}

/// No-op recovery hook on Windows.
#[cfg(not(unix))]
#[must_use]
pub fn recover_hook() -> unsafe extern "C" fn(usize) -> i32 {
    unsafe extern "C" fn noop(_addr: usize) -> i32 {
        0
    }
    noop
}

#[cfg(test)]
mod switch_tests {
    use super::{enabled, set_enabled};

    /// The master switch starts OFF on every platform, so a build that never
    /// calls [`set_enabled`] behaves exactly like one without this module. The
    /// test restores the flag so it cannot leak into a sibling test in the same
    /// process (issue #235).
    #[test]
    fn containment_switch_defaults_off_and_round_trips() {
        let original = enabled();
        assert!(!original, "containment must be off until the server turns it on");

        set_enabled(true);
        assert!(enabled());
        set_enabled(false);
        assert!(!enabled());

        set_enabled(original);
    }
}
