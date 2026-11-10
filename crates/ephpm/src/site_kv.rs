//! `ephpm php --site <key>` KV binding (issue #700).
//!
//! The symmetric counterpart of [`crate::site_db`] for the KV store: it binds
//! the `ephpm_kv_*` SAPI functions to a **running** server's live KV so an
//! out-of-band CLI/cron script — a cache warmer, a nightly rebuild — reads and
//! writes the server's KV, with its writes replicating across the cluster
//! through the server's own store.
//!
//! # Strategy
//!
//! Unlike the DB wire there is no offline mode: the KV is in-process state of a
//! running server, so there is nothing to open when no server is up. The picker
//! is therefore:
//!
//! 1. **Internal endpoint reachable** — the running server's always-on loopback
//!    KV listener ([`ephpm_server::internal_kv`]), discovered via its runtime
//!    file. Preferred: it is loopback-only and present whenever the server runs
//!    multi-tenant with `[kv] secret`.
//! 2. **Public RESP listener reachable** — `[kv.redis_compat]` when the operator
//!    enabled it. A fallback for the case the internal file is missing.
//! 3. **Unattached** — no server found. The `ephpm_kv_*` functions stay unwired
//!    (today's behaviour), now *detectable* via `ephpm_kv_attached()`, and the
//!    first call warns once.
//!
//! KV binding is **best-effort**: unlike `--site`'s DB binding (which the whole
//! invocation depends on), a missing KV endpoint never fails the run — a script
//! that only touches the DB must still work. The reason is logged.
//!
//! # Security
//!
//! Connects to loopback only, authenticating per-site as
//! `AUTH <site> HMAC-SHA256([kv] secret, site)` — the same derivation the
//! server verifies and the public listener uses. Requires `[kv] secret`
//! (fails closed to unattached without it). Same trust boundary as the DB wire:
//! whoever can read the config can derive any tenant's password.

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use ephpm_config::Config;
use ephpm_php::kv_remote::{RemoteKvClient, RemoteKvEndpoint};

/// How long to wait for a TCP connection when probing an endpoint for
/// reachability. Loopback, so a live listener answers far under this.
const PROBE_TIMEOUT: Duration = Duration::from_millis(400);

/// Which endpoint the CLI attached through (for the log line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KvVia {
    /// The server's always-on internal loopback KV endpoint.
    Internal,
    /// The operator-enabled public `[kv.redis_compat]` listener.
    PublicListener,
}

impl KvVia {
    fn label(self) -> &'static str {
        match self {
            Self::Internal => "internal loopback endpoint",
            Self::PublicListener => "public [kv.redis_compat] listener",
        }
    }
}

/// The KV-binding plan for a `--site` invocation.
#[derive(Debug, Clone)]
pub enum KvStrategy {
    /// A running server was found; attach as `user`/`password` to `addr`.
    Attached { addr: SocketAddr, user: String, password: String, via: KvVia },
    /// No usable endpoint; the KV functions stay unwired. `reason` explains why.
    Unattached { reason: String },
}

/// Decide how to reach `site_key`'s KV under `config`: validate the site,
/// require multi-tenant + `[kv] secret`, then discover and probe an endpoint.
///
/// Never errors — an unusable configuration or an absent server yields
/// [`KvStrategy::Unattached`] with a human-readable reason, because KV binding
/// is best-effort (see the module docs).
#[must_use]
pub fn plan_site_kv(config: &Config, site_key: &str) -> KvStrategy {
    // Validate the key against the configuration (fail-closed, #290/#291) and
    // get its canonical form — the same key the server HMACs.
    let key = match ephpm_server::router::resolve_sandbox_site(config, site_key) {
        Ok(site) => site.key,
        Err(e) => {
            return KvStrategy::Unattached {
                reason: format!("invalid --site {site_key:?}: {e:#}"),
            };
        }
    };

    if config.server.sites_dir.is_none() {
        return KvStrategy::Unattached {
            reason: "the CLI KV wire currently supports multi-tenant deployments \
                     ([server] sites_dir) only"
                .into(),
        };
    }

    let Some(secret) = config.kv.secret.as_deref().filter(|s| !s.trim().is_empty()) else {
        return KvStrategy::Unattached {
            reason: "[kv] secret is not set, so the CLI cannot derive the per-site KV \
                     password — set it (the server must use the same value)"
                .into(),
        };
    };
    let password = ephpm_kv::auth::derive_site_password(secret, &key);

    // 1. The server's internal loopback endpoint (preferred).
    if let Some(addr) = ephpm_server::internal_kv::read_endpoint(config)
        && addr_reachable(addr)
    {
        return KvStrategy::Attached { addr, user: key, password, via: KvVia::Internal };
    }

    // 2. The public RESP listener, if the operator enabled it.
    if config.kv.redis_compat.enabled
        && let Some(addr) = resolve_first(&config.kv.redis_compat.listen)
        && addr_reachable(addr)
    {
        return KvStrategy::Attached { addr, user: key, password, via: KvVia::PublicListener };
    }

    KvStrategy::Unattached {
        reason: "no running server was found on this host (neither the internal KV \
                 endpoint nor an enabled [kv.redis_compat] listener answered) — the \
                 CLI KV wire needs `ephpm serve` running with the same config"
            .into(),
    }
}

/// Act on a [`KvStrategy`]: connect and wire the remote KV client into the PHP
/// runtime, or log why the KV stayed unattached. Best-effort — never returns an
/// error, so a KV endpoint that is gone cannot fail a `--site` run that only
/// needed the database.
pub fn bind_site_kv(strategy: KvStrategy) {
    match strategy {
        KvStrategy::Attached { addr, user, password, via } => {
            let endpoint =
                RemoteKvEndpoint { addr, user: Some(user.clone()), password: Some(password) };
            match RemoteKvClient::connect(endpoint) {
                Ok(client) => {
                    ephpm_php::PhpRuntime::set_remote_kv_ops(client);
                    tracing::info!(
                        site = %user,
                        %addr,
                        via = via.label(),
                        "ephpm php --site: attached ephpm_kv_* to the running server's KV"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        site = %user,
                        %addr,
                        error = %e,
                        "ephpm php --site: could not attach to the server's KV ({}); \
                         ephpm_kv_* will report unattached",
                        via.label()
                    );
                }
            }
        }
        KvStrategy::Unattached { reason } => {
            tracing::info!(
                reason = %reason,
                "ephpm php --site: ephpm_kv_* is not attached (ephpm_kv_attached() is false)"
            );
        }
    }
}

/// True when a TCP connection to `addr` is accepted within [`PROBE_TIMEOUT`].
fn addr_reachable(addr: SocketAddr) -> bool {
    match TcpStream::connect_timeout(&addr, PROBE_TIMEOUT) {
        Ok(stream) => {
            drop(stream);
            true
        }
        Err(_) => false,
    }
}

/// Resolve the first socket address of a `host:port` listen string.
fn resolve_first(listen: &str) -> Option<SocketAddr> {
    listen.to_socket_addrs().ok()?.next()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;

    fn toml_path(p: &Path) -> String {
        p.to_string_lossy().replace('\\', "/")
    }

    /// Build a multi-site config with a `shop` vhost. `extra` appends TOML.
    fn cfg(extra: &str) -> (Config, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sites = tmp.path().join("sites");
        fs::create_dir_all(sites.join("shop")).expect("vhost dir");
        let body = format!(
            "[server]\n\
             document_root = \"{docroot}\"\n\
             sites_dir = \"{sites}\"\n\
             {extra}",
            docroot = toml_path(tmp.path()),
            sites = toml_path(&sites),
        );
        let path = tmp.path().join("ephpm.toml");
        fs::write(&path, body).expect("write config");
        let config = Config::load(&path).expect("load config");
        (config, tmp)
    }

    #[test]
    fn unknown_site_is_unattached_not_a_panic() {
        let (config, _g) = cfg("[kv]\nsecret = \"s3cr3t\"\n");
        let strategy = plan_site_kv(&config, "ghost");
        assert!(matches!(strategy, KvStrategy::Unattached { .. }));
    }

    #[test]
    fn traversal_key_is_unattached() {
        let (config, _g) = cfg("[kv]\nsecret = \"s3cr3t\"\n");
        assert!(matches!(plan_site_kv(&config, "../etc"), KvStrategy::Unattached { .. }));
        assert!(matches!(plan_site_kv(&config, "a/b"), KvStrategy::Unattached { .. }));
    }

    #[test]
    fn without_secret_is_unattached_naming_the_secret() {
        let (config, _g) = cfg("");
        match plan_site_kv(&config, "shop") {
            KvStrategy::Unattached { reason } => {
                assert!(reason.contains("[kv] secret"), "reason should name the secret: {reason}");
            }
            KvStrategy::Attached { .. } => panic!("must not attach without a secret"),
        }
    }

    #[test]
    fn no_server_running_is_unattached() {
        // Secret set, but nothing is listening (no discovery file, redis_compat
        // off): the picker must report "no running server", not attach.
        let (config, _g) = cfg("[kv]\nsecret = \"s3cr3t\"\n");
        // Make sure no stale discovery file from another run interferes.
        if let Some(p) = ephpm_server::internal_kv::endpoint_path(&config) {
            let _ = fs::remove_file(p);
        }
        match plan_site_kv(&config, "shop") {
            KvStrategy::Unattached { reason } => {
                assert!(reason.contains("no running server"), "reason: {reason}");
            }
            KvStrategy::Attached { .. } => panic!("nothing is listening; must not attach"),
        }
    }

    #[test]
    fn attaches_via_the_internal_endpoint_when_one_is_listening() {
        let (config, _g) = cfg("[kv]\nsecret = \"s3cr3t\"\n");
        // Stand in for a running server's internal endpoint: a live loopback
        // listener plus a discovery file pointing at it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let path = ephpm_server::internal_kv::endpoint_path(&config).expect("path");
        // Mirror what the server's writer produces: the per-user runtime dir
        // exists, and the file is `0600` and owned by us — otherwise
        // `read_endpoint`'s owner/mode check (Unix) refuses it.
        std::fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir runtime dir");
        fs::write(&path, format!("{{\"port\":{port},\"pid\":1}}")).expect("write discovery");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod 0600");
        }

        let strategy = plan_site_kv(&config, "shop");
        let _ = fs::remove_file(&path);
        match strategy {
            KvStrategy::Attached { addr, user, password, via } => {
                assert_eq!(addr.port(), port);
                assert_eq!(user, "shop");
                assert_eq!(password, ephpm_kv::auth::derive_site_password("s3cr3t", "shop"));
                assert_eq!(via, KvVia::Internal);
            }
            KvStrategy::Unattached { reason } => panic!("should have attached: {reason}"),
        }
    }
}
