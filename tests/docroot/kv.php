<?php
header('Content-Type: text/plain');
$op  = $_GET['op']  ?? '';
$key = $_GET['key'] ?? '';
$val = $_GET['val'] ?? '';
$ttl = (int) ($_GET['ttl'] ?? 0);

switch ($op) {
    case 'set':
        ephpm_kv_set($key, $val, $ttl);
        echo "ok";
        break;
    case 'get':
        $result = ephpm_kv_get($key);
        echo $result === null ? "null" : $result;
        break;
    case 'del':
        ephpm_kv_del($key);
        echo "ok";
        break;
    case 'exists':
        echo ephpm_kv_exists($key) ? "1" : "0";
        break;
    case 'pttl':
        echo ephpm_kv_pttl($key);
        break;
    case 'incr':
        // Use the atomic incr_by SAPI op rather than a get/set pair —
        // concurrent_kv_increments_are_consistent fires 20 of these in
        // parallel and would lose updates on the get→set race window.
        echo ephpm_kv_incr_by($key, 1);
        break;
    case 'incr_by':
        echo ephpm_kv_incr_by($key, (int) $val);
        break;
    case 'expire':
        ephpm_kv_expire($key, $ttl);
        echo "ok";
        break;
    case 'setnx':
        // Use the atomic SAPI op rather than an exists+set pair — the
        // old fixture lost the race under any concurrent setnx test
        // (two callers could both pass exists() before either reached
        // set()). ephpm_kv_setnx does the check-and-insert under the
        // KV store's per-key shard lock.
        echo ephpm_kv_setnx($key, $val, $ttl) ? "1" : "0";
        break;
    case 'mset':
        // val = "k1:v1,k2:v2,..."
        foreach (explode(',', $val) as $pair) {
            [$k, $v] = explode(':', $pair, 2);
            ephpm_kv_set($k, $v, 0);
        }
        echo "ok";
        break;
    case 'wait':
        // Blocking versioned wait. val = last_version; ttl is reused as the
        // timeout in MILLISECONDS here (unlike the seconds of the other
        // ops). Prints "timeout" or "<version>:<value>" ("<version>:null"
        // when the key is absent at wakeup).
        $r = ephpm_kv_wait($key, (int) $val, $ttl);
        if ($r === false) {
            echo "timeout";
        } else {
            echo $r['version'] . ':' . ($r['value'] ?? 'null');
        }
        break;
    case 'mget':
        // key = "k1,k2,..."
        $out = [];
        foreach (explode(',', $key) as $k) {
            $v = ephpm_kv_get($k);
            $out[] = $v === null ? "null" : $v;
        }
        echo implode("\n", $out);
        break;
    case 'binkeys':
        // KV stress report F-14/F-18. `key` is a per-test prefix. Prints one
        // "name=result" line per check.
        $warnings = [];
        set_error_handler(function ($no, $msg) use (&$warnings) {
            $warnings[] = $msg;
            return true;
        }, E_WARNING);
        $a = $key . "nul\0A";
        $b = $key . "nul\0B";
        ephpm_kv_set($a, 'valueA');
        ephpm_kv_set($b, 'valueB');
        $out = [];
        // Before the fix both keys were truncated at the NUL to "<prefix>nul".
        $out[] = 'nul_a=' . var_export(ephpm_kv_get($a), true);
        $out[] = 'nul_b=' . var_export(ephpm_kv_get($b), true);
        $out[] = 'nul_prefix=' . var_export(ephpm_kv_get($key . 'nul'), true);
        // A raw binary digest is not UTF-8: refused, with a warning.
        $raw = $key . md5('x', true);
        $out[] = 'raw_set=' . var_export(ephpm_kv_set($raw, 'v'), true);
        $out[] = 'raw_get=' . var_export(ephpm_kv_get($raw), true);
        $out[] = 'raw_warned=' . (count(array_filter($warnings, fn ($w) => str_contains($w, 'valid UTF-8'))) === 2 ? 'yes' : 'no');
        // A TTL that overflows milliseconds is refused, with a warning.
        $warnings = [];
        $out[] = 'huge_ttl=' . var_export(ephpm_kv_set($key . 'huge', 'v', PHP_INT_MAX), true);
        $out[] = 'huge_ttl_warned=' . (count($warnings) === 1 && str_contains($warnings[0], 'out of range') ? 'yes' : 'no');
        restore_error_handler();
        echo implode("\n", $out);
        break;
    default:
        http_response_code(400);
        echo "unknown op: $op";
}
