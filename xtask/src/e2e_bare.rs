//! Bare-process E2E test driver — the default `cargo xtask e2e`.
//!
//! Spawns ephpm binaries on 127.0.0.1 (single node or 3-node cluster) with
//! per-node scratch data dirs and per-node config files, waits for the health
//! endpoint on each, then runs the pre-built `ephpm-e2e` test binaries against
//! those endpoints. No Kind, no Tilt, no privileged DinD.
//!
//! Design notes:
//!
//! - We intentionally do not depend on the `ephpm-e2e` crate from xtask — that
//!   crate is workspace-excluded and pulls in tokio/reqwest. xtask stays deps-
//!   free; polling `/_ephpm/health` and running child processes uses only
//!   `std`. Test binaries themselves depend on tokio/reqwest, but xtask only
//!   invokes them via `cargo test --no-run` + direct exec.
//!
//! - Cluster tests are gated by name — anything under `crates/ephpm-e2e/tests/
//!   cluster.rs` (and future cluster suites) needs 3 nodes; single-node tests
//!   just need one. The env vars set here mirror what the old Kind harness
//!   exported so existing test files don't need to change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};
use std::{fs, io, thread};

use crate::{DEFAULT_PHP_MINOR, release, workspace_root};

/// Test suites that need a 3-node cluster (rather than a single node).
///
/// Matched against the test binary's stem — e.g. the binary
/// `crates/ephpm-e2e/target/debug/deps/cluster-abc123` has stem `cluster`
/// and gets the cluster fixture. Everything else runs against a single node.
const CLUSTER_SUITES: &[&str] = &["cluster"];

/// Test suites that must be excluded from bare-process runs entirely.
///
/// A few historical suites were Kind-only (they poke Kubernetes services,
/// ClusterIP round-robin, etc.). They still work under `cargo xtask k8s-e2e`
/// — bare-process just skips them. Currently empty; the plan is to migrate
/// or gate them individually.
const SKIP_SUITES: &[&str] = &[];

pub fn run(args: &[String]) -> ExitCode {
    if args.iter().any(|a| a == "--help" || a == "-h" || a == "help") {
        print_usage();
        return ExitCode::SUCCESS;
    }

    let php_version = parse_php_version(args).to_string();
    let ephpm_bin = match resolve_ephpm_binary(args, &php_version) {
        Some(p) => p,
        None => return ExitCode::FAILURE,
    };
    let only_suite = parse_named_flag(args, "--tests");

    let root = workspace_root();
    let docroot = root.join("tests").join("docroot");
    if !docroot.exists() {
        eprintln!("error: docroot not found at {}", docroot.display());
        return ExitCode::FAILURE;
    }
    let docroot = match docroot.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: failed to canonicalize {}: {e}", docroot.display());
            return ExitCode::FAILURE;
        }
    };

    let scratch_root = root.join("target").join("e2e-bare");
    if let Err(e) = recreate_dir(&scratch_root) {
        eprintln!("error: failed to prepare {}: {e}", scratch_root.display());
        return ExitCode::FAILURE;
    }

    // Compile the e2e test binaries. `--no-run` builds every test target
    // without executing anything, then we pick them up out of
    // crates/ephpm-e2e/target/debug/deps/ by stem name.
    eprintln!("==> Building ephpm-e2e test binaries...");
    let e2e_manifest = root.join("crates").join("ephpm-e2e").join("Cargo.toml");
    let status = Command::new("cargo")
        .args(["test", "--manifest-path"])
        .arg(&e2e_manifest)
        .args(["--no-run", "--tests"])
        .status();
    if !matches!(&status, Ok(s) if s.success()) {
        eprintln!("error: cargo test --no-run failed for ephpm-e2e");
        return ExitCode::FAILURE;
    }

    let deps_dir = root.join("crates").join("ephpm-e2e").join("target").join("debug").join("deps");
    let binaries = match discover_test_binaries(&deps_dir) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: failed to discover test binaries: {e}");
            return ExitCode::FAILURE;
        }
    };

    if binaries.is_empty() {
        eprintln!("error: no ephpm-e2e test binaries found under {}", deps_dir.display());
        return ExitCode::FAILURE;
    }

    // Split binaries by whether they need cluster mode.
    let mut single_suites: Vec<(String, PathBuf)> = Vec::new();
    let mut cluster_suites: Vec<(String, PathBuf)> = Vec::new();
    for (name, path) in binaries {
        if SKIP_SUITES.contains(&name.as_str()) {
            continue;
        }
        if let Some(only) = only_suite.as_deref() {
            if name != only {
                continue;
            }
        }
        if CLUSTER_SUITES.contains(&name.as_str()) {
            cluster_suites.push((name, path));
        } else {
            single_suites.push((name, path));
        }
    }

    if single_suites.is_empty() && cluster_suites.is_empty() {
        eprintln!(
            "error: no test suites selected (--tests {:?} filtered everything out)",
            only_suite
        );
        return ExitCode::FAILURE;
    }

    let mut failed: Vec<String> = Vec::new();

    if !single_suites.is_empty() {
        eprintln!(
            "==> Running {} single-node suite(s) against a bare ephpm on 127.0.0.1...",
            single_suites.len()
        );
        let fixture = match SingleNodeSpawn::start(&ephpm_bin, &docroot, &scratch_root) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("error: failed to start single-node fixture: {e}");
                return ExitCode::FAILURE;
            }
        };
        let env = fixture.env(&php_version);
        for (name, path) in &single_suites {
            if !run_suite(name, path, &env) {
                failed.push(name.clone());
            }
        }
        fixture.dump_on_failure(&failed);
        drop(fixture);
    }

    if !cluster_suites.is_empty() {
        eprintln!(
            "==> Running {} cluster suite(s) against a 3-node bare ephpm cluster on 127.0.0.1...",
            cluster_suites.len()
        );
        let fixture = match ClusterSpawn::start(&ephpm_bin, &docroot, &scratch_root, 3) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("error: failed to start cluster fixture: {e}");
                return ExitCode::FAILURE;
            }
        };
        let env = fixture.env(&php_version);
        for (name, path) in &cluster_suites {
            if !run_suite(name, path, &env) {
                failed.push(name.clone());
            }
        }
        fixture.dump_on_failure(&failed);
        drop(fixture);
    }

    if failed.is_empty() {
        eprintln!("==> All bare-process E2E suites passed");
        ExitCode::SUCCESS
    } else {
        eprintln!("==== FAILED E2E SUITES (bare-process) ====");
        for name in &failed {
            eprintln!("  {name}");
        }
        eprintln!("==== end failed suites ====");
        ExitCode::FAILURE
    }
}

fn print_usage() {
    eprintln!(
        "\
Usage: cargo xtask e2e [options]

Runs the ephpm-e2e test binaries against bare ephpm processes spawned on
127.0.0.1 — no Kind, no Tilt, no privileged DinD.

Options:
  --php-version <ver>     PHP version to use (default: {DEFAULT_PHP_MINOR}).
                          Used only when a release build has to be triggered.
  --ephpm-binary <path>   Use an existing ephpm binary (default:
                          target/release/ephpm, built via `cargo xtask release`
                          if it does not exist).
  --tests <suite>         Run only the named test binary (e.g. `basic`, `kv`,
                          `cluster`) — useful for iteration.
  --help                  Print this message.

For opt-in K8s deployment testing (Kind + Tilt), use `cargo xtask k8s-e2e`."
    );
}

fn parse_php_version(args: &[String]) -> &str {
    parse_named_flag_ref(args, "--php-version").unwrap_or(DEFAULT_PHP_MINOR)
}

fn parse_named_flag(args: &[String], name: &str) -> Option<String> {
    parse_named_flag_ref(args, name).map(str::to_string)
}

fn parse_named_flag_ref<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    for (i, arg) in args.iter().enumerate() {
        if arg == name {
            return args.get(i + 1).map(String::as_str);
        }
    }
    None
}

/// Locate an ephpm binary to spawn.
///
/// Precedence:
/// 1. `--ephpm-binary <path>` explicit override
/// 2. `target/release/ephpm` if it already exists
/// 3. Build it via `crate::release(...)` and then use `target/release/ephpm`
fn resolve_ephpm_binary(args: &[String], php_version: &str) -> Option<PathBuf> {
    if let Some(path) = parse_named_flag_ref(args, "--ephpm-binary") {
        let abs = PathBuf::from(path);
        if !abs.exists() {
            eprintln!("error: --ephpm-binary path does not exist: {}", abs.display());
            return None;
        }
        return Some(abs);
    }

    let root = workspace_root();
    let bin_name = if cfg!(windows) { "ephpm.exe" } else { "ephpm" };

    // The release build lands under target/<triple>/release/ephpm, but the
    // top-level target/release/ephpm symlink is what people typically look for.
    let candidates = [
        root.join("target").join("release").join(bin_name),
        root.join("target")
            .join(format!("{}-unknown-linux-gnu", std::env::consts::ARCH))
            .join("release")
            .join(bin_name),
        root.join("target")
            .join(format!("{}-apple-darwin", std::env::consts::ARCH))
            .join("release")
            .join(bin_name),
    ];
    for c in &candidates {
        if c.exists() {
            eprintln!("==> Using existing ephpm binary at {}", c.display());
            return Some(c.clone());
        }
    }

    eprintln!("==> No release binary found — building via `cargo xtask release`...");
    let release_args = ["--php-version".to_string(), php_version.to_string()];
    let code = release(&release_args);
    // Can't inspect ExitCode directly; re-check the candidates.
    for c in &candidates {
        if c.exists() {
            return Some(c.clone());
        }
    }
    eprintln!("error: release build did not produce an ephpm binary (exit={code:?})");
    None
}

/// Enumerate `<deps>/<suite>-<hash>[.exe]`, keeping only the newest binary per
/// suite name.
fn discover_test_binaries(deps: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let mut by_stem: BTreeMap<String, (PathBuf, std::time::SystemTime)> = BTreeMap::new();

    for entry in fs::read_dir(deps)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // Skip .d, .rlib, .rmeta, .pdb, etc. — only keep executables.
        let ext = path.extension().and_then(|s| s.to_str());
        let is_exe = matches!(ext, None | Some("exe"));
        if !is_exe {
            continue;
        }
        // deps files look like `<crate>-<16 hex>`. Split off the hash.
        let Some(sep) = stem.rfind('-') else {
            continue;
        };
        let (name, hash) = stem.split_at(sep);
        if hash.len() < 2 || !hash[1..].chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        // Skip the ephpm_e2e library test binary itself (only integration
        // tests under tests/*.rs matter here — they compile as separate
        // binaries and produce the per-suite deps files we want).
        if name == "ephpm_e2e" {
            continue;
        }
        let mtime = entry.metadata()?.modified()?;
        by_stem
            .entry(name.to_string())
            .and_modify(|(existing, existing_mtime)| {
                if mtime > *existing_mtime {
                    *existing = path.clone();
                    *existing_mtime = mtime;
                }
            })
            .or_insert((path, mtime));
    }

    Ok(by_stem.into_iter().map(|(name, (path, _))| (name, path)).collect())
}

fn run_suite(name: &str, binary: &Path, env: &[(String, String)]) -> bool {
    eprintln!("==> Running suite: {name}");
    let mut cmd = Command::new(binary);
    cmd.args(["--test-threads=4"]);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let status = cmd.status();
    let ok = matches!(&status, Ok(s) if s.success());
    if !ok {
        eprintln!("==> Suite {name} FAILED (exit: {status:?})");
    }
    ok
}

fn recreate_dir(path: &Path) -> io::Result<()> {
    if path.exists() {
        fs::remove_dir_all(path)?;
    }
    fs::create_dir_all(path)
}

// ── config templates ────────────────────────────────────────────────────────

/// Single-node ephpm.toml template. Placeholders: `{HTTP_PORT}`, `{MYSQL_PORT}`,
/// `{HRANA_PORT}`, `{PG_PORT}`, `{TDS_PORT}`, `{DATA_DIR}`, `{DOCROOT}`,
/// `{SITES_DIR}`, `{KV_SOCKET}`.
///
/// Shape mirrors `tests/ephpm-test.toml` (the config baked into the container
/// image) as closely as possible. The only intentional divergences are:
/// (a) `listen`/`sites_dir`/`document_root`/`db.sqlite.path`/`kv.redis_compat.socket`
/// point at per-run scratch paths instead of the container's fixed
/// `/var/www/...` and `/tmp/...` layout; (b) the HTTP `listen` port is a high
/// loopback port so a single-node fixture can coexist with the cluster fixture
/// on the same host.
///
/// The DB proxy listeners deliberately keep the SAME ports as the reference
/// config (mysql 3306, hrana 8081, postgres 5432, tds 1433). The docroot PHP
/// scripts (`db.php`, `sqlite_test.php`, `rw_split_test.php`, ...) connect via
/// `pdo_mysql` to a hardcoded `127.0.0.1:3306` fallback -- `[db.sqlite]` mode
/// does NOT inject `DB_HOST`/`DB_PORT` into PHP (only `[db.mysql]`/`[db.postgres]`
/// do, see `build_db_env_vars` in ephpm-server/src/router.rs). Moving the mysql
/// proxy off 3306 makes every SQLite write 500 with a connection-refused. A
/// single node owns the loopback here, so the standard ports do not collide.
const SINGLE_NODE_TEMPLATE: &str = r#"# Auto-generated by `cargo xtask e2e` -- do not edit.
[server]
listen = "127.0.0.1:{HTTP_PORT}"
document_root = "{DOCROOT}"
sites_dir = "{SITES_DIR}"
index_files = ["index.php", "index.html"]
fallback = ["$uri", "$uri/", "=404"]

[server.request]
max_body_size = 1024
max_header_size = 4096
trusted_hosts = [
    "ephpm", "localhost", "127.0.0.1", "127.0.0.1:{HTTP_PORT}",
    "nonexistent-site.example.com",
    "lazy-test.preview.ephpm.dev",
    "site-a.preview.ephpm.dev",
    "site-b.preview.ephpm.dev",
    "basedir-a.test",
    "basedir-b.test",
    "shell-test.test",
    "kv-smoke.test",
    "kv-a.test",
    "kv-b.test",
]

[server.response]
compression_min_size = 1024
headers = [
    ["X-Frame-Options", "DENY"],
    ["X-Content-Type-Options", "nosniff"],
    ["Strict-Transport-Security", "max-age=31536000; includeSubDomains"],
]

[server.static]
cache_control = "public, max-age=3600"

[server.file_cache]
enabled = true

[server.metrics]
enabled = true

# ETag cache is off by default; the etag_cache suite needs it ON. The container
# path sets this via `-e EPHPM_SERVER__PHP_ETAG_CACHE__ENABLED=true` on the
# server (podman run), so bake it into the server config here rather than the
# test-binary environment (where it would be a no-op).
[server.php_etag_cache]
enabled = true

[server.security]
blocked_paths = ["/vendor/*"]
open_basedir = true
disable_shell_exec = true

[php]
max_execution_time = 30
memory_limit = "128M"
ini_overrides = [
    ["display_errors", "On"],
    ["error_reporting", "E_ALL"],
]

[server.limits]
max_connections = 100
per_ip_rate = 500.0
per_ip_burst = 100

[db.sqlite]
path = "{DATA_DIR}/ephpm-test.db"

[db.sqlite.proxy]
mysql_listen = "127.0.0.1:{MYSQL_PORT}"
hrana_listen = "127.0.0.1:{HRANA_PORT}"
postgres_listen = "127.0.0.1:{PG_PORT}"
tds_listen = "127.0.0.1:{TDS_PORT}"

[server.timeouts]
header_read = 20
idle = 45
request = 5

[db.read_write_split]
enabled = true
sticky_duration = "2s"

[kv]
memory_limit = "64MB"
eviction_policy = "allkeys-lru"

[kv.redis_compat]
enabled = true
socket = "{KV_SOCKET}"
"#;

/// Cluster node template. Placeholders: `{HTTP_PORT}`, `{MYSQL_PORT}`,
/// `{GOSSIP_PORT}`, `{GRPC_PORT}`, `{KV_DATA_PORT}`, `{NODE_ID}`,
/// `{CLUSTER_BIND}`, `{CLUSTER_JOIN}`, `{DATA_DIR}`, `{DOCROOT}`.
///
/// Shape mirrors `cluster-e2e/configs/mode-sqld/node-a.toml`, but every address
/// is `127.0.0.1:PORT` because all nodes share the host loopback instead of
/// having their own container IP.
const CLUSTER_NODE_TEMPLATE: &str = r#"# Auto-generated by `cargo xtask e2e` — do not edit.
[server]
listen = "127.0.0.1:{HTTP_PORT}"
document_root = "{DOCROOT}"
index_files = ["index.php", "index.html"]

[server.request]
trusted_hosts = ["ephpm-a", "ephpm-b", "ephpm-c", "127.0.0.1", "localhost", "127.0.0.1:{HTTP_PORT}"]

[server.metrics]
enabled = true

[php]
mode = "fpm"
max_execution_time = 60
memory_limit = "256M"

[db.sqlite]
path = "{DATA_DIR}/wordpress.db"

[db.sqlite.proxy]
mysql_listen = "127.0.0.1:{MYSQL_PORT}"

[db.sqlite.sqld]
http_listen = "127.0.0.1:{SQLD_HTTP_PORT}"
grpc_listen = "127.0.0.1:{GRPC_PORT}"

[db.sqlite.replication]
role = "auto"

[cluster]
enabled = true
bind = "{CLUSTER_BIND}"
join = [{CLUSTER_JOIN}]
node_id = "{NODE_ID}"
cluster_id = "ephpm-e2e-bare"
secret = "bare-e2e-secret-do-not-use-in-prod-b7e4d3f0"

[cluster.kv]
data_port = {KV_DATA_PORT}
"#;

// ── single-node fixture ─────────────────────────────────────────────────────

struct SingleNodeSpawn {
    child: Child,
    http_port: u16,
    _data_dir: PathBuf,
    sites_dir: PathBuf,
    stderr_log: PathBuf,
    stdout_log: PathBuf,
}

impl SingleNodeSpawn {
    fn start(binary: &Path, docroot: &Path, scratch_root: &Path) -> io::Result<Self> {
        let http_port: u16 = 18100;
        // DB proxy listeners keep the reference-config ports. The docroot PHP
        // scripts connect to a hardcoded 127.0.0.1:3306 fallback (single-node
        // [db.sqlite] does not inject DB_HOST/DB_PORT), so the mysql proxy must
        // own 3306 or every SQLite write 500s. A single node owns the loopback,
        // so these do not collide with anything.
        let mysql_port: u16 = 3306;
        let hrana_port: u16 = 8081;
        let pg_port: u16 = 5432;
        let tds_port: u16 = 1433;

        let node_dir = scratch_root.join("single");
        fs::create_dir_all(&node_dir)?;
        let data_dir = node_dir.join("data");
        fs::create_dir_all(&data_dir)?;
        // Provision the sites_dir the vhost/security suites deploy into. The
        // tests create/remove per-host site dirs themselves at runtime; we just
        // guarantee the parent exists and is writable (mirrors /var/www/sites in
        // the container image).
        let sites_dir = node_dir.join("sites");
        fs::create_dir_all(&sites_dir)?;
        // Unix-domain socket for [kv.redis_compat] (matches the reference
        // config's /tmp/ephpm-kv.sock). Kept short to stay under the ~104-char
        // sun_path limit on Linux.
        let kv_socket = node_dir.join("kv.sock");

        let config = SINGLE_NODE_TEMPLATE
            .replace("{HTTP_PORT}", &http_port.to_string())
            .replace("{MYSQL_PORT}", &mysql_port.to_string())
            .replace("{HRANA_PORT}", &hrana_port.to_string())
            .replace("{PG_PORT}", &pg_port.to_string())
            .replace("{TDS_PORT}", &tds_port.to_string())
            .replace("{DATA_DIR}", &escape_toml(&data_dir))
            .replace("{SITES_DIR}", &escape_toml(&sites_dir))
            .replace("{KV_SOCKET}", &escape_toml(&kv_socket))
            .replace("{DOCROOT}", &escape_toml(docroot));

        let config_path = node_dir.join("ephpm.toml");
        fs::write(&config_path, config)?;

        let stdout_log = node_dir.join("stdout.log");
        let stderr_log = node_dir.join("stderr.log");
        let stdout_file = fs::File::create(&stdout_log)?;
        let stderr_file = fs::File::create(&stderr_log)?;

        eprintln!(
            "    spawning single-node ephpm on 127.0.0.1:{http_port} (config: {})",
            config_path.display()
        );
        let child = Command::new(binary)
            .args(["serve", "--config"])
            .arg(&config_path)
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file))
            .spawn()?;

        wait_for_health(http_port, Duration::from_secs(10))?;
        eprintln!("    single-node ephpm ready on 127.0.0.1:{http_port}");

        Ok(Self { child, http_port, _data_dir: data_dir, sites_dir, stderr_log, stdout_log })
    }

    fn env(&self, php_version: &str) -> Vec<(String, String)> {
        // NOTE: EPHPM_SERVER__PHP_ETAG_CACHE__ENABLED is deliberately NOT set
        // here. Config env overrides only take effect on the ephpm *server*
        // process; setting it on the test binary is a no-op. The etag cache is
        // enabled in the server config template instead (see
        // SINGLE_NODE_TEMPLATE / [server.php_etag_cache]).
        vec![
            ("EPHPM_URL".to_string(), format!("http://127.0.0.1:{}", self.http_port)),
            ("EXPECTED_PHP_VERSION".to_string(), php_version.to_string()),
            // vhost/security_p0 suites deploy per-host site dirs into this path.
            ("EPHPM_SITES_DIR".to_string(), self.sites_dir.to_string_lossy().into_owned()),
        ]
    }

    fn dump_on_failure(&self, failed: &[String]) {
        if failed.is_empty() {
            return;
        }
        dump_log("single-node stderr", &self.stderr_log);
        dump_log("single-node stdout (last 200 lines)", &self.stdout_log);
    }
}

impl Drop for SingleNodeSpawn {
    fn drop(&mut self) {
        terminate(&mut self.child);
    }
}

// ── cluster fixture ────────────────────────────────────────────────────────

struct ClusterSpawn {
    nodes: Vec<ClusterNode>,
}

struct ClusterNode {
    child: Child,
    http_port: u16,
    stderr_log: PathBuf,
    stdout_log: PathBuf,
}

impl ClusterSpawn {
    fn start(binary: &Path, docroot: &Path, scratch_root: &Path, size: usize) -> io::Result<Self> {
        assert!(size >= 2, "cluster fixture needs at least 2 nodes");

        let node_defs: Vec<_> = (0..size)
            .map(|i| {
                let i16 = u16::try_from(i).expect("cluster size fits in u16");
                (
                    i,
                    18100 + i16, // HTTP
                    13306 + i16, // MySQL
                    18200 + i16, // gossip
                    18300 + i16, // sqld gRPC
                    18400 + i16, // sqld HTTP
                    7947 + i16,  // kv data
                )
            })
            .collect();

        // Every node lists all peers as join seeds (chitchat de-dupes; giving
        // it all peers up-front means startup order does not matter).
        let join_addrs: Vec<String> = node_defs
            .iter()
            .map(|(_, _, _, gossip, _, _, _)| format!("127.0.0.1:{gossip}"))
            .collect();

        let mut nodes = Vec::with_capacity(size);

        for (i, http_port, mysql_port, gossip_port, grpc_port, sqld_http_port, kv_data_port) in
            &node_defs
        {
            let node_dir = scratch_root.join(format!("cluster-node-{i}"));
            fs::create_dir_all(&node_dir)?;
            let data_dir = node_dir.join("data");
            fs::create_dir_all(&data_dir)?;

            let cluster_join = join_addrs
                .iter()
                .enumerate()
                .filter(|(j, _)| j != i)
                .map(|(_, a)| format!("\"{a}\""))
                .collect::<Vec<_>>()
                .join(", ");

            let config = CLUSTER_NODE_TEMPLATE
                .replace("{HTTP_PORT}", &http_port.to_string())
                .replace("{MYSQL_PORT}", &mysql_port.to_string())
                .replace("{GOSSIP_PORT}", &gossip_port.to_string())
                .replace("{GRPC_PORT}", &grpc_port.to_string())
                .replace("{SQLD_HTTP_PORT}", &sqld_http_port.to_string())
                .replace("{KV_DATA_PORT}", &kv_data_port.to_string())
                .replace("{NODE_ID}", &format!("bare-node-{i}"))
                .replace("{CLUSTER_BIND}", &format!("127.0.0.1:{gossip_port}"))
                .replace("{CLUSTER_JOIN}", &cluster_join)
                .replace("{DATA_DIR}", &escape_toml(&data_dir))
                .replace("{DOCROOT}", &escape_toml(docroot));

            let config_path = node_dir.join("ephpm.toml");
            fs::write(&config_path, config)?;

            let stdout_log = node_dir.join("stdout.log");
            let stderr_log = node_dir.join("stderr.log");
            let stdout_file = fs::File::create(&stdout_log)?;
            let stderr_file = fs::File::create(&stderr_log)?;

            eprintln!(
                "    spawning cluster node {i} on 127.0.0.1:{http_port} (config: {})",
                config_path.display()
            );
            let child = Command::new(binary)
                .args(["serve", "--config"])
                .arg(&config_path)
                .stdout(Stdio::from(stdout_file))
                .stderr(Stdio::from(stderr_file))
                .spawn()?;

            nodes.push(ClusterNode { child, http_port: *http_port, stderr_log, stdout_log });
        }

        for node in &nodes {
            wait_for_health(node.http_port, Duration::from_secs(15))?;
        }
        eprintln!("    all {size} cluster nodes ready");

        Ok(Self { nodes })
    }

    fn env(&self, php_version: &str) -> Vec<(String, String)> {
        // EPHPM_SERVER__PHP_ETAG_CACHE__ENABLED is intentionally omitted: env
        // config overrides only apply to the ephpm server process, not the test
        // binary, and no cluster suite exercises the etag cache anyway.
        let mut env = vec![
            ("EPHPM_URL".to_string(), format!("http://127.0.0.1:{}", self.nodes[0].http_port)),
            ("EPHPM_CLUSTER_SIZE".to_string(), self.nodes.len().to_string()),
            ("EXPECTED_PHP_VERSION".to_string(), php_version.to_string()),
        ];
        for (i, node) in self.nodes.iter().enumerate() {
            env.push((
                format!("EPHPM_CLUSTER_URL_{i}"),
                format!("http://127.0.0.1:{}", node.http_port),
            ));
        }
        env
    }

    fn dump_on_failure(&self, failed: &[String]) {
        if failed.is_empty() {
            return;
        }
        for (i, node) in self.nodes.iter().enumerate() {
            dump_log(&format!("cluster node {i} stderr"), &node.stderr_log);
            dump_log(&format!("cluster node {i} stdout (last 200 lines)"), &node.stdout_log);
        }
    }
}

impl Drop for ClusterSpawn {
    fn drop(&mut self) {
        for node in &mut self.nodes {
            terminate(&mut node.child);
        }
    }
}

// ── helpers ────────────────────────────────────────────────────────────────

fn wait_for_health(port: u16, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last_err = String::from("no attempts made");
    while Instant::now() < deadline {
        match tcp_get(port, "/_ephpm/health") {
            Ok(200) => return Ok(()),
            Ok(status) => last_err = format!("health returned HTTP {status}"),
            Err(e) => last_err = format!("connect error: {e}"),
        }
        thread::sleep(Duration::from_millis(200));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("ephpm on 127.0.0.1:{port} did not become healthy within {timeout:?}: {last_err}"),
    ))
}

/// Minimal HTTP/1.0 GET — returns the status code. Avoids pulling reqwest
/// into xtask just for a health check.
fn tcp_get(port: u16, path: &str) -> io::Result<u16> {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let mut stream = TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        Duration::from_secs(1),
    )?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    let req = format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes())?;

    let mut buf = [0u8; 128];
    let n = stream.read(&mut buf)?;
    if n == 0 {
        return Err(io::Error::other("empty response"));
    }
    // Expect "HTTP/1.x NNN ..."
    let first_line = std::str::from_utf8(&buf[..n]).unwrap_or("").lines().next().unwrap_or("");
    let code = first_line.split_whitespace().nth(1).unwrap_or("0");
    code.parse::<u16>().map_err(io::Error::other)
}

fn terminate(child: &mut Child) {
    // Try graceful first (SIGTERM on unix, TerminateProcess on windows via kill).
    #[cfg(unix)]
    {
        // SAFETY: libc::kill takes the raw pid and a signal; both are valid.
        // A dead child returns ESRCH, which we ignore.
        unsafe {
            let pid = child.id() as i32;
            libc_kill(pid, 15 /* SIGTERM */);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() >= deadline => break,
                Ok(None) => thread::sleep(Duration::from_millis(100)),
                Err(_) => break,
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

// Tiny inline binding so we don't pull the `libc` crate into xtask.
#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
#[cfg(unix)]
#[inline]
unsafe fn libc_kill(pid: i32, sig: i32) -> i32 {
    // SAFETY: forwarded to libc — see terminate() for invariants.
    unsafe { kill(pid, sig) }
}

fn dump_log(label: &str, path: &Path) {
    eprintln!("--- {label} ({}) ---", path.display());
    match fs::read_to_string(path) {
        Ok(contents) => {
            for line in contents.lines().rev().take(200).collect::<Vec<_>>().iter().rev() {
                eprintln!("{line}");
            }
        }
        Err(e) => eprintln!("(failed to read log: {e})"),
    }
    eprintln!("--- end {label} ---");
}

/// Escape a filesystem path for inclusion in a TOML basic string.
///
/// Windows paths contain backslashes; TOML basic strings need them escaped.
/// Non-ASCII is unlikely in a checkout path but we escape defensively.
fn escape_toml(path: &Path) -> String {
    let mut out = String::new();
    for ch in path.to_string_lossy().chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}
