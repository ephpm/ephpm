//! `ephpm kv snapshot …` — warm-restart snapshot operations.
//!
//! Two kinds of subcommand:
//!
//! * **Offline**, on a file: `inspect` and `verify` read a snapshot directly.
//!   `inspect` needs no secret for the header; given `--config` (for
//!   `[kv] secret`) it also opens the index and prints per-site totals.
//!   `verify` authenticates and decodes every block.
//! * **Against a running server**: `save`, `load` and `status` call the
//!   server's `/_ephpm/kv/snapshot` endpoint, which is gated like
//!   `/_ephpm/requests` (`[server.diagnostics] allow`/`token`). The URL and
//!   token come from `--url`/`--token`, or from `--config` (`[server] listen`
//!   and `[server.diagnostics] token`).
//!
//! Exit status: `0` success; `1` usage, I/O, connection or server error;
//! `2` the snapshot is unusable (`verify` found bad blocks, or `inspect`
//! could not read the file as a snapshot).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context as _;
use clap::{Args, Subcommand};
use ephpm_config::Config;
use ephpm_kv::snapshot::{self, SnapshotError};

/// `ephpm kv snapshot <COMMAND>`.
#[derive(Subcommand, Debug)]
pub enum KvSnapshotCommand {
    /// Print a snapshot's header; with --config (for [kv] secret), its
    /// per-site totals too. Never decrypts a value. The source is a file
    /// path, or an s3://bucket/key object (then --config supplies the
    /// endpoint/region and the environment the credentials).
    Inspect {
        /// The snapshot file, or s3://bucket/key.
        source: String,
        /// Config file to read [kv] secret from, so the index can be opened.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Authenticate and decode every block of a snapshot (exit 2 on any bad
    /// block). Needs [kv] secret from --config. Source as for inspect.
    Verify {
        /// The snapshot file, or s3://bucket/key.
        source: String,
        /// Config file to read [kv] secret from.
        #[arg(long)]
        config: PathBuf,
    },
    /// List the snapshots at the configured location: the objects under
    /// [kv.snapshot] url, or the files under [kv.snapshot] path.
    List {
        /// Config file naming the location.
        #[arg(long)]
        config: PathBuf,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Delete one snapshot (by identity) at the configured location.
    Rm {
        /// The snapshot's identity (the file stem / object name without
        /// `.kvsnap`), as `list` shows it.
        identity: String,
        /// Config file naming the location.
        #[arg(long)]
        config: PathBuf,
    },
    /// Ask a running server to write its snapshot now.
    Save(ServerTarget),
    /// Ask a running server to load its snapshot into the live store now.
    Load(ServerTarget),
    /// Show a running server's snapshot status (file, last save, last load).
    Status(ServerTarget),
}

/// How to reach the running server's control endpoint.
#[derive(Args, Debug, Clone)]
pub struct ServerTarget {
    /// Base URL of the server, e.g. http://127.0.0.1:8080. Defaults to
    /// [server] listen from --config.
    #[arg(long)]
    url: Option<String>,
    /// Bearer token for [server.diagnostics] token. Defaults to the value in
    /// --config. Required on a multi-site server.
    #[arg(long)]
    token: Option<String>,
    /// Config file to take the URL and token from.
    #[arg(long)]
    config: Option<PathBuf>,
}

/// Run a `kv snapshot` subcommand. Blocking; call outside any async runtime.
///
/// # Errors
///
/// Usage, I/O and connection errors (exit 1). A snapshot that fails to
/// verify is reported on stderr with exit 2, not as an error.
pub fn run(cmd: KvSnapshotCommand) -> anyhow::Result<ExitCode> {
    match cmd {
        KvSnapshotCommand::Inspect { source, config, json } => {
            let local = materialize(&source, config.as_deref())?;
            inspect(&local.path, config.as_deref(), json)
        }
        KvSnapshotCommand::Verify { source, config } => {
            let local = materialize(&source, Some(&config))?;
            verify(&local.path, &config)
        }
        KvSnapshotCommand::List { config, json } => list(&config, json),
        KvSnapshotCommand::Rm { identity, config } => rm(&identity, &config),
        KvSnapshotCommand::Save(target) => call(&target, "POST", "/_ephpm/kv/snapshot"),
        KvSnapshotCommand::Load(target) => call(&target, "POST", "/_ephpm/kv/snapshot/load"),
        KvSnapshotCommand::Status(target) => call(&target, "GET", "/_ephpm/kv/snapshot"),
    }
}

/// Read `[kv] secret` from a config file.
fn secret_from_config(path: &Path) -> anyhow::Result<Vec<u8>> {
    let config = load_config(path)?;
    let secret = config
        .kv
        .secret
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .with_context(|| format!("{} sets no [kv] secret", path.display()))?;
    Ok(secret.as_bytes().to_vec())
}

fn load_config(path: &Path) -> anyhow::Result<Config> {
    Config::load(&path.to_path_buf())
        .with_context(|| format!("could not load config {}", path.display()))
}

/// A snapshot available on the local disk: the path given, or an
/// `s3://` object downloaded into a temporary directory that lives as long
/// as this value.
struct LocalSnapshot {
    path: PathBuf,
    _tmp: Option<tempfile::TempDir>,
}

/// Resolve `source` to a local file, downloading an `s3://bucket/key`
/// object with the endpoint/region from `--config` and credentials from the
/// environment.
fn materialize(source: &str, config: Option<&Path>) -> anyhow::Result<LocalSnapshot> {
    let Some(rest) = source.strip_prefix("s3://") else {
        return Ok(LocalSnapshot { path: PathBuf::from(source), _tmp: None });
    };
    let (bucket, key) = rest
        .split_once('/')
        .filter(|(b, k)| !b.is_empty() && !k.is_empty())
        .with_context(|| format!("expected s3://bucket/key, got {source:?}"))?;
    let config_path = config.context(
        "an s3:// source needs --config, for [kv.snapshot] endpoint/region (credentials come \
         from AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY)",
    )?;
    let cfg = load_config(config_path)?;
    let client = ephpm_server::kv_snapshot::object_client_for_bucket(&cfg, bucket.to_string())?;
    let tmp = tempfile::Builder::new().prefix("ephpm-kv-snapshot-").tempdir()?;
    let path = tmp.path().join("object.kvsnap");
    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
    let got = rt.block_on(client.get_to_file(key, &path))?;
    if got.is_none() {
        anyhow::bail!("{source}: no such object");
    }
    Ok(LocalSnapshot { path, _tmp: Some(tmp) })
}

/// One listed snapshot.
struct Listed {
    identity: String,
    bytes: u64,
    modified: String,
}

fn listed(config: &Config) -> anyhow::Result<(String, Vec<Listed>)> {
    let snap = &config.kv.snapshot;
    anyhow::ensure!(snap.enabled(), "[kv.snapshot] mode is \"off\": nothing to list");
    let suffix = format!(".{}", ephpm_server::kv_snapshot::SNAPSHOT_EXTENSION);
    if snap.is_object() {
        let (client, prefix) = ephpm_server::kv_snapshot::object_client_from_config(config)?;
        let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
        let dir = if prefix.is_empty() { String::new() } else { format!("{prefix}/") };
        let objects = rt.block_on(client.list(&dir))?;
        let mut out: Vec<Listed> = objects
            .into_iter()
            .filter_map(|o| {
                let name = o.key.strip_prefix(&dir)?;
                let identity = name.strip_suffix(&suffix)?;
                (!identity.contains('/')).then(|| Listed {
                    identity: identity.to_string(),
                    bytes: o.size,
                    modified: o.last_modified,
                })
            })
            .collect();
        out.sort_by(|a, b| a.identity.cmp(&b.identity));
        Ok((format!("s3://{}/{dir}", client.location().bucket), out))
    } else {
        let dir = PathBuf::from(snap.path.as_deref().context("[kv.snapshot] path is not set")?);
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("could not read {}", dir.display()))?
            .flatten()
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(identity) = name.strip_suffix(&suffix) else { continue };
            if identity.starts_with('.') {
                continue;
            }
            let meta = entry.metadata()?;
            let modified = meta
                .modified()
                .ok()
                .map(|t| format_unix_ms(ephpm_kv::snapshot::unix_ms_for(t)))
                .unwrap_or_default();
            out.push(Listed { identity: identity.to_string(), bytes: meta.len(), modified });
        }
        out.sort_by(|a, b| a.identity.cmp(&b.identity));
        Ok((dir.display().to_string(), out))
    }
}

fn list(config: &Path, json: bool) -> anyhow::Result<ExitCode> {
    let cfg = load_config(config)?;
    let (location, items) = listed(&cfg)?;
    if json {
        let doc = serde_json::json!({
            "location": location,
            "snapshots": items.iter().map(|i| serde_json::json!({
                "identity": i.identity,
                "bytes": i.bytes,
                "modified": i.modified,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&doc)?);
        return Ok(ExitCode::SUCCESS);
    }
    println!("location: {location}");
    if items.is_empty() {
        println!("(no snapshots)");
        return Ok(ExitCode::SUCCESS);
    }
    println!("{:<40} {:>14} {:<28}", "identity", "bytes", "modified");
    for i in &items {
        println!("{:<40} {:>14} {:<28}", i.identity, i.bytes, i.modified);
    }
    Ok(ExitCode::SUCCESS)
}

fn rm(identity: &str, config: &Path) -> anyhow::Result<ExitCode> {
    anyhow::ensure!(
        !identity.is_empty() && !identity.contains(['/', '\\']) && !identity.starts_with('.'),
        "identity must be a plain name, got {identity:?}"
    );
    let cfg = load_config(config)?;
    let snap = &cfg.kv.snapshot;
    anyhow::ensure!(snap.enabled(), "[kv.snapshot] mode is \"off\": nothing to remove");
    if snap.is_object() {
        let (client, prefix) = ephpm_server::kv_snapshot::object_client_from_config(&cfg)?;
        let key = ephpm_server::kv_snapshot::object_key(&prefix, identity);
        let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
        let url = format!("s3://{}/{key}", client.location().bucket);
        if rt.block_on(client.head(&key))?.is_none() {
            eprintln!("error: {url}: no such object");
            return Ok(ExitCode::FAILURE);
        }
        rt.block_on(client.delete(&key))?;
        println!("removed {url}");
    } else {
        let dir = PathBuf::from(snap.path.as_deref().context("[kv.snapshot] path is not set")?);
        let file =
            dir.join(format!("{identity}.{}", ephpm_server::kv_snapshot::SNAPSHOT_EXTENSION));
        match std::fs::remove_file(&file) {
            Ok(()) => println!("removed {}", file.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("error: {}: no such file", file.display());
                return Ok(ExitCode::FAILURE);
            }
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!("remove {}", file.display())));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn inspect(file: &Path, config: Option<&Path>, json: bool) -> anyhow::Result<ExitCode> {
    let secret = config.map(secret_from_config).transpose()?;
    let inspection = match snapshot::inspect(file, secret.as_deref(), u64::MAX) {
        Ok(i) => i,
        Err(SnapshotError::Io(e)) => {
            return Err(anyhow::Error::new(e).context(format!("could not read {}", file.display())));
        }
        Err(e) => {
            eprintln!("error: {}: {e}", file.display());
            return Ok(ExitCode::from(2));
        }
    };
    let h = &inspection.header;
    if json {
        let sites = inspection.sites.as_ref().map(|sites| {
            sites
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "site": s.site,
                        "blocks": s.blocks,
                        "entries": s.entries,
                        "plain_bytes": s.plain_bytes,
                    })
                })
                .collect::<Vec<_>>()
        });
        let doc = serde_json::json!({
            "file": file.display().to_string(),
            "format_version": h.version,
            "created_at_unix_ms": h.created_at_ms,
            "node_id": h.node_id,
            "multi_tenant": h.flags & snapshot::format::FLAG_MULTI_TENANT != 0,
            "cipher": cipher_name(h.cipher),
            "kdf": kdf_name(h.kdf),
            "block_compression": block_compression_name(h.block_compression),
            "bytes": inspection.bytes,
            "blocks": inspection.blocks,
            "sites": sites,
        });
        println!("{}", serde_json::to_string_pretty(&doc)?);
        return Ok(ExitCode::SUCCESS);
    }
    println!("file:              {}", file.display());
    println!("format:            v{}", h.version);
    println!(
        "created:           {} (unix ms {})",
        format_unix_ms(h.created_at_ms),
        h.created_at_ms
    );
    println!("written by:        {}", if h.node_id.is_empty() { "(unknown)" } else { &h.node_id });
    println!(
        "multi-tenant:      {}",
        if h.flags & snapshot::format::FLAG_MULTI_TENANT != 0 { "yes" } else { "no" }
    );
    println!(
        "sealed with:       {} / {} / block compression {}",
        cipher_name(h.cipher),
        kdf_name(h.kdf),
        block_compression_name(h.block_compression)
    );
    println!("size:              {} bytes", inspection.bytes);
    match (&inspection.sites, inspection.blocks) {
        (Some(sites), Some(blocks)) => {
            let entries: u64 = sites.iter().map(|s| s.entries).sum();
            let plain: u64 = sites.iter().map(|s| s.plain_bytes).sum();
            println!("blocks:            {blocks}");
            println!("entries:           {entries}");
            println!("plaintext bytes:   {plain}");
            println!();
            println!("{:<40} {:>8} {:>12} {:>16}", "site", "blocks", "entries", "plaintext bytes");
            for s in sites {
                let name = if s.site.is_empty() { "(default)" } else { s.site.as_str() };
                println!("{name:<40} {:>8} {:>12} {:>16}", s.blocks, s.entries, s.plain_bytes);
            }
        }
        _ => println!("(pass --config to open the index and list per-site totals)"),
    }
    Ok(ExitCode::SUCCESS)
}

fn verify(file: &Path, config: &Path) -> anyhow::Result<ExitCode> {
    let secret = secret_from_config(config)?;
    let report = match snapshot::verify(file, &secret, u64::MAX) {
        Ok(r) => r,
        Err(SnapshotError::Io(e)) => {
            return Err(anyhow::Error::new(e).context(format!("could not read {}", file.display())));
        }
        Err(e) => {
            eprintln!("error: {}: {e}", file.display());
            return Ok(ExitCode::from(2));
        }
    };
    if report.is_clean() {
        println!(
            "ok: {} — {} blocks, {} entries verified (written by {} at {})",
            file.display(),
            report.blocks,
            report.entries,
            if report.header.node_id.is_empty() { "unknown" } else { &report.header.node_id },
            format_unix_ms(report.header.created_at_ms)
        );
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!(
            "error: {}: {} of {} blocks failed to authenticate or decode (block index {:?}); a \
             load would skip them",
            file.display(),
            report.bad_blocks.len(),
            report.blocks,
            report.bad_blocks
        );
        Ok(ExitCode::from(2))
    }
}

/// Call the running server's control endpoint and print its JSON answer.
fn call(target: &ServerTarget, method: &str, path: &str) -> anyhow::Result<ExitCode> {
    let config = target.config.as_deref().map(load_config).transpose()?;
    let base = match (&target.url, &config) {
        (Some(url), _) => url.trim_end_matches('/').to_string(),
        (None, Some(cfg)) => url_from_listen(&cfg.server.listen),
        (None, None) => anyhow::bail!("pass --url http://host:port or --config <ephpm.toml>"),
    };
    let token = target
        .token
        .clone()
        .or_else(|| config.as_ref().and_then(|c| c.server.diagnostics.token.clone()))
        .filter(|t| !t.is_empty());
    let url = format!("{base}{path}");

    // reqwest is built with no crypto provider and no roots of its own
    // (#241); hand it the process provider + bundled webpki roots so an
    // https:// URL works too.
    ephpm_server::tls::install_default_crypto_provider();
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    let client = reqwest::blocking::Client::builder()
        .use_preconfigured_tls(tls)
        .user_agent(concat!("ephpm/", env!("EPHPM_VERSION"), " (kv snapshot)"))
        .connect_timeout(Duration::from_secs(10))
        // A save of a large store can take a while; the server bounds it.
        .timeout(Duration::from_secs(600))
        .build()
        .context("could not build the HTTP client")?;
    let mut req = match method {
        "POST" => client.post(&url),
        _ => client.get(&url),
    };
    if let Some(token) = &token {
        req = req.bearer_auth(token);
    }
    let resp = req.send().with_context(|| format!("{method} {url} failed"))?;
    let status = resp.status();
    let body = resp.text().context("reading the server's answer")?;
    let pretty = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or(body);
    if status.is_success() {
        println!("{pretty}");
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!("error: {method} {url}: HTTP {status}\n{pretty}");
        Ok(ExitCode::FAILURE)
    }
}

/// `http://` URL for a `[server] listen` address; an unspecified bind address
/// (`0.0.0.0`, `[::]`) becomes loopback, since that is where the CLI runs.
fn url_from_listen(listen: &str) -> String {
    let listen = listen.trim();
    let (host, port) = match listen.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => (h.trim_matches(['[', ']']), p),
        _ => (listen, "8080"),
    };
    let host = match host {
        "" | "0.0.0.0" => "127.0.0.1".to_string(),
        "::" => "[::1]".to_string(),
        h if h.contains(':') => format!("[{h}]"),
        h => h.to_string(),
    };
    format!("http://{host}:{port}")
}

fn cipher_name(id: u8) -> &'static str {
    match id {
        snapshot::format::CIPHER_CHACHA20_POLY1305 => "ChaCha20-Poly1305",
        _ => "unknown",
    }
}

fn kdf_name(id: u8) -> &'static str {
    match id {
        snapshot::format::KDF_HKDF_SHA256 => "HKDF-SHA256",
        _ => "unknown",
    }
}

fn block_compression_name(id: u8) -> &'static str {
    match id {
        snapshot::format::BLOCK_COMPRESSION_NONE => "none",
        snapshot::format::BLOCK_COMPRESSION_LZ4 => "lz4",
        _ => "unknown",
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` for a Unix-millisecond timestamp (pure integer
/// arithmetic — no date crate).
fn format_unix_ms(ms: u64) -> String {
    let secs = i64::try_from(ms / 1000).unwrap_or(i64::MAX);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", tod / 3600, (tod % 3600) / 60, tod % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_to_url_maps_unspecified_binds_to_loopback() {
        assert_eq!(url_from_listen("0.0.0.0:8080"), "http://127.0.0.1:8080");
        assert_eq!(url_from_listen("127.0.0.1:9000"), "http://127.0.0.1:9000");
        assert_eq!(url_from_listen("[::]:8080"), "http://[::1]:8080");
        assert_eq!(url_from_listen("[::1]:8080"), "http://[::1]:8080");
        assert_eq!(url_from_listen("10.0.0.5:80"), "http://10.0.0.5:80");
    }

    #[test]
    fn unix_ms_formats_as_utc() {
        assert_eq!(format_unix_ms(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_unix_ms(1_700_000_000_000), "2023-11-14T22:13:20Z");
    }
}
