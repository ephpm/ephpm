<?php
// This file exists to test the PHP execution allowlist (allowed_php_paths).
// When allowed_php_paths is configured and does NOT include /uploads/shell.php,
// requests to this file must return 403 Forbidden.
header('Content-Type: text/plain');
echo "uploads/shell.php executed\n";
