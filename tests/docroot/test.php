<?php
header('Content-Type: text/plain');

echo "=== ePHPm Test Page ===\n\n";

// Request info
echo "REQUEST_METHOD: " . $_SERVER['REQUEST_METHOD'] . "\n";
echo "REQUEST_URI: " . $_SERVER['REQUEST_URI'] . "\n";
echo "QUERY_STRING: " . ($_SERVER['QUERY_STRING'] ?? '') . "\n";
echo "SCRIPT_FILENAME: " . $_SERVER['SCRIPT_FILENAME'] . "\n";
echo "DOCUMENT_ROOT: " . $_SERVER['DOCUMENT_ROOT'] . "\n";
echo "SERVER_NAME: " . $_SERVER['SERVER_NAME'] . "\n";
echo "SERVER_PORT: " . $_SERVER['SERVER_PORT'] . "\n";
echo "REMOTE_ADDR: " . $_SERVER['REMOTE_ADDR'] . "\n";

// Query params
if (!empty($_GET)) {
    echo "\n--- GET params ---\n";
    foreach ($_GET as $k => $v) {
        echo "  $k = $v\n";
    }
}

// POST params
if (!empty($_POST)) {
    echo "\n--- POST params ---\n";
    foreach ($_POST as $k => $v) {
        echo "  $k = $v\n";
    }
}

// Headers
echo "\n--- Request Headers ---\n";
foreach ($_SERVER as $k => $v) {
    if (strpos($k, 'HTTP_') === 0) {
        echo "  $k = $v\n";
    }
}

echo "\nDone.\n";
