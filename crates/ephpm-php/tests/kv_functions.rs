//! PHP-level tests for the v0.13 `ephpm_kv_*` functions: hashes with
//! per-field TTL, multi-key reads/writes, compare-and-set, and `scan`.
//!
//! Each test runs a real PHP script in the embedded runtime against a known
//! store and checks the script's JSON output, so the whole path is covered:
//! argument parsing, UTF-8 checks and array conversion in `ephpm_wrapper.c`,
//! the `(pointer, length)` arrays and the thread-local list buffer across the
//! FFI boundary, and the store itself.
//!
//! Requires a real libphp link (`php_linked`); in stub mode the file compiles
//! to nothing. Run with `cargo test -p ephpm-php --test kv_functions` after
//! `cargo xtask release` (or with `PHP_SDK_PATH` set).

#![cfg(all(test, php_linked))]

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use ephpm_kv::store::{Store, StoreConfig};
use ephpm_php::request::PhpRequest;
use ephpm_php::{PhpRuntime, kv_bridge};
use serial_test::serial;
use tempfile::TempDir;

static STORE: OnceLock<Arc<Store>> = OnceLock::new();
static SCRIPT_DIR: OnceLock<TempDir> = OnceLock::new();

fn init_once() -> Arc<Store> {
    let store = STORE
        .get_or_init(|| {
            let s = Store::new(StoreConfig::default());
            PhpRuntime::init().expect("php_embed_init");
            PhpRuntime::set_kv_store(&s);
            PhpRuntime::finalize_for_http().expect("finalize_for_http");
            s
        })
        .clone();
    kv_bridge::set_site_store(Some(Arc::clone(&store)));
    store
}

/// Run `body` (PHP, without the opening tag) and return its trimmed output.
fn php(name: &str, body: &str) -> String {
    init_once();
    let dir = SCRIPT_DIR.get_or_init(|| TempDir::new().expect("tempdir")).path();
    let script: PathBuf = dir.join(format!("{name}.php"));
    std::fs::write(&script, format!("<?php\n{body}\n")).expect("write script");
    let request = PhpRequest {
        method: "GET".into(),
        uri: format!("/{name}.php"),
        path: format!("/{name}.php"),
        query_string: String::new(),
        document_root: dir.to_path_buf(),
        script_filename: script,
        headers: Vec::new(),
        body: Vec::new(),
        content_type: None,
        remote_addr: "127.0.0.1:12345".parse().unwrap(),
        server_name: "localhost".into(),
        server_port: 8080,
        is_https: false,
        protocol: "HTTP/1.1".into(),
        env_vars: Vec::new(),
        middleware: Vec::new(),
    };
    let response = PhpRuntime::execute(request).expect("php request executed");
    String::from_utf8_lossy(&response.body).trim().to_string()
}

#[test]
#[serial]
fn hash_functions_round_trip() {
    let out = php(
        "hash_round_trip",
        r#"
        $k = 'kvf:h';
        ephpm_kv_del($k);
        $r = [];
        $r['hset1'] = ephpm_kv_hset($k, 'a', '1');
        $r['hset_again'] = ephpm_kv_hset($k, 'a', 'one');
        $r['hset_map'] = ephpm_kv_hset($k, ['b' => '2', 'c' => 3, 42 => 'int-key']);
        $r['hget'] = ephpm_kv_hget($k, 'a');
        $r['hget_missing'] = ephpm_kv_hget($k, 'zz');
        $r['hmget'] = ephpm_kv_hmget($k, ['a', 'zz', 'c']);
        $all = ephpm_kv_hgetall($k);
        ksort($all);
        $r['hgetall'] = $all;
        $keys = ephpm_kv_hkeys($k); sort($keys);
        $r['hkeys'] = $keys;
        $vals = ephpm_kv_hvals($k); sort($vals);
        $r['hvals'] = $vals;
        $r['hlen'] = ephpm_kv_hlen($k);
        $r['hexists'] = [ephpm_kv_hexists($k, 'b'), ephpm_kv_hexists($k, 'nope')];
        $r['hsetnx'] = [ephpm_kv_hsetnx($k, 'a', 'x'), ephpm_kv_hsetnx($k, 'd', '4')];
        $r['hincr'] = [ephpm_kv_hincr_by($k, 'n'), ephpm_kv_hincr_by($k, 'n', 41)];
        $r['hincr_bad'] = @ephpm_kv_hincr_by($k, 'a', 1);
        $r['hdel'] = [ephpm_kv_hdel($k, 'd'), ephpm_kv_hdel($k, ['b', 'c', 'zz'])];
        $r['type'] = [ephpm_kv_type($k), ephpm_kv_type('kvf:none')];
        $r['missing'] = [ephpm_kv_hgetall('kvf:none'), ephpm_kv_hmget('kvf:none', ['x']), ephpm_kv_hlen('kvf:none')];
        echo json_encode($r);
        "#,
    );
    let got: serde_json::Value =
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    let want = serde_json::json!({
        "hset1": 1,
        "hset_again": 0,
        "hset_map": 3,
        "hget": "one",
        "hget_missing": null,
        "hmget": ["one", null, "3"],
        "hgetall": {"42": "int-key", "a": "one", "b": "2", "c": "3"},
        "hkeys": ["42", "a", "b", "c"],
        "hvals": ["2", "3", "int-key", "one"],
        "hlen": 4,
        "hexists": [true, false],
        "hsetnx": [false, true],
        "hincr": [1, 42],
        "hincr_bad": false,
        "hdel": [1, 2],
        "type": ["hash", "none"],
        "missing": [[], [null], 0],
    });
    assert_eq!(got, want, "{out}");
}

#[test]
#[serial]
fn hash_field_ttl_functions() {
    let store = init_once();
    let out = php(
        "hash_ttl",
        r#"
        $k = 'kvf:ttl';
        ephpm_kv_del($k);
        ephpm_kv_hset($k, ['a' => '1', 'b' => '2']);
        $r = [];
        $r['hexpire'] = ephpm_kv_hexpire($k, ['a', 'zz'], 100);
        $r['hexpire_one'] = ephpm_kv_hexpire($k, 'b', 50);
        $r['hexpire_bad'] = @ephpm_kv_hexpire($k, 'a', 0);
        $r['httl'] = [ephpm_kv_httl($k, 'a'), ephpm_kv_httl($k, 'zz')];
        $r['hpttl'] = ephpm_kv_hpttl($k, 'b');
        $r['hpersist'] = [ephpm_kv_hpersist($k, 'a'), ephpm_kv_hpersist($k, 'a')];
        $r['httl_after'] = ephpm_kv_httl($k, 'a');
        $r['key_expire'] = ephpm_kv_expire($k, 300);
        $r['key_ttl'] = ephpm_kv_ttl($k);
        $r['key_persist'] = ephpm_kv_persist($k);
        $r['key_ttl_after'] = ephpm_kv_ttl($k);
        echo json_encode($r);
        "#,
    );
    let got: serde_json::Value =
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(got["hexpire"], 1, "{out}");
    assert_eq!(got["hexpire_one"], 1, "{out}");
    assert_eq!(got["hexpire_bad"], false, "{out}");
    assert_eq!(got["httl"], serde_json::json!([100, -2]), "{out}");
    let hpttl = got["hpttl"].as_i64().unwrap();
    assert!(hpttl > 49_000 && hpttl <= 50_000, "{out}");
    assert_eq!(got["hpersist"], serde_json::json!([true, false]), "{out}");
    assert_eq!(got["httl_after"], -1, "{out}");
    assert_eq!(got["key_expire"], true, "{out}");
    assert_eq!(got["key_ttl"], 300, "{out}");
    assert_eq!(got["key_persist"], true, "{out}");
    assert_eq!(got["key_ttl_after"], -1, "{out}");

    // A field TTL elapses: the field disappears from PHP's view.
    store.hexpire(
        "kvf:ttl",
        &["a"],
        std::time::Duration::from_millis(1),
        ephpm_kv::store::ExpireCondition::Always,
    );
    std::thread::sleep(std::time::Duration::from_millis(20));
    let out = php(
        "hash_ttl_gone",
        r"echo json_encode([ephpm_kv_hget('kvf:ttl', 'a'), ephpm_kv_hkeys('kvf:ttl')]);",
    );
    assert_eq!(out, r#"[null,["b"]]"#);
}

#[test]
#[serial]
fn string_and_key_functions() {
    let out = php(
        "strings",
        r#"
        foreach (['kvf:a', 'kvf:b', 'kvf:c', 'kvf:lock', 'kvf:app'] as $k) { ephpm_kv_del($k); }
        $r = [];
        $r['mset'] = ephpm_kv_mset(['kvf:a' => 'A', 'kvf:b' => 2]);
        $r['mget'] = ephpm_kv_mget(['kvf:a', 'kvf:nope', 'kvf:b']);
        $r['mget_empty'] = ephpm_kv_mget([]);
        $r['mset_ttl'] = ephpm_kv_mset(['kvf:c' => 'C'], 60);
        $r['ttl_c'] = ephpm_kv_ttl('kvf:c');
        $r['getex'] = ephpm_kv_getex('kvf:a', 30);
        $r['ttl_a'] = ephpm_kv_ttl('kvf:a');
        $r['getex_persist'] = ephpm_kv_getex('kvf:a', 0);
        $r['ttl_a_after'] = ephpm_kv_ttl('kvf:a');
        $r['getex_missing'] = ephpm_kv_getex('kvf:nope', 10);
        $r['persist'] = [ephpm_kv_persist('kvf:c'), ephpm_kv_persist('kvf:c')];
        $r['getdel'] = [ephpm_kv_getdel('kvf:b'), ephpm_kv_getdel('kvf:b')];
        $r['append'] = [ephpm_kv_append('kvf:app', 'ab'), ephpm_kv_append('kvf:app', 'cd')];
        $r['strlen'] = [ephpm_kv_strlen('kvf:app'), ephpm_kv_strlen('kvf:nope')];
        $r['type'] = ephpm_kv_type('kvf:app');
        // Lock: acquire, extend, refuse a stale token, release.
        $r['lock'] = [
            ephpm_kv_setnx('kvf:lock', 'tok', 10),
            ephpm_kv_set_if_eq('kvf:lock', 'tok', 'tok', 100),
            ephpm_kv_ttl('kvf:lock'),
            ephpm_kv_set_if_eq('kvf:lock', 'stale', 'mine', 100),
            ephpm_kv_set_if_eq('kvf:lock', 'tok', 'tok2', -1),
            ephpm_kv_ttl('kvf:lock') > 90,
            ephpm_kv_set_if_eq('kvf:nope', 'x', 'y'),
            @ephpm_kv_set_if_eq('kvf:lock', 'tok2', 'tok3', -5),
            ephpm_kv_del_if_eq('kvf:lock', 'tok2'),
        ];
        echo json_encode($r);
        "#,
    );
    let got: serde_json::Value =
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    let want = serde_json::json!({
        "mset": true,
        "mget": ["A", null, "2"],
        "mget_empty": [],
        "mset_ttl": true,
        "ttl_c": 60,
        "getex": "A",
        "ttl_a": 30,
        "getex_persist": "A",
        "ttl_a_after": -1,
        "getex_missing": null,
        "persist": [true, false],
        "getdel": ["2", null],
        "append": [2, 4],
        "strlen": [4, 0],
        "type": "string",
        "lock": [true, true, 100, false, true, true, false, false, true],
    });
    assert_eq!(got, want, "{out}");
}

#[test]
#[serial]
fn scan_visits_every_key() {
    let store = init_once();
    for i in 0..500 {
        store.set(format!("kvf:scan:{i}"), b"v".to_vec(), None);
    }
    store.hset("kvf:scan:hash", "f", b"v".to_vec()).unwrap();
    let out = php(
        "scan",
        r#"
        $seen = [];
        $cursor = 0;
        $calls = 0;
        do {
            [$cursor, $keys] = ephpm_kv_scan($cursor, 'kvf:scan:*', 20);
            foreach ($keys as $k) { $seen[$k] = true; }
            $calls++;
        } while ($cursor !== 0 && $calls < 10000);
        echo json_encode([count($seen), is_int($cursor), @ephpm_kv_scan(-1)]);
        "#,
    );
    assert_eq!(out, "[501,true,false]");
}

#[test]
#[serial]
fn bad_arguments_fail_with_their_failure_values() {
    let out = php(
        "bad_args",
        r#"
        $bad = "k\xff";
        echo json_encode([
            @ephpm_kv_hget('kvf:x', $bad),
            @ephpm_kv_hset('kvf:x', [$bad => 'v']),
            @ephpm_kv_hset('kvf:x', 'f'),
            @ephpm_kv_mget(['ok', $bad]),
            @ephpm_kv_mset([$bad => 'v']),
            @ephpm_kv_getex('kvf:x', -1),
            @ephpm_kv_hdel('kvf:x', [$bad]),
            ephpm_kv_exists('kvf:x'),
        ]);
        "#,
    );
    assert_eq!(out, "[null,false,false,false,false,null,0,false]");
}
