<?php
// Runaway recursion that re-enters the VM through an INTERNAL function
// (issue #116).
//
// This is the shape a template/block renderer produces: WordPress'
// `do_blocks()` -> `render_block()` -> `apply_filters()` ->
// `call_user_func_array()` -> the userland filter -> back into `render_block()`
// for the inner blocks. A userland -> userland call costs no C stack (the VM
// loops via ZEND_VM_ENTER), but a hop *through* an internal function does:
// `execute_ex` re-enters below `zend_call_function`, one C frame per level.
//
// PHP 8.3+ checks `zend_call_stack_overflowed(EG(stack_limit))` at exactly
// those checkpoints and raises a catchable
//
//     Error: Maximum call stack size of N bytes (zend.max_allowed_stack_size -
//     zend.reserved_stack_size) reached. Infinite recursion?
//
// which ePHPm turns into an ordinary 500. Before #116 ePHPm overrode
// `zend_call_stack_init()` with a no-op on Linux, so `EG(stack_limit)` stayed
// NULL, the checkpoint never fired, and this request walked off the end of the
// thread stack: SIGSEGV -> the whole server process aborted.
//
// USED BY: crates/ephpm-e2e/tests/errors.rs (fpm mode). The equivalent
// worker-mode route is `?__deep=` in tests/worker-docroot/worker.php.
//
// The default depth is far beyond anything a real template reaches and is
// bounded so the request cannot run for an unreasonable time. It is deliberately
// NOT bounded by memory: at ~11 bytes of output per level this allocates
// nothing that could trip memory_limit first, so a 500 here really is the stack
// guard and not a memory bailout.

header('Content-Type: text/plain');

$depth = (int) ($_GET['depth'] ?? 200000);
$depth = max(1, min($depth, 2000000));

function ephpm_deep_render(int $level): string
{
    if ($level <= 0) {
        return '<p>leaf</p>';
    }

    // array_map is the internal-function hop: it calls back into userland via
    // zend_call_function, which is a real C frame AND a stack checkpoint.
    return array_map(
        static fn (int $ignored): string => '<div>' . ephpm_deep_render($level - 1) . '</div>',
        [1],
    )[0];
}

$html = ephpm_deep_render($depth);

echo "survived depth={$depth} len=", strlen($html), "\n";
