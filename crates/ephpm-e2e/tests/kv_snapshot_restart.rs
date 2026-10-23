//! Warm-restart KV snapshots across a real process restart.
//!
//! Self-managed suite (`cargo xtask e2e` lists it in `SELF_MANAGED_SUITES`):
//! it spawns its own single node with `[kv.snapshot] mode = "local"`, writes
//! keys through the served PHP (`kv.php`, the `ephpm_kv_*` SAPI functions),
//! stops the node gracefully — the path that writes the snapshot — starts it
//! again on the same ports and data directory, and proves the keys are back,
//! with their TTLs, and that a key whose TTL expired meanwhile is not.
//!
//! Skips when `EPHPM_BINARY` is unset (a direct `cargo test` without the
//! harness). Graceful stop is `SIGTERM`, so the suite is Unix-only; on Windows
//! the fixture can only kill, which never writes a snapshot.

#![cfg(unix)]

use std::path::PathBuf;
use std::time::Duration;

use ephpm_e2e::{SingleNodeFixture, ephpm_binary_env};

/// `[kv.snapshot]` on, under the fixture's scratch `data/` so it dies with
/// the fixture. The `[kv] secret` is what every snapshot is sealed with.
const SNAPSHOT_CONFIG: &str = r#"
[kv]
secret = "e2e-kv-snapshot-secret-do-not-use-in-prod"

[kv.snapshot]
mode = "local"
path = "{DATA_DIR}/kv"
identity = "e2e-node"
load_timeout = "20s"

[server.diagnostics]
token = "e2e-diag-token"
"#;

/// `[server.diagnostics] token` from [`SNAPSHOT_CONFIG`]: the save/load
/// `POST`s need it on every node, single-site included.
const DIAG_TOKEN: &str = "e2e-diag-token";

fn resolve_docroot() -> PathBuf {
    if let Some(dir) = std::env::var_os("EPHPM_DOCROOT") {
        return PathBuf::from(dir);
    }
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    manifest_dir.join("..").join("..").join("tests").join("docroot")
}

async fn kv(base_url: &str, query: &str) -> String {
    let url = format!("{base_url}/kv.php?{query}");
    let resp = reqwest::get(&url).await.unwrap_or_else(|e| panic!("GET {url} failed: {e}"));
    assert_eq!(resp.status().as_u16(), 200, "GET {url}");
    resp.text().await.expect("body").trim().to_owned()
}

async fn snapshot_status(base_url: &str) -> serde_json::Value {
    let url = format!("{base_url}/_ephpm/kv/snapshot");
    // With a diagnostics token configured, the status needs it too (the
    // `/_ephpm/requests` rule).
    let resp = reqwest::Client::new()
        .get(&url)
        .bearer_auth(DIAG_TOKEN)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));
    assert_eq!(resp.status().as_u16(), 200, "GET {url}");
    resp.json().await.expect("status JSON")
}

#[tokio::test]
async fn keys_survive_a_graceful_restart_with_their_ttls() {
    let Some(binary) = ephpm_binary_env() else {
        eprintln!("skip: EPHPM_BINARY not set");
        return;
    };
    let docroot = resolve_docroot();
    let mut node = SingleNodeFixture::start_with(&binary, &docroot, SNAPSHOT_CONFIG)
        .await
        .expect("start node with snapshots on");
    let base = node.base_url().to_string();

    // First start: nothing to load — a cold start with reason "missing".
    let status = snapshot_status(&base).await;
    assert_eq!(status["mode"], "local", "{status}");
    assert_eq!(status["last_load"]["outcome"], "cold", "{status}");
    assert_eq!(status["last_load"]["reason"], "missing", "{status}");

    // Persistent key, a key with a long TTL, a key that will expire during
    // the restart, and a counter.
    assert_eq!(kv(&base, "op=set&key=snap:persist&val=survives&ttl=0").await, "ok");
    assert_eq!(kv(&base, "op=set&key=snap:ttl&val=timed&ttl=600").await, "ok");
    assert_eq!(kv(&base, "op=set&key=snap:short&val=gone&ttl=1").await, "ok");
    assert_eq!(kv(&base, "op=incr&key=snap:counter").await, "1");
    assert_eq!(kv(&base, "op=incr&key=snap:counter").await, "2");

    // The save is a state-changing call: without the diagnostics token it is
    // refused even on this single-site node, and nothing is written.
    for path in ["/_ephpm/kv/snapshot", "/_ephpm/kv/snapshot/load"] {
        let resp = reqwest::Client::new()
            .post(format!("{base}{path}"))
            .send()
            .await
            .expect("unauthenticated POST");
        assert_eq!(resp.status().as_u16(), 401, "POST {path} without the token");
    }

    // Save on demand through the control endpoint: a snapshot while serving
    // must be allowed, and the restart below must overwrite it with a newer
    // one that also carries the key written after it.
    let resp = reqwest::Client::new()
        .post(format!("{base}/_ephpm/kv/snapshot"))
        .bearer_auth(DIAG_TOKEN)
        .send()
        .await
        .expect("POST snapshot");
    assert_eq!(resp.status().as_u16(), 200);
    let saved: serde_json::Value = resp.json().await.expect("save JSON");
    assert_eq!(saved["ok"], true, "{saved}");
    assert!(saved["keys"].as_u64().unwrap() >= 4, "{saved}");
    assert_eq!(kv(&base, "op=set&key=snap:late&val=after-manual-save&ttl=0").await, "ok");

    // Let the 1-second key expire before the restart, then restart.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    node.restart().await.expect("graceful restart");

    let status = snapshot_status(&base).await;
    assert_eq!(status["last_load"]["outcome"], "ok", "{status}");
    assert!(status["last_load"]["keys"].as_u64().unwrap() >= 4, "{status}");
    assert_eq!(status["last_load"]["written_by"], "e2e-node", "{status}");

    assert_eq!(kv(&base, "op=get&key=snap:persist").await, "survives");
    assert_eq!(kv(&base, "op=get&key=snap:ttl").await, "timed");
    let pttl: i64 = kv(&base, "op=pttl&key=snap:ttl").await.parse().expect("pttl");
    assert!((500_000..=600_000).contains(&pttl), "TTL carried across the restart: {pttl}");
    assert_eq!(kv(&base, "op=get&key=snap:short").await, "null", "expired during the restart");
    assert_eq!(kv(&base, "op=get&key=snap:counter").await, "2");
    assert_eq!(kv(&base, "op=get&key=snap:late").await, "after-manual-save");
    assert_eq!(kv(&base, "op=incr&key=snap:counter").await, "3", "the counter keeps counting");

    // The file is where the config says, and the previous process's manual
    // snapshot was replaced atomically (one file, no temp file left).
    let dir = node.scratch_dir().join("data").join("kv");
    let names: Vec<String> = std::fs::read_dir(&dir)
        .expect("snapshot dir")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["e2e-node.kvsnap".to_string()], "{names:?}");
}

/// Changing `[kv] secret` must make the old snapshot unreadable: the node
/// starts cold (reason `wrong_key`) and serves, rather than loading or
/// crashing.
#[tokio::test]
async fn a_changed_secret_starts_cold_and_keeps_serving() {
    let Some(binary) = ephpm_binary_env() else {
        eprintln!("skip: EPHPM_BINARY not set");
        return;
    };
    let docroot = resolve_docroot();
    let mut node = SingleNodeFixture::start_with(&binary, &docroot, SNAPSHOT_CONFIG)
        .await
        .expect("start node with snapshots on");
    let base = node.base_url().to_string();
    assert_eq!(kv(&base, "op=set&key=rot:k&val=v&ttl=0").await, "ok");

    // Rotate the secret in the config the restart will re-read.
    let config_path = node.scratch_dir().join("ephpm.toml");
    let config = std::fs::read_to_string(&config_path).expect("config");
    std::fs::write(&config_path, config.replace("e2e-kv-snapshot-secret", "rotated-secret"))
        .expect("rewrite config");
    node.restart().await.expect("restart with a new secret");

    let status = snapshot_status(&base).await;
    assert_eq!(status["last_load"]["outcome"], "cold", "{status}");
    assert_eq!(status["last_load"]["reason"], "wrong_key", "{status}");
    assert_eq!(kv(&base, "op=get&key=rot:k").await, "null", "nothing restored under the wrong key");
    assert_eq!(kv(&base, "op=set&key=rot:k2&val=v2&ttl=0").await, "ok", "the node serves");
}
