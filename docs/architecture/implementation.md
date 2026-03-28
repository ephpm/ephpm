# ePHPm Implementation Guide

This document covers the repository structure, tooling, PHP embedding strategy, CI pipeline, and MVP specification — everything needed to start building ePHPm.

---

## Repository Structure

Cargo workspace with virtual manifest and `crates/` directory (standard for multi-crate Rust projects, used by rust-analyzer, Pingora, etc.):

```
ephpm/
├── Cargo.toml                  # Virtual manifest ([workspace] only)
├── Cargo.lock
├── rust-toolchain.toml
├── rustfmt.toml
├── clippy.toml
├── deny.toml
├── ephpm.toml                  # Example config file
├── .github/
│   └── workflows/
│       ├── ci.yml              # Lint, test, deny
│       └── release.yml         # Build matrix (PHP 8.3/8.4 × linux/mac/windows)
├── crates/
│   ├── ephpm/                  # Binary crate (main entry point)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── main.rs         # CLI (clap), config loading, server boot
│   ├── ephpm-server/           # HTTP server crate
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── router.rs       # Route .php to PHP, else static files
│   │       └── static_files.rs # Static file serving
│   ├── ephpm-php/              # PHP embedding crate
│   │   ├── Cargo.toml
│   │   ├── build.rs            # bindgen + link libphp.a
│   │   ├── wrapper.h           # C header includes for bindgen
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── sapi.rs         # Custom SAPI implementation
│   │       ├── request.rs      # HTTP request → PHP request mapping
│   │       └── response.rs     # PHP output → HTTP response mapping
│   └── ephpm-config/           # Configuration crate
│       ├── Cargo.toml
│       └── src/
│           └── lib.rs          # Config structs + figment loading
├── benches/
│   └── throughput.rs           # Criterion benchmarks
├── tests/
│   └── integration/
│       └── wordpress.rs        # WordPress smoke test
└── docs/
    ├── analysis/               # Competitive analysis
    └── architecture/           # Architecture docs
```

### Crate Responsibilities

| Crate | Type | Purpose |
|-------|------|---------|
| `ephpm` | Binary | CLI entry point. Parses args, loads config, boots PHP runtime, starts HTTP server, handles graceful shutdown |
| `ephpm-server` | Library | HTTP server (hyper + tokio), request routing, static file serving |
| `ephpm-php` | Library | PHP embedding via FFI. Custom SAPI, request/response mapping, PHP lifecycle management |
| `ephpm-config` | Library | Configuration structs, TOML loading via figment, env var overrides |

### Root Cargo.toml (Virtual Manifest)

```toml
[workspace]
members = ["crates/*"]
resolver = "3"

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.85"
license = "MIT"
repository = "https://github.com/user/ephpm"

[workspace.dependencies]
tokio = { version = "1", features = ["full"] }
hyper = { version = "1", features = ["http1", "http2", "server"] }
hyper-util = "0.1"
http-body-util = "0.1"
tower = { version = "0.5", features = ["full"] }
serde = { version = "1", features = ["derive"] }
figment = { version = "0.10", features = ["toml", "env"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
thiserror = "2"
anyhow = "1"
clap = { version = "4", features = ["derive"] }

[workspace.lints.rust]
unsafe_code = "warn"

[workspace.lints.clippy]
all = "warn"
pedantic = "warn"
```

Member crates inherit from the workspace:

```toml
# crates/ephpm/Cargo.toml
[package]
name = "ephpm"
version.workspace = true
edition.workspace = true
rust-version.workspace = true

[dependencies]
ephpm-server = { path = "../ephpm-server" }
ephpm-php = { path = "../ephpm-php" }
ephpm-config = { path = "../ephpm-config" }
tokio = { workspace = true }
clap = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
anyhow = { workspace = true }

[lints]
workspace = true
```

---

## Tooling

### Essential Tools

| Tool | Purpose | Install |
|------|---------|---------|
| `rustfmt` | Code formatting | Ships with rustup |
| `clippy` | Linting | Ships with rustup |
| [`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny) | License audit, advisory DB, duplicate crate detection | `cargo install cargo-deny` |
| [`cargo-nextest`](https://nexte.st/) | Faster test runner with better output | `cargo install cargo-nextest` |
| [`cargo-llvm-cov`](https://crates.io/crates/cargo-llvm-cov) | Code coverage | `cargo install cargo-llvm-cov` |
| [`criterion`](https://bheisler.github.io/criterion.rs/book/) | Benchmarking framework | Dev dependency |
| [`bindgen`](https://github.com/rust-lang/rust-bindgen) | Generate Rust FFI bindings from PHP C headers | Build dependency |

### Configuration Files

**`rust-toolchain.toml`**

```toml
[toolchain]
channel = "stable"
components = ["rustfmt", "clippy", "llvm-tools-preview"]
```

**`rustfmt.toml`**

```toml
style_edition = "2024"
use_small_heuristics = "Max"
group_imports = "StdExternalCrate"
imports_granularity = "Module"
```

**`deny.toml`**

Generated via `cargo deny init`. Key configuration:

```toml
[licenses]
allow = [
    "MIT",
    "Apache-2.0",
    "Apache-2.0 WITH LLVM-exception",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "Unicode-3.0",
]

[advisories]
db-path = "~/.cargo/advisory-db"
db-urls = ["https://github.com/rustsec/advisory-db"]
```

---

## CI Pipeline

### GitHub Actions: ci.yml

```yaml
name: CI
on: [push, pull_request]

jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo check --workspace --all-targets

  fmt:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt
      - run: cargo fmt --all -- --check

  clippy:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy
      - uses: Swatinem/rust-cache@v2
      - run: cargo clippy --workspace --all-targets -- -D warnings

  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - uses: taiki-e/install-action@nextest
      - run: cargo nextest run --workspace

  deny:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: EmbarkStudios/cargo-deny-action@v2

  msrv:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@1.85.0
      - run: cargo check --workspace
```

### GitHub Actions: release.yml

```yaml
name: Release
on:
  push:
    tags: ['v*']

jobs:
  build:
    strategy:
      matrix:
        php: ['8.3', '8.4']
        include:
          # Linux
          - os: ubuntu-latest
            target: x86_64-unknown-linux-gnu
            artifact_suffix: linux-x86_64
            binary_name: ephpm
          - os: ubuntu-24.04-arm
            target: aarch64-unknown-linux-gnu
            artifact_suffix: linux-aarch64
            binary_name: ephpm
          # macOS
          - os: macos-latest
            target: aarch64-apple-darwin
            artifact_suffix: macos-aarch64
            binary_name: ephpm
          - os: macos-13
            target: x86_64-apple-darwin
            artifact_suffix: macos-x86_64
            binary_name: ephpm
          # Windows
          - os: windows-latest
            target: x86_64-pc-windows-msvc
            artifact_suffix: windows-x86_64
            binary_name: ephpm.exe
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable

      - name: Build PHP static library
        run: |
          # Download and build static-php-cli
          # Build libphp.a with WordPress-required extensions
          bin/spc download --with-php=${{ matrix.php }} \
            --for-extensions="bcmath,curl,dom,exif,fileinfo,filter,gd,hash,iconv,json,mbstring,mysqli,openssl,pcre,session,simplexml,sodium,xml,xmlreader,zip,zlib"
          bin/spc build \
            "bcmath,curl,dom,exif,fileinfo,filter,gd,hash,iconv,json,mbstring,mysqli,openssl,pcre,session,simplexml,sodium,xml,xmlreader,zip,zlib" \
            --build-embed

      - name: Build Rust binary
        env:
          PHP_VERSION: ${{ matrix.php }}
          LIBPHP_DIR: ./buildroot
        run: cargo build --release

      - name: Upload artifact
        uses: actions/upload-artifact@v4
        with:
          name: ephpm-php${{ matrix.php }}-${{ matrix.artifact_suffix }}
          path: target/release/${{ matrix.binary_name }}

  release:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@v4
      - name: Create GitHub Release
        uses: softprops/action-gh-release@v2
        with:
          files: ephpm-*/*
```

Release artifacts are named per platform. One binary per PHP version per platform because `libphp.a` is statically linked.

### Platform Support

| Platform | Target Triple | Production | Development | Notes |
|---|---|---|---|---|
| Linux x86_64 | `x86_64-unknown-linux-gnu` | **Primary** | **Primary** | Servers, CI, WSL2 |
| Linux aarch64 | `aarch64-unknown-linux-gnu` | **Primary** | Supported | AWS Graviton, Ampere, Raspberry Pi |
| macOS Apple Silicon | `aarch64-apple-darwin` | Supported | **Primary** | M1/M2/M3/M4 dev machines |
| macOS Intel | `x86_64-apple-darwin` | Supported | Supported | Older Macs, CI runners |
| Windows x86_64 | `x86_64-pc-windows-msvc` | **Not targeted** | **Primary** | Local dev via native or WSL2 |

**Production vs development:** ePHPm targets Linux for production deployments. macOS and Windows builds exist so developers can run `ephpm serve` locally during development without needing Docker/Podman or WSL. The development experience should feel native on all three operating systems.

**Windows notes:**
- Windows builds use MSVC toolchain, not MinGW
- PHP embed SAPI supports Windows — static-php-cli can produce `php8embed.lib` on Windows
- Signals (`SIGTERM`, `SIGHUP`) are replaced with Windows equivalents: `Ctrl+C` handler via `SetConsoleCtrlHandler`, named pipe or TCP for reload
- Forward slashes work in paths for `ephpm.toml` config values, but native backslash paths are also accepted
- `ephpm ext build` uses the same container-based approach (Podman/Docker works on Windows)

**Fully static binaries on all platforms:** All extensions are compiled into the binary at build time — no runtime `.so`/`.dll` loading. This means:

| Platform | Linking | Runtime dependencies |
|---|---|---|
| Linux | Fully static (musl) | **None** — works on any distro, Alpine, `FROM scratch` |
| macOS | Static libphp, dynamic libSystem | `libSystem.dylib` (always present, Apple-mandated) |
| Windows | Static libphp, static CRT (`/MT`) | Windows system libraries only |

---

## PHP Embedding Strategy

### Thread Safety: NTS for MVP, ZTS for v1

| | MVP | v1 (Production) |
|---|---|---|
| **PHP build** | NTS (Non-Thread-Safe) | ZTS (Zend Thread-Safe) |
| **Concurrency model** | `Mutex<PhpRuntime>` + `spawn_blocking` | Thread-per-request pool (like FrankenPHP) |
| **Throughput** | One PHP request at a time per process | N concurrent PHP requests per process |
| **Complexity** | Low — provably correct | High — thread-local storage, per-request isolation |
| **Sufficient for** | WordPress demo, proof of concept | Production workloads |

NTS is simpler because PHP's internal state (globals, allocator, OPcache) is process-wide. With ZTS, each thread gets its own copy of globals via TSRM (Thread-Safe Resource Manager), which FrankenPHP also uses.

### Building libphp.a

[`static-php-cli`](https://github.com/crazywhalecc/static-php-cli) (v2.7.9) builds a fully static `libphp.a` with selected extensions:

```bash
# Download static-php-cli
curl -fsSL https://dl.static-php.dev/static-php-cli/spc-bin/nightly/spc-linux-x86_64.tar.gz | tar xz

# Download PHP source + extension dependencies
bin/spc download --with-php=8.4 \
  --for-extensions="bcmath,curl,dom,exif,fileinfo,filter,gd,hash,iconv,json,mbstring,mysqli,openssl,pcre,session,simplexml,sodium,xml,xmlreader,zip,zlib"

# Build static libphp.a with embed SAPI
bin/spc build \
  "bcmath,curl,dom,exif,fileinfo,filter,gd,hash,iconv,json,mbstring,mysqli,openssl,pcre,session,simplexml,sodium,xml,xmlreader,zip,zlib" \
  --build-embed

# Output:
#   buildroot/lib/libphp.a          ← link this
#   buildroot/include/php/...       ← bindgen reads these headers
```

Supported platforms: Linux (x86_64, aarch64), macOS (Intel, Apple Silicon), Windows.

### Extensions Required for WordPress

From the [WordPress server environment handbook](https://make.wordpress.org/hosting/handbook/server-environment/):

| Extension | Purpose | Required? |
|-----------|---------|-----------|
| `json` | REST API, settings, plugins | Strictly required |
| `mysqli` | Database access | Strictly required |
| `mbstring` | UTF-8 string handling | Functionally required |
| `xml` / `dom` / `simplexml` | RSS, sitemaps, plugin updates | Functionally required |
| `curl` | HTTP requests (update checks, REST) | Functionally required |
| `openssl` | HTTPS connections | Functionally required |
| `hash` | Password hashing, nonces | Functionally required |
| `pcre` | Regular expressions | Functionally required |
| `fileinfo` | MIME type detection | Functionally required |
| `gd` | Image manipulation (thumbnails) | Functionally required |
| `zip` | Plugin/theme installation | Functionally required |
| `session` | Used by some plugins | Recommended |
| `sodium` | Modern cryptography | Recommended |
| `exif` | Image metadata | Recommended |
| `iconv` | Character encoding | Recommended |
| `zlib` | Compression | Recommended |

### FFI Approach: bindgen + Custom SAPI

The `ephpm-php` crate uses `bindgen` in `build.rs` to generate Rust FFI bindings from PHP's C headers:

**`crates/ephpm-php/wrapper.h`**

```c
#include <sapi/embed/php_embed.h>
#include <main/SAPI.h>
#include <main/php_main.h>
#include <main/php_variables.h>
#include <Zend/zend.h>
#include <Zend/zend_exceptions.h>
```

**`crates/ephpm-php/build.rs`**

```rust
use std::env;
use std::path::PathBuf;

fn main() {
    let php_dir = env::var("LIBPHP_DIR")
        .unwrap_or_else(|_| "/usr/local".to_string());

    // Tell cargo to link against libphp.a
    println!("cargo:rustc-link-lib=static=php");
    println!("cargo:rustc-link-search={}/lib", php_dir);

    // Also link system libraries that PHP depends on
    println!("cargo:rustc-link-lib=dylib=xml2");
    println!("cargo:rustc-link-lib=dylib=z");
    println!("cargo:rustc-link-lib=dylib=curl");
    println!("cargo:rustc-link-lib=dylib=ssl");
    println!("cargo:rustc-link-lib=dylib=crypto");

    // Generate Rust FFI bindings
    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{}/include/php", php_dir))
        .clang_arg(format!("-I{}/include/php/main", php_dir))
        .clang_arg(format!("-I{}/include/php/Zend", php_dir))
        .clang_arg(format!("-I{}/include/php/TSRM", php_dir))
        .clang_arg(format!("-I{}/include/php/sapi/embed", php_dir))
        .generate()
        .expect("Unable to generate PHP bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("php_bindings.rs"))
        .expect("Couldn't write bindings");
}
```

### SAPI Implementation

The custom SAPI is the bridge between ePHPm's HTTP server and PHP's engine. These are the C callback functions that PHP calls during request processing:

| Callback | Called When | ePHPm Action |
|----------|------------|--------------|
| `ub_write(str, len)` | PHP outputs data (`echo`, `print`, template rendering) | Append to response body buffer |
| `send_headers(sapi_headers)` | PHP is ready to send response headers | Capture status code + headers into response struct |
| `send_header(header, replace, status)` | PHP sets an individual response header | Store in response headers map |
| `read_post(buf, count)` | PHP reads POST body (`$_POST`, `php://input`) | Copy from HTTP request body |
| `read_cookies()` | PHP needs the raw cookie string | Return `Cookie` header value |
| `register_server_variables(track_vars)` | PHP populates `$_SERVER` | Register all `$_SERVER` vars from HTTP request |
| `startup(sapi_module)` | PHP initializes (MINIT phase) | Initialize extensions, set INI values |
| `shutdown(sapi_module)` | PHP shuts down (MSHUTDOWN phase) | Cleanup |
| `activate()` | Per-request init (RINIT phase) | Reset request-specific state |
| `deactivate()` | Per-request cleanup (RSHUTDOWN phase) | Flush output, cleanup |
| `flush()` | PHP flushes output buffer | Forward buffered output |
| `log_message(msg, level)` | PHP logs an error/warning | Route to `tracing` |
| `get_request_time()` | PHP accesses `$_SERVER['REQUEST_TIME']` | Return request start timestamp |

### $_SERVER Variables WordPress Needs

These must be populated in `register_server_variables`:

```
REQUEST_URI          /path?query=string
REQUEST_METHOD       GET, POST, PUT, DELETE, etc.
SCRIPT_FILENAME      /var/www/wordpress/index.php (absolute path)
SCRIPT_NAME          /index.php
DOCUMENT_ROOT        /var/www/wordpress
SERVER_NAME          example.com (from Host header)
SERVER_PORT          8080
SERVER_SOFTWARE      ePHPm/0.1.0
SERVER_PROTOCOL      HTTP/1.1
HTTPS                "on" if TLS (empty if not)
HTTP_HOST            example.com:8080 (raw Host header)
HTTP_COOKIE          raw Cookie header value
CONTENT_TYPE         Content-Type header (for POST)
CONTENT_LENGTH       Content-Length header (for POST)
QUERY_STRING         query=string (URL query component)
PATH_INFO            extra path info after script
PHP_SELF             /index.php
REMOTE_ADDR          client IP address
REMOTE_PORT          client port number
```

Additionally, all HTTP request headers are exposed as `HTTP_*` variables (uppercase, hyphens replaced with underscores):
- `Accept` → `HTTP_ACCEPT`
- `User-Agent` → `HTTP_USER_AGENT`
- `Authorization` → `HTTP_AUTHORIZATION`

### Reference Implementations

| Project | Language | What to Study |
|---------|----------|---------------|
| [FrankenPHP `frankenphp.c`](https://github.com/dunglas/frankenphp) | C + Go | The gold standard SAPI for embedded PHP. Study `frankenphp_sapi_module` callbacks, superglobal population, worker lifecycle |
| [ripht-php-sapi](https://github.com/jhavenz/ripht-php-sapi) | Rust | Rust bindings for embed SAPI. NTS only. `WebRequest` builder pattern. Study `ExecutionHooks` trait for output interception |
| [Pasir](https://github.com/el7cosmos/pasir) | Rust | PHP app server using Hyper + Tokio + ext-php-rs. ZTS mode. TOML config. Study the request flow integration |
| [PHP embed SAPI source](https://github.com/php/php-src/tree/master/sapi/embed) | C | The default embed SAPI. Minimal implementation — good starting point |

---

## Configuration

### Format: TOML

TOML is idiomatic for Rust projects (Cargo itself uses TOML). Loaded via [Figment](https://github.com/SergioBenitez/Figment) with layered precedence:

```
defaults < config file (ephpm.toml) < environment variables (EPHPM_*) < CLI args
```

### Example: `ephpm.toml`

```toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/html"
index_files = ["index.php", "index.html"]
# workers = 4                  # future: number of PHP worker threads (ZTS)

[php]
max_execution_time = 30
memory_limit = "128M"
# ini_file = "/etc/php/8.5/php.ini"    # optional: load a custom php.ini
ini_overrides = [
    ["display_errors", "Off"],
    ["error_reporting", "E_ALL"],
]

# [logging]
# level = "info"               # tracing filter level
# format = "json"              # or "pretty" for development

# --- Future sections (not in MVP) ---

# [tls]
# auto = true                  # automatic ACME/Let's Encrypt
# email = "admin@example.com"
# domains = ["example.com", "www.example.com"]

# [db.sqlite]
# path = "./data/app.db"         # file path, or ":memory:" for in-memory
# journal_mode = "wal"
# create = true

# [db.mysql]
# url = "mysql://user:pass@db:3306/myapp"
# min_connections = 5
# max_connections = 50
# inject_env = true

# [db.postgres]
# url = "postgres://user:pass@db:5432/myapp"
# min_connections = 5
# max_connections = 30
# inject_env = true

# [kv]
# memory_limit = "256MB"
# eviction_policy = "allkeys-lru"

# [kv.cluster]
# seeds = ["node-b:7946", "node-c:7946"]

# [observability]
# admin_ui = true
# prometheus = true

# [observability.otlp_export]
# endpoint = "jaeger:4317"
# protocol = "grpc"
```

### Config Loading (ephpm-config crate)

```rust
use figment::{Figment, providers::{Format, Toml, Env, Serialized}};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub php: PhpConfig,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub listen: String,
    pub document_root: String,
    #[serde(default = "default_index_files")]
    pub index_files: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct PhpConfig {
    #[serde(default = "default_max_execution_time")]
    pub max_execution_time: u32,
    #[serde(default = "default_memory_limit")]
    pub memory_limit: String,
    #[serde(default)]
    pub ini_file: Option<PathBuf>,  // path to custom php.ini (applied before ini_overrides)
    #[serde(default)]
    pub ini_overrides: Vec<[String; 2]>,  // INI directives as [key, value] pairs
}

fn default_index_files() -> Vec<String> {
    vec!["index.php".into(), "index.html".into()]
}

fn default_max_execution_time() -> u32 { 30 }
fn default_memory_limit() -> String { "128M".into() }

impl Config {
    pub fn load(path: &str) -> Result<Self, figment::Error> {
        Figment::new()
            .merge(Serialized::defaults(Config::default()))
            .merge(Toml::file(path))
            .merge(Env::prefixed("EPHPM_").split("_"))
            .extract()
    }
}
```

---

## MVP Specification

### MVP Goal

A single Rust binary that reads a TOML config, boots an HTTP server with embedded PHP, and can serve a WordPress site.

### What the MVP Includes

1. **`ephpm` binary** — single Rust binary with PHP statically linked
2. **TOML config** — `ephpm.toml` with `[server]` and `[php]` sections
3. **HTTP server** — hyper-based, HTTP/1.1 + HTTP/2
4. **PHP execution** — custom SAPI, NTS mode, Mutex-guarded
5. **Static file serving** — CSS/JS/images served directly (not through PHP)
6. **WordPress demo** — documented setup: download WordPress, point `document_root`, connect to external MySQL, verify admin panel works

### What the MVP Does NOT Include

- TLS / ACME (use a reverse proxy for now)
- DB proxy / connection pooling
- KV store
- Clustering
- Observability / admin UI
- Worker mode (persistent PHP processes between requests)
- ZTS / multi-threaded PHP execution

### Request Flow

```
Client ──HTTP──► hyper (tokio)
                    │
                    ▼
                Router
                    │
            ┌───────┴───────┐
            │ .php request? │
            └───┬───────┬───┘
            no  │       │ yes
                ▼       ▼
          static file   spawn_blocking
          serving           │
                            ▼
                     Mutex<PhpRuntime>
                            │
                     1. Set SAPI request info
                        (method, URI, headers, body)
                     2. register_server_variables()
                        (populate $_SERVER, $_GET)
                     3. php_request_startup()
                     4. php_execute_script(script_path)
                        ├── ub_write() → buffer body
                        ├── send_header() → capture headers
                        ├── read_post() → provide POST data
                        └── read_cookies() → provide cookies
                     5. php_request_shutdown()
                     6. Return (status, headers, body)
                            │
                            ▼
                Build hyper::Response
                            │
Client ◄──HTTP──────────────┘
```

**URL Rewriting for WordPress:**

WordPress uses "pretty permalinks" (`/2024/03/my-post/` instead of `/?p=123`). This requires routing non-file, non-directory URLs to `index.php`:

```
Request: GET /2024/03/my-post/
  1. Check if /var/www/wordpress/2024/03/my-post/ exists as a file → no
  2. Check if it exists as a directory → no
  3. Route to index.php with REQUEST_URI = /2024/03/my-post/
  4. WordPress's router handles the rest
```

This is the equivalent of nginx's `try_files $uri $uri/ /index.php?$args;`.

### Success Criteria

```bash
# 1. Build
cargo build --release

# 2. Configure
cat > ephpm.toml <<'EOF'
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/wordpress"

[php]
max_execution_time = 30
memory_limit = "128M"
EOF

# 3. Run
./target/release/ephpm --config ephpm.toml

# 4. Verify — all must pass:
```

| Test | What It Validates |
|------|-------------------|
| WordPress install wizard loads | HTTP serving, PHP execution, static files, `$_SERVER` |
| Database connection succeeds | `mysqli` extension, network from PHP |
| Admin login works | `$_POST`, `$_COOKIE`, `$_SESSION`, `set-cookie` headers |
| Create a post | POST requests, form handling, database writes |
| Upload media | `$_FILES`, multipart form parsing, file I/O |
| Frontend CSS/JS/images load | Static file serving with correct MIME types |
| Permalinks work | URL rewriting, `$_SERVER['REQUEST_URI']` |

---

## Key Crates for MVP

| Crate | Version | Purpose |
|-------|---------|---------|
| `tokio` | 1.x | Async runtime |
| `hyper` | 1.x | HTTP/1.1 + HTTP/2 server |
| `hyper-util` | 0.1 | hyper utilities (TokioIo, TokioExecutor) |
| `http-body-util` | 0.1 | HTTP body utilities |
| `tower` | 0.5 | Middleware layer (timeouts, logging) |
| `clap` | 4.x | CLI argument parsing (derive mode) |
| `figment` | 0.10 | Hierarchical config loading (TOML + env) |
| `serde` | 1.x | Serialization/deserialization |
| `tracing` | 0.1 | Structured logging |
| `tracing-subscriber` | 0.3 | Log output formatting |
| `thiserror` | 2.x | Error type definitions (library crates) |
| `anyhow` | 1.x | Error handling (binary crate) |
| `bindgen` | 0.71 | FFI binding generation (build dependency) |
| `mime_guess` | 2.x | MIME type detection for static files |
| `tokio-util` | 0.7 | Async utilities (graceful shutdown) |

---

## Multi-PHP-Version Build Strategy

### One Binary Per PHP Version

`libphp.a` is statically linked into the final binary. Different PHP versions produce different `libphp.a` files with different symbols, so **each PHP version produces a separate binary.**

This is the same approach FrankenPHP uses.

### Version Matrix

| PHP Version | Support Status (March 2026) | ePHPm Support |
|-------------|----------------------------|---------------|
| PHP 8.1 | EOL (December 2025) | Not supported |
| PHP 8.2 | Security-only (until Dec 2026) | Best-effort |
| PHP 8.3 | Active support (until Dec 2026) | **Primary** |
| PHP 8.4 | Active support (until Dec 2027) | **Primary** |
| PHP 8.5 | In development | Track for v1 |

### Release Naming

Binaries are named: `ephpm-{version}-php{php}-{suite}-{platform}`:

```
# Production suites (Linux, fully static musl — zero dependencies)
ephpm-0.1.0-php8.4-core-linux-x86_64
ephpm-0.1.0-php8.4-wordpress-linux-x86_64
ephpm-0.1.0-php8.4-laravel-linux-x86_64
ephpm-0.1.0-php8.4-full-linux-x86_64
ephpm-0.1.0-php8.4-wordpress-linux-aarch64
# ... (same for php8.3, macOS, Windows)

# Development suites (adds xdebug, pcov, spx)
ephpm-0.1.0-php8.4-wordpress-dev-linux-x86_64
ephpm-0.1.0-php8.4-laravel-dev-linux-x86_64
ephpm-0.1.0-php8.4-full-dev-linux-x86_64
ephpm-0.1.0-php8.4-wordpress-dev-macos-aarch64
ephpm-0.1.0-php8.4-laravel-dev-windows-x86_64.exe
# ...
```

**Extension suites:**

| Suite | Extensions | Target audience |
|---|---|---|
| **core** | ~15 exts — minimal PHP (json, pcre, mbstring, openssl, curl, xml, zip, etc.) | Custom builds, minimal footprint |
| **wordpress** | core + mysqli, gd, exif, iconv, simplexml, pdo_sqlite, sqlite3 (~25 exts) | WordPress, CMS apps |
| **laravel** | core + pdo_mysql, pdo_pgsql, pdo_sqlite, sqlite3, redis, gd, intl, bcmath (~30 exts) | Laravel, Symfony, modern frameworks |
| **full** | Everything static-php-cli supports (~100+ exts) | "Just give me everything" |
| ***-dev** | Any suite above + xdebug, pcov, spx (Zend extensions patched into source tree) | Local development, step debugging, coverage |

Dev suites include Zend extensions (xdebug, pcov, spx) that are statically compiled by patching their source into PHP's `ext/` directory before building — the same technique PHP uses for opcache. Dev tools are disabled by default via INI settings and only activate when configured (e.g. `XDEBUG_MODE=debug`). **Dev suites should never be used in production.**

### Container Images

```dockerfile
FROM scratch
COPY ephpm /usr/local/bin/ephpm
ENTRYPOINT ["ephpm"]
```

Fully static musl binaries mean `FROM scratch` works — zero runtime dependencies, smallest possible image. Multi-arch images support both `linux/amd64` and `linux/arm64`.

Tags follow the suite model:

```
# Production
ephpm:0.1.0-php8.4-wordpress          # wordpress suite (default)
ephpm:0.1.0-php8.4-laravel            # laravel suite
ephpm:0.1.0-php8.4-full               # all extensions
ephpm:0.1.0-php8.4-core               # minimal core
ephpm:latest                           # latest PHP + wordpress suite

# Development (adds xdebug, pcov, spx)
ephpm:0.1.0-php8.4-wordpress-dev
ephpm:0.1.0-php8.4-laravel-dev
ephpm:0.1.0-php8.4-full-dev
```

### Builder Images

Container images for `ephpm ext build` (custom binary builds):

```
ghcr.io/ephpm/builder:0.1.0-php8.4
ghcr.io/ephpm/builder:0.1.0-php8.3
```

These contain static-php-cli, Rust toolchain, ePHPm source, and all system library sources needed to compile extensions from source. Multi-arch. The build runs entirely inside the container — no compiler toolchain needed on the host.

---

## Implementation Order

### Step 1: Scaffold Repository

- Create Cargo workspace with virtual manifest
- Create crate directories with stub `Cargo.toml` + `lib.rs` / `main.rs`
- Add config files: `rust-toolchain.toml`, `rustfmt.toml`, `deny.toml`
- Add `.github/workflows/ci.yml`
- Add example `ephpm.toml`
- Verify `cargo check --workspace` passes

### Step 2: Implement `ephpm-config`

- Config structs with serde derive
- Figment-based TOML loading with env var override
- CLI arg parsing with clap (derive mode)
- Unit tests for config loading + defaults

### Step 3: Implement `ephpm-php`

This is the hardest crate and the core of the project.

- `build.rs` with bindgen for PHP headers + static linking
- Define `PhpRuntime` struct wrapping PHP lifecycle:
  - `PhpRuntime::init()` → `php_embed_init()` + register custom SAPI module
  - `PhpRuntime::shutdown()` → `php_embed_shutdown()`
  - `PhpRuntime::execute_request(request) → response`
- Implement all SAPI callbacks:
  - `ub_write` → append to response body buffer
  - `send_headers` / `send_header` → capture status + headers
  - `read_post` → provide POST body from request
  - `read_cookies` → return Cookie header value
  - `register_server_variables` → populate `$_SERVER` from request
  - `log_message` → route to `tracing`
- Request/response mapping:
  - `HttpRequest` (from hyper) → PHP SAPI request info
  - PHP output → `HttpResponse` (status, headers, body)
- Safety: `Mutex<PhpRuntime>` for NTS mode

### Step 4: Implement `ephpm-server`

- hyper HTTP server with tokio
- Router logic:
  1. If request path maps to an existing file → serve static file
  2. If request path maps to a `.php` file → execute via `ephpm-php`
  3. Otherwise → try `index.php` (WordPress-style URL rewriting)
- Static file serving with `mime_guess` for Content-Type
- `spawn_blocking` bridge from async hyper to sync PHP execution
- Graceful shutdown (drain connections on SIGINT/SIGTERM)

### Step 5: Wire Up `ephpm` Binary

- CLI with clap:
  ```
  ephpm --config ephpm.toml
  ephpm --help
  ephpm --version
  ```
- Load config → initialize tracing → boot PhpRuntime → start HTTP server
- Startup banner with version, listen address, document root, PHP version
- Graceful shutdown on SIGINT/SIGTERM

### Step 6: WordPress Integration Test

- Document setup steps:
  1. Download WordPress 6.x
  2. Set up MySQL database (external, e.g., Docker)
  3. Configure `ephpm.toml` with `document_root` pointing to WordPress
  4. Run `ephpm`
  5. Complete WordPress installation wizard
- Verify all success criteria pass
- Write integration test that automates the smoke test

---

## Future Milestones (Post-MVP)

| Milestone | Key Features | Status |
|-----------|-------------|--------|
| **v0.2: ZTS + Workers** | Thread-safe PHP, multiple concurrent requests, worker pool | Planned |
| **v0.3: TLS** | Automatic HTTPS via `rustls-acme`, Let's Encrypt | Planned |
| **v0.4: DB Proxy** | **Implemented (partial)**: MySQL transparent proxy, connection pooling, reset strategy; **Missing**: read/write splitting, replication, slow query analysis | Ahead of schedule |
| **v0.5: KV Store** | **Implemented (partial)**: Single-node RESP2 server, ~30 Redis commands, TTL/expiry, memory tracking, SAPI bridge for direct PHP access; **Missing**: data structures (hashes/lists/sets), clustering, persistence, eviction policies | Ahead of schedule |
| **v0.6: Admin UI** | Embedded web dashboard, request inspector | Planned |
| **v0.7: Observability** | OTLP receiver, auto-instrumentation, Prometheus `/metrics` | Planned |
| **v0.8: Clustering** | Gossip discovery, hash ring, KV replication | Planned |
| **v1.0: Production** | Read/write splitting, replication lag awareness, hardening | Planned |
