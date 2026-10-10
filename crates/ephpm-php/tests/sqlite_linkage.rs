//! Linked tests for the SQLite library PHP's `pdo_sqlite` / `sqlite3` run on.
//!
//! Two regressions are pinned here:
//!
//! 1. **Which SQLite.** PHP must run on the PHP SDK's `libsqlite3`. A Rust
//!    crate that compiles its own SQLite (rusqlite's `bundled` feature, pulled
//!    in through litewire-backend's default features until litewire#45) exports
//!    the same `sqlite3_*` symbols and silently takes over. That copy was built
//!    with `ENABLE_MEMORY_MANAGEMENT` (a single process-wide page-cache mutex,
//!    which serialized threaded PHP) and `DEFAULT_FOREIGN_KEYS=1` (foreign keys
//!    on by default, unlike stock PHP). `deny.toml` bans the crate; this test
//!    checks the resulting binary from the PHP side.
//! 2. **Memory statistics off.** Module startup must disable SQLite's memory
//!    statistics (`SQLITE_CONFIG_MEMSTATUS`), whose global mutex every
//!    allocation takes. The config call only succeeds before SQLite
//!    initializes, so a later change that touches SQLite first would quietly
//!    turn it into `SQLITE_MISUSE`.
//!
//! Requires a real libphp link (`php_linked`); compiles to nothing in stub
//! mode. Run with:
//!
//! ```text
//! cargo test -p ephpm-php --release --test sqlite_linkage -- --test-threads=1
//! ```

#![cfg(all(test, php_linked))]

use std::sync::OnceLock;

use ephpm_php::PhpRuntime;
use ephpm_php::request::PhpRequest;
use serial_test::serial;
use tempfile::TempDir;

static SCRIPT_DIR: OnceLock<TempDir> = OnceLock::new();
static INIT: OnceLock<()> = OnceLock::new();

fn init_once() {
    INIT.get_or_init(|| {
        PhpRuntime::init().expect("php_embed_init");
        PhpRuntime::finalize_for_http().expect("finalize_for_http");
    });
}

fn run_php(name: &str, body: &str) -> String {
    init_once();
    let dir = SCRIPT_DIR.get_or_init(|| TempDir::new().expect("tempdir")).path();
    let script = dir.join(name);
    std::fs::write(&script, body).expect("write test script");
    let resp = PhpRuntime::execute(PhpRequest {
        method: "GET".into(),
        uri: format!("/{name}"),
        path: format!("/{name}"),
        query_string: String::new(),
        document_root: script.parent().unwrap().to_path_buf(),
        script_filename: script,
        headers: vec![("host".into(), "localhost".into())],
        body: Vec::new(),
        content_type: None,
        remote_addr: "127.0.0.1:12345".parse().unwrap(),
        server_name: "localhost".into(),
        server_port: 8080,
        is_https: false,
        protocol: "HTTP/1.1".into(),
        env_vars: Vec::new(),
        middleware: Vec::new(),
    })
    .expect("execute");
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    String::from_utf8_lossy(&resp.body).trim().to_string()
}

#[test]
#[serial]
fn php_sqlite_is_the_sdk_library_not_a_bundled_copy() {
    let out = run_php(
        "sqlite_opts.php",
        r#"<?php
$db = new PDO('sqlite::memory:');
$opts = $db->query('PRAGMA compile_options')->fetchAll(PDO::FETCH_COLUMN);
echo implode(',', $opts), "\n";
echo $db->query('PRAGMA foreign_keys')->fetchColumn(), "\n";
echo (new SQLite3(':memory:'))->querySingle('PRAGMA foreign_keys'), "\n";
"#,
    );
    let mut lines = out.lines();
    let opts: Vec<&str> = lines.next().unwrap_or_default().split(',').collect();
    assert!(opts.contains(&"THREADSAFE=1"), "PHP's SQLite must be thread-safe: {opts:?}");
    assert!(opts.contains(&"ENABLE_FTS5"), "the SDK's SQLite ships FTS5: {opts:?}");
    for bundled_only in ["ENABLE_MEMORY_MANAGEMENT", "DEFAULT_FOREIGN_KEYS"] {
        assert!(
            !opts.contains(&bundled_only),
            "{bundled_only} is a rusqlite-bundled SQLite option: PHP is linked against a \
             second SQLite instead of the PHP SDK's libsqlite3 ({opts:?})"
        );
    }
    assert_eq!(lines.next(), Some("0"), "pdo_sqlite: foreign keys must default to off");
    assert_eq!(lines.next(), Some("0"), "sqlite3: foreign keys must default to off");
}

#[test]
#[serial]
fn sqlite_memory_statistics_are_disabled_at_startup() {
    // Open a handle first so the assertion also covers "SQLite initialized
    // after the config call", which is the order a real server sees.
    assert_eq!(run_php("sqlite_open.php", "<?php new PDO('sqlite::memory:'); echo 'ok';"), "ok");
    let rc = PhpRuntime::sqlite_memstatus_result();
    assert_eq!(
        rc, 0,
        "sqlite3_config(SQLITE_CONFIG_MEMSTATUS, 0) must succeed during module startup \
         (21 = SQLITE_MISUSE: SQLite was initialized first; -1 = never ran)"
    );
}
