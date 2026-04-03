<?php
header('Content-Type: application/json');

$output = [];
$code = -1;

try {
    $result = @exec('echo hello', $output, $code);
    if ($result === false && $code === -1) {
        echo json_encode([
            'success' => false,
            'output' => '',
            'error' => 'exec() returned false',
        ]);
    } else {
        echo json_encode([
            'success' => true,
            'output' => implode("\n", $output),
            'error' => '',
        ]);
    }
} catch (\Error $e) {
    echo json_encode([
        'success' => false,
        'output' => '',
        'error' => $e->getMessage(),
    ]);
}
