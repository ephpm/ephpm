<?php
/**
 * Test the KV store SAPI functions.
 *
 * The ephpm_kv_* functions are available only when PHP is linked with ephpm-php
 * and the KV store is initialized. These tests validate zero-serialization access
 * to the embedded KV store.
 */

// Check if SAPI functions exist (will only be true with php_linked)
if (!function_exists('ephpm_kv_get')) {
    http_response_code(500);
    echo json_encode([
        'error' => 'SAPI KV functions not available',
        'note' => 'Ensure PHP is linked (cargo xtask release) and KV store is enabled'
    ]);
    exit(1);
}

$test = $_GET['test'] ?? 'all';
$result = ['test' => $test, 'passed' => false, 'details' => []];

try {
    match ($test) {
        'set_get' => test_set_get($result),
        'del' => test_del($result),
        'exists' => test_exists($result),
        'incr' => test_incr($result),
        'expire' => test_expire($result),
        'all' => test_all($result),
        default => throw new Exception("Unknown test: {$test}")
    };

    $result['passed'] = true;
} catch (Exception $e) {
    http_response_code(500);
    $result['error'] = $e->getMessage();
}

http_response_code($result['passed'] ? 200 : 500);
header('Content-Type: application/json');
echo json_encode($result, JSON_PRETTY_PRINT);
exit($result['passed'] ? 0 : 1);

// ── Test functions ──────────────────────────────────────────────────────────

function test_set_get(&$result) {
    // SET
    ephpm_kv_set('test:key', 'hello world');

    // GET
    $value = ephpm_kv_get('test:key');
    if ($value !== 'hello world') {
        throw new Exception("Expected 'hello world', got '" . var_export($value, true) . "'");
    }

    $result['details']['set_get'] = 'PASS';
}

function test_del(&$result) {
    // SET
    ephpm_kv_set('del:key', 'value');

    // DEL
    $removed = ephpm_kv_del('del:key');
    if ($removed !== 1) {
        throw new Exception("DEL should return 1, got {$removed}");
    }

    // GET (should be null)
    $value = ephpm_kv_get('del:key');
    if ($value !== null) {
        throw new Exception("GET after DEL should return null, got " . var_export($value, true));
    }

    $result['details']['del'] = 'PASS';
}

function test_exists(&$result) {
    // SET
    ephpm_kv_set('exists:key', 'value');

    // EXISTS existing
    $exists = ephpm_kv_exists('exists:key');
    if ($exists !== 1) {
        throw new Exception("EXISTS for present key should return 1, got {$exists}");
    }

    // EXISTS missing
    $missing = ephpm_kv_exists('does:not:exist');
    if ($missing !== 0) {
        throw new Exception("EXISTS for missing key should return 0, got {$missing}");
    }

    $result['details']['exists'] = 'PASS';
}

function test_incr(&$result) {
    // INCR on new key
    $val1 = ephpm_kv_incr('counter:test', 1);
    if ($val1 !== 1) {
        throw new Exception("INCR on new key should return 1, got {$val1}");
    }

    // INCR again
    $val2 = ephpm_kv_incr('counter:test', 5);
    if ($val2 !== 6) {
        throw new Exception("INCR by 5 on value 1 should return 6, got {$val2}");
    }

    // INCR with negative
    $val3 = ephpm_kv_incr('counter:test', -2);
    if ($val3 !== 4) {
        throw new Exception("INCR by -2 on value 6 should return 4, got {$val3}");
    }

    $result['details']['incr'] = 'PASS';
}

function test_expire(&$result) {
    // SET with TTL via separate EXPIRE call
    ephpm_kv_set('expire:key', 'value');

    // EXPIRE (set TTL to 60 seconds)
    $ok = ephpm_kv_expire('expire:key', 60000);  // milliseconds
    if ($ok !== 1) {
        throw new Exception("EXPIRE on existing key should return 1, got {$ok}");
    }

    // PTTL should return a value > 0 and <= 60000
    $pttl = ephpm_kv_pttl('expire:key');
    if ($pttl <= 0 || $pttl > 60000) {
        throw new Exception("PTTL should be in (0, 60000], got {$pttl}");
    }

    // EXPIRE on missing key
    $notfound = ephpm_kv_expire('missing:key', 10000);
    if ($notfound !== 0) {
        throw new Exception("EXPIRE on missing key should return 0, got {$notfound}");
    }

    $result['details']['expire'] = 'PASS';
}

function test_all(&$result) {
    test_set_get($result);
    test_del($result);
    test_exists($result);
    test_incr($result);
    test_expire($result);
}
