<?php
/**
 * The CONTINUE lane: this path is outside the gate's `prefix`, so the module
 * returns ACTION_CONTINUE and PHP runs exactly as it would with no middleware
 * mounted. The only trace the gate leaves is the `X-Api-Gate: bypass`
 * response header it appended.
 */

header('Content-Type: application/json');

echo json_encode([
    'status' => 'ok',
    'gated'  => false,
    'tenant' => $_SERVER['HTTP_X_API_TENANT'] ?? null, // always null here
], JSON_PRETTY_PRINT), "\n";
