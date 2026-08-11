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

/// Single-node suites that mutate shared SQLite state and must each run
/// against a FRESH database, and single-threaded within the suite.
///
/// Why fresh-DB-per-suite: every single-node suite historically shared one
/// long-lived node (and thus one SQLite file). State accumulated across
/// suites -- e.g. the `sqlite` suite seeds `test_kv`, then a later suite's
/// row-count assertions saw leftover rows, and re-`CREATE TABLE` collided.
/// Spinning each of these up on its own fresh data dir removes cross-suite
/// contamination.
///
/// Why single-threaded (`--test-threads=1`): these suites contain tests that
/// are mutually exclusive on the SAME table within one suite -- e.g.
/// `sqlite`'s `sqlite_create_table_and_insert` expects exactly 2 rows while a
/// sibling test inserts a third, and `..._query_after_cleanup_fails` drops the
/// table out from under a concurrent test. A fresh DB fixes cross-suite bleed
/// but NOT this intra-suite races; serial execution does. These suites are
/// short, so the serial cost is negligible.
///
/// Each of these reuses the single-node DB-proxy ports (3306, ...), so they
/// run strictly sequentially -- spawned, run, torn down -- one at a time,
/// BEFORE the shared node claims those ports.
///
/// `db_bridge` is DB-stateful for the same reason as `sqlite` (it
/// creates/drops its own table and asserts exact row counts), it just
/// reaches the database through the in-process `ephpm_db_*` native
/// functions instead of pdo_mysql. Its rollback-at-request-end assertions
/// additionally depend on no OTHER suite leaving transactions around,
/// which the fresh node guarantees.
const ISOLATED_DB_SUITES: &[&str] =
    &["sqlite", "sqlite_advanced", "rw_split", "query_stats", "db_bridge"];

/// Single-node suites that need a bespoke server config and their own fresh
/// node (also sequential, also reusing the single-node ports).
///
/// `opcache_invalidation` needs `[opcache] cluster_invalidation = true` (off by
/// default in single-node mode) AND a config where the default document_root
/// request resolves to the `_default` OPcache vhost -- which means NOT setting
/// `sites_dir` (with a sites_dir configured, the vhost key becomes the request
/// host `127.0.0.1`, not `_default`, and the test's `opcache:version:_default`
/// write would never match). See `SingleNodeOptions`.
/// `rate_limit` is the one suite that *wants* to be throttled: it fires a burst
/// and asserts a 429. Sharing a token bucket with every other suite made both
/// goals unsatisfiable -- the shared node needs enough headroom that hundreds of
/// back-to-back requests never trip the limiter, while this suite needs a
/// ceiling low enough to hit deliberately. The result was spurious 429s in
/// suites that never touch the limiter (observed repeatedly in the `kv` suite:
/// `left: 429, right: 200`). It now gets its own node with a small bucket, so
/// the shared node can run a generous one.
/// `middleware` needs a `[[middleware]]` mount pointing at a freshly built
/// cdylib, which every other suite must NOT have (the CORS module appends
/// response headers to every PHP response, and `custom_headers`/`etag_cache`
/// assert exact header sets).
///
/// `worker_mode` needs `[php] mode = "worker"`, which is a WHOLE-SERVER switch
/// (every request is served by the persistent worker pool, so the fpm docroot's
/// scripts are unreachable) and hard-errors when combined with
/// `[server] sites_dir`. It therefore gets its own node, its own docroot
/// (`tests/worker-docroot`), and no sites_dir. Before this suite was wired up,
/// nothing in the repo ever set `EPHPM_WORKER_URL`, so all of
/// `crates/ephpm-e2e/tests/worker_mode.rs` self-skipped in every lane —
/// worker mode had zero end-to-end coverage.
const ISOLATED_CONFIG_SUITES: &[&str] =
    &["opcache_invalidation", "rate_limit", "middleware", "worker_mode"];

/// Suites that run single-threaded (a superset of the DB suites). Kept as a
/// helper so `run_suite` can pick the right `--test-threads` value.
///
/// `worker_mode` is serial for a different reason than the DB suites: several
/// of its tests deliberately kill a worker (`?__fatal=1`, `?__exit=1`) while
/// another asserts that a workload recycled *no* worker, reading the
/// `ephpm_worker_recycles_total` counter before and after. That counter is
/// process-global, so a concurrent sibling test would make the assertion flap.
fn suite_is_serial(name: &str) -> bool {
    ISOLATED_DB_SUITES.contains(&name) || name == "worker_mode"
}

/// Whether a suite needs its own freshly-spawned, then-torn-down single node
/// (rather than sharing the long-lived one).
fn suite_needs_fresh_node(name: &str) -> bool {
    ISOLATED_DB_SUITES.contains(&name) || ISOLATED_CONFIG_SUITES.contains(&name)
}

/// Test suites that must be excluded from bare-process runs entirely.
///
/// A few historical suites were Kind-only (they poke Kubernetes services,
/// ClusterIP round-robin, etc.). They still work under `cargo xtask k8s-e2e`
/// — bare-process just skips them. Currently empty; the plan is to migrate
/// or gate them individually.
const SKIP_SUITES: &[&str] = &[];

// Deliberately unclassified suites — i.e. ones where "runs against the
// long-lived shared single node" is a decision rather than an oversight.
// Nothing reads this note; it exists so a future reader does not "fix" the
// omission by moving a suite onto its own fixture.
//
// - `path_traversal` — drives hand-written request lines over a raw TcpStream
//   (reqwest normalizes `../` client-side, so the attack is not expressible
//   with an ordinary client). It asserts against the standard docroot
//   (`/index.php`, `/subdir/index.html`, `/test.html`) and attempts to reach
//   `tests/php/traversal_canary.php`, which sits OUTSIDE that docroot in the
//   sibling `tests/php/`. No bespoke config and no DB state, so the shared
//   node is the right fixture; an isolated node would only cost a spawn.

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

    // Worker mode is a whole-server switch, so its node serves a separate tree
    // holding only the worker entrypoint (`worker_script` must resolve under
    // document_root). Resolved eagerly so a missing fixture fails before any
    // node is spawned.
    let worker_docroot = root.join("tests").join("worker-docroot");
    let worker_docroot = match worker_docroot.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: worker docroot {} unusable: {e}", worker_docroot.display());
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

    // The `middleware` suite mounts a real loadable module by path, so a
    // cdylib has to exist before its node starts. Nothing else in the build
    // produces one: `cargo xtask release` builds the `ephpm` binary only, and
    // the shipped middleware crates' `cdylib` output comes exclusively from an
    // explicit `cargo build -p <crate>`. Build it here, and only when that
    // suite is actually selected, so `--tests basic` stays cheap.
    let middleware_lib = if single_suites.iter().any(|(name, _)| name == "middleware") {
        match build_middleware_cdylib() {
            Some(path) => Some(path),
            None => {
                eprintln!("error: could not build the middleware cdylib for the e2e suite");
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };

    // Split the single-node suites into "isolated" (each needs its own fresh
    // node -- for DB state isolation or a bespoke config) and "shared" (all
    // run against one long-lived node). The isolated suites reuse the same
    // single-node DB-proxy ports (3306, ...), so they MUST run before the
    // shared node claims those ports, and strictly one at a time.
    let (isolated_suites, shared_suites): (Vec<_>, Vec<_>) =
        single_suites.into_iter().partition(|(name, _)| suite_needs_fresh_node(name));

    if !isolated_suites.is_empty() {
        eprintln!(
            "==> Running {} isolated single-node suite(s), each on a fresh ephpm + fresh DB...",
            isolated_suites.len()
        );
        for (name, path) in &isolated_suites {
            let opts = SingleNodeOptions::for_suite(name, middleware_lib.as_deref());
            let node_docroot = if opts.worker { &worker_docroot } else { &docroot };
            let fixture =
                match SingleNodeSpawn::start(&ephpm_bin, node_docroot, &scratch_root, &opts) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("error: failed to start fresh node for suite {name}: {e}");
                        failed.push((*name).clone());
                        continue;
                    }
                };
            let env = fixture.env(&php_version);
            let suite_failed = if run_suite(name, path, &env) {
                Vec::new()
            } else {
                failed.push((*name).clone());
                vec![(*name).clone()]
            };
            fixture.dump_on_failure(&suite_failed);
            drop(fixture);
        }
    }

    if !shared_suites.is_empty() {
        eprintln!(
            "==> Running {} single-node suite(s) against a shared bare ephpm on 127.0.0.1...",
            shared_suites.len()
        );
        let fixture = match SingleNodeSpawn::start(
            &ephpm_bin,
            &docroot,
            &scratch_root,
            &SingleNodeOptions::shared(),
        ) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("error: failed to start single-node fixture: {e}");
                return ExitCode::FAILURE;
            }
        };
        let env = fixture.env(&php_version);
        let mut shared_failed: Vec<String> = Vec::new();
        for (name, path) in &shared_suites {
            if !run_suite(name, path, &env) {
                shared_failed.push((*name).clone());
            }
        }
        fixture.dump_on_failure(&shared_failed);
        failed.extend(shared_failed);
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

/// Build the shipped `ephpm-middleware-cors` cdylib and return its path.
///
/// This is the artifact the `middleware` suite mounts by path, and it is the
/// whole point of that suite: it proves the dlopen lane works with a module
/// built exactly the way the docs tell operators to build one — not with a
/// purpose-made test fixture.
///
/// Built with the same `--release --target <triple>` shape as
/// `crate::release()` so it shares that build's dependency artifacts; the
/// alternative (a bare `--release`) would recompile the dependency graph into
/// a second output directory for no benefit.
fn build_middleware_cdylib() -> Option<PathBuf> {
    let host_target = if cfg!(target_os = "macos") {
        format!("{}-apple-darwin", std::env::consts::ARCH)
    } else {
        format!("{}-unknown-linux-gnu", std::env::consts::ARCH)
    };

    eprintln!("==> Building middleware cdylib (ephpm-middleware-cors, release)...");
    let status = Command::new("cargo")
        .args([
            "build",
            "--release",
            "--package",
            "ephpm-middleware-cors",
            "--target",
            &host_target,
        ])
        .current_dir(workspace_root())
        .status();
    if !matches!(&status, Ok(s) if s.success()) {
        eprintln!("error: cargo build -p ephpm-middleware-cors failed");
        return None;
    }

    let file = format!(
        "{}ephpm_middleware_cors.{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_EXTENSION
    );
    // Honour CARGO_TARGET_DIR: containerised runs mount a shared target volume
    // outside the checkout, where `<root>/target` does not exist at all.
    let target_root = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| workspace_root().join("target"), PathBuf::from);
    let path = target_root.join(&host_target).join("release").join(&file);
    if !path.is_file() {
        eprintln!("error: middleware cdylib not found at {}", path.display());
        return None;
    }
    eprintln!("    middleware cdylib ready: {}", path.display());
    Some(path)
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
    // DB-stateful suites must run single-threaded: several of their tests are
    // mutually exclusive on the same table within one suite (see
    // ISOLATED_DB_SUITES). Everything else parallelises fine.
    let threads = if suite_is_serial(name) { "1" } else { "4" };
    eprintln!("==> Running suite: {name} (--test-threads={threads})");
    let mut cmd = Command::new(binary);
    cmd.arg(format!("--test-threads={threads}"));
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
/// `{SITES_DIR_LINE}` (the whole `sites_dir = "..."` line, or empty),
/// `{OPCACHE_BLOCK}` (an optional `[opcache]` section, or empty),
/// `{MIDDLEWARE_BLOCK}` (an optional `[[middleware]]` mount, or empty),
/// `{KV_SOCKET}`.
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
{SITES_DIR_LINE}
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
{PHP_WORKER_BLOCK}

[server.limits]
max_connections = 100
{LIMITS_BLOCK}

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
{OPCACHE_BLOCK}
{MIDDLEWARE_BLOCK}
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

/// Per-fixture single-node config knobs.
///
/// The isolated suites need small config divergences from the shared node;
/// this captures them so one `SingleNodeSpawn::start` covers every case.
struct SingleNodeOptions {
    /// Scratch subdirectory name (keeps logs/DBs from stomping each other when
    /// several fresh nodes are spawned sequentially). Also used for log labels.
    slug: String,
    /// Configure `[server] sites_dir`. The shared node needs it (vhost/security
    /// suites deploy per-host site dirs); the opcache suite must NOT have it
    /// (with a sites_dir, the OPcache vhost key for a `127.0.0.1` request is
    /// the host, not `_default`, and the test writes `opcache:version:_default`).
    with_sites_dir: bool,
    /// Emit `[opcache] cluster_invalidation = true`. Off by default in
    /// single-node mode, so the opcache_invalidation suite needs it turned on
    /// for the watcher to run at all.
    opcache_invalidation: bool,
    /// Emit a deliberately small `[server.limits]` token bucket. Only the
    /// `rate_limit` suite wants this; every other node gets a generous one so
    /// the limiter never fires in suites that aren't testing it.
    tight_rate_limit: bool,
    /// Mount this shared library as a `[[middleware]]` entry. Only the
    /// `middleware` suite sets it — the CORS module appends response headers
    /// to every PHP response, which other suites assert exact header sets
    /// against.
    middleware_lib: Option<PathBuf>,
    /// Emit `[php] mode = "worker"` + `worker_script`. Only the `worker_mode`
    /// suite; implies `with_sites_dir = false` (the combination hard-errors at
    /// config load) and the `tests/worker-docroot` document root.
    worker: bool,
}

impl SingleNodeOptions {
    /// Options for the long-lived shared node (all non-isolated suites).
    fn shared() -> Self {
        Self {
            slug: "single".to_string(),
            with_sites_dir: true,
            opcache_invalidation: false,
            tight_rate_limit: false,
            middleware_lib: None,
            worker: false,
        }
    }

    /// Options tailored to a specific isolated suite. `middleware_lib` is the
    /// cdylib built by [`build_middleware_cdylib`], present only when the
    /// `middleware` suite was selected.
    fn for_suite(name: &str, middleware_lib: Option<&Path>) -> Self {
        let slug = format!("single-{name}");
        match name {
            // opcache_invalidation: enable the watcher, drop sites_dir so the
            // default docroot resolves to the `_default` OPcache vhost.
            "opcache_invalidation" => Self {
                slug,
                with_sites_dir: false,
                opcache_invalidation: true,
                tight_rate_limit: false,
                middleware_lib: None,
                worker: false,
            },
            // worker_mode: persistent worker pool over tests/worker-docroot.
            // sites_dir MUST stay off — worker mode + sites_dir is rejected at
            // config load, so the node would never start.
            "worker_mode" => Self {
                slug,
                with_sites_dir: false,
                opcache_invalidation: false,
                tight_rate_limit: false,
                middleware_lib: None,
                worker: true,
            },
            // rate_limit: small bucket, on its own node. See
            // ISOLATED_CONFIG_SUITES for why this can't share the shared node.
            "rate_limit" => Self {
                slug,
                with_sites_dir: true,
                opcache_invalidation: false,
                tight_rate_limit: true,
                middleware_lib: None,
                worker: false,
            },
            // middleware: mount the freshly built cors cdylib by explicit
            // path, exercising the dlopen lane in a real server.
            "middleware" => Self {
                slug,
                with_sites_dir: true,
                opcache_invalidation: false,
                tight_rate_limit: false,
                middleware_lib: middleware_lib.map(Path::to_path_buf),
                worker: false,
            },
            // DB suites: fresh DB, but otherwise the same shape as the shared
            // node (sites_dir present is harmless; they don't use it).
            _ => Self {
                slug,
                with_sites_dir: true,
                opcache_invalidation: false,
                tight_rate_limit: false,
                middleware_lib: None,
                worker: false,
            },
        }
    }
}

struct SingleNodeSpawn {
    child: Child,
    http_port: u16,
    _data_dir: PathBuf,
    sites_dir: Option<PathBuf>,
    /// The (canonicalized) document_root this node serves, exported to tests as
    /// EXPECTED_DOCUMENT_ROOT so `$_SERVER['DOCUMENT_ROOT']` assertions are
    /// environment-agnostic (the container path is `/var/www/html`; here it is
    /// the repo's `tests/docroot`).
    docroot: PathBuf,
    label: String,
    stderr_log: PathBuf,
    stdout_log: PathBuf,
    /// The ephpm binary this node was spawned from. Exported as
    /// `EPHPM_BINARY` **only** when `middleware_lib` is set: the `middleware`
    /// suite needs it to assert that a *bad* mount makes startup fail, which
    /// cannot be observed from a node that is already up. Other suites must
    /// not see it — see the note in [`SingleNodeSpawn::env`].
    binary: PathBuf,
    /// Loadable module mounted on this node, exported as
    /// `EPHPM_MIDDLEWARE_LIB` so the suite can reuse the same artifact for
    /// its own spawns instead of re-deriving the build path.
    middleware_lib: Option<PathBuf>,
    /// This node runs `[php] mode = "worker"`; `env()` additionally exports
    /// `EPHPM_WORKER_URL`, which is what the `worker_mode` suite gates on.
    worker: bool,
}

impl SingleNodeSpawn {
    fn start(
        binary: &Path,
        docroot: &Path,
        scratch_root: &Path,
        opts: &SingleNodeOptions,
    ) -> io::Result<Self> {
        let http_port: u16 = 18100;
        // DB proxy listeners keep the reference-config ports. The docroot PHP
        // scripts connect to a hardcoded 127.0.0.1:3306 fallback (single-node
        // [db.sqlite] does not inject DB_HOST/DB_PORT), so the mysql proxy must
        // own 3306 or every SQLite write 500s. Only one single node runs at a
        // time (isolated suites are spawned/torn down sequentially, before the
        // shared node), so these ports never collide.
        let mysql_port: u16 = 3306;
        let hrana_port: u16 = 8081;
        let pg_port: u16 = 5432;
        let tds_port: u16 = 1433;

        let node_dir = scratch_root.join(&opts.slug);
        fs::create_dir_all(&node_dir)?;
        let data_dir = node_dir.join("data");
        fs::create_dir_all(&data_dir)?;
        // Provision the sites_dir the vhost/security suites deploy into. The
        // tests create/remove per-host site dirs themselves at runtime; we just
        // guarantee the parent exists and is writable (mirrors /var/www/sites in
        // the container image).
        let sites_dir = if opts.with_sites_dir {
            let dir = node_dir.join("sites");
            fs::create_dir_all(&dir)?;
            Some(dir)
        } else {
            None
        };
        // Unix-domain socket for [kv.redis_compat] (matches the reference
        // config's /tmp/ephpm-kv.sock). Kept short to stay under the ~104-char
        // sun_path limit on Linux.
        let kv_socket = node_dir.join("kv.sock");

        let sites_dir_line = match &sites_dir {
            Some(dir) => format!("sites_dir = \"{}\"", escape_toml(dir)),
            None => String::new(),
        };
        let opcache_block = if opts.opcache_invalidation {
            "\n[opcache]\ncluster_invalidation = true".to_string()
        } else {
            String::new()
        };
        // Only the rate_limit suite runs a bucket small enough to trip. Every
        // other node gets a budget large enough that the whole suite's traffic
        // never reaches it, which is what stops the spurious 429s.
        // Worker mode: a small, fixed pool so `boot #B` ids stay bounded and
        // the suite can tell "no worker was recycled" from "a worker rebooted".
        // worker_max_requests is left generous on purpose — the recycle test
        // asserts requests keep succeeding, not that a recycle happens.
        let php_worker_block = if opts.worker {
            "mode = \"worker\"\nworker_script = \"worker.php\"\nworker_count = 3\n\
             worker_max_requests = 10000\nworker_boot_timeout = 60"
        } else {
            ""
        };
        let limits_block = if opts.tight_rate_limit {
            "per_ip_rate = 500.0\nper_ip_burst = 100"
        } else {
            "per_ip_rate = 100000.0\nper_ip_burst = 20000"
        };
        // Mount by EXPLICIT PATH on purpose: that is the dlopen lane's entry
        // condition (`resolve_library` treats a value with a separator or an
        // extension as a path), and a bare name like "cors" would silently
        // resolve to the builtin static registry instead — testing nothing.
        // `allow_origins` is a specific origin, not "*", so a request from an
        // unlisted origin must come back with NO CORS headers; that is what
        // distinguishes "the module ran and decided" from "something bolted
        // headers on".
        let middleware_block = match &opts.middleware_lib {
            Some(lib) => format!(
                "\n[[middleware]]\nlibrary = \"{}\"\norder = 10\n\
                 config = {{ allow_origins = [\"https://allowed.e2e.test\"], max_age = 600 }}",
                escape_toml(lib)
            ),
            None => String::new(),
        };

        let config = SINGLE_NODE_TEMPLATE
            .replace("{HTTP_PORT}", &http_port.to_string())
            .replace("{MYSQL_PORT}", &mysql_port.to_string())
            .replace("{HRANA_PORT}", &hrana_port.to_string())
            .replace("{PG_PORT}", &pg_port.to_string())
            .replace("{TDS_PORT}", &tds_port.to_string())
            .replace("{DATA_DIR}", &escape_toml(&data_dir))
            .replace("{SITES_DIR_LINE}", &sites_dir_line)
            .replace("{OPCACHE_BLOCK}", &opcache_block)
            .replace("{MIDDLEWARE_BLOCK}", &middleware_block)
            .replace("{LIMITS_BLOCK}", limits_block)
            .replace("{PHP_WORKER_BLOCK}", php_worker_block)
            .replace("{KV_SOCKET}", &escape_toml(&kv_socket))
            .replace("{DOCROOT}", &escape_toml(docroot));

        let config_path = node_dir.join("ephpm.toml");
        fs::write(&config_path, config)?;

        let stdout_log = node_dir.join("stdout.log");
        let stderr_log = node_dir.join("stderr.log");
        let stdout_file = fs::File::create(&stdout_log)?;
        let stderr_file = fs::File::create(&stderr_log)?;

        eprintln!(
            "    spawning single-node ephpm [{}] on 127.0.0.1:{http_port} (config: {})",
            opts.slug,
            config_path.display()
        );
        let child = Command::new(binary)
            .args(["serve", "--config"])
            .arg(&config_path)
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file))
            .spawn()?;

        // Worker mode has to boot the framework in every worker before the
        // first request can be served, so it gets a longer readiness window.
        let health_timeout =
            if opts.worker { Duration::from_secs(60) } else { Duration::from_secs(10) };
        wait_for_health(http_port, health_timeout)?;
        if opts.worker {
            // `/_ephpm/health` is pure liveness (200 as soon as the listener is
            // up), so on a worker node it says nothing about whether any worker
            // has finished booting the framework. Wait for a real request to be
            // served before handing the node to the suite, otherwise the first
            // test races the boot and sees a 504.
            wait_for_worker(http_port, health_timeout)?;
        }
        eprintln!("    single-node ephpm [{}] ready on 127.0.0.1:{http_port}", opts.slug);

        Ok(Self {
            child,
            http_port,
            _data_dir: data_dir,
            sites_dir,
            docroot: docroot.to_path_buf(),
            label: opts.slug.clone(),
            binary: binary.to_path_buf(),
            middleware_lib: opts.middleware_lib.clone(),
            stderr_log,
            stdout_log,
            worker: opts.worker,
        })
    }

    fn env(&self, php_version: &str) -> Vec<(String, String)> {
        // NOTE: EPHPM_SERVER__PHP_ETAG_CACHE__ENABLED is deliberately NOT set
        // here. Config env overrides only take effect on the ephpm *server*
        // process; setting it on the test binary is a no-op. The etag cache is
        // enabled in the server config template instead (see
        // SINGLE_NODE_TEMPLATE / [server.php_etag_cache]).
        let mut env = vec![
            ("EPHPM_URL".to_string(), format!("http://127.0.0.1:{}", self.http_port)),
            ("EXPECTED_PHP_VERSION".to_string(), php_version.to_string()),
            // php::server_vars_populated asserts $_SERVER['DOCUMENT_ROOT'].
            // Under bare-process that is the repo's tests/docroot, not the
            // container's /var/www/html, so hand the test the actual value.
            ("EXPECTED_DOCUMENT_ROOT".to_string(), self.docroot.to_string_lossy().into_owned()),
        ];
        // vhost/security_p0 suites deploy per-host site dirs into this path.
        // Only set when this node actually has a sites_dir configured.
        if let Some(dir) = &self.sites_dir {
            env.push(("EPHPM_SITES_DIR".to_string(), dir.to_string_lossy().into_owned()));
        }
        // `EPHPM_BINARY` and `EPHPM_MIDDLEWARE_LIB` go ONLY to the node that
        // has a middleware mount -- i.e. only the `middleware` suite, which
        // spawns extra ephpm processes to assert that a broken `[[middleware]]`
        // entry aborts startup.
        //
        // Do NOT hoist `EPHPM_BINARY` out of this branch and set it for every
        // suite. `ephpm-e2e`'s self-managed fixtures treat it as an opt-in
        // switch -- `bare_process_smoke` reads it and, when present, spawns its
        // own 2-node cluster. Exporting it globally woke that suite up for the
        // first time ever, and it failed immediately: it derives its docroot
        // from `CARGO_MANIFEST_DIR`, which under `cargo xtask e2e` is *xtask's*
        // manifest dir (the harness execs pre-built test binaries, which
        // inherit xtask's environment), so it looked for `xtask/tests/docroot`.
        // That suite is dormant *and* broken under this harness; un-dormanting
        // it is separate work.
        if let Some(lib) = &self.middleware_lib {
            env.push(("EPHPM_BINARY".to_string(), self.binary.to_string_lossy().into_owned()));
            env.push(("EPHPM_MIDDLEWARE_LIB".to_string(), lib.to_string_lossy().into_owned()));
        }
        // The worker_mode suite gates on EPHPM_WORKER_URL rather than EPHPM_URL
        // precisely so it cannot be pointed at an fpm-mode node by accident.
        if self.worker {
            env.push((
                "EPHPM_WORKER_URL".to_string(),
                format!("http://127.0.0.1:{}", self.http_port),
            ));
        }
        env
    }

    fn dump_on_failure(&self, failed: &[String]) {
        if failed.is_empty() {
            return;
        }
        dump_log(&format!("single-node [{}] stderr", self.label), &self.stderr_log);
        dump_log(
            &format!("single-node [{}] stdout (last 200 lines)", self.label),
            &self.stdout_log,
        );
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

/// Wait until a worker-mode node actually serves a PHP request (not just the
/// liveness probe). Proves at least one worker finished booting the framework.
fn wait_for_worker(port: u16, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last_err = String::from("no attempts made");
    while Instant::now() < deadline {
        match tcp_get(port, "/_xtask_worker_warmup") {
            Ok(200) => return Ok(()),
            Ok(status) => last_err = format!("worker request returned HTTP {status}"),
            Err(e) => last_err = format!("connect error: {e}"),
        }
        thread::sleep(Duration::from_millis(200));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("no worker on 127.0.0.1:{port} served a request within {timeout:?}: {last_err}"),
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
