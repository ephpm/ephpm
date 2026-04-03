//! P0 security e2e tests.
//!
//! Validates critical security boundaries:
//! - `open_basedir` prevents cross-site filesystem reads
//! - `disable_functions` blocks shell execution
//! - KV SAPI functions work even when RESP auth is configured
//! - Multi-tenant KV isolation (per-site DashMap scoping)
//! - `trusted_hosts` rejects requests with spoofed Host headers
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)
//! - `EPHPM_SITES_DIR` — writable path to the sites directory on the ephpm host

use std::path::PathBuf;

use ephpm_e2e::required_env;

/// Get the sites directory path from env.
fn sites_dir() -> PathBuf {
    PathBuf::from(required_env("EPHPM_SITES_DIR"))
}

/// Make an HTTP request with a specific Host header and return (status, body).
async fn get_with_host(base_url: &str, host: &str, path: &str) -> (u16, String) {
    let client = reqwest::Client::new();
    let url = format!("{base_url}{path}");
    let resp = client
        .get(&url)
        .header("Host", host)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {path} with Host: {host} failed: {e}"));
    let status = resp.status().as_u16();
    let body = resp
        .text()
        .await
        .expect("failed to read response body")
        .trim()
        .to_owned();
    (status, body)
}

/// Deploy a site directory with a PHP file copied from the default docroot.
fn deploy_site(sites: &PathBuf, hostname: &str, php_filename: &str, php_content: &str) {
    let site_dir = sites.join(hostname);
    let _ = std::fs::remove_dir_all(&site_dir);
    std::fs::create_dir_all(&site_dir)
        .unwrap_or_else(|e| panic!("failed to create site dir {}: {e}", site_dir.display()));
    std::fs::write(site_dir.join(php_filename), php_content)
        .unwrap_or_else(|e| panic!("failed to write {php_filename} to {}: {e}", site_dir.display()));
}

/// Remove a site directory, ignoring errors.
fn teardown_site(sites: &PathBuf, hostname: &str) {
    let _ = std::fs::remove_dir_all(sites.join(hostname));
}

// ---------------------------------------------------------------------------
// open_basedir: cross-site reads must be blocked
// ---------------------------------------------------------------------------

#[tokio::test]
async fn open_basedir_blocks_cross_site_reads() {
    let base_url = required_env("EPHPM_URL");
    let sites = sites_dir();

    let host_a = "basedir-a.test";
    let host_b = "basedir-b.test";

    // Deploy site B with a secret file.
    deploy_site(&sites, host_b, "secret.txt", "top-secret-data");

    // Deploy site A with the basedir_test.php script.
    let php = include_str!("../../../tests/docroot/basedir_test.php");
    deploy_site(&sites, host_a, "basedir_test.php", php);

    // Site A tries to read site B's secret file.
    let target_path = sites.join(host_b).join("secret.txt");
    let encoded_path = urlencoding::encode(target_path.to_str().unwrap());
    let (status, body) = get_with_host(
        &base_url,
        host_a,
        &format!("/basedir_test.php?path={encoded_path}"),
    )
    .await;

    assert_eq!(status, 200, "PHP script itself should execute, got {status}");

    let json: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("invalid JSON: {e}\nbody: {body}"));

    assert_eq!(
        json["success"], false,
        "open_basedir must prevent cross-site file reads, got: {body}"
    );
    assert!(
        json["error"]
            .as_str()
            .unwrap_or("")
            .contains("open_basedir"),
        "error message should mention open_basedir restriction, got: {}",
        json["error"]
    );

    teardown_site(&sites, host_a);
    teardown_site(&sites, host_b);
}

// ---------------------------------------------------------------------------
// disable_functions: shell execution must be blocked
// ---------------------------------------------------------------------------

#[tokio::test]
async fn disable_functions_blocks_shell_exec() {
    let base_url = required_env("EPHPM_URL");
    let sites = sites_dir();

    let host = "shell-test.test";
    let php = include_str!("../../../tests/docroot/shell_test.php");
    deploy_site(&sites, host, "shell_test.php", php);

    let (status, body) = get_with_host(&base_url, host, "/shell_test.php").await;

    assert_eq!(status, 200, "PHP script itself should execute, got {status}");

    let json: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("invalid JSON: {e}\nbody: {body}"));

    assert_eq!(
        json["success"], false,
        "exec() must be disabled in multi-tenant mode, got: {body}"
    );

    let error_msg = json["error"].as_str().unwrap_or("");
    assert!(
        error_msg.contains("disabled") || error_msg.contains("has been disabled"),
        "error should indicate function is disabled, got: {error_msg}"
    );

    teardown_site(&sites, host);
}

// ---------------------------------------------------------------------------
// KV SAPI functions: must work even when RESP auth is configured
// ---------------------------------------------------------------------------

#[tokio::test]
async fn kv_sapi_functions_work_with_resp_auth() {
    let base_url = required_env("EPHPM_URL");
    let sites = sites_dir();

    let host = "kv-smoke.test";
    let php = r#"<?php
header('Content-Type: text/plain');
ephpm_kv_set('smoke-test-key', 'smoke-value', 0);
$val = ephpm_kv_get('smoke-test-key');
echo $val;
"#;
    deploy_site(&sites, host, "kv_smoke.php", php);

    let (status, body) = get_with_host(&base_url, host, "/kv_smoke.php").await;

    assert_eq!(status, 200, "KV smoke test should return 200, got {status}");
    assert_eq!(
        body, "smoke-value",
        "KV SAPI set/get must work regardless of RESP auth config, got: {body}"
    );

    // Clean up the KV key and site.
    let cleanup_php = r#"<?php
ephpm_kv_del('smoke-test-key');
echo "ok";
"#;
    std::fs::write(sites.join(host).join("kv_smoke.php"), cleanup_php).unwrap();
    let _ = get_with_host(&base_url, host, "/kv_smoke.php").await;

    teardown_site(&sites, host);
}

// ---------------------------------------------------------------------------
// Multi-tenant KV isolation: sites must not see each other's keys
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multi_tenant_kv_isolation() {
    let base_url = required_env("EPHPM_URL");
    let sites = sites_dir();

    let host_a = "kv-a.test";
    let host_b = "kv-b.test";

    let php = include_str!("../../../tests/docroot/kv_site_test.php");
    deploy_site(&sites, host_a, "kv_site_test.php", php);
    deploy_site(&sites, host_b, "kv_site_test.php", php);

    // Site A sets isolation-key = "from-a"
    let (status_a, body_a) =
        get_with_host(&base_url, host_a, "/kv_site_test.php?site=a").await;
    assert_eq!(status_a, 200, "site A KV test failed with status {status_a}");
    assert_eq!(
        body_a, "from-a",
        "site A should read back its own value, got: {body_a}"
    );

    // Site B sets isolation-key = "from-b"
    let (status_b, body_b) =
        get_with_host(&base_url, host_b, "/kv_site_test.php?site=b").await;
    assert_eq!(status_b, 200, "site B KV test failed with status {status_b}");
    assert_eq!(
        body_b, "from-b",
        "site B should read back its own value (not site A's), got: {body_b}"
    );

    // Re-read site A to confirm it still has its own value (not overwritten by B).
    let (_, body_a2) =
        get_with_host(&base_url, host_a, "/kv_site_test.php?site=a").await;
    assert_eq!(
        body_a2, "from-a",
        "site A's value must not be affected by site B's write, got: {body_a2}"
    );

    teardown_site(&sites, host_a);
    teardown_site(&sites, host_b);
}

// ---------------------------------------------------------------------------
// trusted_hosts: spoofed Host header must be rejected with 421
// ---------------------------------------------------------------------------

#[tokio::test]
async fn trusted_hosts_rejects_spoofed_host() {
    let base_url = required_env("EPHPM_URL");
    let client = reqwest::Client::new();

    // Request with an untrusted Host header.
    let resp = client
        .get(format!("{base_url}/"))
        .header("Host", "evil.example.com")
        .send()
        .await
        .expect("request with spoofed Host failed");

    assert_eq!(
        resp.status().as_u16(),
        421,
        "untrusted Host header must receive 421 Misdirected Request, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn trusted_hosts_allows_valid_host() {
    let base_url = required_env("EPHPM_URL");
    let client = reqwest::Client::new();

    // "ephpm" is in the trusted_hosts list (k8s service name).
    let resp = client
        .get(format!("{base_url}/"))
        .header("Host", "ephpm")
        .send()
        .await
        .expect("request with trusted Host failed");

    let status = resp.status().as_u16();
    assert_ne!(
        status, 421,
        "trusted Host must not receive 421, got {status}"
    );
    assert!(
        status == 200 || status == 404,
        "trusted Host should get 200 or 404, got {status}"
    );
}
