//! `ephpm db restore` and `ephpm db backup list` — the restore half of the
//! scheduled embedded-database backups (`[db.sqlite.backup]`, #686 Tier 0).
//!
//! A backup object is a gzip of a standard SQLite main file written by
//! `VACUUM INTO` (`ephpm_server::db_backup`). Restoring one is a **file
//! install**, not an SQL replay:
//!
//! 1. Resolve the target the way `db import` / `db query` do (`--site`
//!    validated fail-closed; single-node mode's one database is `default`).
//! 2. Pick the object: the newest under `<prefix>/<site>/` with a timestamp
//!    at or before `--at` (default: the newest), or a local `.db` /
//!    `.db.gz`. Print the choice and the target, and require `--yes` or a
//!    TTY confirmation before touching anything.
//! 3. **Offline only.** Refuse while a server holds the file: the MySQL
//!    listener (`[db.sqlite.proxy] mysql_listen`) and the HTTP listener
//!    (`[server] listen`) are both probed, so a bridge-only server with the
//!    wire listener off is still seen. A rename under an open Turso handle
//!    would be ignored on Linux and fail on Windows, so there is no safe
//!    live path with a physical artifact; a live restore (quiesce the
//!    site's handle, swap, reopen) is a follow-up.
//! 4. Download to `<target>.restore-tmp`, gunzip, open with the engine and
//!    run `PRAGMA integrity_check` — a file that does not verify is deleted
//!    and the command fails before the live database is touched.
//! 5. Rename over `<site>.db` and delete the stale `-wal`/`-shm`.

use std::io::IsTerminal as _;
use std::net::{SocketAddr, ToSocketAddrs as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context as _, bail};
use ephpm_config::Config;
use ephpm_server::db_backup::{self, BackupObject};
use ephpm_server::s3::{S3Client, S3Location};
use ephpm_server::{EmbeddedSqliteMode, embedded_sqlite_mode};

use crate::site_db;

/// `ephpm db restore`.
#[derive(Debug)]
pub struct RestoreRequest {
    /// The vhost key (`default` in single-node mode).
    pub site: String,
    /// The config file.
    pub config: PathBuf,
    /// `s3://bucket[/prefix]`, or a local `.db` / `.db.gz` file.
    pub from: String,
    /// Restore the newest object at or before this time.
    pub at: Option<String>,
    /// Skip the confirmation.
    pub yes: bool,
}

/// `ephpm db backup list`.
#[derive(Debug)]
pub struct ListRequest {
    /// The vhost key (`default` in single-node mode).
    pub site: String,
    /// The config file.
    pub config: PathBuf,
    /// `s3://bucket[/prefix]` to list instead of the configured location.
    pub from: Option<String>,
    /// JSON output.
    pub json: bool,
}

/// Where the live database for `site` is.
#[derive(Debug)]
struct Target {
    key: String,
    path: PathBuf,
}

/// What `--from` resolved to.
enum Source {
    Object { client: S3Client, object: BackupObject },
    Local { path: PathBuf, gzip: bool },
}

fn load_config(path: &Path) -> anyhow::Result<Config> {
    Config::load(&path.to_path_buf())
        .with_context(|| format!("could not load config {}", path.display()))
}

/// The site's database file under `config`, by mode. Fail-closed: an
/// unknown or malformed key, or a clustered mode, is an error.
fn resolve_target(config: &Config, site: &str) -> anyhow::Result<Target> {
    let sqlite = config.db.sqlite.as_ref().context(
        "`ephpm db restore` needs an embedded database ([db.sqlite]); this config has none",
    )?;
    match embedded_sqlite_mode(config) {
        EmbeddedSqliteMode::SingleNode => {
            if site != ephpm_server::wal_gate::DEFAULT_SITE_LABEL {
                bail!(
                    "this config serves one shared database (single-node mode), whose backups \
                     are stored under the site key `default`; pass --site default"
                );
            }
            Ok(Target { key: site.to_string(), path: PathBuf::from(&sqlite.path) })
        }
        EmbeddedSqliteMode::PerSite => {
            let resolved = ephpm_server::router::resolve_sandbox_site(config, site)
                .context("resolving --site against the configuration")?;
            let dir = sqlite.dir.as_deref().context(
                "per-site mode requires [db.sqlite] dir (the directory holding each vhost's \
                 <key>.db); it is not set",
            )?;
            let path = Path::new(dir).join(format!("{}.db", resolved.key));
            Ok(Target { key: resolved.key, path })
        }
        EmbeddedSqliteMode::Clustered | EmbeddedSqliteMode::PerSiteClustered => bail!(
            "this config is CLUSTERED: `ephpm db restore` installs a file on one node, which a \
             replica set would discard or diverge from. Clustered restore is not implemented; \
             restore on an unclustered node ([db.sqlite.replication] role = \"single\") and \
             re-seed the cluster from it."
        ),
        EmbeddedSqliteMode::None => {
            bail!(
                "`ephpm db restore` needs an embedded database ([db.sqlite]); this config has none"
            )
        }
    }
}

/// Resolve `from` to an object (newest at or before `at`) or a local file.
fn resolve_source(
    config: &Config,
    site: &str,
    from: &str,
    at: Option<&str>,
    rt: &tokio::runtime::Runtime,
) -> anyhow::Result<Source> {
    if !from.starts_with("s3://") {
        let path = PathBuf::from(from);
        anyhow::ensure!(path.is_file(), "{from}: no such file");
        anyhow::ensure!(at.is_none(), "--at only applies to an s3:// source");
        let gzip = path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("gz"));
        return Ok(Source::Local { path, gzip });
    }
    let (bucket, prefix) = S3Location::parse_url(from)?;
    let client = db_backup::object_client_for_bucket(config, bucket)?;
    let at = match at {
        Some(s) => Some(db_backup::parse_timestamp(s).with_context(|| {
            format!(
                "--at {s:?} is not a time: use YYYYMMDDTHHMMSSZ (as the object names), \
                 YYYY-MM-DDTHH:MM:SSZ, or YYYY-MM-DD"
            )
        })?),
        None => None,
    };
    let objects = rt
        .block_on(db_backup::list_site_backups(&client, &prefix, site))
        .with_context(|| format!("listing {from}/{site}/"))?;
    let object =
        db_backup::select_at_or_before(&objects, at).cloned().with_context(|| match at {
            Some(at) => format!(
                "no backup of site {site:?} at or before {} under {}/{site}/ ({} objects, the \
                 oldest is {})",
                db_backup::format_rfc3339(at),
                from.trim_end_matches('/'),
                objects.len(),
                objects
                    .first()
                    .map_or_else(|| "none".to_string(), |o| db_backup::format_rfc3339(o.timestamp)),
            ),
            None => {
                format!("no backup of site {site:?} under {}/{site}/", from.trim_end_matches('/'))
            }
        })?;
    Ok(Source::Object { client, object })
}

/// `Some(addr)` when something accepts a TCP connection at `listen`
/// (`0.0.0.0`/`::` mapped to loopback, since that is where the CLI runs).
fn listener_answers(listen: &str) -> Option<SocketAddr> {
    let listen = listen.trim();
    let mapped = match listen.rsplit_once(':') {
        Some((h, p)) if h == "0.0.0.0" || h.is_empty() => format!("127.0.0.1:{p}"),
        Some((h, p)) if h == "[::]" || h == "::" => format!("[::1]:{p}"),
        _ => listen.to_string(),
    };
    let addr = mapped.to_socket_addrs().ok()?.next()?;
    site_db::server_reachable(&addr.to_string())
}

/// Refuse while a server appears to hold the database.
fn ensure_offline(config: &Config) -> anyhow::Result<()> {
    let mut probes: Vec<(&str, String)> = vec![("[server] listen", config.server.listen.clone())];
    if let Some(sqlite) = config.db.sqlite.as_ref()
        && sqlite.proxy.mysql_wire_enabled
        && !sqlite.proxy.mysql_listen.trim().is_empty()
    {
        probes.push(("[db.sqlite.proxy] mysql_listen", sqlite.proxy.mysql_listen.clone()));
    }
    for (knob, listen) in probes {
        if let Some(addr) = listener_answers(&listen) {
            bail!(
                "a server is running ({knob} = {listen:?} answers on {addr}): `ephpm db \
                 restore` is offline-only — a database file cannot be replaced under an open \
                 engine handle. Stop the server first, then re-run."
            );
        }
    }
    Ok(())
}

fn confirm(prompt: &str, yes: bool) -> anyhow::Result<()> {
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!("refusing to restore without confirmation: pass --yes (stdin is not a terminal)");
    }
    eprint!("{prompt} Type `yes` to continue: ");
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).context("reading the confirmation")?;
    if answer.trim() != "yes" {
        bail!("aborted");
    }
    Ok(())
}

fn remove_quietly(path: &Path) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(file = %path.display(), "could not remove: {e}");
    }
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Run `ephpm db restore`. Blocking; call outside any async runtime.
///
/// # Errors
///
/// Config, resolution, download, verification or install failures. A
/// verification failure removes the downloaded file and leaves the live
/// database untouched.
pub fn run_restore(req: &RestoreRequest) -> anyhow::Result<ExitCode> {
    let config = load_config(&req.config)?;
    let target = resolve_target(&config, &req.site)?;
    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
    let source = resolve_source(&config, &target.key, &req.from, req.at.as_deref(), &rt)?;

    let description = match &source {
        Source::Object { client, object } => format!(
            "s3://{}/{} (taken {}, {} bytes compressed)",
            client.location().bucket,
            object.key,
            db_backup::format_rfc3339(object.timestamp),
            object.size
        ),
        Source::Local { path, .. } => path.display().to_string(),
    };
    let exists = target.path.is_file();
    println!("site:    {}", target.key);
    println!("source:  {description}");
    println!(
        "target:  {}{}",
        target.path.display(),
        if exists { " (will be REPLACED)" } else { " (does not exist yet)" }
    );
    confirm("This replaces the live database with the backup.", req.yes)?;

    ensure_offline(&config)?;
    if let Some(parent) = target.path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    // Stage next to the target so the final rename is on one filesystem.
    let tmp = sidecar(&target.path, ".restore-tmp");
    let tmp_gz = sidecar(&target.path, ".restore-tmp.gz");
    remove_quietly(&tmp);
    remove_quietly(&tmp_gz);
    let staged = stage(&source, &tmp, &tmp_gz, &rt);
    remove_quietly(&tmp_gz);
    if let Err(e) = staged {
        remove_quietly(&tmp);
        return Err(e);
    }

    // Verify before the live file is touched.
    if let Err(e) = rt.block_on(db_backup::verify_integrity(&tmp)) {
        remove_quietly(&tmp);
        eprintln!("error: the backup does not verify; the live database was not touched: {e:#}");
        return Ok(ExitCode::from(2));
    }

    // Install: rename over the target, then drop the stale sidecars (the
    // server is stopped and its shutdown truncated the log, so they are
    // empty or absent; a stale one must never be replayed onto the new
    // file).
    ensure_offline(&config)?;
    std::fs::rename(&tmp, &target.path)
        .with_context(|| format!("installing {} over {}", tmp.display(), target.path.display()))?;
    remove_quietly(&sidecar(&target.path, "-wal"));
    remove_quietly(&sidecar(&target.path, "-shm"));
    println!("restored {} from {description}", target.path.display());
    Ok(ExitCode::SUCCESS)
}

/// Materialise `source` as the plain database file `tmp` (via `tmp_gz` when
/// gzipped).
fn stage(
    source: &Source,
    tmp: &Path,
    tmp_gz: &Path,
    rt: &tokio::runtime::Runtime,
) -> anyhow::Result<()> {
    match source {
        Source::Object { client, object } => {
            let got = rt
                .block_on(client.get_to_file(&object.key, tmp_gz))
                .with_context(|| format!("downloading {}", object.key))?;
            anyhow::ensure!(
                got.is_some(),
                "{}: the object vanished during the restore",
                object.key
            );
            db_backup::gunzip_file(tmp_gz, tmp).context("decompressing the backup")?;
        }
        Source::Local { path, gzip: true } => {
            db_backup::gunzip_file(path, tmp).context("decompressing the backup")?;
        }
        Source::Local { path, gzip: false } => {
            std::fs::copy(path, tmp).with_context(|| format!("copying {}", path.display()))?;
        }
    }
    Ok(())
}

/// Run `ephpm db backup list`. Blocking.
///
/// # Errors
///
/// Config or listing failures.
pub fn run_list(req: &ListRequest) -> anyhow::Result<ExitCode> {
    let config = load_config(&req.config)?;
    let target = resolve_target(&config, &req.site)?;
    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
    let (client, prefix) = match &req.from {
        Some(from) => {
            let (bucket, prefix) = S3Location::parse_url(from)?;
            (db_backup::object_client_for_bucket(&config, bucket)?, prefix)
        }
        None => db_backup::object_client_from_config(&config)
            .context("pass --from s3://bucket/prefix, or enable [db.sqlite.backup]")?,
    };
    let location = format!(
        "s3://{}/{}/",
        client.location().bucket,
        db_backup::site_prefix(&prefix, &target.key)
    );
    let objects = rt
        .block_on(db_backup::list_site_backups(&client, &prefix, &target.key))
        .with_context(|| format!("listing {location}"))?;
    if req.json {
        let doc = serde_json::json!({
            "site": target.key,
            "location": location,
            "backups": objects.iter().map(|o| serde_json::json!({
                "key": o.key,
                "taken_at": db_backup::format_rfc3339(o.timestamp),
                "taken_at_unix": o.timestamp,
                "bytes": o.size,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&doc)?);
        return Ok(ExitCode::SUCCESS);
    }
    println!("site:     {}", target.key);
    println!("location: {location}");
    if objects.is_empty() {
        println!("(no backups)");
        return Ok(ExitCode::SUCCESS);
    }
    println!("{:<22} {:>14} key", "taken (UTC)", "bytes");
    for o in &objects {
        println!("{:<22} {:>14} {}", db_backup::format_rfc3339(o.timestamp), o.size, o.key);
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn toml_path(p: &Path) -> String {
        p.to_string_lossy().replace('\\', "/")
    }

    fn per_site_config() -> (Config, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let sites = tmp.path().join("sites");
        fs::create_dir_all(sites.join("shop")).unwrap();
        let dbdir = tmp.path().join("db");
        fs::create_dir_all(&dbdir).unwrap();
        let body = format!(
            "[server]\ndocument_root = \"{d}\"\nsites_dir = \"{s}\"\n[db.sqlite]\ndir = \"{db}\"\n",
            d = toml_path(tmp.path()),
            s = toml_path(&sites),
            db = toml_path(&dbdir),
        );
        let path = tmp.path().join("ephpm.toml");
        fs::write(&path, body).unwrap();
        (Config::load(&path).unwrap(), tmp)
    }

    #[test]
    fn per_site_target_is_the_sites_own_file_fail_closed() {
        let (config, tmp) = per_site_config();
        let target = resolve_target(&config, "shop").unwrap();
        assert_eq!(target.key, "shop");
        assert_eq!(target.path, tmp.path().join("db").join("shop.db"));
        assert!(resolve_target(&config, "ghost").is_err(), "unknown vhost");
        assert!(resolve_target(&config, "../x").is_err(), "traversal");
        assert!(resolve_target(&config, "default").is_err(), "not a vhost here");
    }

    #[test]
    fn single_node_target_is_default_only() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ephpm.toml");
        fs::write(&path, "[db.sqlite]\npath = \"/var/lib/ephpm/one.db\"\n").unwrap();
        let config = Config::load(&path).unwrap();
        let target = resolve_target(&config, "default").unwrap();
        assert_eq!(target.path, PathBuf::from("/var/lib/ephpm/one.db"));
        let err = resolve_target(&config, "shop").unwrap_err();
        assert!(format!("{err:#}").contains("default"), "{err:#}");
    }

    #[test]
    fn clustered_targets_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ephpm.toml");
        fs::write(&path, "[db.sqlite]\n[cluster]\nenabled = true\nsecret = \"s\"\n").unwrap();
        let config = Config::load(&path).unwrap();
        let err = resolve_target(&config, "default").unwrap_err();
        assert!(format!("{err:#}").contains("CLUSTERED"), "{err:#}");
    }

    /// A local source needs no bucket; a missing file or `--at` is refused
    /// up front.
    #[test]
    fn local_source_resolution() {
        let (config, tmp) = per_site_config();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let file = tmp.path().join("x.db.gz");
        fs::write(&file, b"not really gzip").unwrap();
        let from = file.to_string_lossy().into_owned();
        match resolve_source(&config, "shop", &from, None, &rt).unwrap() {
            Source::Local { path, gzip } => {
                assert_eq!(path, file);
                assert!(gzip);
            }
            Source::Object { .. } => panic!("local"),
        }
        assert!(resolve_source(&config, "shop", &from, Some("2026-01-01"), &rt).is_err());
        let missing = tmp.path().join("nope.db").to_string_lossy().into_owned();
        assert!(resolve_source(&config, "shop", &missing, None, &rt).is_err());
    }

    /// End to end on a local artifact: a real `VACUUM INTO` snapshot,
    /// gzipped, is installed over the site's file (stale sidecars removed),
    /// and a corrupt artifact never touches the live file.
    #[test]
    fn restore_installs_a_verified_artifact_and_refuses_a_corrupt_one() {
        let (_config, tmp) = per_site_config();
        let live = tmp.path().join("db").join("shop.db");
        let rt = tokio::runtime::Runtime::new().unwrap();

        // Build a source database and snapshot it.
        let source = tmp.path().join("source.db");
        rt.block_on(async {
            let db = litewire::Turso::open(source.to_str().unwrap()).await.unwrap();
            let conn = db.raw_connection().unwrap();
            conn.execute("CREATE TABLE t (v TEXT)", ()).await.unwrap();
            conn.execute("INSERT INTO t VALUES ('restored row')", ()).await.unwrap();
        });
        let snap = tmp.path().join("snap.db");
        rt.block_on(db_backup::snapshot_into(&source, &snap)).unwrap();
        let gz = tmp.path().join("snap.db.gz");
        db_backup::gzip_file(&snap, &gz).unwrap();

        // The "live" file and stale sidecars that must not survive.
        fs::write(&live, b"old").unwrap();
        fs::write(sidecar(&live, "-wal"), b"stale").unwrap();
        fs::write(sidecar(&live, "-shm"), b"stale").unwrap();

        let req = RestoreRequest {
            site: "shop".into(),
            config: tmp.path().join("ephpm.toml"),
            from: gz.to_string_lossy().into_owned(),
            at: None,
            yes: true,
        };
        // The probe of `[server] listen` (default 0.0.0.0:8080) could see an
        // unrelated local service; pin the config's listeners to ports that
        // are certainly closed.
        let cfg = fs::read_to_string(&req.config).unwrap();
        let free = || {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        fs::write(
            &req.config,
            cfg.replace("[server]\n", &format!("[server]\nlisten = \"127.0.0.1:{}\"\n", free()))
                + &format!("[db.sqlite.proxy]\nmysql_listen = \"127.0.0.1:{}\"\n", free()),
        )
        .unwrap();
        let code = run_restore(&req).unwrap();
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(!sidecar(&live, "-wal").exists());
        assert!(!sidecar(&live, "-shm").exists());
        assert!(!sidecar(&live, ".restore-tmp").exists());
        assert!(!sidecar(&live, ".restore-tmp.gz").exists());
        rt.block_on(async {
            let db = litewire::Turso::open(live.to_str().unwrap()).await.unwrap();
            let conn = db.raw_connection().unwrap();
            let mut rows = conn.query("SELECT v FROM t", ()).await.unwrap();
            let row = rows.next().await.unwrap().expect("the restored row");
            assert_eq!(format!("{:?}", row.get_value(0).unwrap()), "Text(\"restored row\")");
        });

        // A corrupt artifact: gzip of junk. Verification fails, exit 2, the
        // live file is untouched and no temp file is left.
        let junk = tmp.path().join("junk.db");
        fs::write(&junk, vec![0x42; 8192]).unwrap();
        let junk_gz = tmp.path().join("junk.db.gz");
        db_backup::gzip_file(&junk, &junk_gz).unwrap();
        let before = fs::read(&live).unwrap();
        let req = RestoreRequest { from: junk_gz.to_string_lossy().into_owned(), ..req };
        let code = run_restore(&req).unwrap();
        assert_eq!(code, ExitCode::from(2));
        assert_eq!(fs::read(&live).unwrap(), before, "the live database was not touched");
        assert!(!sidecar(&live, ".restore-tmp").exists());
    }

    /// Without `--yes` and without a terminal, the restore refuses before
    /// touching anything.
    #[test]
    fn non_tty_without_yes_refuses() {
        if std::io::stdin().is_terminal() {
            return; // interactive run: the prompt would block
        }
        let (_config, tmp) = per_site_config();
        let file = tmp.path().join("x.db");
        fs::write(&file, b"x").unwrap();
        let req = RestoreRequest {
            site: "shop".into(),
            config: tmp.path().join("ephpm.toml"),
            from: file.to_string_lossy().into_owned(),
            at: None,
            yes: false,
        };
        let err = run_restore(&req).unwrap_err();
        assert!(format!("{err:#}").contains("--yes"), "{err:#}");
        assert!(!tmp.path().join("db").join("shop.db").exists());
    }
}
