<?php
/**
 * Revoke (or restore) a tenant from PHP, to show that the native middleware
 * and PHP share ONE embedded KV store under the same literal key names.
 *
 * `ephpm_kv_set()` here writes exactly the key the module reads with the host
 * table's `kv_get` callback, so the very next request through the gate is
 * rejected with 403 — in native code, with no restart and no config reload.
 * When clustering is enabled the marker gossips to every node.
 *
 * NOTE — demo only. This endpoint has no authentication of its own. It sits
 * outside the gate's `prefix` on purpose (so you can still un-revoke a tenant
 * after revoking it), which also means nothing is guarding it. Do not copy
 * this file into anything real.
 *
 * NOTE — single-site only. With `[server] sites_dir` set, PHP is rebound to a
 * per-vhost store per request while the middleware host table keeps one
 * process-wide store, so the two would no longer meet on the same key.
 */

header('Content-Type: application/json');

if (!function_exists('ephpm_kv_set')) {
    http_response_code(501);
    echo json_encode(['error' => 'not running under ePHPm — ephpm_kv_* unavailable']), "\n";
    return;
}

$tenant = $_GET['tenant'] ?? '';
if ($tenant === '' || !preg_match('/^[A-Za-z0-9_-]+$/', $tenant)) {
    http_response_code(400);
    echo json_encode(['error' => 'pass ?tenant=<name>']), "\n";
    return;
}

// Must match ApiGate::revocation_key() in src/lib.rs.
$key = "apigate:revoked:{$tenant}";

if (isset($_GET['restore'])) {
    ephpm_kv_del($key);
    echo json_encode(['tenant' => $tenant, 'revoked' => false, 'key' => $key]), "\n";
    return;
}

// TTL is in SECONDS on both surfaces: PHP's ephpm_kv_set() and the middleware
// host table agree. Any value at all means "revoked"; the module only checks
// for presence.
ephpm_kv_set($key, '1', 300);
echo json_encode(['tenant' => $tenant, 'revoked' => true, 'key' => $key, 'ttl' => 300]), "\n";
