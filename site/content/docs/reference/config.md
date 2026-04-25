+++
title = "Configuration"
weight = 2
+++

> **Stub** — not yet written. The source of truth is `crates/ephpm-config/src/lib.rs`.

## What this will cover

Exhaustive reference for every key in `ephpm.toml`:

- `[server]` — listen address, document root, request timeout, max body size
- `[php]` — version, ini overrides, request limits
- `[db.mysql]` — proxy passthrough, pool size, R/W split
- `[db.sqlite]` — embedded SQLite path, replication settings
- `[db.analysis]` — query stats, slow query log, auto-explain
- `[kv]` — bind address, eviction, compression
- `[cluster]` — gossip seeds, node identity, replication factor
- `[tls]` — manual cert paths or ACME settings
- `[metrics]` — Prometheus endpoint
