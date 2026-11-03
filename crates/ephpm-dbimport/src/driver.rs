//! The import driver: pull statements off a [`DumpReader`], run each through
//! the [`Sanitizer`], and push the result to a [`StatementSink`], batching
//! executions into transactions and honoring the error policy.
//!
//! The driver is sink-agnostic and dry-run aware: pass `None` for the sink to
//! parse-and-report with no execution and no backend. That is exactly what the
//! `--dry-run` CLI flag does, and what the fixture tests use with an in-memory
//! recording sink.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::BufRead;

use crate::attribute::{Located, locate};
use crate::reader::ReadError;
use crate::report::{CompatReport, Finding, FindingKind};
use crate::sanitizer::{SanitizeOutcome, Sanitizer, create_table_columns};
use crate::sink::{SinkError, StatementSink};
use crate::utf8::Utf8Policy;
use crate::{DumpReader, RawStatement};

/// What to do when a statement fails at execution time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnError {
    /// Stop the import at the first blocking finding or execution error, roll
    /// back the open batch, and report non-zero.
    Stop,
    /// Record the failure as a [`Finding`] and keep going.
    Skip,
}

/// Knobs for one import run.
#[derive(Debug, Clone)]
pub struct ImportOptions {
    /// How many executed statements go in one transaction (≥ 1).
    pub batch_size: usize,
    /// Error policy.
    pub on_error: OnError,
    /// Hold `CREATE INDEX` statements and replay them after all data.
    pub defer_indexes: bool,
    /// Record [`FindingKind::Dropped`] safe-no-op findings (otherwise they are
    /// counted but not listed).
    pub verbose: bool,
    /// What to do with input bytes that are not valid UTF-8. The driver
    /// applies it (the reader it is handed must have been built with the
    /// same policy — see [`DumpReader::with_utf8_policy`]): under
    /// [`Utf8Policy::Reject`] a statement carrying an invalid run is never
    /// executed and the run aborts the import (a dry run keeps scanning so
    /// the report is complete); under the other policies the run is counted
    /// and reported as a [`FindingKind::Downgraded`] per `table.column`.
    pub utf8: Utf8Policy,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            batch_size: 1000,
            on_error: OnError::Stop,
            defer_indexes: false,
            verbose: false,
            utf8: Utf8Policy::Reject,
        }
    }
}

/// How an import run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportOutcome {
    /// Every statement was processed.
    Completed,
    /// The run stopped early under [`OnError::Stop`] at a blocking finding or
    /// execution error.
    Aborted,
}

/// Tallies from one import run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportStats {
    /// Statements the sanitizer produced for execution (before deferral).
    pub emitted: usize,
    /// Statements actually executed against the sink (0 in a dry run).
    pub executed: usize,
    /// Statements dropped as safe no-ops.
    pub dropped: usize,
    /// `CREATE INDEX` statements held back by `--defer-indexes`.
    pub deferred: usize,
    /// Transactions committed.
    pub batches: usize,
    /// Values (string literals / `COPY` fields) that contained at least one
    /// byte run that was not valid UTF-8.
    pub invalid_utf8_values: usize,
    /// Rows (tuples / `COPY` lines) containing at least one such value.
    pub invalid_utf8_rows: usize,
}

/// A fatal error that stops the whole import (as opposed to a per-statement
/// error, which becomes a [`FindingKind::Error`] in the report).
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// The dump could not be read/split.
    #[error(transparent)]
    Read(#[from] ReadError),
    /// Transaction control (`BEGIN`/`COMMIT`) failed — the import cannot
    /// continue safely.
    #[error("transaction control failed: {0}")]
    Transaction(#[source] SinkError),
}

/// Whether a statement is a `CREATE [UNIQUE] INDEX` (eligible for deferral).
fn is_create_index(sql: &str) -> bool {
    let head = sql.trim_start().to_ascii_uppercase();
    head.starts_with("CREATE INDEX") || head.starts_with("CREATE UNIQUE INDEX")
}

/// Run an import. `sink` is `None` for a dry run (parse + sanitize + report,
/// no execution). Findings are appended to `report`; counts come back in
/// [`ImportStats`].
///
/// # Errors
///
/// Returns [`ImportError::Read`] if the dump cannot be read and
/// [`ImportError::Transaction`] if the sink rejects `BEGIN`/`COMMIT`.
/// Per-statement execution failures are recorded as findings, not returned.
pub fn run_import<'a, R: BufRead>(
    reader: DumpReader<R>,
    sanitizer: &'a Sanitizer,
    sink: Option<&'a mut dyn StatementSink>,
    opts: &ImportOptions,
    report: &'a mut CompatReport,
) -> Result<(ImportStats, ImportOutcome), ImportError> {
    let mut run = Run {
        sanitizer,
        sink,
        batch_size: opts.batch_size.max(1),
        on_error: opts.on_error,
        defer_indexes: opts.defer_indexes,
        verbose: opts.verbose,
        utf8: opts.utf8,
        report,
        stats: ImportStats::default(),
        in_txn: false,
        batch_n: 0,
        deferred: Vec::new(),
        schema: HashMap::new(),
        utf8_tally: BTreeMap::new(),
        utf8_rejected: false,
    };

    let mut outcome = ImportOutcome::Completed;
    for stmt in reader {
        let stmt = stmt?;
        if run.handle(&stmt)? == Flow::Abort {
            outcome = ImportOutcome::Aborted;
            break;
        }
    }
    // A dry run under `Reject` scans to the end so the report names every
    // affected table/column, then fails as a whole.
    if run.utf8_rejected {
        outcome = ImportOutcome::Aborted;
    }

    if outcome == ImportOutcome::Aborted {
        run.rollback_quiet();
        run.emit_utf8_findings();
        return Ok((run.stats, outcome));
    }

    // Replay deferred indexes (if any) after all data has landed.
    let deferred = std::mem::take(&mut run.deferred);
    for sql in deferred {
        // A deferred index has no original line; anchor at 0.
        if run.exec_recording(&sql, 0)? == Flow::Abort {
            run.rollback_quiet();
            run.emit_utf8_findings();
            return Ok((run.stats, ImportOutcome::Aborted));
        }
    }

    run.commit_final()?;
    run.emit_utf8_findings();
    Ok((run.stats, outcome))
}

/// Per-`table.column` tally of invalid-UTF-8 runs for the report.
#[derive(Debug, Default)]
struct Utf8Tally {
    /// Values (literals / fields) affected — one per affected row, since a
    /// column occurs once per row.
    values: usize,
    /// Dump line of the first affected statement.
    first_line: u64,
}

/// Control flow out of a per-statement step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Abort,
}

/// Mutable state threaded through one run; owning it in a struct avoids the
/// borrow tangle of passing a dozen `&mut` locals to helpers.
struct Run<'a> {
    sanitizer: &'a Sanitizer,
    sink: Option<&'a mut dyn StatementSink>,
    batch_size: usize,
    on_error: OnError,
    defer_indexes: bool,
    verbose: bool,
    utf8: Utf8Policy,
    report: &'a mut CompatReport,
    stats: ImportStats,
    in_txn: bool,
    batch_n: usize,
    deferred: Vec<String>,
    /// Column names per table key, remembered from `CREATE TABLE`s, so a run
    /// in a column-list-less `INSERT` can be named by column.
    schema: HashMap<String, Vec<String>>,
    /// Invalid-UTF-8 tallies keyed by `table.column`.
    utf8_tally: BTreeMap<String, Utf8Tally>,
    /// Set once a run was met under [`Utf8Policy::Reject`].
    utf8_rejected: bool,
}

impl Run<'_> {
    /// Process one raw statement.
    fn handle(&mut self, stmt: &RawStatement) -> Result<Flow, ImportError> {
        // Remember column order from DDL (cheap: one keyword check per
        // statement, the body parse only for a CREATE TABLE).
        if let Some((table, cols)) = create_table_columns(&stmt.sql, self.sanitizer.dialect()) {
            self.schema.insert(table, cols);
        }

        // The UTF-8 contract comes before anything else: a statement carrying
        // an invalid run is never executed under `Reject`.
        if !stmt.invalid_utf8.is_empty() {
            self.tally_invalid_utf8(stmt);
            if self.utf8.is_reject() {
                self.utf8_rejected = true;
                // A dry run keeps scanning so the report is complete; a real
                // run stops here, before this statement reaches the sink.
                return Ok(if self.sink.is_some() { Flow::Abort } else { Flow::Continue });
            }
        }

        match self.sanitizer.sanitize(stmt) {
            SanitizeOutcome::Drop(reason) => {
                self.stats.dropped += 1;
                if self.verbose {
                    self.report.push(Finding::new(FindingKind::Dropped, "", reason, stmt.line));
                }
                Ok(Flow::Continue)
            }
            SanitizeOutcome::Skip(finding) => {
                let blocking = finding.kind.is_blocking();
                self.report.push(finding);
                if blocking && self.on_error == OnError::Stop {
                    Ok(Flow::Abort)
                } else {
                    Ok(Flow::Continue)
                }
            }
            SanitizeOutcome::Convert(sqls, findings) => {
                // A Convert always produced usable SQL; emit it and record the
                // finding(s). It never aborts mid-stream even under Stop — the
                // final exit code still reflects report.has_blocking().
                for finding in findings {
                    self.report.push(finding);
                }
                self.emit_all(sqls, stmt.line)
            }
            SanitizeOutcome::Emit(sqls) => self.emit_all(sqls, stmt.line),
        }
    }

    fn emit_all(&mut self, sqls: Vec<String>, line: u64) -> Result<Flow, ImportError> {
        for sql in sqls {
            self.stats.emitted += 1;
            if self.defer_indexes && is_create_index(&sql) {
                self.stats.deferred += 1;
                self.deferred.push(sql);
                continue;
            }
            if self.exec_recording(&sql, line)? == Flow::Abort {
                return Ok(Flow::Abort);
            }
        }
        Ok(Flow::Continue)
    }

    /// Execute one SQL statement (no-op in a dry run), recording an execution
    /// error as a [`FindingKind::Error`] and applying the error policy.
    fn exec_recording(&mut self, sql: &str, line: u64) -> Result<Flow, ImportError> {
        let Some(sink) = self.sink.as_deref_mut() else {
            // Dry run: nothing executes.
            return Ok(Flow::Continue);
        };

        if !self.in_txn {
            sink.begin().map_err(ImportError::Transaction)?;
            self.in_txn = true;
        }

        match sink.exec(sql) {
            Ok(()) => {
                self.stats.executed += 1;
                self.batch_n += 1;
                if self.batch_n >= self.batch_size {
                    self.commit_batch()?;
                }
                Ok(Flow::Continue)
            }
            Err(e) => {
                self.report.push(Finding::new(
                    FindingKind::Error,
                    "",
                    e.code.map_or_else(|| e.message.clone(), |c| format!("[{c}] {}", e.message)),
                    line,
                ));
                match self.on_error {
                    OnError::Skip => Ok(Flow::Continue),
                    OnError::Stop => Ok(Flow::Abort),
                }
            }
        }
    }

    /// Commit the current batch and start a fresh transaction window.
    fn commit_batch(&mut self) -> Result<(), ImportError> {
        if let Some(sink) = self.sink.as_deref_mut()
            && self.in_txn
        {
            sink.commit().map_err(ImportError::Transaction)?;
            self.stats.batches += 1;
        }
        self.in_txn = false;
        self.batch_n = 0;
        Ok(())
    }

    /// Commit any open transaction at the end of a successful run.
    fn commit_final(&mut self) -> Result<(), ImportError> {
        if self.in_txn {
            self.commit_batch()?;
        }
        Ok(())
    }

    /// Attribute every invalid run in `stmt` to its `table.column` and count
    /// the affected values and rows.
    fn tally_invalid_utf8(&mut self, stmt: &RawStatement) {
        let located: Located = locate(&stmt.sql, self.sanitizer.dialect(), &stmt.invalid_utf8);
        let table = located.table.clone().unwrap_or_else(|| format!("statement@{}", stmt.line));
        let schema_cols = located.table_key().and_then(|k| self.schema.get(&k));

        // Distinct (key, row) and (row) within this statement; runs in the
        // same cell count as one value.
        let mut cells: BTreeSet<(String, usize)> = BTreeSet::new();
        let mut rows: BTreeSet<usize> = BTreeSet::new();
        for hit in &located.hits {
            let key = match hit.column {
                Some(idx) => {
                    let column = located
                        .columns
                        .as_ref()
                        .and_then(|c| c.get(idx))
                        .or_else(|| schema_cols.and_then(|c| c.get(idx)))
                        .cloned()
                        .unwrap_or_else(|| format!("column#{}", idx + 1));
                    format!("{table}.{column}")
                }
                // No row/column structure (an UPDATE, a DDL comment): the
                // statement's object is the best name there is.
                None => table.clone(),
            };
            cells.insert((key, hit.row));
            rows.insert(hit.row);
        }

        for (key, _row) in &cells {
            let entry = self
                .utf8_tally
                .entry(key.clone())
                .or_insert_with(|| Utf8Tally { first_line: stmt.line, ..Utf8Tally::default() });
            entry.values += 1;
        }
        self.stats.invalid_utf8_values += cells.len();
        self.stats.invalid_utf8_rows += rows.len();
    }

    /// Turn the tallies into one finding per `table.column`: an
    /// [`Error`](FindingKind::Error) under `Reject` (blocking), a
    /// [`Downgraded`](FindingKind::Downgraded) otherwise.
    fn emit_utf8_findings(&mut self) {
        let kind = if self.utf8.is_reject() { FindingKind::Error } else { FindingKind::Downgraded };
        let applied = self.utf8.applied();
        for (key, tally) in std::mem::take(&mut self.utf8_tally) {
            self.report.push(Finding::new(
                kind,
                key,
                format!("{} TEXT value(s) not valid UTF-8; {applied}", tally.values),
                tally.first_line,
            ));
        }
    }

    /// Roll back the open transaction, best-effort (a failure here is logged
    /// by the caller's reporting, not escalated — we are already aborting).
    fn rollback_quiet(&mut self) {
        if let Some(sink) = self.sink.as_deref_mut()
            && self.in_txn
        {
            let _ = sink.rollback();
        }
        self.in_txn = false;
        self.batch_n = 0;
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::Dialect;
    use crate::utf8::Utf8Policy;

    /// A recording sink. `fail_on` makes `exec` return an error for any SQL
    /// containing that substring, to exercise the error policy.
    #[derive(Default)]
    struct MockSink {
        log: Vec<String>,
        fail_on: Option<String>,
    }

    impl StatementSink for MockSink {
        fn exec(&mut self, sql: &str) -> Result<(), SinkError> {
            if let Some(f) = &self.fail_on
                && sql.contains(f.as_str())
            {
                return Err(SinkError::coded(1064, format!("boom on {sql}")));
            }
            self.log.push(sql.to_string());
            Ok(())
        }
        fn begin(&mut self) -> Result<(), SinkError> {
            self.log.push("BEGIN".into());
            Ok(())
        }
        fn commit(&mut self) -> Result<(), SinkError> {
            self.log.push("COMMIT".into());
            Ok(())
        }
        fn rollback(&mut self) -> Result<(), SinkError> {
            self.log.push("ROLLBACK".into());
            Ok(())
        }
    }

    fn reader(sql: &str, dialect: Dialect) -> DumpReader<Cursor<Vec<u8>>> {
        DumpReader::new(Cursor::new(sql.as_bytes().to_vec()), dialect)
    }

    #[test]
    fn executes_and_batches_in_transactions() {
        let mut sink = MockSink::default();
        let mut report = CompatReport::new();
        let opts = ImportOptions { batch_size: 2, ..ImportOptions::default() };
        let dump =
            "INSERT INTO t VALUES (1);\nINSERT INTO t VALUES (2);\nINSERT INTO t VALUES (3);\n";
        let (stats, outcome) = run_import(
            reader(dump, Dialect::MySql),
            &Sanitizer::new(Dialect::MySql),
            Some(&mut sink),
            &opts,
            &mut report,
        )
        .unwrap();
        assert_eq!(outcome, ImportOutcome::Completed);
        assert_eq!(stats.executed, 3);
        // Two full batches: BEGIN i1 i2 COMMIT, then BEGIN i3 COMMIT.
        assert_eq!(stats.batches, 2);
        assert_eq!(sink.log.first().map(String::as_str), Some("BEGIN"));
        assert_eq!(sink.log.last().map(String::as_str), Some("COMMIT"));
        assert_eq!(sink.log.iter().filter(|l| *l == "BEGIN").count(), 2);
    }

    #[test]
    fn dry_run_executes_nothing_but_reports() {
        let mut report = CompatReport::new();
        let dump = "LOCK TABLES `t` WRITE;\nCREATE TABLE `t` (`s` enum('a','b'));\nINSERT INTO t VALUES (1);\n";
        let (stats, outcome) = run_import(
            reader(dump, Dialect::MySql),
            &Sanitizer::new(Dialect::MySql),
            None,
            &ImportOptions::default(),
            &mut report,
        )
        .unwrap();
        assert_eq!(outcome, ImportOutcome::Completed);
        assert_eq!(stats.executed, 0, "dry run must not execute");
        assert_eq!(stats.emitted, 2, "the CREATE TABLE and the INSERT are emitted");
        assert_eq!(stats.dropped, 1, "LOCK TABLES dropped");
        assert_eq!(report.summary().downgraded, 1, "ENUM downgrade reported even in dry run");
    }

    #[test]
    fn on_error_stop_aborts_at_unsupported() {
        let mut sink = MockSink::default();
        let mut report = CompatReport::new();
        let dump = "INSERT INTO t VALUES (1);\n\
            CREATE DEFINER=`r`@`h` PROCEDURE `p`() BEGIN SELECT 1; END;\n\
            INSERT INTO t VALUES (2);\n";
        let opts = ImportOptions { on_error: OnError::Stop, ..ImportOptions::default() };
        let (stats, outcome) = run_import(
            reader(dump, Dialect::MySql),
            &Sanitizer::new(Dialect::MySql),
            Some(&mut sink),
            &opts,
            &mut report,
        )
        .unwrap();
        assert_eq!(outcome, ImportOutcome::Aborted);
        assert_eq!(stats.executed, 1, "only the first INSERT ran");
        assert!(
            sink.log.contains(&"ROLLBACK".to_string()),
            "open batch rolled back: {:?}",
            sink.log
        );
        assert!(report.has_blocking());
    }

    #[test]
    fn on_error_skip_imports_the_rest() {
        let mut sink = MockSink::default();
        let mut report = CompatReport::new();
        let dump = "INSERT INTO t VALUES (1);\n\
            CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW SET NEW.x=1;\n\
            INSERT INTO t VALUES (2);\n";
        let opts = ImportOptions { on_error: OnError::Skip, ..ImportOptions::default() };
        let (stats, outcome) = run_import(
            reader(dump, Dialect::MySql),
            &Sanitizer::new(Dialect::MySql),
            Some(&mut sink),
            &opts,
            &mut report,
        )
        .unwrap();
        assert_eq!(outcome, ImportOutcome::Completed);
        assert_eq!(stats.executed, 2, "both inserts ran, trigger skipped");
        assert_eq!(report.summary().unsupported, 1);
    }

    #[test]
    fn execution_error_under_skip_is_recorded_and_continues() {
        let mut sink = MockSink { fail_on: Some("VALUES (2)".into()), ..MockSink::default() };
        let mut report = CompatReport::new();
        let dump =
            "INSERT INTO t VALUES (1);\nINSERT INTO t VALUES (2);\nINSERT INTO t VALUES (3);\n";
        let opts = ImportOptions { on_error: OnError::Skip, ..ImportOptions::default() };
        let (stats, outcome) = run_import(
            reader(dump, Dialect::MySql),
            &Sanitizer::new(Dialect::MySql),
            Some(&mut sink),
            &opts,
            &mut report,
        )
        .unwrap();
        assert_eq!(outcome, ImportOutcome::Completed);
        assert_eq!(stats.executed, 2, "the failing statement is not counted executed");
        assert_eq!(report.summary().error, 1);
    }

    // ── UTF-8 policy ───────────────────────────────────────────────────────

    /// A `CREATE TABLE` (so columns can be named), a clean INSERT, then an
    /// extended INSERT whose second and third rows carry a raw cp1252 `0x92`
    /// in `body` and a `0x97` in `title`.
    const UTF8_DUMP: &[u8] = b"CREATE TABLE `posts` (`id` int, `title` text, `body` text);\n\
        INSERT INTO `posts` VALUES (1,'clean','clean');\n\
        INSERT INTO `posts` VALUES (2,'ok','it\x92s'),(3,'dash \x97 here','bad\x92'),(4,'x','y');\n\
        INSERT INTO `posts` VALUES (5,'after','after');\n";

    fn run_bytes(
        dump: &[u8],
        policy: Utf8Policy,
        sink: Option<&mut MockSink>,
    ) -> (ImportStats, ImportOutcome, CompatReport) {
        let mut report = CompatReport::new();
        let sanitizer = Sanitizer::new(Dialect::MySql);
        let reader =
            DumpReader::new(Cursor::new(dump.to_vec()), Dialect::MySql).with_utf8_policy(policy);
        let opts = ImportOptions { utf8: policy, batch_size: 1, ..ImportOptions::default() };
        let sink = sink.map(|s| s as &mut dyn StatementSink);
        let (stats, outcome) = run_import(reader, &sanitizer, sink, &opts, &mut report).unwrap();
        (stats, outcome, report)
    }

    #[test]
    fn reject_is_the_default_and_never_executes_an_invalid_statement() {
        assert!(ImportOptions::default().utf8.is_reject());
        let mut sink = MockSink::default();
        let (stats, outcome, report) = run_bytes(UTF8_DUMP, Utf8Policy::Reject, Some(&mut sink));
        assert_eq!(outcome, ImportOutcome::Aborted);
        // The CREATE TABLE and the clean INSERT ran (the CLI pre-validates a
        // file so even those never happen there); the invalid INSERT and
        // everything after it did not.
        assert_eq!(stats.executed, 2);
        assert!(!sink.log.iter().any(|l| l.contains("it\u{FFFD}s")), "{:?}", sink.log);
        assert!(!sink.log.iter().any(|l| l.contains("after")), "{:?}", sink.log);
        // Blocking findings naming table.column.
        assert!(report.has_blocking());
        let objects: Vec<&str> = report.findings().iter().map(|f| f.object.as_str()).collect();
        assert_eq!(objects, vec!["posts.body", "posts.title"]);
        assert!(report.findings().iter().all(|f| f.kind == FindingKind::Error));
        assert!(report.findings()[0].detail.contains("--source-charset"), "{report:?}");
        assert_eq!(report.findings()[0].line, 3);
        assert_eq!(stats.invalid_utf8_values, 3, "body×2 + title×1");
        assert_eq!(stats.invalid_utf8_rows, 2);
    }

    #[test]
    fn reject_dry_run_scans_everything_and_fails_as_a_whole() {
        let mut dump = UTF8_DUMP.to_vec();
        dump.extend_from_slice(b"INSERT INTO `other` (v) VALUES ('\x93late\x94');\n");
        let (stats, outcome, report) = run_bytes(&dump, Utf8Policy::Reject, None);
        assert_eq!(outcome, ImportOutcome::Aborted);
        assert_eq!(stats.executed, 0);
        let objects: Vec<&str> = report.findings().iter().map(|f| f.object.as_str()).collect();
        assert_eq!(objects, vec!["other.v", "posts.body", "posts.title"]);
        assert_eq!(stats.invalid_utf8_values, 4);
        assert_eq!(stats.invalid_utf8_rows, 3);
    }

    #[test]
    fn replace_and_strip_import_everything_and_report_downgraded() {
        let mut sink = MockSink::default();
        let (stats, outcome, report) = run_bytes(UTF8_DUMP, Utf8Policy::Replace, Some(&mut sink));
        assert_eq!(outcome, ImportOutcome::Completed);
        assert_eq!(stats.executed, 4);
        assert!(sink.log.iter().any(|l| l.contains("'it\u{FFFD}s'")), "{:?}", sink.log);
        assert!(sink.log.iter().any(|l| l.contains("'dash \u{FFFD} here'")), "{:?}", sink.log);
        assert!(!report.has_blocking());
        assert_eq!(report.summary().downgraded, 2, "{report:?}");
        assert!(report.findings().iter().all(|f| f.detail.contains("U+FFFD")), "{report:?}");

        let mut sink = MockSink::default();
        let (stats, outcome, report) = run_bytes(UTF8_DUMP, Utf8Policy::Strip, Some(&mut sink));
        assert_eq!(outcome, ImportOutcome::Completed);
        assert_eq!(stats.executed, 4);
        assert!(sink.log.iter().any(|l| l.contains("'its'")), "{:?}", sink.log);
        assert!(sink.log.iter().any(|l| l.contains("'dash  here'")), "{:?}", sink.log);
        assert!(report.findings().iter().all(|f| f.detail.contains("stripped")), "{report:?}");
    }

    #[test]
    fn transcode_from_windows_1252_is_lossless_and_names_the_charset() {
        let cs = crate::Charset::parse("windows-1252").unwrap();
        let mut sink = MockSink::default();
        let (stats, outcome, report) =
            run_bytes(UTF8_DUMP, Utf8Policy::Transcode(cs), Some(&mut sink));
        assert_eq!(outcome, ImportOutcome::Completed);
        assert_eq!(stats.executed, 4);
        assert!(sink.log.iter().any(|l| l.contains("'it\u{2019}s'")), "{:?}", sink.log);
        assert!(sink.log.iter().any(|l| l.contains("'dash \u{2014} here'")), "{:?}", sink.log);
        assert!(sink.log.iter().any(|l| l.contains("'bad\u{2019}'")), "{:?}", sink.log);
        assert_eq!(report.summary().downgraded, 2);
        assert!(
            report.findings().iter().all(|f| f.detail.contains("transcoded from windows-1252")),
            "{report:?}"
        );
    }

    /// Axis C: valid multi-byte UTF-8 (3- and 4-byte sequences) reaches the
    /// sink byte-identical under every policy — the reader, sanitizer and
    /// driver have no narrower charset to squeeze it through.
    #[test]
    fn clean_dumps_report_nothing_about_utf8_and_reach_the_sink_byte_identical() {
        let dump = "INSERT INTO t VALUES (1,'\u{23F0} x \u{1F600} caf\u{e9} \u{2014} na\u{ef}ve \u{4E2D}');\n";
        let expected = "INSERT INTO t VALUES (1,'\u{23F0} x \u{1F600} caf\u{e9} \u{2014} na\u{ef}ve \u{4E2D}')";
        let at = dump.find('\u{23F0}').unwrap();
        assert_eq!(&dump.as_bytes()[at..at + 5], b"\xE2\x8F\xB0 x");
        let cp1252 = crate::Charset::parse("windows-1252").unwrap();
        for policy in [
            Utf8Policy::Reject,
            Utf8Policy::Strip,
            Utf8Policy::Replace,
            Utf8Policy::Transcode(cp1252),
        ] {
            let mut sink = MockSink::default();
            let (stats, outcome, report) = run_bytes(dump.as_bytes(), policy, Some(&mut sink));
            assert_eq!(outcome, ImportOutcome::Completed, "{policy:?}");
            assert_eq!(stats.executed, 1);
            assert_eq!(stats.invalid_utf8_values, 0, "{policy:?}");
            assert_eq!(report.summary(), crate::Summary::default(), "{policy:?}");
            let executed = sink.log.iter().find(|l| l.starts_with("INSERT")).unwrap();
            assert_eq!(executed.as_bytes(), expected.as_bytes(), "{policy:?} altered the bytes");
        }
    }

    #[test]
    fn defer_indexes_replays_them_after_data() {
        let mut sink = MockSink::default();
        let mut report = CompatReport::new();
        let dump = "CREATE INDEX i1 ON t (a);\nINSERT INTO t VALUES (1);\nCREATE UNIQUE INDEX i2 ON t (b);\n";
        let opts =
            ImportOptions { defer_indexes: true, batch_size: 100, ..ImportOptions::default() };
        let (stats, _outcome) = run_import(
            reader(dump, Dialect::MySql),
            &Sanitizer::new(Dialect::MySql),
            Some(&mut sink),
            &opts,
            &mut report,
        )
        .unwrap();
        assert_eq!(stats.deferred, 2);
        // The data INSERT executes before either CREATE INDEX.
        let exec_only: Vec<&String> = sink
            .log
            .iter()
            .filter(|l| l.starts_with("INSERT") || l.starts_with("CREATE"))
            .collect();
        assert_eq!(exec_only[0], "INSERT INTO t VALUES (1)");
        assert!(exec_only[1].starts_with("CREATE INDEX"));
        assert!(exec_only[2].starts_with("CREATE UNIQUE INDEX"));
    }
}
