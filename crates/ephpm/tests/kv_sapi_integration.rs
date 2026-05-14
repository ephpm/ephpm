//! Integration tests for KV store SAPI functions.
//!
//! Spins up ephpm with the KV store enabled, executes PHP test code that calls
//! the `ephpm_kv_*` native functions, and validates the results via HTTP responses.
//!
//! Requires: `cargo xtask release` (PHP linked)
//!
//! Run with: cargo nextest run -p ephpm --run-ignored all

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// Helper to wait for a port to be listening.
fn wait_for_port(port: u16, timeout_secs: u64) -> bool {
    let start = std::time::Instant::now();
    loop {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        if start.elapsed() > Duration::from_secs(timeout_secs) {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Helper to make HTTP GET request and parse JSON response.
async fn get_json(url: &str) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let resp = reqwest::get(url).await?;
    let status = resp.status();
    let body = resp.text().await?;

    if !status.is_success() {
        return Err(format!("HTTP {}: {}", status, body).into());
    }

    serde_json::from_str(&body).map_err(|e| format!("JSON parse error: {}", e).into())
}

/// Create a temporary config file with KV store enabled.
fn create_test_config(port: u16, docroot: &str) -> (tempfile::NamedTempFile, String) {
    use std::io::Write;

    let config_content = format!(
        r#"[server]
listen = "127.0.0.1:{port}"
document_root = "{docroot}"
index_files = ["index.php", "kv_sapi_test.php"]

[php]
max_execution_time = 30
memory_limit = "128M"

[kv]
enabled = true
listen = "127.0.0.1:6379"
"#
    );

    let mut f = tempfile::NamedTempFile::new().expect("failed to create temp config");
    f.write_all(config_content.as_bytes()).expect("failed to write config");
    f.flush().expect("failed to flush config");

    (f, config_content)
}

/// Find the ephpm binary (built with `cargo xtask release`).
fn find_ephpm_binary() -> PathBuf {
    // Try common locations after `cargo xtask release`
    let candidates = [
        PathBuf::from("target/x86_64-unknown-linux-gnu/release/ephpm"),
        PathBuf::from("target/release/ephpm"),
        PathBuf::from("./ephpm"),
    ];

    for candidate in &candidates {
        if candidate.exists() {
            return candidate.clone();
        }
    }

    panic!("ephpm binary not found. Run `cargo xtask release` first. Checked: {:?}", candidates);
}

#[tokio::test]
#[ignore = "requires cargo xtask release (PHP linked)"]
async fn kv_sapi_set_get() {
    let port = 9876;
    let docroot = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests");

    let (_config_file, _config) = create_test_config(port, docroot);
    let ephpm = find_ephpm_binary();

    // Spawn ephpm server
    let mut child = Command::new(&ephpm)
        .arg("--config")
        .arg(_config_file.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn ephpm");

    // Wait for server to start
    assert!(wait_for_port(port as u16, 5), "ephpm server failed to start on port {}", port);

    // Run test and clean up
    let result = get_json(&format!("http://127.0.0.1:{}/kv_sapi_test.php?test=set_get", port))
        .await
        .expect("request failed");

    child.kill().ok();
    child.wait().ok();

    assert_eq!(
        result["passed"],
        true,
        "test failed: {}",
        result.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error")
    );
}

#[tokio::test]
#[ignore = "requires cargo xtask release (PHP linked)"]
async fn kv_sapi_del() {
    let port = 9877;
    let docroot = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests");

    let (_config_file, _config) = create_test_config(port, docroot);
    let ephpm = find_ephpm_binary();

    let mut child = Command::new(&ephpm)
        .arg("--config")
        .arg(_config_file.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn ephpm");

    assert!(wait_for_port(port as u16, 5), "ephpm failed to start");

    let result = get_json(&format!("http://127.0.0.1:{}/kv_sapi_test.php?test=del", port))
        .await
        .expect("request failed");

    child.kill().ok();
    child.wait().ok();

    assert_eq!(result["passed"], true, "test failed");
}

#[tokio::test]
#[ignore = "requires cargo xtask release (PHP linked)"]
async fn kv_sapi_all() {
    let port = 9878;
    let docroot = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests");

    let (_config_file, _config) = create_test_config(port, docroot);
    let ephpm = find_ephpm_binary();

    let mut child = Command::new(&ephpm)
        .arg("--config")
        .arg(_config_file.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn ephpm");

    assert!(wait_for_port(port as u16, 5), "ephpm failed to start");

    let result = get_json(&format!("http://127.0.0.1:{}/kv_sapi_test.php?test=all", port))
        .await
        .expect("request failed");

    child.kill().ok();
    child.wait().ok();

    assert_eq!(result["passed"], true, "test failed: {:?}", result.get("error"));
}
