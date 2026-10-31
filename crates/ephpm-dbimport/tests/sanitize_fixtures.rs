//! Golden-fixture tests: run a real `mysqldump` / `pg_dump` excerpt through
//! the reader + sanitizer and assert both the emitted statement stream and the
//! compatibility report. These are the acceptance cases from the spec —
//! multi-row INSERT, `/*! */` gates, `LOCK TABLES`, ENUM/SET, `COPY FROM
//! stdin`, trigger, procedure, sequence.

use std::io::Cursor;

use ephpm_dbimport::{
    CompatReport, Dialect, DumpReader, FindingKind, SanitizeOutcome, Sanitizer, sniff,
};

/// Run a dump through reader + sanitizer, returning every emitted statement
/// (in order) and the accumulated report.
fn run(dump: &str, dialect: Dialect) -> (Vec<String>, CompatReport) {
    let reader = DumpReader::new(Cursor::new(dump.as_bytes().to_vec()), dialect);
    let sanitizer = Sanitizer::new(dialect);
    let mut emitted = Vec::new();
    let mut report = CompatReport::new();
    for stmt in reader {
        let stmt = stmt.expect("read");
        match sanitizer.sanitize(&stmt) {
            SanitizeOutcome::Emit(sqls) => emitted.extend(sqls),
            SanitizeOutcome::Convert(sqls, findings) => {
                emitted.extend(sqls);
                for f in findings {
                    report.push(f);
                }
            }
            SanitizeOutcome::Skip(f) => report.push(f),
            SanitizeOutcome::Drop(reason) => {
                report.push(ephpm_dbimport::Finding::new(
                    FindingKind::Dropped,
                    "",
                    reason,
                    stmt.line,
                ));
            }
        }
    }
    (emitted, report)
}

const MYSQL: &str = include_str!("fixtures/mysql/wordpress.sql");
const PG: &str = include_str!("fixtures/pg/shop.sql");

#[test]
fn mysql_dialect_is_sniffed() {
    assert_eq!(sniff(MYSQL.as_bytes()), Some(Dialect::MySql));
}

#[test]
fn pg_dialect_is_sniffed() {
    assert_eq!(sniff(PG.as_bytes()), Some(Dialect::Pg));
}

#[test]
fn mysql_fixture_emits_table_and_data_and_reports() {
    let (emitted, report) = run(MYSQL, Dialect::MySql);
    let all = emitted.join("\n");

    // The CREATE TABLE is emitted with storage options stripped …
    let create = emitted.iter().find(|s| s.contains("CREATE TABLE")).expect("CREATE TABLE emitted");
    let up = create.to_ascii_uppercase();
    assert!(!up.contains("ENGINE="), "ENGINE= not stripped:\n{create}");
    assert!(!up.contains("CHARSET="), "CHARSET= not stripped:\n{create}");
    assert!(!up.contains("ROW_FORMAT="), "ROW_FORMAT= not stripped:\n{create}");
    assert!(!up.contains("AUTO_INCREMENT="), "AUTO_INCREMENT= option not stripped:\n{create}");
    // … and ENUM/SET rewritten to TEXT, full-text index removed.
    assert!(create.contains("`post_status` TEXT"), "ENUM→TEXT missing:\n{create}");
    assert!(create.contains("`comment_status` TEXT"), "SET→TEXT missing:\n{create}");
    assert!(!up.contains("FULLTEXT"), "full-text index kept:\n{create}");
    // The ordinary secondary KEY survives for litewire to expand.
    assert!(create.contains("KEY `type_status_date`"), "secondary KEY dropped:\n{create}");

    // The multi-row INSERT passes through verbatim (one statement, three rows,
    // the escaped `;` inside the strings intact).
    let insert = emitted.iter().find(|s| s.starts_with("INSERT INTO `wp_posts`")).expect("INSERT");
    assert_eq!(insert.matches("),(").count(), 2, "three rows should remain one INSERT: {insert}");
    assert!(insert.contains(r"it\'s great"), "escaped quote data lost: {insert}");

    // No LOCK/UNLOCK/SET/ALTER … KEYS reached the emitted stream.
    let up_all = all.to_ascii_uppercase();
    assert!(!up_all.contains("LOCK TABLES"));
    assert!(!up_all.contains("DISABLE KEYS"));
    assert!(!up_all.contains("SET NAMES"));

    // Report: trigger + procedure Unsupported, ENUM + SET + full-text present.
    let summary = report.summary();
    assert_eq!(summary.unsupported, 3, "trigger + procedure + full-text: {summary:?}");
    assert!(
        report.significant().any(|f| f.detail.to_ascii_lowercase().contains("trigger")),
        "trigger not reported"
    );
    assert!(
        report.significant().any(|f| f.detail.to_ascii_lowercase().contains("procedure")),
        "procedure not reported"
    );
    assert!(summary.downgraded >= 1, "ENUM/SET downgrade not reported: {summary:?}");
}

#[test]
fn pg_fixture_converts_copy_and_reports() {
    let (emitted, report) = run(PG, Dialect::Pg);
    let all = emitted.join("\n");

    // COPY → INSERT with \N→NULL and spaces preserved.
    let insert = emitted
        .iter()
        .find(|s| s.starts_with("INSERT INTO public.customers"))
        .expect("COPY converted to INSERT");
    assert!(insert.contains("('1', 'Ada Lovelace', 'ada@example.test')"), "row 1: {insert}");
    assert!(insert.contains("('2', 'Alan Turing', NULL)"), "\\N → NULL: {insert}");
    assert!(insert.contains("('3', 'Grace Hopper', 'grace@example.test')"), "row 3: {insert}");

    // The CREATE TABLE survives for litewire to translate.
    assert!(emitted.iter().any(|s| s.contains("CREATE TABLE public.customers")), "no CREATE TABLE");

    // Session setup, ownership, comments, grants, extension are all dropped.
    let up = all.to_ascii_uppercase();
    assert!(!up.contains("SET STATEMENT_TIMEOUT"));
    assert!(!up.contains("OWNER TO"));
    assert!(!up.contains("COMMENT ON"));
    assert!(!up.contains("GRANT "));
    assert!(!up.contains("CREATE EXTENSION"));

    // The sequence is a Downgraded finding, not an emitted statement.
    assert!(!up.contains("CREATE SEQUENCE"), "sequence should not be emitted");
    assert_eq!(report.summary().downgraded, 1, "sequence downgrade: {:?}", report.summary());
    assert!(report.significant().any(|f| f.detail.to_ascii_lowercase().contains("sequence")));
}
