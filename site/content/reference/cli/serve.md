+++
title = "ephpm serve"
weight = 1
+++

Start the PHP application server. The systemd and launchd services that `ephpm install` registers run `ephpm serve --config <config>`. The Windows service instead runs a hidden `ephpm service-run` entry point, which reads the config path the service was registered with and then runs the same server.

Note that `ephpm` with **no** subcommand is not `serve` — it starts the [development server](../#ephpm-dev) on port 8080. The subcommand is required for production use.

## Synopsis

```bash
ephpm serve [-c FILE] [-l ADDR] [-d DIR] [-v...]
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
ephpm serve

# Explicit config file
ephpm serve --config /etc/ephpm/ephpm.toml

# Quick override for a one-off serve
ephpm serve --listen 127.0.0.1:9090 --document-root ./public

# Trace logging for debugging (-v lives on the subcommand, not the bare binary)
ephpm serve -vv

# Drop logs into a file
RUST_LOG=info ephpm serve 2>&1 | tee /var/log/ephpm.log
```

## What it starts

In one process:

- HTTP/1.1 + HTTP/2 listener on `[server] listen`
- PHP runtime (ZTS) ready to dispatch on the dedicated execution pool
- The MySQL proxy on `127.0.0.1:3306` if `[db.mysql]` or `[db.sqlite]` is configured
- The KV store on `127.0.0.1:6379` if `[kv.redis_compat] enabled = true`
- The `/metrics` endpoint if `[server.metrics] enabled = true`, on the main listener or on `[server.metrics] listen` when set (multi-site nodes answer `403` until a token or mTLS is configured — see [access to internal endpoints](/reference/config/#access-to-internal-endpoints))
- TLS listener (manual cert or ACME) if `[server.tls]` is set
- Gossip on UDP `7946` and KV data plane on TCP `7947` if `[cluster] enabled = true`

## Signals

- `SIGTERM` / `SIGINT` — graceful shutdown. New connections are rejected, in-flight requests run to completion up to `[server.timeouts] shutdown` (default 30s), then connections are force-closed. With an embedded database, every open database then gets a final `wal_checkpoint(TRUNCATE)` that runs to completion **outside** that budget (it is what lets the next start skip WAL recovery), and the async runtime gets at most 15 s more to stop. So the process can outlive `[server.timeouts] shutdown`: give a supervisor that sends `SIGKILL` after a grace period (Kubernetes' `terminationGracePeriodSeconds`, default 30 s; systemd's `TimeoutStopSec`, default 90 s; the unit `ephpm service install` writes sets it to 180 s) room for the checkpoint too — see [Clustering setup → Kubernetes](/guides/clustering-setup/#kubernetes).
- `SIGHUP` — **not handled.** ePHPm installs no `SIGHUP` handler, so the default disposition applies and the process terminates immediately with **no graceful drain** — in-flight requests are dropped. Config reload is not implemented. Always use `SIGTERM` to stop the server.

## Exit codes

- `0` — clean shutdown
- non-zero — startup failure or fatal runtime error. Errors are logged via `tracing`, and a startup error is also printed as `error: ...` on stderr with exit `1`.

## See also

- [Configuration reference](/reference/config/)
- [Environment variables](/reference/environment-variables/)
