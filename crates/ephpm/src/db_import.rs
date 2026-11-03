//! `ephpm db import` — load a `mysqldump` / `pg_dump` SQL file into a virtual
//! host's embedded (Turso) database.
//!
//! The dump *file-format* work (statement splitting, comment/gate stripping,
//! `LOCK TABLES` / session-`SET` dropping, `COPY FROM stdin` → `INSERT`,
//! classification of unsupported constructs) lives in the pure
//! [`ephpm_dbimport`] crate. This module is the glue: it opens the dump
//! (path / `-` stdin / `.gz`), resolves the target backend with the **same**
//! strategy picker `ephpm php --site` uses ([`crate::site_db`]), wraps that
//! backend in a [`StatementSink`](ephpm_dbimport::StatementSink), and drives
//! the import — then prints the compatibility report.
//!
//! The SQL *type/dialect translation* (MySQL/PostgreSQL → SQLite) is **not**
//! done here: sanitized statements are handed to litewire's translating
//! `Session` (offline direct-open) or forwarded to a running server's wire
//! listener, exactly as the `ephpm_db_*` bridge and `pdo_mysql` already route
//! tenant SQL.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use ephpm_config::Config;
use ephpm_dbimport::{
    Cell, CompatReport, Dialect, DumpReader, FindingKind, ImportOptions, ImportOutcome,
    ImportStats, OnError, RowSet, SinkError, SourceCharset, StatementSink, Utf8Policy, run_import,
    sniff,
};
use ephpm_php::db_bridge::{RemoteBackend, screen_sql, screen_sql_mysql};
use litewire::backend::{BackendConn, BackendError, ResultSet, SharedBackend, Value};
use litewire::translate::Dialect as LwDialect;
use litewire::{Session, SessionError, SessionResult};

use crate::site_db::{self, RemoteMysqlBackend, Strategy};

/// How the operator named the dump dialect on the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialectArg {
    /// Sniff it from the dump header (fail if ambiguous).
    Auto,
    /// Force MySQL.
    MySql,
    /// Force PostgreSQL.
    Pg,
}

/// How the operator named the error policy on the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnErrorArg {
    /// Stop at the first blocking finding / execution error.
    Stop,
    /// Record and continue.
    Skip,
}

/// How the operator named the invalid-UTF-8 policy on the CLI
/// (`--on-invalid-utf8`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnInvalidUtf8Arg {
    /// Refuse the import (the default).
    Reject,
    /// Drop the invalid bytes.
    Strip,
    /// Replace each invalid sequence with U+FFFD.
    Replace,
    /// Transcode each invalid sequence from `--source-charset`.
    Transcode,
}

/// One `ephpm db import` invocation, assembled from the CLI flags.
pub struct ImportRequest {
    /// The dump path, or `-` for stdin.
    pub dump: String,
    /// The vhost key whose database to load.
    pub site: String,
    /// The ephpm config file.
    pub config: PathBuf,
    /// Dialect selection.
    pub dialect: DialectArg,
    /// Force gzip decompression regardless of extension.
    pub gzip: bool,
    /// Error policy.
    pub on_error: OnErrorArg,
    /// Parse + sanitize + report only; execute nothing, bind no backend.
    pub dry_run: bool,
    /// Statements per transaction.
    pub batch_size: usize,
    /// Write the compatibility report as JSON to this path.
    pub report: Option<PathBuf>,
    /// Hold `CREATE INDEX` statements and replay them after the data.
    pub defer_indexes: bool,
    /// List dropped safe-no-op findings too.
    pub verbose: bool,
    /// `--on-invalid-utf8`, when given explicitly.
    pub on_invalid_utf8: Option<OnInvalidUtf8Arg>,
    /// `--source-charset`, when given. Implies `--on-invalid-utf8 transcode`.
    pub source_charset: Option<String>,
}

/// WAL-checkpoint give-up budget for the direct-open path at exit (nothing
/// else holds the file, so only a dump that left a transaction open could keep
/// it busy).
const CHECKPOINT_GIVE_UP: Duration = Duration::from_secs(30);

/// Resolve `--source-charset`, `--on-invalid-utf8` and `--on-error` into one
/// [`Utf8Policy`].
///
/// `--source-charset` (default `utf8mb4`) says what the dump's bytes are.
/// `utf8mb4` means the stream is UTF-8: valid text passes through untouched
/// and an invalid run is handled by the policy without any transcoding. A
/// legacy single-byte charset means invalid runs are *transcoded* from it.
///
/// Without an explicit `--on-invalid-utf8`, the policy follows the flags the
/// operator already set:
///
/// * a legacy `--source-charset` → `transcode` (naming the charset is the
///   instruction);
/// * otherwise `--on-error stop` (the default) → `reject`: nothing is written;
/// * otherwise `--on-error skip` → `strip`: keep going, report per column.
///
/// An explicit `--on-invalid-utf8` wins, but must agree with the charset:
/// `transcode` needs a legacy charset, and `reject`/`strip`/`replace` cannot
/// be combined with one (that would silently ignore the charset).
///
/// # Errors
///
/// A contradiction, a missing charset for `transcode`, or an unknown /
/// multi-byte charset label.
pub fn resolve_utf8_policy(
    on_error: OnErrorArg,
    on_invalid: Option<OnInvalidUtf8Arg>,
    source_charset: Option<&str>,
) -> anyhow::Result<Utf8Policy> {
    let charset = source_charset
        .map(|label| {
            SourceCharset::parse(label).with_context(|| format!("--source-charset {label:?}"))
        })
        .transpose()?
        .unwrap_or_default();
    let legacy = charset.legacy();
    let name = |o: OnInvalidUtf8Arg| match o {
        OnInvalidUtf8Arg::Reject => "reject",
        OnInvalidUtf8Arg::Strip => "strip",
        OnInvalidUtf8Arg::Replace => "replace",
        OnInvalidUtf8Arg::Transcode => "transcode",
    };
    match (on_invalid, legacy) {
        (None | Some(OnInvalidUtf8Arg::Transcode), Some(cs)) => Ok(Utf8Policy::Transcode(cs)),
        (None, None) => Ok(match on_error {
            OnErrorArg::Stop => Utf8Policy::Reject,
            OnErrorArg::Skip => Utf8Policy::Strip,
        }),
        (Some(OnInvalidUtf8Arg::Transcode), None) => anyhow::bail!(
            "--on-invalid-utf8 transcode needs a legacy --source-charset (e.g. windows-1252 \
             for a latin1/cp1252 dump) to say what the invalid bytes are encoded in; \
             --source-charset utf8mb4 (the default) has nothing to transcode from"
        ),
        (Some(OnInvalidUtf8Arg::Reject), None) => Ok(Utf8Policy::Reject),
        (Some(OnInvalidUtf8Arg::Strip), None) => Ok(Utf8Policy::Strip),
        (Some(OnInvalidUtf8Arg::Replace), None) => Ok(Utf8Policy::Replace),
        (Some(other), Some(cs)) => anyhow::bail!(
            "--on-invalid-utf8 {} would ignore --source-charset {cs}: a legacy charset means \
             invalid sequences are transcoded from it (--on-invalid-utf8 transcode, or just \
             omit --on-invalid-utf8); drop --source-charset to {} them instead",
            name(other),
            name(other)
        ),
    }
}

/// Run `ephpm db import`.
///
/// # Errors
///
/// Returns an error when the config or dump cannot be read, the site key is
/// invalid, the chosen transport cannot be established, or the dump cannot be
/// split. Per-statement SQL failures are recorded in the compatibility report
/// (and gate the exit code) rather than returned.
pub fn run(req: &ImportRequest) -> anyhow::Result<ExitCode> {
    let config = Config::load(&req.config)
        .with_context(|| format!("loading configuration from {}", req.config.display()))?;

    let utf8 =
        resolve_utf8_policy(req.on_error, req.on_invalid_utf8, req.source_charset.as_deref())?;

    // Open the dump and settle the dialect before deciding anything else.
    let (mut source, dialect) = open_dump(req)?;
    tracing::info!(
        dialect = dialect.as_str(),
        dump = %req.dump,
        on_invalid_utf8 = %utf8,
        "db import: starting"
    );

    let opts = ImportOptions {
        batch_size: req.batch_size,
        on_error: match req.on_error {
            OnErrorArg::Stop => OnError::Stop,
            OnErrorArg::Skip => OnError::Skip,
        },
        defer_indexes: req.defer_indexes,
        verbose: req.verbose,
        utf8,
    };
    let sanitizer = ephpm_dbimport::Sanitizer::new(dialect);
    let mut report = CompatReport::new();

    // `reject` is a promise that nothing is written if the dump holds a value
    // the engine would refuse to read back — so the dump is validated whole
    // *before* the database is even opened. That needs a second pass over the
    // source, which stdin cannot give (and spooling a multi-GiB stream to disk
    // behind the operator's back is worse than asking).
    if utf8.is_reject() && !req.dry_run {
        if req.dump == "-" {
            anyhow::bail!(
                "--on-invalid-utf8 reject (the default under --on-error stop) validates the \
                 whole dump before writing anything, which needs a re-readable source and stdin \
                 is not one. Either save the dump to a file first, or choose a streaming policy: \
                 --on-error skip (which strips invalid bytes and reports them), \
                 --on-invalid-utf8 strip|replace, or --source-charset <legacy charset> to \
                 transcode them"
            );
        }
        tracing::info!("db import: validating the dump's UTF-8 before opening the database");
        let (mut prescan, _) = open_dump(req)?;
        let reader = DumpReader::new(&mut prescan, dialect).with_utf8_policy(utf8);
        let mut prescan_report = CompatReport::new();
        let scan_opts = ImportOptions { on_error: OnError::Skip, ..opts.clone() };
        let (scan_stats, _) = run_import(reader, &sanitizer, None, &scan_opts, &mut prescan_report)
            .context("validating the dump's UTF-8")?;
        if scan_stats.invalid_utf8_values > 0 {
            print_utf8_rejection(&prescan_report, scan_stats);
            if let Some(path) = &req.report {
                std::fs::write(path, prescan_report.to_json()).with_context(|| {
                    format!("writing the compatibility report to {}", path.display())
                })?;
            }
            anyhow::bail!(
                "refusing to import: {} TEXT value(s) across {} row(s) are not valid UTF-8 and \
                 nothing was written (see the report above)",
                scan_stats.invalid_utf8_values,
                scan_stats.invalid_utf8_rows
            );
        }
    }

    let (stats, outcome) = if req.dry_run {
        let reader = DumpReader::new(&mut source, dialect).with_utf8_policy(utf8);
        run_import(reader, &sanitizer, None, &opts, &mut report)
            .context("parsing the dump for the dry run")?
    } else {
        // A dedicated runtime drives the async backend; this (main) thread runs
        // the import loop and `block_on`s onto it — never a runtime worker, the
        // same invariant `ephpm php --site` relies on.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .thread_name("ephpm-db-import")
            .build()
            .context("building the database runtime for `ephpm db import`")?;

        let strategy = resolve_strategy(&config, &req.site)?;
        let mut sink = build_sink(&strategy, dialect, runtime.handle())?;
        let reader = DumpReader::new(&mut source, dialect).with_utf8_policy(utf8);
        let result = run_import(reader, &sanitizer, Some(sink.as_mut()), &opts, &mut report)
            .context("importing the dump");

        // Direct-open path: truncate the WAL before exit (the Turso SDK never
        // checkpoints on drop). Do it whether or not the import succeeded —
        // whatever committed is in the log.
        if let Strategy::DirectOpen { path } = &strategy {
            let checkpoint = runtime.block_on(ephpm_server::db_shutdown::checkpoint_database(
                &req.site,
                Path::new(path),
                CHECKPOINT_GIVE_UP,
            ));
            if !checkpoint.abandoned.is_empty() {
                tracing::warn!(
                    path = %path,
                    "db import: could not truncate the write-ahead log before exit (a \
                     transaction left open?); it is recovered on the next open"
                );
            }
        }
        result?
    };

    print_report(&report, stats, req.verbose);
    print_utf8_summary(stats, utf8);

    if let Some(path) = &req.report {
        std::fs::write(path, report.to_json())
            .with_context(|| format!("writing the compatibility report to {}", path.display()))?;
        tracing::info!(path = %path.display(), "db import: wrote compatibility report");
    }

    let failed = outcome == ImportOutcome::Aborted
        || (matches!(opts.on_error, OnError::Stop) && report.has_blocking());
    Ok(if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

/// Open the dump as a buffered reader and settle its dialect. `--dialect auto`
/// sniffs the first buffered chunk (peeked, not consumed).
///
/// Returns the reader plus the resolved [`Dialect`].
fn open_dump(req: &ImportRequest) -> anyhow::Result<(BufReader<Box<dyn Read>>, Dialect)> {
    let is_stdin = req.dump == "-";
    let has_gz_ext = Path::new(&req.dump).extension().is_some_and(|e| e.eq_ignore_ascii_case("gz"));
    let gz = req.gzip || (!is_stdin && has_gz_ext);

    let raw: Box<dyn Read> = if is_stdin {
        Box::new(std::io::stdin().lock())
    } else {
        Box::new(
            File::open(&req.dump).with_context(|| format!("opening the dump file {}", req.dump))?,
        )
    };
    let raw: Box<dyn Read> = if gz { Box::new(flate2::read::GzDecoder::new(raw)) } else { raw };
    let mut buf = BufReader::new(raw);

    let dialect = match req.dialect {
        DialectArg::MySql => Dialect::MySql,
        DialectArg::Pg => Dialect::Pg,
        DialectArg::Auto => {
            let head = buf.fill_buf().context("reading the dump header to detect the dialect")?;
            sniff(head).ok_or_else(|| {
                anyhow::anyhow!(
                    "could not detect the dump dialect from its header; pass --dialect mysql or \
                     --dialect pg"
                )
            })?
        }
    };
    Ok((buf, dialect))
}

/// Resolve the backend binding strategy for `site`, probing the running
/// server's MySQL listener exactly as `ephpm php --site` does.
fn resolve_strategy(config: &Config, site: &str) -> anyhow::Result<Strategy> {
    site_db::plan_site_db(config, site)
}

/// Build a [`StatementSink`] over the resolved backend.
fn build_sink(
    strategy: &Strategy,
    dialect: Dialect,
    handle: &tokio::runtime::Handle,
) -> anyhow::Result<Box<dyn StatementSink>> {
    match strategy {
        Strategy::DirectOpen { path } => {
            tracing::info!(path = %path, "db import: opening the site database (offline)");
            let sink = DirectSink::open(path, dialect, handle.clone())
                .with_context(|| format!("opening the site database at {path}"))?;
            Ok(Box::new(sink))
        }
        Strategy::Wire { addr, user, password, .. } => {
            if matches!(dialect, Dialect::Pg) {
                tracing::warn!(
                    "importing a PostgreSQL dump through the running server's MySQL listener: the \
                     server translates MySQL, so PG-specific SQL may fail. Stop the server and \
                     re-run for an offline direct-open, which uses the PostgreSQL translator."
                );
            }
            let backend = handle
                .block_on(RemoteMysqlBackend::connect(*addr, user, password))
                .with_context(|| {
                    format!(
                        "connecting to the running server's MySQL listener at {addr} as {user:?}"
                    )
                })?;
            Ok(Box::new(WireSink { backend, handle: handle.clone(), screen: true }))
        }
    }
}

/// Map this crate's [`Dialect`] onto litewire's translation dialect, or `None`
/// for [`Dialect::Sqlite`] — which runs straight against the engine with no
/// translation (the raw-connection path), so it needs no translator.
fn lw_dialect(dialect: Dialect) -> Option<LwDialect> {
    match dialect {
        Dialect::MySql => Some(LwDialect::MySQL),
        Dialect::Pg => Some(LwDialect::PostgreSQL),
        Dialect::Sqlite => None,
    }
}

/// Turn a litewire [`SessionError`] into a sink error, preserving the mapped
/// error code.
fn session_err(e: &SessionError) -> SinkError {
    SinkError::coded(e.code(), e.to_string())
}

/// Turn a litewire [`BackendError`] (raw, sqlite-native path) into a sink
/// error. The backend carries no MySQL errno, so the code is left unset.
fn backend_err(e: &BackendError) -> SinkError {
    SinkError::message(e.to_string())
}

/// Screen one statement when `enabled`, mapping a rejection to a
/// [`SinkError`]. `enabled` is `false` only on the console's offline
/// `--maintenance` path; every other caller screens.
///
/// The lexing rule follows the text's dialect: MySQL text (a `mysqldump`, the
/// `--dialect mysql` console, and everything forwarded over the wire, which is
/// MySQL by construction) is screened by [`screen_sql_mysql`], because MySQL
/// escapes an embedded quote as `\'` and litewire's translator reads it that
/// way; the standard-rule [`screen_sql`] would take the escaped quote for the
/// closing one and refuse a statement with an odd number of them as "malformed
/// SQL" — which is how v0.14.0 dropped every extended `INSERT` holding an
/// apostrophe (~257k of 530k rows on a real dump). PostgreSQL and
/// `sqlite`-native text keep the standard rule.
fn screen_statement(enabled: bool, dialect: Dialect, sql: &str) -> Result<(), SinkError> {
    if enabled {
        let screen = match dialect {
            Dialect::MySql => screen_sql_mysql,
            Dialect::Pg | Dialect::Sqlite => screen_sql,
        };
        screen(sql).map_err(|offending| {
            SinkError::message(format!("statement rejected on the tenant path: {offending}"))
        })?;
    }
    Ok(())
}

/// Map a litewire [`Value`] onto a dump-crate [`Cell`] for a [`RowSet`].
fn value_to_cell(v: Value) -> Cell {
    match v {
        Value::Null => Cell::Null,
        Value::Integer(i) => Cell::Integer(i),
        Value::Float(f) => Cell::Float(f),
        Value::Text(s) => Cell::Text(s),
        Value::Blob(b) => Cell::Blob(b),
    }
}

/// Convert a litewire [`ResultSet`] into a dump-crate [`RowSet`].
fn result_set_to_rowset(rs: ResultSet) -> RowSet {
    RowSet {
        columns: rs.columns.into_iter().map(|c| c.name).collect(),
        rows: rs.rows.into_iter().map(|row| row.into_iter().map(value_to_cell).collect()).collect(),
    }
}

/// How a [`DirectSink`] reaches a directly-opened Turso file.
enum DirectInner {
    /// `mysql` / `pg`: a translating litewire [`Session`] over the connection.
    Translating(Session),
    /// `sqlite`: the raw [`BackendConn`], run with no translation.
    Native(Box<dyn BackendConn>),
}

/// A sink that executes statements against a directly-opened Turso file,
/// offline (no server holds it). It speaks either the translating litewire
/// [`Session`] (`mysql` / `pg`) or the raw engine connection (`sqlite`-native),
/// chosen at construction from the dialect.
///
/// `db import` always builds it screening (the tenant path) and in a MySQL or
/// PostgreSQL dialect, so its execute path is unchanged. The `db query` /
/// `db shell` consoles reuse it, add the [`query_rows`](StatementSink::query_rows)
/// read path, pick `sqlite` by default, and drop screening only in the offline
/// `--maintenance` mode.
pub(crate) struct DirectSink {
    inner: DirectInner,
    handle: tokio::runtime::Handle,
    /// Screen each statement (by `dialect`'s lexing rule) before running it.
    screen: bool,
    /// The dialect the statements are in — selects the translator and the
    /// screen's string-escaping rule.
    dialect: Dialect,
    // Keep the backend alive for the connection's lifetime.
    _backend: SharedBackend,
}

impl DirectSink {
    /// Open the Turso file at `path` and bind a screening sink speaking
    /// `dialect` (the `db import` / tenant path).
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened or a connection made.
    pub(crate) fn open(
        path: &str,
        dialect: Dialect,
        handle: tokio::runtime::Handle,
    ) -> anyhow::Result<Self> {
        let backend = open_offline_backend(path, &handle)?;
        Self::from_backend(backend, dialect, handle, true)
    }

    /// Bind a sink over an already-open `backend`. The console opens the file
    /// once and rebinds across `/dialect` switches (and the `--maintenance`
    /// `screen = false` toggle) without reopening it.
    ///
    /// # Errors
    ///
    /// Returns an error if a connection to the backend cannot be opened.
    pub(crate) fn from_backend(
        backend: SharedBackend,
        dialect: Dialect,
        handle: tokio::runtime::Handle,
        screen: bool,
    ) -> anyhow::Result<Self> {
        let conn = handle
            .block_on(backend.connect())
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("opening a connection to the site database")?;
        let inner = match lw_dialect(dialect) {
            Some(lw) => DirectInner::Translating(Session::new(conn, lw)),
            None => DirectInner::Native(conn),
        };
        Ok(Self { inner, handle, screen, dialect, _backend: backend })
    }

    /// Execute `sql`, discarding any result set (the import / transaction-control
    /// path).
    fn exec_inner(&mut self, sql: &str) -> Result<(), SinkError> {
        screen_statement(self.screen, self.dialect, sql)?;
        match &mut self.inner {
            DirectInner::Translating(session) => {
                match self.handle.block_on(session.query(sql, &[] as &[Value])) {
                    Ok(SessionResult::Ok(_) | SessionResult::Rows(_)) => Ok(()),
                    Err(e) => Err(session_err(&e)),
                }
            }
            DirectInner::Native(conn) => {
                match self.handle.block_on(conn.execute(sql, &[] as &[Value])) {
                    Ok(_) => Ok(()),
                    Err(e) => Err(backend_err(&e)),
                }
            }
        }
    }

    /// Execute `sql` and return its result set (empty for a write / DDL).
    fn query_inner(&mut self, sql: &str) -> Result<RowSet, SinkError> {
        screen_statement(self.screen, self.dialect, sql)?;
        match &mut self.inner {
            DirectInner::Translating(session) => {
                match self.handle.block_on(session.query(sql, &[] as &[Value])) {
                    Ok(SessionResult::Rows(rs)) => Ok(result_set_to_rowset(rs)),
                    Ok(SessionResult::Ok(_)) => Ok(RowSet::empty()),
                    Err(e) => Err(session_err(&e)),
                }
            }
            DirectInner::Native(conn) => {
                match self.handle.block_on(conn.query(sql, &[] as &[Value])) {
                    Ok(rs) => Ok(result_set_to_rowset(rs)),
                    Err(e) => Err(backend_err(&e)),
                }
            }
        }
    }
}

impl StatementSink for DirectSink {
    fn exec(&mut self, sql: &str) -> Result<(), SinkError> {
        self.exec_inner(sql)
    }
    fn begin(&mut self) -> Result<(), SinkError> {
        self.exec_inner("BEGIN")
    }
    fn commit(&mut self) -> Result<(), SinkError> {
        self.exec_inner("COMMIT")
    }
    fn rollback(&mut self) -> Result<(), SinkError> {
        self.exec_inner("ROLLBACK")
    }
    fn query_rows(&mut self, sql: &str) -> Result<RowSet, SinkError> {
        self.query_inner(sql)
    }
}

/// Open a Turso database file directly and return it as a shared backend.
///
/// Deliberately opened **without** the server's WAL checkpoint gate — this is a
/// short-lived CLI process with no concurrent readers (the same reasoning as
/// [`crate::site_db`]'s offline direct-open). The caller truncates the log at
/// exit.
///
/// # Errors
///
/// Returns an error if the database cannot be opened.
pub(crate) fn open_offline_backend(
    path: &str,
    handle: &tokio::runtime::Handle,
) -> anyhow::Result<SharedBackend> {
    let turso = handle.block_on(litewire::Turso::open(path)).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Arc::new(turso))
}

/// A sink that forwards raw SQL to a running server's MySQL wire listener,
/// where the server does the translation/screening (the same path
/// `ephpm php --site` uses against a live server).
pub(crate) struct WireSink {
    backend: RemoteMysqlBackend,
    handle: tokio::runtime::Handle,
    /// Screen each statement CLI-side with [`screen_sql`] before forwarding it.
    /// The server screens again; this is defence in depth and keeps the error
    /// local and fast.
    screen: bool,
}

impl WireSink {
    /// Build a wire sink over an already-connected [`RemoteMysqlBackend`].
    pub(crate) fn new(
        backend: RemoteMysqlBackend,
        handle: tokio::runtime::Handle,
        screen: bool,
    ) -> Self {
        Self { backend, handle, screen }
    }

    fn exec_inner(&self, sql: &str) -> Result<(), SinkError> {
        // The wire is MySQL by construction (the server's listener speaks
        // MySQL and translates), so the text is MySQL-dialect.
        screen_statement(self.screen, Dialect::MySql, sql)?;
        match self.handle.block_on(self.backend.run(sql, &[] as &[Value])) {
            Ok(SessionResult::Ok(_) | SessionResult::Rows(_)) => Ok(()),
            Err(e) => Err(session_err(&e)),
        }
    }

    fn query_inner(&self, sql: &str) -> Result<RowSet, SinkError> {
        screen_statement(self.screen, Dialect::MySql, sql)?;
        match self.handle.block_on(self.backend.run(sql, &[] as &[Value])) {
            Ok(SessionResult::Rows(rs)) => Ok(result_set_to_rowset(rs)),
            Ok(SessionResult::Ok(_)) => Ok(RowSet::empty()),
            Err(e) => Err(session_err(&e)),
        }
    }
}

impl StatementSink for WireSink {
    fn exec(&mut self, sql: &str) -> Result<(), SinkError> {
        self.exec_inner(sql)
    }
    fn begin(&mut self) -> Result<(), SinkError> {
        self.exec_inner("BEGIN")
    }
    fn commit(&mut self) -> Result<(), SinkError> {
        self.exec_inner("COMMIT")
    }
    fn rollback(&mut self) -> Result<(), SinkError> {
        self.exec_inner("ROLLBACK")
    }
    fn query_rows(&mut self, sql: &str) -> Result<RowSet, SinkError> {
        self.query_inner(sql)
    }
}

/// Print the headline summary and, by default, the significant findings
/// (everything that is not a dropped safe-no-op). With `verbose`, the dropped
/// findings are listed too.
fn print_report(report: &CompatReport, stats: ephpm_dbimport::ImportStats, verbose: bool) {
    let s = report.summary();
    println!(
        "\ndb import: {executed} statements executed, {emitted} emitted, {dropped} dropped, \
         {deferred} indexes deferred, {batches} batches committed",
        executed = stats.executed,
        emitted = stats.emitted,
        dropped = stats.dropped,
        deferred = stats.deferred,
        batches = stats.batches,
    );
    println!(
        "compatibility: {} unsupported, {} downgraded, {} error(s), {} dropped",
        s.unsupported, s.downgraded, s.error, s.dropped
    );

    let mut shown = 0usize;
    for f in report.findings() {
        if f.kind == FindingKind::Dropped && !verbose {
            continue;
        }
        let where_ = if f.object.is_empty() {
            format!("line {}", f.line)
        } else {
            format!("line {} {}", f.line, f.object)
        };
        println!("  [{}] {where_}: {}", f.kind.label(), f.detail);
        shown += 1;
    }
    if shown == 0 && (s.unsupported + s.downgraded + s.error) == 0 {
        println!("  (no compatibility concerns)");
    }
}

/// One headline line for the invalid-UTF-8 tally, when there was one.
fn print_utf8_summary(stats: ImportStats, policy: Utf8Policy) {
    if stats.invalid_utf8_values > 0 {
        println!(
            "invalid UTF-8: {} TEXT value(s) across {} row(s) — {}",
            stats.invalid_utf8_values,
            stats.invalid_utf8_rows,
            policy.applied()
        );
    }
}

/// The pre-validation refusal under `--on-invalid-utf8 reject`: the per
/// `table.column` findings, then how to proceed. Nothing has been opened or
/// written when this prints.
fn print_utf8_rejection(report: &CompatReport, stats: ImportStats) {
    println!(
        "\ndb import: the dump holds {} TEXT value(s) across {} row(s) that are not valid UTF-8. \
         The embedded engine validates TEXT when it is read, so importing them would produce a \
         database whose SELECTs fail with `TEXT value contains invalid UTF-8`. Nothing was written.",
        stats.invalid_utf8_values, stats.invalid_utf8_rows
    );
    for f in report.findings().iter().filter(|f| f.kind == FindingKind::Error) {
        println!("  [error] line {} {}: {}", f.line, f.object, f.detail);
    }
    println!(
        "Re-run with --source-charset <charset> (windows-1252 for a latin1/cp1252 dump) to \
         transcode those bytes losslessly, or --on-invalid-utf8 strip|replace to drop or \
         replace them. --dry-run previews the same report."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The v0.14.1 row-loss regression at the sink boundary: every `db import`
    /// sink screens each statement before executing it, and the screen used
    /// to apply the SQLite lexing rule to MySQL text. An extended `INSERT`
    /// whose rows carry `\'` (an odd number of them), `''`, `);`, `,`, a `\\`
    /// before a closing quote and HTML was refused as "malformed SQL" — the
    /// dry run (which never screens) was clean — and under `--on-error skip`
    /// the whole statement's rows were silently lost.
    #[test]
    fn mysql_sink_screen_accepts_escaped_quotes_in_an_extended_insert() {
        let rows: Vec<String> = (1..=50)
            .map(|i| match i % 5 {
                0 => format!(r"({i},'O\'Brien & co; it\'s fine')"),
                1 => format!(r"({i},'it''s doubled); not a boundary')"),
                2 => format!(r#"({i},'<a href="x">O\'Brien & co</a>, <b>bold</b>')"#),
                3 => format!(r"({i},'trailing backslash \\')"),
                _ => format!(r"({i},'plain, with comma)')"),
            })
            .collect();
        let insert = format!("INSERT INTO `t` VALUES {}", rows.join(","));
        screen_statement(true, Dialect::MySql, &insert)
            .expect("a MySQL-dialect extended INSERT with escaped quotes must pass the screen");
        // The one-row shape from the report: a single `\'` in the only row.
        screen_statement(true, Dialect::MySql, r"INSERT INTO t VALUES (1,'it\'s')").unwrap();
        // The screen still refuses what it must, under both rules.
        assert!(screen_statement(true, Dialect::MySql, "ATTACH DATABASE 'x' AS y").is_err());
        assert!(screen_statement(true, Dialect::Sqlite, "ATTACH DATABASE 'x' AS y").is_err());
        assert!(screen_statement(true, Dialect::Pg, "VACUUM").is_err());
        // `sqlite`-native text keeps the standard rule: litewire's own
        // emitted form of the same value (quote doubled, backslash literal).
        screen_statement(true, Dialect::Sqlite, r"INSERT INTO t VALUES (1,'O\''Brien')").unwrap();
        // Disabled screening (the console's --maintenance) passes anything.
        screen_statement(false, Dialect::Sqlite, "PRAGMA writable_schema = 1").unwrap();
    }

    #[test]
    fn utf8_policy_follows_on_error_unless_overridden() {
        use OnErrorArg::{Skip, Stop};
        let cp1252 = ephpm_dbimport::Charset::parse("windows-1252").unwrap();
        // The defaults: `--on-error stop` rejects, `--on-error skip` strips.
        assert_eq!(resolve_utf8_policy(Stop, None, None).unwrap(), Utf8Policy::Reject);
        assert_eq!(resolve_utf8_policy(Skip, None, None).unwrap(), Utf8Policy::Strip);
        // `utf8mb4` is the native passthrough — the same defaults.
        assert_eq!(resolve_utf8_policy(Stop, None, Some("utf8mb4")).unwrap(), Utf8Policy::Reject);
        assert_eq!(resolve_utf8_policy(Skip, None, Some("UTF-8")).unwrap(), Utf8Policy::Strip);
        // A legacy charset means transcode, whatever --on-error says, and
        // MySQL's latin1 is windows-1252.
        for on_error in [Stop, Skip] {
            assert_eq!(
                resolve_utf8_policy(on_error, None, Some("latin1")).unwrap(),
                Utf8Policy::Transcode(cp1252)
            );
        }
        // Explicit --on-invalid-utf8 overrides the --on-error default.
        assert_eq!(
            resolve_utf8_policy(Stop, Some(OnInvalidUtf8Arg::Strip), None).unwrap(),
            Utf8Policy::Strip
        );
        assert_eq!(
            resolve_utf8_policy(Skip, Some(OnInvalidUtf8Arg::Reject), None).unwrap(),
            Utf8Policy::Reject
        );
        assert_eq!(
            resolve_utf8_policy(Skip, Some(OnInvalidUtf8Arg::Replace), Some("utf8mb4")).unwrap(),
            Utf8Policy::Replace
        );
        assert_eq!(
            resolve_utf8_policy(Stop, Some(OnInvalidUtf8Arg::Transcode), Some("windows-1252"))
                .unwrap(),
            Utf8Policy::Transcode(cp1252)
        );
    }

    #[test]
    fn utf8_policy_contradictions_are_refused() {
        use OnErrorArg::Stop;
        // transcode without a legacy charset has nothing to transcode from.
        let err = resolve_utf8_policy(Stop, Some(OnInvalidUtf8Arg::Transcode), None).unwrap_err();
        assert!(err.to_string().contains("legacy --source-charset"), "{err}");
        let err = resolve_utf8_policy(Stop, Some(OnInvalidUtf8Arg::Transcode), Some("utf8mb4"))
            .unwrap_err();
        assert!(err.to_string().contains("nothing to transcode from"), "{err}");
        // A legacy charset with a policy that would ignore it is a no-op flag.
        let err =
            resolve_utf8_policy(Stop, Some(OnInvalidUtf8Arg::Strip), Some("latin1")).unwrap_err();
        assert!(err.to_string().contains("would ignore --source-charset windows-1252"), "{err}");
        let err = resolve_utf8_policy(Stop, None, Some("gbk")).unwrap_err();
        assert!(format!("{err:#}").contains("not a single-byte charset"), "{err:#}");
        let err = resolve_utf8_policy(Stop, None, Some("klingon")).unwrap_err();
        assert!(format!("{err:#}").contains("unknown charset"), "{err:#}");
    }
}
