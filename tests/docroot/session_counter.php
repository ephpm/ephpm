<?php

/*
 * Session locking regression fixture.
 *
 * Performs a deliberately racy read-modify-write on a $_SESSION counter:
 * read the value, sleep 50ms (wide race window), increment, write, close.
 * Without pessimistic session locking, N concurrent requests sharing one
 * session cookie lose updates and the final counter lands below N. With
 * the ephpm handler's per-session lock (acquired in PS_READ, released in
 * PS_CLOSE), the requests serialize and the counter reaches exactly N.
 *
 * Modes (?mode=...):
 *   init — reset the counter to 0 and emit the session cookie
 *   incr — read counter, usleep(50ms), write counter+1, echo new value
 *   read — echo the current counter
 *
 * Consumed by crates/ephpm-e2e/tests/session_locking.rs.
 */

ini_set('session.save_handler', 'ephpm');
header('Content-Type: text/plain');

$mode = $_GET['mode'] ?? 'incr';

session_start();

if ($mode === 'init') {
    $_SESSION['counter'] = 0;
    session_write_close();
    echo "0\n";
    return;
}

if ($mode === 'read') {
    $value = $_SESSION['counter'] ?? -1;
    session_write_close();
    echo $value . "\n";
    return;
}

// incr: read-modify-write with a wide race window.
$current = $_SESSION['counter'] ?? 0;
usleep(50000); // 50ms — plenty of time for a concurrent request to interleave
$_SESSION['counter'] = $current + 1;
session_write_close();
echo ($current + 1) . "\n";
