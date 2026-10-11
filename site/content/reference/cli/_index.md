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

# Step debugging: fetch (first run) and load an ABI-matched Xdebug,
# then debug any request with ?XDEBUG_TRIGGER=1 (Linux)
ephpm dev --xdebug

# ... with your own Xdebug build (no network), IDE on another host
ephpm dev --xdebug-path ~/xdebug.so --xdebug-client-host 172.20.0.1
```

| Flag | Long | Default | Purpose |
|------|------|---------|---------|
| `-p` | `--port <PORT>` | `8080` | Preferred port — if busy, the next free port is picked |
| `-d` | `--document-root <DIR>` | CWD | Directory to serve |
| `-s` | `--sites <DIR>` | — | Sites directory for `*.localhost` vhosting |
| `-l` | `--listen <ADDR>` | `127.0.0.1:<port>` | Override the listen address entirely |
| `-v` | `--verbose` | off | Increase log verbosity (repeatable) |
| | `--xdebug` | off | Load Xdebug for step debugging. Linux (glibc, x86_64/aarch64) and macOS (arm64): an ABI-matched ZTS build is downloaded from the ePHPm extension catalog on first use, verified (API number, ZTS, OS, libc, arch, sha256), and cached under the user cache dir; later runs work offline. Refused on Windows (static PHP cannot load extension DLLs). macOS catalog tarballs are still being added; until one exists for your PHP minor, use `--xdebug-path`. See [Debugging with Xdebug](/guides/xdebug/) |
| | `--xdebug-path <FILE>` | — | Load this local Xdebug shared object instead (implies `--xdebug`, no network). Must be a ZTS build for this binary's PHP minor |
| | `--xdebug-client-host <HOST>` | `127.0.0.1` | `xdebug.client_host` — where the IDE listens (from WSL with the IDE on Windows: the Windows host IP). Requires `--xdebug`/`--xdebug-path` |
| | `--xdebug-client-port <PORT>` | `9003` | `xdebug.client_port`. Requires `--xdebug`/`--xdebug-path` |

With `--xdebug`, the dev server also sets `xdebug.mode=debug,develop` and `xdebug.start_with_request=trigger`, disables the request deadline (`[server.timeouts] request = 0`) and — where PHP enforces it natively — `max_execution_time`, so a request paused on a breakpoint is not cut off. Startup fails with a clear error if Xdebug does not register as a Zend extension. `EPHPM_EXT_CATALOG_URL` (download base, laid out as `<base>/<tag>/<asset>`) and `EPHPM_EXT_CATALOG_API` (a GitHub-releases-shaped listing) point the download at a mirror; both must be `https://` unless they point at a loopback address.

**Configuration in dev mode:** `ephpm dev` never reads an `ephpm.toml`, not even one in the current directory. It starts from the built-in defaults and **does** apply `EPHPM_*` environment overrides, except for the listen address and document root, which `dev` always sets itself — from the flags above, or from their defaults — so `EPHPM_SERVER__LISTEN` and `EPHPM_SERVER__DOCUMENT_ROOT` have no effect. `--sites` likewise overrides the sites directory. So a dev server can still turn features on through the environment, for example `EPHPM_KV__REDIS_COMPAT__ENABLED=true ephpm dev` to enable the RESP listener for [`ephpm kv`](kv/). For anything more, use `ephpm serve --config`.

**Port selection:** without `--listen`, `dev` tries `--port` and then each following port, up to 50 in total (`8080`–`8129` by default). If all of them are busy it exits with `no free port in range ...`.

---

## `ephpm php`

Run PHP CLI commands using the embedded PHP runtime. Apart from two ePHPm options, every argument after `php` goes straight to PHP's own argument parser, so all standard PHP CLI flags work. See the [full `php` reference](php/).

The two ePHPm options are `--site <key>` and `--config <path>`. When they come **first**, before the PHP program and its arguments, ePHPm consumes them and binds the [`ephpm_db_*` bridge](/guides/db-from-php/) to that virtual host's database (`--config` is required with `--site`). They are `--long`-only, so they never collide with php-cli's own `-c`. Anywhere after the program they pass through to PHP untouched. `--config` without `--site` has no effect: it prints a warning and is ignored. See [Binding a virtual host's database](php/#binding-a-virtual-hosts-database---site).

```bash
ephpm php --site shop --config /etc/ephpm/ephpm.toml -- vendor/bin/wp option get siteurl
```

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

## `ephpm composer`

Install dependencies with the embedded [vivacity](https://github.com/Adelagric/vivacity) installer (pinned to 0.17.0), a Rust reimplementation of Composer's `install`, `update`, `require`, `remove` and `dump-autoload` that is compiled into the binary. Every argument after `composer` is forwarded verbatim, and the process exits with vivacity's exit code. It is **not** full Composer. No other command exists. Scripts do not run unless you pass `--run-scripts`, which needs a real `composer`. Some common Composer flags (`--no-interaction`, `--prefer-dist`, `-q`) are usage errors on `install`/`update`. `update`/`require`/`remove` need a `php` executable on `PATH` for the platform probe. Locks it cannot install natively are handed to a real `composer` on `PATH`, and without one it exits `3`. Read the [full `composer` reference](composer/) before relying on it.

```bash
# Install locked dependencies (drop-in `composer install`)
ephpm composer install --no-dev --optimize-autoloader

# Add a package
ephpm composer require monolog/monolog

# Regenerate the autoloader
ephpm composer dump-autoload -o

# Version of the embedded installer
ephpm composer --version   # → vivacity <x.y.z>
```

**Implementation notes:**
- Nothing here links PHP, so the subcommand exists in every build, including stub mode and Windows. `install` and `dump-autoload` of an in-scope lock need neither PHP nor Composer. `update`/`require`/`remove` run an external `php` (`VIVACITY_PHP` or `php` on `PATH`, never the embedded one) to probe platform requirements. The out-of-scope fallback and `--run-scripts` shell out to a real `composer` on `PATH`.
- The subcommand disables clap's help flag and captures all trailing arguments verbatim (`trailing_var_arg` + `allow_hyphen_values`); a synthetic `composer` program-name element is prepended before dispatch to `vivacity::run`.

---

## `ephpm kv`

Inspect and manipulate the KV store on a running server. Connects directly to the embedded KV server over the RESP2 protocol (requires `[kv.redis_compat] enabled = true`). See the [full `kv` reference](kv/).

```bash
ephpm kv [--host <HOST>] [--port <PORT>] [--password <PW>] [--user <NAME>] <COMMAND>

Options:
  --host <HOST>      KV server IP address [default: 127.0.0.1]
  --port <PORT>      KV server port [default: 6379]
  --password <PW>    Sent as RESP AUTH before the first command
                     [default: $EPHPM_KV_PASSWORD]
  --user <NAME>      First argument of AUTH <user> <password> (per-site HMAC
                     auth: the vhost hostname). Requires --password.
```

These options belong to `ephpm kv` itself, so they go **before** the subcommand. Pass `--password` (or set `EPHPM_KV_PASSWORD`) whenever the server sets `[kv.redis_compat] password` or `[kv] secret`. Without it every command fails with `NOAUTH`. `--host` must be an IP literal (`10.0.1.5`, or `[::1]` for IPv6). A hostname such as `localhost` fails with `invalid address`. See [Authentication](kv/#authentication).

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

## `ephpm analyze`

Statically analyze a PHP application and gate on the result — external security tools (composer audit, semgrep, yara) wrapped as subprocesses plus native passes, merged under a fail-closed allow/quarantine/deny policy. Supports baselines (gate only on new findings), diff-aware scans (`--since <git-ref>`), strictness levels (`--level 0..3`), an incremental result cache, and operator-only suppressions (inline suppression comments in the scanned code are never honored — they are flagged instead). The exit code reflects the verdict, so it works directly as a CI or deploy-gate step. See the [full `analyze` reference](analyze/).

```bash
# Gate the current directory (security profile, fail on quarantine)
ephpm analyze

# Report-only over a checkout, as SARIF
ephpm analyze /srv/app --fail-on allow --format sarif

# Fast per-push gate: only files changed vs main
ephpm analyze /srv/app --since origin/main
```

---

## `ephpm deploy`

Invalidate the OPcache for one vhost (`--site`) or every vhost (`--all`) across a cluster. It writes `opcache:version:<vhost>` through the running server's RESP listener, and gossip replicates the write to every node. Needs `[kv.redis_compat] enabled = true` on the server. See the [full `deploy` reference](deploy/).

```bash
ephpm deploy --site blog --rev a8f13d2
ephpm deploy --all
```

| Flag | Default | Purpose |
|------|---------|---------|
| `--site <NAME>` | — | Vhost to invalidate. Mutually exclusive with `--all`. Neither flag means the default vhost. |
| `--all` | off | Invalidate every vhost via the broadcast key |
| `--rev <REV>` | — | Revision tag recorded at `opcache:revision:<vhost>` |
| `--host <HOST>` | `127.0.0.1` | RESP server host (hostnames are resolved) |
| `--port <PORT>` | `6379` | RESP server port |
| `--password <PW>` | `$EPHPM_KV_PASSWORD` | Sent as RESP `AUTH` |

---

## `ephpm cache`

Cache management. The only subcommand is `reset`, which performs the same OPcache invalidation as `deploy` without the `--rev` stamp. Same requirements as `deploy`. See the [full `cache` reference](cache/).

```bash
ephpm cache reset --site blog
ephpm cache reset --all
ephpm cache --host 10.0.1.5 --password "$PW" reset --all   # options go before `reset`
```

`--host` (default `127.0.0.1`), `--port` (default `6379`), and `--password` (default `$EPHPM_KV_PASSWORD`) are options of `ephpm cache`. `--site` / `--all` belong to `reset`.

---

## `ephpm exec`

Run a command inside a virtual host's tenant sandbox: Landlock filesystem scope plus an irreversible drop to the tenant uid. **Linux only.** It must start as root, and it needs multi-tenant mode (`[server] sites_dir`). See the [full `exec` reference](exec/), including what the sandbox does and does not cover.

```bash
sudo ephpm exec --site shop -- composer install --no-dev
sudo ephpm exec --site shop --timeout 600 -- bash -c './build.sh'
```

| Flag | Long | Default | Purpose |
|------|------|---------|---------|
| `-c` | `--config <FILE>` | `/etc/ephpm/ephpm.toml` | Config holding `[server] sites_dir` |
| `-s` | `--site <KEY>` | (required) | Vhost directory name under `sites_dir` |
| `-t` | `--timeout <SECS>` | `0` (none) | Wall-clock limit, enforced by `SIGALRM` |
| | `--no-sandbox` | off | **Removes all containment**: no Landlock, no uid drop. The command runs with the caller's privileges |

The command goes after `--`. `ephpm exec --help` does not print help, because the subcommand captures its arguments verbatim. Use `ephpm help exec`.

---

## Service Lifecycle

Install and manage ePHPm as a system service — systemd on Linux, launchd on macOS, SCM on Windows. Every lifecycle command except `status` and `logs` needs root (`sudo`) or an Administrator shell. Without it, it exits `1` with `requires root privileges`.

### Where things live

| | Linux | macOS | Windows |
|---|---|---|---|
| Binary | `/usr/local/bin/ephpm` | `/usr/local/bin/ephpm` | `C:\Program Files\ephpm\ephpm.exe` |
| Config | `/etc/ephpm/ephpm.toml` | `/etc/ephpm/ephpm.toml` | `C:\ProgramData\ephpm\ephpm.toml` |
| Service | `/etc/systemd/system/ephpm.service` | `/Library/LaunchDaemons/dev.ephpm.plist` | Windows service `ephpm` |
| Data dir | `/var/lib/ephpm/` | `/var/lib/ephpm/` | `C:\ProgramData\ephpm\data\` |
| Log file | `/var/log/ephpm/ephpm.log` | `/var/log/ephpm/ephpm.log` | `C:\ProgramData\ephpm\logs\ephpm.log` |
| Default document root (in the generated config) | `/var/www/html` | `/var/www/html` | `C:\ProgramData\ephpm\www` |

The data dir is created by `install` and deleted by `uninstall`, but **the generated config does not point anything at it**. `[db.sqlite] path` defaults to `ephpm.db` and the ACME `cache_dir` defaults to `certs`. Both are relative to the service's working directory. With `install --user` on Linux/macOS that working directory is the data dir, so they land there. Without `--user` it is `/` (systemd/launchd) or `C:\Windows\System32` (a Windows service), not the data dir. To keep databases and certificates in the data dir in every case, set absolute paths in the config (for example `path = "/var/lib/ephpm/app.db"`).

### `ephpm install`

Install ePHPm as a system service and start it.

```bash
sudo ephpm install

# Run the service as a dedicated non-root account (Linux/macOS)
sudo ephpm install --user ephpm-web
sudo ephpm install --user ephpm-web --group www-data
```

What it does, in order:

1. Copies the **running** binary to the install path. If you run the installed binary itself, the copy is skipped.
2. Writes a default config **only if none exists**. Re-running `install`, for example with a newer binary, keeps your config, so re-running is the upgrade path. The generated config listens on `0.0.0.0:8080`, serves the platform's default document root, and uses `[php] mode = "per_request"`, `max_execution_time = 30`, `memory_limit = "128M"`.
3. Creates the data and log directories. With `--user`, it also creates the directories the config writes to (see below) and hands them to the service account.
4. Registers the service: a systemd unit (enabled, `Restart=on-failure`, output appended to the log file), a launchd daemon (`RunAtLoad`, `KeepAlive`), or a Windows auto-start service running as `LocalSystem`. On Windows it also adds `C:\Program Files\ephpm` to the system `Path`. The Windows service runs the hidden `ephpm service-run` entry point, while systemd and launchd run `ephpm serve --config <config>`.
5. Starts the service.

`install` does not create the document root.

| Flag | Default | Purpose |
|------|---------|---------|
| `--user <USER>` | — (service runs as root / `LocalSystem`) | Run the service as this existing account. It also `chown -R`s the data and log directories to it. |
| `--group <GROUP>` | same name as `--user` | Group to run the service as. Only valid together with `--user`. Without `--user` it is a usage error. |

What `--user` changes per platform:

- **Linux and macOS:** the service's working directory is the data dir (`WorkingDirectory=` in the unit, the `WorkingDirectory` plist key), so relative paths in the config — the default `[db.sqlite] path = "ephpm.db"` and the default ACME `cache_dir = "certs"` — land in the directory the account owns. Without `--user` the working directory stays `/` (unchanged, so an existing relative database does not move). The installer also reads the config and creates the absolute `[db.sqlite] path` parent directory, `[db.sqlite] dir` and ACME `[server.tls] cache_dir`. A directory it creates is chowned to the account (recursively for the per-site `dir` and the ACME cache, non-recursively for a database file's parent). A directory that **already existed** outside the data dir is not re-owned, because it may be a shared directory such as `/var/lib`: the installer logs a warning naming it, and granting access is up to you. It reads the config that exists at install time; if you later move the database, re-run `ephpm install --user …` (it keeps your config) or grant access by hand.
- **Linux:** the unit also gets `User=` / `Group=` plus a hardening profile: `NoNewPrivileges=true`, `ProtectSystem=strict`, `ProtectHome=true`, `AmbientCapabilities=` and `CapabilityBoundingSet=CAP_NET_BIND_SERVICE` (so the service can still bind ports 80/443), and `ReadWritePaths=` for the data dir, log dir, default document root, `/tmp`, `/var/tmp`, and the config-derived directories above. `ProtectSystem=strict` would otherwise leave `/tmp` read-only too, and that is where PHP writes uploads and file sessions by default and where multi-tenant mode keeps each site's private temp/session state (`/tmp/ephpm-vhosts/…`). Everything else stays read-only, so any other path the server must write to — a `sites_dir` outside the document root, an access-log file elsewhere — has to be added to `ReadWritePaths` in a drop-in.
- **macOS:** the plist gets `UserName` / `GroupName`. No other hardening is added.
- **Windows:** not supported. Passing `--user` is a hard error, because the SCM needs a stored account password that ePHPm has no secure way to collect. It does not quietly install as `LocalSystem`.

Without `--user` the service runs as root (`LocalSystem` on Windows) with no sandboxing. That is the historical default.

The account must already exist. `install` does not create users.

### `ephpm uninstall`

Stop and remove the system service.

```bash
sudo ephpm uninstall

# Keep the configuration file, data directory, and logs in place
sudo ephpm uninstall --keep-data
```

> **Without `--keep-data`, `uninstall` deletes data.** It removes the config file (`/etc/ephpm/ephpm.toml`, or `C:\ProgramData\ephpm\ephpm.toml`), the **whole data directory** (`/var/lib/ephpm/` or `C:\ProgramData\ephpm\data\`) including any databases or certificates you put there, and the **whole log directory**. It then removes the config directory if it is empty. Back up anything you need first, or pass `--keep-data`.

Either way it stops the service (if it can't, it logs a warning and carries on), deregisters it (and removes the install dir from the Windows `Path`), and deletes the installed binary. It never touches the document root or a `sites_dir`.

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

It prints the service state, PID, uptime, the `[server] listen` value read from the installed config file, and the config path. Uptime is reported on Linux only, as the systemd start timestamp. macOS and Windows show `-`. The listen address comes from the file only, so an `EPHPM_SERVER__LISTEN` override is not reflected.

### `ephpm logs`

Print the last 200 lines of the service log file (`/var/log/ephpm/ephpm.log` on Linux/macOS, `C:\ProgramData\ephpm\logs\ephpm.log` on Windows).

```bash
ephpm logs

# Follow the log (like tail -f)
ephpm logs -f
```

- **Every platform:** prints the last 200 lines of the log file. With `-f` it then polls for new lines every 500 ms and starts over from the beginning if the file shrinks (rotation). On Linux and macOS only the last 1 MiB of the file is read to find those lines, and bytes that are not valid UTF-8 are shown as `�` instead of failing the command. On Windows the whole file is read, and a file that is not valid UTF-8 makes `logs` fail.
- **Linux:** the generated unit appends the server's stdout/stderr to `/var/log/ephpm/ephpm.log` (`StandardOutput=append:`), not to the journal, so the journal holds only systemd's own start/stop messages. `logs` therefore reads the file, and runs `journalctl -u ephpm.service --no-pager -n 200` (or `-f`) only when the file does not exist, for example under a hand-written unit that logs to the journal. If that `journalctl` fails too, `logs` exits `1`.

The log file is at the path in [Where things live](#where-things-live). On macOS and Windows a missing log file makes `logs` exit `1`.

---

## Exit codes

| Command | Exit code |
|---------|-----------|
| `serve`, `dev` | `0` on clean shutdown, non-zero on a startup or fatal error. See [`serve`](serve/#exit-codes). |
| `php` | PHP's own exit status. See [`php`](php/#errors-and-exit-codes). |
| `composer` | The exit code of the embedded installer. See [`composer`](composer/). |
| `analyze` | `0` pass, `2` quarantine, `3` deny, `1` internal error. See [`analyze`](analyze/). |
| `exec` | The command's own exit status once it has started, `1` if setup fails. See [`exec`](exec/#exit-status). |
| `kv` | `0` on success. Server replies such as `(nil)`, `(empty)` or `key does not exist` count as success. `1` if the server answers with an error (`-ERR`, `-NOAUTH`, …), which is printed as `error: ...` on stderr. |
| `deploy`, `cache reset` | `0` on success, `1` if the RESP listener is unreachable or returns an error |
| `install`, `uninstall`, `start`, `stop`, `restart`, `status`, `logs` | `0` on success, `1` on any failure (`error: service command failed: ...` on stderr) |

Any other failure, such as a connection error, an unreadable config, or an invalid argument value, prints `error: ...` on stderr and exits `1`. A clap usage error (unknown flag, missing required argument) exits `2`.

## Logging for CLI commands

`analyze`, `exec`, `php --site`, and the service lifecycle commands log diagnostics to **stderr**. stdout is reserved for the command's output (for example `analyze --format sarif`). The level comes from `RUST_LOG` and defaults to `info`. `serve` and `dev` use `RUST_LOG`, then `-v`, then `[server.logging] level`. See [Environment variables](/reference/environment-variables/).

---

## Command Summary

```
ephpm                Local dev server (same as `ephpm dev`)
ephpm serve          Start the production server (--config/--listen/--document-root/-v)
ephpm dev            Development server (--port/--document-root/--sites/--xdebug)
ephpm php            Run the embedded PHP CLI (pure passthrough)
ephpm composer       Embedded vivacity installer: install/update/require/remove/dump-autoload
ephpm kv             KV store client (keys/get/set/del/incr/ttl/ping)
ephpm analyze        Static analysis + fail-closed deploy gate (--format/--profile/--fail-on/
                     --level/--since/--baseline/--cache-dir/--no-cache)
ephpm deploy         Cluster-wide OPcache invalidation (--site/--all/--rev)
ephpm cache reset    OPcache reset for one vhost or all (--site/--all)
ephpm exec           Run a command in a vhost's tenant sandbox (Linux only;
                     --site/--config/--timeout/--no-sandbox)

ephpm install        Install + start the system service (--user/--group)
ephpm uninstall      Remove the service AND delete config/data/logs (--keep-data keeps them)
ephpm start          Start the installed service
ephpm stop           Stop the installed service
ephpm restart        Restart the installed service
ephpm status         Service status (PID, uptime, listen address)
ephpm logs           Tail the service log (-f to follow)
ephpm help <cmd>     Help for a subcommand (the only way to get help for exec)
```

Plain `ephpm php` / `ephpm composer` / `ephpm exec` don't accept `--help`. `ephpm php` passes `-h` to PHP, and `ephpm composer --help` goes to the embedded installer. Use `ephpm help <cmd>` for ePHPm's own help. On Windows there is also a hidden `ephpm service-run` subcommand. The Windows service manager invokes it to run the installed service, and it is not meant to be run by hand.

`ephpm --version` prints the ePHPm version. For everything else — configuration, metrics, cluster state — see [Reference → Configuration](/reference/config/) and the `/metrics` endpoint.
