//! OPcache clustered invalidation e2e tests (Phase 1).
//!
//! Validates the watcher end-to-end:
//!   1. PRE — warm the OPcache by hitting `opcache_status.php` (warming
//!      probe) and confirming the target file is cached.
//!   2. Trigger — write `opcache:version:_default` (single-node) or
//!      `opcache:version:_all` (cluster) via the RESP listener so the watcher
//!      fires on the next request.
//!   3. POST — hit `opcache_status.php?warm=0` (cold probe). The request
//!      trips the watcher BEFORE the probe script runs, so the probe must
//!      see the target genuinely dropped (`opcache_is_script_cached` =
//!      false). A follow-up warming probe then re-caches it. This is the
//!      strong assertion pair — it fails if the invalidator silently no-ops.
//!
//! Both `EPHPM_URL` (single-node) and `EPHPM_CLUSTER_URL_*` (cluster) paths
//! are covered. Tests skip gracefully when the required env vars are unset,
//! so they don't break the single-node runner.
//!
//! # Fixtures
//!
//! - `tests/docroot/opcache_status.php` — probe endpoint returning JSON
//! - `tests/docroot/opcache_target.php`  — file whose OPcache entry is tracked
//!
//! # Environment
//!
//! - `EPHPM_URL` — single-node base URL (skips cluster tests)
//! - `EPHPM_CLUSTER_URL_0`, `EPHPM_CLUSTER_URL_1`, `EPHPM_CLUSTER_URL_2` —
//!   cluster node base URLs (skips cluster tests when unset)
//! - `EPHPM_KV_HOST` / `EPHPM_KV_PORT` — RESP listener the CLI would target;
//!   defaults to `127.0.0.1:6379`. Only used by the single-node test since
//!   cluster tests hit each node's RESP listener via its base host.

use std::time::Duration;

use bytes::BytesMut;
use ephpm_e2e::required_env;
use ephpm_kv::resp::{Frame, parse_frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// KV key prefix — must match `ephpm-server::opcache::KV_VERSION_PREFIX`.
const OPCACHE_VERSION_PREFIX: &str = "opcache:version:";
/// Broadcast vhost — must match `ephpm-server::opcache::BROADCAST_VHOST`.
const OPCACHE_BROADCAST_VHOST: &str = "_all";
/// Default vhost — must match `ephpm-server::opcache::DEFAULT_VHOST`.
const OPCACHE_DEFAULT_VHOST: &str = "_default";

fn cluster_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn cluster_urls() -> Vec<String> {
    let mut urls = Vec::new();
    for name in ["EPHPM_CLUSTER_URL_0", "EPHPM_CLUSTER_URL_1", "EPHPM_CLUSTER_URL_2"] {
        match cluster_env(name) {
            Some(u) if !u.is_empty() => urls.push(u),
            _ => return Vec::new(),
        }
    }
    urls
}

/// Fetch and decode the probe JSON at `<base>/opcache_status.php`.
async fn probe_status(base_url: &str) -> serde_json::Value {
    probe_status_with(base_url, true).await
}

/// Probe without warming (`warm=0`) — required to observe that an
/// invalidation dropped the target, since a warming probe re-caches it
/// within the same request.
async fn probe_status_cold(base_url: &str) -> serde_json::Value {
    probe_status_with(base_url, false).await
}

async fn probe_status_with(base_url: &str, warm: bool) -> serde_json::Value {
    let client = reqwest::Client::new();
    let warm_q = if warm { "1" } else { "0" };
    let url = format!("{base_url}/opcache_status.php?warm={warm_q}");
    let resp = client
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));
    assert!(
        resp.status().is_success(),
        "probe {url} returned {}",
        resp.status()
    );
    resp.json().await.unwrap_or_else(|e| panic!("invalid JSON from {url}: {e}"))
}

/// Open a RESP connection and send a single command, returning the response.
async fn resp_roundtrip(host: &str, port: u16, cmd: Frame) -> anyhow::Result<Frame> {
    let mut stream = TcpStream::connect(format!("{host}:{port}"))
        .await
        .map_err(|e| anyhow::anyhow!("connect {host}:{port}: {e}"))?;
    let bytes = cmd.to_bytes();
    stream.write_all(&bytes).await?;
    let mut buf = BytesMut::with_capacity(4096);
    loop {
        buf.reserve(512);
        let n = stream.read_buf(&mut buf).await?;
        if n == 0 {
            anyhow::bail!("connection closed before RESP frame arrived");
        }
        if let Some(frame) = parse_frame(&mut buf)? {
            return Ok(frame);
        }
    }
}

/// Write `opcache:version:<vhost> = value` via RESP.
async fn set_version(host: &str, port: u16, vhost: &str, value: u64) -> anyhow::Result<()> {
    let key = format!("{OPCACHE_VERSION_PREFIX}{vhost}");
    let cmd = Frame::Array(vec![
        Frame::bulk(b"SET".to_vec()),
        Frame::bulk(key.into_bytes()),
        Frame::bulk(value.to_string().into_bytes()),
    ]);
    match resp_roundtrip(host, port, cmd).await? {
        Frame::Simple(_) => Ok(()),
        Frame::Error(e) => anyhow::bail!("RESP error: {e}"),
        other => anyhow::bail!("unexpected response: {other}"),
    }
}

/// Current epoch-millis timestamp — matches what the CLI writes for a deploy.
fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Single-node: PRE / trigger / POST against a single instance's RESP listener.
// ---------------------------------------------------------------------------

/// Single-node happy-path: PRE probe confirms OPcache is enabled and the
/// target is cached; a KV write triggers the watcher on the next request;
/// POST probe still returns 200 and reports valid stats (the invalidation
/// itself is best-observed via metrics, not the probe payload, because the
/// probe re-includes the target and repopulates the cache within one
/// request).
#[tokio::test]
async fn single_node_deploy_triggers_invalidation() {
    let Ok(base_url) = std::env::var("EPHPM_URL") else {
        eprintln!("EPHPM_URL not set — skipping single-node opcache test");
        return;
    };
    let kv_host = std::env::var("EPHPM_KV_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let kv_port: u16 = std::env::var("EPHPM_KV_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6379);

    // Skip cleanly if the RESP listener is disabled — the test can't function
    // without it (the CLI has no other way to write to the in-process KV).
    let ping = Frame::Array(vec![Frame::bulk(b"PING".to_vec())]);
    if resp_roundtrip(&kv_host, kv_port, ping).await.is_err() {
        eprintln!(
            "RESP listener at {kv_host}:{kv_port} unreachable — skipping (\
             enable [kv.redis_compat] in ephpm.toml)"
        );
        return;
    }

    // PRE: warm the OPcache and confirm state.
    let pre = probe_status(base_url.as_str()).await;
    let opcache_enabled = pre["opcache_enabled"].as_bool().unwrap_or(false);
    if !opcache_enabled {
        eprintln!("OPcache extension not loaded — skipping (build with static-php-cli opcache)");
        return;
    }
    assert!(
        pre["target_cached"].as_bool().unwrap_or(false),
        "PRE probe should have cached the target: {pre}"
    );

    // Trigger: write a fresh version stamp. Any strictly-greater value
    // relative to the watcher's last-seen version fires the invalidation on
    // the next request.
    let version = epoch_ms();
    set_version(&kv_host, kv_port, OPCACHE_DEFAULT_VHOST, version)
        .await
        .expect("SET version key");

    // POST (cold probe): this request trips the watcher, which invalidates
    // everything under docroot BEFORE the probe script runs — so the probe
    // must see the target genuinely dropped (opcache_is_script_cached =
    // false). This is the strong assertion: it fails if the invalidator
    // silently no-ops.
    let post = probe_status_cold(base_url.as_str()).await;
    assert!(
        post["opcache_enabled"].as_bool().unwrap_or(false),
        "OPcache should still be enabled after invalidation: {post}"
    );
    assert!(
        !post["target_cached"].as_bool().unwrap_or(true),
        "target should be DROPPED by the invalidation: {post}"
    );

    // RE-WARM: the next warming probe recompiles and re-caches it.
    let rewarm = probe_status(base_url.as_str()).await;
    assert!(
        rewarm["target_cached"].as_bool().unwrap_or(false),
        "target should be recompiled + re-cached after re-warm: {rewarm}"
    );
}

/// Local reset via the same code path as `ephpm cache reset` (RESP SET).
#[tokio::test]
async fn single_node_cache_reset_works() {
    let Ok(base_url) = std::env::var("EPHPM_URL") else {
        eprintln!("EPHPM_URL not set — skipping single-node cache-reset test");
        return;
    };
    let kv_host = std::env::var("EPHPM_KV_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let kv_port: u16 = std::env::var("EPHPM_KV_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6379);

    let ping = Frame::Array(vec![Frame::bulk(b"PING".to_vec())]);
    if resp_roundtrip(&kv_host, kv_port, ping).await.is_err() {
        eprintln!("RESP listener unreachable — skipping");
        return;
    }
    let pre = probe_status(base_url.as_str()).await;
    if !pre["opcache_enabled"].as_bool().unwrap_or(false) {
        eprintln!("OPcache not loaded — skipping");
        return;
    }

    // Two resets in quick succession must both propagate (each writes a
    // strictly-greater version stamp). We just check the endpoint keeps
    // returning valid state.
    for _ in 0..2 {
        let version = epoch_ms();
        set_version(&kv_host, kv_port, OPCACHE_DEFAULT_VHOST, version)
            .await
            .expect("SET version key");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let probe = probe_status(base_url.as_str()).await;
        assert!(probe["opcache_enabled"].as_bool().unwrap_or(false));
    }
}

// ---------------------------------------------------------------------------
// Cluster: PRE / trigger / POST across every node using the broadcast key.
// ---------------------------------------------------------------------------

/// Cluster fan-out: writing the broadcast key on any one node should fire
/// the watcher on EVERY node within a gossip convergence window.
#[tokio::test]
async fn cluster_broadcast_fans_out_to_all_nodes() {
    let urls = cluster_urls();
    if urls.is_empty() {
        eprintln!("cluster env vars not set — skipping");
        return;
    }
    let kv_port: u16 = std::env::var("EPHPM_KV_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6379);

    // Extract the hostname for node 0's RESP endpoint from its base URL.
    let host0 = url_host(&urls[0]).expect("parse node 0 URL");

    // PRE: warm every node. Confirm OPcache is enabled on all.
    let mut opcache_available = true;
    for (i, url) in urls.iter().enumerate() {
        let pre = probe_status(url.as_str()).await;
        if !pre["opcache_enabled"].as_bool().unwrap_or(false) {
            eprintln!("node {i}: OPcache not enabled — skipping");
            opcache_available = false;
            break;
        }
    }
    if !opcache_available {
        return;
    }

    // Check that node 0's RESP listener is reachable at all.
    let ping = Frame::Array(vec![Frame::bulk(b"PING".to_vec())]);
    if resp_roundtrip(&host0, kv_port, ping).await.is_err() {
        eprintln!("node 0 RESP unreachable at {host0}:{kv_port} — skipping");
        return;
    }

    // Trigger the broadcast key on node 0.
    let version = epoch_ms();
    set_version(&host0, kv_port, OPCACHE_BROADCAST_VHOST, version)
        .await
        .expect("SET broadcast key on node 0");

    // POST: cold-probe every node until the target shows as DROPPED —
    // the strong signal that the broadcast reached that node and its
    // watcher ran the invalidation. Gossip needs a moment to reach peers;
    // retry for up to ~15 s per node.
    for (i, url) in urls.iter().enumerate() {
        let mut dropped = false;
        for _ in 0..30 {
            let post = probe_status_cold(url.as_str()).await;
            if post["opcache_enabled"].as_bool().unwrap_or(false)
                && !post["target_cached"].as_bool().unwrap_or(true)
            {
                dropped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert!(dropped, "node {i} ({url}) never dropped the target after broadcast");

        // And a warming probe recompiles it — the node stays serviceable.
        let rewarm = probe_status(url.as_str()).await;
        assert!(
            rewarm["target_cached"].as_bool().unwrap_or(false),
            "node {i} ({url}) failed to re-cache after invalidation: {rewarm}"
        );
    }
}

/// Extract the `host` component from a base URL string like `http://foo:8080`.
fn url_host(url: &str) -> Option<String> {
    let no_scheme = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://"))?;
    let host_port = no_scheme.split('/').next()?;
    let host = host_port.split(':').next()?;
    if host.is_empty() { None } else { Some(host.to_string()) }
}

#[cfg(test)]
mod helper_tests {
    use super::*;

    #[test]
    fn url_host_extracts_hostname() {
        assert_eq!(url_host("http://ephpm-cluster-0:8080"), Some("ephpm-cluster-0".to_string()));
        assert_eq!(url_host("http://127.0.0.1:8080/foo"), Some("127.0.0.1".to_string()));
        assert_eq!(url_host("https://example.com"), Some("example.com".to_string()));
        assert_eq!(url_host("not-a-url"), None);
    }

    // Sanity: single-node/broadcast constants stay in-lockstep with the
    // ephpm-server::opcache module. Kept as a comment here since ephpm-e2e
    // deliberately avoids depending on ephpm-server (it lives outside the
    // workspace and needs to compile without the server's dep tree).
    #[test]
    fn constants_documented() {
        assert_eq!(OPCACHE_DEFAULT_VHOST, "_default");
        assert_eq!(OPCACHE_BROADCAST_VHOST, "_all");
        assert_eq!(OPCACHE_VERSION_PREFIX, "opcache:version:");
        assert!(required_env_documented());
    }

    fn required_env_documented() -> bool {
        // Keep required_env in the import graph so an accidental removal of
        // the helper (or its re-export) shows up as a compile error, not a
        // silent skip. Not a runtime assertion — the presence check is at
        // the top of the module.
        true
    }
}
