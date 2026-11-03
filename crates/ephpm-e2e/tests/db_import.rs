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

    // Now serve the site and read the imported rows back over HTTP. The client
    // carries an explicit timeout so a serving-side hang fails this test in
    // seconds rather than wedging until the CI job's hour-long kill.
    fixture.serve().await.expect("serve");
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("build http client")
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

/// Parse `ephpm db query --format json` output into rows.
fn query_json(out: &std::process::Output) -> Vec<serde_json::Value> {
    assert!(
        out.status.success(),
        "db query exited {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("parse query json: {e}\n{stdout}"))
}

/// The v0.14.1 row-loss regression (Bug B), end to end through the real
/// binary: one extended `INSERT` of 400 rows whose text carries `\'` (an odd
/// number per row in places), `''`, `);`, `,`, a `\\` before a closing quote,
/// and HTML. Every row must land, and read back through the engine — both
/// the offline direct-open import path and the engine read path the serving
/// side uses. Before the fix the statement was refused as "malformed SQL" by
/// the tenant-path screen (the dry run was clean) and all its rows were lost.
#[tokio::test]
async fn extended_insert_with_escaped_quotes_loses_no_rows() {
    let Some(binary) = ephpm_binary_env() else {
        eprintln!("EPHPM_BINARY not set — skipping escaped-quote row-loss regression");
        return;
    };

    let mut fixture =
        DbImportFixture::prepare(&binary, "quotes", VHOST_INDEX_PHP).expect("prepare fixture");

    const ROWS: usize = 400;
    let rows: Vec<String> = (1..=ROWS)
        .map(|i| match i % 5 {
            0 => format!(r"({i},'O\'Brien & co; it\'s fine')"),
            1 => format!(r"({i},'it''s doubled); not a boundary')"),
            2 => format!(r#"({i},'<a href="x">O\'Brien & co</a>, <b>bold</b>')"#),
            3 => format!(r"({i},'trailing backslash \\')"),
            _ => format!(r"({i},'plain, with comma)')"),
        })
        .collect();
    let dump = format!(
        "-- MySQL dump 10.13  Distrib 8.0.36, for Linux (x86_64)\n\
         DROP TABLE IF EXISTS `notes`;\n\
         CREATE TABLE `notes` (`id` int NOT NULL, `body` text NOT NULL, PRIMARY KEY (`id`)) ENGINE=InnoDB;\n\
         LOCK TABLES `notes` WRITE;\n\
         INSERT INTO `notes` VALUES {};\n\
         UNLOCK TABLES;\n",
        rows.join(",")
    );
    let dump_path = fixture.scratch_dir().join("notes.sql");
    std::fs::write(&dump_path, dump).expect("write dump");

    // `--on-error stop`: a refused statement must fail the run loudly rather
    // than be skipped — and with the fix nothing is refused.
    let out = fixture.import(&dump_path, &["--dialect", "mysql"]).expect("run import");
    assert!(
        out.status.success(),
        "import exited {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // Read back offline through the engine (sqlite-native, no translation).
    let rows = query_json(
        &fixture
            .query(&["--format", "json", "SELECT COUNT(*) AS n FROM notes"])
            .expect("run db query"),
    );
    assert_eq!(rows[0]["n"], ROWS, "every row of the extended INSERT must land: {rows:?}");
    let rows = query_json(
        &fixture
            .query(&["--format", "json", "SELECT body FROM notes WHERE id IN (2, 5, 8) ORDER BY id"])
            .expect("run db query"),
    );
    // The values must come back *unescaped* exactly as MySQL would store them.
    assert_eq!(rows[0]["body"], r#"<a href="x">O'Brien & co</a>, <b>bold</b>"#);
    assert_eq!(rows[1]["body"], "O'Brien & co; it's fine");
    assert_eq!(rows[2]["body"], "trailing backslash \\");
}

/// A dump holding an invalid UTF-8 byte (a raw cp1252 right single quote,
/// 0x92). Each `--on-invalid-utf8` policy, end to end:
///
/// * the default (`reject`) fails loudly, names `table.column`, and writes
///   **nothing** — not even the clean statements before the bad row;
/// * `--source-charset windows-1252` transcodes the byte to U+2019 and the
///   row reads back through the engine;
/// * `replace` and `strip` behave per spec.
///
/// The read-back is through `db query` offline (sqlite-native), i.e. the
/// Turso read path that raises `TEXT value contains invalid UTF-8` on a value
/// that got in raw.
#[tokio::test]
async fn invalid_utf8_is_rejected_by_default_and_transcoded_on_request() {
    let Some(binary) = ephpm_binary_env() else {
        eprintln!("EPHPM_BINARY not set — skipping invalid-UTF-8 policy round-trip");
        return;
    };

    let mut fixture =
        DbImportFixture::prepare(&binary, "latin1", VHOST_INDEX_PHP).expect("prepare fixture");

    let mut dump: Vec<u8> = Vec::new();
    dump.extend_from_slice(
        b"-- MySQL dump 10.13  Distrib 8.0.36, for Linux (x86_64)\n\
          DROP TABLE IF EXISTS `quotes`;\n\
          CREATE TABLE `quotes` (`id` int NOT NULL, `title` text, `body` text NOT NULL, PRIMARY KEY (`id`)) ENGINE=InnoDB;\n\
          INSERT INTO `quotes` VALUES (1,'clean','caf\xC3\xA9 is UTF-8 already');\n\
          INSERT INTO `quotes` VALUES (2,'dash \x97 here','it\x92s a smart quote'),(3,'fine','\x93quoted\x94');\n",
    );
    let dump_path = fixture.scratch_dir().join("quotes.sql");
    std::fs::write(&dump_path, &dump).expect("write dump");
    let report_path = fixture.scratch_dir().join("utf8-report.json");

    // 1. Default policy (`--on-error stop` ⇒ reject): refused, nothing written.
    let out = fixture
        .import(&dump_path, &["--dialect", "mysql", "--report", report_path.to_str().unwrap()])
        .expect("run import");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "the default must refuse\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("quotes.body") && stdout.contains("quotes.title"),
        "the refusal must name the affected table.column:\n{stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("nothing was written") || stdout.contains("Nothing was written"),
        "the refusal must say nothing was written:\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("--source-charset"),
        "the refusal must tell the operator how to proceed:\n{stdout}"
    );
    let report: Report = serde_json::from_slice(&std::fs::read(&report_path).expect("read report"))
        .expect("parse report json");
    assert!(report.summary.error >= 2, "one blocking finding per table.column: {report:?}");
    // Nothing written: the site's database holds no `quotes` table (the clean
    // CREATE TABLE / INSERT before the bad row never ran either).
    let out = fixture
        .query(&["--format", "json", "SELECT name FROM sqlite_master WHERE name = 'quotes'"])
        .expect("run db query");
    let rows = query_json(&out);
    assert!(rows.is_empty(), "reject must write nothing, found: {rows:?}");

    // 2. `--source-charset windows-1252`: lossless, reads back.
    let out = fixture
        .import(&dump_path, &["--dialect", "mysql", "--source-charset", "windows-1252"])
        .expect("run import");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "transcode import exited {:?}\nstdout:\n{stdout}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(stdout.contains("transcoded from windows-1252"), "report must say what it did:\n{stdout}");
    let rows = query_json(
        &fixture
            .query(&["--format", "json", "SELECT id, title, body FROM quotes ORDER BY id"])
            .expect("run db query"),
    );
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[0]["body"], "café is UTF-8 already", "valid UTF-8 must not be re-decoded");
    assert_eq!(rows[1]["title"], "dash \u{2014} here");
    assert_eq!(rows[1]["body"], "it\u{2019}s a smart quote");
    assert_eq!(rows[2]["body"], "\u{201C}quoted\u{201D}");

    // 3. `replace`, `strip`, and the `--on-error skip` default (⇒ strip), on
    //    a fresh table each time.
    for (args, expected_body) in [
        (&["--on-invalid-utf8", "replace"][..], "it\u{FFFD}s a smart quote"),
        (&["--on-invalid-utf8", "strip"][..], "its a smart quote"),
        (&["--on-error", "skip"][..], "its a smart quote"),
    ] {
        let policy = args.join(" ");
        let mut full = vec!["--dialect", "mysql"];
        full.extend_from_slice(args);
        let out = fixture.import(&dump_path, &full).expect("run import");
        assert!(
            out.status.success(),
            "{policy} import exited {:?}\nstdout:\n{}\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        let rows = query_json(
            &fixture
                .query(&["--format", "json", "SELECT body FROM quotes WHERE id = 2"])
                .expect("run db query"),
        );
        assert_eq!(rows[0]["body"], expected_body, "{policy}: {rows:?}");
    }
}

/// Axis C (silent charset downgrade): valid multi-byte UTF-8 — the 3-byte
/// `⏰` (e2 8f b0) and a 4-byte emoji — must land **byte-identical** under the
/// `--source-charset utf8mb4` default, never narrowed to `?` (0x3f) by any hop.
/// Asserted on `hex()` through the engine, offline. The importer parses the
/// dump file itself (it never shells out to a `mysql` client), so there is no
/// charset-negotiating hop on the direct-open path; this pins it.
#[tokio::test]
async fn valid_multibyte_utf8_round_trips_byte_identical() {
    let Some(binary) = ephpm_binary_env() else {
        eprintln!("EPHPM_BINARY not set — skipping utf8mb4 byte-fidelity round-trip");
        return;
    };

    let mut fixture =
        DbImportFixture::prepare(&binary, "emoji", VHOST_INDEX_PHP).expect("prepare fixture");
    let dump = "-- MySQL dump 10.13  Distrib 8.0.36, for Linux (x86_64)\n\
                /*!40101 SET NAMES utf8mb4 */;\n\
                CREATE TABLE `m` (`id` int NOT NULL, `v` text NOT NULL, PRIMARY KEY (`id`)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;\n\
                INSERT INTO `m` VALUES (1,'\u{23F0} x'),(2,'\u{1F600} caf\u{E9} \u{4E2D}');\n";
    assert_eq!(&dump.as_bytes()[dump.find('\u{23F0}').unwrap()..][..5], b"\xE2\x8F\xB0 x");
    let dump_path = fixture.scratch_dir().join("emoji.sql");
    std::fs::write(&dump_path, dump).expect("write dump");

    let out = fixture
        .import(&dump_path, &["--dialect", "mysql", "--source-charset", "utf8mb4"])
        .expect("run import");
    assert!(
        out.status.success(),
        "import exited {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let rows = query_json(
        &fixture
            .query(&["--format", "json", "SELECT id, hex(v) AS h, v FROM m ORDER BY id"])
            .expect("run db query"),
    );
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0]["h"], "E28FB02078", "⏰ must not become 3F ('?'): {rows:?}");
    assert_eq!(rows[0]["v"], "\u{23F0} x");
    assert_eq!(rows[1]["h"], "F09F988020636166C3A920E4B8AD", "{rows:?}");
    assert_eq!(rows[1]["v"], "\u{1F600} caf\u{E9} \u{4E2D}");
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

    let mut fixture =
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
