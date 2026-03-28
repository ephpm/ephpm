<?php
/**
 * Using ePHPm's KV store via Redis protocol with Predis.
 *
 * The embedded KV store speaks the Redis RESP2 protocol, allowing you to use
 * any Redis client library. This example uses Predis, the popular pure-PHP
 * Redis client.
 *
 * Install Predis:
 *   composer require predis/predis
 *
 * Note: ePHPm's KV store currently supports strings, counters, and TTL.
 * It does NOT yet support hashes, lists, sets, or transactions.
 */

require_once __DIR__ . '/../vendor/autoload.php';

use Predis\Client;

// Connect to the embedded KV store
// The KV server listens on 127.0.0.1:6379 by default
try {
    $redis = new Client([
        'scheme' => 'tcp',
        'host'   => '127.0.0.1',
        'port'   => 6379,
    ]);

    // Verify connection
    $redis->ping();
    echo "Connected to ePHPm KV store\n\n";
} catch (Exception $e) {
    http_response_code(500);
    echo "Error: Could not connect to KV store at 127.0.0.1:6379\n";
    echo "Make sure the KV server is running: cargo xtask release && ./target/release/ephpm\n";
    exit(1);
}

// ── Basic String Operations ──────────────────────────────────────────────

echo "=== Basic Operations ===\n";

// SET: Store values
$redis->set('product:1:name', 'Laptop');
$redis->set('product:1:price', '999.99');
$redis->set('product:1:stock', '50');
echo "Stored 3 product attributes\n";

// GET: Retrieve values
$name = $redis->get('product:1:name');
$price = $redis->get('product:1:price');
echo "Product: $name (Price: \$$price)\n";

// EXISTS: Check if keys exist
$exists = $redis->exists('product:1:name', 'product:1:price', 'missing');
echo "Keys exist: $exists (checked 3 keys)\n";

// DEL: Remove keys
$deleted = $redis->del('product:1:price');
echo "Deleted $deleted keys\n";

// ── Multiple Keys ────────────────────────────────────────────────────────

echo "\n=== Multiple Key Operations ===\n";

// MSET: Set multiple key-value pairs
$redis->mset([
    'user:1:name' => 'Alice',
    'user:1:email' => 'alice@example.com',
    'user:2:name' => 'Bob',
    'user:2:email' => 'bob@example.com',
]);
echo "Stored user data (MSET)\n";

// MGET: Get multiple values
$users = $redis->mget('user:1:name', 'user:1:email', 'user:2:name', 'missing');
echo "Retrieved users: " . json_encode($users) . "\n";

// ── String Manipulation ──────────────────────────────────────────────────

echo "\n=== String Operations ===\n";

// APPEND: Add to a string
$redis->set('message', 'Hello');
$redis->append('message', ' World!');
$msg = $redis->get('message');
echo "Appended string: $msg\n";

// STRLEN: Get string length
$len = $redis->strlen('message');
echo "String length: $len\n";

// ── Counters and Increments ─────────────────────────────────────────────

echo "\n=== Atomic Counters ===\n";

// INCR: Increment by 1
$views = $redis->incr('page:home:views');
echo "Page views: $views\n";

// INCRBY: Increment by amount
$views = $redis->incrby('page:home:views', 9);
echo "Page views after batch: $views\n";

// DECR / DECRBY: Decrement
$stock = $redis->decrby('product:1:stock', 5);
echo "Stock after sale: $stock\n";

// ── Expiration (TTL) ─────────────────────────────────────────────────────

echo "\n=== Key Expiration ===\n";

// SETEX: Set with expiration (shorthand)
$redis->setex('cache:result:42', 300, 'cached_data_here');
echo "Set cache key with 300 second TTL\n";

// Check TTL
$ttl = $redis->ttl('cache:result:42');
echo "Cache TTL: $ttl seconds\n";

// EXPIRE: Add expiration to existing key
$redis->set('temp:session:xyz', 'session_data');
$redis->expire('temp:session:xyz', 1800); // 30 minutes
echo "Expired temp:session:xyz for 1800 seconds\n";

// PEXPIRE: Set expiration in milliseconds
$redis->pexpire('temp:session:xyz', 30 * 60 * 1000);

// PERSIST: Remove expiration
$redis->persist('temp:session:xyz');
echo "Removed expiration from temp:session:xyz\n";

// ── Key Management ──────────────────────────────────────────────────────

echo "\n=== Key Management ===\n";

// KEYS: List keys matching pattern
$pattern_keys = $redis->keys('user:*');
echo "Keys matching 'user:*': " . json_encode($pattern_keys) . "\n";

// TYPE: Get key type
$type = $redis->type('user:1:name');
echo "Type of 'user:1:name': $type\n";

// RENAME: Rename a key
$redis->set('old_key', 'value');
$redis->rename('old_key', 'new_key');
echo "Renamed old_key → new_key\n";
echo "old_key exists? " . ($redis->exists('old_key') ? 'yes' : 'no') . "\n";
echo "new_key exists? " . ($redis->exists('new_key') ? 'yes' : 'no') . "\n";

// DBSIZE: Count all keys
$size = $redis->dbsize();
echo "Total keys in store: $size\n";

// ── Useful Patterns ─────────────────────────────────────────────────────

echo "\n=== Common Patterns ===\n";

// Rate limiting: Track requests per user
function check_rate_limit($redis, $user_id, $max_requests, $window_sec) {
    $key = "ratelimit:$user_id";

    $requests = $redis->incr($key);
    if ($requests == 1) {
        $redis->expire($key, $window_sec);
    }

    return $requests <= $max_requests;
}

// Session storage (simple string-based)
function store_session($redis, $session_id, $data, $ttl_sec = 3600) {
    $redis->setex("session:$session_id", $ttl_sec, json_encode($data));
}

function get_session($redis, $session_id) {
    $data = $redis->get("session:$session_id");
    return $data ? json_decode($data, true) : null;
}

// Page view tracking (per endpoint)
function track_view($redis, $page) {
    $redis->incr("views:$page");
}

// Test the patterns
if (check_rate_limit($redis, 'user:123', 100, 60)) {
    echo "✓ User 123 within rate limit (request #1)\n";
}

store_session($redis, 'sess_abc', [
    'user_id' => 123,
    'username' => 'alice',
    'login_time' => date('Y-m-d H:i:s'),
]);
$session = get_session($redis, 'sess_abc');
echo "✓ Session stored and retrieved: " . json_encode($session) . "\n";

track_view($redis, 'home');
track_view($redis, 'home');
track_view($redis, 'about');
$home_views = $redis->get('views:home');
$about_views = $redis->get('views:about');
echo "✓ Page views tracked: home=$home_views, about=$about_views\n";

// ── Server Commands ─────────────────────────────────────────────────────

echo "\n=== Server Info ===\n";

// PING: Test connection
echo "PING: " . $redis->ping() . "\n";

// INFO: Get server info
$info = $redis->info();
echo "Server info: " . substr((string)$info, 0, 100) . "...\n";

// ECHO: Echo a string
echo "ECHO result: " . $redis->echo('Hello from ePHPm') . "\n";

echo "\n=== All Tests Complete ===\n";
echo "Supported commands: GET, SET, SETEX, MGET, MSET, SETNX, INCR, DECR, INCRBY, DECRBY\n";
echo "                    APPEND, STRLEN, GETSET, DEL, EXISTS, EXPIRE, PEXPIRE, PERSIST\n";
echo "                    TTL, PTTL, TYPE, KEYS, DBSIZE, FLUSHDB, RENAME, PING, ECHO\n";
