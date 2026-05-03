+++
title = "ePHPm"
toc = false
type = "docs"

[cascade]
  type = "docs"
+++

**Embedded PHP application server, written in Rust.**

Run PHP applications without the infrastructure. No PHP-FPM, no MySQL server, no Redis, no reverse proxy, no certbot. One binary, one config file. Drop in WordPress or Laravel and go.

When you need more, it's already built in: MySQL connection pooling, read/write splitting, a Redis-compatible KV store, clustered SQLite with automatic failover, TLS, and Prometheus metrics.

[Get Started →](/getting-started/)
[Architecture →](/architecture/)
[GitHub](https://github.com/ephpm/ephpm)

---

## Built with

ePHPm stands on the shoulders of some excellent open-source projects.

### ePHPm ecosystem

- [litewire](https://github.com/ephpm/litewire) — MySQL/PostgreSQL wire protocol proxy that translates queries to SQLite. This is what lets PHP applications talk `pdo_mysql` to an embedded SQLite database with zero config changes.
- [ephemerd](https://github.com/luthermonson/ephemerd) — self-hosted GitHub Actions runner manager. ePHPm borrows its self-installing binary pattern (`ephpm install` / `ephpm uninstall`).

### Rust crates

- [tokio](https://github.com/tokio-rs/tokio) — async runtime powering the HTTP server, KV store, cluster protocol, and every background task
- [hyper](https://github.com/hyperium/hyper) — low-level HTTP/1.1 and HTTP/2 implementation behind the request router
- [rustls](https://github.com/rustls/rustls) — TLS library for manual cert loading and automatic ACME (Let's Encrypt)
- [rusqlite](https://github.com/rusqlite/rusqlite) — SQLite bindings used by litewire for single-node embedded database mode
- [chitchat](https://github.com/quickwit-oss/chitchat) — SWIM gossip protocol library for cluster membership, failure detection, and KV replication
- [dashmap](https://github.com/xacrimon/dashmap) — concurrent hashmap backing the in-process KV store
- [figment](https://github.com/SergioBenitez/Figment) — layered configuration (TOML files + `EPHPM_` environment variable overrides)
- [clap](https://github.com/clap-rs/clap) — CLI argument parsing
- [tracing](https://github.com/tokio-rs/tracing) — structured logging and diagnostics
- [metrics](https://github.com/metrics-rs/metrics) + [metrics-exporter-prometheus](https://github.com/metrics-rs/metrics) — Prometheus-compatible metrics export

### Embedded at build time

- [PHP](https://www.php.net/) — embedded via FFI as a statically linked library (ZTS on Linux/macOS, NTS on Windows)
- [static-php-cli](https://github.com/crazywhalecc/static-php-cli) — builds PHP and its extensions as a single static archive that gets linked into the ephpm binary
- [sqld](https://github.com/tursodatabase/libsql) — Turso's libSQL server, embedded via `include_bytes!()` for clustered SQLite replication over gRPC
