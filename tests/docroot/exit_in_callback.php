<?php
// exit() from a callback PHP runs with no calling frame. The engine finishes
// the exit's unwind and then calls zend_bailout(); php-fpm absorbs that and
// sends the response. ePHPm used to answer a bare 500 and discard the page
// (Yii 2 error pages, every phpMyAdmin page). Each case must be delivered with
// the status the script set. (Status and headers are set before any output:
// with output_buffering off the first echo sends the headers, as in php-fpm.)
switch ($_GET['case'] ?? '') {
    case 'exception_handler':
        // Yii 2 ErrorHandler shape: render the error page, then exit(1).
        set_exception_handler(function ($e) {
            http_response_code(404);
            header('X-Exit-Case: exception_handler');
            echo 'handled: ', $e->getMessage();
            exit(1);
        });
        throw new Exception('boom');

    case 'shutdown':
        // phpMyAdmin shape: the page is sent from a shutdown function ending in
        // exit. The second shutdown function must not run (php-fpm semantics).
        register_shutdown_function(function () {
            echo '|shutdown-1';
            exit;
        });
        register_shutdown_function(function () { echo '|SHUTDOWN-2-MUST-NOT-RUN'; });
        header('X-Exit-Case: shutdown');
        echo 'main';
        break;

    case 'shutdown_redirect':
        register_shutdown_function(function () {
            header('Location: /elsewhere', true, 302);
            exit;
        });
        break;

    case 'exit_then_oom':
        // A real bailout after an exit()-bailout still fails closed.
        register_shutdown_function(function () {
            ini_set('memory_limit', '2M');
            $a = [];
            for ($i = 0; $i < 100; $i++) {
                $a[] = str_repeat('x', 1024 * 1024);
            }
        });
        // The handler sets a non-200 status, so only the bailout
        // classification (not "fatal recorded and status still 200") can 500.
        set_exception_handler(function ($e) {
            http_response_code(404);
            echo 'EXIT-THEN-OOM-PARTIAL';
            exit(1);
        });
        throw new Exception('boom');

    default:
        http_response_code(400);
        echo 'unknown case';
}
