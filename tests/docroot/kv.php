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
        $current = (int) (ephpm_kv_get($key) ?? 0);
        $current++;
        ephpm_kv_set($key, (string) $current, 0);
        echo $current;
        break;
    case 'incr_by':
        echo ephpm_kv_incr_by($key, (int) $val);
        break;
    case 'expire':
        ephpm_kv_expire($key, $ttl);
        echo "ok";
        break;
    case 'setnx':
        if (!ephpm_kv_exists($key)) {
            ephpm_kv_set($key, $val, $ttl);
            echo "1";
        } else {
            echo "0";
        }
        break;
    case 'mset':
        // val = "k1:v1,k2:v2,..."
        foreach (explode(',', $val) as $pair) {
            [$k, $v] = explode(':', $pair, 2);
            ephpm_kv_set($k, $v, 0);
        }
        echo "ok";
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
    default:
        http_response_code(400);
        echo "unknown op: $op";
}
