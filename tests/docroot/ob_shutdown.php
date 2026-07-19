<?php
// Regression fixture for the fpm shutdown-buffer bug: content in an
// unclosed ob_start() buffer, plus output printed from a shutdown
// function, must BOTH appear in the response. Before the fix this
// returned content-length: 0 (the capture ran before PHP flushed
// buffers / ran shutdown functions), which made WordPress 7.0 render
// every page blank under fpm mode.
ob_start();
register_shutdown_function(static function (): void {
    echo 'SHUTDOWN_RAN';
});
echo 'OB_BODY_';
// Deliberately no ob_end_flush() — WP 7.0's template-enhancement
// buffer relies on end-of-request finalization exactly like this.
