/* crash_guard.c — crash-containment primitive for PHP threads.
 *
 * This is the C half of ePHPm's single-process ZTS crash containment. It is
 * deliberately PURE libc — <setjmp.h>, <pthread.h>, <signal.h> — with NO PHP
 * dependency, so it compiles on every Unix build (stub AND php_linked) and the
 * mechanism is unit-tested without libphp.
 *
 * It contains two fault classes, both by abandoning the PHP thread:
 *
 *   1. A C-stack overflow (`[php] crash_containment`, experimental, off by
 *      default) — a SIGSEGV/SIGBUS, recovered from the fatal-signal handler.
 *   2. PHP's hard memory limit (always on where the linker supports it — see
 *      "The hard memory limit" below) — an `_exit(137)`, recovered from an
 *      `_exit` wrapper.
 *
 * # What it does
 *
 * `ephpm_guard_run(work, ctx, catch_faults)` runs `work(ctx)` inside a
 * per-thread recovery region established with `sigsetjmp`. If a
 * *stack-overflow* SIGSEGV/SIGBUS is delivered while inside a region armed
 * with `catch_faults`, the fatal-signal handler (in the `ephpm` binary crate,
 * src/fatal_signal.rs) calls `ephpm_guard_try_recover()`, which `siglongjmp`s
 * back into `ephpm_guard_run`, which then returns EPHPM_GUARD_STACK_OVERFLOW
 * instead of the process dying.
 *
 * # The hard memory limit
 *
 * PHP 8.3.36 / 8.4.27 / 8.5.12 (php-src 31c0ea7d7f) made `memory_limit` a soft
 * limit — exceeding it queues an out-of-memory error that the VM raises at its
 * next interrupt check — and added a hard limit (`memory_hard_limit_percent`,
 * by default 200% of `memory_limit`, or `memory_limit` + 1G when that is above
 * 1G). An allocation that cannot fail gracefully and would cross the hard limit
 * ends in `zend_mm_hard_limit_error()`, which prints a message to stderr and
 * calls `_exit(137)`. That kills one php-fpm worker; in ePHPm, where every
 * tenant runs on a thread of ONE process, it would kill the whole server.
 *
 * On Linux the binary is linked with `-Wl,--wrap=_exit`, which routes every
 * `_exit` call in the link (libphp.a's included) to `__wrap__exit` below. If
 * the caller is a PHP thread inside an armed region of THIS process and the
 * status is 137, it `siglongjmp`s back into `ephpm_guard_run`, which returns
 * EPHPM_GUARD_HARD_MEMORY_LIMIT; every other `_exit` goes to the real one
 * unchanged. The process check matters: a child created with fork()/vfork()
 * (proc_open, posix_spawn) inherits the forking thread's armed state, and must
 * exit, never jump into a copy of its parent's request.
 *
 * Why abandoning the thread is sound here: the hard-limit check runs at the
 * top of the allocator's slow paths, before the heap is modified, so the
 * thread's Zend heap itself is consistent. What is NOT consistent is whatever
 * the interrupted C code was half-way through — exactly the state the upstream
 * fix refuses to run destructors over. So the thread runs no PHP teardown and
 * never runs PHP again; the caller frees its Zend heap (see
 * `ephpm_release_contained_heap` in ephpm_wrapper.c) and retires it.
 *
 * macOS (ld64 has no --wrap) and Windows (link.exe has no --wrap) do not get
 * this interception: there `_exit(137)` still ends the process.
 *
 * What abandoning does NOT undo: process-level resources the request held —
 * open files and sockets and the `flock()` locks on them — are neither closed
 * nor released, because closing them means
 * running the same PHP teardown. Each contained request leaks them; that is
 * the price of not taking every other tenant down with it.
 *
 * # Why ONLY stack overflow among faults
 *
 * A stack overflow exhausts a *thread-local* resource (the thread's own stack)
 * and, for the ePHPm crash this targets — a deep `zend_object_std_dtor` <->
 * `zend_objects_store_del` C recursion freeing a large plain-object graph —
 * every write is into thread-local Zend state (the per-thread ZMM heap and
 * `EG(objects_store)`). No other thread's memory and no process-global
 * allocator lock is touched at the fault instant (the Zend MM manager mmaps its
 * own 2 MiB chunks; it does not go through the C `malloc` — glibc's, or on
 * Linux release builds the process-wide mimalloc — so no allocator lock is
 * held). The rest of the process is therefore intact, and abandoning the
 * poisoned thread contains the blast.
 *
 * A wild write / heap corruption produces the SAME SIGSEGV but may already have
 * corrupted another thread's or a shared allocator's memory — continuing is
 * unsafe. The two are told apart by the fault address: a stack overflow faults
 * at/just below THIS thread's stack low bound (the guard region); anything else
 * is NOT contained and the handler proceeds to its normal die-with-diagnostic
 * path. See `ephpm_guard_try_recover`.
 *
 * # Async-signal-safety
 *
 * `ephpm_guard_try_recover` is reached from a signal handler. It touches only
 * `__thread` integer state (initial-exec TLS: a fixed offset from the thread
 * pointer, no lazy allocation, no lock), does integer comparisons, and calls
 * `siglongjmp` — all on the POSIX async-signal-safe list. It never allocates,
 * never locks, never calls back into libphp.
 */

#if defined(__unix__) || defined(__APPLE__)

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <setjmp.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stddef.h>
#include <sys/types.h>
#include <unistd.h>

/* Return codes of ephpm_guard_run — mirrored by `crash_guard::imp` in Rust. */
#define EPHPM_GUARD_COMPLETED         0
#define EPHPM_GUARD_STACK_OVERFLOW    1
#define EPHPM_GUARD_HARD_MEMORY_LIMIT 2

/* The status zend_mm_hard_limit_error() passes to _exit() (Zend/zend_alloc.c,
 * php-src 31c0ea7d7f). */
#define EPHPM_HARD_LIMIT_EXIT_STATUS 137

/* Per-thread recovery state. `__thread` (not pthread_key_create) so that reads
 * from a signal handler are async-signal-safe. */
static __thread sigjmp_buf g_recover_buf;
static __thread int        g_armed;        /* 1 while inside a guarded region */
static __thread int        g_catch_faults; /* 1 if that region also contains stack overflows */
static __thread uintptr_t  g_stack_lo;     /* lowest usable stack address, 0 = unknown */
static __thread uintptr_t  g_stack_hi;     /* one past the highest usable stack address */

/* The process whose threads armed a guard. Recorded at the first arm (every
 * arming thread stores the same value, so the race is benign) and compared in
 * the _exit wrapper, so a forked child never jumps into its parent's region. */
static pid_t g_owner_pid;

/* How far below the stack's low bound a fault is still classified as a stack
 * overflow. A thread guard is normally one page, but glibc can place several
 * guard pages, and the faulting access (a `sub rsp` + write, or a `call`
 * pushing a return address) can land a little past the guard. 64 KiB is
 * comfortably wider than any real guard and far narrower than the gap to any
 * other mapping, so it neither misses a real overflow nor mistakes a heap wild
 * write (which lands nowhere near this thread's stack) for one. */
#define EPHPM_GUARD_WINDOW ((uintptr_t)(64 * 1024))

/* A fault at/above the stack low bound but within this slack still counts as an
 * overflow: the guard-page trip can be the first byte of the new (too-low)
 * frame, an address marginally above `g_stack_lo`. One page is plenty. */
#define EPHPM_GUARD_SLACK ((uintptr_t)4096)

/* Capture this thread's stack bounds at ARM time (normal context), never from
 * the handler: `pthread_getattr_np` reads the pthread descriptor (and, for the
 * main thread, /proc/self/maps) and is not async-signal-safe. On an unknown
 * platform the bounds stay 0 and `ephpm_guard_try_recover` refuses to contain
 * anything — fail-safe (a real fatal still dies with its diagnostic). */
static void ephpm_guard_capture_bounds(void)
{
    g_stack_lo = 0;
    g_stack_hi = 0;
#if defined(__linux__)
    pthread_attr_t attr;
    if (pthread_getattr_np(pthread_self(), &attr) == 0) {
        void  *base = NULL;
        size_t size = 0;
        if (pthread_attr_getstack(&attr, &base, &size) == 0 && base && size) {
            /* glibc returns `base` = lowest usable address (the guard page
             * sits BELOW it) and `size` = usable bytes. */
            g_stack_lo = (uintptr_t)base;
            g_stack_hi = (uintptr_t)base + size;
        }
        pthread_attr_destroy(&attr);
    }
#elif defined(__APPLE__)
    void  *hi   = pthread_get_stackaddr_np(pthread_self()); /* highest address */
    size_t size = pthread_get_stacksize_np(pthread_self());
    if (hi && size) {
        g_stack_hi = (uintptr_t)hi;
        g_stack_lo = (uintptr_t)hi - (uintptr_t)size;
    }
#endif
}

/*
 * Run `work(ctx)` inside a recovery region.
 *
 * Returns EPHPM_GUARD_COMPLETED if `work` returned normally. Otherwise `work`
 * did NOT finish and the calling thread's PHP context is POISONED:
 *
 *   EPHPM_GUARD_STACK_OVERFLOW     a stack overflow was contained (only when
 *                                  `catch_faults` is non-zero);
 *   EPHPM_GUARD_HARD_MEMORY_LIMIT  PHP's hard memory limit called _exit(137)
 *                                  (Linux builds linked with --wrap=_exit).
 *
 * After either, the caller must retire the OS thread (exit it and spawn a
 * replacement) and must never run PHP on it again, nor tear it down through
 * php_request_shutdown/ts_free_thread. See the Rust `crash_guard::run_guarded`
 * docs and `ephpm-server/src/fpm_pool.rs`.
 *
 * `catch_faults` = 0 arms only the _exit interception. The signal mask is then
 * not saved (`sigsetjmp(buf, 0)` makes no system call), which keeps the
 * always-on hard-limit guard free on the request path.
 *
 * Nested guards are not supported (ePHPm never nests PHP execution); a nested
 * call runs `work` without establishing a second recovery point so it cannot
 * clobber the outer `sigjmp_buf`.
 */
int ephpm_guard_run(void (*work)(void *), void *ctx, int catch_faults)
{
    if (g_armed) {
        work(ctx);
        return EPHPM_GUARD_COMPLETED;
    }

    if (catch_faults) {
        ephpm_guard_capture_bounds();
    } else {
        g_stack_lo = 0;
        g_stack_hi = 0;
    }

    if (__atomic_load_n(&g_owner_pid, __ATOMIC_RELAXED) == 0) {
        __atomic_store_n(&g_owner_pid, getpid(), __ATOMIC_RELAXED);
    }

    /* With catch_faults, savesigs=1: the signal mask is saved here and
     * restored on siglongjmp, so SIGSEGV — auto-blocked on handler entry
     * because the handler is installed without SA_NODEFER — is unblocked again
     * after recovery and a subsequent overflow can still be caught. The _exit
     * path jumps from ordinary (non-handler) context and needs no mask restore.
     *
     * The sigsetjmp value is consumed directly by `switch`, one of the
     * contexts the C standard allows for a setjmp invocation. */
    switch (sigsetjmp(g_recover_buf, catch_faults ? 1 : 0)) {
    case 0:
        break;
    case EPHPM_GUARD_STACK_OVERFLOW:
        /* Returned here via siglongjmp from ephpm_guard_try_recover. */
        g_armed = 0;
        g_catch_faults = 0;
        return EPHPM_GUARD_STACK_OVERFLOW;
    default:
        /* Returned here via siglongjmp from __wrap__exit. */
        g_armed = 0;
        g_catch_faults = 0;
        return EPHPM_GUARD_HARD_MEMORY_LIMIT;
    }

    g_catch_faults = catch_faults ? 1 : 0;
    g_armed = 1;
    work(ctx);
    g_armed = 0;
    g_catch_faults = 0;
    return EPHPM_GUARD_COMPLETED;
}

/*
 * Classify a SIGSEGV/SIGBUS fault and, if it is a contained stack overflow,
 * recover. Called from the fatal-signal handler with the faulting address
 * (`siginfo_t.si_addr`).
 *
 * Returns 0 (and the handler proceeds to its normal fatal path) when the fault
 * is NOT a contained stack overflow: not inside a guarded region, bounds
 * unknown, or the address is not within this thread's stack guard window. When
 * it IS a contained stack overflow this function does not return — it
 * siglongjmps back into ephpm_guard_run.
 *
 * Async-signal-safe (see file header).
 */
int ephpm_guard_try_recover(uintptr_t fault_addr)
{
    if (!g_armed || !g_catch_faults) {
        return 0;
    }
    if (g_stack_lo == 0) {
        return 0; /* bounds unknown on this platform — never contain */
    }
    /* Below the guard window? (written to avoid unsigned underflow) */
    if (fault_addr + EPHPM_GUARD_WINDOW < g_stack_lo) {
        return 0;
    }
    /* Well above the low bound — inside or above the live stack, i.e. a wild
     * write, not an overflow. (A write into the *mapped* live stack would not
     * fault at all, so any fault this high is not a stack overflow.) */
    if (fault_addr > g_stack_lo + EPHPM_GUARD_SLACK) {
        return 0;
    }

    /* Contained. Disarm and jump back into ephpm_guard_run. */
    g_armed = 0;
    siglongjmp(g_recover_buf, EPHPM_GUARD_STACK_OVERFLOW);
    return 0; /* unreachable */
}

/*
 * The hard-memory-limit interception (see "The hard memory limit" above).
 *
 * Linux only: it depends on GNU ld / lld `--wrap=_exit`, which the build
 * scripts pass for every linked target that runs PHP requests
 * (crates/ephpm/build.rs for the binary, crates/ephpm-php/build.rs for this
 * crate's tests). `__real__exit` is declared weak so that a target linked
 * WITHOUT the wrap still links: there it stays NULL, nothing ever calls
 * `__wrap__exit`, and ephpm_guard_exit_containment_available() reports 0 so
 * the Rust side does not claim a containment it cannot provide.
 */
#if defined(__linux__)

#include <sys/syscall.h>

extern void __real__exit(int status) __attribute__((weak, noreturn));

int ephpm_guard_exit_containment_available(void)
{
    return __real__exit != NULL;
}

void __wrap__exit(int status) __attribute__((noreturn));

void __wrap__exit(int status)
{
    if (status == EPHPM_HARD_LIMIT_EXIT_STATUS && g_armed) {
        pid_t owner = __atomic_load_n(&g_owner_pid, __ATOMIC_RELAXED);
        /* getpid() is a real system call on glibc >= 2.25 (no cached pid), so
         * it is correct in a vfork()/CLONE_VM child that shares this TLS. */
        if (owner != 0 && owner == getpid()) {
            g_armed = 0;
            g_catch_faults = 0;
            siglongjmp(g_recover_buf, EPHPM_GUARD_HARD_MEMORY_LIMIT);
        }
    }

    if (__real__exit != NULL) {
        __real__exit(status);
    }
    /* Not reachable in practice: __wrap__exit is only ever called when the
     * link used --wrap=_exit, which also binds __real__exit. Still exit the
     * way glibc's _exit does rather than return from a noreturn function. */
    for (;;) {
        syscall(SYS_exit_group, status);
    }
}

#else

int ephpm_guard_exit_containment_available(void)
{
    return 0;
}

#endif /* __linux__ */

/*
 * Test-only: report whether the current thread is inside a guarded region.
 * Used by the unit tests to assert arm/disarm bookkeeping.
 */
int ephpm_guard_is_armed(void)
{
    return g_armed;
}

#endif /* unix */
