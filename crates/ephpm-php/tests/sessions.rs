//! Integration tests for the native `session.save_handler = ephpm`.
//!
//! These tests boot the embedded PHP runtime and run a sequence of
//! "requests" against it, mimicking a real browser session: write a
//! value, read it back through a fresh request, destroy, confirm gone.
//!
//! They require a real libphp link (`php_linked`) because `php_embed_init`
//! is what registers our `ps_module ps_mod_ephpm` with the session
//! extension. In stub mode the whole file compiles to nothing, so
//! `cargo build` (no PHP SDK) still passes.
//!
//! Run with: `cargo test -p ephpm-php --test sessions` after building the
//! PHP SDK (`cargo xtask release` will have done this).
//!
//! Each test reuses the process-wide PHP runtime — `php_embed_init` is
//! a once-per-process operation. `serial_test` ensures the tests do not
//! step on each other's session ids.

#![cfg(all(test, php_linked))]

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use ephpm_kv::store::{Store, StoreConfig};
use ephpm_php::request::PhpRequest;
use ephpm_php::response::PhpResponse;
use ephpm_php::{PhpRuntime, kv_bridge};
use serial_test::serial;
use tempfile::TempDir;

// ── Shared process-wide PHP runtime ────────────────────────────────────

/// One Store, one PhpRuntime per test process. `OnceLock` guarantees we
/// only init PHP once even across the many `#[test]` functions that share
/// this binary.
static SESSION_STORE: OnceLock<Arc<Store>> = OnceLock::new();
static SCRIPT_DIR: OnceLock<TempDir> = OnceLock::new();

fn init_once() -> Arc<Store> {
    let store = SESSION_STORE
        .get_or_init(|| {
            let s = Store::new(StoreConfig::default());
            PhpRuntime::init().expect("php_embed_init");
            PhpRuntime::set_kv_store(&s);
            PhpRuntime::finalize_for_http().expect("finalize_for_http");
            s
        })
        .clone();

    // Bind the per-thread KV "site store" to the same instance so SAPI
    // callbacks (and through them, our session handler) resolve to it.
    kv_bridge::set_site_store(Some(Arc::clone(&store)));
    store
}

fn script_dir() -> &'static std::path::Path {
    SCRIPT_DIR.get_or_init(|| TempDir::new().expect("tempdir for session test scripts")).path()
}

/// Write a PHP script to a tempfile and return its absolute path.
fn write_script(name: &str, body: &str) -> PathBuf {
    let path = script_dir().join(name);
    std::fs::write(&path, body).expect("write test script");
    path
}

/// Build a synthetic `PhpRequest` that targets `script` with a single
/// `Cookie: PHPSESSID=<sid>` header (so PHP's session module picks the
/// id up via `$_COOKIE`).
fn make_request(script: PathBuf, sid: Option<&str>) -> PhpRequest {
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(s) = sid {
        headers.push(("Cookie".to_string(), format!("PHPSESSID={s}")));
    }
    PhpRequest {
        method: "GET".into(),
        uri: format!("/{}", script.file_name().unwrap().to_string_lossy()),
        path: format!("/{}", script.file_name().unwrap().to_string_lossy()),
        query_string: String::new(),
        document_root: script.parent().unwrap().to_path_buf(),
        script_filename: script,
        headers,
        body: Vec::new(),
        content_type: None,
        remote_addr: "127.0.0.1:12345".parse().unwrap(),
        server_name: "localhost".into(),
        server_port: 8080,
        is_https: false,
        protocol: "HTTP/1.1".into(),
        env_vars: vec![
            // Force the ephpm save handler regardless of any inherited php.ini.
            // session.use_strict_mode mirrors what the task spec asked for.
            ("PHP_INI_SCAN_DIR".into(), String::new()),
        ],
        middleware: Vec::new(),
    }
}

/// Execute one request and pull out a trimmed body.
fn run(script: PathBuf, sid: Option<&str>) -> PhpResponse {
    let req = make_request(script, sid);
    PhpRuntime::execute(req).expect("php request executed")
}

fn body_str(resp: &PhpResponse) -> String {
    String::from_utf8_lossy(&resp.body).trim().to_string()
}

// The ini directives every test needs. Inlined into each script so the
// ephpm save handler is selected regardless of how the embed SAPI's
// default php.ini was configured.
//
// No `session.use_only_cookies` here: disabling it is deprecated since PHP 8.4,
// and the deprecation notice is output — it "sends headers", after which every
// later session ini_set() and session_start() fails. The scripts pass the id via
// session_id() anyway, so the directive was never needed.
const SESSION_INI: &str = r#"
ini_set('session.save_handler', 'ephpm');
ini_set('session.use_strict_mode', '1');
ini_set('session.use_cookies', '0');
ini_set('session.cache_limiter', '');
ini_set('session.gc_maxlifetime', '600');
"#;

// ── Tests ──────────────────────────────────────────────────────────────

#[test]
#[serial]
fn write_then_read_round_trip() {
    let store = init_once();
    let sid = "ephpmtestsid_writeread_aaaaaaaaaa";

    // Seed the store with an empty payload so use_strict_mode accepts the SID.
    store.set(format!("session:{sid}"), b"".to_vec(), None);

    let write_path = write_script(
        "session_write.php",
        &format!(
            r#"<?php
{SESSION_INI}
session_id('{sid}');
session_start();
$_SESSION['x'] = 'hello';
$_SESSION['n'] = 42;
session_write_close();
echo 'WROTE';
"#,
        ),
    );
    let resp = run(write_path, Some(sid));
    assert_eq!(body_str(&resp), "WROTE");

    // The KV key should now hold PHP's serialised session blob.
    let raw = store.get(&format!("session:{sid}")).expect("session blob stored");
    assert!(!raw.is_empty(), "expected non-empty session blob, got 0 bytes");

    let read_script = write_script(
        "session_read.php",
        &format!(
            r#"<?php
{SESSION_INI}
session_id('{sid}');
session_start();
echo $_SESSION['x'] . '|' . $_SESSION['n'];
"#,
        ),
    );
    let resp = run(read_script, Some(sid));
    assert_eq!(body_str(&resp), "hello|42");
}

#[test]
#[serial]
fn destroy_removes_key() {
    let store = init_once();
    let sid = "ephpmtestsid_destroy_bbbbbbbbbbbb";

    store.set(format!("session:{sid}"), b"x|s:1:\"y\";".to_vec(), None);

    let destroy_script = write_script(
        "session_destroy.php",
        &format!(
            r#"<?php
{SESSION_INI}
session_id('{sid}');
session_start();
session_destroy();
echo 'GONE';
"#,
        ),
    );
    let resp = run(destroy_script, Some(sid));
    assert_eq!(body_str(&resp), "GONE");

    assert!(
        store.get(&format!("session:{sid}")).is_none(),
        "session key should be deleted after session_destroy()"
    );
}

#[test]
#[serial]
fn write_sets_ttl_from_gc_maxlifetime() {
    let store = init_once();
    let sid = "ephpmtestsid_ttl_cccccccccccccccc";

    store.set(format!("session:{sid}"), b"".to_vec(), None);

    let script = write_script(
        "session_ttl.php",
        &format!(
            r#"<?php
{SESSION_INI}
ini_set('session.gc_maxlifetime', '120');
session_id('{sid}');
session_start();
$_SESSION['v'] = 1;
session_write_close();
echo 'OK';
"#,
        ),
    );
    let _ = run(script, Some(sid));

    let pttl = store.pttl(&format!("session:{sid}")).expect("ttl recorded");
    assert!(
        pttl > 0 && pttl <= 120_000,
        "expected TTL in (0, 120_000] ms (gc_maxlifetime=120s); got {pttl}",
    );
}

/// The session lock is released with a compare-and-delete against a
/// per-acquisition token, not a plain DEL. Simulates the TTL-overrun case: while
/// the request holds the session, the lock key is taken over by "someone else"
/// (what an expiry + re-acquire by a concurrent request leaves behind). The
/// original holder's release must then leave the new holder's lock alone.
#[test]
#[serial]
fn session_lock_release_cannot_delete_a_lock_it_no_longer_holds() {
    let store = init_once();
    let sid = "ephpmtestsid_lockcad_eeeeeeeeeeee";
    let lock_key = format!("session_lock:{sid}");
    store.set(format!("session:{sid}"), b"".to_vec(), None);

    let script = write_script(
        "session_lock_cad.php",
        &format!(
            r#"<?php
{SESSION_INI}
session_id('{sid}');
session_start();
$held = ephpm_kv_get('{lock_key}');
ephpm_kv_set('{lock_key}', 'someone-else');
session_write_close();
echo $held, '|', var_export(ephpm_kv_get('{lock_key}'), true);
"#,
        ),
    );
    let body = body_str(&run(script, Some(sid)));
    let (held, after) = body.split_once('|').expect("two fields");

    assert_eq!(held.len(), 32, "lock value must be a per-acquisition token, got {held:?}");
    assert!(held.bytes().all(|b| b.is_ascii_hexdigit()), "token must be hex, got {held:?}");
    assert_eq!(after, "'someone-else'", "release must not delete a lock taken over by another");

    // A normal acquire/release cycle still cleans up after itself.
    store.remove(&lock_key);
    let script = write_script(
        "session_lock_cycle.php",
        &format!(
            r#"<?php
{SESSION_INI}
session_id('{sid}');
session_start();
session_write_close();
echo var_export(ephpm_kv_get('{lock_key}'), true);
"#,
        ),
    );
    assert_eq!(body_str(&run(script, Some(sid))), "NULL", "own lock must be released");
}

#[test]
#[serial]
fn validate_sid_rejects_unknown_in_strict_mode() {
    let _store = init_once();

    // use_strict_mode = 1 + an SID we never seeded: PHP must invent a new SID
    // rather than accept the supplied one. We detect this indirectly by
    // observing that the original SID's KV key is still absent after the
    // request — only the freshly-generated SID gets written.
    let forged = "ephpmtestsid_forged_dddddddddddd";
    let script = write_script(
        "session_strict.php",
        &format!(
            r#"<?php
{SESSION_INI}
session_id('{forged}');
session_start();
$_SESSION['v'] = 'created';
session_write_close();
echo session_id();
"#,
        ),
    );
    let resp = run(script, Some(forged));
    let returned_sid = body_str(&resp);
    assert_ne!(
        returned_sid, forged,
        "strict mode must reject unseeded SID and regenerate; got back {returned_sid}",
    );
    assert!(
        SESSION_STORE.get().unwrap().get(&format!("session:{forged}")).is_none(),
        "forged SID should never have been written",
    );
}
