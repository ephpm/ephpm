+++
title = "Credits"
type = "docs"
weight = 11
+++

ePHPm stands on the shoulders of some excellent open-source projects. Everything below is either linked into the binary or part of the toolchain that builds it.

## ePHPm ecosystem

- [litewire](https://github.com/ephpm/litewire) — MySQL/PostgreSQL/Hrana/TDS wire protocol proxy that translates queries to SQLite. This is what lets PHP applications talk `pdo_mysql` to an embedded database with zero config changes.
- [ephemerd](https://github.com/luthermonson/ephemerd) — self-hosted GitHub Actions runner manager. ePHPm borrows its self-installing binary pattern (`ephpm install` / `ephpm uninstall`).

## Rust crates

- [tokio](https://github.com/tokio-rs/tokio) — async runtime powering the HTTP server, KV store, cluster protocol, and every background task
- [hyper](https://github.com/hyperium/hyper) — low-level HTTP/1.1 and HTTP/2 implementation behind the request router
- [rustls](https://github.com/rustls/rustls) — TLS library for manual cert loading and automatic ACME (Let's Encrypt)
- [Turso Database](https://github.com/tursodatabase/turso) — the pure-Rust SQLite rewrite used by litewire as the embedded database engine (single-node and clustered)
- [chitchat](https://github.com/quickwit-oss/chitchat) — SWIM gossip protocol library for cluster membership, failure detection, and KV replication
- [dashmap](https://github.com/xacrimon/dashmap) — concurrent hashmap backing the in-process KV store
- [figment](https://github.com/SergioBenitez/Figment) — layered configuration (TOML files + `EPHPM_` environment variable overrides)
- [clap](https://github.com/clap-rs/clap) — CLI argument parsing
- [tracing](https://github.com/tokio-rs/tracing) — structured logging and diagnostics
- [metrics](https://github.com/metrics-rs/metrics) + [metrics-exporter-prometheus](https://github.com/metrics-rs/metrics) — Prometheus-compatible metrics export

## Embedded at build time

- [PHP](https://www.php.net/) — embedded via FFI as a statically linked library (ZTS on every platform, Windows included)
- [static-php-cli](https://github.com/crazywhalecc/static-php-cli) — builds PHP and its extensions as a single static archive that gets linked into the ephpm binary
