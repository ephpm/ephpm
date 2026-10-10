<?php

/*
 * Request-teardown regression fixture: a session counter that NEVER calls
 * session_write_close().
 *
 * The session is therefore written and its lock released only in the session
 * module's RSHUTDOWN, i.e. in php_request_shutdown(). ePHPm used to defer that
 * call to the top of the same PHP thread's NEXT request, so under the default
 * multi-thread pool a second request on this session landed on another thread
 * and blocked on the lock the idle first thread still held (files handler:
 * forever; ephpm KV handler: until its lock TTL).
 *
 *   ?handler=files  (default) — PHP's files save handler
 *   ?handler=ephpm            — ePHPm's native KV-backed handler
 *
 * Consumed by crates/ephpm-e2e/tests/request_teardown.rs.
 */

ini_set('session.save_handler', ($_GET['handler'] ?? 'files') === 'ephpm' ? 'ephpm' : 'files');
header('Content-Type: text/plain');

session_start();
$_SESSION['n'] = ($_SESSION['n'] ?? 0) + 1;
echo $_SESSION['n'] . "\n";
