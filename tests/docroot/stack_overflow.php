<?php
// Induces a C-STACK OVERFLOW inside PHP's object-destructor cascade.
//
// Freeing a long chain of plain objects recurses in C —
// zend_object_std_dtor() <-> zend_objects_store_del() — one frame per node,
// with no VM checkpoint in between. Past a few tens of thousands of nodes that
// exhausts the executing thread's stack and the guard page faults (SIGSEGV).
//
// Without `[php] crash_containment` this kills the whole server process. With
// it, the fault is contained: this request gets a 500 and the pool thread that
// ran it is retired and replaced. Nothing in PHP userland can catch this — no
// try/catch, no `disable_functions`, no memory_limit — which is exactly why the
// interception has to happen at the signal handler.
//
// USED BY: crates/ephpm-e2e/tests/crash_containment.rs (its own isolated node).
// Requesting it on a node WITHOUT containment will crash that server on purpose.

header('Content-Type: text/plain');

// Deep enough to blow a thread stack with margin (the recursion needs well
// under 100 bytes per node, and pool threads get ephpm_php::PHP_THREAD_STACK =
// 8 MiB), while staying far below any sane memory_limit — a memory bailout
// would ALSO produce a 500 and would silently pass a test that meant to prove
// containment. Overridable so the threshold can be probed by hand.
//
// NOTE: this fixture targets the destructor-cascade class specifically, which
// PHP's own C-stack guard cannot see (no VM / zend_call_function checkpoint
// runs between zend_object_std_dtor frames). Runaway *recursion* is a different
// class and is now caught by PHP itself — see deep_recursion.php.
$depth = (int) ($_GET['depth'] ?? 400000);
$depth = max(1000, min($depth, 5000000));

final class EphpmCrashNode
{
    public ?EphpmCrashNode $next = null;
}

$head = new EphpmCrashNode();
$cursor = $head;
for ($i = 0; $i < $depth; $i++) {
    $node = new EphpmCrashNode();
    $cursor->next = $node;
    $cursor = $node;
}

// CRITICAL: drop every other reference into the chain first. PHP's refcounting
// frees the graph at the moment the LAST reference goes away — if `$cursor` or
// `$node` still points into it, `$head = null` frees nothing and the recursive
// free is deferred to this thread's next request shutdown, landing the crash on
// somebody else's request. Releasing them here makes the fault land on THIS
// request, deterministically.
unset($cursor, $node);

// The deep recursive free. Execution does not continue past this line.
$head = null;

echo "survived depth={$depth}\n";
