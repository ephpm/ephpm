//! End-to-end `ephpm db import` round-trip.
//!
//! Exercises the whole feature against a real (php_linked) ephpm binary:
//!
//! 1. Generate a small **fake** dump at test time.
//! 2. `ephpm db import <dump> --site <key> --config <cfg>` — offline
//!    direct-open into the site's own `.db` (server down).
//! 3. Start ephpm serving that site and read the imported rows back over HTTP
//!    (a vhost `index.php` SELECTs via the in-process `ephpm_db_*` bridge and
//!    echoes JSON).
//! 4. Assert the fake data round-tripped (row count + a known fake value) and
//!    that the compatibility report flagged the dump's trigger as Unsupported.
//!
//! This is a **self-managed** suite ([`DbImportFixture`]): it owns an isolated
//! per-site node under a per-test tempdir, so it never touches any shared
//! node or sibling suite (#288/#295). It needs `EPHPM_BINARY` (a real build);
//! without it every test **skips** so the crate still compile-checks on a box
//! with no binary. `cargo xtask e2e` sets `EPHPM_BINARY` for this suite (it is
//! listed in `SELF_MANAGED_SUITES`).

use ephpm_e2e::{DbImportFixture, ephpm_binary_env};
use serde::Deserialize;

/// A vhost `index.php` that SELECTs the imported `customers` table through the
/// in-process `ephpm_db_*` bridge and echoes `{status, count, names}`.
const VHOST_INDEX_PHP: &str = r#"<?php
header('Content-Type: application/json');
try {
    $rows = ephpm_db_query('SELECT id, name, email, status FROM customers ORDER BY id');
    if (!is_array($rows)) { $rows = []; }
    $names = array_map(static fn($r) => $r['name'] ?? null, $rows);
    echo json_encode(['status' => 'ok', 'count' => count($rows), 'names' => $names]);
} catch (\Throwable $e) {
    http_response_code(500);
    echo json_encode(['status' => 'error', 'message' => $e->getMessage()]);
}
"#;

/// A fake `mysqldump`-flavored dump: the realism-carrying bits (version gates,
/// `LOCK TABLES`, an `ENUM` column, a gate-split trigger) plus obviously-fake
/// customer rows. The trigger must surface as an Unsupported finding; the
/// three rows must round-trip.
fn fake_mysqldump() -> String {
    r#"-- MySQL dump 10.13  Distrib 8.0.36, for Linux (x86_64)
-- Host: localhost    Database: shop
/*!40101 SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT */;
/*!40101 SET NAMES utf8mb4 */;

DROP TABLE IF EXISTS `customers`;
CREATE TABLE `customers` (
  `id` int unsigned NOT NULL AUTO_INCREMENT,
  `name` varchar(128) COLLATE utf8mb4_unicode_ci NOT NULL,
  `email` varchar(190) COLLATE utf8mb4_unicode_ci NOT NULL,
  `status` enum('active','inactive') COLLATE utf8mb4_unicode_ci NOT NULL DEFAULT 'active',
  PRIMARY KEY (`id`)
) ENGINE=InnoDB AUTO_INCREMENT=4 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

LOCK TABLES `customers` WRITE;
/*!40000 ALTER TABLE `customers` DISABLE KEYS */;
INSERT INTO `customers` VALUES (1,'Ada Fakeington','ada@fake.test','active'),(2,'Bram Testworth','bram@fake.test','active'),(3,'Cleo Mockby','cleo@fake.test','inactive');
/*!40000 ALTER TABLE `customers` ENABLE KEYS */;
UNLOCK TABLES;

/*!50003 CREATE*/ /*!50017 DEFINER=`root`@`localhost`*/ /*!50003 TRIGGER `customers_bi` BEFORE INSERT ON `customers` FOR EACH ROW BEGIN SET NEW.name = TRIM(NEW.name); END */;

/*!40101 SET CHARACTER_SET_CLIENT=@OLD_CHARACTER_SET_CLIENT */;
-- Dump completed
"#
    .to_string()
}

/// A fake `pg_dump`-flavored dump with a `COPY … FROM stdin` data block, a
/// sequence (→ Downgraded), and a trigger (→ Unsupported). Table names are
/// deliberately unqualified so the dry-run report path does not depend on
/// PostgreSQL schema translation.
fn fake_pgdump() -> String {
    "--\n\
     -- PostgreSQL database dump\n\
     --\n\
     SET statement_timeout = 0;\n\
     SELECT pg_catalog.set_config('search_path', '', false);\n\
     CREATE SEQUENCE customers_id_seq START WITH 1 INCREMENT BY 1;\n\
     CREATE TABLE customers (id integer NOT NULL, name text NOT NULL, email text);\n\
     ALTER TABLE customers OWNER TO shop;\n\
     COPY customers (id, name, email) FROM stdin;\n\
     1\tAda Fakeington\tada@fake.test\n\
     2\tBram Testworth\t\\N\n\
     3\tCleo Mockby\tcleo@fake.test\n\
     \\.\n\
     CREATE TRIGGER customers_audit AFTER INSERT ON customers FOR EACH ROW EXECUTE FUNCTION noop();\n\
     --\n\
     -- PostgreSQL database dump complete\n\
     --\n"
        .to_string()
}

#[derive(Debug, Deserialize)]
struct Summary {
    unsupported: usize,
    downgraded: usize,
    error: usize,
}

#[derive(Debug, Deserialize)]
struct Report {
    summary: Summary,
}

#[derive(Debug, Deserialize)]
struct RowsResponse {
    status: String,
    count: usize,
    names: Vec<Option<String>>,
}

/// mysqldump → import (offline direct-open) → serve → read the rows back.
#[tokio::test]
async fn mysqldump_imports_and_rows_round_trip() {
    let Some(binary) = ephpm_binary_env() else {
        eprintln!("EPHPM_BINARY not set — skipping db_import round-trip");
        return;
    };

    let mut fixture =
        DbImportFixture::prepare(&binary, "shop", VHOST_INDEX_PHP).expect("prepare fixture");

    // Write the fake dump into the fixture's scratch dir.
    let dump = fixture.scratch_dir().join("customers.sql");
    std::fs::write(&dump, fake_mysqldump()).expect("write dump");
    let report_path = fixture.scratch_dir().join("report.json");

    // Import offline (server down). --on-error skip so the trigger is recorded
    // and skipped while the data imports; still exits 0 under skip.
    let out = fixture
        .import(
            &dump,
            &[
                "--dialect",
                "mysql",
                "--on-error",
                "skip",
                "--report",
                report_path.to_str().expect("utf-8 report path"),
            ],
        )
        .expect("run import");
    assert!(
        out.status.success(),
        "import exited {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // The compatibility report must flag the trigger as Unsupported and the
    // ENUM column as Downgraded.
    let report: Report = serde_json::from_slice(&std::fs::read(&report_path).expect("read report"))
        .expect("parse report json");
    assert!(report.summary.unsupported >= 1, "trigger should be Unsupported: {report:?}");
    assert!(report.summary.downgraded >= 1, "ENUM column should be Downgraded: {report:?}");

    // Now serve the site and read the imported rows back over HTTP.
    fixture.serve().await.expect("serve");
    let resp = reqwest::Client::new()
        .get(format!("{}/index.php", fixture.base_url()))
        .header("Host", fixture.site())
        .send()
        .await
        .expect("GET /index.php");
    let status = resp.status();
    let body = resp.text().await.expect("body");
    assert_eq!(status, 200, "serve read-back returned {status}: {body}");

    let rows: RowsResponse = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("parse rows json: {e}\nbody: {body}"));
    assert_eq!(rows.status, "ok", "bridge SELECT failed: {body}");
    assert_eq!(rows.count, 3, "all three imported rows should be readable: {body}");
    assert!(
        rows.names.iter().flatten().any(|n| n == "Bram Testworth"),
        "the known fake value did not round-trip: {body}"
    );
}

/// pg_dump → import `--dry-run`: exercises the binary's open → sniff → split →
/// sanitize → report path for a `COPY FROM stdin` dump end to end, without
/// depending on litewire's PostgreSQL *translation* maturity (the execution
/// path). Asserts the sequence downgrade and the trigger Unsupported finding,
/// and that the COPY conversion produced no error findings.
#[tokio::test]
async fn pgdump_dry_run_reports_compatibility() {
    let Some(binary) = ephpm_binary_env() else {
        eprintln!("EPHPM_BINARY not set — skipping pg dry-run report");
        return;
    };

    let fixture =
        DbImportFixture::prepare(&binary, "pgshop", VHOST_INDEX_PHP).expect("prepare fixture");
    let dump = fixture.scratch_dir().join("pg.sql");
    std::fs::write(&dump, fake_pgdump()).expect("write dump");
    let report_path = fixture.scratch_dir().join("pg-report.json");

    let out = fixture
        .import(
            &dump,
            &[
                "--dialect",
                "pg",
                "--dry-run",
                "--on-error",
                "skip",
                "--report",
                report_path.to_str().expect("utf-8 report path"),
            ],
        )
        .expect("run import");
    assert!(
        out.status.success(),
        "dry-run exited {:?}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );

    let report: Report = serde_json::from_slice(&std::fs::read(&report_path).expect("read report"))
        .expect("parse report json");
    assert!(report.summary.downgraded >= 1, "sequence should be Downgraded: {report:?}");
    assert!(report.summary.unsupported >= 1, "trigger should be Unsupported: {report:?}");
    // A dry run never executes, so there can be no execution errors — and the
    // COPY block must have converted cleanly rather than erroring.
    assert_eq!(report.summary.error, 0, "dry run must record no execution errors: {report:?}");
}
