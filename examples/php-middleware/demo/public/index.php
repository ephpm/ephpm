<?php
/**
 * Front controller for the php-middleware demo.
 *
 * Requests to /api/... do not exist as files, so ePHPm's default `fallback`
 * chain (["$uri", "$uri/", "/index.php?$query_string"]) resolves them here.
 *
 * By the time this runs, middleware.php has already executed in this very same
 * request. If it hit its RESPOND path (401), this file never ran at all. If it
 * fell through (CONTINUE), the token it verified is waiting in $_SERVER — set
 * by the mount's REWRITE — because both share one request.
 */

declare(strict_types=1);

header('Content-Type: application/json');

echo json_encode([
    'script'  => basename($_SERVER['SCRIPT_NAME'] ?? __FILE__),
    // Injected by the middleware's REWRITE (assignment to $_SERVER).
    'token'   => $_SERVER['HTTP_X_TOKEN'] ?? null,
    // The middleware also set an `X-Auth-Checked: 1` response header — inspect
    // it with `curl -i`.
    'checked' => true,
], JSON_PRETTY_PRINT), "\n";
