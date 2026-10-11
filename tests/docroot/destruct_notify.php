<?php

/*
 * Request-teardown regression fixture: a global object whose destructor
 * reports back to the test over a loopback TCP connection.
 *
 * Global objects are destroyed in php_request_shutdown(). ePHPm used to defer
 * that call to the top of the same PHP thread's NEXT request, so the
 * destructor's side effect only happened when some unrelated later request
 * reached this thread. It must now happen right after the response, with no
 * follow-up request at all.
 *
 *   ?port=N — the test's loopback listener; the destructor connects to
 *             127.0.0.1:N and writes "destructed\n".
 *
 * Consumed by crates/ephpm-e2e/tests/request_teardown.rs.
 */

final class DestructNotify
{
    public function __construct(private int $port)
    {
    }

    public function __destruct()
    {
        $sock = @fsockopen('127.0.0.1', $this->port, $errno, $errstr, 2.0);
        if ($sock !== false) {
            fwrite($sock, "destructed\n");
            fclose($sock);
        }
    }
}

header('Content-Type: text/plain');
$notify = new DestructNotify((int) ($_GET['port'] ?? 0));
echo "ok\n";
