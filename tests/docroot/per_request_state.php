<?php

/*
 * Per-request PHP state isolation probe.
 *
 * On a correct per-request SAPI (php-fpm, php-cgi, php cli-server, embed
 * with proper php_request_startup/shutdown), every response is
 * byte-identical regardless of how many times the same worker thread has
 * already served this script:
 *
 *   was_defined=false
 *   static_counter=1
 *   global_counter=1
 *   func_existed=false
 *
 * On a leaky embed SAPI that reuses a single PHP request across HTTP
 * requests, request #2+ on the same worker thread sees state that should
 * have been destroyed in request shutdown — constants, function-local
 * statics, $GLOBALS entries, and user-declared functions all persist.
 *
 * Regression for ephpm/ephpm#101 ("isolate per-request state with
 * request shutdown/startup cycle"). The test that consumes this fixture
 * is `per_request_isolation.rs`.
 */

header('Content-Type: text/plain');

// `function tick() { ... }` declared at top level would fatal on the
// second request to the same worker thread under the leak — guard it
// so the fixture survives long enough to report the other canaries.
$func_existed = function_exists('tick');
if (!$func_existed) {
    function tick() {
        static $n = 0;
        $n++;
        return $n;
    }
}

$was_defined = defined('EPHPM_LEAK_PROBE');
if (!$was_defined) {
    define('EPHPM_LEAK_PROBE', 1);
}

if (!isset($GLOBALS['leak_global_counter'])) {
    $GLOBALS['leak_global_counter'] = 0;
}
$GLOBALS['leak_global_counter']++;

echo "was_defined=" . ($was_defined ? "true" : "false") . "\n";
echo "static_counter=" . tick() . "\n";
echo "global_counter=" . $GLOBALS['leak_global_counter'] . "\n";
echo "func_existed=" . ($func_existed ? "true" : "false") . "\n";
