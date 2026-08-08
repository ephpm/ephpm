//! Fatal-signal diagnostics — capture, report, then die exactly as before.
//!
//! # Why this exists
//!
//! A native memory fault inside a PHP extension, a native middleware, or the
//! Zend engine itself takes the whole process down. Fault injection against a
//! PHP-linked build (14 cases, a real ZTS PHP extension and a native
//! middleware) established that ePHPm said **nothing** on the way out: a plain
//! null dereference produced total silence — no `tracing` line, no panic, no
//! signal notice. The only components that ever spoke were not ours (Rust
//! std's stack-overflow reporter, and glibc's `free(): double free detected`).
//! Recovery is fast — 61–90 ms to respawn — so availability was never the
//! problem. *Diagnosis* was: an operator got a restarted pod and no way to
//! tell whether an extension, a middleware, or PHP did it.
//!
//! This module closes that gap. It installs a handler for the synchronous
//! fatal signals, writes a compact report to stderr, and then dies with the
//! **original disposition** so the wait status is byte-identical to a build
//! without it. Kubernetes still sees `Error(139)`/`Error(134)`,
//! CrashLoopBackOff detection is unaffected, and any alerting keyed on exit
//! code keeps working.
//!
//! # What this is explicitly NOT
//!
//! It is not a recovery mechanism, and recovery must never be added here.
//! The measured reason: a Zend-memory-manager smash (`emalloc(16)` followed by
//! a 4 KiB `memset`) **returned HTTP 200 to its own client** and killed the
//! process 17 ms later on a *different* thread serving an innocent request.
//! There is no request to blame and no thread to isolate — the Zend memory
//! manager, the glibc arena and the OPcache SHM are process-wide and shared by
//! every ZTS thread. Once one of them is corrupt the process is not
//! recoverable, only reportable. So: no `siglongjmp`, no "continue anyway", no
//! swallowing.
//!
//! # Async-signal-safety
//!
//! Everything on the handler path is restricted to the async-signal-safe set:
//!
//! * Output is raw `write(2)` to fd 2. No `tracing`, no `log`, no `println!`,
//!   no `format!` — every one of those allocates or takes a lock, and the
//!   faulting thread may already hold the allocator lock (the glibc
//!   double-free case aborts from *inside* malloc, holding the arena lock).
//! * Integers and pointers are formatted by hand into a fixed stack buffer
//!   (`Buf`) that cannot allocate, cannot panic, and truncates instead of
//!   growing.
//! * Shared state is lock-free atomics only.
//! * The thread name comes from `prctl(PR_GET_NAME)` — a bare syscall — not
//!   from `pthread_getname_np`, which on glibc opens and reads
//!   `/proc/self/task/<tid>/comm`.
//!
//! Two calls are *not* strictly async-signal-safe and are used deliberately,
//! with mitigations:
//!
//! * `backtrace()` lazily loads and initialises libgcc's unwinder on first
//!   use, which can allocate. [`install`] therefore calls it once at startup
//!   ("warm-up") so the handler's call hits an already-initialised unwinder.
//! * `backtrace_symbols_fd()` does not allocate (unlike `backtrace_symbols`,
//!   which mallocs and is unusable here), but it resolves names through
//!   glibc's `_dl_addr`, which takes the dynamic-loader lock. A thread that
//!   faulted *inside* `dlopen` would deadlock there. That is why the raw
//!   return addresses are written **first**, by hand: even if symbolisation
//!   wedges, the operator already has frame addresses that `addr2line` can
//!   resolve offline. `install` also warms this path against `/dev/null`.
//!
//! # Chaining, not clobbering
//!
//! Rust std installs its own `SIGSEGV`/`SIGBUS` handler before `main` runs, to
//! turn a guard-page hit into the genuinely useful
//! `thread '<name>' has overflowed its stack` message. Losing that would be a
//! regression, so [`install`] saves the previous disposition and the handler
//! calls it after writing its own report. For a stack overflow std's handler
//! prints and aborts (status 134, unchanged); for any other fault it reverts
//! to `SIG_DFL` and returns, at which point this handler re-raises (status
//! 139, unchanged).
//!
//! # Platform
//!
//! Unix only. Windows delivers memory faults as SEH exceptions, not signals;
//! diagnosing them needs `AddVectoredExceptionHandler` plus `dbghelp` symbol
//! resolution, which shares no code with this module. That is deliberately out
//! of scope rather than half-implemented — on Windows every entry point here
//! compiles to a no-op.

#![allow(unsafe_code)]

/// Install the fatal-signal diagnostic handler for the current process.
///
/// Idempotent: only the first call has any effect. Call this as early as
/// possible in `main`, before any `dlopen` of a PHP extension or a native
/// middleware, so a fault raised during module initialisation is still
/// reported.
///
/// Set `EPHPM_FATAL_HANDLER=0` (or `off`/`false`/`no`) to skip installation.
/// The escape hatch exists because this code runs inside an already-corrupt
/// process; if it ever misbehaves in production an operator must be able to
/// turn it off without a new build.
///
/// On Windows this is a no-op.
pub fn install() {
    #[cfg(unix)]
    unix::install();
}

/// Give the calling thread an alternate signal stack if it does not already
/// have one.
///
/// Wired into the tokio runtime's `on_thread_start` hook so that every worker
/// and blocking-pool thread can run the handler even for a stack-overflow
/// fault, where the thread's own stack is exhausted and no handler can run
/// without `sigaltstack`.
///
/// In practice this is a no-op on every thread ePHPm creates: Rust std already
/// installs an alternate stack on the main thread and on every
/// `std::thread`-spawned thread (which is what tokio uses), and this function
/// deliberately leaves an existing one alone rather than replacing it with a
/// possibly smaller one. It exists for threads std did not create — those
/// spawned by C code inside a PHP extension or a linked library — and so that
/// the guarantee does not silently depend on a std implementation detail.
///
/// On Windows this is a no-op.
pub fn install_thread_altstack() {
    #[cfg(unix)]
    unix::install_thread_altstack();
}

#[cfg(unix)]
mod unix {
    use std::cell::RefCell;
    // Only the backtrace machinery uses `UnsafeCell` (for the frame array), so
    // on a target without `backtrace(3)` an ungated import is a dead one.
    #[cfg(has_execinfo)]
    use std::cell::UnsafeCell;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

    use libc::{c_int, siginfo_t};

    // `backtrace(3)` lives in glibc / Apple's libSystem / FreeBSD's
    // libexecinfo and is absent on musl. Declared here rather than taken from
    // the `libc` crate so the exact signature and the platform gate are visible
    // at the point of use. `backtrace_symbols_fd` is the malloc-free variant;
    // `backtrace_symbols` allocates and must never be called from a handler.
    #[cfg(has_execinfo)]
    unsafe extern "C" {
        fn backtrace(buf: *mut *mut c_void, sz: c_int) -> c_int;
        fn backtrace_symbols_fd(buf: *const *mut c_void, sz: c_int, fd: c_int);
    }

    /// `si_code` values, which the `libc` crate does not export.
    ///
    /// Taken from the kernel/libc headers rather than guessed, and split by
    /// platform because the numbering genuinely differs: on Linux
    /// `FPE_INTDIV` is 1, on Darwin it is 7. Getting this wrong would print a
    /// confident, wrong cause — worse than printing nothing.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    mod si {
        use libc::c_int;

        // asm-generic/siginfo.h
        pub const SI_USER: c_int = 0;
        pub const SI_KERNEL: c_int = 0x80;
        pub const SI_QUEUE: c_int = -1;
        /// What `abort(3)` actually produces on Linux: glibc routes it through
        /// `tgkill`, not `kill`. Every SIGABRT ePHPm will ever see - a Rust
        /// stack overflow, a glibc double-free, an extension calling `abort()` -
        /// arrives with this code, so leaving it out would have made the most
        /// common crash the one with an unlabelled cause.
        pub const SI_TKILL: c_int = -6;
        pub const SEGV_MAPERR: c_int = 1;
        pub const SEGV_ACCERR: c_int = 2;
        pub const BUS_ADRALN: c_int = 1;
        pub const BUS_ADRERR: c_int = 2;
        pub const BUS_OBJERR: c_int = 3;
        pub const ILL_ILLOPC: c_int = 1;
        pub const ILL_ILLOPN: c_int = 2;
        pub const ILL_PRVOPC: c_int = 5;
        pub const FPE_INTDIV: c_int = 1;
        pub const FPE_INTOVF: c_int = 2;
        pub const FPE_FLTDIV: c_int = 3;
    }

    /// `si_code` values, which the `libc` crate does not export.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    mod si {
        use libc::c_int;

        // sys/signal.h (Darwin/BSD)
        pub const SI_USER: c_int = 0x1_0001;
        pub const SEGV_MAPERR: c_int = 1;
        pub const SEGV_ACCERR: c_int = 2;
        pub const BUS_ADRALN: c_int = 1;
        pub const BUS_ADRERR: c_int = 2;
        pub const BUS_OBJERR: c_int = 3;
        pub const ILL_ILLOPC: c_int = 1;
        pub const ILL_ILLOPN: c_int = 4;
        pub const ILL_PRVOPC: c_int = 3;
        pub const FPE_INTDIV: c_int = 7;
        pub const FPE_INTOVF: c_int = 8;
        pub const FPE_FLTDIV: c_int = 1;
    }

    /// Signals handled. All five are *synchronous* faults or deliberate
    /// self-termination — none of them is a routine control signal, so
    /// handling them cannot interfere with graceful shutdown (`SIGTERM`,
    /// `SIGINT`) or PHP's `SIGPROF`-based execution timer (already neutralised
    /// by the `--wrap` linker flags in this crate's `build.rs`).
    const FATAL: [c_int; 5] =
        [libc::SIGSEGV, libc::SIGBUS, libc::SIGILL, libc::SIGFPE, libc::SIGABRT];

    /// Maximum captured stack frames. Kept modest on purpose: the handler may
    /// be running on an alternate signal stack that Rust std sized at roughly
    /// one page plus `SIGSTKSZ`, and a stack-overflow fault is exactly the case
    /// where that budget is tightest.
    #[cfg(has_execinfo)]
    const MAX_FRAMES: usize = 64;

    /// [`MAX_FRAMES`] in the type `backtrace(3)` wants, without a lossy cast.
    #[cfg(has_execinfo)]
    const MAX_FRAMES_C: c_int = 64;

    #[cfg(has_execinfo)]
    const _: () = assert!(MAX_FRAMES_C as usize == MAX_FRAMES);

    /// Alternate-stack size used only when a thread has none. 32 KiB is
    /// comfortably above `SIGSTKSZ` on every supported platform and leaves room
    /// for the chained std handler on top of this one.
    const ALT_STACK_SIZE: usize = 32 * 1024;

    /// Handler phase, so a *second* entry can describe itself accurately
    /// instead of guessing.
    mod phase {
        /// Nobody is in the handler.
        pub const IDLE: u32 = 0;
        /// Writing the diagnostic report.
        pub const REPORTING: u32 = 1;
        /// Inside the previously installed (std) handler.
        pub const CHAINING: u32 = 2;
        /// Restoring `SIG_DFL` and re-raising.
        pub const DYING: u32 = 3;
    }

    /// Previous `sa_sigaction` for each signal in [`FATAL`], as a raw
    /// `sighandler_t`. `0` is `SIG_DFL`, `1` is `SIG_IGN`; anything else is a
    /// function pointer we chain to. Atomics rather than a stored `sigaction`
    /// because a signal handler must not take a lock to read them, and because
    /// `libc::sigaction` contains a private-field `sigset_t` that cannot be
    /// const-constructed for a `static`.
    static PREV_HANDLER: [AtomicUsize; FATAL.len()] = [const { AtomicUsize::new(0) }; FATAL.len()];

    /// Whether the previous handler was registered with `SA_SIGINFO`, which
    /// decides the signature we must call it through.
    static PREV_SIGINFO: [AtomicBool; FATAL.len()] =
        [const { AtomicBool::new(false) }; FATAL.len()];

    /// Thread that currently owns the handler, stored as `tid + 1` so that `0`
    /// unambiguously means "nobody".
    static OWNER: AtomicU64 = AtomicU64::new(0);

    /// What the owning thread is doing — see [`phase`].
    static PHASE: AtomicU32 = AtomicU32::new(phase::IDLE);

    /// Installed exactly once.
    static INSTALLED: AtomicBool = AtomicBool::new(false);

    /// Captured return addresses. A `static` rather than a local so the
    /// handler's own frame stays small enough for a cramped alternate stack;
    /// exclusive access is guaranteed by the [`OWNER`] gate, which admits one
    /// thread only.
    ///
    /// Gated with the rest of the backtrace machinery: without `backtrace(3)`
    /// nothing ever fills it, and an ungated dead `static` fails the workspace's
    /// `-D warnings` on a musl build.
    #[cfg(has_execinfo)]
    static FRAMES: SyncCell<[*mut c_void; MAX_FRAMES]> =
        SyncCell(UnsafeCell::new([std::ptr::null_mut(); MAX_FRAMES]));

    /// `Sync` wrapper for handler-owned mutable statics.
    #[cfg(has_execinfo)]
    struct SyncCell<T>(UnsafeCell<T>);

    // SAFETY: the contained value is only ever touched by the single thread
    // that won the `OWNER` compare-exchange, and that thread never releases
    // ownership — the process terminates instead. No concurrent access is
    // therefore possible, which is what `Sync` requires.
    #[cfg(has_execinfo)]
    unsafe impl<T> Sync for SyncCell<T> {}

    pub(super) fn install() {
        if !enabled() {
            return;
        }
        if INSTALLED.swap(true, Ordering::SeqCst) {
            return;
        }

        warm_up();
        install_thread_altstack();

        for (idx, &sig) in FATAL.iter().enumerate() {
            // SAFETY: `new` and `old` are zeroed then fully initialised before
            // use. `sigaction` with a valid signal number and valid `new`/`old`
            // pointers is defined behaviour. `install` runs at the top of
            // `main` while the process is still single-threaded, so no other
            // thread can observe a half-installed state.
            unsafe {
                let mut new: libc::sigaction = std::mem::zeroed();
                let mut old: libc::sigaction = std::mem::zeroed();

                libc::sigemptyset(&raw mut new.sa_mask);
                // `sa_sigaction` is an opaque `sighandler_t` (a `usize`), so
                // the handler address goes in via a pointer cast rather than a
                // direct function-to-integer cast.
                new.sa_sigaction = handler as *const () as usize;
                // SA_SIGINFO: we need `si_addr` and `si_code`.
                // SA_ONSTACK: run on the alternate signal stack, without which
                //   a stack-overflow fault cannot be reported at all.
                // Deliberately no SA_NODEFER: leaving the signal blocked for
                //   the duration of the handler is what stops a fault *inside*
                //   the handler from recursing without bound.
                new.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;

                if libc::sigaction(sig, &raw const new, &raw mut old) == 0 {
                    PREV_HANDLER[idx].store(old.sa_sigaction, Ordering::SeqCst);
                    PREV_SIGINFO[idx].store(old.sa_flags & libc::SA_SIGINFO != 0, Ordering::SeqCst);
                }
            }
        }
    }

    /// Honour the `EPHPM_FATAL_HANDLER` escape hatch.
    fn enabled() -> bool {
        match std::env::var("EPHPM_FATAL_HANDLER") {
            Ok(v) => !is_falsey(&v),
            Err(_) => true,
        }
    }

    fn is_falsey(value: &str) -> bool {
        matches!(value.trim().to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no")
    }

    /// Force the lazy initialisation that `backtrace` and
    /// `backtrace_symbols_fd` would otherwise perform on their first call —
    /// which, in a handler, would be a first call inside an already broken
    /// process.
    ///
    /// `backtrace()` loads libgcc's unwinder and populates its FDE lookup
    /// caches; `backtrace_symbols_fd()` walks the loader's link map through
    /// `_dl_addr`. Doing both here, at startup, on a healthy process, is the
    /// difference between "may allocate under the faulting thread's allocator
    /// lock" and "touches only memory it already owns".
    #[cfg(has_execinfo)]
    fn warm_up() {
        let mut scratch: [*mut c_void; 8] = [std::ptr::null_mut(); 8];

        // SAFETY: `scratch` is a valid, correctly sized array of pointers and
        // the length passed matches it. `backtrace` writes at most that many
        // entries and returns how many it wrote.
        let n = unsafe { backtrace(scratch.as_mut_ptr(), 8) };
        if n <= 0 {
            return;
        }

        // SAFETY: a NUL-terminated literal path with standard open flags.
        let devnull = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY) };
        if devnull >= 0 {
            // SAFETY: `scratch` holds `n` valid entries (0 < n <= 8) and
            // `devnull` is an open, writable descriptor. The output is
            // discarded; we only want the loader-side caches warm.
            unsafe { backtrace_symbols_fd(scratch.as_ptr(), n, devnull) };
            // SAFETY: `devnull` is an open descriptor this function owns.
            unsafe { libc::close(devnull) };
        }
    }

    /// No unwinder on this target, so there is nothing to warm.
    #[cfg(not(has_execinfo))]
    fn warm_up() {}

    pub(super) fn install_thread_altstack() {
        thread_local! {
            /// Owns the mmap'd alternate stack for threads that needed one, and
            /// disables + unmaps it at thread exit so a later signal cannot land
            /// on freed memory.
            static ALT_STACK: RefCell<Option<AltStack>> = const { RefCell::new(None) };
        }

        struct AltStack {
            ptr: *mut c_void,
            size: usize,
        }

        impl Drop for AltStack {
            fn drop(&mut self) {
                // SAFETY: the alternate stack is disabled *before* the mapping
                // is returned to the kernel, so no signal can be delivered onto
                // memory that is about to be unmapped. `ss_sp` and `ss_size`
                // are ignored by the kernel when `SS_DISABLE` is set. `self.ptr`
                // and `self.size` are the exact values from the `mmap` below,
                // which nothing else has unmapped.
                unsafe {
                    let disable = libc::stack_t {
                        ss_sp: std::ptr::null_mut(),
                        ss_flags: libc::SS_DISABLE,
                        ss_size: 0,
                    };
                    libc::sigaltstack(&raw const disable, std::ptr::null_mut());
                    libc::munmap(self.ptr, self.size);
                }
            }
        }

        // SAFETY: querying with a null `new` pointer is the documented way to
        // read the current alternate stack without changing it; `cur` is a
        // valid, zeroed out-parameter.
        let current = unsafe {
            let mut cur: libc::stack_t = std::mem::zeroed();
            if libc::sigaltstack(std::ptr::null(), &raw mut cur) != 0 {
                return;
            }
            cur
        };

        // Leave an existing alternate stack alone. Rust std installs one on
        // every thread it creates and sizes it for its own stack-overflow
        // reporter; replacing it with ours could only make that worse.
        if current.ss_flags & libc::SS_DISABLE == 0 && !current.ss_sp.is_null() {
            return;
        }

        // SAFETY: an anonymous private mapping of a fixed size, with no fixed
        // address requested. `mmap` reports failure as `MAP_FAILED`, which is
        // checked before the pointer is used.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                ALT_STACK_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return;
        }

        // SAFETY: `ptr` is a live mapping of exactly `ALT_STACK_SIZE` bytes
        // owned by this thread; the `AltStack` guard stored below keeps it
        // mapped for as long as the kernel may write to it.
        let installed = unsafe {
            let stack = libc::stack_t { ss_sp: ptr, ss_flags: 0, ss_size: ALT_STACK_SIZE };
            libc::sigaltstack(&raw const stack, std::ptr::null_mut()) == 0
        };

        if installed {
            ALT_STACK.with(|slot| {
                *slot.borrow_mut() = Some(AltStack { ptr, size: ALT_STACK_SIZE });
            });
        } else {
            // SAFETY: the mapping was never handed to the kernel as a signal
            // stack and nothing else refers to it, so unmapping it here is
            // unobservable.
            unsafe { libc::munmap(ptr, ALT_STACK_SIZE) };
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // The handler
    // ─────────────────────────────────────────────────────────────────────────

    /// The fatal-signal handler.
    ///
    /// Everything reachable from here is async-signal-safe (see the module
    /// docs). It never returns in a healthy sense — either the chained handler
    /// terminates the process, or [`die`] re-raises with the default
    /// disposition.
    extern "C" fn handler(sig: c_int, info: *mut siginfo_t, ctx: *mut c_void) {
        let me = thread_id().wrapping_add(1);

        match OWNER.compare_exchange(0, me, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => {
                PHASE.store(phase::REPORTING, Ordering::SeqCst);
                report(sig, info);
                die(sig, info, ctx, true);
            }
            Err(owner) if owner == me => {
                // Re-entered on the same thread. Which of the three ways it can
                // happen matters a lot to whoever reads this, so say which.
                match PHASE.load(Ordering::SeqCst) {
                    phase::CHAINING => {
                        // Expected for a stack overflow: Rust std's handler
                        // prints its message and calls abort(), which arrives
                        // here as SIGABRT. Not an error.
                        write_stderr(b"[ephpm] ");
                        write_stderr(signal_name(sig).as_bytes());
                        write_stderr(
                            b" raised by the chained handler \
                              (expected after a stack overflow) - terminating\n",
                        );
                    }
                    phase::DYING => {
                        write_stderr(b"[ephpm] fault while re-raising - terminating\n");
                    }
                    _ => {
                        // The case worth knowing about: the diagnostic path
                        // itself faulted. Say so plainly rather than looping.
                        write_stderr(b"[ephpm] the fatal-signal handler itself faulted (");
                        write_stderr(signal_name(sig).as_bytes());
                        write_stderr(
                            b") - diagnostics are incomplete; \
                              set EPHPM_FATAL_HANDLER=0 to disable it\n",
                        );
                    }
                }
                die(sig, info, ctx, false);
            }
            Err(_) => {
                // A second thread faulted while the first was still reporting.
                // Interleaving two reports on one descriptor would corrupt
                // both, and the first thread has the head start.
                write_stderr(
                    b"[ephpm] concurrent fatal signal on another thread - \
                      diagnostics suppressed for this one\n",
                );
                die(sig, info, ctx, false);
            }
        }
    }

    /// Write the diagnostic block. Fixed-size buffers, hand-rolled formatting,
    /// no allocation, no locks.
    fn report(sig: c_int, info: *mut siginfo_t) {
        let mut b = Buf::<320>::new();

        write_stderr(b"\n=== ephpm fatal signal ===\n");

        b.str("signal:   ");
        b.str(signal_name(sig));
        b.str(" (");
        b.dec(u64::from(sig.unsigned_abs()));
        b.str(")");
        if !info.is_null() {
            // SAFETY: registered with SA_SIGINFO, so the kernel guarantees
            // `info` points at a valid `siginfo_t` for the handler's lifetime.
            let code = unsafe { (*info).si_code };
            b.str("  si_code=");
            b.int(i64::from(code));
            b.str(" ");
            b.str(si_code_name(sig, code));
        }
        b.str("\n");
        flush(&mut b);

        if !info.is_null()
            && matches!(sig, libc::SIGSEGV | libc::SIGBUS | libc::SIGILL | libc::SIGFPE)
        {
            let addr = fault_addr(info);
            b.str("fault_at: ");
            b.hex(addr as u64);
            if addr < 4096 {
                b.str("  (null or near-null: a null-pointer dereference)");
            }
            b.str("\n");
            flush(&mut b);
        }

        let mut name = [0u8; 16];
        let name_len = thread_name(&mut name);
        b.str("thread:   \"");
        b.bytes(&name[..name_len]);
        b.str("\" tid=");
        b.dec(thread_id());
        b.str(" pid=");
        // SAFETY: `getpid` takes no arguments, cannot fail, and is
        // async-signal-safe.
        b.dec(u64::from(unsafe { libc::getpid() }.unsigned_abs()));
        b.str("\n");
        flush(&mut b);

        b.str("version:  ephpm ");
        b.str(env!("EPHPM_VERSION"));
        b.str("\n");
        flush(&mut b);

        // Reported as a fact about the handler, not as a diagnosis. Because
        // SA_ONSTACK is always set this is expected to say "yes" for every
        // fault, stack overflow or not - its value is that a "no" would mean
        // the alternate stack did not take effect and a real stack-overflow
        // fault could not have been reported at all. The authority on whether
        // this *was* a stack overflow is Rust std's chained handler, which owns
        // the guard-page range and prints "has overflowed its stack" below.
        b.str("altstack: ");
        b.str(if on_altstack() {
            "yes"
        } else {
            "NO (a stack-overflow fault could not be reported)"
        });
        b.str("\n");
        flush(&mut b);

        backtrace_report(&mut b);

        write_stderr(b"=== end ephpm fatal signal ===\n");
    }

    /// Capture and print the stack. Raw addresses first — they are formatted by
    /// hand and cannot wedge — then symbol names, which can.
    #[cfg(has_execinfo)]
    fn backtrace_report(b: &mut Buf<320>) {
        // Kept as a raw pointer throughout: taking a `&mut [_; N]` to a static
        // inside a signal handler would be a needless aliasing claim.
        let frames: *mut *mut c_void = FRAMES.0.get().cast();

        // SAFETY: `frames` points at a `static` array of exactly `MAX_FRAMES`
        // pointers, and `MAX_FRAMES_C` matches its length. Only the thread that
        // won the `OWNER` compare-exchange reaches this code and it never
        // yields ownership, so there is no aliasing.
        let n = unsafe { backtrace(frames, MAX_FRAMES_C) };
        if n <= 0 {
            write_stderr(b"frames:   unavailable\n");
            return;
        }
        let count = usize::try_from(n).unwrap_or(0).min(MAX_FRAMES);

        // Collapse runs of the same return address in place. A runaway
        // recursion fills all 64 slots with one address, which printed
        // verbatim is 128 lines that say a single thing; the fault-injection
        // run produced exactly that. Compacting the array before symbolising
        // keeps the raw list and the symbol list aligned one-to-one.
        //
        // Written back into the same static: `out` only advances when `i`
        // advances by at least as much, so the write cursor never passes the
        // read cursor.
        // ePHPm's own frames resolve to `binary(+0xOFFSET)` rather than a
        // function name: `backtrace_symbols_fd` resolves through `_dl_addr`,
        // which sees only *dynamic* symbols, and a Rust executable exports
        // none. PHP's own symbols DO resolve, because libphp exports them for
        // extensions to link against, so the Zend call chain reads in full. The
        // `+0x` offsets are load-base-relative, which is what addr2line wants
        // for a PIE - point people at those, not at the absolute addresses,
        // which are meaningless once ASLR has moved on. dlopen'd objects (PHP
        // extensions, native middleware) export dynamic symbols and so appear by
        // name, which is the question this module exists to answer.
        b.str("frames:   ");
        b.dec(count as u64);
        b.str(
            " captured, repeats collapsed (for ephpm's own frames feed the +0x \
               offsets to `addr2line -Cfpe <ephpm binary>`)\n",
        );
        flush(b);

        let mut out = 0usize;
        let mut i = 0usize;

        while i < count {
            // SAFETY: `i < count <= MAX_FRAMES`, so `frames.add(i)` is within
            // the static array `backtrace` just filled.
            let addr = unsafe { *frames.add(i) };
            let mut run = 1usize;
            // SAFETY: `i + run < count <= MAX_FRAMES` is checked first.
            while i + run < count && unsafe { *frames.add(i + run) } == addr {
                run += 1;
            }

            b.str("  #");
            b.dec2(out as u64);
            b.str(" ");
            b.hex(addr as usize as u64);
            if run > 1 {
                b.str("  (x");
                b.dec(run as u64);
                b.str(", recursion)");
            }
            b.str("\n");
            flush(b);

            // SAFETY: `out <= i < count <= MAX_FRAMES`, so this stays inside
            // the static array.
            unsafe { *frames.add(out) = addr };
            out += 1;
            i += run;
        }

        write_stderr(b"symbols:\n");
        // SAFETY: `frames` holds `out` valid entries after the compaction above
        // and 2 is stderr. `backtrace_symbols_fd` - unlike `backtrace_symbols` -
        // does not allocate. It can still block on the dynamic-loader lock,
        // which is exactly why every address above was printed first.
        unsafe {
            backtrace_symbols_fd(frames.cast_const(), c_int::try_from(out).unwrap_or(0), 2);
        }
    }

    /// No unwinder on this target; the header fields above are still useful.
    #[cfg(not(has_execinfo))]
    fn backtrace_report(_b: &mut Buf<320>) {
        write_stderr(b"frames:   unavailable (no backtrace(3) on this target)\n");
    }

    /// Restore the default disposition and re-raise, so the process dies with
    /// exactly the wait status it would have had without this handler.
    ///
    /// `chain` is false when the handler was re-entered — chaining again from a
    /// broken state is how a diagnostic handler turns one crash into two.
    fn die(sig: c_int, info: *mut siginfo_t, ctx: *mut c_void, chain: bool) {
        if chain {
            chain_to_previous(sig, info, ctx);
        }

        PHASE.store(phase::DYING, Ordering::SeqCst);

        // SAFETY: restore `SIG_DFL` for a signal this module installed, unblock
        // it in this thread's mask (it was auto-blocked on handler entry, since
        // SA_NODEFER was deliberately not set), then re-raise. The unblock is
        // what makes death happen *here* rather than after returning into a
        // corrupted frame that might fault differently and change the wait
        // status. All three calls are async-signal-safe.
        unsafe {
            let mut dfl: libc::sigaction = std::mem::zeroed();
            libc::sigemptyset(&raw mut dfl.sa_mask);
            dfl.sa_sigaction = libc::SIG_DFL;
            dfl.sa_flags = 0;
            libc::sigaction(sig, &raw const dfl, std::ptr::null_mut());

            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&raw mut set);
            libc::sigaddset(&raw mut set, sig);
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &raw const set, std::ptr::null_mut());

            libc::raise(sig);
        }
        // Not reached: `raise` delivers a now-unblocked signal whose
        // disposition is SIG_DFL. If it somehow were reached, returning re-runs
        // the faulting instruction, which faults again against SIG_DFL - the
        // same outcome by a longer road. Deliberately no `_exit` fallback:
        // that would replace a signal wait status with a normal one, which is
        // the one thing this module must never do.
    }

    /// Hand the signal to whatever was installed before us — in practice Rust
    /// std's stack-overflow reporter.
    fn chain_to_previous(sig: c_int, info: *mut siginfo_t, ctx: *mut c_void) {
        let Some(idx) = FATAL.iter().position(|&s| s == sig) else {
            return;
        };
        let prev = PREV_HANDLER[idx].load(Ordering::SeqCst);
        // 0 is SIG_DFL and 1 is SIG_IGN; neither is a callable address.
        if prev == libc::SIG_DFL || prev == libc::SIG_IGN {
            return;
        }

        PHASE.store(phase::CHAINING, Ordering::SeqCst);

        if PREV_SIGINFO[idx].load(Ordering::SeqCst) {
            // SAFETY: the previous disposition was registered with SA_SIGINFO,
            // so its ABI is `extern "C" fn(c_int, *mut siginfo_t, *mut c_void)`
            // and `prev` is the address the kernel gave back from `sigaction`.
            // The kernel's own `info` and `ctx` are forwarded unmodified: this
            // is what preserves Rust std's
            // `thread '<name>' has overflowed its stack` message, because std's
            // handler compares `si_addr` against the thread's guard page and
            // must therefore see the real siginfo.
            let f: extern "C" fn(c_int, *mut siginfo_t, *mut c_void) = unsafe {
                std::mem::transmute::<usize, extern "C" fn(c_int, *mut siginfo_t, *mut c_void)>(
                    prev,
                )
            };
            f(sig, info, ctx);
        } else {
            // SAFETY: registered without SA_SIGINFO, so the ABI is
            // `extern "C" fn(c_int)` and `prev` is the address `sigaction`
            // returned for it.
            let f: extern "C" fn(c_int) =
                unsafe { std::mem::transmute::<usize, extern "C" fn(c_int)>(prev) };
            f(sig);
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Async-signal-safe primitives
    // ─────────────────────────────────────────────────────────────────────────

    /// Write a byte slice to stderr, retrying short writes and `EINTR`.
    ///
    /// Bounded so that a persistently failing descriptor (`EBADF`, `EPIPE`)
    /// cannot spin forever inside a signal handler. `errno` is deliberately not
    /// consulted: it lives in thread-local storage this module would rather not
    /// depend on mid-fault, and the attempt cap covers the same ground.
    fn write_stderr(mut buf: &[u8]) {
        let mut attempts = 0u32;
        while !buf.is_empty() && attempts < 64 {
            attempts += 1;
            // SAFETY: `buf` is a valid readable slice of exactly the length
            // passed, and 2 is stderr. `write` is async-signal-safe.
            let n = unsafe { libc::write(2, buf.as_ptr().cast::<c_void>(), buf.len()) };
            if n > 0 {
                buf = &buf[usize::try_from(n).unwrap_or(buf.len()).min(buf.len())..];
            } else if n == 0 {
                break;
            }
        }
    }

    fn flush<const N: usize>(b: &mut Buf<N>) {
        write_stderr(b.as_slice());
        b.clear();
    }

    /// Fixed-capacity, allocation-free, panic-free formatting buffer.
    ///
    /// Truncates on overflow rather than growing or panicking — a truncated
    /// diagnostic is strictly better than an abort inside the abort path.
    struct Buf<const N: usize> {
        buf: [u8; N],
        len: usize,
    }

    impl<const N: usize> Buf<N> {
        const fn new() -> Self {
            Self { buf: [0; N], len: 0 }
        }

        fn clear(&mut self) {
            self.len = 0;
        }

        fn as_slice(&self) -> &[u8] {
            &self.buf[..self.len]
        }

        fn push(&mut self, byte: u8) {
            if self.len < N {
                self.buf[self.len] = byte;
                self.len += 1;
            }
        }

        fn bytes(&mut self, s: &[u8]) {
            for &byte in s {
                self.push(byte);
            }
        }

        fn str(&mut self, s: &str) {
            self.bytes(s.as_bytes());
        }

        /// Unsigned decimal.
        fn dec(&mut self, value: u64) {
            let mut digits = [0u8; 20];
            let mut n = 0;
            let mut v = value;
            loop {
                digits[n] = b'0' + (v % 10) as u8;
                n += 1;
                v /= 10;
                if v == 0 {
                    break;
                }
            }
            while n > 0 {
                n -= 1;
                self.push(digits[n]);
            }
        }

        /// Signed decimal. `si_code` is genuinely negative for some sources
        /// (`SI_QUEUE` is -1, `SI_TKILL` is -6), so printing it unsigned would
        /// be actively misleading.
        fn int(&mut self, value: i64) {
            if value < 0 {
                self.push(b'-');
            }
            self.dec(value.unsigned_abs());
        }

        /// Zero-padded two-digit decimal, for frame indices.
        #[cfg(has_execinfo)]
        fn dec2(&mut self, value: u64) {
            if value < 10 {
                self.push(b'0');
            }
            self.dec(value);
        }

        /// Fixed-width 16-digit hex with an `0x` prefix, so addresses line up.
        fn hex(&mut self, value: u64) {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            self.push(b'0');
            self.push(b'x');
            for shift in (0..16u32).rev() {
                self.push(HEX[((value >> (shift * 4)) & 0xf) as usize]);
            }
        }
    }

    /// Signal number to name. Static strings only.
    fn signal_name(sig: c_int) -> &'static str {
        match sig {
            libc::SIGSEGV => "SIGSEGV",
            libc::SIGBUS => "SIGBUS",
            libc::SIGILL => "SIGILL",
            libc::SIGFPE => "SIGFPE",
            libc::SIGABRT => "SIGABRT",
            _ => "SIG?",
        }
    }

    /// `si_code` to a human name, scoped by signal because the numeric values
    /// are reused across signals.
    fn si_code_name(sig: c_int, code: c_int) -> &'static str {
        match (sig, code) {
            (libc::SIGSEGV, si::SEGV_MAPERR) => "SEGV_MAPERR (address not mapped)",
            (libc::SIGSEGV, si::SEGV_ACCERR) => "SEGV_ACCERR (no permission for access)",
            (libc::SIGBUS, si::BUS_ADRALN) => "BUS_ADRALN (invalid address alignment)",
            (libc::SIGBUS, si::BUS_ADRERR) => "BUS_ADRERR (nonexistent physical address)",
            (libc::SIGBUS, si::BUS_OBJERR) => "BUS_OBJERR (object-specific hardware error)",
            (libc::SIGILL, si::ILL_ILLOPC) => "ILL_ILLOPC (illegal opcode)",
            (libc::SIGILL, si::ILL_ILLOPN) => "ILL_ILLOPN (illegal operand)",
            (libc::SIGILL, si::ILL_PRVOPC) => "ILL_PRVOPC (privileged opcode)",
            (libc::SIGFPE, si::FPE_INTDIV) => "FPE_INTDIV (integer divide by zero)",
            (libc::SIGFPE, si::FPE_INTOVF) => "FPE_INTOVF (integer overflow)",
            (libc::SIGFPE, si::FPE_FLTDIV) => "FPE_FLTDIV (float divide by zero)",
            (_, si::SI_USER) => "SI_USER (sent by kill/raise)",
            // Linux-only sources. Guarded per-arm rather than per-constant so
            // the Darwin table does not have to invent values it does not have.
            #[cfg(any(target_os = "linux", target_os = "android"))]
            (_, si::SI_TKILL) => "SI_TKILL (abort/pthread_kill/tgkill)",
            #[cfg(any(target_os = "linux", target_os = "android"))]
            (_, si::SI_KERNEL) => "SI_KERNEL (raised by the kernel)",
            #[cfg(any(target_os = "linux", target_os = "android"))]
            (_, si::SI_QUEUE) => "SI_QUEUE (sigqueue)",
            _ => "(unrecognised)",
        }
    }

    /// The faulting address from `siginfo_t`.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn fault_addr(info: *mut siginfo_t) -> usize {
        // SAFETY: `info` is non-null (checked by the caller) and points at a
        // kernel-provided `siginfo_t` valid for the handler's lifetime. On
        // Linux `si_addr()` reads the union member that is populated for
        // exactly the signals the caller gates on (SEGV/BUS/ILL/FPE).
        unsafe { (*info).si_addr() as usize }
    }

    /// The faulting address from `siginfo_t`.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn fault_addr(info: *mut siginfo_t) -> usize {
        // SAFETY: as above; on the BSD/Darwin layout `si_addr` is a plain
        // struct field rather than a union accessor.
        unsafe { (*info).si_addr as usize }
    }

    /// Kernel thread id.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn thread_id() -> u64 {
        // SAFETY: `gettid` takes no arguments, cannot fail, and is a bare
        // syscall - no locks, no allocation, safe from a signal handler.
        unsafe { libc::syscall(libc::SYS_gettid) }.unsigned_abs()
    }

    /// Kernel thread id.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    fn thread_id() -> u64 {
        let mut tid: u64 = 0;
        // SAFETY: on Apple `pthread_t` is `uintptr_t`, not a pointer type, so
        // "the calling thread" is the integer 0 rather than a null pointer.
        // `tid` is a valid out-pointer. Reads the thread struct without
        // locking or allocating.
        unsafe { libc::pthread_threadid_np(0, &raw mut tid) };
        tid
    }

    /// Kernel thread id (unavailable on this target).
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    )))]
    fn thread_id() -> u64 {
        0
    }

    /// Copy the current thread's name into `out`, returning its length.
    ///
    /// Uses `prctl(PR_GET_NAME)`, a bare syscall that copies at most 16 bytes
    /// into a caller-owned buffer. Deliberately *not* `pthread_getname_np`,
    /// which on glibc opens and reads `/proc/self/task/<tid>/comm` — file I/O
    /// is a poor idea in a handler and fails outright where `/proc` is not
    /// mounted.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn thread_name(out: &mut [u8; 16]) -> usize {
        // SAFETY: `PR_GET_NAME` writes at most 16 bytes, NUL included, into the
        // supplied buffer, which is exactly 16 bytes long.
        unsafe { libc::prctl(libc::PR_GET_NAME, out.as_mut_ptr().cast::<libc::c_char>()) };
        out.iter().position(|&b| b == 0).unwrap_or(out.len())
    }

    /// Copy the current thread's name into `out`, returning its length.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn thread_name(out: &mut [u8; 16]) -> usize {
        // SAFETY: Darwin's `pthread_getname_np` copies from the thread struct
        // into the caller's buffer without locking or allocating, and the
        // length passed matches the buffer.
        unsafe {
            libc::pthread_getname_np(
                libc::pthread_self(),
                out.as_mut_ptr().cast::<libc::c_char>(),
                out.len(),
            );
        }
        out.iter().position(|&b| b == 0).unwrap_or(out.len())
    }

    /// Whether the handler is currently executing on the alternate signal
    /// stack, which for a `SIGSEGV` is a strong hint the fault was a stack
    /// overflow.
    fn on_altstack() -> bool {
        // SAFETY: reading with a null `new` pointer leaves the alternate stack
        // unchanged; `cur` is a valid, zeroed out-parameter.
        unsafe {
            let mut cur: libc::stack_t = std::mem::zeroed();
            libc::sigaltstack(std::ptr::null(), &raw mut cur) == 0
                && cur.ss_flags & libc::SS_ONSTACK != 0
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn buf_formats_decimals() {
            let mut b = Buf::<64>::new();
            b.dec(0);
            b.str(",");
            b.dec(7);
            b.str(",");
            b.dec(1_234_567_890);
            b.str(",");
            b.dec(u64::MAX);
            assert_eq!(b.as_slice(), b"0,7,1234567890,18446744073709551615");
        }

        #[test]
        fn buf_formats_negative_si_codes() {
            let mut b = Buf::<64>::new();
            b.int(0);
            b.str(",");
            b.int(-6);
            b.str(",");
            b.int(i64::MIN);
            assert_eq!(b.as_slice(), b"0,-6,-9223372036854775808");
        }

        #[test]
        fn buf_formats_fixed_width_hex() {
            let mut b = Buf::<64>::new();
            b.hex(0);
            assert_eq!(b.as_slice(), b"0x0000000000000000");

            b.clear();
            b.hex(0xdead_beef);
            assert_eq!(b.as_slice(), b"0x00000000deadbeef");

            b.clear();
            b.hex(u64::MAX);
            assert_eq!(b.as_slice(), b"0xffffffffffffffff");
        }

        #[test]
        #[cfg(has_execinfo)]
        fn buf_pads_frame_indices() {
            let mut b = Buf::<16>::new();
            b.dec2(0);
            b.dec2(9);
            b.dec2(10);
            b.dec2(63);
            assert_eq!(b.as_slice(), b"00091063");
        }

        #[test]
        fn buf_truncates_instead_of_panicking() {
            // The whole point: overflow must never panic inside a handler.
            let mut b = Buf::<4>::new();
            b.str("abcdefghijklmnop");
            b.dec(u64::MAX);
            b.hex(u64::MAX);
            assert_eq!(b.as_slice(), b"abcd");
        }

        #[test]
        fn every_handled_signal_has_a_name() {
            for sig in FATAL {
                assert_ne!(signal_name(sig), "SIG?", "unnamed signal {sig}");
            }
        }

        #[test]
        fn si_code_names_are_signal_scoped() {
            // The numeric codes are reused across signals, so 1 must not decode
            // to the same thing for SIGSEGV and SIGBUS.
            assert!(si_code_name(libc::SIGSEGV, si::SEGV_MAPERR).starts_with("SEGV_"));
            assert!(si_code_name(libc::SIGBUS, si::BUS_ADRALN).starts_with("BUS_"));
            assert!(si_code_name(libc::SIGABRT, si::SI_USER).starts_with("SI_USER"));
        }

        #[test]
        #[cfg(any(target_os = "linux", target_os = "android"))]
        fn abort_si_code_is_labelled() {
            // Regression guard: glibc's abort() goes via tgkill, so every
            // SIGABRT arrives as SI_TKILL. This read "(unrecognised)" in the
            // first fault-injection run.
            assert!(si_code_name(libc::SIGABRT, si::SI_TKILL).starts_with("SI_TKILL"));
        }

        #[test]
        fn escape_hatch_recognises_falsey_values() {
            for v in ["0", "off", "false", "no", "OFF", " False "] {
                assert!(is_falsey(v), "{v} should disable the handler");
            }
            for v in ["1", "on", "true", "yes", ""] {
                assert!(!is_falsey(v), "{v} should leave the handler enabled");
            }
        }

        #[test]
        fn thread_name_is_readable() {
            let mut out = [0u8; 16];
            assert!(thread_name(&mut out) <= out.len());
        }

        // ─────────────────────────────────────────────────────────────────────
        // The test that matters: the handler is actually observed firing, and
        // the wait status is unchanged.
        //
        // A forked child faults for real. Everything the child's handler needs
        // is warmed and installed in the *parent* first, so the only code the
        // child runs before the fault is async-signal-safe - which is also the
        // invariant under test.
        // ─────────────────────────────────────────────────────────────────────

        /// Outcome of one forked fault.
        struct Fault {
            stderr: String,
            term_signal: Option<c_int>,
        }

        /// Fork, run `fault` in the child with stderr piped back, and collect
        /// what it printed plus how it died.
        ///
        /// `fault` must only call async-signal-safe things. It is expected not
        /// to return.
        fn run_faulting_child(fault: fn()) -> Fault {
            super::install();

            let mut fds = [0 as c_int; 2];
            // SAFETY: `fds` is a valid two-element array, which is what `pipe`
            // writes into.
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe() failed");
            let (read_fd, write_fd) = (fds[0], fds[1]);

            // SAFETY: `fork` is safe to call; the child below restricts itself
            // to async-signal-safe operations, which is the requirement for
            // forking from a multi-threaded process.
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0, "fork() failed");

            if pid == 0 {
                // SAFETY: child. Redirect stderr into the pipe, suppress core
                // dumps so a fault does not litter the build directory, then
                // fault. `_exit` (never `exit`) if the fault somehow returns,
                // so no atexit handler inherited from the parent runs twice.
                unsafe {
                    libc::dup2(write_fd, 2);
                    libc::close(read_fd);
                    libc::close(write_fd);
                    let no_core = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
                    libc::setrlimit(libc::RLIMIT_CORE, &raw const no_core);
                }
                fault();
                // SAFETY: unreachable unless `fault` failed to fault; exiting
                // non-zero makes that show up as a test failure rather than a
                // hang.
                unsafe { libc::_exit(97) };
            }

            // SAFETY: parent closes its copy of the write end so the read end
            // sees EOF when the child dies.
            unsafe { libc::close(write_fd) };

            let stderr = read_until_eof(read_fd, pid);
            // SAFETY: `read_fd` is owned by this function and still open.
            unsafe { libc::close(read_fd) };

            let mut status: c_int = 0;
            // SAFETY: `pid` is this process's child and `status` is a valid
            // out-pointer.
            unsafe { libc::waitpid(pid, &raw mut status, 0) };

            let term_signal =
                if libc::WIFSIGNALED(status) { Some(libc::WTERMSIG(status)) } else { None };

            Fault { stderr, term_signal }
        }

        /// Drain `fd` until EOF, with a hard deadline.
        ///
        /// The deadline is the point of the exercise as much as the output is:
        /// a handler that deadlocks (say, on the dynamic-loader lock inside
        /// `backtrace_symbols_fd`) would otherwise hang the test suite forever
        /// instead of failing it.
        fn read_until_eof(fd: c_int, child: libc::pid_t) -> String {
            const DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

            let start = std::time::Instant::now();
            let mut out = Vec::new();
            let mut chunk = [0u8; 4096];

            loop {
                let remaining = DEADLINE.saturating_sub(start.elapsed());
                if remaining.is_zero() {
                    // SAFETY: `child` is this process's child; SIGKILL cannot
                    // be caught, so the pipe is guaranteed to reach EOF and
                    // `waitpid` will not block.
                    unsafe { libc::kill(child, libc::SIGKILL) };
                    panic!(
                        "the fatal-signal handler did not finish within {DEADLINE:?} - \
                         it deadlocked. Captured so far:\n{}",
                        String::from_utf8_lossy(&out)
                    );
                }

                let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
                // SAFETY: `pfd` is a valid single-element pollfd array.
                let ready = unsafe {
                    libc::poll(&raw mut pfd, 1, i32::try_from(remaining.as_millis()).unwrap_or(1))
                };
                if ready < 0 {
                    continue; // EINTR
                }
                if ready == 0 {
                    continue; // re-check the deadline
                }

                // SAFETY: `chunk` is a valid writable buffer of the given size
                // and `fd` is open for reading.
                let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast::<c_void>(), chunk.len()) };
                if n <= 0 {
                    break;
                }
                out.extend_from_slice(&chunk[..usize::try_from(n).unwrap_or(0)]);
            }

            String::from_utf8_lossy(&out).into_owned()
        }

        #[test]
        fn segv_reports_diagnostics_and_still_exits_139() {
            let fault = run_faulting_child(|| {
                // SAFETY: deliberate. A volatile write through a null pointer
                // is the canonical null dereference and cannot be optimised
                // away.
                unsafe { std::ptr::write_volatile(std::ptr::null_mut::<u32>(), 0x41) };
            });

            assert_eq!(
                fault.term_signal,
                Some(libc::SIGSEGV),
                "wait status changed - the container would no longer exit 139.\n\
                 stderr was:\n{}",
                fault.stderr
            );

            let s = &fault.stderr;
            assert!(s.contains("=== ephpm fatal signal ==="), "no report header in:\n{s}");
            assert!(s.contains("=== end ephpm fatal signal ==="), "report truncated:\n{s}");
            assert!(s.contains("SIGSEGV"), "signal name missing from:\n{s}");
            assert!(s.contains("fault_at: 0x0000000000000000"), "fault address wrong in:\n{s}");
            assert!(s.contains("null-pointer dereference"), "null hint missing from:\n{s}");
            assert!(s.contains("thread:"), "thread identity missing from:\n{s}");
            assert!(s.contains("tid="), "tid missing from:\n{s}");
            assert!(
                !s.contains("the fatal-signal handler itself faulted"),
                "the handler faulted while handling:\n{s}"
            );
        }

        #[test]
        #[cfg(has_execinfo)]
        fn segv_report_includes_a_backtrace() {
            let fault = run_faulting_child(|| {
                // SAFETY: deliberate null dereference, as above.
                unsafe { std::ptr::write_volatile(std::ptr::null_mut::<u32>(), 0x41) };
            });

            let s = &fault.stderr;
            assert!(s.contains("frames:"), "no frame count in:\n{s}");
            assert!(s.contains("  #00 0x"), "no raw frame addresses in:\n{s}");
            assert!(s.contains("symbols:"), "no symbol section in:\n{s}");
        }

        #[test]
        fn abort_reports_diagnostics_and_still_exits_134() {
            let fault = run_faulting_child(|| {
                // SAFETY: deliberate. `abort` raises SIGABRT, which is the
                // status a glibc double-free or a Rust stack overflow ends up
                // producing.
                unsafe { libc::abort() };
            });

            assert_eq!(
                fault.term_signal,
                Some(libc::SIGABRT),
                "wait status changed - the container would no longer exit 134.\n\
                 stderr was:\n{}",
                fault.stderr
            );
            assert!(fault.stderr.contains("SIGABRT"), "SIGABRT not reported in:\n{}", fault.stderr);
        }

        #[test]
        fn fpe_reports_the_divide_by_zero() {
            let fault = run_faulting_child(|| {
                // SAFETY: deliberate. Raising SIGFPE directly rather than
                // dividing by zero, because the compiler is entitled to turn a
                // literal division by zero into unreachable code.
                unsafe { libc::raise(libc::SIGFPE) };
            });

            assert_eq!(fault.term_signal, Some(libc::SIGFPE), "stderr:\n{}", fault.stderr);
            assert!(fault.stderr.contains("SIGFPE"), "stderr:\n{}", fault.stderr);
        }
    }
}
