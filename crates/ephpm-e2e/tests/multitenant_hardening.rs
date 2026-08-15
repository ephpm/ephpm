//! Multi-tenant hardening e2e proof.
//!
//! Reproduces the cross-tenant channels a hostile-PHP-userland pentest proved
//! reachable in the shared-process ZTS model, and asserts the
//! `multi_tenant_hardening` preset (default-on in multi-tenant mode) has closed
//! each one: the SysV IPC family, persistent-socket inheritance
//! (`pfsockopen`/`fsockopen`), `posix_kill`/`pcntl_fork` process control, and
//! `opcache_reset`. Also proves the `disable_shell_exec` clobber fix — an
//! operator's own `disable_functions` entry (`getmypid`, injected by the e2e
//! config) stays disabled *alongside* ePHPm's baseline rather than being
//! overwritten by it.
//!
//! Runs on the shared multi-tenant node (`EPHPM_SITES_DIR` set), whose
//! `[server.security]` section makes `multi_tenant_hardening` resolve on.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance
//! - `EPHPM_SITES_DIR` — writable path to the sites directory on the ephpm host

use std::path::PathBuf;

use ephpm_e2e::required_env;

fn sites_dir() -> PathBuf {
    PathBuf::from(required_env("EPHPM_SITES_DIR"))
}

async fn get_with_host(base_url: &str, host: &str, path: &str) -> (u16, String) {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{base_url}{path}"))
        .header("Host", host)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {path} with Host: {host} failed: {e}"));
    let status = resp.status().as_u16();
    let body = resp.text().await.expect("failed to read response body").trim().to_owned();
    (status, body)
}

fn deploy_site(sites: &PathBuf, hostname: &str, php_filename: &str, php_content: &str) {
    let site_dir = sites.join(hostname);
    let _ = std::fs::remove_dir_all(&site_dir);
    std::fs::create_dir_all(&site_dir)
        .unwrap_or_else(|e| panic!("failed to create site dir {}: {e}", site_dir.display()));
    std::fs::write(site_dir.join(php_filename), php_content)
        .unwrap_or_else(|e| panic!("failed to write {php_filename}: {e}"));
}

/// Probe reports `function_exists` for every function of interest as JSON.
const PROBE_PHP: &str = r#"<?php
header('Content-Type: application/json');
$disabled = [
    // SysV IPC — cross-tenant read/write via the kernel IPC namespace.
    'shm_attach', 'shm_get_var', 'shm_put_var',
    'sem_get', 'sem_acquire',
    'msg_get_queue', 'msg_send', 'msg_receive',
    // Persistent-socket inheritance (EG(persistent_list) keyed host:port).
    'pfsockopen', 'fsockopen',
    // Process control against the shared process.
    'posix_kill', 'pcntl_fork',
    // OPcache whole-cache flush.
    'opcache_reset',
    // Module loading + mail relay.
    'dl', 'mail',
    // Operator-supplied entry (clobber-fix proof).
    'getmypid',
];
$still_callable = [
    // Non-persistent socket + ordinary functions must remain available.
    'stream_socket_client', 'strlen', 'preg_match', 'unserialize',
];
$out = ['disabled' => [], 'callable' => []];
foreach ($disabled as $f) { $out['disabled'][$f] = function_exists($f); }
foreach ($still_callable as $f) { $out['callable'][$f] = function_exists($f); }
echo json_encode($out);
"#;

#[tokio::test]
async fn hardening_denylist_closes_proven_channels_and_composes_with_operator_list() {
    let base_url = required_env("EPHPM_URL");
    let sites = sites_dir();
    let host = "harden.test";

    deploy_site(&sites, host, "probe.php", PROBE_PHP);
    let (status, body) = get_with_host(&base_url, host, "/probe.php").await;
    assert_eq!(status, 200, "probe script should execute, got {status}: {body}");

    let json: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("invalid JSON: {e}\nbody: {body}"));

    // Every hardening-denied function (and the operator's getmypid) must be
    // undefined — PHP removes disabled functions from the function table.
    let disabled = json["disabled"].as_object().expect("disabled object");
    for (func, defined) in disabled {
        assert_eq!(
            defined,
            &serde_json::Value::Bool(false),
            "{func} must be disabled in a multi-tenant node, but function_exists() = true",
        );
    }
    // Specifically call out the clobber-fix invariant: BOTH a hardening-baseline
    // function AND the operator's own entry are blocked at once.
    assert_eq!(disabled["shm_attach"], serde_json::Value::Bool(false), "baseline blocked");
    assert_eq!(disabled["getmypid"], serde_json::Value::Bool(false), "operator entry blocked");

    // Functions real apps need (non-persistent sockets, string/regex/serialize)
    // must stay callable — the hardening must not over-reach.
    let callable = json["callable"].as_object().expect("callable object");
    for (func, defined) in callable {
        assert_eq!(
            defined,
            &serde_json::Value::Bool(true),
            "{func} must remain callable (hardening over-reached)",
        );
    }

    let _ = std::fs::remove_dir_all(sites.join(host));
}
