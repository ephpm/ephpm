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
```

Prerequisites for `cargo xtask release`: git, curl, tar, `build-essential`, `pkg-config`, and `libclang-dev` (for bindgen). On Linux the build targets the host-default `<arch>-unknown-linux-gnu` triple against the glibc-linked (`-gnu`) `libphp.a` — the resulting binary is a single glibc-dynamic file that can `dlopen()` shared PHP extensions and middleware; no musl toolchain is involved. The xtask downloads only the PHP SDK from `github.com/ephpm/php-sdk` releases — no PHP CLI, Composer, static-php-cli, or sqld binary needed (the Turso engine is a pure-Rust crate compiled into the binary).

The PHP SDK is cached at `php-sdk/<version>-<os>-<arch>[-gnu]/` (the `-gnu` libc suffix applies on Linux, e.g. `php-sdk/8.5.7-linux-x86_64-gnu/`). Delete that directory to force a re-download.

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
| `xtask` | Build & test tooling — `release`, `php-sdk`, `e2e` (bare-process default), `k8s-e2e`/`k8s-e2e-up`/`k8s-e2e-down` (opt-in Kind path) |

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

Stock `pdo_mysql` also works per-site (`crates/ephpm-server/src/site_wire_auth.rs`). **One** MySQL listener serves every tenant; a connection's database is fixed by the credential it authenticates with, not by anything it claims. `DB_USER` is the site key, `DB_PASSWORD` is `HMAC-SHA256(per-process master secret, site_key)` — the same derivation the KV RESP listener uses (`ephpm_kv::auth::derive_site_password`) — injected into that site's `$_SERVER` per request by the router. Verification happens *before* the backend is resolved, so a failed auth never opens the target's database file. Per-site listeners were rejected deliberately: all tenants share one process and one uid, so neither a port (enumerable) nor a unix socket (identical permissions; `open_basedir` does not gate `unix://` or PDO's `unix_socket`) can separate them — see the module docs. Hrana/PG/TDS stay off in multi-site mode (they cannot bind a backend per connection). `ATTACH`/`DETACH`/`VACUUM`/path-`PRAGMA` are rejected on the tenant path in all modes. Per-site isolation is single-node only.

Note that `replication.role` defaults to `"auto"`. So omitting `[db.sqlite.replication]` entirely is identical to setting `role = "auto"` — clustered mode if `[cluster].enabled = true`, single-node otherwise. To force single-node even with clustering on, set `replication.role` to anything other than `"primary"`, `"replica"`, or `"auto"` (e.g. `"single"`).

The `[db.sqlite].engine` knob defaults to `"turso"` and `"turso"` is the only accepted value. Legacy `engine = "sqlite"` / `"rusqlite"` is a **hard startup error** (with a migration message) rather than a silent fallback.

**File-format compatibility:** the Turso engine opens existing rusqlite/sqlite3-created `.db` files in place — a cleanly-shut-down 0.6.x database (WAL or rollback journal) upgrades to Turso with no dump/reload (verified: `PRAGMA integrity_check` = ok, rows intact). Caveat: a database left with an uncheckpointed hot `-wal` from a hard crash should be cleanly shut down before upgrading; and non-UTF-8 TEXT cells may not round-trip (Turso surfaces TEXT as `String`).

**Clustered replication (Turso CDC):** clustered mode replicates through the in-process Turso CDC path (`turso_cdc.rs`, `start_clustered_turso_cdc`) over the cluster channel — no sqld child process, no gRPC WAL-frame transport. The primary's `turso_cdc` stream is tailed and per-transaction batches are shipped to replicas; cold replicas bootstrap from a logical snapshot. Primary election still uses the gossip KV tier (`kv:sqlite:primary`, `ephpm-cluster/src/sqlite_election.rs`). The Turso engine is Beta upstream, so **clustered mode is experimental**.

The `TrackedBackend` wrapper in `ephpm-server/src/tracked_backend.rs` wraps any litewire backend to record query stats. Disable with `[db.analysis] query_stats = false`.

## Critical Conventions

- **Conditional compilation**: All PHP FFI code is gated with `#[cfg(php_linked)]`. The `php_linked` cfg is set by `ephpm-php/build.rs` when `PHP_SDK_PATH` env var is present. Stub mode must always compile and pass tests without it.
- **C wrapper required**: PHP uses setjmp/longjmp for error handling. Never call PHP functions directly from Rust without going through `ephpm_wrapper.c` and its `zend_try/zend_catch` guards — otherwise SIGSEGV.
- **PHP threading**: ZTS (Zend Thread Safety) is implemented. PHP is compiled with `--enable-zts` and each `spawn_blocking` thread auto-registers with TSRM on first use, getting its own isolated PHP context. No dedicated worker pool — tokio's `spawn_blocking` pool is the thread pool. A `Mutex` protects only one-time `init()`/`shutdown()`, not request execution. An `AtomicBool` fast-path check avoids the mutex for the common "is PHP ready?" path. Per-request C statics use `__thread` for thread isolation. Windows stays NTS (`ZTS=0`) due to DLL constraints.
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
| `ephpm-server/src/turso_cdc.rs` | `start_clustered_turso_cdc()` — the in-process Turso CDC clustered replication path |
| `ephpm-server/src/tracked_backend.rs` | `TrackedBackend<B>` — wraps litewire `Backend` with query stats |
| `ephpm-server/src/router.rs` | HTTP request routing, PHP dispatch, static file serving |
| `ephpm-server/src/websocket.rs` | Native WebSocket upgrade + per-connection session task (`[server.websocket]`, experimental) |
| `ephpm-php/src/ws_bridge.rs` | `ephpm_ws_*` native functions — per-thread site scope + current-connection context |
| `ephpm-config/src/lib.rs` | All config structs: `SqliteConfig`, `ReplicationConfig`, `ClusterConfig`, `DbAnalysisConfig` |
| `ephpm-cluster/src/sqlite_election.rs` | Primary election via gossip KV (lowest ordinal wins, TTL heartbeat) |
| `ephpm-query-stats/src/digest.rs` | SQL normalizer (state machine replacing literals with `?`) |
| `ephpm-query-stats/src/lib.rs` | `QueryStats` — DashMap-based digest tracking, Prometheus metrics |
| `xtask/src/main.rs` | Build tooling — PHP SDK download, release/e2e commands |

## Git & Remotes

- **`origin`** = `github.com/ephpm/ephpm.git` (org repo, source of truth)
- Local `main` tracks `origin/main`
- The old `luthermonson/ephpm.git` remote was removed

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
