+++
title = "CLI"
type = "docs"
weight = 1
+++

Single binary, all commands. Built with `clap` (Rust).

```
ephpm [command] [flags]
```

Running plain `ephpm` with no subcommand starts the [local development server](#ephpm-dev) — bound to `127.0.0.1`, serving the current directory, no config file required. Production deployments use `ephpm serve --config` or the [service lifecycle commands](#service-lifecycle).

---

## `ephpm serve`

Start the PHP application server in production mode (binds `0.0.0.0`, expects an `ephpm.toml`). See the [full `serve` reference](serve/).

```bash
# Start with config file (default: ./ephpm.toml)
ephpm serve

# Explicit config path
ephpm serve --config /etc/ephpm/ephpm.toml

# Override listen address
ephpm serve --listen 0.0.0.0:443

# Override document root
ephpm serve --document-root /var/www/html/public

# Increase log verbosity (-v = debug, -vv = trace)
ephpm serve -v
```

| Flag | Long | Default | Purpose |
|------|------|---------|---------|
| `-c` | `--config <FILE>` | `ephpm.toml` | Path to the TOML config file |
| `-l` | `--listen <ADDR>` | from config | Override `[server] listen` |
| `-d` | `--document-root <DIR>` | from config | Override `[server] document_root` |
| `-v` | `--verbose` | off | Increase log verbosity (repeatable) |

**Graceful shutdown:** `SIGTERM` or `SIGINT` drains in-flight requests and exits cleanly. On Windows, `Ctrl+C` and `Ctrl+Break` trigger graceful shutdown via `SetConsoleCtrlHandler`. There is no config hot-reload — restart the process to pick up config changes (`ephpm restart` when running as a service).

Configuration can also be overridden with environment variables using the `EPHPM_` prefix and double underscores between nesting levels:

```bash
EPHPM_SERVER__LISTEN=0.0.0.0:9090 ephpm serve
EPHPM_SERVER__DOCUMENT_ROOT=/var/www/html ephpm serve
EPHPM_PHP__MEMORY_LIMIT=256M ephpm serve
```

---

## `ephpm dev`

Local development server. Binds `127.0.0.1`, serves the current working directory, and auto-picks the next free port if the preferred one is busy. This is also what plain `ephpm` (no subcommand) runs.

```bash
# Serve the current directory on 127.0.0.1:8080 (or next free port)
ephpm dev
ephpm            # same thing

# Preferred port
ephpm dev --port 3000

# Explicit document root
ephpm dev --document-root ./public

# *.localhost vhosting — each subdirectory of ./sites becomes
# http://<name>.localhost:<port> (no /etc/hosts edit needed)
ephpm dev --sites ./sites
```

| Flag | Long | Default | Purpose |
|------|------|---------|---------|
| `-p` | `--port <PORT>` | `8080` | Preferred port — if busy, the next free port is picked |
| `-d` | `--document-root <DIR>` | CWD | Directory to serve |
| `-s` | `--sites <DIR>` | — | Sites directory for `*.localhost` vhosting |
| `-l` | `--listen <ADDR>` | `127.0.0.1:<port>` | Override the listen address entirely |
| `-v` | `--verbose` | off | Increase log verbosity (repeatable) |

---

## `ephpm php`

Run PHP CLI commands using the embedded PHP runtime. This is a **pure passthrough** — every argument after `php` goes straight to PHP's own argument parser, so all standard PHP CLI flags work. See the [full `php` reference](php/).

```bash
# Version
ephpm php -v

# Run code inline (use -- so your shell/clap don't eat the flags)
ephpm php -- -r 'echo 1;'

# Execute a file
ephpm php script.php

# PHP info / loaded modules
ephpm php -i
ephpm php -m

# Syntax check (lint)
ephpm php -l src/Controller.php

# INI overrides
ephpm php -d memory_limit=256M -r "echo ini_get('memory_limit');"

# Framework CLIs work as-is
ephpm php artisan migrate

# Exit codes propagate correctly
ephpm php -r "exit(42);"; echo $?   # → 42
```

**Implementation notes:**
- The `ephpm php` subcommand disables clap's help flag so `-h` passes through to PHP instead of being intercepted.
- Backed by `ephpm_cli_main()` in `crates/ephpm-php/ephpm_wrapper.c`, which uses PHP's own `php_getopt` with a copy of the CLI SAPI's option table.
- Output goes directly to stdout/stderr — same PHP version, same extensions, same INI overrides as the server.

---

## `ephpm kv`

Inspect and manipulate the KV store on a running server. Connects directly to the embedded KV server over the RESP2 protocol (requires `[kv.redis_compat] enabled = true`). See the [full `kv` reference](kv/).

```bash
ephpm kv [--host <HOST>] [--port <PORT>] <COMMAND>

Options:
  --host <HOST>    KV server host [default: 127.0.0.1]
  --port <PORT>    KV server port [default: 6379]
```

| Command | Purpose |
|---------|---------|
| `ephpm kv keys [PATTERN]` | List keys matching a pattern (default `*`) |
| `ephpm kv get KEY` | Get the value of a key |
| `ephpm kv set KEY VALUE [--ttl SECS]` | Set a key, optionally with a TTL |
| `ephpm kv del KEY [KEY...]` | Delete one or more keys |
| `ephpm kv incr KEY [--by N]` | Increment a counter key |
| `ephpm kv ttl KEY` | Show TTL information for a key |
| `ephpm kv ping` | Check the connection |

```bash
$ ephpm kv ping
PONG

$ ephpm kv set greeting "hello world"
OK

$ ephpm kv get greeting
hello world

$ ephpm kv incr counter --by 5
(integer) 5

$ ephpm kv set temp value --ttl 60
OK

$ ephpm kv ttl temp
expires in 59s (59986ms)

$ ephpm kv keys "*"
1) greeting
2) counter
3) temp

$ ephpm kv del counter temp
(integer) 2
```

---

## Service Lifecycle

Install and manage ePHPm as a system service — systemd on Linux, launchd on macOS, SCM on Windows.

### `ephpm install`

Install ePHPm as a system service and start it.

```bash
sudo ephpm install
```

### `ephpm uninstall`

Uninstall the system service.

```bash
sudo ephpm uninstall

# Keep the configuration file and data directory in place
sudo ephpm uninstall --keep-data
```

### `ephpm start` / `ephpm stop` / `ephpm restart`

Control the installed service.

```bash
sudo ephpm start
sudo ephpm stop
sudo ephpm restart      # e.g. after editing the config file
```

### `ephpm status`

Show service status (PID, uptime, listen address).

```bash
ephpm status
```

### `ephpm logs`

Tail the service log file.

```bash
ephpm logs

# Follow the log (like tail -f)
ephpm logs -f
```

---

## Command Summary

```
ephpm                Local dev server (same as `ephpm dev`)
ephpm serve          Start the production server (--config/--listen/--document-root/-v)
ephpm dev            Development server (--port/--document-root/--sites)
ephpm php            Run the embedded PHP CLI (pure passthrough)
ephpm kv             KV store client (keys/get/set/del/incr/ttl/ping)

ephpm install        Install + start the system service
ephpm uninstall      Remove the system service (--keep-data)
ephpm start          Start the installed service
ephpm stop           Stop the installed service
ephpm restart        Restart the installed service
ephpm status         Service status (PID, uptime, listen address)
ephpm logs           Tail the service log (-f to follow)
```

`ephpm --version` prints the ePHPm version. For everything else — configuration, metrics, cluster state — see [Reference → Configuration](/reference/config/) and the `/metrics` endpoint.
