<?php
// Returns a JSON object containing the $_SERVER variables most commonly
// asserted in e2e tests. Used for precise assertions on REQUEST_URI,
// SCRIPT_NAME, HTTPS, SERVER_PORT, REMOTE_ADDR, and similar fields.
header('Content-Type: application/json');
$keys = [
    'REQUEST_METHOD',
    'REQUEST_URI',
    'SCRIPT_NAME',
    'SCRIPT_FILENAME',
    'DOCUMENT_ROOT',
    'QUERY_STRING',
    'SERVER_NAME',
    'SERVER_PORT',
    'SERVER_PROTOCOL',
    'REMOTE_ADDR',
    'HTTPS',
    'HTTP_HOST',
    'HTTP_X_FORWARDED_FOR',
    'HTTP_X_FORWARDED_PROTO',
    'GATEWAY_INTERFACE',
];
$out = [];
foreach ($keys as $k) {
    $out[$k] = $_SERVER[$k] ?? null;
}
echo json_encode($out, JSON_PRETTY_PRINT | JSON_UNESCAPED_SLASHES);
