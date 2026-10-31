//! The import driver: pull statements off a [`DumpReader`], run each through
//! the [`Sanitizer`], and push the result to a [`StatementSink`], batching
//! executions into transactions and honoring the error policy.
//!
//! The driver is sink-agnostic and dry-run aware: pass `None` for the sink to
//! parse-and-report with no execution and no backend. That is exactly what the
//! `--dry-run` CLI flag does, and what the fixture tests use with an in-memory
//! recording sink.

use std::io::BufRead;

use crate::reader::ReadError;
use crate::report::{CompatReport, Finding, FindingKind};
use crate::sanitizer::{SanitizeOutcome, Sanitizer};
use crate::sink::{SinkError, StatementSink};
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
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self { batch_size: 1000, on_error: OnError::Stop, defer_indexes: false, verbose: false }
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
        report,
        stats: ImportStats::default(),
        in_txn: false,
        batch_n: 0,
        deferred: Vec::new(),
    };

    let mut outcome = ImportOutcome::Completed;
    for stmt in reader {
        let stmt = stmt?;
        if run.handle(&stmt)? == Flow::Abort {
            outcome = ImportOutcome::Aborted;
            break;
        }
    }

    if outcome == ImportOutcome::Aborted {
        run.rollback_quiet();
        return Ok((run.stats, outcome));
    }

    // Replay deferred indexes (if any) after all data has landed.
    let deferred = std::mem::take(&mut run.deferred);
    for sql in deferred {
        // A deferred index has no original line; anchor at 0.
        if run.exec_recording(&sql, 0)? == Flow::Abort {
            run.rollback_quiet();
            return Ok((run.stats, ImportOutcome::Aborted));
        }
    }

    run.commit_final()?;
    Ok((run.stats, outcome))
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
    report: &'a mut CompatReport,
    stats: ImportStats,
    in_txn: bool,
    batch_n: usize,
    deferred: Vec<String>,
}

impl Run<'_> {
    /// Process one raw statement.
    fn handle(&mut self, stmt: &RawStatement) -> Result<Flow, ImportError> {
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
