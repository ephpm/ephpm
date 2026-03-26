<?php
header('Content-Type: text/plain');

echo "=== SERVER VARS ===\n";
echo "REQUEST_URI: " . $_SERVER['REQUEST_URI'] . "\n";
echo "SCRIPT_NAME: " . $_SERVER['SCRIPT_NAME'] . "\n";
echo "SCRIPT_FILENAME: " . $_SERVER['SCRIPT_FILENAME'] . "\n";
echo "DOCUMENT_ROOT: " . $_SERVER['DOCUMENT_ROOT'] . "\n";
echo "QUERY_STRING: " . $_SERVER['QUERY_STRING'] . "\n";
echo "REQUEST_METHOD: " . $_SERVER['REQUEST_METHOD'] . "\n";
echo "GATEWAY_INTERFACE: " . ($_SERVER['GATEWAY_INTERFACE'] ?? 'unset') . "\n";
echo "REDIRECT_STATUS: " . ($_SERVER['REDIRECT_STATUS'] ?? 'unset') . "\n";
echo "SERVER_PROTOCOL: " . $_SERVER['SERVER_PROTOCOL'] . "\n";
echo "HTTP_HOST: " . ($_SERVER['HTTP_HOST'] ?? 'unset') . "\n";
echo "REMOTE_ADDR: " . $_SERVER['REMOTE_ADDR'] . "\n";

echo "\n=== POST ===\n";
var_export($_POST);
echo "\n\n=== FILES ===\n";
var_export($_FILES);
echo "\n\n=== GET ===\n";
var_export($_GET);
echo "\n\n=== COOKIE ===\n";
var_export($_COOKIE);
echo "\n\n=== php://input ===\n";
echo file_get_contents('php://input');
echo "\n";
