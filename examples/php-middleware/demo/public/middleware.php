<?php
/**
 * PHP middleware — a bearer-token gate (EXPERIMENTAL `php:` lane).
 *
 * Mounted with `library = "php:middleware.php"`. This file runs INSIDE the
 * PHP request, immediately before the application script (public/index.php),
 * for every request matching the mount's `match = "/api/*"` glob.
 *
 * It demonstrates all four verdicts the lane understands. The mapping is stock
 * PHP — there is no C ABI transliterated into PHP:
 *
 *   ACTION_RESPOND  → set a status, echo a body, exit;   (the app never runs)
 *   ACTION_REWRITE  → assign to $_SERVER
 *   response header → header()
 *   ACTION_CONTINUE → fall off the end / return;
 *
 * Fail-closed: a parse error, an uncaught Throwable or a fatal here answers
 * 500 and the application script does not run.
 */

declare(strict_types=1);

// The mount's `config` table, as JSON. `null` when no `config` was set.
$cfg   = json_decode(ephpm_middleware_config() ?? '{}', true);
$realm = $cfg['realm'] ?? 'api';

$auth = $_SERVER['HTTP_AUTHORIZATION'] ?? '';

// ── RESPOND ────────────────────────────────────────────────────────────────
// No usable bearer token: answer 401 ourselves and stop. `exit` ends the
// chain — later mounts and the application script are skipped.
if (!str_starts_with($auth, 'Bearer ')) {
    http_response_code(401);
    header('WWW-Authenticate: Bearer realm="' . $realm . '"');
    header('Content-Type: application/json');
    echo json_encode(['error' => 'missing or malformed bearer token'], JSON_PRETTY_PRINT), "\n";
    exit; // RESPOND — the application script never runs.
}

$token = substr($auth, 7);

// ── REWRITE ──────────────────────────────────────────────────────────────
// Hand the verified token to the application through $_SERVER. Because the
// mount shares the request's superglobals, the app just reads it back — no
// side channel, no re-parsing the header.
$_SERVER['HTTP_X_TOKEN'] = $token;

// ── response header ──────────────────────────────────────────────────────
// header() here lands on the eventual client response, same as anywhere else.
header('X-Auth-Checked: 1');

// ── CONTINUE ─────────────────────────────────────────────────────────────
// Falling off the end (or `return;`) hands control to the next mount, then to
// the application script. Nothing else to do.
