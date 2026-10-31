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
//!
//! - **No fixed ports.** Every listener a node opens — HTTP, the SQLite wire
//!   frontends, the KV RESP listener, a cluster node's gossip/channel/KV-data
//!   ports — is drawn at random from [`PORT_RANGE`], held by a probe socket
//!   until the node is spawned, and then *proven* to be the node's own (it
//!   must log binding it). A node that loses a port anyway is respawned on a
//!   fresh set (see [`SPAWN_ATTEMPTS`]). The suites learn the ports from the
//!   environment (`EPHPM_URL`, `EPHPM_KV_PORT`, `EPHPM_HRANA_URL`, ...) and the
//!   docroot's PHP from `DB_HOST`/`DB_PORT` in the node's process environment.
//!   The fixed ports this harness used to hard-code (18100, 3306, 6379, ...)
//!   meant two runs on one host — or one run and a leftover process from an
//!   earlier fixture — answered each other's health checks and served each
//!   other's suites.

use std::collections::BTreeMap;
use std::net::{TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{fs, io, thread};

use crate::{DEFAULT_PHP_MINOR, canonicalize_portable, release, workspace_root};

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
/// Each of these runs strictly sequentially -- spawned, run, torn down -- one
/// at a time, on its own freshly reserved ports.
///
/// `db_bridge` is DB-stateful for the same reason as `sqlite` (it
/// creates/drops its own table and asserts exact row counts), it just
/// reaches the database through the in-process `ephpm_db_*` native
/// functions instead of pdo_mysql. Its rollback-at-request-end assertions
/// additionally depend on no OTHER suite leaving transactions around,
/// which the fresh node guarantees.
///
/// `hrana`, `postgres_proxy` and `tds_proxy` are here for a second reason on
/// top of the state one (they each `CREATE TABLE` through `/db.php` or
/// `/sqlite_test.php`): they are the suites that assert the *wire listeners*
/// still work when a second frontend is enabled, and a `sites_dir` node has no
/// wire listeners at all — per-site database mode deliberately does not start
/// them, because one listener cannot route by virtual host. They therefore
/// cannot live on the shared (multi-tenant) node.
const ISOLATED_DB_SUITES: &[&str] = &[
    "sqlite",
    "sqlite_advanced",
    "rw_split",
    "query_stats",
    "db_bridge",
    "hrana",
    "postgres_proxy",
    "tds_proxy",
];

/// Single-node suites that need a bespoke server config and their own fresh
/// node (also sequential, each on freshly reserved ports).
///
/// `opcache_invalidation` needs `[opcache] cluster_invalidation = true` (off by
/// default in single-node mode) AND a config where the default document_root
/// request resolves to the `_DEFAULT` OPcache vhost -- which means NOT setting
/// `sites_dir` (with a sites_dir configured, the vhost key becomes the request
/// host `127.0.0.1`, not `_DEFAULT`, and the test's `opcache:version:_DEFAULT`
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
///
/// `multitenant_hardening` needs `[server.security] multi_tenant_hardening`
/// resolved ON, which is a WHOLE-SERVER switch: with it on (and `sites_dir`
/// set) ePHPm rewrites the generated php.ini to disable `opcache_get_status`/
/// `opcache_invalidate` (single-node has no `cluster_invalidation`) and points
/// `opcache.restrict_api` at a sentinel — both make `opcache_get_status()`
/// return false. That is exactly what the shared node must NOT do, or the
/// `php_config` suite's `opcache_is_enabled_over_http` (which shares that node)
/// fails. So hardening gets its OWN node: `sites_dir` set (multi-tenant, which
/// resolves hardening on), the `harden.test` vhost, and the operator's
/// `disable_functions = getmypid` override, so `multitenant_hardening.rs` still
/// proves the union/clobber fix. The shared node keeps hardening explicitly
/// OFF (see `SingleNodeOptions::shared`).
///
/// `fpm_pool` proves the dedicated per-request execution pool (the only
/// engine) with a **pinned** pool size (`[php] concurrency`), so its metrics
/// assertions are deterministic rather than host-derived. Sizing the pool is
/// a whole-server decision, so — like `worker_mode` — it gets its own node.
/// It reuses the standard single-site `[db.sqlite]` docroot so the suite can
/// prove the pool runs real PHP AND the `ephpm_db_*` bridge, and reads
/// `ephpm_fpm_pool_size` from `/metrics` to confirm the pinned size took.
///
/// `crash_containment` needs `[php] crash_containment = true`, and — unlike
/// every other suite — it
/// deliberately **crashes PHP**: it drives a C-stack overflow that poisons and
/// retires a pool thread. That is destructive to whatever node it runs on
/// (threads are retired, PHP module shutdown is skipped at exit), so it gets its
/// own throwaway node and must never share one.
///
/// `preview` needs `[server] preview = true`, a WHOLE-SERVER switch: it stamps
/// `X-Ephpm-Preview: 1` on every response (which `custom_headers`-style suites
/// must never see) and turns on the per-site PHP rate cap (5/s burst 20 from
/// the preset), which any other suite's PHP traffic would trip. It gets its
/// own multi-tenant node (`sites_dir` set — the per-site cap only acts on
/// requests that resolve to a site key) with the template's generous explicit
/// per-IP limits and explicit `max_connections = 100`, which doubles as an
/// end-to-end proof that explicit `[server.limits]` values beat the preset.
///
/// `websockets` needs `[server.websocket] enabled = true`, which changes how
/// **every** upgrade request routes (they resolve `[server] websocket_files`
/// and 404 when no entrypoint exists). That is a whole-server switch, so — like
/// `fpm_pool` — it gets its own node rather than being bolted onto the shared
/// template. It runs multi-tenant (`sites_dir`) so the suite can prove
/// cross-site isolation with two real vhosts on one server, which is the
/// property most worth an end-to-end test.
///
/// `load_shedding` needs `[php] overload = "shed"` on a **deliberately
/// tiny** execution pool (`concurrency = 1`, `queue_depth = 1`) so that
/// saturation is reachable with a handful of concurrent requests. Both halves
/// are whole-server switches and both are hostile to every other suite: a
/// one-thread pool serialises all PHP, and the shed policy answers 503 to
/// anything that arrives while that thread is busy — which is exactly what
/// `fpm_pool`'s `pool_handles_concurrent_requests` (40 in-flight, all must be
/// 200) asserts must NOT happen. So it cannot be bolted onto the `fpm_pool`
/// node, and certainly not the shared one (the #295 lesson).
const ISOLATED_CONFIG_SUITES: &[&str] = &[
    "opcache_invalidation",
    "rate_limit",
    "middleware",
    "worker_mode",
    "multitenant_hardening",
    "fpm_pool",
    "crash_containment",
    "preview",
    "websockets",
    "load_shedding",
];

/// Suites that exercise `ephpm_kv_*` (or the KV-backed `ephpm` session handler)
/// from the **default document root**, so they need a single-site node.
///
/// The shared node runs multi-tenant (`sites_dir`, for `vhosts`/`security_p0`),
/// and on a multi-tenant node a `Host` that matches no vhost — `127.0.0.1`,
/// which is what these suites send — has no KV keyspace: every `ephpm_kv_*`
/// call fails with "no per-site KV context". It used to get a store keyed by
/// the raw `Host`, which let any client mint a never-freed store per header
/// value (KV stress report F-2). These suites test the KV functions
/// themselves, not tenancy, so each gets a fresh single-site node (the `_` arm
/// of [`SingleNodeOptions::for_suite`]) where the default docroot uses the
/// global store. Per-tenant KV isolation stays covered on multi-tenant nodes by
/// `request_teardown` (which runs its KV-backed session case under a vhost) and
/// `websockets`.
const SINGLE_SITE_KV_SUITES: &[&str] =
    &["concurrency", "kv", "kv_advanced", "kv_unix_socket", "session_locking"];

/// Suites that run single-threaded (a superset of the DB suites). Kept as a
/// helper so `run_suite` can pick the right `--test-threads` value.
///
/// `worker_mode` is serial for a different reason than the DB suites: several
/// of its tests deliberately kill a worker (`?__fatal=1`, `?__exit=1`) while
/// another asserts that a workload recycled *no* worker, reading the
/// `ephpm_worker_recycles_total` counter before and after. That counter is
/// process-global, so a concurrent sibling test would make the assertion flap.
fn suite_is_serial(name: &str) -> bool {
    // `fpm_pool` mutates shared DB state (creates/drops `bridge_e2e`) the same
    // way the DB suites do, so it must not run its own tests concurrently.
    //
    // `crash_containment` is serial for the `worker_mode` reason squared: its
    // tests crash PHP on purpose and read process-global counters
    // (`ephpm_fpm_pool_contained_crashes_total`, `..._recycles_total`) before
    // and after, and they assert that a *concurrent* request on another thread
    // is unaffected — which only means anything if this suite owns the pool.
    //
    // `rate_limit` is serial for a resource reason: its #299 shed test fills
    // the node's ENTIRE `max_connections` budget with held-open sockets while
    // it probes the over-cap behaviour. Run concurrently, the burst test's
    // connections would be shed too and its "some requests succeed" half
    // could never hold.
    //
    // `websockets` deploys two vhost directories into `sites_dir` and shares
    // one KV keyspace per vhost across its tests. Running serially means each
    // test can assert on absolute connection counts and KV state instead of
    // having to name every key uniquely and hope.
    // `load_shedding` is serial for the `rate_limit` reason: its whole method is
    // saturating a one-thread pool and reading the process-global
    // `ephpm_php_shed_total` counter around the burst. A concurrent sibling
    // would both consume the slot it means to occupy and move the counter.
    // `turso_cdc` is serial as a resource matter: each test spawns a
    // multi-process cluster, and running two topologies at once would double
    // the process count and the CPU noise its election-timing deadlines have
    // to absorb.
    ISOLATED_DB_SUITES.contains(&name)
        || name == "worker_mode"
        || name == "fpm_pool"
        || name == "crash_containment"
        || name == "rate_limit"
        || name == "websockets"
        || name == "load_shedding"
        || name == "turso_cdc"
}

/// Whether a suite needs its own freshly-spawned, then-torn-down single node
/// (rather than sharing the long-lived one).
fn suite_needs_fresh_node(name: &str) -> bool {
    ISOLATED_DB_SUITES.contains(&name)
        || ISOLATED_CONFIG_SUITES.contains(&name)
        || SINGLE_SITE_KV_SUITES.contains(&name)
}

/// Suites that need **no ephpm server at all** — they exec the release binary
/// directly and assert on its stdout/stderr/exit status.
///
/// `cli` covers `ephpm php` (issue #321: fatals were silent and exited 0).
/// Spawning an HTTP node for it would be pure waste, and worse, it would have
/// to be a node whose config could never perturb — or be perturbed by — a
/// suite that only ever runs a subprocess. So these run first, before any
/// port is claimed, with just `EPHPM_CLI_BINARY` in the environment.
///
/// It deliberately uses a distinct variable name rather than `EPHPM_BINARY`,
/// which `ephpm-e2e`'s self-managed fixtures treat as an opt-in switch to
/// spawn their own topology (see `SELF_MANAGED_SUITES`). A `cli` suite that
/// saw `EPHPM_BINARY` would gain that behaviour for free, which it must not.
const NO_NODE_SUITES: &[&str] = &["cli"];

/// Suites that manage their own ephpm topology entirely — the harness starts
/// **no** node for them and hands them only `EPHPM_BINARY` (plus
/// `EXPECTED_PHP_VERSION`). Each such suite spawns, kills, and restarts its
/// own processes on freshly-reserved loopback ports, so it cannot collide
/// with the fixtures spawned later in the run.
///
/// `turso_cdc` is here because its scenarios are process-lifecycle tests:
/// electing a primary among nodes it can kill, and cold-bootstrapping a node
/// it starts *after* data exists. Neither is expressible against the
/// long-lived 3-node cluster fixture (killing one of its nodes would break
/// the sibling `cluster` suite), and per the #288/#295 lesson its clustered
/// `hrana_listen` config must live in the suite's own template, not the
/// shared one.
///
/// `bare_process_smoke` is here because it is the only coverage `ephpm-e2e`'s
/// own [`ClusterFixture`](../../crates/ephpm-e2e/src/lib.rs) has: it spawns a
/// 2-node cluster from the library fixture and asserts both nodes serve PHP.
/// That fixture writes its own config template and reserves its own loopback
/// ports, so — exactly like `turso_cdc` — it needs no node from the harness
/// and cannot collide with the fixtures spawned later. Until #239
/// it was dormant (nothing set `EPHPM_BINARY`, so it self-skipped and reported
/// `ok`) *and* broken (it derived its docroot from `CARGO_MANIFEST_DIR`, which
/// is xtask's under this harness). `EPHPM_DOCROOT` below fixes the second half.
///
/// `pdo_mysql_proxy` is here because it needs a `[db.mysql]` node in front of a
/// **real MySQL/MariaDB** — a backend nothing in the bare-process rig
/// provisions — so it spawns its own `MysqlProxyFixture` and reads the backend
/// URL from `EPHPM_MYSQL_PROXY_TEST_URL`. On the ordinary bare-process lane that
/// variable is unset and the suite self-skips; it does real work only in the
/// CI job that boots a database (`.github/workflows/pdo-mysql-e2e.yml`), which
/// also sets `EPHPM_REQUIRE_DB_TESTS=1` so a missing backend fails rather than
/// skips there (issue #433). Its config is `[db.mysql]`, which must NOT live on
/// any shared or `[db.sqlite]` node (#288/#295), hence self-managed.
///
/// `kv_snapshot_restart` is here because it is a process-lifecycle test: it
/// stops its node gracefully (the path that writes the warm-restart KV
/// snapshot) and starts it again on the same ports and data directory, which
/// no shared node could survive. Its `[kv.snapshot]` config lives in the
/// suite's own template (`SingleNodeFixture::start_with`), per #288/#295.
///
/// `db_import` is here because it is a two-phase process-lifecycle test: it
/// runs `ephpm db import` offline (direct-open, server down) into a per-site
/// `[db.sqlite]` database, then starts a node serving that site to read the
/// imported rows back over HTTP. The import must own the database file while
/// no server holds it, so it cannot share any node; its per-site config lives
/// in the suite's own template (`DbImportFixture`), per #288/#295.
///
/// NOTE: `EPHPM_BINARY` goes to exactly these suites and the `middleware`
/// node — see the note in [`SingleNodeSpawn::env`]. It stays scoped rather
/// than global because it is an opt-in switch for `ephpm-e2e`'s self-managed
/// fixtures, and a suite that gets it starts spawning real processes.
const SELF_MANAGED_SUITES: &[&str] =
    &["turso_cdc", "bare_process_smoke", "pdo_mysql_proxy", "kv_snapshot_restart", "db_import"];

/// Test suites that must be excluded from bare-process runs entirely.
///
/// A few historical suites were Kind-only (they poke Kubernetes services,
/// ClusterIP round-robin, etc.). They still work under `cargo xtask k8s-e2e`
/// — bare-process just skips them. Currently empty; the plan is to migrate
/// or gate them individually.
const SKIP_SUITES: &[&str] = &[];

/// Why `suite` cannot run on the host this harness is running on, if it can't.
///
/// Unlike [`SKIP_SUITES`] this is a statement about a **missing product
/// capability**, not about the harness, so a skip is never silent: the harness
/// prints it as a banner when the suite is dropped and again in the end-of-run
/// summary, naming what is missing (issue #454).
///
/// `crash_containment` on Windows: `[php] crash_containment` is built on POSIX
/// signal handling (`crates/ephpm-php/crash_guard.c`, `fatal_signal.rs`), and
/// Windows delivers a stack overflow as an SEH exception that nothing in ePHPm
/// catches, so the deliberately-crashing requests in that suite take the whole
/// server down. Running it there can only ever report "the crash was not
/// contained", which is a known, documented gap (the Windows guide's "No crash
/// containment" note), not a regression. The day SEH containment exists, delete
/// this arm and the suite runs on Windows again.
fn platform_skip_reason(suite: &str) -> Option<&'static str> {
    platform_skip_reason_for(suite, cfg!(windows))
}

/// [`platform_skip_reason`] with the host platform as a parameter, so both
/// branches are unit-testable from any host.
fn platform_skip_reason_for(suite: &str, windows: bool) -> Option<&'static str> {
    match suite {
        "crash_containment" if windows => Some(
            "Windows has NO stack-overflow crash containment: [php] crash_containment is \
             Unix-only (POSIX signals; Windows raises SEH exceptions, which ePHPm does not \
             handle), so a PHP C-stack overflow kills the whole server on this platform",
        ),
        _ => None,
    }
}

// Deliberately unclassified suites — i.e. ones where "runs against the
// long-lived shared single node" is a decision rather than an oversight.
// Nothing reads this note; it exists so a future reader does not "fix" the
// omission by moving a suite onto its own fixture.
//
// - `vhosts` and `security_p0` — the only readers of `EPHPM_SITES_DIR`, and so
//   the only reason the shared node runs multi-tenant at all. Everything else
//   on that node is indifferent to `sites_dir`; nothing else on it touches the
//   database, which is what makes multi-tenant mode (no SQLite wire listeners)
//   viable there. If a future suite on the shared node needs `pdo_mysql`, it
//   belongs in ISOLATED_DB_SUITES, not on this node. Likewise a suite that
//   needs `ephpm_kv_*` on the default docroot belongs in SINGLE_SITE_KV_SUITES
//   (an unmatched `Host` has no KV keyspace on a multi-tenant node).
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
    // `canonicalize_portable`, not `Path::canonicalize`: on Windows the latter
    // yields a `\\?\`-prefixed path, which PHP's `open_basedir` check rejects —
    // every script under the docroot then 500s and the whole suite is
    // unrunnable on a Windows host (issue #367).
    let docroot = match canonicalize_portable(&docroot) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: failed to canonicalize {}: {e}", docroot.display());
            return ExitCode::FAILURE;
        }
    };

    // Worker mode is a whole-server switch, so its node serves a separate tree
    // holding only the worker entrypoint (`[php.worker] script` must resolve under
    // document_root). Resolved eagerly so a missing fixture fails before any
    // node is spawned.
    let worker_docroot = root.join("tests").join("worker-docroot");
    let worker_docroot = match canonicalize_portable(&worker_docroot) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: worker docroot {} unusable: {e}", worker_docroot.display());
            return ExitCode::FAILURE;
        }
    };

    // Per-node configs, data dirs and logs. Overridable so two runs can share
    // one checkout (e.g. to reproduce two CI legs landing on one host).
    let (scratch_root, scratch_is_default) = match parse_named_flag(args, "--scratch-dir") {
        Some(dir) => (PathBuf::from(dir), false),
        None => (root.join("target").join("e2e-bare"), true),
    };
    if let Err(e) = prepare_scratch_dir(&scratch_root, scratch_is_default) {
        eprintln!("error: failed to prepare {}: {e}", scratch_root.display());
        return ExitCode::FAILURE;
    }

    // Compile the e2e test binaries. `--no-run` builds every test target
    // without executing anything; cargo's JSON messages name each test
    // binary's executable, wherever CARGO_TARGET_DIR put it.
    eprintln!("==> Building ephpm-e2e test binaries...");
    let e2e_manifest = root.join("crates").join("ephpm-e2e").join("Cargo.toml");
    let binaries = match build_test_binaries(&e2e_manifest) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: cargo test --no-run failed for ephpm-e2e: {e}");
            return ExitCode::FAILURE;
        }
    };

    if binaries.is_empty() {
        eprintln!("error: cargo reported no ephpm-e2e test binaries");
        return ExitCode::FAILURE;
    }

    // Split binaries by whether they need cluster mode.
    let mut single_suites: Vec<(String, PathBuf)> = Vec::new();
    let mut cluster_suites: Vec<(String, PathBuf)> = Vec::new();
    let mut platform_skipped: Vec<(String, &'static str)> = Vec::new();
    for (name, path) in binaries {
        if SKIP_SUITES.contains(&name.as_str()) {
            continue;
        }
        if let Some(only) = only_suite.as_deref()
            && name != only
        {
            continue;
        }
        if let Some(reason) = platform_skip_reason(&name) {
            eprintln!("!!! SKIPPING suite `{name}` on this platform: {reason}");
            platform_skipped.push((name, reason));
            continue;
        }
        if CLUSTER_SUITES.contains(&name.as_str()) {
            cluster_suites.push((name, path));
        } else {
            single_suites.push((name, path));
        }
    }

    if single_suites.is_empty() && cluster_suites.is_empty() && !platform_skipped.is_empty() {
        // `--tests <suite>` named only a suite this platform cannot run. Not a
        // harness error, and not a pass either: say exactly that and stop.
        print_platform_skips(&platform_skipped);
        return ExitCode::SUCCESS;
    }
    if single_suites.is_empty() && cluster_suites.is_empty() {
        eprintln!(
            "error: no test suites selected (--tests {:?} filtered everything out)",
            only_suite
        );
        return ExitCode::FAILURE;
    }

    // Suites that need no server run first and cheapest: no node, no ports.
    let (no_node_suites, single_suites): (Vec<_>, Vec<_>) =
        single_suites.into_iter().partition(|(name, _)| NO_NODE_SUITES.contains(&name.as_str()));

    let mut failed: Vec<String> = Vec::new();

    if !no_node_suites.is_empty() {
        eprintln!(
            "==> Running {} server-less suite(s) directly against the binary...",
            no_node_suites.len()
        );
        let env = vec![("EPHPM_CLI_BINARY".to_string(), ephpm_bin.to_string_lossy().into_owned())];
        for (name, path) in &no_node_suites {
            if !run_suite(name, path, &env) {
                failed.push((*name).clone());
            }
        }
    }

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

    // Self-managed suites next (after the server-less ones): the harness
    // spawns nothing for them either, they just need the binary path. Every
    // topology in this run reserves its own ports, so ordering is a non-issue
    // — running them first simply keeps all "one at a time" phases together.
    let (self_managed_suites, single_suites): (Vec<_>, Vec<_>) = single_suites
        .into_iter()
        .partition(|(name, _)| SELF_MANAGED_SUITES.contains(&name.as_str()));

    if !self_managed_suites.is_empty() {
        eprintln!(
            "==> Running {} self-managed suite(s), each spawning its own ephpm topology...",
            self_managed_suites.len()
        );
        // `EPHPM_DOCROOT` is the harness-supplied document root for suites
        // that spawn their own nodes. It exists because a pre-built test
        // binary exec'd from here inherits *xtask's* `CARGO_MANIFEST_DIR`, so
        // a suite cannot derive the repo's `tests/docroot` for itself (#239).
        // `turso_cdc` ignores it and builds a throwaway docroot in its own
        // tempdir; `bare_process_smoke` serves this one.
        let env = vec![
            ("EPHPM_BINARY".to_string(), ephpm_bin.to_string_lossy().into_owned()),
            ("EPHPM_DOCROOT".to_string(), docroot.to_string_lossy().into_owned()),
            ("EXPECTED_PHP_VERSION".to_string(), php_version.clone()),
        ];
        for (name, path) in &self_managed_suites {
            if !run_suite(name, path, &env) {
                failed.push((*name).clone());
            }
        }
    }

    // Split the single-node suites into "isolated" (each needs its own fresh
    // node -- for DB state isolation or a bespoke config) and "shared" (all
    // run against one long-lived node). The isolated suites run strictly one
    // at a time, each node torn down before the next is spawned.
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

    // Repeated at the end so the skip cannot scroll away above thousands of
    // lines of suite output.
    print_platform_skips(&platform_skipped);

    if failed.is_empty() {
        if platform_skipped.is_empty() {
            eprintln!("==> All bare-process E2E suites passed");
        } else {
            eprintln!(
                "==> All bare-process E2E suites that can run on this platform passed \
                 ({} skipped — see above)",
                platform_skipped.len()
            );
        }
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

/// Print the suites dropped by [`platform_skip_reason`], loudly. No-op when
/// nothing was skipped.
fn print_platform_skips(skipped: &[(String, &'static str)]) {
    if skipped.is_empty() {
        return;
    }
    eprintln!("==== SKIPPED E2E SUITES (capability missing on this platform) ====");
    for (name, reason) in skipped {
        eprintln!("  {name}: {reason}");
    }
    eprintln!("==== end skipped suites ====");
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
  --scratch-dir <path>    Where node configs, data and logs go (default:
                          target/e2e-bare). Wiped at the start of the run, so
                          an existing directory must be empty or one this
                          harness created. Lets two runs share a checkout.
  --help                  Print this message.

Every node listens on loopback ports picked at random per run, so concurrent
runs on one host do not collide.

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
/// 2. `target/release/ephpm` (or its per-triple equivalent) if it already
///    exists, under `CARGO_TARGET_DIR` first when that is set
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
    //
    // The windows-msvc triple is in this list for the same reason as the other
    // two: `cargo xtask release --target windows` is the only way to produce a
    // Windows binary, it writes to `target/x86_64-pc-windows-msvc/release/`, and
    // without this entry a Windows host never finds a binary it just built and
    // falls through to a redundant `cargo xtask release` (issue #367).
    //
    // `CARGO_TARGET_DIR` is searched first: a release build made with it set
    // lands there, never under `<root>/target`.
    let mut target_roots: Vec<PathBuf> =
        std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).into_iter().collect();
    target_roots.push(root.join("target"));
    let candidates: Vec<PathBuf> = target_roots
        .iter()
        .flat_map(|t| {
            [
                t.join("release").join(bin_name),
                t.join(format!("{}-unknown-linux-gnu", std::env::consts::ARCH))
                    .join("release")
                    .join(bin_name),
                t.join(format!("{}-apple-darwin", std::env::consts::ARCH))
                    .join("release")
                    .join(bin_name),
                t.join("x86_64-pc-windows-msvc").join("release").join(bin_name),
            ]
        })
        .collect();
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
    // Every host needs its own branch here. Falling through to the linux-gnu
    // triple on Windows asked cargo to cross-compile to a target that is not
    // installed, and the suite died on `can't find crate for core` (issue
    // #367) — a rustup message, in the middle of an e2e run, for a build that
    // was only ever meant to be native.
    let host_target = if cfg!(target_os = "macos") {
        format!("{}-apple-darwin", std::env::consts::ARCH)
    } else if cfg!(windows) {
        format!("{}-pc-windows-msvc", std::env::consts::ARCH)
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

/// Build every `ephpm-e2e` integration-test binary and return `(suite, path)`
/// pairs, one per `tests/*.rs` file.
///
/// The paths come from cargo's own JSON artifact messages rather than from
/// scanning a `deps/` directory. A scan has to guess the directory (it used to
/// assume `crates/ephpm-e2e/target`, which does not exist when
/// `CARGO_TARGET_DIR` is set) and the binary (the newest `<name>-<hash>`
/// file; a target dir shared with the main workspace holds that workspace's
/// test binaries too, under names a scan cannot tell apart).
fn build_test_binaries(manifest: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let output = Command::new("cargo")
        .args(["test", "--manifest-path"])
        .arg(manifest)
        .args(["--no-run", "--tests", "--message-format=json-render-diagnostics"])
        .stderr(Stdio::inherit())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!("cargo exited with {}", output.status)));
    }
    Ok(parse_test_executables(&String::from_utf8_lossy(&output.stdout)))
}

/// Pick the integration-test executables out of cargo's JSON message stream.
///
/// Only `compiler-artifact` messages whose target kind is exactly `test` (a
/// `tests/*.rs` file) count; the library's own unit-test binary is kind `lib`
/// and is skipped. xtask is dependency-free, so this reads just the three
/// fields it needs instead of parsing JSON in general.
fn parse_test_executables(messages: &str) -> Vec<(String, PathBuf)> {
    let mut by_name: BTreeMap<String, PathBuf> = BTreeMap::new();
    for line in messages.lines() {
        if !line.contains("\"reason\":\"compiler-artifact\"") {
            continue;
        }
        // The `target` object holds only scalars and arrays, so it ends at
        // the first `}` after it opens.
        let Some(start) = line.find("\"target\":{") else {
            continue;
        };
        let target = &line[start..];
        let target = &target[..target.find('}').unwrap_or(target.len())];
        if !target.contains("\"kind\":[\"test\"]") {
            continue;
        }
        let (Some(name), Some(exe)) =
            (json_string_field(target, "name"), json_string_field(line, "executable"))
        else {
            continue;
        };
        by_name.insert(name, PathBuf::from(exe));
    }
    by_name.into_iter().collect()
}

/// The value of the first `"key":"..."` string field in `json`, unescaped.
/// `None` when the key is absent or its value is not a string (e.g. `null`).
fn json_string_field(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let rest = &json[json.find(&needle)? + needle.len()..];
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
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

/// Marker file identifying a directory as this harness's scratch space.
const SCRATCH_MARKER: &str = ".ephpm-e2e-scratch";

/// Empty `path` and (re)create it as a scratch directory.
///
/// The default scratch dir is always wiped. A `--scratch-dir` the caller named
/// is wiped only when it is empty or carries [`SCRATCH_MARKER`] — i.e. when
/// this harness made it — so a typo cannot delete someone's directory.
fn prepare_scratch_dir(path: &Path, is_default: bool) -> io::Result<()> {
    if path.exists() {
        let ours = is_default
            || path.join(SCRATCH_MARKER).exists()
            || fs::read_dir(path)?.next().is_none();
        if !ours {
            return Err(io::Error::other(format!(
                "refusing to wipe {}: it is not empty and was not created by `cargo xtask e2e` \
                 (no {SCRATCH_MARKER})",
                path.display()
            )));
        }
        fs::remove_dir_all(path)?;
    }
    fs::create_dir_all(path)?;
    fs::write(path.join(SCRATCH_MARKER), "")
}

// ── config templates ────────────────────────────────────────────────────────

/// Single-node ephpm.toml template. Placeholders: `{HTTP_PORT}`, `{DATA_DIR}`,
/// `{DOCROOT}`, `{SITES_DIR_LINE}` (the whole `sites_dir = "..."` line, or
/// empty), `{DB_SQLITE_BLOCK}` (the whole `[db.sqlite]` section — see
/// [`SingleNodeOptions::with_sites_dir`]), `{KV_SECRET_LINE}` (a `[kv] secret`
/// line, or empty), `{OPCACHE_BLOCK}` (an optional `[opcache]` section, or
/// empty), `{WEBSOCKET_BLOCK}` (an optional `[server.websocket]` section, or
/// empty), `{MIDDLEWARE_BLOCK}` (an optional `[[middleware]]` mount, or empty),
/// `{MTH_LINE}` (the `multi_tenant_hardening = <bool>` line in
/// `[server.security]`), `{INI_OVERRIDES_EXTRA}` (extra `[php] ini_overrides`
/// rows — the hardening node's operator `disable_functions`, or empty),
/// `{CONCURRENCY_LINE}` (a pinned `[php] concurrency` line for the `fpm_pool`
/// and `crash_containment` nodes, or empty), `{CRASH_CONTAINMENT_LINE}` (the
/// `[php] crash_containment = true` line for the `crash_containment` node, or
/// empty), `{OVERLOAD_BLOCK}` (the load-shedding policy plus the tiny pool
/// sizing for the `load_shedding` node, or empty), `{KV_SOCKET}`, `{KV_PORT}`
/// (the RESP listener's reserved port).
///
/// Shape mirrors `tests/ephpm-test.toml` (the config baked into the container
/// image) as closely as possible. The only intentional divergences are:
/// (a) `listen`/`sites_dir`/`document_root`/`db.sqlite.*`/`kv.redis_compat.socket`
/// point at per-run scratch paths instead of the container's fixed
/// `/var/www/...` and `/tmp/...` layout; (b) every listener — HTTP, the
/// `[db.sqlite.proxy]` frontends, the KV RESP listener — is on a port
/// reserved for this node (see [`PORT_RANGE`]) instead of a fixed one.
///
/// # Why the `[db.sqlite]` and `[kv] secret` bits are a placeholder
///
/// Both are *forced* by `sites_dir`, in opposite directions, so a single
/// hard-coded shape cannot serve both kinds of node:
///
/// - **`[kv] secret`** — `Config::validate` refuses to start when `sites_dir`
///   is set, `[kv.redis_compat] enabled = true`, and no `[kv] secret` is
///   configured (the listener would serve one shared KV store to every tenant
///   unauthenticated). Setting a secret unconditionally is equally wrong: it
///   makes the RESP listener demand `AUTH`, and `opcache_invalidation` talks to
///   that listener unauthenticated.
/// - **`[db.sqlite]`** — with `sites_dir` set, single-node embedded SQLite runs
///   in per-site mode, where `dir` (one database file per vhost) is REQUIRED
///   and the only wire listener is the per-site-authenticating `mysql_listen`
///   (Hrana/PG/TDS are not started: they cannot bind a backend per
///   connection). So the docroot's single-database `pdo_mysql` scripts cannot
///   run on a `sites_dir` node, and a node whose suites use them cannot have a
///   `sites_dir`. `mysql_listen` is still pinned to the node's reserved port —
///   left at its default it would bind 127.0.0.1:3306.
///
/// On nodes WITHOUT `sites_dir` the docroot PHP scripts (`db.php`,
/// `sqlite_test.php`, `rw_split_test.php`, ...) reach the mysql proxy via
/// `getenv('DB_HOST')`/`getenv('DB_PORT')`, falling back to `127.0.0.1:3306`.
/// Single-site `[db.sqlite]` mode does NOT inject those into PHP (only
/// `[db.mysql]`/`[db.postgres]` and per-site mode do, see `build_db_env_vars`
/// in ephpm-server/src/router.rs), so the harness puts them in the node's
/// *process* environment, which PHP's `getenv()` reads — see
/// [`SingleNodeSpawn::start`].
const SINGLE_NODE_TEMPLATE: &str = r#"# Auto-generated by `cargo xtask e2e` -- do not edit.
[server]
listen = "127.0.0.1:{HTTP_PORT}"
document_root = "{DOCROOT}"
{SITES_DIR_LINE}
{PREVIEW_LINE}
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
    # Consumed only by the isolated `preview` node; inert elsewhere.
    "preview-hot.test",
    "preview-quiet.test",
    # Consumed only by the isolated `multitenant_hardening` node (which renders
    # from this same template); inert on every other node.
    "harden.test",
    # Consumed only by the isolated `websockets` node. Two vhosts, so the suite
    # can prove one tenant's connections are unreachable from the other.
    "ws-a.test",
    "ws-b.test",
    # Consumed only by the `request_teardown` suite's cross-tenant destructor
    # test (shared node, which runs multi-tenant). Inert elsewhere.
    "teardown-a.test",
    "teardown-b.test",
    # The same suite's KV-backed (`ephpm`) session case: an unmatched Host has
    # no KV keyspace on a multi-tenant node, so it runs under this vhost.
    "teardown-session.test",
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
{METRICS_TOKEN_LINE}

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
{MTH_LINE}

[php]
max_execution_time = 30
memory_limit = "128M"
ini_overrides = [
    ["display_errors", "On"],
    ["error_reporting", "E_ALL"],{INI_OVERRIDES_EXTRA}
]
{CONCURRENCY_LINE}
{CRASH_CONTAINMENT_LINE}
{OVERLOAD_BLOCK}
{PHP_WORKER_BLOCK}

[server.limits]
max_connections = 100
{LIMITS_BLOCK}

{DB_SQLITE_BLOCK}

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
{KV_SECRET_LINE}

[kv.redis_compat]
enabled = true
listen = "127.0.0.1:{KV_PORT}"
socket = "{KV_SOCKET}"
{OPCACHE_BLOCK}
{WEBSOCKET_BLOCK}
{MIDDLEWARE_BLOCK}
"#;

/// Cluster node template. Placeholders: `{HTTP_PORT}`, `{MYSQL_PORT}`,
/// `{KV_DATA_PORT}`, `{CHANNEL_PORT}`, `{NODE_ID}`, `{CLUSTER_BIND}`,
/// `{CLUSTER_JOIN}`, `{DATA_DIR}`, `{DOCROOT}`.
///
/// Clustered SQLite uses the in-process Turso CDC path (v0.7.0+): the engine
/// defaults to `turso`, `replication.role = "auto"` with `[cluster] enabled`
/// elects a primary via gossip, and the cluster channel carries CDC batches.
/// No sqld sidecar. Every address is `127.0.0.1:PORT` because all nodes share
/// the host loopback instead of having their own container IP.
///
/// `[cluster.channel] listen` is pinned to a reserved port rather than left
/// to its derived default (gossip port + 2), which nobody would otherwise
/// reserve — the `ephpm-e2e` fixtures learned that the hard way (#239).
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
mode = "per_request"
max_execution_time = 60
memory_limit = "256M"

[db.sqlite]
path = "{DATA_DIR}/wordpress.db"

[db.sqlite.proxy]
mysql_listen = "127.0.0.1:{MYSQL_PORT}"

[db.sqlite.replication]
role = "auto"

[cluster]
enabled = true
bind = "{CLUSTER_BIND}"
join = [{CLUSTER_JOIN}]
node_id = "{NODE_ID}"
cluster_id = "ephpm-e2e-bare"
secret = "bare-e2e-secret-do-not-use-in-prod-b7e4d3f0"

[cluster.channel]
listen = "127.0.0.1:{CHANNEL_PORT}"

[cluster.kv]
data_port = {KV_DATA_PORT}
"#;

// ── single-node fixture ─────────────────────────────────────────────────────

/// `[kv] secret` handed to every multi-tenant (`sites_dir`) node.
///
/// A fixed value is fine: these nodes are throwaway and their RESP listener is
/// bound to loopback plus a scratch unix socket. It exists only because the
/// server fails closed without it — see [`SingleNodeOptions::with_sites_dir`].
const E2E_KV_SECRET: &str = "e2e-kv-secret-not-for-production";

/// `[server.metrics] token` for every multi-site node. Must equal
/// `ephpm_e2e::E2E_METRICS_TOKEN` (the scrape helper sends it).
const E2E_METRICS_TOKEN: &str = "e2e-metrics-token-not-for-production";

/// Per-fixture single-node config knobs.
///
/// The isolated suites need small config divergences from the shared node;
/// this captures them so one `SingleNodeSpawn::start` covers every case.
struct SingleNodeOptions {
    /// Scratch subdirectory name (keeps logs/DBs from stomping each other when
    /// several fresh nodes are spawned sequentially). Also used for log labels.
    slug: String,
    /// Configure `[server] sites_dir` — i.e. run this node **multi-tenant**.
    ///
    /// Set on the long-lived shared node (for `vhosts` and `security_p0`) and
    /// on the isolated nodes that run multi-tenant: `multitenant_hardening`
    /// and `websockets` (whose subject *is* multi-tenancy) plus `preview`
    /// (whose per-site rate cap only acts on requests that resolve to a site
    /// key). Every other isolated node runs single-site. `EPHPM_SITES_DIR` is
    /// exported to the suite exactly when this is set.
    ///
    /// This one flag decides three things at once, because the server ties them
    /// together and fails closed on the mismatched combinations:
    ///
    /// 1. `[kv] secret` is emitted (required: multi-tenant RESP with no secret
    ///    is refused at startup) — and NOT emitted otherwise (a secret makes the
    ///    RESP listener demand `AUTH`, which `opcache_invalidation` does not
    ///    send).
    /// 2. `[db.sqlite]` uses `dir` (per-site database files, required in
    ///    multi-site mode) instead of `path`.
    /// 3. There are no SQLite wire listeners, because per-site mode does not
    ///    start them — so no suite that reaches the database over `pdo_mysql`
    ///    can run on a `sites_dir` node.
    ///
    /// It also moves the OPcache vhost key for a `127.0.0.1` request from
    /// `_DEFAULT` to the request host, which is why `opcache_invalidation`
    /// (which writes `opcache:version:_DEFAULT`) must not have it.
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
    /// Emit `[php] mode = "worker"` + `[php.worker] script`. Only the `worker_mode`
    /// suite; implies `with_sites_dir = false` (the combination hard-errors at
    /// config load) and the `tests/worker-docroot` document root.
    worker: bool,
    /// Turn the multi-tenant hardening preset ON for this node.
    ///
    /// Only the `multitenant_hardening` suite sets it. It does two coupled
    /// things the hardening test needs and every other node must NOT have:
    /// emits `[server.security] multi_tenant_hardening = true` (which, with
    /// `sites_dir` set, disables the OPcache introspection API — the very
    /// thing the `php_config` suite depends on, so the shared node keeps it
    /// off) and adds the operator's `disable_functions = getmypid` override to
    /// `[php] ini_overrides` so the suite can prove the clobber-union fix.
    /// Implies `with_sites_dir = true` (hardening is inert without it).
    multi_tenant_hardening: bool,
    /// Emit a pinned `[php] concurrency` line so the node's execution-pool
    /// size is deterministic rather than host-derived.
    ///
    /// Only the `fpm_pool` and `crash_containment` suites set it. Every other
    /// node leaves it `false`, so the rendered `{CONCURRENCY_LINE}` is empty
    /// and their pool size is the stock derived value.
    pin_pool_size: bool,
    /// Emit `[php] overload = "shed"` together with a one-thread,
    /// one-slot execution pool — the smallest configuration in which saturation
    /// (and therefore a 503) is reachable from a test with a handful of
    /// concurrent requests.
    ///
    /// Only the `load_shedding` suite sets it. Every other node leaves it
    /// `false`, so the rendered `{OVERLOAD_BLOCK}` is empty and their
    /// admission behaviour is the historical "queue and wait".
    overload_shed: bool,
    /// Emit `[php] crash_containment = true` — contain a PHP C-stack overflow
    /// (500 + retire the pool thread) instead of aborting the process.
    ///
    /// Only the `crash_containment` suite sets it (with a pinned pool size so
    /// the retire-and-replace assertions are deterministic). Its node
    /// is deliberately destroyed after the suite: threads are abandoned and PHP
    /// module shutdown is skipped once a crash has been contained.
    crash_containment: bool,
    /// Emit `[server] preview = true` — the preview-host preset. Only the
    /// `preview` suite sets it (see ISOLATED_CONFIG_SUITES): the preset adds a
    /// marker header to every response and rate-caps each site's PHP at the
    /// preset 5/s (the template's `[server.limits]` explicitly sets the per-IP
    /// and `max_connections` knobs, so only the per-site pair comes from the
    /// preset — deliberately, as an explicit-beats-preset proof). Implies
    /// `with_sites_dir = true`: the per-site cap only acts on requests that
    /// resolve to a site key.
    preview: bool,
    /// Emit `[server.websocket] enabled = true` — turn on native WebSockets.
    ///
    /// Only the `websockets` suite sets it. Every other node leaves it `false`,
    /// so `{WEBSOCKET_BLOCK}` renders empty and their behaviour is
    /// byte-identical to before this knob existed. That matters more here than
    /// for most flags: with the feature ON, **every** `Connection: Upgrade` +
    /// `Upgrade: websocket` request stops falling through to the fallback chain
    /// and 404s unless the vhost has a `websocket_files` entrypoint.
    ///
    /// Implies `with_sites_dir = true`. The suite's most valuable assertion is
    /// that one tenant cannot reach another's sockets, which needs two real
    /// vhosts on one server; running the whole suite multi-tenant is what makes
    /// that a single-node test rather than a second fixture.
    websocket: bool,
}

impl SingleNodeOptions {
    /// Options for the long-lived shared node (all non-isolated suites).
    ///
    /// This is the ONLY node with a `sites_dir`, because `vhosts` and
    /// `security_p0` are the only suites that need one.
    fn shared() -> Self {
        Self {
            slug: "single".to_string(),
            with_sites_dir: true,
            opcache_invalidation: false,
            tight_rate_limit: false,
            middleware_lib: None,
            worker: false,
            // Hardening is EXPLICITLY off here even though `sites_dir` is set
            // (which would otherwise resolve it on). The shared node hosts
            // `php_config`, whose opcache introspection test breaks under the
            // preset. Hardening coverage lives on the isolated
            // `multitenant_hardening` node instead.
            multi_tenant_hardening: false,
            pin_pool_size: false,
            overload_shed: false,
            crash_containment: false,
            preview: false,
            websocket: false,
        }
    }

    /// Options tailored to a specific isolated suite. `middleware_lib` is the
    /// cdylib built by [`build_middleware_cdylib`], present only when the
    /// `middleware` suite was selected.
    fn for_suite(name: &str, middleware_lib: Option<&Path>) -> Self {
        let slug = format!("single-{name}");
        match name {
            // opcache_invalidation: enable the watcher, drop sites_dir so the
            // default docroot resolves to the `_DEFAULT` OPcache vhost.
            "opcache_invalidation" => Self {
                slug,
                with_sites_dir: false,
                opcache_invalidation: true,
                tight_rate_limit: false,
                middleware_lib: None,
                worker: false,
                multi_tenant_hardening: false,
                pin_pool_size: false,
                overload_shed: false,
                crash_containment: false,
                preview: false,
                websocket: false,
            },
            // multitenant_hardening: the ONLY isolated node that runs
            // multi-tenant (sites_dir set) AND with the hardening preset on.
            // sites_dir gives it EPHPM_SITES_DIR (the suite deploys the
            // `harden.test` vhost into it) and resolves hardening on; the flag
            // additionally injects the operator's `disable_functions=getmypid`
            // override so the union/clobber fix is exercised. Kept off the
            // shared node because the preset disables opcache introspection,
            // which `php_config` asserts is live.
            "multitenant_hardening" => Self {
                slug,
                with_sites_dir: true,
                opcache_invalidation: false,
                tight_rate_limit: false,
                middleware_lib: None,
                worker: false,
                multi_tenant_hardening: true,
                pin_pool_size: false,
                overload_shed: false,
                crash_containment: false,
                preview: false,
                websocket: false,
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
                multi_tenant_hardening: false,
                pin_pool_size: false,
                overload_shed: false,
                crash_containment: false,
                preview: false,
                websocket: false,
            },
            // rate_limit: small bucket, on its own node. See
            // ISOLATED_CONFIG_SUITES for why this can't share the shared node.
            "rate_limit" => Self {
                slug,
                with_sites_dir: false,
                opcache_invalidation: false,
                tight_rate_limit: true,
                middleware_lib: None,
                worker: false,
                multi_tenant_hardening: false,
                pin_pool_size: false,
                overload_shed: false,
                crash_containment: false,
                preview: false,
                websocket: false,
            },
            // middleware: mount the freshly built cors cdylib by explicit
            // path, exercising the dlopen lane in a real server.
            "middleware" => Self {
                slug,
                with_sites_dir: false,
                opcache_invalidation: false,
                tight_rate_limit: false,
                middleware_lib: middleware_lib.map(Path::to_path_buf),
                worker: false,
                multi_tenant_hardening: false,
                pin_pool_size: false,
                overload_shed: false,
                crash_containment: false,
                preview: false,
                websocket: false,
            },
            // fpm_pool: the dedicated per-request execution pool with a
            // PINNED size (`[php] concurrency = 4`). Single-site so it gets the
            // standard `[db.sqlite]` path + wire listeners and the default
            // docroot's `db_bridge_test.php`, letting the suite prove the pool
            // runs the full per-request path (PHP + the `ephpm_db_*` bridge)
            // end to end — not just plumbing. It keeps its OWN node because
            // pool sizing is a whole-server switch; bolting it onto the shared
            // template would silently resize every other suite's pool
            // (the #295 lesson: never mutate the shared node's config).
            "fpm_pool" => Self {
                slug,
                with_sites_dir: false,
                opcache_invalidation: false,
                tight_rate_limit: false,
                middleware_lib: None,
                worker: false,
                multi_tenant_hardening: false,
                pin_pool_size: true,
                overload_shed: false,
                crash_containment: false,
                preview: false,
                websocket: false,
            },
            // crash_containment: the ONLY node with `[php] crash_containment =
            // true`. This suite deliberately crashes PHP: it drives a C-stack
            // overflow that poisons a pool thread, so the node ends its life
            // with abandoned TSRM entries and skips PHP module shutdown at
            // exit. Nothing else may share it, and it must be torn down after.
            // Single-site (no sites_dir) so the standard docroot — including
            // `stack_overflow.php` and `sleep.php` — is served.
            "crash_containment" => Self {
                slug,
                with_sites_dir: false,
                opcache_invalidation: false,
                tight_rate_limit: false,
                middleware_lib: None,
                worker: false,
                multi_tenant_hardening: false,
                pin_pool_size: true,
                overload_shed: false,
                crash_containment: true,
                preview: false,
                websocket: false,
            },
            // preview: the ONLY node with `[server] preview = true`. Runs
            // multi-tenant (sites_dir set) because the per-site PHP rate cap —
            // the load-bearing thing the suite asserts — only acts on requests
            // that resolve to a canonical site key. The template's explicit
            // `[server.limits]` values (max_connections = 100 and the generous
            // per-IP bucket) deliberately stay in place: the suite thereby also
            // proves end-to-end that explicit values beat the preset, while the
            // per-site pair (left unset) comes from the preset (5/s, burst 20).
            "preview" => Self {
                slug,
                with_sites_dir: true,
                opcache_invalidation: false,
                tight_rate_limit: false,
                middleware_lib: None,
                worker: false,
                multi_tenant_hardening: false,
                pin_pool_size: false,
                overload_shed: false,
                crash_containment: false,
                preview: true,
                websocket: false,
            },
            // websockets: the ONLY node with `[server.websocket] enabled`.
            // Multi-tenant (`sites_dir`) so the suite gets two real vhosts and
            // can prove a connection opened on one is unreachable from the
            // other — the property the whole site-scoped registry exists for.
            // Hardening stays off: it is orthogonal here, and leaving it on
            // would couple this suite to the preset's behaviour.
            "websockets" => Self {
                slug,
                with_sites_dir: true,
                opcache_invalidation: false,
                tight_rate_limit: false,
                middleware_lib: None,
                worker: false,
                multi_tenant_hardening: false,
                pin_pool_size: false,
                overload_shed: false,
                crash_containment: false,
                preview: false,
                websocket: true,
            },
            // load_shedding: the ONLY node with `[php] overload =
            // "shed"`. Runs the execution pool deliberately undersized — one
            // thread, one queue slot — so a burst of concurrent requests
            // reaches saturation (and therefore a real 503 over a real socket)
            // in a couple of seconds instead of needing a load generator.
            // Single-site so the docroot's `sleep.php` is reachable and no
            // per-site rate cap can produce a 429 that masquerades as shedding.
            "load_shedding" => Self {
                slug,
                with_sites_dir: false,
                opcache_invalidation: false,
                tight_rate_limit: false,
                middleware_lib: None,
                worker: false,
                multi_tenant_hardening: false,
                // Sizing comes from the overload block (`concurrency = 1`);
                // pinning here too would render a duplicate key.
                pin_pool_size: false,
                overload_shed: true,
                crash_containment: false,
                preview: false,
                websocket: false,
            },
            // SINGLE_SITE_KV_SUITES also land here: single-site is the whole
            // point (the default docroot only has a KV keyspace without
            // `sites_dir`).
            //
            // DB suites: fresh DB, single-site. `sites_dir` is NOT optional
            // detail here — it would put the node in per-site database mode,
            // which does not start the SQLite wire listeners at all, and every
            // one of these suites reaches the database over `pdo_mysql` on
            // 127.0.0.1:3306 (or, for `db_bridge`, through the process-wide
            // backend the single-site path registers with the PHP bridge).
            _ => Self {
                slug,
                with_sites_dir: false,
                opcache_invalidation: false,
                tight_rate_limit: false,
                middleware_lib: None,
                worker: false,
                multi_tenant_hardening: false,
                pin_pool_size: false,
                overload_shed: false,
                crash_containment: false,
                preview: false,
                websocket: false,
            },
        }
    }
}

/// The ports one single node listens on, all reserved together.
#[derive(Clone, Copy, Debug)]
struct SingleNodePorts {
    http: u16,
    mysql: u16,
    hrana: u16,
    pg: u16,
    tds: u16,
    kv: u16,
}

impl SingleNodePorts {
    const KINDS: [PortKind; 6] = [PortKind::Tcp; 6];

    fn from_reserved(ports: &[u16]) -> Self {
        Self {
            http: ports[0],
            mysql: ports[1],
            hrana: ports[2],
            pg: ports[3],
            tds: ports[4],
            kv: ports[5],
        }
    }
}

struct SingleNodeSpawn {
    child: Child,
    http_port: u16,
    /// The KV RESP listener's port, exported as `EPHPM_KV_PORT`.
    kv_port: u16,
    /// The Hrana listener's port, exported as `EPHPM_HRANA_URL`. `None` on a
    /// `sites_dir` node, which does not start Hrana.
    hrana_port: Option<u16>,
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
    /// This node runs `[server] preview = true`; `env()` additionally exports
    /// `EPHPM_PREVIEW_URL`, which is what the `preview` suite gates on — so it
    /// can never be pointed at a non-preview node by accident (same pattern
    /// as `EPHPM_WORKER_URL`).
    preview: bool,
}

impl SingleNodeSpawn {
    /// Spawn a node on freshly reserved ports and wait until it is serving.
    ///
    /// A node that loses one of its ports to another process before binding
    /// it is torn down and respawned on a new set, up to [`SPAWN_ATTEMPTS`]
    /// times. Any other startup failure is returned at once, after the node's
    /// logs are dumped. Either way the child is never left running.
    fn start(
        binary: &Path,
        docroot: &Path,
        scratch_root: &Path,
        opts: &SingleNodeOptions,
    ) -> io::Result<Self> {
        let mut picker = PortPicker::new();
        let mut last_collision = String::new();
        for attempt in 1..=SPAWN_ATTEMPTS {
            match Self::try_start(binary, docroot, scratch_root, opts, &mut picker) {
                Ok(node) => return Ok(node),
                Err(StartFailure::PortTaken(line)) => {
                    eprintln!(
                        "    [{}] lost a reserved port (attempt {attempt}/{SPAWN_ATTEMPTS}): \
                         {line} -- respawning on fresh ports",
                        opts.slug
                    );
                    last_collision = line;
                }
                Err(StartFailure::Fatal(e)) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "[{}] lost a reserved port on all {SPAWN_ATTEMPTS} attempts; last: {last_collision}",
                opts.slug
            ),
        ))
    }

    fn try_start(
        binary: &Path,
        docroot: &Path,
        scratch_root: &Path,
        opts: &SingleNodeOptions,
        picker: &mut PortPicker,
    ) -> Result<Self, StartFailure> {
        let reservation = picker.reserve(&SingleNodePorts::KINDS)?;
        let ports = SingleNodePorts::from_reserved(reservation.ports());
        let http_port = ports.http;

        // A fresh directory per attempt: a respawn must not inherit the
        // previous attempt's database or logs.
        let node_dir = scratch_root.join(&opts.slug);
        if node_dir.exists() {
            fs::remove_dir_all(&node_dir)?;
        }
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
        // Multi-tenant hardening: ON only for the `multitenant_hardening`
        // suite's node. Everywhere else it is EXPLICITLY off, so the preset
        // never disables OPcache introspection on a node another suite shares.
        // The `false` line is inert on non-`sites_dir` nodes but load-bearing
        // on the shared node — that node sets `sites_dir`, so an unset value
        // would resolve the preset ON and break `php_config`'s opcache test.
        let mth_line = if opts.multi_tenant_hardening {
            "multi_tenant_hardening = true"
        } else {
            "multi_tenant_hardening = false"
        };
        // Operator-supplied `disable_functions` entry, added to the hardening
        // node ONLY. multitenant_hardening.rs asserts getmypid stays disabled
        // *alongside* the hardening baseline (the union/clobber fix). Emitting
        // it on other nodes would strip getmypid from suites that never opt in.
        let ini_overrides_extra = if opts.multi_tenant_hardening {
            "\n    [\"disable_functions\", \"getmypid\"],"
        } else {
            ""
        };
        // Only the rate_limit suite runs a bucket small enough to trip. Every
        // other node gets a budget large enough that the whole suite's traffic
        // never reaches it, which is what stops the spurious 429s.
        // Worker mode: a small, fixed pool so `boot #B` ids stay bounded and
        // the suite can tell "no worker was recycled" from "a worker rebooted".
        // worker_max_requests is left generous on purpose — the recycle test
        // asserts requests keep succeeding, not that a recycle happens.
        // Pinned execution-pool size. Empty (derived `[php] concurrency`) on
        // every node except the `fpm_pool` and `crash_containment` suites',
        // whose assertions want a deterministic pool size rather than a
        // host-derived one. Never combines with the worker block below
        // (worker-mode nodes derive their own count) or the overload block
        // (which pins its own 1-thread sizing).
        let concurrency_line = if opts.pin_pool_size { "concurrency = 4" } else { "" };
        // Stack-overflow crash containment. Empty everywhere except the
        // `crash_containment` node, so no other suite's crash semantics change
        // (a PHP segfault still kills the process for everyone else, which is
        // what several suites' expectations are built on).
        let crash_containment_line =
            if opts.crash_containment { "crash_containment = true" } else { "" };
        // Preview preset: ON only for the `preview` suite's node. Empty on
        // every other node, so their rendered [server] block is byte-identical
        // to before the knob existed — the preset's marker header and per-site
        // cap must not perturb suites that assert exact header sets or fire
        // bursts of PHP requests.
        let preview_line = if opts.preview { "preview = true" } else { "" };
        // Load shedding: ON only for the `load_shedding` suite's node, and
        // deliberately at the smallest possible scale. One pool thread plus one
        // backlog slot means the third concurrent request has nowhere to go, so
        // the suite can observe a genuine overload 503 without a load generator.
        // `shed_after_ms = 0` keeps it deterministic: no grace window means no
        // timing race between "shed" and "a slot freed up in the meantime".
        let overload_block = if opts.overload_shed {
            "overload = \"shed\"\nshed_after_ms = 0\nconcurrency = 1\nqueue_depth = 1"
        } else {
            ""
        };
        // NOTE: `[php.worker]` opens a subsection, so this block must stay the
        // LAST thing rendered inside `[php]` (the template's next line opens
        // `[server.limits]`).
        let php_worker_block = if opts.worker {
            "mode = \"worker\"\nconcurrency = 3\n\n[php.worker]\nscript = \"worker.php\"\n\
             max_requests = 10000\nboot_timeout = 60"
        } else {
            ""
        };
        // Native WebSockets: ON only for the `websockets` suite's node. The
        // bounds are deliberately small so the suite can reach them cheaply —
        // a 4-frame send queue and a 64 KiB message cap — while the ping/idle
        // pair stays wide enough that a slow test run never trips the keepalive
        // and produces a flake.
        let websocket_block = if opts.websocket {
            "\n[server.websocket]\nenabled = true\nsend_queue = 4\n\
             max_message_size = 65536\nmax_frame_size = 65536\n\
             ping_interval_secs = 5\nidle_timeout_secs = 120"
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
        // `sites_dir` forces multi-tenant mode, and multi-tenant mode forces
        // BOTH of the following (see SINGLE_NODE_TEMPLATE's doc comment for the
        // full reasoning). Deriving them from the same flag is what keeps a node
        // from being configured into a combination the server fails closed on.
        let (db_sqlite_block, kv_secret_line) = if opts.with_sites_dir {
            // Per-site database isolation: one file per vhost under `dir`.
            // `dir` is REQUIRED here — the server refuses to start multi-site
            // with only `path`, which would be one shared database for every
            // tenant. Only `mysql_listen` is configured: it is the one wire
            // listener per-site mode starts (credential-routed per tenant), and
            // its default would be the fixed 127.0.0.1:3306. Hrana/PG/TDS are
            // not started in this mode (one listener cannot route by virtual
            // host), so configuring them would only earn an "IGNORED" warning.
            (
                format!(
                    "[db.sqlite]\ndir = \"{}\"\n\n\
                     [db.sqlite.proxy]\n\
                     mysql_listen = \"127.0.0.1:{}\"",
                    escape_toml(&data_dir.join("sites-db")),
                    ports.mysql
                ),
                format!("secret = \"{E2E_KV_SECRET}\""),
            )
        } else {
            // Single-site: one database file, reachable over every wire
            // listener, each on its own reserved port.
            (
                format!(
                    "[db.sqlite]\npath = \"{}/ephpm-test.db\"\n\n\
                     [db.sqlite.proxy]\n\
                     mysql_listen = \"127.0.0.1:{}\"\n\
                     hrana_listen = \"127.0.0.1:{}\"\n\
                     postgres_listen = \"127.0.0.1:{}\"\n\
                     tds_listen = \"127.0.0.1:{}\"",
                    escape_toml(&data_dir),
                    ports.mysql,
                    ports.hrana,
                    ports.pg,
                    ports.tds
                ),
                // Deliberately NO `[kv] secret`: without a `sites_dir` the
                // shared KV store is the intended behaviour, and a secret would
                // switch the RESP listener to requiring `AUTH` — which
                // `opcache_invalidation` (the one suite that speaks RESP
                // directly) does not send.
                String::new(),
            )
        };

        // A multi-site node refuses `/metrics` (403) unless a credential is
        // configured: every tenant's PHP can reach the listener. The e2e
        // scrape helper (`ephpm_e2e::get_metrics`) always sends this token.
        let metrics_token_line = if opts.with_sites_dir {
            format!("token = \"{E2E_METRICS_TOKEN}\"")
        } else {
            String::new()
        };

        let config = SINGLE_NODE_TEMPLATE
            .replace("{HTTP_PORT}", &http_port.to_string())
            .replace("{SITES_DIR_LINE}", &sites_dir_line)
            .replace("{PREVIEW_LINE}", preview_line)
            .replace("{DB_SQLITE_BLOCK}", &db_sqlite_block)
            .replace("{KV_SECRET_LINE}", &kv_secret_line)
            .replace("{METRICS_TOKEN_LINE}", &metrics_token_line)
            .replace("{OPCACHE_BLOCK}", &opcache_block)
            .replace("{WEBSOCKET_BLOCK}", websocket_block)
            .replace("{MIDDLEWARE_BLOCK}", &middleware_block)
            .replace("{LIMITS_BLOCK}", limits_block)
            .replace("{MTH_LINE}", mth_line)
            .replace("{INI_OVERRIDES_EXTRA}", ini_overrides_extra)
            .replace("{CONCURRENCY_LINE}", concurrency_line)
            .replace("{CRASH_CONTAINMENT_LINE}", crash_containment_line)
            .replace("{OVERLOAD_BLOCK}", overload_block)
            .replace("{PHP_WORKER_BLOCK}", php_worker_block)
            .replace("{KV_SOCKET}", &escape_toml(&kv_socket))
            .replace("{KV_PORT}", &ports.kv.to_string())
            .replace("{DOCROOT}", &escape_toml(docroot));

        let config_path = node_dir.join("ephpm.toml");
        fs::write(&config_path, config)?;

        let logs =
            NodeLogs { stdout: node_dir.join("stdout.log"), stderr: node_dir.join("stderr.log") };
        let stdout_file = fs::File::create(&logs.stdout)?;
        let stderr_file = fs::File::create(&logs.stderr)?;

        eprintln!(
            "    spawning single-node ephpm [{}] on 127.0.0.1:{http_port} (config: {})",
            opts.slug,
            config_path.display()
        );
        let mut cmd = Command::new(binary);
        cmd.args(["serve", "--config"])
            .arg(&config_path)
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file));
        if !opts.with_sites_dir {
            // The docroot's pdo_mysql scripts find the proxy through
            // `getenv('DB_HOST')`/`getenv('DB_PORT')`, which reads this
            // process environment; single-site `[db.sqlite]` injects nothing
            // itself. See SINGLE_NODE_TEMPLATE.
            cmd.env("DB_HOST", "127.0.0.1").env("DB_PORT", ports.mysql.to_string());
        }
        // Hand the ports over: drop the probes and spawn in the same breath.
        reservation.release();
        let child = cmd.spawn()?;

        // From here on the child is owned by `node`, whose Drop kills it: an
        // early return can no longer orphan a half-started ephpm that later
        // binds its ports and answers the next fixture's health checks.
        let mut node = Self {
            child,
            http_port,
            kv_port: ports.kv,
            hrana_port: (!opts.with_sites_dir).then_some(ports.hrana),
            _data_dir: data_dir,
            sites_dir,
            docroot: docroot.to_path_buf(),
            label: opts.slug.clone(),
            binary: binary.to_path_buf(),
            middleware_lib: opts.middleware_lib.clone(),
            stderr_log: logs.stderr.clone(),
            stdout_log: logs.stdout.clone(),
            worker: opts.worker,
            preview: opts.preview,
        };
        let ready = node.await_ready(&logs, opts.worker);
        if let Err(StartFailure::Fatal(_)) = &ready {
            node.dump_logs();
        }
        ready?;
        eprintln!("    single-node ephpm [{}] ready on 127.0.0.1:{http_port}", opts.slug);
        Ok(node)
    }

    /// Wait until this node serves HTTP on its own port, its KV RESP listener
    /// is bound, and (in worker mode) a worker has booted.
    fn await_ready(&mut self, logs: &NodeLogs, worker: bool) -> Result<(), StartFailure> {
        // Generous on purpose: the wait ends the moment the node answers (or
        // exits), so only a node that is alive but stuck pays for it. A busy
        // CI host has been measured taking >10s to start a node; a deadline
        // shorter than that turns "slow" into "failed". Worker mode boots the
        // framework in every worker first, hence longer still.
        let health_timeout = if worker { Duration::from_secs(90) } else { Duration::from_secs(60) };
        wait_until_serving(&mut self.child, logs, self.http_port, health_timeout)?;
        // The RESP listener's bind failure is logged, not fatal — the node
        // keeps serving HTTP without it — so it needs its own check.
        wait_for_listener(
            &mut self.child,
            logs,
            "RESP server listening",
            self.kv_port,
            Duration::from_secs(15),
        )?;
        if worker {
            // `/_ephpm/health` is pure liveness (200 as soon as the listener is
            // up), so on a worker node it says nothing about whether any worker
            // has finished booting the framework. Wait for a real request to be
            // served before handing the node to the suite, otherwise the first
            // test races the boot and sees a 504.
            wait_for_worker(self.http_port, health_timeout)?;
        }
        Ok(())
    }

    fn dump_logs(&self) {
        dump_log(&format!("single-node [{}] stderr", self.label), &self.stderr_log);
        dump_log(
            &format!("single-node [{}] stdout (last 200 lines)", self.label),
            &self.stdout_log,
        );
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
            // The RESP listener, for suites that speak RESP directly
            // (`opcache_invalidation` writes its version key through it).
            ("EPHPM_KV_HOST".to_string(), "127.0.0.1".to_string()),
            ("EPHPM_KV_PORT".to_string(), self.kv_port.to_string()),
        ];
        // `hrana` probes the Hrana HTTP API directly when this is set.
        if let Some(port) = self.hrana_port {
            env.push(("EPHPM_HRANA_URL".to_string(), format!("http://127.0.0.1:{port}")));
        }
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
        // switch -- a suite that sees it starts spawning real ephpm processes.
        // `bare_process_smoke` is the one that does; since #239 it gets the
        // variable deliberately, via SELF_MANAGED_SUITES, together with the
        // `EPHPM_DOCROOT` it needs (it cannot derive the docroot itself --
        // pre-built test binaries inherit *xtask's* `CARGO_MANIFEST_DIR`).
        // Setting `EPHPM_BINARY` globally would hand that switch to every
        // suite instead, including ones that share this node.
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
        // Same pattern for the preview suite: it gates on EPHPM_PREVIEW_URL so
        // it self-skips anywhere its `[server] preview = true` node isn't up.
        if self.preview {
            env.push((
                "EPHPM_PREVIEW_URL".to_string(),
                format!("http://127.0.0.1:{}", self.http_port),
            ));
        }
        env
    }

    fn dump_on_failure(&self, failed: &[String]) {
        if failed.is_empty() {
            return;
        }
        self.dump_logs();
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
    logs: NodeLogs,
}

/// The ports one cluster node listens on.
#[derive(Clone, Copy, Debug)]
struct ClusterNodePorts {
    http: u16,
    mysql: u16,
    kv_data: u16,
    channel: u16,
    /// UDP: chitchat gossip.
    gossip: u16,
}

impl ClusterNodePorts {
    const KINDS: [PortKind; 5] =
        [PortKind::Tcp, PortKind::Tcp, PortKind::Tcp, PortKind::Tcp, PortKind::Udp];
}

/// How long each cluster node gets to answer `/_ephpm/health`. Matches
/// `ephpm-e2e`'s `CLUSTER_HEALTH_TIMEOUT`, which was sized from measured
/// cold starts (three nodes starting at once on a loaded host); the old 15s
/// sat inside that spread. Costs nothing on a healthy node, and a node that
/// dies returns at once.
const CLUSTER_HEALTH_TIMEOUT: Duration = Duration::from_secs(90);

impl ClusterSpawn {
    /// Spawn a `size`-node cluster on freshly reserved ports. Retries on a
    /// lost port exactly like [`SingleNodeSpawn::start`]; every node of a
    /// failed attempt is killed before the next one.
    fn start(binary: &Path, docroot: &Path, scratch_root: &Path, size: usize) -> io::Result<Self> {
        assert!(size >= 2, "cluster fixture needs at least 2 nodes");
        let mut picker = PortPicker::new();
        let mut last_collision = String::new();
        for attempt in 1..=SPAWN_ATTEMPTS {
            match Self::try_start(binary, docroot, scratch_root, size, &mut picker) {
                Ok(cluster) => return Ok(cluster),
                Err(StartFailure::PortTaken(line)) => {
                    eprintln!(
                        "    cluster lost a reserved port (attempt {attempt}/{SPAWN_ATTEMPTS}): \
                         {line} -- respawning every node on fresh ports"
                    );
                    last_collision = line;
                }
                Err(StartFailure::Fatal(e)) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "cluster lost a reserved port on all {SPAWN_ATTEMPTS} attempts; last: \
                 {last_collision}"
            ),
        ))
    }

    fn try_start(
        binary: &Path,
        docroot: &Path,
        scratch_root: &Path,
        size: usize,
        picker: &mut PortPicker,
    ) -> Result<Self, StartFailure> {
        // One reservation for the whole cluster, so no two nodes can be handed
        // the same port, held until the moment the nodes are spawned.
        let kinds: Vec<PortKind> = (0..size).flat_map(|_| ClusterNodePorts::KINDS).collect();
        let reservation = picker.reserve(&kinds)?;
        let node_ports: Vec<ClusterNodePorts> = reservation
            .ports()
            .chunks(ClusterNodePorts::KINDS.len())
            .map(|p| ClusterNodePorts {
                http: p[0],
                mysql: p[1],
                kv_data: p[2],
                channel: p[3],
                gossip: p[4],
            })
            .collect();

        // Every node lists all peers as join seeds (chitchat de-dupes; giving
        // it all peers up-front means startup order does not matter).
        let join_addrs: Vec<String> =
            node_ports.iter().map(|p| format!("127.0.0.1:{}", p.gossip)).collect();

        let mut commands = Vec::with_capacity(size);
        for (i, ports) in node_ports.iter().enumerate() {
            let node_dir = scratch_root.join(format!("cluster-node-{i}"));
            if node_dir.exists() {
                fs::remove_dir_all(&node_dir)?;
            }
            let data_dir = node_dir.join("data");
            fs::create_dir_all(&data_dir)?;

            let cluster_join = join_addrs
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, a)| format!("\"{a}\""))
                .collect::<Vec<_>>()
                .join(", ");

            let config = CLUSTER_NODE_TEMPLATE
                .replace("{HTTP_PORT}", &ports.http.to_string())
                .replace("{MYSQL_PORT}", &ports.mysql.to_string())
                .replace("{KV_DATA_PORT}", &ports.kv_data.to_string())
                .replace("{CHANNEL_PORT}", &ports.channel.to_string())
                .replace("{NODE_ID}", &format!("bare-node-{i}"))
                .replace("{CLUSTER_BIND}", &format!("127.0.0.1:{}", ports.gossip))
                .replace("{CLUSTER_JOIN}", &cluster_join)
                .replace("{DATA_DIR}", &escape_toml(&data_dir))
                .replace("{DOCROOT}", &escape_toml(docroot));

            let config_path = node_dir.join("ephpm.toml");
            fs::write(&config_path, config)?;

            let logs = NodeLogs {
                stdout: node_dir.join("stdout.log"),
                stderr: node_dir.join("stderr.log"),
            };
            let mut cmd = Command::new(binary);
            cmd.args(["serve", "--config"])
                .arg(&config_path)
                .stdout(Stdio::from(fs::File::create(&logs.stdout)?))
                .stderr(Stdio::from(fs::File::create(&logs.stderr)?));
            eprintln!(
                "    spawning cluster node {i} on 127.0.0.1:{} (config: {})",
                ports.http,
                config_path.display()
            );
            commands.push((cmd, ports.http, logs));
        }

        // Hand the ports over: drop the probes, then spawn every node at once.
        reservation.release();
        let mut cluster = Self { nodes: Vec::with_capacity(size) };
        for (mut cmd, http_port, logs) in commands {
            // Pushed as soon as it exists, so the cluster's Drop kills it on
            // any early return below.
            cluster.nodes.push(ClusterNode { child: cmd.spawn()?, http_port, logs });
        }

        let ready = cluster.await_ready();
        if let Err(StartFailure::Fatal(_)) = &ready {
            cluster.dump_logs();
        }
        ready?;
        eprintln!("    all {size} cluster nodes ready");
        Ok(cluster)
    }

    fn await_ready(&mut self) -> Result<(), StartFailure> {
        for node in &mut self.nodes {
            wait_until_serving(
                &mut node.child,
                &node.logs,
                node.http_port,
                CLUSTER_HEALTH_TIMEOUT,
            )?;
        }
        // Gossip, channel and KV-data-plane bind failures are logged rather
        // than fatal, so a node can be healthy over HTTP with a port lost.
        for node in &self.nodes {
            if let Some(line) = bind_failure_line(&node.logs.read()) {
                return Err(StartFailure::PortTaken(line));
            }
        }
        Ok(())
    }

    fn dump_logs(&self) {
        for (i, node) in self.nodes.iter().enumerate() {
            dump_log(&format!("cluster node {i} stderr"), &node.logs.stderr);
            dump_log(&format!("cluster node {i} stdout (last 200 lines)"), &node.logs.stdout);
        }
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
        self.dump_logs();
    }
}

impl Drop for ClusterSpawn {
    fn drop(&mut self) {
        for node in &mut self.nodes {
            terminate(&mut node.child);
        }
    }
}

// ── loopback port allocation ───────────────────────────────────────────────

/// The range (half-open) every node listener's port is drawn from, at random.
///
/// It sits below the ephemeral range of every host this runs on (Linux
/// 32768-60999, Windows and macOS 49152-65535), so a port the harness picked
/// is never handed to an unrelated *outgoing* connection in the gap between
/// the probe releasing it and the node binding it. That gap is what makes
/// `bind(":0")` reservations racy on a busy host (`PortLease` in `ephpm-e2e`,
/// #438). It also stays clear of the 20000-43999 host ports the
/// `db-integration` and `pdo-mysql-e2e` workflows publish database containers
/// on. A port something else already holds fails its probe and is skipped.
const PORT_RANGE: std::ops::Range<u16> = 10_000..20_000;

/// How many candidate ports one reservation may try per port before failing.
const PORT_PROBE_ATTEMPTS: usize = 256;

/// How many times a node (or a whole cluster) is respawned on fresh ports
/// after losing one of its reserved ports to another process.
///
/// Probing closes most of the window, not all of it: between the probe
/// releasing a port and the node binding it, anything else on the host can
/// take it. The node then fails its bind, the harness notices (the node exits
/// or logs the failure — see [`bind_failure_line`]) and tries again.
const SPAWN_ATTEMPTS: usize = 5;

/// The protocol a reserved port will be bound with. A TCP probe says nothing
/// about UDP availability and vice versa, so each port is probed as what it
/// will be.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PortKind {
    Tcp,
    /// Chitchat gossip.
    Udp,
}

/// Picks random candidate ports from [`PORT_RANGE`].
///
/// xorshift64*, seeded from std's per-process random hasher keys plus the pid
/// and the clock: two harness runs on one host start from unrelated points,
/// so they do not walk the range in lockstep and keep colliding. xtask is
/// dependency-free, hence no `rand`.
struct PortPicker {
    state: u64,
}

impl PortPicker {
    fn new() -> Self {
        use std::hash::{BuildHasher, Hasher};
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u32(std::process::id());
        h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        // xorshift must never hold an all-zero state.
        Self { state: h.finish() | 1 }
    }

    fn candidate(&mut self) -> u16 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        let span = u64::from(PORT_RANGE.end - PORT_RANGE.start);
        let offset = x.wrapping_mul(0x2545_F491_4F6C_DD1D) % span;
        PORT_RANGE.start + u16::try_from(offset).expect("offset < span <= u16::MAX")
    }

    /// Reserve one distinct port per entry in `kinds`, each **held** by a
    /// bound probe socket until [`PortReservation::release`].
    fn reserve(&mut self, kinds: &[PortKind]) -> io::Result<PortReservation> {
        let mut res = PortReservation { ports: Vec::new(), tcp: Vec::new(), udp: Vec::new() };
        for kind in kinds {
            let mut reserved = false;
            for _ in 0..PORT_PROBE_ATTEMPTS {
                let port = self.candidate();
                if res.ports.contains(&port) {
                    continue;
                }
                let held = match kind {
                    PortKind::Tcp => {
                        TcpListener::bind(("127.0.0.1", port)).map(|s| res.tcp.push(s))
                    }
                    PortKind::Udp => UdpSocket::bind(("127.0.0.1", port)).map(|s| res.udp.push(s)),
                };
                if held.is_ok() {
                    res.ports.push(port);
                    reserved = true;
                    break;
                }
            }
            if !reserved {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!(
                        "no bindable loopback {kind:?} port in {}..{} after {PORT_PROBE_ATTEMPTS} \
                         tries",
                        PORT_RANGE.start, PORT_RANGE.end
                    ),
                ));
            }
        }
        Ok(res)
    }
}

/// Ports reserved by [`PortPicker::reserve`], held open until handed off.
///
/// The probes are `std` sockets, which are close-on-exec on Unix and
/// non-inheritable on Windows, so one held across a `spawn` does not leak into
/// the child (see the note on `PortProbes` in `ephpm-e2e`).
struct PortReservation {
    ports: Vec<u16>,
    tcp: Vec<TcpListener>,
    udp: Vec<UdpSocket>,
}

impl PortReservation {
    /// The reserved ports, in the order their kinds were requested.
    fn ports(&self) -> &[u16] {
        &self.ports
    }

    /// Close the probes so the node about to be spawned can bind the ports.
    /// Call it immediately before `spawn`.
    fn release(self) {
        drop(self.tcp);
        drop(self.udp);
    }
}

// ── node startup ───────────────────────────────────────────────────────────

/// Why a node did not come up.
#[derive(Debug)]
enum StartFailure {
    /// The node lost one of its reserved ports to another process (the
    /// offending log line). Recoverable: respawn on fresh ports.
    PortTaken(String),
    /// Anything else. Returned to the caller as-is.
    Fatal(io::Error),
}

impl From<io::Error> for StartFailure {
    fn from(e: io::Error) -> Self {
        Self::Fatal(e)
    }
}

impl From<StartFailure> for io::Error {
    fn from(f: StartFailure) -> Self {
        match f {
            StartFailure::PortTaken(line) => {
                io::Error::new(io::ErrorKind::AddrInUse, format!("reserved port taken: {line}"))
            }
            StartFailure::Fatal(e) => e,
        }
    }
}

/// A node's two log files. ePHPm's tracing output goes to stdout; a fatal
/// startup error (e.g. the HTTP bind failing) to stderr.
struct NodeLogs {
    stdout: PathBuf,
    stderr: PathBuf,
}

impl NodeLogs {
    fn read(&self) -> String {
        let mut text = fs::read_to_string(&self.stderr).unwrap_or_default();
        text.push('\n');
        text.push_str(&fs::read_to_string(&self.stdout).unwrap_or_default());
        text
    }
}

/// Error texts that mean "a port this node was given is held by someone else".
///
/// Linux reports `EADDRINUSE` as os error 98, macOS as 48, Windows as 10048
/// (`WSAEADDRINUSE`). Windows also refuses ports inside a Hyper-V/WinNAT
/// excluded range with 10013 (`WSAEACCES`), which a fresh port fixes too.
const BIND_FAILURE_MARKERS: &[&str] = &[
    "Address already in use",
    "address already in use",
    "(os error 98)",
    "(os error 48)",
    "(os error 10048)",
    "(os error 10013)",
];

/// The first log line reporting a lost port, if any.
fn bind_failure_line(logs: &str) -> Option<String> {
    logs.lines()
        .find(|line| BIND_FAILURE_MARKERS.iter().any(|m| line.contains(m)))
        .map(|line| line.trim().to_string())
}

/// Whether some log line contains `marker` and mentions `127.0.0.1:<port>`
/// (the port not merely a prefix of a longer number). Tolerates the ANSI
/// colour codes ePHPm's log lines carry.
fn log_reports_listener(logs: &str, marker: &str, port: u16) -> bool {
    let needle = format!(":{port}");
    logs.lines().filter(|line| line.contains(marker)).any(|line| {
        line.match_indices(&needle)
            .any(|(at, _)| !line[at + needle.len()..].starts_with(|c: char| c.is_ascii_digit()))
    })
}

/// Wait until the node serves `/_ephpm/health` **on its own listener**.
///
/// A `200` alone proves nothing: if the node lost its HTTP port, whatever owns
/// that port answers instead. That is exactly how one leaked ePHPm used to
/// serve every later fixture's health check (and then its suites). So readiness
/// also requires the node to have logged binding the port (`HTTP listening`).
/// A node that exits returns at once — as [`StartFailure::PortTaken`] when its
/// logs say a port was taken, else as fatal.
fn wait_until_serving(
    child: &mut Child,
    logs: &NodeLogs,
    port: u16,
    timeout: Duration,
) -> Result<(), StartFailure> {
    let deadline = Instant::now() + timeout;
    loop {
        exit_check(child, logs)?;
        let last = match tcp_get(port, "/_ephpm/health") {
            Ok(200) if log_reports_listener(&logs.read(), "HTTP listening", port) => return Ok(()),
            Ok(200) => format!(
                "/_ephpm/health answered 200, but this node has not logged binding \
                 127.0.0.1:{port} -- another process may own the port (or a RUST_LOG \
                 setting hides ePHPm's INFO lines, which this check reads)"
            ),
            Ok(status) => format!("/_ephpm/health returned HTTP {status}"),
            Err(e) => e.to_string(),
        };
        if Instant::now() >= deadline {
            return Err(StartFailure::Fatal(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("ephpm on 127.0.0.1:{port} not serving within {timeout:?}: {last}"),
            )));
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// Wait for a listener whose bind failure is *logged* rather than fatal (the
/// node keeps running without it) to report `marker` on `port`.
fn wait_for_listener(
    child: &mut Child,
    logs: &NodeLogs,
    marker: &str,
    port: u16,
    timeout: Duration,
) -> Result<(), StartFailure> {
    let deadline = Instant::now() + timeout;
    loop {
        exit_check(child, logs)?;
        let text = logs.read();
        if let Some(line) = bind_failure_line(&text) {
            return Err(StartFailure::PortTaken(line));
        }
        if log_reports_listener(&text, marker, port) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(StartFailure::Fatal(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no \"{marker}\" log line for 127.0.0.1:{port} within {timeout:?}"),
            )));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// `Err` if the node has exited: `PortTaken` when its logs blame a taken
/// port, fatal otherwise.
fn exit_check(child: &mut Child, logs: &NodeLogs) -> Result<(), StartFailure> {
    let Some(status) = child.try_wait()? else {
        return Ok(());
    };
    Err(match bind_failure_line(&logs.read()) {
        Some(line) => StartFailure::PortTaken(line),
        None => StartFailure::Fatal(io::Error::other(format!(
            "ephpm exited ({status}) before it was serving"
        ))),
    })
}

// ── helpers ────────────────────────────────────────────────────────────────

/// Wait until a worker-mode node actually serves a PHP request (not just the
/// liveness probe). Proves at least one worker finished booting the framework.
fn wait_for_worker(port: u16, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last_err = String::from("no attempts made");
    while Instant::now() < deadline {
        match tcp_get(port, "/_xtask_worker_warmup") {
            Ok(200) => return Ok(()),
            Ok(status) => last_err = format!("worker request returned HTTP {status}"),
            Err(e) => last_err = e.to_string(),
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
///
/// Errors say which step failed. In particular a read that times out is
/// reported as such: on Unix it surfaces as `EAGAIN` ("Resource temporarily
/// unavailable (os error 11)"), which used to be printed as a *connect* error
/// and read like a networking failure rather than a node that accepted the
/// connection and never answered.
fn tcp_get(port: u16, path: &str) -> io::Result<u16> {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let step =
        |what: &'static str| move |e: io::Error| io::Error::new(e.kind(), format!("{what}: {e}"));
    let mut stream = TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        Duration::from_secs(1),
    )
    .map_err(step("connect"))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    let req = format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).map_err(step("write"))?;

    let mut buf = [0u8; 128];
    let n = stream.read(&mut buf).map_err(|e| match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => io::Error::new(
            io::ErrorKind::TimedOut,
            format!("connected, but no response within 2s ({e})"),
        ),
        _ => step("read")(e),
    })?;
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        CLUSTER_NODE_TEMPLATE, ISOLATED_CONFIG_SUITES, PORT_RANGE, PortKind, PortPicker,
        SINGLE_NODE_TEMPLATE, bind_failure_line, log_reports_listener, parse_test_executables,
        platform_skip_reason_for,
    };

    /// A reservation hands out distinct, in-range ports that are genuinely
    /// held: nothing else can bind them until the reservation is released.
    #[test]
    fn reservation_holds_distinct_in_range_ports_until_released() {
        let mut picker = PortPicker::new();
        let kinds = [PortKind::Tcp, PortKind::Tcp, PortKind::Udp, PortKind::Tcp, PortKind::Udp];
        let res = picker.reserve(&kinds).expect("reserve");
        let ports = res.ports().to_vec();
        assert_eq!(ports.len(), kinds.len());
        let mut sorted = ports.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ports.len(), "ports must be distinct: {ports:?}");
        for (port, kind) in ports.iter().zip(kinds) {
            assert!(PORT_RANGE.contains(port), "{port} outside {PORT_RANGE:?}");
            let taken = match kind {
                PortKind::Tcp => std::net::TcpListener::bind(("127.0.0.1", *port)).is_err(),
                PortKind::Udp => std::net::UdpSocket::bind(("127.0.0.1", *port)).is_err(),
            };
            assert!(taken, "{kind:?} port {port} must be held while reserved");
        }
        res.release();
        let tcp_port = ports[0];
        assert!(
            std::net::TcpListener::bind(("127.0.0.1", tcp_port)).is_ok(),
            "port {tcp_port} must be bindable once released"
        );
    }

    /// Two pickers (two harness runs) do not start from the same candidate
    /// sequence, so they do not keep reaching for the same ports.
    #[test]
    fn pickers_are_independently_seeded() {
        let a: Vec<u16> = (0..8).map(|_| PortPicker::new().candidate()).collect();
        let mut p = PortPicker::new();
        let b: Vec<u16> = (0..8).map(|_| p.candidate()).collect();
        assert_ne!(a, b);
    }

    /// No template may name a fixed port: every listener goes through a
    /// placeholder the harness fills from a reservation.
    #[test]
    fn templates_hard_code_no_ports() {
        for (name, template) in
            [("single", SINGLE_NODE_TEMPLATE), ("cluster", CLUSTER_NODE_TEMPLATE)]
        {
            for line in template.lines().filter(|l| l.contains("127.0.0.1:")) {
                assert!(line.contains("127.0.0.1:{"), "{name} template binds a fixed port: {line}");
            }
        }
        assert!(SINGLE_NODE_TEMPLATE.contains("listen = \"127.0.0.1:{KV_PORT}\""));
        assert!(CLUSTER_NODE_TEMPLATE.contains("listen = \"127.0.0.1:{CHANNEL_PORT}\""));
    }

    #[test]
    fn bind_failures_are_recognised_on_every_platform() {
        for line in [
            "error: failed to bind to 127.0.0.1:18100: Address already in use (os error 98)",
            "\u{1b}[31mERROR\u{1b}[0m KV RESP server error: failed to bind KV server to \
             127.0.0.1:6379: Address already in use (os error 98)",
            "failed to bind: Only one usage of each socket address is normally permitted. \
             (os error 10048)",
            "failed to bind: An attempt was made to access a socket in a way forbidden by its \
             access permissions. (os error 10013)",
        ] {
            assert!(bind_failure_line(&format!("ok\n{line}\nok")).is_some(), "{line}");
        }
        assert_eq!(bind_failure_line("INFO ephpm_server: HTTP listening addr=127.0.0.1:1"), None);
    }

    #[test]
    fn listener_lines_must_name_the_exact_port() {
        let logs = "\u{1b}[32m INFO\u{1b}[0m ephpm_server: HTTP listening \
                    \u{1b}[3maddr\u{1b}[0m\u{1b}[2m=\u{1b}[0m127.0.0.1:12345\n\
                    INFO ephpm: starting ePHPm listen=127.0.0.1:11111";
        assert!(log_reports_listener(logs, "HTTP listening", 12345));
        assert!(!log_reports_listener(logs, "HTTP listening", 1234), "prefix of another port");
        assert!(!log_reports_listener(logs, "HTTP listening", 11111), "not a listening line");
    }

    /// Cargo's JSON stream: only `tests/*.rs` targets count, escaped paths
    /// are unescaped, and a `null` executable is skipped.
    #[test]
    fn test_executables_come_from_cargo_json() {
        let messages = concat!(
            r#"{"reason":"compiler-artifact","package_id":"x","target":{"kind":["lib"],"crate_types":["lib"],"name":"ephpm_e2e","src_path":"s","test":true},"executable":"/t/deps/ephpm_e2e-1","fresh":true}"#,
            "\n",
            r#"{"reason":"compiler-artifact","package_id":"x","target":{"kind":["test"],"crate_types":["bin"],"name":"basic","src_path":"s","test":true},"executable":"C:\\t\\deps\\basic-2.exe","fresh":true}"#,
            "\n",
            r#"{"reason":"compiler-artifact","package_id":"x","target":{"kind":["test"],"crate_types":["bin"],"name":"kv","src_path":"s","test":true},"executable":null,"fresh":true}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
        );
        assert_eq!(
            parse_test_executables(messages),
            vec![("basic".to_string(), PathBuf::from(r"C:\t\deps\basic-2.exe"))]
        );
    }

    /// Issue #454: on Windows the `crash_containment` suite is dropped with a
    /// reason that names the missing capability, never silently.
    #[test]
    fn crash_containment_is_skipped_on_windows_with_a_named_reason() {
        let reason = platform_skip_reason_for("crash_containment", true)
            .expect("crash_containment must be skipped on Windows");
        assert!(reason.contains("crash containment"), "reason must name the capability: {reason}");
        assert!(reason.contains("Windows"), "reason must name the platform: {reason}");
    }

    /// The skip is Windows-only: on Unix the suite runs, and no other suite is
    /// skipped on either platform.
    #[test]
    fn nothing_else_is_platform_skipped() {
        assert_eq!(platform_skip_reason_for("crash_containment", false), None);
        for suite in ISOLATED_CONFIG_SUITES.iter().filter(|s| **s != "crash_containment") {
            assert_eq!(platform_skip_reason_for(suite, true), None, "{suite} (windows)");
            assert_eq!(platform_skip_reason_for(suite, false), None, "{suite} (unix)");
        }
    }
}
