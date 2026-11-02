//! Shared engine for `ephpm db query` (one-shot) and `ephpm db shell` (REPL).
//!
//! Both consoles talk to a virtual host's embedded (Turso) database through the
//! **same** transport the importer uses ([`crate::db_import`]'s `DirectSink` /
//! `WireSink`) and the **same** strategy picker `ephpm php --site` uses
//! ([`crate::site_db`]). This module layers the interactive concerns on top:
//!
//! * **Transport & mode** — [`Console::connect`] probes the server's MySQL
//!   listener and, via [`site_db::choose_strategy`], picks *live* (forward to
//!   the running server, MySQL dialect only) or *offline* (direct-open the
//!   site's file, any dialect). The single-writer rule is enforced fail-closed:
//!   a `sqlite`-native (or `pg`) request while a server is up is a clear error,
//!   never a silent fallback.
//! * **Dialect** — `sqlite` (default) runs straight against the engine with no
//!   litewire translation; `mysql` / `pg` go through the translating `Session`.
//! * **Screening** — every statement is screened with
//!   [`ephpm_php::db_bridge::screen_sql`] in normal mode; `--maintenance`
//!   (offline-only) drops the screen so engine operations like
//!   `PRAGMA integrity_check` can run.
//! * **Read-only** — `--read-only` refuses any statement whose leading keyword
//!   is not a pure read, including a data-modifying CTE hidden behind a `WITH`
//!   (`WITH … DELETE …`, or a Postgres `WITH t AS (DELETE … RETURNING …)`).
//! * **Formatting** — result sets render as an aligned table, TSV, CSV, or a
//!   JSON array of objects.
//! * **KV** — the REPL's `/kv` and `/kv-put` reach the live KV over the same
//!   RESP client and per-site HMAC auth the `ephpm kv` subcommand uses. `/kv`
//!   also accepts an output-formatting **pipe** (`/kv <cmd…> | <format> <path>`):
//!   the RESP reply is normalized to a JSON value, an optional jq-lite path
//!   selects part of it, and a `raw`/`json`/`yaml`/`table` format renders the
//!   result (see [`parse_kv_pipe`] and [`render_kv_piped`]).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use bytes::BytesMut;
use ephpm_config::Config;
use ephpm_dbimport::{Cell, Dialect, RowSet, StatementSink};
use ephpm_kv::resp::{Frame, parse_frame};
use litewire::backend::SharedBackend;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::db_import::{DirectSink, WireSink, open_offline_backend};
use crate::site_db::{self, RemoteMysqlBackend, Strategy};

/// Output format for a result set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    /// Aligned, human-readable columns (the default).
    Table,
    /// Tab-separated values with a header row.
    Tsv,
    /// RFC 4180 comma-separated values with a header row.
    Csv,
    /// A JSON array of row objects (`[{"col": value, ...}, ...]`).
    Json,
}

impl Format {
    /// Parse a `/format` argument or `--format` value. Case-insensitive.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "table" => Some(Self::Table),
            "tsv" => Some(Self::Tsv),
            "csv" => Some(Self::Csv),
            "json" => Some(Self::Json),
            _ => None,
        }
    }

    /// The lowercase name used on the CLI and in the prompt.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::Tsv => "tsv",
            Self::Csv => "csv",
            Self::Json => "json",
        }
    }
}

/// Whether the console is forwarding to a live server or has the file open
/// directly offline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// A server answered on the MySQL listener; statements are forwarded to it.
    Live,
    /// No server is running; the site's database file is open directly.
    Offline,
}

impl Mode {
    /// The word shown in the REPL prompt (`‹live · mysql›`).
    fn label(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Offline => "offline",
        }
    }
}

/// Parse a `--dialect` / `/dialect` token into a [`Dialect`]. Case-insensitive.
/// `pg` and `postgres` are both accepted.
pub(crate) fn parse_dialect(s: &str) -> Option<Dialect> {
    match s.trim().to_ascii_lowercase().as_str() {
        "sqlite" => Some(Dialect::Sqlite),
        "mysql" => Some(Dialect::MySql),
        "pg" | "postgres" | "postgresql" => Some(Dialect::Pg),
        _ => None,
    }
}

/// The settings a console is built from, assembled from the CLI flags.
pub(crate) struct ConsoleConfig {
    /// The vhost key whose database to open.
    pub site: String,
    /// The ephpm config file.
    pub config_path: PathBuf,
    /// The initial dialect (`sqlite` by default).
    pub dialect: Dialect,
    /// Whether [`dialect`](Self::dialect) was given explicitly on the command
    /// line (vs left at the `sqlite` default). Live mode is MySQL-only: a
    /// *default* sqlite is auto-promoted to MySQL when the server is up, but an
    /// *explicit* `--dialect sqlite`/`pg` there is refused rather than silently
    /// overridden.
    pub dialect_explicit: bool,
    /// Reject any statement that is not a pure read.
    pub read_only: bool,
    /// Offline-only: drop statement screening so engine operations
    /// (`PRAGMA integrity_check`, `wal_checkpoint`, path-PRAGMAs) can run.
    pub maintenance: bool,
}

/// Everything needed to reach the live KV store for the REPL's `/kv` commands.
struct KvAccess {
    /// `[kv.redis_compat] listen` (default `127.0.0.1:6379`).
    listen: String,
    /// `[kv] secret`, for the per-site `AUTH <site> <hmac>` handshake.
    secret: Option<String>,
    /// Whether `[kv.redis_compat] enabled = true`.
    enabled: bool,
}

/// Per-connection state kept only in offline mode: the open backend (so a
/// `/dialect` switch rebinds a fresh session without reopening the file) and
/// the file path (so the log can be truncated at exit).
struct OfflineState {
    backend: SharedBackend,
    path: String,
}

/// A live connection to one site's database, driving both consoles.
pub(crate) struct Console {
    runtime: tokio::runtime::Runtime,
    site: String,
    mode: Mode,
    dialect: Dialect,
    read_only: bool,
    /// Screen each statement with `screen_sql`; `false` only under
    /// `--maintenance` (which is offline-only).
    screen: bool,
    sink: Box<dyn StatementSink>,
    /// `Some` in offline mode (for `/dialect` switching and the exit
    /// checkpoint); `None` in live mode (the server owns the file).
    offline: Option<OfflineState>,
    kv: KvAccess,
}

impl Console {
    /// Build a console: load the config, pick the transport, and open the
    /// database.
    ///
    /// # Errors
    ///
    /// Fails closed when the config or site key is invalid, the single-writer
    /// rule is violated (a `sqlite`/`pg` dialect requested while a server is
    /// up, or `--maintenance` with a server up / in per-site clustered mode),
    /// or the chosen transport cannot be established.
    pub(crate) fn connect(cfg: &ConsoleConfig) -> anyhow::Result<Self> {
        let config = Config::load(&cfg.config_path)
            .with_context(|| format!("loading configuration from {}", cfg.config_path.display()))?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .thread_name("ephpm-db-console")
            .build()
            .context("building the database runtime for the db console")?;

        let listen =
            config.db.sqlite.as_ref().map(|s| s.proxy.mysql_listen.clone()).unwrap_or_default();
        let reachable = if listen.is_empty() { None } else { site_db::server_reachable(&listen) };

        let strategy = site_db::choose_strategy(&config, &cfg.site, reachable)
            .context("resolving --site against the configuration")?;

        let kv = KvAccess {
            listen: config.kv.redis_compat.listen.clone(),
            secret: config.kv.secret.clone(),
            enabled: config.kv.redis_compat.enabled,
        };

        let (mode, dialect, screen, sink, offline) = match strategy {
            Strategy::Wire { addr, user, password } => {
                // Live mode is MySQL-only: only the server's MySQL listener is
                // bound, and the single-writer rule means we must forward to it
                // rather than open the file. A *default* sqlite dialect is
                // auto-promoted to MySQL here so `db shell`/`db query` just work
                // against a running server; only an *explicit* sqlite/pg request
                // is refused — silently overriding a deliberate choice would be
                // worse than a clear error telling you to stop the server.
                if cfg.dialect != Dialect::MySql && cfg.dialect_explicit {
                    bail!(server_up_dialect_error(cfg.dialect, &listen));
                }
                if cfg.maintenance {
                    bail!(
                        "a server is serving this site on {listen}, so --maintenance is \
                         refused: it is offline-only (it drops statement screening and needs \
                         exclusive access to the engine). Stop the server and re-run offline."
                    );
                }
                let backend = Self::connect_wire(&runtime, addr, &user, &password)?;
                let sink: Box<dyn StatementSink> =
                    Box::new(WireSink::new(backend, runtime.handle().clone(), true));
                (Mode::Live, Dialect::MySql, true, sink, None)
            }
            Strategy::DirectOpen { path } => {
                let screen = !cfg.maintenance;
                let backend = open_offline_backend(&path, runtime.handle())
                    .with_context(|| format!("opening the site database at {path}"))?;
                let direct = DirectSink::from_backend(
                    backend.clone(),
                    cfg.dialect,
                    runtime.handle().clone(),
                    screen,
                )?;
                let sink: Box<dyn StatementSink> = Box::new(direct);
                (Mode::Offline, cfg.dialect, screen, sink, Some(OfflineState { backend, path }))
            }
        };

        Ok(Self {
            runtime,
            site: cfg.site.clone(),
            mode,
            dialect,
            read_only: cfg.read_only,
            screen,
            sink,
            offline,
            kv,
        })
    }

    fn connect_wire(
        runtime: &tokio::runtime::Runtime,
        addr: SocketAddr,
        user: &str,
        password: &str,
    ) -> anyhow::Result<RemoteMysqlBackend> {
        runtime.block_on(RemoteMysqlBackend::connect(addr, user, password)).with_context(|| {
            format!("connecting to the running server's MySQL listener at {addr} as {user:?}")
        })
    }

    /// The active dialect.
    pub(crate) fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// The mode word for banners (`"live"` / `"offline"`).
    pub(crate) fn mode_label(&self) -> &'static str {
        self.mode.label()
    }

    /// The site key.
    pub(crate) fn site(&self) -> &str {
        &self.site
    }

    /// The REPL prompt, e.g. `  blog ‹offline · sqlite› > `.
    pub(crate) fn prompt(&self) -> String {
        format!(
            "  {} \u{2039}{} \u{b7} {}\u{203a} > ",
            self.site,
            self.mode.label(),
            self.dialect.as_str()
        )
    }

    /// Switch the active dialect. Live mode is MySQL-only; offline mode rebinds
    /// a fresh session over the already-open backend.
    ///
    /// # Errors
    ///
    /// Returns an error if a non-MySQL dialect is requested in live mode, or if
    /// the rebind fails.
    pub(crate) fn set_dialect(&mut self, dialect: Dialect) -> anyhow::Result<()> {
        if self.dialect == dialect {
            return Ok(());
        }
        match (&self.offline, self.mode) {
            (Some(state), Mode::Offline) => {
                let sink = DirectSink::from_backend(
                    state.backend.clone(),
                    dialect,
                    self.runtime.handle().clone(),
                    self.screen,
                )?;
                self.sink = Box::new(sink);
                self.dialect = dialect;
                Ok(())
            }
            _ => bail!(
                "live mode talks to the server's MySQL listener, so it is MySQL-only; \
                 '{}' is not available. Stop the server for offline access to sqlite/pg.",
                dialect.as_str()
            ),
        }
    }

    /// Run one SQL statement, returning its result set (empty for a write/DDL).
    ///
    /// Enforces `--read-only` (a non-read leading keyword is refused before the
    /// statement reaches the engine). Reads and `PRAGMA`s take the result path;
    /// everything else executes and returns an empty set.
    ///
    /// # Errors
    ///
    /// Returns an error if the statement is refused by `--read-only`, screening,
    /// or the backend.
    pub(crate) fn run_sql(&mut self, sql: &str) -> anyhow::Result<RowSet> {
        let trimmed = sql.trim();
        if trimmed.is_empty() {
            return Ok(RowSet::empty());
        }
        if self.read_only
            && let Some(kw) = write_keyword(trimmed)
        {
            bail!(
                "--read-only: refusing to run '{kw}' (only SELECT / WITH / VALUES / \
                 EXPLAIN / SHOW / DESCRIBE statements are allowed)"
            );
        }
        if routes_to_query(trimmed) {
            self.sink.query_rows(trimmed).map_err(|e| anyhow::anyhow!("{e}"))
        } else {
            self.sink.exec(trimmed).map(|()| RowSet::empty()).map_err(|e| anyhow::anyhow!("{e}"))
        }
    }

    /// List the user tables in the database (dialect-appropriate introspection).
    ///
    /// # Errors
    ///
    /// Propagates a backend error.
    pub(crate) fn list_tables(&mut self) -> anyhow::Result<RowSet> {
        let sql = match self.dialect {
            Dialect::Sqlite => {
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE \
                 'sqlite_%' ORDER BY name"
            }
            Dialect::MySql | Dialect::Pg => "SHOW TABLES",
        };
        self.sink.query_rows(sql).map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Describe one table's schema (dialect-appropriate introspection). The
    /// identifier is validated to a safe `[A-Za-z0-9_]+` set before use.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid table name or a backend failure.
    pub(crate) fn describe_table(&mut self, table: &str) -> anyhow::Result<RowSet> {
        if !is_safe_identifier(table) {
            bail!(
                "invalid table name {table:?}: only letters, digits and underscores are \
                 accepted by /schema"
            );
        }
        let sql = match self.dialect {
            // The stored CREATE statements are more useful than a column list.
            Dialect::Sqlite => format!(
                "SELECT type, name, sql FROM sqlite_master WHERE tbl_name = '{table}' \
                 ORDER BY type DESC, name"
            ),
            Dialect::MySql | Dialect::Pg => format!("DESCRIBE {table}"),
        };
        self.sink.query_rows(&sql).map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Run one `/kv` RESP command against the live KV store and return its
    /// rendered reply. Live mode only.
    ///
    /// With no `pipe`, the reply renders exactly as before ([`render_kv_frame`]).
    /// With a `pipe`, the reply is normalized to JSON, an optional path selects
    /// part of it, and the chosen format renders it ([`render_kv_piped`]).
    ///
    /// # Errors
    ///
    /// Fails when not in live mode, when the RESP listener is disabled or
    /// unreachable, when authentication fails, or when a pipe path does not
    /// resolve.
    pub(crate) fn kv_command(
        &self,
        args: &[String],
        pipe: Option<&KvPipe>,
    ) -> anyhow::Result<KvReply> {
        if args.is_empty() {
            bail!("/kv needs a command, e.g. /kv GET mykey");
        }
        let frame = self.kv_roundtrip(args)?;
        match pipe {
            None => Ok(KvReply { text: render_kv_frame(&frame), note: None }),
            Some(pipe) => render_kv_piped(&frame, pipe),
        }
    }

    /// Pipe the rows of `rows` into the live KV store: for each row, `SET` the
    /// interpolated `key_template` to `value_col` (or the whole-row JSON when
    /// `value_col` is `None`). Returns the number of keys written. Live mode
    /// only; this is the **client-side** loop (see the module docs and the
    /// REPL guide — the in-SQL `kv_set()` is a separate, future feature).
    ///
    /// # Errors
    ///
    /// Fails when not in live mode, the template names an unknown column, or a
    /// `SET` is rejected.
    pub(crate) fn kv_put(
        &self,
        rows: &RowSet,
        key_template: &str,
        value_col: Option<&str>,
    ) -> anyhow::Result<usize> {
        self.require_live_kv()?;
        if rows.is_empty() {
            return Ok(0);
        }
        let value_idx = match value_col {
            Some(col) => {
                Some(rows.columns.iter().position(|c| c == col).with_context(|| {
                    format!("value column {col:?} is not in the last result set")
                })?)
            }
            None => None,
        };

        let auth = self.kv_auth_frame();
        let mut conn = self
            .runtime
            .block_on(KvConn::connect(&self.kv.listen, auth))
            .with_context(|| format!("connecting to the KV RESP listener at {}", self.kv.listen))?;

        let mut written = 0usize;
        for row in &rows.rows {
            let key = interpolate_template(key_template, &rows.columns, row)
                .context("interpolating the /kv-put key template")?;
            let value = match value_idx {
                Some(i) => cell_to_plain(&row[i]),
                None => row_to_json(&rows.columns, row),
            };
            let reply = self.runtime.block_on(conn.command(vec![
                Frame::bulk(b"SET".to_vec()),
                Frame::bulk(key.into_bytes()),
                Frame::bulk(value.into_bytes()),
            ]))?;
            if let Frame::Error(e) = reply {
                bail!("KV SET failed: {e}");
            }
            written += 1;
        }
        Ok(written)
    }

    fn require_live_kv(&self) -> anyhow::Result<()> {
        if self.mode != Mode::Live {
            bail!(
                "/kv and /kv-put reach the live KV store, which needs a running server; \
                 this console is offline"
            );
        }
        if !self.kv.enabled {
            bail!(
                "the server's RESP listener is disabled ([kv.redis_compat] enabled = false), \
                 so /kv cannot reach the KV store"
            );
        }
        Ok(())
    }

    /// Build the per-site `AUTH` frame, or `None` when no secret is configured.
    fn kv_auth_frame(&self) -> Option<Frame> {
        let secret = self.kv.secret.as_deref().filter(|s| !s.is_empty())?;
        let password = ephpm_kv::auth::derive_site_password(secret, &self.site);
        Some(Frame::Array(vec![
            Frame::bulk(b"AUTH".to_vec()),
            Frame::bulk(self.site.as_bytes().to_vec()),
            Frame::bulk(password.into_bytes()),
        ]))
    }

    fn kv_roundtrip(&self, args: &[String]) -> anyhow::Result<Frame> {
        self.require_live_kv()?;
        let auth = self.kv_auth_frame();
        let cmd = Frame::Array(args.iter().map(|a| Frame::bulk(a.as_bytes().to_vec())).collect());
        self.runtime.block_on(async {
            let mut conn = KvConn::connect(&self.kv.listen, auth).await.with_context(|| {
                format!("connecting to the KV RESP listener at {}", self.kv.listen)
            })?;
            conn.command(vec![cmd]).await
        })
    }

    /// Truncate the offline database's WAL before exit (the Turso SDK never
    /// checkpoints on drop). A no-op in live mode. Call once, at shutdown.
    pub(crate) fn finish(&self) {
        let Some(state) = &self.offline else { return };
        let report = self.runtime.block_on(ephpm_server::db_shutdown::checkpoint_database(
            &self.site,
            std::path::Path::new(&state.path),
            Duration::from_secs(10),
        ));
        if !report.abandoned.is_empty() {
            tracing::warn!(
                path = %state.path,
                "db console: could not truncate the write-ahead log before exit (a \
                 transaction left open?); it is recovered on the next open"
            );
        }
    }
}

/// The actionable error for a `sqlite`/`pg` dialect requested while a server is
/// serving the site (the single-writer, fail-closed rule).
fn server_up_dialect_error(dialect: Dialect, listen: &str) -> String {
    format!(
        "a server is serving this site on {listen}; {} needs offline access (direct file \
         open) — use --dialect mysql for the live DB, or stop the server and re-run for \
         {} access.",
        dialect.as_str(),
        dialect.as_str()
    )
}

// ── statement classification ────────────────────────────────────────────────

/// The uppercased leading SQL keyword, skipping leading whitespace and
/// `--` / `/* */` comments. Empty when the statement is blank.
fn leading_keyword(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut i = 0usize;
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if bytes.get(i) == Some(&b'-') && bytes.get(i + 1) == Some(&b'-') {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes.get(i) == Some(&b'/') && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        break;
    }
    let start = i;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    sql[start..i].to_ascii_uppercase()
}

/// Leading keywords that return a result set and so take the query path
/// (`PRAGMA` is here because `PRAGMA integrity_check` / `table_info` return
/// rows; a `PRAGMA` *assignment* returns none and simply yields an empty set).
fn routes_to_query(sql: &str) -> bool {
    matches!(
        leading_keyword(sql).as_str(),
        "SELECT" | "WITH" | "VALUES" | "EXPLAIN" | "PRAGMA" | "SHOW" | "DESCRIBE" | "DESC"
    )
}

/// The leading keyword when a statement is **not** a pure read (used by
/// `--read-only`). `PRAGMA` is treated as a write here — conservatively, since
/// its assignment form mutates engine state — so read-only permits only the
/// unambiguous read verbs.
fn write_keyword(sql: &str) -> Option<String> {
    let kw = leading_keyword(sql);
    match kw.as_str() {
        "" | "SELECT" | "VALUES" | "EXPLAIN" | "SHOW" | "DESCRIBE" | "DESC" => None,
        // A CTE can wrap a data-modifying statement — `WITH x AS (...) DELETE ...`
        // (SQLite) or a data-modifying CTE body `WITH t AS (DELETE ... RETURNING ...)`
        // (Postgres). Classifying `WITH` as a pure read by its leading keyword alone
        // would let those slip past `--read-only`, so `WITH` is read-only only when it
        // carries no DML token.
        "WITH" => dml_token(sql),
        _ => Some(kw),
    }
}

/// The first data-modifying keyword (`INSERT` / `UPDATE` / `DELETE`) appearing
/// as a bare word token in `sql`, skipping the contents of string and
/// backtick-quoted literals and `--` / `/* */` comments. Used to stop a
/// data-modifying CTE from slipping past `--read-only` behind a `WITH`.
/// `REPLACE` / `MERGE` are deliberately excluded: `replace()` is a common
/// read-only scalar function, so matching the bare word would false-positive;
/// `INSERT`/`UPDATE`/`DELETE` are not standard function names.
fn dml_token(sql: &str) -> Option<String> {
    let bytes = sql.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == q {
                        // A doubled quote is an escaped quote, not a close.
                        if bytes.get(i + 1) == Some(&q) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b if b.is_ascii_alphabetic() || b == b'_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                let word = sql[start..i].to_ascii_uppercase();
                if matches!(word.as_str(), "INSERT" | "UPDATE" | "DELETE") {
                    return Some(word);
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// Whether `s` is a safe bare SQL identifier (`[A-Za-z0-9_]+`, non-empty) — the
/// only shape `/schema` interpolates into a query.
fn is_safe_identifier(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

// ── /kv-put template interpolation ───────────────────────────────────────────

/// Expand `${col}` placeholders in `template` with each row cell's plain value.
///
/// A `${col}` naming a column not in `columns` is an error (so a typo fails
/// loudly rather than writing a key with a literal `${col}` in it). `$$` is a
/// literal `$`.
///
/// # Errors
///
/// Returns an error for an unterminated `${`, or a `${col}` naming an unknown
/// column.
pub(crate) fn interpolate_template(
    template: &str,
    columns: &[String],
    row: &[Cell],
) -> anyhow::Result<String> {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'$' && bytes.get(i + 1) == Some(&b'$') {
            out.push('$');
            i += 2;
            continue;
        }
        if bytes[i] == b'$' && bytes.get(i + 1) == Some(&b'{') {
            let Some(end) = template[i + 2..].find('}') else {
                bail!("unterminated ${{ in key template {template:?}");
            };
            let name = &template[i + 2..i + 2 + end];
            let idx = columns
                .iter()
                .position(|c| c == name)
                .with_context(|| format!("key template names unknown column {name:?}"))?;
            out.push_str(&cell_to_plain(&row[idx]));
            i += 2 + end + 1;
            continue;
        }
        // Advance one UTF-8 char so multibyte template text is preserved.
        let ch_len = utf8_len(bytes[i]);
        out.push_str(&template[i..(i + ch_len).min(template.len())]);
        i += ch_len;
    }
    Ok(out)
}

/// Byte length of a UTF-8 sequence from its lead byte.
fn utf8_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// Split `sql` into individual statements on top-level `;`, skipping separators
/// hidden inside string literals (`'`, `"`, `` ` ``), `--` line comments and
/// `/* */` block comments. Blank statements are dropped; each result is
/// trimmed. A trailing statement with no `;` is returned too.
pub(crate) fn split_statements(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == q {
                        // A doubled quote is an escaped quote, not a close.
                        if bytes.get(i + 1) == Some(&q) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b';' => {
                push_statement(&mut out, &sql[start..i]);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    push_statement(&mut out, &sql[start..]);
    out
}

/// Push `stmt` onto `out` if it holds more than whitespace.
fn push_statement(out: &mut Vec<String>, stmt: &str) {
    let trimmed = stmt.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_string());
    }
}

// ── value rendering ──────────────────────────────────────────────────────────

/// A cell's plain string value (no quoting): `NULL` renders empty, a blob as
/// lowercase hex. Used for KV values and template interpolation.
fn cell_to_plain(cell: &Cell) -> String {
    match cell {
        Cell::Null => String::new(),
        Cell::Integer(i) => i.to_string(),
        Cell::Float(f) => f.to_string(),
        Cell::Text(s) => s.clone(),
        Cell::Blob(b) => hex(b),
    }
}

/// A single row as a compact JSON object (column → value), for `/kv-put`
/// whole-row mode.
fn row_to_json(columns: &[String], row: &[Cell]) -> String {
    let obj: serde_json::Map<String, serde_json::Value> =
        columns.iter().zip(row.iter()).map(|(c, v)| (c.clone(), cell_to_json(v))).collect();
    serde_json::Value::Object(obj).to_string()
}

/// Lowercase hex for a blob cell.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Map a [`Cell`] to a JSON value: NULL → null, integers/floats → numbers, text
/// → string, blob → a hex string.
fn cell_to_json(cell: &Cell) -> serde_json::Value {
    match cell {
        Cell::Null => serde_json::Value::Null,
        Cell::Integer(i) => serde_json::Value::from(*i),
        Cell::Float(f) => serde_json::Number::from_f64(*f)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        Cell::Text(s) => serde_json::Value::String(s.clone()),
        Cell::Blob(b) => serde_json::Value::String(hex(b)),
    }
}

/// Render a RESP reply frame as a single display line for `/kv`.
fn render_kv_frame(frame: &Frame) -> String {
    match frame {
        Frame::Simple(s) => s.clone(),
        Frame::Error(e) => format!("(error) {e}"),
        Frame::Integer(n) => format!("(integer) {n}"),
        Frame::Bulk(b) => match std::str::from_utf8(b) {
            Ok(s) => s.to_string(),
            Err(_) => format!("<{} bytes of binary data>", b.len()),
        },
        Frame::Null => "(nil)".to_string(),
        Frame::Array(items) => {
            if items.is_empty() {
                "(empty)".to_string()
            } else {
                items
                    .iter()
                    .enumerate()
                    .map(|(i, f)| format!("{}) {}", i + 1, render_kv_frame(f)))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
    }
}

// ── /kv output pipe (`/kv <cmd…> | <format> <path>`) ──────────────────────────

/// An output format for the `/kv` pipe. `Raw` with no path is the classic
/// [`render_kv_frame`] rendering; the others operate on the RESP reply after it
/// is normalized to JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KvFormat {
    /// A selected scalar bare; a selected object/array as compact JSON. With no
    /// path, the classic reply rendering.
    Raw,
    /// Pretty-printed (2-space) JSON.
    Json,
    /// YAML (via `serde_yaml_ng`).
    Yaml,
    /// A `key | value` table (object) or a multi-column table (array of
    /// objects), mirroring the result-set table style.
    Table,
}

impl KvFormat {
    /// Parse a pipe format keyword. Case-insensitive; `yml` is an alias of
    /// `yaml`.
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "raw" => Some(Self::Raw),
            "json" => Some(Self::Json),
            "yaml" | "yml" => Some(Self::Yaml),
            "table" => Some(Self::Table),
            _ => None,
        }
    }
}

/// A parsed `/kv … | <format> <path>` pipe. At least one of the two fields is
/// present — an empty pipe is a parse error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KvPipe {
    /// An explicit format keyword, or `None` for "auto" (a scalar prints raw;
    /// an object/array prints pretty JSON).
    pub(crate) format: Option<KvFormat>,
    /// A jq-lite field-selector path, or `None`.
    pub(crate) path: Option<String>,
}

/// The stdout text and an optional stderr note a `/kv` command produces.
pub(crate) struct KvReply {
    /// The rendered reply, for stdout.
    pub(crate) text: String,
    /// A one-line advisory for stderr (the fail-soft non-JSON note), or `None`.
    pub(crate) note: Option<String>,
}

/// Parse the text after a top-level `|` in a `/kv` line into a [`KvPipe`].
///
/// The grammar is `[ <format> ] [ <path> ]`: an optional leading format keyword
/// (`raw` | `json` | `yaml` | `table`) followed by an optional path token (one
/// beginning with `.`). At least one must be present. Tokens are
/// whitespace-separated, so a path carries no spaces (a comma list like
/// `.a,.b` is one token).
///
/// # Errors
///
/// Returns an error when the pipe is empty, names an unknown format, places a
/// non-path bareword where a path is expected, or carries extra tokens.
pub(crate) fn parse_kv_pipe(text: &str) -> anyhow::Result<KvPipe> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let mut idx = 0usize;
    let mut format = None;

    if let Some(first) = tokens.first()
        && !first.starts_with('.')
    {
        match KvFormat::parse(first) {
            Some(f) => {
                format = Some(f);
                idx = 1;
            }
            None => bail!(
                "unknown pipe format {first:?}: want raw | json | yaml | table, or a path \
                 starting with '.'"
            ),
        }
    }

    let path = match tokens.get(idx) {
        Some(tok) => {
            if !tok.starts_with('.') {
                bail!("unexpected pipe token {tok:?}: a path must start with '.'");
            }
            idx += 1;
            Some((*tok).to_string())
        }
        None => None,
    };

    if let Some(extra) = tokens.get(idx) {
        bail!("pipe takes at most one format and one path (unexpected {extra:?})");
    }

    if format.is_none() && path.is_none() {
        bail!("pipe needs a format or a path, e.g. | json .price");
    }

    Ok(KvPipe { format, path })
}

/// Normalize a RESP reply [`Frame`] to a [`Value`] so the `/kv` pipe can select
/// and reformat any command's reply uniformly:
///
/// * `Simple` / `Error` → a JSON string.
/// * `Integer` → a JSON number.
/// * `Null` → JSON null.
/// * `Bulk` → the parsed JSON when the bytes are valid JSON, else the UTF-8
///   text as a string, else a lowercase-hex string (non-UTF-8).
/// * `Array` → a JSON array of recursively-normalized items.
fn frame_to_json(frame: &Frame) -> Value {
    match frame {
        Frame::Simple(s) | Frame::Error(s) => Value::String(s.clone()),
        Frame::Integer(n) => Value::from(*n),
        Frame::Null => Value::Null,
        Frame::Bulk(b) => bulk_to_json(b),
        Frame::Array(items) => Value::Array(items.iter().map(frame_to_json).collect()),
    }
}

/// A bulk value as JSON: the parsed JSON if it is valid JSON, else the UTF-8
/// text as a string, else lowercase hex (non-UTF-8).
fn bulk_to_json(b: &[u8]) -> Value {
    if let Ok(v) = serde_json::from_slice::<Value>(b) {
        return v;
    }
    match std::str::from_utf8(b) {
        Ok(s) => Value::String(s.to_string()),
        Err(_) => Value::String(hex(b)),
    }
}

/// When `frame` is a top-level bulk value that is **not** valid JSON, return its
/// raw display text (UTF-8, or a `<N bytes…>` summary) so the pipe can fail soft
/// rather than quote a bare scalar, error on a path, or panic. `None` otherwise.
fn non_json_bulk_raw(frame: &Frame) -> Option<String> {
    let Frame::Bulk(b) = frame else { return None };
    if serde_json::from_slice::<Value>(b).is_ok() {
        return None;
    }
    Some(match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(_) => format!("<{} bytes of binary data>", b.len()),
    })
}

/// Apply a `/kv` pipe to a reply frame, producing the stdout text and an
/// optional stderr note.
///
/// # Errors
///
/// Returns an error only when a path fails to resolve or YAML serialization
/// fails. A non-JSON bulk value fails **soft** (raw + a note), never an error.
fn render_kv_piped(frame: &Frame, pipe: &KvPipe) -> anyhow::Result<KvReply> {
    // An error reply prints exactly as it does without a pipe.
    if let Frame::Error(_) = frame {
        return Ok(KvReply { text: render_kv_frame(frame), note: None });
    }

    // `| raw` with no path is the classic rendering — no normalization needed.
    if pipe.format == Some(KvFormat::Raw) && pipe.path.is_none() {
        return Ok(KvReply { text: render_kv_frame(frame), note: None });
    }

    // Fail soft: a top-level bulk value that is not JSON cannot be selected or
    // structurally reformatted. Print it raw with a one-line note rather than a
    // quoted scalar, a path error, or a panic.
    if let Some(raw) = non_json_bulk_raw(frame) {
        return Ok(KvReply {
            text: raw,
            note: Some("value is not JSON; printed raw".to_string()),
        });
    }

    let value = frame_to_json(frame);
    let selected = match &pipe.path {
        Some(path) => kv_path::select(&value, path)?,
        None => vec![value],
    };

    let mut lines = Vec::with_capacity(selected.len());
    for v in &selected {
        lines.push(format_kv_value(v, pipe.format)?);
    }
    Ok(KvReply { text: lines.join("\n"), note: None })
}

/// Render one selected [`Value`] in the pipe's format (`None` = auto).
fn format_kv_value(value: &Value, format: Option<KvFormat>) -> anyhow::Result<String> {
    match format {
        Some(KvFormat::Raw) => Ok(raw_value(value)),
        Some(KvFormat::Json) => Ok(pretty_json(value)),
        Some(KvFormat::Yaml) => yaml_value(value),
        Some(KvFormat::Table) => Ok(table_value(value)),
        // auto: a scalar prints raw; an object/array prints pretty JSON.
        None => Ok(auto_value(value)),
    }
}

/// A scalar as its bare string; an object/array as compact JSON.
fn raw_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        // Numbers, booleans and null print as their JSON scalar form; objects
        // and arrays as compact JSON.
        _ => value.to_string(),
    }
}

/// Auto rendering: a scalar prints raw; an object or array prints pretty JSON.
fn auto_value(value: &Value) -> String {
    match value {
        Value::Array(_) | Value::Object(_) => pretty_json(value),
        _ => raw_value(value),
    }
}

/// Pretty-printed (2-space, serde_json default) JSON.
fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// YAML for `value` (trailing newline trimmed).
fn yaml_value(value: &Value) -> anyhow::Result<String> {
    let s = serde_yaml_ng::to_string(value).context("serializing the /kv reply to YAML")?;
    Ok(s.trim_end().to_string())
}

/// Render a [`Value`] as a table, mirroring the result-set table style:
///
/// * an **object** → a two-column `key | value` table;
/// * a non-empty **array of objects** → a multi-column table (columns are the
///   union of keys in first-seen order; a missing key is an empty cell);
/// * any other array → a one-column `value` table;
/// * a **scalar** → the scalar itself.
///
/// A nested object/array in a cell is rendered as compact JSON.
fn table_value(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let columns = vec!["key".to_string(), "value".to_string()];
            let rows: Vec<Vec<String>> =
                map.iter().map(|(k, v)| vec![k.clone(), cell_json(v)]).collect();
            render_table_grid(&columns, &rows)
        }
        Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_object) => {
            let mut columns: Vec<String> = Vec::new();
            for item in items {
                if let Value::Object(map) = item {
                    for k in map.keys() {
                        if !columns.iter().any(|c| c == k) {
                            columns.push(k.clone());
                        }
                    }
                }
            }
            let rows: Vec<Vec<String>> = items
                .iter()
                .map(|item| {
                    columns
                        .iter()
                        .map(|c| item.get(c).map_or_else(String::new, cell_json))
                        .collect()
                })
                .collect();
            render_table_grid(&columns, &rows)
        }
        Value::Array(items) => {
            let columns = vec!["value".to_string()];
            let rows: Vec<Vec<String>> = items.iter().map(|v| vec![cell_json(v)]).collect();
            render_table_grid(&columns, &rows)
        }
        scalar => raw_value(scalar),
    }
}

/// A table cell's text: a scalar plainly, a nested object/array as compact JSON.
fn cell_json(value: &Value) -> String {
    match value {
        Value::Array(_) | Value::Object(_) => value.to_string(),
        _ => raw_value(value),
    }
}

/// A tiny jq-lite field selector over a [`Value`], used by the `/kv` pipe.
///
/// Supported syntax:
/// * `.` — identity (the whole value).
/// * `.field` / `.a.b.c` — object field access, nestable.
/// * `.arr[2]` / `.arr[-1]` — array index (negative counts from the end).
/// * `.arr[]` — fan out: each element becomes its own output value/line.
/// * `.a,.b` — a comma list: build one reduced object keyed by each branch's
///   final field segment (a deliberate simplification of jq's comma stream).
mod kv_path {
    use anyhow::{Result, anyhow, bail};
    use serde_json::Value;

    /// One step in a single path expression.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Step {
        /// `.field`
        Field(String),
        /// `[n]` / `[-n]`
        Index(i64),
        /// `[]` — fan out over an array's elements.
        FanOut,
    }

    /// Apply `path` (possibly a comma list) to `value`.
    ///
    /// A single expression yields one value, or several when it contains a `[]`
    /// fan-out. A comma list yields one reduced object.
    ///
    /// # Errors
    ///
    /// Returns an error naming the path when a field is missing, an index is out
    /// of range, a step meets the wrong kind of value, or the syntax is invalid.
    pub(super) fn select(value: &Value, path: &str) -> Result<Vec<Value>> {
        let branches = split_branches(path);
        if branches.is_empty() {
            bail!("path {path:?} is empty");
        }
        if branches.len() == 1 {
            let steps = parse_steps(branches[0])?;
            return apply(value, &steps, branches[0]);
        }
        let mut obj = serde_json::Map::new();
        for branch in &branches {
            let steps = parse_steps(branch)?;
            let resolved = apply(value, &steps, branch)?;
            let v = if resolved.len() == 1 {
                resolved.into_iter().next().unwrap_or(Value::Null)
            } else {
                Value::Array(resolved)
            };
            obj.insert(branch_key(&steps, branch), v);
        }
        Ok(vec![Value::Object(obj)])
    }

    /// Split a comma list into its branches, trimming each and dropping empties.
    fn split_branches(path: &str) -> Vec<&str> {
        path.split(',').map(str::trim).filter(|s| !s.is_empty()).collect()
    }

    /// The object key a comma branch contributes: its last field segment, else
    /// the branch text with its leading dot stripped.
    fn branch_key(steps: &[Step], branch: &str) -> String {
        for step in steps.iter().rev() {
            if let Step::Field(name) = step {
                return name.clone();
            }
        }
        branch.trim_start_matches('.').to_string()
    }

    /// Parse one path expression (no commas) into its steps. `.` alone is the
    /// identity (no steps).
    fn parse_steps(expr: &str) -> Result<Vec<Step>> {
        let expr = expr.trim();
        if !expr.starts_with('.') {
            bail!("path {expr:?} must start with '.'");
        }
        if expr == "." {
            return Ok(Vec::new());
        }
        let bytes = expr.as_bytes();
        let mut i = 0usize;
        let mut steps = Vec::new();
        while i < bytes.len() {
            match bytes[i] {
                b'.' => {
                    i += 1;
                    let start = i;
                    while i < bytes.len()
                        && (bytes[i].is_ascii_alphanumeric()
                            || bytes[i] == b'_'
                            || bytes[i] == b'-')
                    {
                        i += 1;
                    }
                    if i > start {
                        steps.push(Step::Field(expr[start..i].to_string()));
                    } else if bytes.get(i) == Some(&b'[') {
                        // `.[` — the index/fan-out step follows; the dot is just
                        // a separator here.
                    } else {
                        bail!("path {expr:?}: expected a field name after '.'");
                    }
                }
                b'[' => {
                    let end = expr[i..]
                        .find(']')
                        .map(|rel| i + rel)
                        .ok_or_else(|| anyhow!("path {expr:?}: missing ']'"))?;
                    let inner = expr[i + 1..end].trim();
                    if inner.is_empty() {
                        steps.push(Step::FanOut);
                    } else {
                        let idx: i64 = inner
                            .parse()
                            .map_err(|_| anyhow!("path {expr:?}: invalid array index {inner:?}"))?;
                        steps.push(Step::Index(idx));
                    }
                    i = end + 1;
                }
                other => bail!("path {expr:?}: unexpected character {:?}", other as char),
            }
        }
        Ok(steps)
    }

    /// Apply `steps` to `value`, threading a working set so `[]` can fan out.
    fn apply(value: &Value, steps: &[Step], path: &str) -> Result<Vec<Value>> {
        let mut set = vec![value.clone()];
        for step in steps {
            let mut next = Vec::new();
            for v in &set {
                match step {
                    Step::Field(name) => {
                        let obj = v.as_object().ok_or_else(|| {
                            anyhow!("path {path}: field {name:?} needs an object, found {}", kind(v))
                        })?;
                        let field = obj
                            .get(name)
                            .ok_or_else(|| anyhow!("path {path}: field {name:?} not found"))?;
                        next.push(field.clone());
                    }
                    Step::Index(idx) => {
                        let arr = v.as_array().ok_or_else(|| {
                            anyhow!("path {path}: index {idx} needs an array, found {}", kind(v))
                        })?;
                        let len = arr.len();
                        // Resolve a (possibly negative) index to an in-range
                        // offset without any lossy int casts.
                        let resolved = if *idx < 0 {
                            usize::try_from(idx.unsigned_abs())
                                .ok()
                                .and_then(|from_end| len.checked_sub(from_end))
                        } else {
                            usize::try_from(*idx).ok().filter(|&i| i < len)
                        };
                        let resolved = resolved.ok_or_else(|| {
                            anyhow!("path {path}: index {idx} out of range (len {len})")
                        })?;
                        next.push(arr[resolved].clone());
                    }
                    Step::FanOut => {
                        let arr = v.as_array().ok_or_else(|| {
                            anyhow!("path {path}: '[]' needs an array, found {}", kind(v))
                        })?;
                        next.extend(arr.iter().cloned());
                    }
                }
            }
            set = next;
        }
        Ok(set)
    }

    /// A human word for a value's kind, for error messages.
    fn kind(v: &Value) -> &'static str {
        match v {
            Value::Null => "null",
            Value::Bool(_) => "a boolean",
            Value::Number(_) => "a number",
            Value::String(_) => "a string",
            Value::Array(_) => "an array",
            Value::Object(_) => "an object",
        }
    }

    #[cfg(test)]
    mod tests {
        use serde_json::json;

        use super::*;

        fn one(value: &Value, path: &str) -> Value {
            let out = select(value, path).unwrap();
            assert_eq!(out.len(), 1, "expected a single value for {path}");
            out.into_iter().next().unwrap()
        }

        fn sample() -> Value {
            json!({
                "id": 42,
                "slug": "blue-hat",
                "price": 1900,
                "tags": ["a", "b", "c"],
                "meta": { "nested": { "deep": true } }
            })
        }

        #[test]
        fn identity_returns_whole_value() {
            assert_eq!(one(&sample(), "."), sample());
        }

        #[test]
        fn field_and_nested_field() {
            assert_eq!(one(&sample(), ".slug"), json!("blue-hat"));
            assert_eq!(one(&sample(), ".meta.nested.deep"), json!(true));
        }

        #[test]
        fn index_and_negative_index() {
            assert_eq!(one(&sample(), ".tags[0]"), json!("a"));
            assert_eq!(one(&sample(), ".tags[-1]"), json!("c"));
            assert_eq!(one(&sample(), ".tags[-3]"), json!("a"));
        }

        #[test]
        fn fan_out_yields_each_element() {
            let out = select(&sample(), ".tags[]").unwrap();
            assert_eq!(out, vec![json!("a"), json!("b"), json!("c")]);
        }

        #[test]
        fn comma_builds_reduced_object_keyed_by_final_segment() {
            let out = one(&sample(), ".slug,.price");
            assert_eq!(out, json!({ "slug": "blue-hat", "price": 1900 }));
            // A nested branch keys on its last field.
            let out = one(&sample(), ".slug,.meta.nested");
            assert_eq!(out, json!({ "slug": "blue-hat", "nested": { "deep": true } }));
        }

        #[test]
        fn missing_field_errors_naming_the_path() {
            let err = select(&sample(), ".nope").unwrap_err().to_string();
            assert!(err.contains(".nope"), "{err}");
            assert!(err.contains("not found"), "{err}");
        }

        #[test]
        fn out_of_range_index_errors() {
            let err = select(&sample(), ".tags[9]").unwrap_err().to_string();
            assert!(err.contains("out of range"), "{err}");
        }

        #[test]
        fn wrong_kind_errors() {
            assert!(select(&sample(), ".slug.x").is_err());
            assert!(select(&sample(), ".slug[0]").is_err());
        }

        #[test]
        fn bad_syntax_errors() {
            assert!(select(&sample(), "price").is_err());
            assert!(select(&sample(), ".tags[").is_err());
            assert!(select(&sample(), ".tags[x]").is_err());
        }
    }
}

// ── result-set formatting ─────────────────────────────────────────────────────

/// Render a result set in `format`. A set with no columns (a write/DDL) renders
/// to an empty string — the caller prints its own `OK`.
pub(crate) fn format_rows(rows: &RowSet, format: Format) -> String {
    if rows.columns.is_empty() {
        return String::new();
    }
    match format {
        Format::Table => format_table(rows),
        Format::Tsv => format_separated(rows, '\t'),
        Format::Csv => format_csv(rows),
        Format::Json => format_json(rows),
    }
}

/// An aligned ASCII table with a header and a separator rule.
fn format_table(rows: &RowSet) -> String {
    let cells: Vec<Vec<String>> =
        rows.rows.iter().map(|row| row.iter().map(cell_for_table).collect::<Vec<_>>()).collect();
    render_table_grid(&rows.columns, &cells)
}

/// Render `columns` + pre-stringified `rows` as an aligned ASCII table with a
/// header, a `-+-` separator rule, and a `(N rows)` footer. Shared by the
/// result-set formatter ([`format_table`]) and the `/kv` pipe's `table` output
/// ([`table_value`]).
fn render_table_grid(columns: &[String], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = columns.iter().map(|c| c.chars().count()).collect();
    for row in rows {
        for (i, c) in row.iter().enumerate() {
            if let Some(w) = widths.get_mut(i) {
                *w = (*w).max(c.chars().count());
            }
        }
    }

    let mut out = String::new();
    let header = columns
        .iter()
        .enumerate()
        .map(|(i, c)| pad(c, widths[i]))
        .collect::<Vec<_>>()
        .join(" | ");
    out.push_str(&header);
    out.push('\n');
    out.push_str(&widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("-+-"));
    out.push('\n');
    for row in rows {
        let line = row
            .iter()
            .enumerate()
            .map(|(i, c)| pad(c, *widths.get(i).unwrap_or(&0)))
            .collect::<Vec<_>>()
            .join(" | ");
        out.push_str(&line);
        out.push('\n');
    }
    use std::fmt::Write as _;
    let _ = write!(out, "({} row{})", rows.len(), if rows.len() == 1 { "" } else { "s" });
    out
}

/// Pad `s` with trailing spaces to `width` display columns.
fn pad(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len >= width { s.to_string() } else { format!("{s}{}", " ".repeat(width - len)) }
}

/// A table cell's display text (NULL shown as `NULL`, blobs summarised).
fn cell_for_table(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_string(),
        Cell::Blob(b) => format!("<blob {} bytes>", b.len()),
        other => cell_to_plain(other),
    }
}

/// Tab/other-delimited output with a header row. Tabs and newlines inside text
/// are replaced with spaces so each row stays one physical line.
fn format_separated(rows: &RowSet, sep: char) -> String {
    let mut out = String::new();
    out.push_str(&rows.columns.join(&sep.to_string()));
    out.push('\n');
    for row in &rows.rows {
        let line = row
            .iter()
            .map(|c| cell_to_plain(c).replace(['\t', '\r', '\n'], " "))
            .collect::<Vec<_>>()
            .join(&sep.to_string());
        out.push_str(&line);
        out.push('\n');
    }
    out.truncate(out.trim_end_matches('\n').len());
    out
}

/// RFC 4180 CSV with a header row.
fn format_csv(rows: &RowSet) -> String {
    let mut out = String::new();
    out.push_str(&rows.columns.iter().map(|c| csv_field(c)).collect::<Vec<_>>().join(","));
    out.push('\n');
    for row in &rows.rows {
        let line = row.iter().map(|c| csv_field(&cell_to_plain(c))).collect::<Vec<_>>().join(",");
        out.push_str(&line);
        out.push('\n');
    }
    out.truncate(out.trim_end_matches('\n').len());
    out
}

/// Quote a CSV field when it contains a comma, quote, CR or LF (doubling any
/// embedded quote), per RFC 4180.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\r', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// A JSON array of row objects.
fn format_json(rows: &RowSet) -> String {
    let arr: Vec<serde_json::Value> = rows
        .rows
        .iter()
        .map(|row| {
            let obj: serde_json::Map<String, serde_json::Value> = rows
                .columns
                .iter()
                .zip(row.iter())
                .map(|(c, v)| (c.clone(), cell_to_json(v)))
                .collect();
            serde_json::Value::Object(obj)
        })
        .collect();
    serde_json::Value::Array(arr).to_string()
}

// ── minimal RESP client (for /kv and /kv-put) ─────────────────────────────────

/// One RESP connection to the live KV listener, authenticated on connect.
struct KvConn {
    stream: TcpStream,
}

impl KvConn {
    /// Connect to `listen` and, when `auth` is `Some`, send it and check the
    /// reply before returning.
    async fn connect(listen: &str, auth: Option<Frame>) -> anyhow::Result<Self> {
        let stream = TcpStream::connect(listen)
            .await
            .with_context(|| format!("could not connect to KV listener at {listen}"))?;
        let mut conn = Self { stream };
        if let Some(frame) = auth {
            match conn.send_recv(&frame).await? {
                Frame::Simple(_) => {}
                Frame::Error(e) => bail!("KV authentication failed: {e}"),
                other => bail!("unexpected AUTH response: {}", render_kv_frame(&other)),
            }
        }
        Ok(conn)
    }

    /// Send one command (a vec of argument frames, wrapped in a RESP array) and
    /// return the reply.
    async fn command(&mut self, args: Vec<Frame>) -> anyhow::Result<Frame> {
        // `/kv` passes a single pre-built array frame; a bare arg vec is wrapped.
        let frame = match args.as_slice() {
            [only @ Frame::Array(_)] => only.clone(),
            _ => Frame::Array(args),
        };
        self.send_recv(&frame).await
    }

    async fn send_recv(&mut self, frame: &Frame) -> anyhow::Result<Frame> {
        self.stream
            .write_all(&frame.to_bytes())
            .await
            .context("failed to write command to KV listener")?;
        let mut buf = BytesMut::with_capacity(4096);
        loop {
            buf.reserve(512);
            let n =
                self.stream.read_buf(&mut buf).await.context("failed to read from KV listener")?;
            if n == 0 {
                bail!("KV listener closed the connection unexpectedly");
            }
            if let Some(f) = parse_frame(&mut buf).context("invalid RESP data from KV listener")? {
                return Ok(f);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    fn sample() -> RowSet {
        RowSet {
            columns: cols(&["id", "name"]),
            rows: vec![
                vec![Cell::Integer(1), Cell::Text("Ada".into())],
                vec![Cell::Integer(2), Cell::Null],
            ],
        }
    }

    #[test]
    fn dialect_parsing_is_case_insensitive_and_accepts_postgres_aliases() {
        assert_eq!(parse_dialect("SQLite"), Some(Dialect::Sqlite));
        assert_eq!(parse_dialect("mysql"), Some(Dialect::MySql));
        assert_eq!(parse_dialect("pg"), Some(Dialect::Pg));
        assert_eq!(parse_dialect(" Postgres "), Some(Dialect::Pg));
        assert_eq!(parse_dialect("oracle"), None);
    }

    #[test]
    fn format_parsing() {
        assert_eq!(Format::parse("TABLE"), Some(Format::Table));
        assert_eq!(Format::parse("json"), Some(Format::Json));
        assert_eq!(Format::parse("yaml"), None);
    }

    #[test]
    fn leading_keyword_skips_comments_and_whitespace() {
        assert_eq!(leading_keyword("  SELECT 1"), "SELECT");
        assert_eq!(leading_keyword("-- c\nUPDATE t SET x=1"), "UPDATE");
        assert_eq!(leading_keyword("/* hi */ insert into t"), "INSERT");
        assert_eq!(leading_keyword("   "), "");
    }

    #[test]
    fn read_only_classification() {
        assert_eq!(write_keyword("SELECT * FROM t"), None);
        assert_eq!(write_keyword("  with x as (select 1) select * from x"), None);
        assert_eq!(write_keyword("INSERT INTO t VALUES (1)").as_deref(), Some("INSERT"));
        assert_eq!(write_keyword("drop table t").as_deref(), Some("DROP"));
        // PRAGMA is conservatively a write under --read-only.
        assert_eq!(write_keyword("PRAGMA integrity_check").as_deref(), Some("PRAGMA"));
    }

    #[test]
    fn read_only_blocks_data_modifying_cte() {
        // A plain read-only CTE is allowed.
        assert_eq!(write_keyword("WITH x AS (SELECT 1) SELECT * FROM x"), None);
        // SQLite-style: a DML after the CTE must be refused.
        assert_eq!(write_keyword("WITH x AS (SELECT 1) DELETE FROM t").as_deref(), Some("DELETE"));
        // Postgres-style data-modifying CTE body must be refused.
        assert_eq!(
            write_keyword("WITH t AS (DELETE FROM x RETURNING *) SELECT * FROM t").as_deref(),
            Some("DELETE")
        );
        // replace() as a scalar function is NOT a DML false-positive.
        assert_eq!(
            write_keyword("WITH x AS (SELECT replace(a,'d','e') FROM t) SELECT * FROM x"),
            None
        );
        // An identifier containing a DML word is not a false positive.
        assert_eq!(write_keyword("WITH x AS (SELECT deleted_at FROM t) SELECT * FROM x"), None);
        // A DML word hidden in a string literal is not flagged.
        assert_eq!(write_keyword("WITH x AS (SELECT 'DELETE me' AS s) SELECT * FROM x"), None);
    }

    #[test]
    fn query_routing() {
        assert!(routes_to_query("SELECT 1"));
        assert!(routes_to_query("pragma table_info('t')"));
        assert!(routes_to_query("SHOW TABLES"));
        assert!(!routes_to_query("INSERT INTO t VALUES (1)"));
        assert!(!routes_to_query("CREATE TABLE t (id int)"));
    }

    #[test]
    fn table_format_aligns_and_counts() {
        let out = format_table(&sample());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "id | name");
        assert!(lines[1].starts_with("--"));
        assert_eq!(lines[2], "1  | Ada ");
        assert_eq!(lines[3], "2  | NULL");
        assert_eq!(lines[4], "(2 rows)");
    }

    #[test]
    fn tsv_and_csv_render_null_as_empty() {
        assert_eq!(format_separated(&sample(), '\t'), "id\tname\n1\tAda\n2\t");
        assert_eq!(format_csv(&sample()), "id,name\n1,Ada\n2,");
    }

    #[test]
    fn csv_quotes_fields_with_separators() {
        let rows = RowSet { columns: cols(&["v"]), rows: vec![vec![Cell::Text("a,b".into())]] };
        assert_eq!(format_csv(&rows), "v\n\"a,b\"");
        // The quoting itself:
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("plain"), "plain");
    }

    #[test]
    fn json_renders_typed_values() {
        let out = format_json(&sample());
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v[0]["id"], serde_json::json!(1));
        assert_eq!(v[0]["name"], serde_json::json!("Ada"));
        assert_eq!(v[1]["name"], serde_json::Value::Null);
    }

    #[test]
    fn empty_rowset_formats_to_nothing() {
        assert_eq!(format_rows(&RowSet::empty(), Format::Table), "");
        assert_eq!(format_rows(&RowSet::empty(), Format::Json), "");
    }

    #[test]
    fn template_interpolates_named_columns() {
        let columns = cols(&["id", "name"]);
        let row = vec![Cell::Integer(7), Cell::Text("Ada".into())];
        assert_eq!(
            interpolate_template("user:${id}:${name}", &columns, &row).unwrap(),
            "user:7:Ada"
        );
        // $$ is a literal dollar.
        assert_eq!(interpolate_template("$$${id}", &columns, &row).unwrap(), "$7");
    }

    #[test]
    fn template_rejects_unknown_column_and_unterminated() {
        let columns = cols(&["id"]);
        let row = vec![Cell::Integer(1)];
        assert!(interpolate_template("x:${missing}", &columns, &row).is_err());
        assert!(interpolate_template("x:${id", &columns, &row).is_err());
    }

    #[test]
    fn whole_row_json_for_kv_put() {
        let columns = cols(&["id", "name"]);
        let row = vec![Cell::Integer(1), Cell::Text("Ada".into())];
        let json = row_to_json(&columns, &row);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["id"], serde_json::json!(1));
        assert_eq!(v["name"], serde_json::json!("Ada"));
    }

    #[test]
    fn safe_identifier_rules() {
        assert!(is_safe_identifier("wp_posts"));
        assert!(is_safe_identifier("Table1"));
        assert!(!is_safe_identifier(""));
        assert!(!is_safe_identifier("a b"));
        assert!(!is_safe_identifier("a;DROP"));
        assert!(!is_safe_identifier("a'"));
    }

    #[test]
    fn server_up_dialect_error_is_actionable() {
        let msg = server_up_dialect_error(Dialect::Sqlite, "127.0.0.1:3306");
        assert!(msg.contains("--dialect mysql"));
        assert!(msg.contains("stop the server"));
        assert!(msg.contains("sqlite"));
    }

    #[test]
    fn splitter_respects_quotes_and_comments() {
        assert_eq!(split_statements("SELECT 1; SELECT 2"), vec!["SELECT 1", "SELECT 2"]);
        // A ';' inside a string literal is not a separator.
        assert_eq!(
            split_statements("SELECT ';' AS a; SELECT 2"),
            vec!["SELECT ';' AS a", "SELECT 2"]
        );
        // A ';' inside a block comment is not a separator.
        assert_eq!(
            split_statements("SELECT 1 /* ; */ ; SELECT 2"),
            vec!["SELECT 1 /* ; */", "SELECT 2"]
        );
        assert_eq!(split_statements("  ;  ;SELECT 1;"), vec!["SELECT 1"]);
        assert_eq!(split_statements(""), Vec::<String>::new());
    }

    #[test]
    fn kv_frame_rendering() {
        assert_eq!(render_kv_frame(&Frame::Simple("OK".into())), "OK");
        assert_eq!(render_kv_frame(&Frame::Integer(5)), "(integer) 5");
        assert_eq!(render_kv_frame(&Frame::Null), "(nil)");
        assert_eq!(render_kv_frame(&Frame::bulk(b"hi".to_vec())), "hi");
    }

    // ── /kv output pipe ───────────────────────────────────────────────────

    fn widget_frame() -> Frame {
        Frame::bulk(
            br#"{"id":42,"slug":"blue-hat","price":1900,"updated_at":"2026-10-30 11:02:04"}"#
                .to_vec(),
        )
    }

    #[test]
    fn frame_to_json_normalizes_each_variant() {
        assert_eq!(frame_to_json(&Frame::Simple("OK".into())), serde_json::json!("OK"));
        assert_eq!(frame_to_json(&Frame::Integer(1900)), serde_json::json!(1900));
        assert_eq!(frame_to_json(&Frame::Null), Value::Null);
        // A JSON bulk parses to its structured value.
        assert_eq!(frame_to_json(&widget_frame())["price"], serde_json::json!(1900));
        // A non-JSON bulk becomes a JSON string of its UTF-8 text.
        assert_eq!(frame_to_json(&Frame::bulk(b"blue-hat".to_vec())), serde_json::json!("blue-hat"));
        // A nested array converts recursively (non-JSON items → strings).
        let arr = Frame::Array(vec![Frame::Integer(1), Frame::bulk(b"two".to_vec()), Frame::Null]);
        assert_eq!(frame_to_json(&arr), serde_json::json!([1, "two", null]));
    }

    #[test]
    fn frame_to_json_non_utf8_bulk_is_hex() {
        assert_eq!(frame_to_json(&Frame::bulk(vec![0xff, 0x00])), serde_json::json!("ff00"));
    }

    #[test]
    fn pipe_parsing_grammar() {
        assert_eq!(parse_kv_pipe("json").unwrap(), KvPipe { format: Some(KvFormat::Json), path: None });
        assert_eq!(parse_kv_pipe("yaml").unwrap().format, Some(KvFormat::Yaml));
        assert_eq!(parse_kv_pipe("yml").unwrap().format, Some(KvFormat::Yaml));
        assert_eq!(
            parse_kv_pipe(".price").unwrap(),
            KvPipe { format: None, path: Some(".price".into()) }
        );
        assert_eq!(
            parse_kv_pipe("json .price").unwrap(),
            KvPipe { format: Some(KvFormat::Json), path: Some(".price".into()) }
        );
        assert_eq!(
            parse_kv_pipe("table .slug,.price").unwrap(),
            KvPipe { format: Some(KvFormat::Table), path: Some(".slug,.price".into()) }
        );
    }

    #[test]
    fn pipe_parsing_errors() {
        // Empty pipe: the spec's exact message.
        assert_eq!(
            parse_kv_pipe("").unwrap_err().to_string(),
            "pipe needs a format or a path, e.g. | json .price"
        );
        assert_eq!(parse_kv_pipe("   ").unwrap_err().to_string(), "pipe needs a format or a path, e.g. | json .price");
        // Unknown leading bareword format.
        assert!(parse_kv_pipe("xml").unwrap_err().to_string().contains("unknown pipe format"));
        // Extra tokens.
        assert!(parse_kv_pipe("json .a .b").unwrap_err().to_string().contains("at most one"));
    }

    #[test]
    fn piped_raw_without_path_is_classic_rendering() {
        let reply = render_kv_piped(&widget_frame(), &parse_kv_pipe("raw").unwrap()).unwrap();
        assert_eq!(reply.text, render_kv_frame(&widget_frame()));
        assert!(reply.note.is_none());
    }

    #[test]
    fn piped_json_is_pretty() {
        let reply = render_kv_piped(&widget_frame(), &parse_kv_pipe("json").unwrap()).unwrap();
        assert!(reply.text.starts_with("{\n  \"id\": 42"), "{}", reply.text);
        assert!(reply.note.is_none());
    }

    #[test]
    fn piped_scalar_path_is_raw_bare() {
        let reply = render_kv_piped(&widget_frame(), &parse_kv_pipe(".price").unwrap()).unwrap();
        assert_eq!(reply.text, "1900");
        let reply = render_kv_piped(&widget_frame(), &parse_kv_pipe("json .price").unwrap()).unwrap();
        assert_eq!(reply.text, "1900");
    }

    #[test]
    fn piped_comma_select_auto_is_pretty_object() {
        let reply = render_kv_piped(&widget_frame(), &parse_kv_pipe(".slug,.price").unwrap()).unwrap();
        let v: Value = serde_json::from_str(&reply.text).unwrap();
        assert_eq!(v, serde_json::json!({ "slug": "blue-hat", "price": 1900 }));
    }

    #[test]
    fn piped_null_reply() {
        let reply = render_kv_piped(&Frame::Null, &parse_kv_pipe("json").unwrap()).unwrap();
        assert_eq!(reply.text, "null");
    }

    #[test]
    fn piped_fan_out_prints_a_line_per_element() {
        let list = Frame::Array(vec![
            Frame::bulk(b"a".to_vec()),
            Frame::bulk(b"b".to_vec()),
            Frame::bulk(b"c".to_vec()),
        ]);
        // The whole array has no field, so fan out with the identity-ish `.[]`.
        let reply = render_kv_piped(&list, &parse_kv_pipe(".[]").unwrap()).unwrap();
        assert_eq!(reply.text, "a\nb\nc");
    }

    #[test]
    fn piped_error_frame_prints_as_today() {
        let reply =
            render_kv_piped(&Frame::Error("ERR nope".into()), &parse_kv_pipe("json").unwrap())
                .unwrap();
        assert_eq!(reply.text, "(error) ERR nope");
        assert!(reply.note.is_none());
    }

    #[test]
    fn piped_non_json_bulk_fails_soft_with_note() {
        let frame = Frame::bulk(b"blue-hat".to_vec());
        for pipe in ["json", "yaml", "table", ".price"] {
            let reply = render_kv_piped(&frame, &parse_kv_pipe(pipe).unwrap()).unwrap();
            assert_eq!(reply.text, "blue-hat", "pipe {pipe}");
            assert_eq!(reply.note.as_deref(), Some("value is not JSON; printed raw"), "pipe {pipe}");
        }
    }

    #[test]
    fn piped_yaml_renders_a_block() {
        let reply = render_kv_piped(&widget_frame(), &parse_kv_pipe("yaml").unwrap()).unwrap();
        assert!(reply.text.contains("slug: blue-hat"), "{}", reply.text);
        assert!(reply.text.contains("price: 1900"), "{}", reply.text);
        // No trailing blank line.
        assert!(!reply.text.ends_with('\n'));
    }

    #[test]
    fn piped_table_object_is_key_value() {
        let reply = render_kv_piped(&widget_frame(), &parse_kv_pipe("table").unwrap()).unwrap();
        let lines: Vec<&str> = reply.text.lines().collect();
        // Two columns, aligned, with a `-+-` separator rule and a row count.
        assert!(lines[0].starts_with("key"), "{}", lines[0]);
        assert!(lines[0].contains("| value"), "{}", lines[0]);
        assert!(lines[1].starts_with("--") && lines[1].contains("-+-"), "{}", lines[1]);
        assert!(lines.iter().any(|l| l.starts_with("slug") && l.contains("| blue-hat")));
        assert!(reply.text.ends_with("(4 rows)"));
    }

    #[test]
    fn table_value_array_of_objects_is_multi_column() {
        let v = serde_json::json!([
            { "id": 1, "name": "Ada" },
            { "id": 2, "name": "Alan", "extra": true },
        ]);
        let out = table_value(&v);
        let lines: Vec<&str> = out.lines().collect();
        // Columns are the union of keys in first-seen order.
        assert_eq!(lines[0], "id | name | extra");
        assert!(out.contains("1  | Ada  |"));
        assert!(out.ends_with("(2 rows)"));
    }

    #[test]
    fn table_value_nested_cell_is_compact_json() {
        let v = serde_json::json!({ "tags": ["a", "b"] });
        let out = table_value(&v);
        assert!(out.contains(r#"["a","b"]"#), "{out}");
    }

    #[test]
    fn raw_and_auto_value_rendering() {
        assert_eq!(raw_value(&serde_json::json!("hi")), "hi");
        assert_eq!(raw_value(&serde_json::json!(5)), "5");
        assert_eq!(raw_value(&serde_json::json!(true)), "true");
        assert_eq!(raw_value(&serde_json::json!({ "a": 1 })), r#"{"a":1}"#);
        // auto: a scalar bare, an object pretty.
        assert_eq!(auto_value(&serde_json::json!("hi")), "hi");
        assert!(auto_value(&serde_json::json!({ "a": 1 })).contains("\n  \"a\": 1"));
    }
}
