# ePHPm — Embedded PHP Manager

An all-in-one PHP application server written in Rust that embeds PHP via FFI into a single binary. Runs WordPress, Laravel, etc. without external PHP-FPM. Includes an embedded SQLite-compatible database (the Turso engine via litewire), gossip clustering, and a built-in KV store.

## Build & Run

```bash
# Stub mode (no PHP, fast iteration on HTTP/routing logic)
cargo build

# Preflight: check build prerequisites (exits non-zero if a required tool is missing)
cargo xtask doctor            # add --target windows to check the Windows-build toolchain

# Release binary with PHP embedded (Turso engine is built in — no external binary)
cargo xtask release           # → target/release/ephpm (PHP 8.5)
cargo xtask release 8.4       # → target/release/ephpm (PHP 8.4)

# Windows .exe — must be built ON Windows (native MSVC toolchain).
# The cargo-xwin cross-compile-from-WSL path was REMOVED; `cargo xtask
# release --target windows` errors out on a non-Windows host.
cargo xtask release --target windows       # → target/x86_64-pc-windows-msvc/release/ephpm.exe

# Experimental TAILCALL Windows build (PHP 8.5 only): links the clang-cl-built
# SDK (php-sdk-<ver>-windows-x86_64-clang.tar.gz) whose interpreter is the
# TAILCALL VM — 1.6-1.7x faster on CPU-bound PHP than the MSVC CALL VM.
# xtask hard-gates on the SDK's VM kind (disassembles zend_vm_kind, requires
# ZEND_VM_KIND_TAILCALL) before linking. Same output path as the MSVC exe.
cargo xtask release --target windows --variant clang
```

Prerequisites for `cargo xtask release`: git, curl, tar, `build-essential`, `pkg-config`, and `libclang-dev` (for bindgen). On Linux the build targets the host-default `<arch>-unknown-linux-gnu` triple against the glibc-linked (`-gnu`) `libphp.a` — the resulting binary is a single glibc-dynamic file that can `dlopen()` shared PHP extensions and middleware; no musl toolchain is involved. The xtask downloads only the PHP SDK from `github.com/ephpm/php-sdk` releases — no PHP CLI, Composer, static-php-cli, or sqld binary needed (the Turso engine is a pure-Rust crate compiled into the binary).

The PHP SDK is cached at `php-sdk/<version>-<os>-<arch>[-gnu][-clang]/` (the `-gnu` libc suffix applies on Linux, e.g. `php-sdk/8.5.7-linux-x86_64-gnu/`; the `-clang` variant suffix applies to the experimental Windows TAILCALL SDK, e.g. `php-sdk/8.5.7-windows-x86_64-clang/`). Delete that directory to force a re-download.

## Testing

```bash
cargo test -p <crate> <test_name>          # run a single test (preferred)
cargo test -p <crate>                      # run all tests in a crate
cargo test --workspace                     # all tests (may fail without openssl for e2e deps)
cargo clippy --workspace --all-targets -- -D warnings  # lint (pedantic, warnings = errors)
cargo +nightly fmt --all -- --check        # format check (nightly required for import grouping)
cargo deny check                           # license/advisory audit
```

IMPORTANT: Run single tests when possible, not the full suite. Use `cargo test -p <crate> <test_name>`. `cargo nextest` is preferred but may not be installed — fall back to `cargo test`.

**The dlopen middleware tests need an explicit build first.** `cargo test` and `cargo nextest run` do **not** emit example artifacts (`cargo test --no-run` does; a plain `cargo test` does not — verified, issue #435), so `crates/ephpm-server/tests/middleware_dlopen.rs` and `tests/middleware_response_phase.rs` hard-fail on a missing cdylib unless you run `cargo build --workspace --lib --examples` first — the same step CI runs before its test step. This is platform-independent; it was reported as a Windows bug only because that is where someone ran a bare `cargo test` on a clean checkout.

The `ephpm-e2e` crate is **excluded from the workspace** and has different dependencies — don't try to compile it with `cargo test --workspace`. It runs bare-process by default via `cargo xtask e2e` (spawns ephpm on 127.0.0.1, no Kind), or via `cargo xtask k8s-e2e` for opt-in Kind + Tilt cluster testing (dispatched from `.github/workflows/k8s-e2e.yml`).

## Workspace Structure

| Crate | Purpose |
|-------|---------|
| `ephpm` | CLI binary — clap args, config loading, server startup, graceful shutdown |
| `ephpm-server` | HTTP server (hyper + tokio) — routing, static files, TLS/ACME, metrics, litewire/SQLite startup, query stats |
| `ephpm-php` | PHP embedding via FFI — SAPI implementation, worker thread pool, request/response mapping |
| `ephpm-config` | Configuration (figment) — TOML + env var overrides (`EPHPM_` prefix) |
| `ephpm-kv` | Embedded KV store — DashMap, RESP2 protocol, TTL/expiry, compression (gzip/zstd/brotli) |
| `ephpm-db` | DB proxy — MySQL wire protocol, connection pooling, R/W splitting |
| `ephpm-cluster` | Clustering — SWIM gossip (chitchat), hashed large-value ownership (`hash(key)` mod sorted alive nodes — not a consistent-hash ring), KV replication, SQLite primary election |
| `ephpm-query-stats` | Query observability — SQL normalization, digest tracking, slow query logging, Prometheus metrics |
| `ephpm-ws` | Site-scoped WebSocket connection registry — connection/channel maps, bounded per-connection outbound queues, capability-style connection IDs. Protocol-free (no framing), so both `ephpm-php` and `ephpm-server` depend on it |
| `ephpm-middleware` | Native middleware C ABI + safe Rust authoring kit. Two phases: **request** (`Middleware::invoke`, before serving — reject/rewrite/annotate, runs on both the PHP and static-file paths, **fails closed**) and optional **response** (`ResponseMiddleware::invoke_response`, reverse chain order — compression/ETag/headers, **fails safe, not a security gate**) |
| `ephpm-middleware-builtins` | The ten in-tree official middleware modules as plain Rust — **no C ABI exports**, so `ephpm-server` links them all in and serves them through the static builtin registry (`library = "jwt"` works even in a fully static build with no `dlopen`) |
| `ephpm-middleware-{cors,jwt,ratelimit,security-headers}` | Loadable **cdylib shells** over the matching `ephpm-middleware-builtins` module — they add only the `declare!` C ABI exports so the same module can be `dlopen`ed by dynamically linked builds. The implementation, docs, and tests live in builtins, not here. `security-headers` is also the guinea pig for `ephpm-server`'s dlopen round-trip test |
| `xtask` | Build & test tooling — `release`, `php-sdk`, `e2e` (bare-process default), `k8s-e2e`/`k8s-e2e-up`/`k8s-e2e-down` (opt-in Kind path) |

Workspace membership is `crates/*` + `examples/rust-middleware` + `xtask`, with `default-members = crates/*` and `crates/ephpm-e2e` **excluded**. `examples/rust-middleware` is a member on purpose — it's the worked example for the native-middleware C ABI, and keeping it in-tree means `cargo clippy --workspace` / `cargo +nightly fmt --all` catch ABI drift; it's out of `default-members` so a plain `cargo build` doesn't pay for it.

## External Dependencies

| Dependency | Location | Purpose |
|-----------|----------|---------|
| **litewire** | Git dep on `github.com/ephpm/litewire`, pinned by `rev` in the workspace `Cargo.toml` | MySQL/Hrana/PG/TDS wire protocol → SQLite (Turso) translation proxy |
| **PHP SDK** | Downloaded by `cargo xtask php-sdk` from `github.com/ephpm/php-sdk` releases | Prebuilt `libphp.a` (Linux/macOS) or `php8embed.{dll,lib}` (Windows) plus PHP headers. Pinned per minor in `xtask/src/main.rs::PHP_SDK_VERSIONS` |
| **turso** | Pure-Rust crate (`turso =0.7.2`), compiled into the binary via litewire | The embedded SQLite-compatible database engine — single-node and CDC-replicated clustered. No external process. |

litewire is a standalone project at `github.com/ephpm/litewire`. It's used as a library — ePHPm calls `LiteWire::new(backend).mysql(addr).serve()`. Bumping it means updating the `rev` in the workspace `Cargo.toml` and running `cargo update -p litewire`. To work against a sibling checkout without changing the pin, add a `[patch."https://github.com/ephpm/litewire.git"]` entry pointing at `../litewire/crates/litewire` in your local config — see the comment above the dependency in `Cargo.toml`.

The PHP SDK is built by a separate pipeline at `github.com/ephpm/php-sdk` (uses static-php-cli internally). ePHPm itself doesn't depend on static-php-cli at all — it just consumes the resulting tarballs.

## Architecture: Database

**As of v0.7.0 the embedded SQLite-family engine is Turso only** — the rusqlite (genuine SQLite C engine) backend was de-linked from ePHPm and the sqld sidecar was removed. Two database modes, both transparent to PHP (`pdo_mysql` connects to `127.0.0.1:3306`):

1. **DB Proxy** (`[db.mysql]` / `[db.postgres]`) — forwards MySQL/PostgreSQL wire traffic to a real database server with connection pooling
2. **Embedded Turso** (`[db.sqlite]`) — litewire + the in-process Turso engine. Single-node by default; **clustered** when `[cluster]` is enabled (replication via the in-process Turso CDC path — see below).

Mode detection (`is_clustered_sqlite()` in `crates/ephpm-server/src/lib.rs`):
- If `replication.role = "primary"` or `"replica"` → clustered
- If `replication.role = "auto"` AND `cluster.enabled = true` → clustered (election via gossip)
- Otherwise → single-node (in-process Turso)

**Per-site databases (multi-tenant, `is_per_site_sqlite()`):** when `[server] sites_dir` is set with `[db.sqlite]` (single-node), each virtual host gets its **own** database file at `[db.sqlite].dir`/`<site-key>.db` — the tenant-isolation boundary (Turso has no per-schema ACL). `dir` is required (fail-closed) and `max_open_dbs` bounds an LRU of open databases (`crates/ephpm-server/src/site_backends.rs`, `SiteBackends`). The `ephpm_db_*` bridge resolves each request's own database via a per-thread session that swaps when the request's site changes (`crates/ephpm-php/src/db_bridge.rs`).

**One canonical site key.** A request's tenant identity is derived exactly once, by `Router::resolve_site` (`crates/ephpm-server/src/router.rs`), which returns a `ResolvedSite { key, document_root, .. }`. Everything per-tenant — the database filename, the per-vhost temp/session state root, the `pdo_mysql` credential, the KV keyspace, the OPcache vhost — is derived from that `key` (via `Router::site_identities`), never re-derived from the `Host` header. The key is the vhost-directory name: `Host` normalized (port + trailing dot stripped, lowercased) with `[server] sites_domain_suffix` removed. `key` is `None` for a host that matches no site, and such a request gets **no** per-site database and no `DB_*` credentials (`ephpm_db_*` fails with "no per-site database context"). Issues #290/#291; the invariant is pinned by `router::tests::site_key_agreement`.

Stock `pdo_mysql` also works per-site (`crates/ephpm-server/src/site_wire_auth.rs`). **One** MySQL listener serves every tenant; a connection's database is fixed by the credential it authenticates with, not by anything it claims. `DB_USER` is the site key, `DB_PASSWORD` is `HMAC-SHA256(per-process master secret, site_key)` — the same derivation the KV RESP listener uses (`ephpm_kv::auth::derive_site_password`) — injected into that site's `$_SERVER` per request by the router. Verification happens *before* the backend is resolved, so a failed auth never opens the target's database file. Per-site listeners were rejected deliberately: all tenants share one process and one uid, so neither a port (enumerable) nor a unix socket (identical permissions; `open_basedir` does not gate `unix://` or PDO's `unix_socket`) can separate them — see the module docs. Hrana/PG/TDS stay off in multi-site mode (they cannot bind a backend per connection). `ATTACH`/`DETACH`/`VACUUM`/path-`PRAGMA` are rejected on the tenant path in all modes.

**Per-site clustered replication (`is_per_site_clustered()`, EXPERIMENTAL).** Opt in with `[db.sqlite.replication] per_site = true` and each virtual host gets its own database that *replicates across the cluster*, instead of every tenant sharing the one clustered database (the `per_site = false` default, which warns at startup). Ownership of a site is by rendezvous hashing (HRW) over the alive nodes (`ephpm-cluster/src/sqlite_election.rs`, `hrw_owner`), so a node death moves only that node's sites, each to a node already holding a warm replica. Replication is per-site CDC over the cluster channel (`cdc/<site>` / `snapshot/<site>` streams in `turso_cdc.rs`). Writes are **owner-served**: a non-owner forwards each statement to the site's owner over `sql/<site>` (`ephpm-server/src/sql_forward.rs`), so the write is captured into CDC and replicates everywhere — reads and writes both work on any node. **Both** tenant routes forward: the `ephpm_db_*` bridge via `SiteBackendResolver` (sync, `block_on` on a PHP worker) and stock `pdo_mysql` via `SiteWireRoute` (async, on a tokio worker — litewire's authenticator is async, where `block_on` would panic), both backed by the one `ClusteredSiteResolver` that `wire_per_site_clustered_db` builds. The wire route used to resolve locally unconditionally, which meant a `pdo_mysql` write landing on a non-owner committed to a replica whose writes are never captured into CDC — unreplicated, invisible to the owner, and discarded at that replica's next re-bootstrap. It diverged *silently*, with every health check green. Two follow-on effects, both intended: a non-owner no longer opens a site's database just to serve wire traffic (it announces instead — see below), and a forwarded statement's query stats and screening run on the **owner**, though litewire's own tenant-session screen still runs locally so the `ATTACH`/`VACUUM`/path-`PRAGMA` rejections a tenant sees are unchanged. Routing is **resolve-at-connect**: the route is consulted once per connection and pinned for its life, so an ownership move mid-connection makes the (HRW-gating) old owner refuse and the tenant's statements fail loudly until it reconnects — the same lifetime the bridge already has (one resolve per (thread, site) session, recovered by recycling on a connection-shaped error). Both paths trade "an ownership move breaks open sessions, loudly" for never writing where writes do not replicate.

**Serving a site and replicating it are the same event.** A node enters a site's replication set when the site is *announced* to the per-site driver (`ensure_site_driver`), and there are exactly **two** announcers, which must stay in sync: `SiteBackends::get_or_open`'s `on_open` hook (a local database open — i.e. whenever this node is the site's owner, on either route) and `ClusteredSiteResolver`'s forwarding branch (`plan_serve`, on a non-owner — now both routes, since `pdo_mysql` resolves through the resolver too). Both are handed the *same* hook by `wire_per_site_clustered_db`. Wiring only the first is a silent data-durability bug: a forwarding-only node forwards a site's traffic correctly forever while never replicating it, so the tenant's data sits on one node and HRW failover promotes a node holding nothing — which is exactly what the "each site moves to a node already holding a warm replica" premise above depends on not happening. Note the replication set is **not** bounded by `max_open_dbs`: a driver holds its own `SiteMgmtRegistry` mgmt factory, independent of the LRU-bounded serving handle, so evicting a serving handle never stops replication (and announcing a forwarded site deliberately does *not* open it locally — that would burn an LRU slot for a database the node is not serving from). Mode selection is strictly ordered — `is_per_site_clustered` is a subset of `is_clustered_sqlite` and must be tested first. Experimental: Turso is Beta upstream.

**Ownership moves must carry the data.** HRW ownership is recomputed from live membership, so it moves when a node merely *joins*. Three rules make that safe, and none of them may be weakened without a replacement: (1) a per-site database records its **data lineage** in `__ephpm_cdc_source` — the log its contents descend from, which for the node that *created* the site is its own log id (`site_data_lineage`); a node never treats data it originated as divergence and never discards it to bootstrap from a peer. (2) A re-bootstrap is ordered **fetch → vet → discard → apply**, and a snapshot declaring no tables is refused over live data (`snapshot_may_replace_local_data`) — `validate_snapshot_dump("")` succeeds, so emptiness has to be checked separately. (3) A new owner whose copy is cold pulls a snapshot from the previous owner — HRW over the alive set minus self (`take_ownership_handoff`) — before serving, which is why `snapshot/<site>` is served to the site's current owner by any node, while `cdc/<site>` stays owner-only. Ownership resolution follows live HRW, not the published election claim (`sqlite_election.rs`): a claim that outlives its membership is released rather than refreshed. Multi-node ownership churn is unit-tested, **not** live-cluster validated. Also: `/_ephpm/primary` returns 200 on every healthy node in this mode — there is no cluster-wide primary, and every node accepts writes for every site via forwarding.

Note that `replication.role` defaults to `"auto"`. So omitting `[db.sqlite.replication]` entirely is identical to setting `role = "auto"` — clustered mode if `[cluster].enabled = true`, single-node otherwise. To force single-node even with clustering on, set `replication.role` to anything other than `"primary"`, `"replica"`, or `"auto"` (e.g. `"single"`).

The `[db.sqlite].engine` knob defaults to `"turso"` and `"turso"` is the only accepted value. Legacy `engine = "sqlite"` / `"rusqlite"` is a **hard startup error** (with a migration message) rather than a silent fallback.

**Every config section struct is `#[serde(deny_unknown_fields)]`** — an unknown key under any section fails startup naming the key. Two deliberate exceptions, both pinned by tests: the **top-level `Config`** stays lenient because `Env::prefixed("EPHPM_")` is unfiltered and turns *every* `EPHPM_*` variable into a top-level key (ePHPm itself sets `EPHPM_SERVICE_LOG_FILE` in the Windows service wrapper, and the e2e harness sets `EPHPM_URL`/`EPHPM_BINARY` — a strict root would refuse to start as a Windows service); and `DeprecatedSqldConfig` stays lenient because tolerating keys this binary no longer declares is its whole purpose. Nested sections have no env exposure — nothing sets an `EPHPM_*` variable containing the `__` nesting separator. Adding a section means adding a case to `unknown_keys_are_rejected_in_every_strict_section`, and `every_config_shipped_in_this_repo_loads` guards the example/smoke configs. The reasoning generalizes #429's `[db.sqlite]` fix: These knobs select a *mode*, and serde's default (drop unknown fields silently) turned an operator's explicit instruction into a no-op — `per_site = true` on a binary predating the knob parsed fine and came up in whole-DB clustered mode with every health check green. Forward-compat breakage is the intended trade. Removed-but-honoured knobs (`sqld`, `cdc_experimental`) stay *declared* so upgrading configs still parse and warn. The mode actually chosen is logged once at INFO as `embedded SQLite mode selected` with `mode` = `per-site-clustered` | `clustered` | `per-site` | `single-node` (`sqlite_mode_label`) — the line operators and bench gates assert on.

**File-format compatibility:** the Turso engine opens existing rusqlite/sqlite3-created `.db` files in place — a cleanly-shut-down 0.6.x database (WAL or rollback journal) upgrades to Turso with no dump/reload (verified: `PRAGMA integrity_check` = ok, rows intact). Caveat: a database left with an uncheckpointed hot `-wal` from a hard crash should be cleanly shut down before upgrading; and non-UTF-8 TEXT cells may not round-trip (Turso surfaces TEXT as `String`).

**Clustered replication (Turso CDC):** clustered mode replicates through the in-process Turso CDC path (`turso_cdc.rs`, `start_clustered_turso_cdc`) over the cluster channel — no sqld child process, no gRPC WAL-frame transport. The primary's `turso_cdc` stream is tailed and per-transaction batches are shipped to replicas; cold replicas bootstrap from a logical snapshot. Primary election still uses the gossip KV tier (`kv:sqlite:primary`, `ephpm-cluster/src/sqlite_election.rs`). The Turso engine is Beta upstream, so **clustered mode is experimental**.

The `TrackedBackend` wrapper in `ephpm-server/src/tracked_backend.rs` wraps any litewire backend to record query stats. Disable with `[db.analysis] query_stats = false`.

## Critical Conventions

- **Conditional compilation**: All PHP FFI code is gated with `#[cfg(php_linked)]`. The `php_linked` cfg is set by `ephpm-php/build.rs` when `PHP_SDK_PATH` env var is present. Stub mode must always compile and pass tests without it.
- **C wrapper required**: PHP uses setjmp/longjmp for error handling. Never call PHP functions directly from Rust without going through `ephpm_wrapper.c` and its `zend_try/zend_catch` guards — otherwise SIGSEGV.
- **PHP threading**: ZTS (Zend Thread Safety) is implemented. PHP is compiled with `--enable-zts` and each `spawn_blocking` thread auto-registers with TSRM on first use, getting its own isolated PHP context. No dedicated worker pool — tokio's `spawn_blocking` pool is the thread pool. A `Mutex` protects only one-time `init()`/`shutdown()`, not request execution. An `AtomicBool` fast-path check avoids the mutex for the common "is PHP ready?" path. Per-request C statics use `__thread` for thread isolation. **Windows is ZTS too** — the php-sdk's static `php8embed.lib` is a ZTS build (TSRM exports, `_tsrm_ls_cache`; `ephpm php -v` reports ZTS) and the wrapper/bindgen compile with `ZTS=1` (issue #326; the old "Windows is NTS" claim was wrong). What Windows genuinely lacks: per-thread execution timers (`ZEND_MAX_EXECUTION_TIMERS` — so `max_execution_time` is not natively enforced; the `[server.timeouts] request` ceiling still applies) and the Unix-only stack-overflow crash containment (`crash_guard.c`).
- **MSRV**: Rust 1.88 (forced by the litewire dependency's `rust-version = "1.88"`) — do not use features from newer toolchains without checking.
- **Clippy**: Pedantic + all warnings denied (`-D warnings`). Zero warnings policy.
- **Rustfmt**: 2024 edition style, `group_imports = "StdExternalCrate"`. Requires **nightly** toolchain (`cargo +nightly fmt`).
- **Error handling**: `thiserror` for domain errors, `anyhow` for propagation with context. Always add context to errors with `.context()`.
- **Logging**: `tracing` crate. Use appropriate levels — debug for request details, info for lifecycle events, warn/error for problems.
- **Windows database**: Single-node Turso works on Windows. Clustered mode (Turso CDC) has no sqld dependency any more, but has not been validated on Windows — treat clustered mode as Linux/macOS + experimental.

## Code Style

- Crate names: `ephpm-*` (kebab-case)
- Safety comments (`// SAFETY:`) before every `unsafe` block explaining FFI invariants
- Public API documentation with `///` on all exported items
- Module-level docs with `//!` explaining purpose and design

## Key Files

| File | What it does |
|------|-------------|
| `ephpm-server/src/lib.rs` | `serve()` entry point, cluster startup, `start_db_proxies()` with single-node and clustered Turso branches |
| `ephpm-server/src/turso_cdc.rs` | `start_clustered_turso_cdc()` — the in-process Turso CDC clustered replication path; `start_clustered_per_site_turso()` — the per-site (`cdc/<site>` / `snapshot/<site>`) variant |
| `ephpm-server/src/sql_forward.rs` | Owner-serves SQL write-forwarding (`sql/<site>`) for per-site clustered mode — `ClusteredSiteResolver` (client) + `spawn_owner_sql_handler` (owner) |
| `ephpm-cluster/src/site_namespace.rs` | `\x1f<site>\x1f<key>` transport envelope that gives the flat KV wire a per-vhost dimension; `decode` fails closed to the global keyspace |
| `ephpm-server/src/tracked_backend.rs` | `TrackedBackend<B>` — wraps litewire `Backend` with query stats |
| `ephpm-server/src/router.rs` | HTTP request routing, PHP dispatch, static file serving |
| `ephpm-server/src/websocket.rs` | Native WebSocket upgrade + per-connection session task (`[server.websocket]`, experimental) |
| `ephpm-server/src/middleware.rs` | Server-side middleware chain — resolves `library = "<name>"` against the static builtin registry first, else `dlopen`s a cdylib |
| `ephpm-middleware/src/{abi,builtin,host}.rs` | `abi.rs` the C ABI + `declare!`; `builtin.rs` the static registry (no-`dlopen` builds); `host.rs` the host-side loader/invoker |
| `ephpm-middleware-builtins/src/*.rs` | The ten official modules: `api_key`, `cors`, `header_transform`, `ip_allowlist`, `jwt`, `maintenance_mode`, `ratelimit`, `redirect`, `request_id`, `security_headers` |
| `ephpm-php/src/ws_bridge.rs` | `ephpm_ws_*` native functions — per-thread site scope + current-connection context |
| `ephpm-config/src/lib.rs` | All config structs: `SqliteConfig`, `ReplicationConfig`, `ClusterConfig`, `DbAnalysisConfig` |
| `ephpm-cluster/src/sqlite_election.rs` | Primary election via gossip KV (lowest ordinal wins, TTL heartbeat); `hrw_owner()` + per-site election (`sqlite:primary:<site>`) for per-site clustered mode |
| `ephpm-query-stats/src/digest.rs` | SQL normalizer (state machine replacing literals with `?`) |
| `ephpm-query-stats/src/lib.rs` | `QueryStats` — DashMap-based digest tracking, Prometheus metrics |
| `xtask/src/main.rs` | Build tooling — PHP SDK download, release/e2e commands |

## Git & Remotes

- **`origin`** = `github.com/ephpm/ephpm.git` (org repo, source of truth)
- Local `main` tracks `origin/main`
- The old `luthermonson/ephpm.git` remote was removed

## ePHPm Organization Repositories

Quick-reference map of the **public** repos in the `github.com/ephpm` org (`origin` = this repo, the source of truth). Verify against a repo's own README before relying on details — the "Docs Must Match Code" rule applies here too. The **DB-bridge** and **KV/cache** and **worker-mode** rows are PHP Composer packages that apps *install*, distributed via their GitHub repos (Composer `vcs` repositories), **not Packagist**; they are consumers of ePHPm's SAPI functions, not part of this Rust workspace. See **External Dependencies** above for the fuller story on `litewire` and `php-sdk`.

| Repo | What it is | Relationship to ePHPm |
|------|-----------|-----------------------|
| `ephpm` | This repo — the Rust application server. | Source of truth (`origin`). |
| `litewire` | Rust wire-protocol proxy (MySQL/PG/TDS/Hrana → SQLite/Turso). | **Dependency** — git dep pinned by `rev` in workspace `Cargo.toml`; used as a library. |
| `php-sdk` | Prebuilt PHP embed static libs (`libphp.a` / `php8embed.lib`) + headers, per OS/arch/PHP-version; built with static-php-cli. | **Dependency** — tarballs downloaded by `cargo xtask php-sdk`, pinned per-minor in `xtask/src/main.rs`. Separate build pipeline. |
| `ephemerd` | Ephemeral GitHub Actions runner daemon (Go). Containers on Linux, Hyper-V on Windows, VMs on macOS. | **CI infra** — the self-hosted runner fleet that runs ePHPm's Actions (incl. Windows/macOS legs). Not linked into the binary. |
| `db` | Base PHP library for the in-process DB bridge — typed `Connection`, exceptions, IDE stubs over `ephpm_db_*`. | **Consumer (PHP pkg)** — base dep of the DB-driver packages below. Requires ePHPm **≥ v0.6.3**, the first tag carrying the `ephpm_db_*` bridge (`ephpm_kv_*` has shipped since v0.1.0). |
| `db-wordpress` | WordPress `wp-content/db.php` drop-in (wpdb) over `ephpm_db_*`. | **Consumer (PHP pkg)** — WordPress on per-site Turso, no mysqli/socket. Falls back to stock mysqli off-ePHPm. |
| `db-laravel` | Laravel database driver (`'driver' => 'ephpm'`) over `ephpm_db_*`. | **Consumer (PHP pkg)** — Laravel query builder/Eloquent, no PDO/socket. |
| `db-doctrine` | Doctrine DBAL 4 driver over `ephpm_db_*`. | **Consumer (PHP pkg)** — DBAL 4 only (not DBAL 3). |
| `mysqli-shim` | Userland `mysqli` compatibility shim over `ephpm_db_*`. | **Consumer (PHP pkg)** — global `mysqli_*` surface active only when ext-mysqli is absent (stock SDK builds compile mysqli in, so it's inert there); namespaced API always available. |
| `cache` | PSR-16 + PSR-6 cache over the embedded KV (`ephpm_kv_*`). | **Consumer (PHP pkg)** — in-process KV, gossip-replicated when clustered. |
| `cache-symfony` | Symfony Cache adapter (PSR-6/Contracts) over `ephpm_kv_*`. | **Consumer (PHP pkg)**. |
| `cache-laravel` | Laravel cache store over `ephpm_kv_*`. | **Consumer (PHP pkg)**. |
| `cache-wordpress` | WordPress `object-cache.php` drop-in over `ephpm_kv_*` (real `wp_cache_flush()`). | **Consumer (PHP pkg)**. |
| `predis-connection` | Predis `Connection` backend routing to `ephpm_kv_*`. | **Consumer (PHP pkg)** — drop-in for `Predis\Client`. |
| `session-handler` | PHP session save handler storing `$_SESSION` in the KV via `ephpm_kv_*`. | **Consumer (PHP pkg)** — framework-agnostic (`session_start()`). |
| `php-worker` | Base SDK for persistent worker mode — worker primitives + IDE stubs + runtime guard. | **Consumer (PHP pkg)** — base dep of the worker adapters below; most users install an adapter, not this. |
| `psr15-worker` | Runs any PSR-15 app (Mezzio/Slim 4/…) under worker mode. | **Consumer (PHP pkg)** — depends on `php-worker`. |
| `octane-driver` | **Laravel Octane server driver** — runs Laravel under ePHPm worker mode via Octane's `Client` contract. | **Consumer (PHP pkg / framework integration)** — the official Octane driver. **Check here before assuming Laravel integration is missing.** |
| `wordpress-worker` | WordPress adapter for persistent worker mode (boot once, reset per request). | **Consumer (PHP pkg)** — experimental; classic themes only (block themes documented limitation). |
| `wordpress-sample` | Deployable real WordPress for PR previews — per-site Turso (`db-wordpress`) + KV object cache (`cache-wordpress`) + native WebSockets. | **Example app** — the PR-preview showcase. |
| `wordpress-datastar` | Live WordPress demo: realtime comments + admin dashboard via Datastar SSE, KV as the event bus. | **Example app / demo.** |
| `datastar-demo` | Pixelboard — multiplayer realtime Datastar demo on worker mode + native KV. | **Example app / demo.** |
| `lab` | Benchmark lab — local runtimes + DB-path tiers (podman) and k6 app workloads (Kubernetes); hosts the multitenant `scale/` tier and the Turso per-vhost cluster suite (recorded on v0.8.7). | **Benchmark/standalone** — org-maintained (originally authored by Benjamin Pace; no external review needed). Historical suites stay pinned to old images by design. |
| `multitenant-scalebench` | Multi-tenant scaling benchmark (1 instance × N WordPress sites). | **Benchmark/standalone (historical)** — harness moved into `lab/scale/`; this repo is now the historical results record. |
| `turso-cluster-e2e` | Two-node clustered-SQLite ePHPm fixture/demo (WordPress + wp-cli). | **Example/e2e fixture (historical)** — README predates v0.7.0; describes the now-removed sqld path and `cdc_experimental` knob alongside Turso CDC. |

## CI Pipeline

Runs on push/PR to main: fmt check → clippy → test → cargo-deny. PRs touching `crates/ephpm-php/**`, `crates/ephpm/build.rs`, `xtask/**`, or `windows-php-check.yml` additionally get a Windows PHP-linked `cargo check -p ephpm-php` (`.github/workflows/windows-php-check.yml`, PHP 8.3 SDK) — the LLP64 compile gate that stub-mode CI can't provide (#318/#320/#319). Release builds triggered by `v*` tags across PHP 8.3/8.4/8.5 × linux-x64/linux-arm64/macos/windows, plus the Docker image; `Create Release` publishes only after all build legs succeed.

## Truthfulness: Docs Must Match Code

A full audit (PRs #106/#107) once found ~25 documented commands that didn't exist, config knobs that were silently ignored, and docs claiming security properties the code doesn't have. These rules prevent recurrence:

- **Never document something as working without verifying it in source.** Before writing user-facing docs, check the actual definition: CLI flags/subcommands in `crates/ephpm/src/main.rs` (clap), config keys and defaults in `crates/ephpm-config/src/lib.rs`, PHP SAPI functions and their arities/units in `crates/ephpm-php/ephpm_wrapper.c`, RESP commands in `crates/ephpm-kv/src/command.rs`, metric names/labels at their `counter!`/`histogram!` call sites.
- **Future features are labeled, not implied.** Anything not implemented must say "Planned — not yet implemented." Design/aspirational docs belong in `site/content/roadmap/`, never in `reference/` or `guides/`.
- **Never claim security or durability properties that aren't implemented** (auth, encryption, replication, isolation, credential validation). If in doubt, grep for the mechanism — a config field existing does not mean the feature exists.
- **No silent no-op config knobs.** A new config field must be read and enforced by code in the same PR. If it genuinely can't be yet: the doc comment must say "Planned: not yet implemented — parsed but not acted upon" AND startup must `tracing::warn!` when the knob is set.
- **No phantom metrics.** Don't register buckets for or document a metric unless something records it.
- **Behavior changes update docs in the same PR.** When changing a default, mechanism, label, or lifecycle, grep `site/content/`, `docs/`, `examples/`, and `README.md` for the old claim and fix every hit.

## Session Hygiene (branches, worktrees, scratch)

- **Merge PRs with `--delete-branch`**, then delete the local branch too. After a worktree's PR merges: `git worktree remove <path>` and delete its `worktree-agent-*` branch. Never leave merged branches or worktrees behind.
- **Review checkouts (`pr-NN-review`) are disposable** — delete them when the review ends; they're re-fetchable with `gh pr checkout NN`.
- **Scratch output goes in gitignored paths** (`/tmp_*` at repo root is ignored) and gets deleted when the investigation ends. Never `git add -A`/`git add .` — stage files by name.
- **Windows shells:** the null device is `$null` (PowerShell), not `NUL` — a stray redirect creates a literal `NUL` file at repo root.
