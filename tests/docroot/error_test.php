<?php
// Triggers a recoverable PHP warning. The server must not crash and must return
// a 200 response. Used to verify that non-fatal PHP errors are handled safely.
header('Content-Type: text/plain');
$arr = [];
$val = $arr['missing_key'];  // E_WARNING: Undefined array key
echo "error_test: survived\n";
