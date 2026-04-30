+++
title = "ephpm serve"
weight = 1
+++

Start the PHP application server. This is the default command — `ephpm` with no subcommand is `ephpm serve`.

## Synopsis

```bash
ephpm serve [-c FILE] [-l ADDR] [-d DIR] [-v...]
ephpm                                              # equivalent
```

## Flags

| Flag | Long | Default | Purpose |
|------|------|---------|---------|
| `-c` | `--config <FILE>` | `ephpm.toml` | Path to the TOML config file. Missing files fall back to defaults. |
| `-l` | `--listen <ADDR>` | from config | Override `[server] listen`. Format `HOST:PORT`. |
| `-d` | `--document-root <DIR>` | from config | Override `[server] document_root`. |
| `-v` | `--verbose` | off | Increase log verbosity. `-v` = debug, `-vv` = trace. Lower-priority than `RUST_LOG`. |

## Examples

```bash
# Default config, default listen
ephpm

# Explicit config file
ephpm serve --config /etc/ephpm/ephpm.toml

# Quick override for a one-off serve
ephpm serve --listen 127.0.0.1:9090 --document-root ./public

# Trace logging for debugging
ephpm -vv

# Drop logs into a file
RUST_LOG=info ephpm serve 2>&1 | tee /var/log/ephpm.log
```

## What it starts

In one process:

- HTTP/1.1 + HTTP/2 listener on `[server] listen`
- PHP runtime (ZTS) ready to dispatch on tokio's `spawn_blocking` pool
- The MySQL proxy on `127.0.0.1:3306` if `[db.mysql]` or `[db.sqlite]` is configured
- The KV store on `127.0.0.1:6379` if `[kv.redis_compat] enabled = true`
- The `/metrics` endpoint if `[server.metrics] enabled = true`
- TLS listener (manual cert or ACME) if `[server.tls]` is set
- Gossip on UDP `7946` and KV data plane on TCP `7947` if `[cluster] enabled = true`

## Signals

- `SIGTERM` / `SIGINT` — graceful shutdown. New connections are rejected, in-flight requests run to completion up to `[server.timeouts] shutdown` (default 30s), then connections are force-closed.
- `SIGHUP` — reserved for future config reload (not yet implemented).

## Exit codes

- `0` — clean shutdown
- non-zero — startup failure or fatal runtime error (logged via `tracing`)

## See also

- [Configuration reference](/docs/reference/config/)
- [Environment variables](/docs/reference/environment-variables/)
