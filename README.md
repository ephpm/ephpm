# ePHPm — Embedded PHP Manager

An all-in-one PHP application server written in Rust. Embeds PHP via FFI into a single static binary — no external PHP-FPM, no CGO overhead, no runtime dependencies. Drop in your WordPress or Laravel project and go.

## Why ePHPm?

| | ePHPm | FrankenPHP | RoadRunner | Swoole | Apache + mod_php | Nginx + php-fpm |
|---|---|---|---|---|---|---|
| Language | Rust | Go (CGO) | Go | PHP + C | C | C + PHP |
| PHP FFI overhead | Zero (native C call) | ~2.2μs/req (11+ CGO crossings) | N/A (worker mode) | N/A (native) | N/A (in-process) | IPC (FastCGI) |
| GC pauses | None | Go GC | Go GC | PHP GC | PHP GC | PHP GC |
| Binary | Single static binary | Caddy module | Go binary + PHP workers | PHP + extension | Apache + modules | Nginx + separate FPM |
| DB proxy | Planned | No | No | Connection pool | No | No |
| Clustering | Planned | No | No | Built-in | Manual | Manual |
| PHP compatibility | Drop-in (embed SAPI) | Drop-in (worker SAPI) | Requires PSR-7 packages | Requires async code | Native (100%) | Native (100%) |
| Deployment | Single binary | Requires Caddy | Multi-process | Requires PHP + Swoole extension | Apache + modules | Separate services |
| Container-friendly | ✓ (single binary) | ✓ (Caddy module) | ✓ | ⚠️ (PHP + extension) | ⚠️ (heavier) | ⚠️ (two services) |

## Feature Status

The docs describe the full vision. Here's what actually exists today:

| Feature | Status |
|---------|--------|
| HTTP/1.1 serving | **Implemented** |
| Static file serving | **Implemented** |
| PHP embedding (NTS) | **Implemented** |
| Request routing (pretty permalinks) | **Implemented** |
| Configuration (TOML + env vars) | **Implemented** |
| Embedded KV store (strings, TTL, counters) | **Implemented** |
| KV store CLI debugging (`ephpm kv`) | **Implemented** |
| SAPI functions (`ephpm_kv_*` in PHP) | **Implemented** |
| Observability (tracing logs) | Partial |
| Graceful shutdown | Partial |
| CLI | Partial |
| HTTP/2 | Planned |
| TLS / ACME | Planned |
| PHP embedding (ZTS) | Planned |
| DB proxy (MySQL/Postgres) | Planned |
| Clustered KV store (hashes, lists, sets, replication) | Planned |
| Admin UI / API | Planned |
| External PHP mode | Planned |
| OpenTelemetry export | Planned |

## Quick Start

### Stub mode (no PHP, fast iteration)

Requires only [Rust 1.85+](https://rustup.rs):

```bash
cargo build
cargo run -- --config ephpm.toml
```

Serves static files and returns a placeholder for `.php` routes. Good for working on HTTP/routing logic.

### Full build with PHP (xtask)

The xtask builds the PHP SDK via [static-php-cli](https://github.com/crazywhalecc/static-php-cli) and compiles the release binary. First build ~15 min, cached after.

**Linux / macOS:**

```bash
# Install prerequisites (Ubuntu/Debian)
sudo apt install php-cli composer git build-essential autoconf cmake pkg-config re2c

# Build
cargo xtask release       # → target/release/ephpm
```

**Windows (auto-delegates to WSL):**

The xtask detects Windows and automatically re-invokes itself inside WSL. One-time WSL setup:

```powershell
# PowerShell (Admin) — install WSL + Ubuntu
wsl --install
```

After restarting, open Ubuntu from the Start menu and install the tools:

```bash
# Inside WSL
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
sudo apt update && sudo apt install -y php-cli composer git build-essential autoconf cmake pkg-config re2c libclang-dev
```

Then from your normal Windows terminal:

```bash
cargo xtask release       # auto-runs inside WSL
```

## Configuration

```toml
# ephpm.toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/html"
index_files = ["index.php", "index.html"]

[php]
mode = "embedded"           # or "external" to use your own PHP binary
max_execution_time = 30
memory_limit = "128M"
ini_overrides = [
    ["display_errors", "Off"],
    ["error_reporting", "E_ALL"],
]

# External mode (use any PHP binary — custom extensions, custom builds):
# [php]
# mode = "external"
# binary = "/usr/bin/php"
# workers = 4
```

All config values can be overridden with `EPHPM_` prefixed environment variables (e.g., `EPHPM_SERVER__LISTEN=0.0.0.0:9090`).

## Roadmap

### MVP (current)

Single-process PHP application server that can host WordPress out of the box:
- HTTP/1.1 server with static file serving and PHP routing
- NTS PHP embedded via custom SAPI (mutex-serialized, `spawn_blocking`)
- TOML configuration with env var overrides
- `cargo xtask release` builds PHP SDK via static-php-cli and links the binary

### v0.2 — Production Hardening

- HTTP/2 via hyper's auto-negotiation
- TLS termination with automatic ACME certificates (rustls)
- Graceful shutdown with connection draining
- SIGHUP-based config reload
- `ephpm doctor` diagnostic command
- **External PHP mode** — spawn worker processes using any PHP binary (`php.mode = "external"`), for custom builds/extensions without rebuilding ePHPm

### v0.3 — Observability

- OpenTelemetry tracing and metrics export
- Request debug mode
- Structured log output (JSON)

### v1 — Performance

- ZTS PHP with thread-per-request model (replacing mutex serialization)
- Connection keep-alive tuning
- Benchmark suite (Criterion)

### Future

- DB proxy with connection pooling, query digest, slow query analysis (MySQL + Postgres wire protocol)
- Clustered KV store with gossip protocol
- Admin UI and management API
- Extension suites (curated PHP extension bundles)
- Multi-node deployment with consistent hashing

## Project Structure

```
crates/
├── ephpm/           CLI binary — clap args, config loading, server boot
├── ephpm-server/    HTTP server — hyper + tokio, routing, static files
├── ephpm-php/       PHP embedding — FFI bindings, SAPI, request/response
└── ephpm-config/    Configuration — figment, TOML + env var overrides
```

Key design decisions:
- **Conditional compilation** — All PHP FFI code is gated behind `#[cfg(php_linked)]`. Stub mode compiles and tests without a PHP SDK.
- **C wrapper for safety** — PHP uses `setjmp`/`longjmp` for error handling. All Rust→PHP calls go through `ephpm_wrapper.c` with `zend_try`/`zend_catch` guards to prevent stack corruption.
- **Async I/O, blocking PHP** — tokio handles HTTP connections. PHP execution runs on `spawn_blocking` threads behind a `Mutex` (NTS).

## Contributing

### Prerequisites

- **Rust 1.85+** — https://rustup.rs (on Windows, also install [C++ Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/))
- **Nightly Rust** — `rustup toolchain install nightly` (required for `cargo +nightly fmt`)
- **cargo-nextest** — `cargo install cargo-nextest --locked`
- **cargo-deny** — `cargo install cargo-deny --locked`
- **WSL + Ubuntu** (Windows only) — needed for `cargo xtask release` (see Quick Start above)

See [docs/developer/getting-started.md](docs/developer/getting-started.md) for detailed setup instructions including per-platform Rust installation.

### Workflow

Most development uses stub mode — no PHP SDK or container engine needed:

```bash
# Build (stub mode)
cargo build

# Run tests (prefer single-crate runs)
cargo nextest run -p ephpm-server

# Lint (must pass with zero warnings)
cargo clippy --workspace --all-targets -- -D warnings

# Format (requires nightly)
cargo +nightly fmt --all

# Dependency audit
cargo deny check
```

### Build & test tooling (xtask)

The project uses [cargo-xtask](https://github.com/matklad/cargo-xtask) for build automation and E2E testing:

```bash
cargo xtask release     # Build PHP SDK + ephpm binary (release mode)
cargo xtask php-sdk     # Build only the static PHP SDK (~15 min first time)
cargo xtask e2e-install # Download kind, tilt, kubectl to ./bin (no global install)
cargo xtask e2e         # Run E2E tests (creates Kind cluster, builds images, tilt ci)
cargo xtask e2e-up      # Start E2E dev env (tilt dashboard at localhost:10350)
cargo xtask e2e-down    # Tear down Kind cluster
```

On Windows, `release` and `php-sdk` auto-detect the platform and re-invoke themselves inside WSL. The PHP SDK is cached at `php-sdk/static-php-cli/buildroot/` — delete that directory to force a rebuild.

E2E commands require Podman or Docker. Run `cargo xtask e2e-install` to download kind/tilt/kubectl to `./bin/` — no global install needed. See [docs/developer/testing.md](docs/developer/testing.md) for details.

### Code conventions

- **Clippy**: Pedantic + all warnings denied. Zero warnings policy.
- **Formatting**: 2024 edition style, grouped imports. Run `cargo +nightly fmt --all`.
- **Error handling**: `thiserror` in library crates, `anyhow` in the binary. Always add `.context()`.
- **Logging**: `tracing` crate — debug for requests, info for lifecycle, warn/error for problems.
- **Unsafe code**: Safety comment (`// SAFETY:`) before every `unsafe` block explaining invariants.
- **Documentation**: `///` on public items, `//!` at module level.

## Docs

- [Getting started](docs/developer/getting-started.md) — Prerequisites, building, IDE setup
- [Testing strategy](docs/developer/testing.md) — Unit tests, Tilt + Kind E2E, database testing
- [Architecture decisions](docs/architecture/architecture.md) — Language choice, crate design, PHP execution modes
- [Implementation guide](docs/architecture/implementation.md) — Build system, CI, MVP spec
- [CLI design](docs/architecture/cli.md) — Command structure, UX principles
- [Security model](docs/architecture/security.md) — Threat model, FFI safety, trust boundaries
- [Competitive analysis](docs/analysis/) — FrankenPHP, RoadRunner, Swoole comparisons

## License

MIT
