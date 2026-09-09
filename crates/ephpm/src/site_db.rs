//! `ephpm php --site <key>` database binding (issue #471).
//!
//! Makes the in-process `ephpm_db_*` bridge work from the standalone PHP CLI so
//! `wp`, `artisan`, migrations, and seeders — anything built on the `db-*`
//! Composer packages — can reach a virtual host's Turso database without a
//! running HTTP server in the loop for every command.
//!
//! # The strategy picker (A + B)
//!
//! Exactly which backend the bridge gets depends on config **and** whether an
//! ePHPm server is already running, because Turso is an embedded engine — two
//! processes must never open the same database file at once, and a per-site
//! *clustered* deployment routes writes to a site's HRW owner that a standalone
//! CLI cannot reach:
//!
//! 1. **Server reachable** (its MySQL wire listener answers on
//!    `[db.sqlite.proxy] mysql_listen`) → [`RemoteMysqlBackend`]: connect to the
//!    listener authenticating **as the site** (`DB_USER = <site key>`,
//!    `DB_PASSWORD = HMAC-SHA256([kv] secret, site key)` — the same derivation
//!    the server verifies) and forward **raw** SQL. The server does per-site
//!    resolution, clustered owner-forwarding, screening, and query-stats, so
//!    this is correct in per-site *clustered* mode and while the server is
//!    live-serving the site. Requires `[kv] secret` (fails closed if unset).
//! 2. **Server not running, single-node/per-site-single** → [direct-open the
//!    site's own database file](open_direct) with `litewire::Turso`, for
//!    offline seeding.
//! 3. **Server not running, per-site-clustered** → **refuse**: a standalone CLI
//!    can neither reach the site's owner nor safely open the file locally
//!    (writes to a replica never replicate).
//!
//! In every case the request's SQL is screened CLI-side by
//! [`ephpm_php::db_bridge::screen_sql`] before it reaches a backend (the bridge
//! does this for the direct-open path; the remote path forwards to a server
//! that screens again).

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use ephpm_config::Config;
use ephpm_php::db_bridge::RemoteBackend;
use ephpm_server::EmbeddedSqliteMode;
use litewire::backend::{Column, ResultSet, Value};
use litewire::{SessionError, SessionOk, SessionResult};
use mysql_async::prelude::Queryable;

/// How long to wait for the server's MySQL listener to answer before deciding
/// it is not running. Loopback, so a live listener answers in well under this;
/// a closed port refuses immediately (the timeout only bounds a firewalled or
/// wedged listener).
const REACHABILITY_TIMEOUT: Duration = Duration::from_millis(400);

/// SQLSTATE reported for a client-side (connection/driver) failure, matching
/// what a MySQL client library reports for its own errors.
const CLIENT_SQLSTATE: [u8; 5] = *b"HY000";

/// MySQL client error code (`CR_SERVER_LOST`) used when the failure is the
/// connection to the server, not a SQL error the server returned. Sits in the
/// client range (2000-2999), which a server never emits, so it can never
/// collide with a real SQL errno.
const CR_SERVER_LOST: u16 = 2013;

/// The backend-binding strategy chosen for a `--site` invocation. Produced by
/// the pure [`choose_strategy`] and acted on by [`bind_site_db`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Strategy {
    /// A server is running: connect to its MySQL listener at `addr` as `user`
    /// (the site key) with `password`, and forward raw SQL.
    Wire { addr: SocketAddr, user: String, password: String },
    /// No server: open the site's own database file at `path` directly.
    DirectOpen { path: String },
}

/// Decide how to bind `site_key`'s database under `config`, given the result of
/// probing the server's wire listener (`reachable` is `Some(addr)` iff a server
/// answered). Pure except for the filesystem check inside
/// [`resolve_sandbox_site`](ephpm_server::router::resolve_sandbox_site) — no
/// sockets, no globals — so the whole strategy picker is unit-testable.
///
/// # Errors
///
/// Fails closed when the site key is invalid, the config is not multi-tenant
/// per-site, the wire path is needed but `[kv] secret` is unset, per-site
/// clustered mode is offline, or `[db.sqlite] dir` is missing for a direct-open.
fn choose_strategy(
    config: &Config,
    site_key: &str,
    reachable: Option<SocketAddr>,
) -> anyhow::Result<Strategy> {
    // Fail-closed validation, identical to a request's derivation (#290/#291):
    // invalid key / missing sites_dir / missing sites_dir/<key> all error here
    // rather than falling back to a wider database.
    let site = ephpm_server::router::resolve_sandbox_site(config, site_key)
        .context("resolving --site against the configuration")?;
    let key = site.key;

    let sqlite = config.db.sqlite.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "`ephpm php --site` requires an embedded database ([db.sqlite]); this config has none"
        )
    })?;

    let mode = ephpm_server::embedded_sqlite_mode(config);
    match mode {
        EmbeddedSqliteMode::PerSite | EmbeddedSqliteMode::PerSiteClustered => {}
        EmbeddedSqliteMode::None => bail!(
            "`ephpm php --site` requires an embedded database ([db.sqlite]); this config has none"
        ),
        EmbeddedSqliteMode::SingleNode | EmbeddedSqliteMode::Clustered => bail!(
            "`ephpm php --site {key}` requires multi-tenant per-site mode ([server] sites_dir with \
             [db.sqlite]); this config serves one shared database, which has no per-site identity \
             to bind"
        ),
    }

    let listen = &sqlite.proxy.mysql_listen;
    if let Some(addr) = reachable {
        // Strategy 1 — a server is live. Forward raw SQL over the wire,
        // authenticating as the site. This is the only correct path while the
        // server holds the database open, and the only safe path in per-site
        // clustered mode.
        let secret = config.kv.secret.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| {
            anyhow::anyhow!(
                "a server is running on {listen}, so `ephpm php --site {key}` must reach the site \
                 through it — but [kv] secret is not set, so the CLI cannot derive the per-site \
                 MySQL password. Set [kv] secret (the server must use the same value) to enable \
                 the CLI wire path."
            )
        })?;
        let password = ephpm_kv::auth::derive_site_password(secret, &key);
        return Ok(Strategy::Wire { addr, user: key, password });
    }

    // Server is not running. Direct-open is only safe when there is a single
    // local writer — never in per-site clustered mode, where the file on this
    // node may be a replica whose writes never propagate.
    if mode == EmbeddedSqliteMode::PerSiteClustered {
        bail!(
            "no server is running on {listen} and this is a per-site CLUSTERED deployment: a \
             standalone `ephpm php --site {key}` cannot safely reach the site's owner or open the \
             file locally (writes to a non-owner replica never replicate). Start `ephpm serve` on \
             this cluster and re-run, or use a single-node configuration for offline work."
        );
    }

    // Strategy 2 — offline direct-open of this site's own database file.
    let dir = sqlite.dir.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "per-site mode requires [db.sqlite] dir (the directory holding each vhost's <key>.db); \
             it is not set"
        )
    })?;
    let db_path = std::path::Path::new(dir).join(format!("{key}.db"));
    let path = db_path
        .to_str()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "database path for site {key:?} is not valid UTF-8: {}",
                db_path.display()
            )
        })?
        .to_string();
    Ok(Strategy::DirectOpen { path })
}

/// Bind the `ephpm_db_*` bridge to the right backend for `site_key` under
/// `config`, registering it with the PHP runtime on `handle`.
///
/// Probes the server's MySQL listener, then acts on the [`Strategy`] the pure
/// [`choose_strategy`] returns: connect a [`RemoteMysqlBackend`] (server up) or
/// direct-open the site's Turso file (server down, single-node/per-site-single).
/// Per-site-clustered-offline is refused inside [`choose_strategy`].
///
/// # Errors
///
/// Returns an error — and registers nothing — when [`choose_strategy`] fails
/// closed, the server refuses authentication, or the database cannot be opened.
pub fn bind_site_db(
    config: &Config,
    site_key: &str,
    handle: &tokio::runtime::Handle,
) -> anyhow::Result<()> {
    let listen =
        config.db.sqlite.as_ref().map(|s| s.proxy.mysql_listen.clone()).unwrap_or_default();
    let reachable = if listen.is_empty() { None } else { server_reachable(&listen) };

    match choose_strategy(config, site_key, reachable)? {
        Strategy::Wire { addr, user, password } => {
            let backend = handle
                .block_on(RemoteMysqlBackend::connect(addr, &user, &password))
                .with_context(|| {
                    format!(
                        "connecting to the running server's MySQL listener at {addr} as site \
                             {user:?} (check [kv] secret matches the server and that {user:?} is a \
                             configured vhost)"
                    )
                })?;
            ephpm_php::PhpRuntime::set_remote_db_backend(Arc::new(backend), handle.clone());
            tracing::info!(site = %user, %addr, "ephpm php --site: bound to the running server (wire)");
        }
        Strategy::DirectOpen { path } => {
            let backend = open_direct(handle, &path)
                .with_context(|| format!("opening site database at {path}"))?;
            ephpm_php::PhpRuntime::set_db_backend(backend, handle.clone());
            tracing::info!(path = %path, "ephpm php --site: opened local database (offline)");
        }
    }
    Ok(())
}

/// Open a Turso database file directly and return it as a shared backend for
/// the bridge (single-backend mode). The bridge wraps it in a translating
/// `Session` and screens every statement — the correct offline path.
///
/// # Errors
///
/// Returns an error if the database cannot be opened.
fn open_direct(
    handle: &tokio::runtime::Handle,
    db_path: &str,
) -> anyhow::Result<litewire::backend::SharedBackend> {
    let turso =
        handle.block_on(litewire::Turso::open(db_path)).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Arc::new(turso))
}

/// Probe the server's MySQL listener. Returns the resolved address when a TCP
/// connection is accepted within [`REACHABILITY_TIMEOUT`], else `None`.
fn server_reachable(listen: &str) -> Option<SocketAddr> {
    let addr = listen.to_socket_addrs().ok()?.next()?;
    match TcpStream::connect_timeout(&addr, REACHABILITY_TIMEOUT) {
        Ok(stream) => {
            drop(stream);
            Some(addr)
        }
        Err(_) => None,
    }
}

/// A [`RemoteBackend`] that forwards raw MySQL SQL to a running ePHPm server's
/// wire listener over a single, persistent `mysql_async` connection.
///
/// A single connection (not a pool) is deliberate: `BEGIN`/`INSERT`/`COMMIT`
/// must land on the *same* connection for the server to track transaction state
/// correctly, and the CLI runs one PHP request on one thread. The connection is
/// wrapped in a `tokio::sync::Mutex` only to satisfy `Send + Sync`; there is no
/// real contention.
pub struct RemoteMysqlBackend {
    conn: tokio::sync::Mutex<mysql_async::Conn>,
}

impl RemoteMysqlBackend {
    /// Connect to `addr` and authenticate as `user`/`password`
    /// (`mysql_native_password`, no TLS — loopback to a co-located server).
    ///
    /// # Errors
    ///
    /// Returns an error if the connection or authentication fails.
    pub async fn connect(addr: SocketAddr, user: &str, password: &str) -> anyhow::Result<Self> {
        let opts = mysql_async::OptsBuilder::default()
            .ip_or_hostname(addr.ip().to_string())
            .tcp_port(addr.port())
            .user(Some(user.to_string()))
            .pass(Some(password.to_string()));
        let conn = mysql_async::Conn::new(opts).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Self { conn: tokio::sync::Mutex::new(conn) })
    }
}

#[litewire::async_trait]
impl RemoteBackend for RemoteMysqlBackend {
    async fn run(&self, sql: &str, params: &[Value]) -> Result<SessionResult, SessionError> {
        let mut conn = self.conn.lock().await;

        // No params → text protocol (`query_iter`), which handles any statement
        // including DDL and transaction control verbatim. With params → a
        // prepared statement (`exec_iter`) on the same connection. The two
        // return distinct `QueryResult` protocol types, so each branch captures
        // its own columns (present even for a zero-row SELECT — issue #262) and
        // rows before the borrow ends.
        let (columns, rows_raw): (Vec<Column>, Vec<mysql_async::Row>) = if params.is_empty() {
            let result = conn.query_iter(sql).await.map_err(map_mysql_err)?;
            let columns = result.columns_ref().iter().map(to_lw_column).collect();
            let rows =
                result.collect_and_drop::<mysql_async::Row>().await.map_err(map_mysql_err)?;
            (columns, rows)
        } else {
            let bound: Vec<mysql_async::Value> = params.iter().map(to_mysql_value).collect();
            let result = conn
                .exec_iter(sql, mysql_async::Params::Positional(bound))
                .await
                .map_err(map_mysql_err)?;
            let columns = result.columns_ref().iter().map(to_lw_column).collect();
            let rows =
                result.collect_and_drop::<mysql_async::Row>().await.map_err(map_mysql_err)?;
            (columns, rows)
        };

        // A statement that produced a column set is a result set, exactly as
        // the server's own frontend classifies it — never inferred from the SQL
        // text (issue #263).
        if columns.is_empty() {
            let affected = conn.affected_rows();
            let last_insert_id = conn.last_insert_id().unwrap_or(0);
            Ok(SessionResult::Ok(SessionOk {
                affected_rows: affected,
                last_insert_id,
                // The server owns transaction state; the CLI never reads this.
                in_transaction: false,
                noop: false,
            }))
        } else {
            let rows: Vec<Vec<Value>> = rows_raw
                .into_iter()
                .map(|row| row.unwrap().iter().map(to_lw_value).collect())
                .collect();
            Ok(SessionResult::Rows(ResultSet { columns, rows }))
        }
    }
}

/// Map a `mysql_async` value into a litewire [`Value`].
fn to_lw_value(v: &mysql_async::Value) -> Value {
    match v {
        mysql_async::Value::NULL => Value::Null,
        mysql_async::Value::Int(i) => Value::Integer(*i),
        // A `u64` past `i64::MAX` cannot be an `Integer`; preserve it losslessly
        // as text rather than wrapping.
        mysql_async::Value::UInt(u) => {
            i64::try_from(*u).map_or_else(|_| Value::Text(u.to_string()), Value::Integer)
        }
        mysql_async::Value::Float(f) => Value::Float(f64::from(*f)),
        mysql_async::Value::Double(f) => Value::Float(*f),
        mysql_async::Value::Bytes(b) => match std::str::from_utf8(b) {
            Ok(s) => Value::Text(s.to_string()),
            Err(_) => Value::Blob(b.clone()),
        },
        // Turso surfaces DATE/TIME columns as text through the server, so these
        // are rarely produced; render them to their canonical string form for
        // the uncommon case a binary temporal value comes back.
        mysql_async::Value::Date(year, month, day, hour, minute, second, micros) => Value::Text(
            format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{micros:06}"),
        ),
        mysql_async::Value::Time(negative, days, hours, minutes, seconds, micros) => {
            let sign = if *negative { "-" } else { "" };
            let total_hours = u32::from(*hours) + 24 * *days;
            Value::Text(format!("{sign}{total_hours:03}:{minutes:02}:{seconds:02}.{micros:06}"))
        }
    }
}

/// Map a litewire [`Value`] into a `mysql_async` bind value.
fn to_mysql_value(v: &Value) -> mysql_async::Value {
    match v {
        Value::Null => mysql_async::Value::NULL,
        Value::Integer(i) => mysql_async::Value::Int(*i),
        Value::Float(f) => mysql_async::Value::Double(*f),
        Value::Text(s) => mysql_async::Value::Bytes(s.clone().into_bytes()),
        Value::Blob(b) => mysql_async::Value::Bytes(b.clone()),
    }
}

/// Map a `mysql_async` column to a litewire [`Column`]. `decltype` is left
/// `None`: this path mirrors stock `pdo_mysql`, where the server has already
/// mapped the SQLite storage class to a MySQL wire type; the column *name* and
/// the has-rowset signal are what the `db-*` adapters rely on.
fn to_lw_column(c: &mysql_async::Column) -> Column {
    Column { name: c.name_str().into_owned(), decltype: None }
}

/// Map a `mysql_async` error into a [`SessionError`], preserving the server's
/// real MySQL error code and SQLSTATE when the failure is a server error, and
/// reporting a client-side connection error ([`CR_SERVER_LOST`]) otherwise.
fn map_mysql_err(e: mysql_async::Error) -> SessionError {
    if let mysql_async::Error::Server(server) = &e {
        let mut sqlstate = CLIENT_SQLSTATE;
        let bytes = server.state.as_bytes();
        if bytes.len() == 5 {
            sqlstate.copy_from_slice(bytes);
        }
        return SessionError::Db { code: server.code, sqlstate, message: server.message.clone() };
    }
    SessionError::Db { code: CR_SERVER_LOST, sqlstate: CLIENT_SQLSTATE, message: e.to_string() }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;

    /// Absolute path as a forward-slash string safe to embed in TOML on any OS.
    fn toml_path(p: &Path) -> String {
        p.to_string_lossy().replace('\\', "/")
    }

    /// Build a config from an inline TOML body written to a tempfile, plus a
    /// `sites_dir` containing a single vhost directory `shop` and an empty
    /// `db` dir. Returns `(config, tempdir_guard)`.
    fn cfg(extra: &str) -> (Config, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sites = tmp.path().join("sites");
        fs::create_dir_all(sites.join("shop")).expect("vhost dir");
        let dbdir = tmp.path().join("db");
        fs::create_dir_all(&dbdir).expect("db dir");

        let body = format!(
            "[server]\n\
             document_root = \"{docroot}\"\n\
             sites_dir = \"{sites}\"\n\
             [db.sqlite]\n\
             dir = \"{dbdir}\"\n\
             [db.sqlite.proxy]\n\
             mysql_listen = \"127.0.0.1:3306\"\n\
             {extra}",
            docroot = toml_path(tmp.path()),
            sites = toml_path(&sites),
            dbdir = toml_path(&dbdir),
        );
        let path = tmp.path().join("ephpm.toml");
        fs::write(&path, body).expect("write config");
        let config = Config::load(&path).expect("load config");
        (config, tmp)
    }

    fn a_reachable_addr() -> SocketAddr {
        "127.0.0.1:3306".parse().unwrap()
    }

    fn expected_db_path(config: &Config) -> String {
        // Mirror the production join exactly (OS separator), not a normalized
        // form — `choose_strategy` hands the path straight to `Turso::open`.
        let dir = config.db.sqlite.as_ref().unwrap().dir.clone().unwrap();
        Path::new(&dir).join("shop.db").to_string_lossy().into_owned()
    }

    // ── Strategy selection ──────────────────────────────────────────────────

    #[test]
    fn per_site_offline_direct_opens_the_sites_own_file() {
        let (config, _g) = cfg("[kv]\nsecret = \"s3cr3t\"\n");
        let strategy = choose_strategy(&config, "shop", None).expect("strategy");
        assert_eq!(strategy, Strategy::DirectOpen { path: expected_db_path(&config) });
    }

    #[test]
    fn per_site_online_forwards_over_the_wire_with_the_derived_password() {
        let (config, _g) = cfg("[kv]\nsecret = \"s3cr3t\"\n");
        let addr = a_reachable_addr();
        let strategy = choose_strategy(&config, "shop", Some(addr)).expect("strategy");
        assert_eq!(
            strategy,
            Strategy::Wire {
                addr,
                user: "shop".to_string(),
                // Exactly what the server derives (same secret, same key).
                password: ephpm_kv::auth::derive_site_password("s3cr3t", "shop"),
            }
        );
    }

    #[test]
    fn online_without_kv_secret_fails_closed() {
        // No [kv] secret at all.
        let (config, _g) = cfg("");
        let err = choose_strategy(&config, "shop", Some(a_reachable_addr()))
            .expect_err("must refuse the wire path without a secret");
        let msg = format!("{err:#}");
        assert!(msg.contains("[kv] secret"), "message should name the missing secret: {msg}");
    }

    #[test]
    fn per_site_clustered_offline_refuses() {
        let (config, _g) = cfg("[kv]\nsecret = \"s3cr3t\"\n\
             [cluster]\nenabled = true\n\
             [db.sqlite.replication]\nper_site = true\n");
        // Sanity: this really is per-site clustered.
        assert_eq!(
            ephpm_server::embedded_sqlite_mode(&config),
            EmbeddedSqliteMode::PerSiteClustered
        );
        let err = choose_strategy(&config, "shop", None)
            .expect_err("a standalone CLI cannot serve per-site clustered offline");
        assert!(format!("{err:#}").contains("CLUSTERED"));
    }

    #[test]
    fn per_site_clustered_online_forwards_over_the_wire() {
        let (config, _g) = cfg("[kv]\nsecret = \"s3cr3t\"\n\
             [cluster]\nenabled = true\n\
             [db.sqlite.replication]\nper_site = true\n");
        let addr = a_reachable_addr();
        let strategy = choose_strategy(&config, "shop", Some(addr)).expect("strategy");
        assert!(matches!(strategy, Strategy::Wire { .. }));
    }

    #[test]
    fn shared_clustered_database_has_no_per_site_identity() {
        // Clustered but NOT per_site → one shared database; `--site` is refused.
        let (config, _g) = cfg("[kv]\nsecret = \"s3cr3t\"\n[cluster]\nenabled = true\n");
        assert_eq!(ephpm_server::embedded_sqlite_mode(&config), EmbeddedSqliteMode::Clustered);
        let err = choose_strategy(&config, "shop", None)
            .expect_err("shared clustered db is not per-site");
        assert!(format!("{err:#}").contains("per-site"));
    }

    #[test]
    fn unknown_site_fails_closed() {
        let (config, _g) = cfg("[kv]\nsecret = \"s3cr3t\"\n");
        // No `ghost` directory under sites_dir.
        assert!(choose_strategy(&config, "ghost", None).is_err());
    }

    #[test]
    fn traversal_site_key_fails_closed() {
        let (config, _g) = cfg("[kv]\nsecret = \"s3cr3t\"\n");
        assert!(choose_strategy(&config, "../etc", None).is_err());
        assert!(choose_strategy(&config, "a/b", None).is_err());
    }

    // ── Value / param mapping ───────────────────────────────────────────────

    #[test]
    fn mysql_values_map_to_litewire_values() {
        assert_eq!(to_lw_value(&mysql_async::Value::NULL), Value::Null);
        assert_eq!(to_lw_value(&mysql_async::Value::Int(-7)), Value::Integer(-7));
        assert_eq!(to_lw_value(&mysql_async::Value::Double(1.5)), Value::Float(1.5));
        // UInt within i64 range folds to Integer …
        assert_eq!(to_lw_value(&mysql_async::Value::UInt(42)), Value::Integer(42));
        // … but a value past i64::MAX is preserved losslessly as text.
        assert_eq!(
            to_lw_value(&mysql_async::Value::UInt(u64::MAX)),
            Value::Text(u64::MAX.to_string())
        );
        // UTF-8 bytes → Text, arbitrary bytes → Blob.
        assert_eq!(
            to_lw_value(&mysql_async::Value::Bytes(b"hi".to_vec())),
            Value::Text("hi".into())
        );
        assert_eq!(
            to_lw_value(&mysql_async::Value::Bytes(vec![0xff, 0xfe])),
            Value::Blob(vec![0xff, 0xfe])
        );
    }

    #[test]
    fn litewire_params_map_to_mysql_values() {
        assert_eq!(to_mysql_value(&Value::Null), mysql_async::Value::NULL);
        assert_eq!(to_mysql_value(&Value::Integer(9)), mysql_async::Value::Int(9));
        assert_eq!(to_mysql_value(&Value::Float(2.5)), mysql_async::Value::Double(2.5));
        assert_eq!(
            to_mysql_value(&Value::Text("x".into())),
            mysql_async::Value::Bytes(b"x".to_vec())
        );
        assert_eq!(to_mysql_value(&Value::Blob(vec![1, 2])), mysql_async::Value::Bytes(vec![1, 2]));
    }

    // ── Live round-trip against a real litewire MySQL frontend ──────────────

    /// End-to-end proof the Remote path forwards raw SQL correctly, including a
    /// multi-statement transaction and a parameterised insert, and that a server
    /// error surfaces with its MySQL code. Spins an in-process litewire MySQL
    /// frontend over an in-memory Turso and points a [`RemoteMysqlBackend`] at
    /// it — the same shape as the CLI reaching a running server.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_backend_transaction_and_error_roundtrip() {
        let turso = litewire::Turso::memory().await.expect("memory turso");

        // Grab a free port, then hand it to litewire.
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe bind");
        let addr = probe.local_addr().expect("local addr");
        drop(probe);
        let listen = addr.to_string();
        tokio::spawn(async move {
            let _ = litewire::LiteWire::new(turso).mysql(&listen).serve().await;
        });

        // The listener needs a moment to bind; retry the connect briefly.
        let backend = {
            let mut attempt = 0;
            loop {
                match RemoteMysqlBackend::connect(addr, "shop", "pw").await {
                    Ok(b) => break b,
                    Err(_) if attempt < 50 => {
                        attempt += 1;
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    Err(e) => panic!("could not connect to the test litewire frontend: {e}"),
                }
            }
        };

        backend
            .run("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[])
            .await
            .expect("create table");

        // A multi-statement transaction: BEGIN / parameterised INSERT / COMMIT,
        // all on the one persistent connection.
        backend.run("BEGIN", &[]).await.expect("begin");
        match backend
            .run("INSERT INTO t (v) VALUES (?)", &[Value::Text("hello".into())])
            .await
            .expect("insert")
        {
            SessionResult::Ok(ok) => assert_eq!(ok.affected_rows, 1),
            SessionResult::Rows(_) => panic!("INSERT must not return a rowset"),
        }
        backend.run("COMMIT", &[]).await.expect("commit");

        // The committed row is visible on a fresh statement.
        match backend.run("SELECT v FROM t", &[]).await.expect("select") {
            SessionResult::Rows(rs) => {
                assert_eq!(rs.rows.len(), 1, "the committed row must be visible");
                assert_eq!(rs.rows[0][0], Value::Text("hello".into()));
                assert_eq!(rs.columns.len(), 1);
                assert_eq!(rs.columns[0].name, "v");
            }
            SessionResult::Ok(_) => panic!("SELECT must return a rowset"),
        }

        // A server error surfaces with a nonzero MySQL code (not the client
        // CR_SERVER_LOST), exercising `map_mysql_err`'s server branch.
        let err = backend.run("SELECT * FROM does_not_exist", &[]).await.expect_err("must error");
        assert_ne!(err.code(), 0, "a SQL error must carry the server's MySQL errno");
        assert_ne!(err.code(), CR_SERVER_LOST, "a SQL error is not a connection error");
    }
}
