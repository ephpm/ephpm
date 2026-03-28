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
    echo "Connected to KV store\n";
} catch (Exception $e) {
    http_response_code(500);
    echo "Error: Could not connect to KV store at 127.0.0.1:6379\n";
    echo "Make sure the KV server is running.\n";
    exit(1);
}

// ── Basic Operations ────────────────────────────────────────────────────────

// SET: Store values
$redis->set('product:1:name', 'Laptop');
$redis->set('product:1:price', '999.99');
$redis->set('product:1:stock', '50');

// GET: Retrieve values
$name = $redis->get('product:1:name');
$price = $redis->get('product:1:price');
echo "Product: $name (Price: \$$price)\n";

// EXISTS: Check if keys exist
$exists = $redis->exists('product:1:name');
echo "Product exists: " . ($exists ? 'yes' : 'no') . "\n";

// DEL: Remove keys
$deleted = $redis->del('product:1:price');
echo "Deleted $deleted keys\n";

// ── Working with Hashes ────────────────────────────────────────────────────
// Hashes are perfect for representing objects

// HSET: Store multiple fields for a key
$redis->hset('user:123', [
    'name'       => 'Bob',
    'email'      => 'bob@example.com',
    'created_at' => date('Y-m-d'),
]);

// HGET: Get a specific field
$name = $redis->hget('user:123', 'name');
echo "User name: $name\n";

// HGETALL: Get all fields as array
$user = $redis->hgetall('user:123');
echo "User data: " . json_encode($user, JSON_PRETTY_PRINT) . "\n";

// HINCRBY: Increment a hash field (useful for counters)
$redis->hset('user:123', 'login_count', '0');
$redis->hincrby('user:123', 'login_count', 1);
$logins = $redis->hget('user:123', 'login_count');
echo "User login count: $logins\n";

// ── Working with Lists ──────────────────────────────────────────────────────
// Lists are great for queues and activity logs

// RPUSH: Add items to the end of a list
$redis->rpush('queue:jobs', ['job:1', 'job:2', 'job:3']);

// LLEN: Get list length
$count = $redis->llen('queue:jobs');
echo "Jobs in queue: $count\n";

// LPOP: Remove and return the first item
$job = $redis->lpop('queue:jobs');
echo "Processing job: $job\n";

// LRANGE: Get a range of items
$remaining = $redis->lrange('queue:jobs', 0, -1);
echo "Remaining jobs: " . implode(', ', $remaining) . "\n";

// ── Working with Sets ───────────────────────────────────────────────────────
// Sets are useful for membership and unique lists

// SADD: Add items to a set
$redis->sadd('tags:active', ['php', 'web', 'backend', 'php']); // duplicate 'php' ignored

// SCARD: Get set size
$count = $redis->scard('tags:active');
echo "Active tags: $count\n";

// SMEMBERS: Get all members
$tags = $redis->smembers('tags:active');
echo "Tags: " . implode(', ', $tags) . "\n";

// SISMEMBER: Check membership
if ($redis->sismember('tags:active', 'php')) {
    echo "'php' is in tags:active\n";
}

// ── Expiration (TTL) ────────────────────────────────────────────────────────

// Set a key with expiration (EX = seconds)
$redis->setex('cache:result:42', 300, 'cached_data_here'); // expires in 5 minutes

// Check remaining TTL
$ttl = $redis->ttl('cache:result:42');
echo "Cache TTL: $ttl seconds\n";

// Expire an existing key
$redis->set('temp:session:xyz', 'session_data');
$redis->expire('temp:session:xyz', 1800); // 30 minutes

// PEXPIRE: Set expiration in milliseconds (more precise)
$redis->pexpire('temp:session:xyz', 30 * 60 * 1000);

// ── Atomic Counters ─────────────────────────────────────────────────────────

// INCR: Increment by 1
$views = $redis->incr('page:home:views');
echo "Home page views: $views\n";

// INCRBY: Increment by amount
$views = $redis->incrby('page:home:views', 10);
echo "Page views after batch: $views\n";

// DECR / DECRBY: Decrement
$stock = $redis->decrby('product:1:stock', 1);
echo "Stock after sale: $stock\n";

// ── Transactions (MULTI/EXEC) ───────────────────────────────────────────────
// Atomic operations on multiple keys

try {
    $pipe = $redis->pipeline(function ($pipe) {
        $pipe->set('account:1:balance', '1000');
        $pipe->set('account:2:balance', '500');
        $pipe->incr('account:1:balance');
        $pipe->decr('account:2:balance');
    });
    echo "Transaction completed: " . json_encode($pipe) . "\n";
} catch (Exception $e) {
    echo "Transaction error: " . $e->getMessage() . "\n";
}

// ── Useful Patterns ─────────────────────────────────────────────────────────

// Rate limiting: Track requests per user
function check_rate_limit($user_id, $max_requests, $window_sec) {
    global $redis;
    $key = "ratelimit:$user_id";

    $requests = $redis->incr($key);
    if ($requests == 1) {
        $redis->expire($key, $window_sec);
    }

    return $requests <= $max_requests;
}

// Session storage
function store_session($session_id, $data, $ttl_sec = 3600) {
    global $redis;
    $redis->setex("session:$session_id", $ttl_sec, json_encode($data));
}

function get_session($session_id) {
    global $redis;
    $data = $redis->get("session:$session_id");
    return $data ? json_decode($data, true) : null;
}

echo "\n--- Pattern Examples ---\n";

// Rate limit check
if (check_rate_limit('user:123', 100, 60)) {
    echo "User 123 within rate limit\n";
}

// Session
store_session('sess_abc', ['user_id' => 123, 'username' => 'alice']);
$session = get_session('sess_abc');
echo "Session: " . json_encode($session) . "\n";

// ── Available Redis Commands ────────────────────────────────────────────────

echo "\n--- Supported Commands ---\n";
echo "Strings: GET, SET, SETEX, INCR, DECR, APPEND, STRLEN, GETRANGE, SETRANGE\n";
echo "Hashes: HGET, HSET, HMGET, HMSET, HGETALL, HDEL, HEXISTS, HINCRBY, HLEN\n";
echo "Lists: LPUSH, RPUSH, LPOP, RPOP, LLEN, LRANGE, LINDEX, LSET, LTRIM\n";
echo "Sets: SADD, SREM, SMEMBERS, SISMEMBER, SCARD, SINTER, SUNION, SDIFF\n";
echo "Keys: DEL, EXISTS, EXPIRE, TTL, KEYS, SCAN, TYPE, RENAME\n";
echo "Transactions: MULTI, EXEC, DISCARD, WATCH\n";
echo "Server: PING, ECHO, SELECT, FLUSHDB, FLUSHALL, DBSIZE\n";
