//! End-to-end `ephpm php --site` as a cron runner (issue #689): the script's
//! `$_SERVER['DB_*']` carries the same per-site credentials a request gets, so
//! a stock `mysqli` connection opened from them reaches the running server's
//! MySQL listener and the site's own database.
//!
//! Over a self-managed per-site node ([`DbImportFixture`], isolated under its
//! own tempdir — never a shared node or sibling suite, #288/#295):
//!
//! 1. Seed the site offline with `ephpm db import`.
//! 2. **Offline** `ephpm php --site` — no server answers, so `$_SERVER` must
//!    carry **no** `DB_*` (a visibly absent config, never a credential that
//!    points at nothing), while the `ephpm_db_*` bridge still reads the rows.
//! 3. Start the server. **Live** `ephpm php --site` — the script reads
//!    `$_SERVER['DB_HOST'|'DB_PORT'|'DB_USER'|'DB_PASSWORD']`, opens its own
//!    `mysqli` connection with them, runs a `SELECT`, and echoes what it got.
//!    The same script over HTTP must see byte-identical `DB_*` (one map, two
//!    deliveries).
//!
//! Needs `EPHPM_BINARY` (a real build); without it every test **skips** so the
//! crate still compile-checks on a box with no binary. `cargo xtask e2e` sets
//! `EPHPM_BINARY` for this suite (listed in `SELF_MANAGED_SUITES`).

use ephpm_e2e::{DbImportFixture, ephpm_binary_env};
use serde::Deserialize;

/// The cron-style script. Reports which `DB_*` keys `$_SERVER` carries and,
/// when the four a raw-`mysqli` app needs are present, opens that connection
/// and reads a known row through it. Served over HTTP as the vhost's
/// `index.php` *and* run under `ephpm php --site`, so both deliveries are
/// compared on the same code.
const PROBE_PHP: &str = r#"<?php
header('Content-Type: application/json');
$keys = ['DB_CONNECTION','DB_HOST','DB_PORT','DB_DATABASE','DB_NAME','DB_USER','DB_USERNAME','DB_PASSWORD','DATABASE_URL'];
$present = [];
foreach ($keys as $k) { if (isset($_SERVER[$k])) { $present[$k] = $_SERVER[$k]; } }
$out = ['sapi' => PHP_SAPI, 'present' => array_keys($present), 'vars' => $present];

// The bridge path works in both modes (offline direct-open, or the wire).
try {
    $rows = ephpm_db_query('SELECT name FROM customers WHERE id = 2');
    $out['bridge'] = $rows[0]['name'] ?? null;
} catch (\Throwable $e) {
    $out['bridge_error'] = $e->getMessage();
}

// Raw mysqli from $_SERVER — exactly what a Sendy-style config.php does.
if (isset($_SERVER['DB_HOST'], $_SERVER['DB_PORT'], $_SERVER['DB_USER'], $_SERVER['DB_PASSWORD'])) {
    mysqli_report(MYSQLI_REPORT_OFF);
    $m = @mysqli_connect($_SERVER['DB_HOST'], $_SERVER['DB_USER'], $_SERVER['DB_PASSWORD'], $_SERVER['DB_NAME'], (int)$_SERVER['DB_PORT']);
    if ($m) {
        $r = mysqli_query($m, 'SELECT name FROM customers WHERE id = 2');
        $row = $r ? mysqli_fetch_assoc($r) : null;
        $out['mysqli'] = $row['name'] ?? null;
        $out['mysqli_error'] = $r ? null : mysqli_error($m);
        mysqli_close($m);
    } else {
        $out['mysqli_connect_error'] = mysqli_connect_error();
    }
}
echo json_encode($out);
"#;

/// A small fake `mysqldump` with three obviously-fake customer rows.
fn fake_mysqldump() -> String {
    r#"-- MySQL dump 10.13  Distrib 8.0.36, for Linux (x86_64)
/*!40101 SET NAMES utf8mb4 */;
DROP TABLE IF EXISTS `customers`;
CREATE TABLE `customers` (
  `id` int unsigned NOT NULL AUTO_INCREMENT,
  `name` varchar(128) NOT NULL,
  PRIMARY KEY (`id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
LOCK TABLES `customers` WRITE;
INSERT INTO `customers` VALUES (1,'Ada Fakeington'),(2,'Bram Testworth'),(3,'Cleo Mockby');
UNLOCK TABLES;
-- Dump completed
"#
    .to_string()
}

#[derive(Debug, Deserialize)]
struct Probe {
    sapi: String,
    present: Vec<String>,
    #[serde(default)]
    vars: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    bridge: Option<String>,
    #[serde(default)]
    bridge_error: Option<String>,
    #[serde(default)]
    mysqli: Option<String>,
    #[serde(default)]
    mysqli_error: Option<String>,
    #[serde(default)]
    mysqli_connect_error: Option<String>,
}

/// The full `DB_*` map the router injects, in the order the shared function
/// emits it. Pinned here too so a key dropped from the map fails the e2e, not
/// just a unit test.
const ALL_DB_KEYS: [&str; 9] = [
    "DB_CONNECTION",
    "DB_HOST",
    "DB_PORT",
    "DB_DATABASE",
    "DB_NAME",
    "DB_USER",
    "DB_USERNAME",
    "DB_PASSWORD",
    "DATABASE_URL",
];

fn parse_probe(what: &str, stdout: &[u8], stderr: &[u8]) -> Probe {
    let text = String::from_utf8_lossy(stdout);
    // The CLI's own INFO lines go to stderr; stdout is the script's JSON only.
    serde_json::from_str(text.trim()).unwrap_or_else(|e| {
        panic!(
            "{what}: could not parse the probe's JSON: {e}\nstdout:\n{text}\nstderr:\n{}",
            String::from_utf8_lossy(stderr)
        )
    })
}

/// Seed the fixture offline (releases the port reservation, so a following
/// offline `php --site` probe sees a genuinely closed port — #673).
fn seed(binary: &std::path::Path) -> DbImportFixture {
    let mut fixture = DbImportFixture::prepare(binary, "shop", PROBE_PHP).expect("prepare");
    let dump = fixture.scratch_dir().join("customers.sql");
    std::fs::write(&dump, fake_mysqldump()).expect("write dump");
    let out =
        fixture.import(&dump, &["--dialect", "mysql", "--on-error", "skip"]).expect("run import");
    assert!(
        out.status.success(),
        "import failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    fixture
}

/// Path of the vhost's `index.php` as a CLI argument (the script to run).
fn script_path(fixture: &DbImportFixture) -> String {
    fixture
        .scratch_dir()
        .join("sites")
        .join("shop")
        .join("index.php")
        .to_string_lossy()
        .into_owned()
}

/// Offline: no listener, so no `DB_*` — and the bridge still works.
#[test]
fn offline_php_site_exports_no_db_vars_but_bridge_works() {
    let Some(binary) = ephpm_binary_env() else {
        eprintln!(
            "EPHPM_BINARY not set — skipping offline_php_site_exports_no_db_vars_but_bridge_works"
        );
        return;
    };
    let fixture = seed(&binary);

    let script = script_path(&fixture);
    let out = fixture.php(&[script.as_str()]).expect("ephpm php --site (offline)");
    assert!(
        out.status.success(),
        "offline php --site exited {:?}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let probe = parse_probe("offline", &out.stdout, &out.stderr);
    assert_eq!(probe.sapi, "cli");
    assert!(
        probe.present.is_empty(),
        "offline direct-open must export no DB_* (there is no listener to reach): {:?}",
        probe.present
    );
    assert_eq!(probe.bridge.as_deref(), Some("Bram Testworth"), "bridge: {:?}", probe.bridge_error);
    assert!(probe.mysqli.is_none() && probe.mysqli_connect_error.is_none());
    // The CLI says which path it took and why raw mysqli will not work here.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("offline"), "stderr must name the offline path:\n{stderr}");
}

/// Live: `$_SERVER['DB_*']` is the request-path map, and a stock `mysqli`
/// connection opened from it reads the site's rows through the server.
#[tokio::test]
async fn live_php_site_exports_db_vars_and_mysqli_connects() {
    let Some(binary) = ephpm_binary_env() else {
        eprintln!(
            "EPHPM_BINARY not set — skipping live_php_site_exports_db_vars_and_mysqli_connects"
        );
        return;
    };
    let mut fixture = seed(&binary);
    fixture.serve().await.expect("serve");

    // What a request sees.
    let client = reqwest::Client::new();
    let body = client
        .get(format!("{}/index.php", fixture.base_url()))
        .header("Host", fixture.site())
        .send()
        .await
        .expect("GET /index.php")
        .bytes()
        .await
        .expect("body");
    let http = parse_probe("http", &body, b"");
    assert_eq!(http.present, ALL_DB_KEYS, "the request path must inject the full map");
    assert_eq!(
        http.mysqli.as_deref(),
        Some("Bram Testworth"),
        "request-path mysqli: connect={:?} query={:?}",
        http.mysqli_connect_error,
        http.mysqli_error
    );

    // What the cron invocation sees: the same map, the same working connection.
    let script = script_path(&fixture);
    let out = fixture.php(&[script.as_str()]).expect("ephpm php --site (live)");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "live php --site exited {:?}\nstderr:\n{stderr}", out.status);
    let cli = parse_probe("cli", &out.stdout, &out.stderr);
    assert_eq!(cli.sapi, "cli");
    assert_eq!(cli.present, ALL_DB_KEYS, "php --site must export the full map");
    assert_eq!(cli.vars, http.vars, "CLI and request DB_* must be byte-identical (one map)");
    assert_eq!(cli.vars.get("DB_USER").map(String::as_str), Some("shop"));
    assert_eq!(
        cli.mysqli.as_deref(),
        Some("Bram Testworth"),
        "cron-style mysqli from $_SERVER['DB_*'] must reach the site's rows: connect={:?} \
         query={:?}\nstderr:\n{stderr}",
        cli.mysqli_connect_error,
        cli.mysqli_error
    );
    assert_eq!(cli.bridge.as_deref(), Some("Bram Testworth"), "bridge: {:?}", cli.bridge_error);
    assert!(stderr.contains("exported the per-site database credentials"), "stderr:\n{stderr}");
}
