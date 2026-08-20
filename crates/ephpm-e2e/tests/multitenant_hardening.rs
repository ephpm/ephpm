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

// ── Issue #274: cross-tenant RCE via SQLite ATTACH ──────────────────────────
//
// The original pentest (v0.6.3) proved that from a hostile vhost, `ATTACH
// DATABASE` reached the process-global embedded engine *outside* PHP's per-vhost
// `open_basedir`, giving an arbitrary-filesystem read/write primitive:
//   1. read another tenant's `<site>.db`,
//   2. write a PHP shell into another tenant's docroot, then execute it —
//      full cross-tenant RCE — and
//   3. the same `ATTACH` worked through the always-on native `ephpm_db_*`
//      bridge, so firewalling the TCP proxy did not close it.
//
// v0.7.0 hardened this on BOTH tenant routes: `screen_sql` (shared by the
// `ephpm_db_*` bridge and the wire path's `ScreenedBackend`) rejects
// `ATTACH`/`DETACH`/`VACUUM` and path-bearing `PRAGMA`s before they reach the
// engine, and the MySQL wire listener verifies a per-site credential before it
// resolves any backend. This test pins all of that end to end, from real PHP
// userland in multi-site mode, so #274 cannot silently regress.

/// Victim tenant: seeds a canary row into its OWN per-site database. Running
/// this creates the victim's `<key>.db` file the attacker then tries to reach.
const VICTIM_SEED_PHP: &str = r#"<?php
header('Content-Type: application/json');
try {
    ephpm_db_execute("DROP TABLE IF EXISTS secrets");
    ephpm_db_execute("CREATE TABLE secrets (v TEXT)");
    ephpm_db_execute("INSERT INTO secrets (v) VALUES ('CROSS_TENANT_CANARY')");
    echo json_encode(['ok' => true]);
} catch (Throwable $e) {
    echo json_encode(['ok' => false, 'err' => $e->getMessage()]);
}
"#;

/// Attacker tenant: attempts the #274 primitive on both tenant routes and
/// reports the outcome of each as JSON. `vdb`/`shell` arrive hex-encoded so the
/// absolute paths survive the query string unmangled.
const ATTACKER_PHP: &str = r#"<?php
header('Content-Type: application/json');
$victim_db = @hex2bin($_GET['vdb'] ?? '');
$shell     = @hex2bin($_GET['shell'] ?? '');
$out = [];

// VECTOR A — native bridge ATTACH + cross-tenant read.
try {
    ephpm_db_execute("ATTACH DATABASE '$victim_db' AS victim");
    try {
        $rows = ephpm_db_query("SELECT v FROM victim.secrets");
        $out['native_read'] = ['result' => 'EXPLOITABLE', 'stolen' => $rows];
    } catch (Throwable $e2) {
        $out['native_read'] = ['result' => 'attach_ok_select_failed', 'err' => $e2->getMessage()];
    }
} catch (Throwable $e) {
    $out['native_read'] = ['result' => 'BLOCKED', 'err' => $e->getMessage()];
}

// VECTOR B — native bridge ATTACH write a shell into the victim's docroot.
// PHP's own write there must already be denied by open_basedir.
$out['php_write_denied'] = (@file_put_contents($shell, 'x') === false);
@unlink($shell);
try {
    ephpm_db_execute("ATTACH DATABASE '$shell' AS pwn");
    ephpm_db_execute("CREATE TABLE pwn.x (c TEXT)");
    $out['native_write'] = ['result' => 'EXPLOITABLE', 'planted' => file_exists($shell)];
} catch (Throwable $e) {
    $out['native_write'] = ['result' => 'BLOCKED', 'err' => $e->getMessage()];
}

// VECTOR C — pdo_mysql wire: junk creds, name-claim, own-creds ATTACH.
if (function_exists('mysqli_connect')) {
    mysqli_report(MYSQLI_REPORT_OFF);
    $host = $_SERVER['DB_HOST'] ?? '127.0.0.1';
    $port = (int)($_SERVER['DB_PORT'] ?? 3306);
    $user = $_SERVER['DB_USER'] ?? '';
    $pass = $_SERVER['DB_PASSWORD'] ?? '';

    $junk = @mysqli_connect($host, $user, 'definitely-the-wrong-password', '', $port);
    $out['wire_junk_denied'] = ($junk === false || $junk === null);
    if ($junk) { @mysqli_close($junk); }

    $conn = @mysqli_connect($host, $user, $pass, '', $port);
    if ($conn) {
        $out['wire_own_conn'] = true;
        $r = @mysqli_query($conn, "ATTACH DATABASE '$victim_db' AS victim");
        if ($r === false) {
            $out['wire_attach'] = ['result' => 'BLOCKED', 'err' => mysqli_error($conn)];
        } else {
            $res = @mysqli_query($conn, "SELECT v FROM victim.secrets");
            $stolen = $res ? mysqli_fetch_all($res, MYSQLI_ASSOC) : null;
            $out['wire_attach'] = ['result' => 'EXPLOITABLE', 'stolen' => $stolen];
        }
        @mysqli_close($conn);
    } else {
        $out['wire_own_conn'] = false;
    }
} else {
    $out['wire_skipped'] = true;
}

echo json_encode($out);
"#;

/// Hex-encode so an absolute filesystem path rides the query string intact.
fn hex(s: &str) -> String {
    use std::fmt::Write as _;
    s.bytes().fold(String::with_capacity(s.len() * 2), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

#[tokio::test]
async fn cross_tenant_attach_is_rejected_on_native_and_wire_paths() {
    let base_url = required_env("EPHPM_URL");
    let sites = sites_dir();
    let victim = "basedir-a.test";
    let attacker = "basedir-b.test";

    // Per-site database files live at `<node>/data/sites-db/<key>.db`, a sibling
    // of the sites dir (`<node>/sites`). Derive the victim's real file and a
    // real path inside its docroot, so a *regression* (screen removed) would
    // genuinely leak the canary / plant a shell rather than error on a bogus
    // path.
    let node_dir = sites.parent().expect("sites dir has a parent");
    let victim_db = node_dir.join("data").join("sites-db").join(format!("{victim}.db"));
    let shell = sites.join(victim).join("planted_shell.php");

    deploy_site(&sites, victim, "seed.php", VICTIM_SEED_PHP);
    deploy_site(&sites, attacker, "attack.php", ATTACKER_PHP);

    // Victim seeds a canary into ITS OWN database (also creates the .db file).
    let (status, body) = get_with_host(&base_url, victim, "/seed.php").await;
    assert_eq!(status, 200, "victim seed should execute, got {status}: {body}");
    let seed: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("seed JSON: {e}\nbody: {body}"));
    assert_eq!(seed["ok"], serde_json::Value::Bool(true), "victim seed failed: {body}");

    let path = format!(
        "/attack.php?vdb={}&shell={}",
        hex(victim_db.to_str().unwrap()),
        hex(shell.to_str().unwrap())
    );
    let (status, body) = get_with_host(&base_url, attacker, &path).await;
    assert_eq!(status, 200, "attacker probe should execute, got {status}: {body}");
    let j: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("attacker JSON: {e}\nbody: {body}"));

    // VECTOR A — native bridge ATTACH must not read another tenant's data.
    assert_eq!(
        j["native_read"]["result"], "BLOCKED",
        "native ephpm_db_* ATTACH read must be BLOCKED (#274): {body}"
    );

    // VECTOR B — native bridge ATTACH must not plant a file in another docroot,
    // and PHP's own write there must still be denied by open_basedir.
    assert_eq!(
        j["native_write"]["result"], "BLOCKED",
        "native ephpm_db_* ATTACH write must be BLOCKED (#274): {body}"
    );
    assert_eq!(
        j["php_write_denied"],
        serde_json::Value::Bool(true),
        "open_basedir must still deny PHP's own write into another tenant's docroot: {body}"
    );
    assert!(!shell.exists(), "no file may be planted in the victim docroot: {}", shell.display());

    // VECTOR C — wire path: junk creds denied, own creds connect, ATTACH refused.
    if j.get("wire_skipped").is_none() {
        assert_eq!(
            j["wire_junk_denied"],
            serde_json::Value::Bool(true),
            "junk credentials must be denied on the pdo_mysql wire path: {body}"
        );
        assert_eq!(
            j["wire_own_conn"],
            serde_json::Value::Bool(true),
            "a site's own injected credentials must connect to its own database: {body}"
        );
        assert_eq!(
            j["wire_attach"]["result"], "BLOCKED",
            "pdo_mysql ATTACH must be BLOCKED (#274): {body}"
        );
    }

    // Belt and suspenders: the canary must not appear anywhere in the response.
    assert!(
        !body.contains("CROSS_TENANT_CANARY"),
        "victim canary leaked to the attacker — cross-tenant read succeeded: {body}"
    );

    let _ = std::fs::remove_dir_all(sites.join(victim));
    let _ = std::fs::remove_dir_all(sites.join(attacker));
}
