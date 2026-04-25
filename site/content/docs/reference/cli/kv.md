+++
title = "ephpm kv"
weight = 3
+++

> **Stub** — not yet written.

## What this will cover

Connects to a running ePHPm KV server (default `127.0.0.1:6379`) and runs commands.

- `ephpm kv ping` — check the connection
- `ephpm kv keys [PATTERN]` — list keys (default `*`)
- `ephpm kv get <KEY>`
- `ephpm kv set <KEY> <VALUE> [--ttl SECS]`
- `ephpm kv del <KEY> [<KEY>...]`
- `ephpm kv incr <KEY> [--by N]`
- `ephpm kv ttl <KEY>` — show TTL info

Plus `--host` and `--port` overrides.
