<?php
/**
 * Front controller for the api-gate demo.
 *
 * Requests to /api/... do not exist as files, so ePHPm's default `fallback`
 * chain (["$uri", "$uri/", "/index.php?$query_string"]) resolves them here.
 * That is what makes the middleware's REWRITE verdict meaningful: the gate
 * rewrites REQUEST_URI, and this front controller is what routes on it.
 */

header('Content-Type: application/json');

echo json_encode([
    // Which file actually executed. In fpm mode this is decided before the
    // middleware chain runs, so a REWRITE never changes it.
    'script'      => basename($_SERVER['SCRIPT_NAME'] ?? __FILE__),
    // What the REWRITE verdict rewrote. Compare with the URL you requested.
    'request_uri' => $_SERVER['REQUEST_URI'] ?? null,
    // Injected by the middleware as a request-header override.
    'tenant'      => $_SERVER['HTTP_X_API_TENANT'] ?? null,
], JSON_PRETTY_PRINT), "\n";
