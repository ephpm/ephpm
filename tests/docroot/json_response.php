<?php
// Returns a JSON response with correct Content-Type.
// Used by e2e tests to verify content-type propagation.
header('Content-Type: application/json');
echo json_encode(['status' => 'ok', 'value' => 42]);
