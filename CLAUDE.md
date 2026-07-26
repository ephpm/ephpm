# ePHPm — Embedded PHP Manager

An all-in-one PHP application server written in Rust that embeds PHP via FFI into a single binary. Runs WordPress, Laravel, etc. without external PHP-FPM. Includes embedded SQLite (via litewire), gossip clustering, and a built-in KV store.

## Build & Run

```bash
# Stub mode (no PHP, fast iteration on HTTP/routing logic)
cargo build

# Preflight: check build prerequisites (exits non-zero if a required tool is missing)
cargo xtask doctor            # add --target windows to check the Windows-build toolchain

# Release binary with PHP + sqld embedded
cargo xtask release           # → target/release/ephpm (PHP 8.5, sqld auto-downloaded)
cargo xtask release 8.4       # → target/release/ephpm (PHP 8.4)
cargo xtask release --no-sqld # skip sqld embedding (single-node SQLite only)

# Windows .exe (cross-compiled from WSL, requires cargo-xwin)
cargo install cargo-xwin
cargo xtask release --target windows       # → target/x86_64-pc-windows-msvc/release/ephpm.exe
# Note: Windows builds never include sqld (no Windows binary available from Turso)
```

Prerequisites for `cargo xtask release`: git, curl, tar, `build-essential`, `pkg-config`, and `libclang-dev` (for bindgen). On Linux the build targets the host-default `<arch>-unknown-linux-gnu` triple against the glibc-linked (`-gnu`) `libphp.a` — the resulting binary is a single glibc-dynamic file that can `dlopen()` shared PHP extensions and middleware; no musl toolchain is involved. The xtask downloads the PHP SDK from `github.com/ephpm/php-sdk` releases and the sqld binary from Turso releases — no PHP CLI, Composer, or static-php-cli needed.

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
| `ephpm-cluster` | Clustering — SWIM gossip (chitchat), consistent hash ring, KV replication, SQLite primary election |
| `ephpm-sqld` | sqld embedding — binary extraction via `include_bytes!()`, child process lifecycle, health checks |
| `ephpm-query-stats` | Query observability — SQL normalization, digest tracking, slow query logging, Prometheus metrics |
| `xtask` | Build & test tooling — `release`, `php-sdk`, `e2e` (bare-process default), `k8s-e2e`/`k8s-e2e-up`/`k8s-e2e-down` (opt-in Kind path) |

## External Dependencies

| Dependency | Location | Purpose |
|-----------|----------|---------|
| **litewire** | `../litewire/crates/litewire` (path dep) | MySQL/Hrana wire protocol → SQLite translation proxy |
| **PHP SDK** | Downloaded by `cargo xtask php-sdk` from `github.com/ephpm/php-sdk` releases | Prebuilt `libphp.a` (Linux/macOS) or `php8embed.{dll,lib}` (Windows) plus PHP headers. Pinned per minor in `xtask/src/main.rs::PHP_SDK_VERSIONS` |
| **sqld** | Embedded via `include_bytes!()` at build time | SQLite replication server for clustered mode (v0.24.32 pinned in xtask) |

litewire is a standalone project at `github.com/ephpm/litewire`. It's used as a library — ePHPm calls `LiteWire::new(backend).mysql(addr).serve()`.

The PHP SDK is built by a separate pipeline at `github.com/ephpm/php-sdk` (uses static-php-cli internally). ePHPm itself doesn't depend on static-php-cli at all — it just consumes the resulting tarballs.

## Architecture: Database

Three database modes, all transparent to PHP (`pdo_mysql` connects to `127.0.0.1:3306`):

1. **DB Proxy** (`[db.mysql]`) — forwards MySQL wire traffic to a real MySQL/PG server with connection pooling
2. **Single-node SQLite** (`[db.sqlite]`) — litewire + rusqlite in-process, no external database
3. **Clustered SQLite** (`[db.sqlite]` + `[cluster]`) — litewire + sqld sidecar, WAL frame replication via gRPC

Mode detection (`is_clustered_sqlite()` in `crates/ephpm-server/src/lib.rs`):
- If `replication.role = "primary"` or `"replica"` → clustered
- If `replication.role = "auto"` AND `cluster.enabled = true` → clustered (election via gossip)
- Otherwise → single-node (rusqlite in-process)

Note that `replication.role` defaults to `"auto"`. So omitting `[db.sqlite.replication]` entirely is identical to setting `role = "auto"` — clustered mode if `[cluster].enabled = true`, single-node otherwise. To force single-node even with clustering on, set `replication.role` to anything other than `"primary"`, `"replica"`, or `"auto"` (e.g. `"single"`).

Clustered mode spawns sqld as a child process. Primary election uses the gossip KV tier (`kv:sqlite:primary`). On failover, the role-change watcher restarts sqld in the new mode.

The `TrackedBackend` wrapper in `ephpm-server/src/tracked_backend.rs` wraps any litewire backend to record query stats. Disable with `[db.analysis] query_stats = false`.

## Architecture: sqld Binary Embedding

sqld v0.24.32 is downloaded from Turso's GitHub releases by `cargo xtask release` and embedded via `SQLD_BINARY_PATH` env var → `ephpm-sqld/build.rs` → `include_bytes!()`. At runtime:
1. Extract to temp file
2. `chmod +x`
3. Spawn as child process with args based on role (primary/replica)
4. Poll `/health` until ready
5. On role change: SIGTERM → wait → re-spawn with new args

Without `SQLD_BINARY_PATH` (dev builds), `ephpm-sqld` compiles in stub mode — `SqldProcess::spawn()` returns an error. This is gated by `#[cfg(sqld_embedded)]`.

## Critical Conventions

- **Conditional compilation**: All PHP FFI code is gated with `#[cfg(php_linked)]`. The `php_linked` cfg is set by `ephpm-php/build.rs` when `PHP_SDK_PATH` env var is present. Stub mode must always compile and pass tests without it.
- **C wrapper required**: PHP uses setjmp/longjmp for error handling. Never call PHP functions directly from Rust without going through `ephpm_wrapper.c` and its `zend_try/zend_catch` guards — otherwise SIGSEGV.
- **PHP threading**: ZTS (Zend Thread Safety) is implemented. PHP is compiled with `--enable-zts` and each `spawn_blocking` thread auto-registers with TSRM on first use, getting its own isolated PHP context. No dedicated worker pool — tokio's `spawn_blocking` pool is the thread pool. A `Mutex` protects only one-time `init()`/`shutdown()`, not request execution. An `AtomicBool` fast-path check avoids the mutex for the common "is PHP ready?" path. Per-request C statics use `__thread` for thread isolation. Windows stays NTS (`ZTS=0`) due to DLL constraints.
- **MSRV**: Rust 1.85 — do not use features from newer editions without checking.
- **Clippy**: Pedantic + all warnings denied (`-D warnings`). Zero warnings policy.
- **Rustfmt**: 2024 edition style, `group_imports = "StdExternalCrate"`. Requires **nightly** toolchain (`cargo +nightly fmt`).
- **Error handling**: `thiserror` for domain errors, `anyhow` for propagation with context. Always add context to errors with `.context()`.
- **Logging**: `tracing` crate. Use appropriate levels — debug for request details, info for lifecycle events, warn/error for problems.
- **sqld on Windows**: Not supported. Turso does not publish Windows binaries. The xtask errors if you try `--sqld-binary` with `--target windows`. Runtime: `start_clustered_sqlite()` bails with a clear message on `#[cfg(target_os = "windows")]`. Single-node SQLite works fine on Windows.

## Code Style

- Crate names: `ephpm-*` (kebab-case)
- Safety comments (`// SAFETY:`) before every `unsafe` block explaining FFI invariants
- Public API documentation with `///` on all exported items
- Module-level docs with `//!` explaining purpose and design

## Key Files

| File | What it does |
|------|-------------|
| `ephpm-server/src/lib.rs` | `serve()` entry point, cluster startup, `start_db_proxies()` with single-node and clustered SQLite branches |
| `ephpm-server/src/tracked_backend.rs` | `TrackedBackend<B>` — wraps litewire `Backend` with query stats |
| `ephpm-server/src/router.rs` | HTTP request routing, PHP dispatch, static file serving |
| `ephpm-config/src/lib.rs` | All config structs: `SqliteConfig`, `SqldConfig`, `ReplicationConfig`, `ClusterConfig`, `DbAnalysisConfig` |
| `ephpm-cluster/src/sqlite_election.rs` | Primary election via gossip KV (lowest ordinal wins, TTL heartbeat) |
| `ephpm-sqld/src/lib.rs` | `SqldProcess` — spawn, health check, restart, shutdown |
| `ephpm-query-stats/src/digest.rs` | SQL normalizer (state machine replacing literals with `?`) |
| `ephpm-query-stats/src/lib.rs` | `QueryStats` — DashMap-based digest tracking, Prometheus metrics |
| `xtask/src/main.rs` | Build tooling — `SQLD_VERSION` pinned, `download_sqld()`, `parse_sqld_binary()` |

## Git & Remotes

- **`origin`** = `github.com/ephpm/ephpm.git` (org repo, source of truth)
- Local `main` tracks `origin/main`
- The old `luthermonson/ephpm.git` remote was removed

## CI Pipeline

Runs on push/PR to main: fmt check → clippy → test → cargo-deny. Release builds triggered by `v*` tags across PHP 8.4/8.5 × Linux/macOS matrix.

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
