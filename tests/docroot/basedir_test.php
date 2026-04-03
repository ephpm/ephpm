<?php
header('Content-Type: application/json');

$path = $_GET['path'] ?? '';

if ($path === '') {
    echo json_encode(['success' => false, 'error' => 'missing path parameter']);
    exit;
}

$result = @file_get_contents($path);
if ($result === false) {
    $err = error_get_last();
    echo json_encode([
        'success' => false,
        'error' => $err['message'] ?? 'unknown error',
    ]);
} else {
    echo json_encode([
        'success' => true,
        'error' => '',
    ]);
}
