<?php
/**
 * Using ePHPm's built-in KV store via SAPI functions.
 *
 * The ephpm_kv_* functions are native PHP functions that provide zero-copy,
 * zero-serialization access to the embedded KV store. These are only available
 * when PHP is linked (cargo xtask release) and the KV store is initialized.
 *
 * No dependencies required — works out of the box.
 */

// Check if SAPI functions are available
if (!function_exists('ephpm_kv_get')) {
    http_response_code(500);
    echo "Error: SAPI KV functions not available.\n";
    echo "Make sure you:\n";
    echo "  1. Built with: cargo xtask release\n";
    echo "  2. Have KV store enabled in your config\n";
    exit(1);
}

// ── Basic Operations ────────────────────────────────────────────────────────

// SET: Store a value
ephpm_kv_set('user:1:name', 'Alice');
ephpm_kv_set('user:1:email', 'alice@example.com');

// GET: Retrieve a value
$name = ephpm_kv_get('user:1:name');
echo "Name: $name\n";

// EXISTS: Check if a key exists
if (ephpm_kv_exists('user:1:name')) {
    echo "Found user:1:name\n";
}

// DEL: Remove a key
$deleted = ephpm_kv_del('user:1:email');
echo "Deleted $deleted keys\n";

// ── Counters (with INCR) ────────────────────────────────────────────────────

// Initialize a counter
ephpm_kv_set('page:views', '0');

// Increment by 1
$views = ephpm_kv_incr('page:views', 1);
echo "Page views: $views\n";

// Increment by 5
$views = ephpm_kv_incr('page:views', 5);
echo "Page views: $views\n";

// Decrement (use negative delta)
$views = ephpm_kv_incr('page:views', -2);
echo "Page views: $views\n";

// ── TTL (Expiration) ────────────────────────────────────────────────────────

// Set a temporary value (e.g., session data)
ephpm_kv_set('session:abc123', 'user_data_here');

// Expire in 30 minutes (30 * 60 * 1000 milliseconds)
ephpm_kv_expire('session:abc123', 30 * 60 * 1000);

// Check remaining TTL
$ttl_ms = ephpm_kv_pttl('session:abc123');
$ttl_sec = $ttl_ms / 1000;
echo "Session expires in $ttl_sec seconds\n";

// ── Example: Cache Pattern ──────────────────────────────────────────────────

function get_expensive_data($id) {
    $cache_key = "expensive:$id";

    // Try to get from cache first
    $cached = ephpm_kv_get($cache_key);
    if ($cached !== null) {
        echo "Cache HIT for $cache_key\n";
        return $cached;
    }

    // Cache miss — compute the expensive data
    echo "Cache MISS for $cache_key, computing...\n";
    $data = "Result of expensive computation for $id";

    // Store in cache for 5 minutes
    ephpm_kv_set($cache_key, $data);
    ephpm_kv_expire($cache_key, 5 * 60 * 1000);

    return $data;
}

echo "\nCache example:\n";
echo get_expensive_data(42) . "\n";
echo get_expensive_data(42) . "\n"; // Should hit cache
echo get_expensive_data(43) . "\n";

// ── Available SAPI Functions ────────────────────────────────────────────────

echo "\nAvailable KV SAPI functions:\n";
echo "  ephpm_kv_get(key) -> string|null\n";
echo "  ephpm_kv_set(key, value) -> void\n";
echo "  ephpm_kv_del(key) -> int (1 if deleted, 0 if not found)\n";
echo "  ephpm_kv_exists(key) -> int (1 or 0)\n";
echo "  ephpm_kv_incr(key, delta) -> int\n";
echo "  ephpm_kv_expire(key, ttl_ms) -> int (1 if key exists, 0 if not)\n";
echo "  ephpm_kv_pttl(key) -> int (milliseconds left, -1 if no expiry, -2 if missing)\n";
