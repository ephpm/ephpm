/* crash_guard.c — stack-overflow crash-containment primitive.
 *
 * This is the C half of ePHPm's single-process ZTS crash containment
 * (`[php] crash_containment`, experimental, off by default). It is deliberately
 * PURE libc — <setjmp.h>, <pthread.h>, <signal.h> — with NO PHP dependency, so
 * it compiles on every Unix build (stub AND php_linked) and the mechanism is
 * unit-tested without libphp.
 *
 * # What it does
 *
 * `ephpm_guard_run(work, ctx)` runs `work(ctx)` inside a per-thread recovery
 * region established with `sigsetjmp`. If a *stack-overflow* SIGSEGV/SIGBUS is
 * delivered while inside that region, the fatal-signal handler (in the `ephpm`
 * binary crate, src/fatal_signal.rs) calls `ephpm_guard_try_recover()`, which
 * `siglongjmp`s back into `ephpm_guard_run`, which then returns 1 ("contained")
 * instead of the process dying.
 *
 * # Why ONLY stack overflow
 *
 * A stack overflow exhausts a *thread-local* resource (the thread's own stack)
 * and, for the ePHPm crash this targets — a deep `zend_object_std_dtor` <->
 * `zend_objects_store_del` C recursion freeing a large plain-object graph —
 * every write is into thread-local Zend state (the per-thread ZMM heap and
 * `EG(objects_store)`). No other thread's memory and no process-global
 * allocator lock is touched at the fault instant (the Zend MM manager mmaps its
 * own 2 MiB chunks; it does not go through glibc `malloc`, so the arena lock is
 * not held). The rest of the process is therefore intact, and abandoning the
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

/* Per-thread recovery state. `__thread` (not pthread_key_create) so that reads
 * from a signal handler are async-signal-safe. */
static __thread sigjmp_buf g_recover_buf;
static __thread int        g_armed;     /* 1 while inside a guarded region */
static __thread uintptr_t  g_stack_lo;  /* lowest usable stack address, 0 = unknown */
static __thread uintptr_t  g_stack_hi;  /* one past the highest usable stack address */

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
 * Run `work(ctx)` inside a stack-overflow recovery region.
 *
 * Returns 0 if `work` returned normally, or 1 if a stack overflow was contained
 * — in which case `work` did NOT finish, and the calling thread's PHP context
 * is POISONED: the caller must retire the OS thread (exit it and spawn a
 * replacement) and must never run PHP on it again, nor tear it down through
 * php_request_shutdown/ts_free_thread. See the Rust `crash_guard::run_guarded`
 * docs and `ephpm-server/src/fpm_pool.rs`.
 *
 * Nested guards are not supported (ePHPm never nests PHP execution); a nested
 * call runs `work` without establishing a second recovery point so it cannot
 * clobber the outer `sigjmp_buf`.
 */
int ephpm_guard_run(void (*work)(void *), void *ctx)
{
    if (g_armed) {
        work(ctx);
        return 0;
    }

    ephpm_guard_capture_bounds();

    /* sigsetjmp with savesigs=1: the signal mask is saved here and restored on
     * siglongjmp, so SIGSEGV — auto-blocked on handler entry because the
     * handler is installed without SA_NODEFER — is unblocked again after
     * recovery and a subsequent overflow can still be caught. */
    if (sigsetjmp(g_recover_buf, 1) != 0) {
        /* Returned here via siglongjmp from ephpm_guard_try_recover. */
        g_armed = 0;
        return 1;
    }

    g_armed = 1;
    work(ctx);
    g_armed = 0;
    return 0;
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
    if (!g_armed) {
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

    /* Contained. Disarm and jump back into ephpm_guard_run, which returns 1. */
    g_armed = 0;
    siglongjmp(g_recover_buf, 1);
    return 0; /* unreachable */
}

/*
 * Test-only: report whether the current thread is inside a guarded region.
 * Used by the unit tests to assert arm/disarm bookkeeping.
 */
int ephpm_guard_is_armed(void)
{
    return g_armed;
}

#endif /* unix */
