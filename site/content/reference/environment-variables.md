+++
title = "Environment Variables"
weight = 3
+++

Every key in `ephpm.toml` can be overridden by an environment variable. The mapping is mechanical — `EPHPM_` prefix, double underscore (`__`) for nesting, uppercase the key name.

## The rule

```
[section] subsection.key  →  EPHPM_SECTION__SUBSECTION__KEY
```

Examples:

| TOML | Environment variable |
|------|----------------------|
| `[server] listen = "0.0.0.0:9090"` | `EPHPM_SERVER__LISTEN=0.0.0.0:9090` |
| `[server.metrics] enabled = true` | `EPHPM_SERVER__METRICS__ENABLED=true` |
| `[server.metrics] listen = "127.0.0.1:9100"` | `EPHPM_SERVER__METRICS__LISTEN=127.0.0.1:9100` |
| `[php] memory_limit = "256M"` | `EPHPM_PHP__MEMORY_LIMIT=256M` |
| `[db.sqlite] path = "/var/lib/app.db"` | `EPHPM_DB__SQLITE__PATH=/var/lib/app.db` |
| `[db.sqlite.replication] role = "primary"` | `EPHPM_DB__SQLITE__REPLICATION__ROLE=primary` |
| `[kv] compression = "zstd"` | `EPHPM_KV__COMPRESSION=zstd` |
| `[kv.redis_compat] enabled = true` | `EPHPM_KV__REDIS_COMPAT__ENABLED=true` |
| `[cluster] enabled = true` | `EPHPM_CLUSTER__ENABLED=true` |
| `[cluster.kv] data_port = 7950` | `EPHPM_CLUSTER__KV__DATA_PORT=7950` |

This works because ePHPm uses [figment](https://github.com/SergioBenitez/Figment) with `Env::prefixed("EPHPM_").split("__")`.

## Single underscores do not nest (startup warns)

The separator between levels must be a **double** underscore. Underscores inside a key name stay single (`MEMORY_LIMIT`, `DOCUMENT_ROOT`), so a missing underscore at a boundary is easy to type and fails in one of two ways:

| Mistake | Becomes | Result |
|---------|---------|--------|
| `EPHPM_SERVER_LISTEN` (single `_` after the section) | top-level key `server_listen` | **Ignored, with a startup `WARN`** naming the variable and suggesting `EPHPM_SERVER__LISTEN`. `listen` keeps its TOML or default value. |
| `EPHPM_PHP_MODE=worker` | top-level key `php_mode` | **Ignored, with a startup `WARN`** suggesting `EPHPM_PHP__MODE`. The server runs in the default `per_request` mode. |
| `EPHPM_SERVER__REQUEST_MAX_BODY_SIZE` (single `_` at a deeper level) | `[server] request_max_body_size` | **Startup error** naming the unknown key. |

The first two cases are a warning rather than an error because the top level of the config is deliberately lenient: every `EPHPM_*` variable in the environment becomes a top-level key, and some of them are not configuration (ePHPm's Windows service wrapper sets `EPHPM_SERVICE_LOG_FILE`; the e2e harness sets `EPHPM_URL`). Every section below the top level rejects unknown keys — see [Unknown keys are a startup error](/reference/config/#unknown-keys-are-a-startup-error). `ephpm serve` and `ephpm dev` log one `WARN` per unmapped variable:

```
WARN environment variable EPHPM_SERVER_LISTEN maps to no config key and is ignored — did you mean EPHPM_SERVER__LISTEN? ...
```

Not reported, because ePHPm or its tooling reads them directly rather than as config keys:

- the variables in [Other non-config environment variables](#other-non-config-environment-variables) (`EPHPM_KV_PASSWORD`, `EPHPM_SERVICE_LOG_FILE`, `EPHPM_MIDDLEWARE_DIR`) and `EPHPM_FATAL_HANDLER`;
- `EPHPM_RELEASE_VERSION` (read at build time; the release workflows export it into the job environment);
- `EPHPM_SESSION_SECRET` and `EPHPM_PREVIEW_SESSION_SECRET` (the secret names these docs use in `env:` examples), and any other variable that a `[[middleware]]` mount's `config` references as `env:NAME`;
- variables the org's PHP packages read with `getenv()`: `EPHPM_APP_BASE` ([Laravel Octane](/guides/laravel-octane/)), `EPHPM_WORKER_BOOTSTRAP` ([PSR-15 worker](/guides/psr15-worker/)), `EPHPM_WP_PATH` (`ephpm/wordpress-worker`) and `EPHPM_SEED_TOKEN` (`ephpm/wordpress-sample`);
- the variables the e2e and integration harnesses set (`EPHPM_URL`, `EPHPM_BINARY`, `EPHPM_CLUSTER_URL_*`, `EPHPM_TEST_*`, …).

Any other `EPHPM_*` variable you set for some other purpose (for example one referenced as `env:NAME` from a per-site override file) is still reported. The warning is advisory: it changes nothing about how the variable is used. Two limits: the "did you mean" suggestion only fixes the separator after the section name (`EPHPM_DB_SQLITE_PATH` suggests `EPHPM_DB__SQLITE_PATH`, while the real key is `EPHPM_DB__SQLITE__PATH`), and a misspelling that collides with a harness variable (`EPHPM_SITES_DIR`, `EPHPM_DOCROOT`, `EPHPM_KV_HOST`, `EPHPM_KV_PORT`, `EPHPM_TEST_*`) is not reported.

## `ephpm dev` differs

The table above describes `ephpm serve`. Under `ephpm dev` (and a bare `ephpm`, which runs it):

- **No `ephpm.toml` is read at all**, not even one in the current directory. Only the built-in defaults and `EPHPM_*` variables apply.
- `EPHPM_SERVER__LISTEN` and `EPHPM_SERVER__DOCUMENT_ROOT` have **no effect**: dev always replaces them with its own values — `--listen`, or `127.0.0.1` on the first free port from `--port` (default `8080`); `--document-root`, or the current directory.
- With `--sites <dir>`, dev also sets `sites_dir` to that directory and forces `sites_domain_suffix = ".localhost"`, overriding any variable for either key.

Other `EPHPM_*` variables apply in dev as they do in serve.

## Precedence

Highest to lowest, for `ephpm serve` (for `ephpm dev`, see [`ephpm dev` differs](#ephpm-dev-differs)):

1. **CLI flags** (`--listen`, `--document-root`, `--config`)
2. **Environment variables** (`EPHPM_*`)
3. **TOML file** (whatever `--config` points at, default `ephpm.toml`)
4. **Built-in defaults**

So you can bake a `ephpm.toml` into a container image and override per-environment via env vars without rebuilding.

## Type coercion

Values come in as strings; figment + serde coerce them:

- `bool` — `"true"` / `"false"` (case-insensitive)
- numbers — parsed as the target type (e.g. `"30"` → `u32`)
- arrays — JSON-style: `EPHPM_SERVER__INDEX_FILES='["index.php","index.html"]'`
- nested tables — usually easier to keep these in TOML; you *can* set them via JSON env values but it gets unwieldy

A variable creates its section even when the TOML has none. That matters for one section: setting any `EPHPM_SERVER__SECURITY__*` variable makes `[server.security]` "present", which changes the defaults of its isolation flags — see the note under [`[server.security]`](/reference/config/#serversecurity).

## Common production overrides

```bash
# Container with a baked-in default config, overridden per env
EPHPM_SERVER__LISTEN=0.0.0.0:8080
EPHPM_SERVER__DOCUMENT_ROOT=/var/www/app
EPHPM_DB__SQLITE__PATH=/data/app.db

# Show PHP errors in HTTP responses (staging only): `ephpm serve` defaults
# display_errors to Off and logs them instead — see reference/config.md
EPHPM_PHP__INI_OVERRIDES='[["display_errors","On"]]'

# Logging
RUST_LOG=info                                       # or info,ephpm_php=debug
EPHPM_SERVER__LOGGING__LEVEL=info                   # alternative to RUST_LOG

# Cluster identity (for k8s StatefulSet pods)
EPHPM_CLUSTER__ENABLED=true
EPHPM_CLUSTER__JOIN='["ephpm-headless.default.svc.cluster.local:7946"]'
EPHPM_CLUSTER__SECRET=$GOSSIP_SECRET                 # from a Secret/SealedSecret
EPHPM_CLUSTER__NODE_ID=$HOSTNAME                     # pod name as ordinal hint

# TLS via ACME
EPHPM_SERVER__TLS__DOMAINS='["example.com"]'
EPHPM_SERVER__TLS__EMAIL=admin@example.com
EPHPM_SERVER__TLS__CACHE_DIR=/data/certs

# DNS-01 wildcard ACME (Cloudflare). The token is a secret, so this is the
# preferred way to supply it — keep it out of ephpm.toml.
EPHPM_SERVER__TLS__DOMAINS='["*.preview.example.com","preview.example.com"]'
EPHPM_SERVER__TLS__CHALLENGE=dns-01
EPHPM_SERVER__TLS__DNS_PROVIDER=cloudflare
EPHPM_SERVER__TLS__CLOUDFLARE_API_TOKEN=$CF_DNS_EDIT_TOKEN   # zone-scoped Zone.DNS:Edit
# Other providers use the analogous EPHPM_SERVER__TLS__* variables, e.g.
# LINODE_API_TOKEN, DIGITALOCEAN_API_TOKEN, ROUTE53_ACCESS_KEY_ID +
# ROUTE53_SECRET_ACCESS_KEY, or GOOGLE_SERVICE_ACCOUNT_JSON + GOOGLE_PROJECT.
# See reference/config.md for the full per-provider field list.

# HTTP/3 (QUIC). Needs a static [server.tls] cert+key — enabling this with
# ACME is a startup error, not a silent downgrade to TCP-only.
EPHPM_SERVER__HTTP3__ENABLED=true
EPHPM_SERVER__HTTP3__ALT_SVC_MAX_AGE=86400
```

## Logging-only env var

`RUST_LOG` is read directly by the `tracing` subscriber and **takes precedence** over `[server.logging] level` / `EPHPM_SERVER__LOGGING__LEVEL`. Use `RUST_LOG` for fine-grained control:

```bash
RUST_LOG=info,ephpm_query_stats=debug,ephpm_cluster=trace
```

For `serve` and `dev`, the order is `RUST_LOG`, then `-v`/`-vv`, then `[server.logging] level`. The CLI subcommands `analyze`, `exec`, `php --site`, and the service lifecycle commands (`install`, `status`, …) also honour `RUST_LOG`, defaulting to `info`. They write their logs to **stderr**, so stdout stays clean for the command's output (for example `ephpm analyze --format sarif`).

## Crash-diagnostics env var

`EPHPM_FATAL_HANDLER` controls the fatal-signal diagnostic handler. It is **not**
a config key — it is read once at process start, before the config file is even
located, because the handler has to be installed before anything can crash.

| Value | Effect |
|-------|--------|
| unset (default) | Handler installed. On `SIGSEGV`/`SIGBUS`/`SIGILL`/`SIGFPE`/`SIGABRT`, ePHPm writes a diagnostic block to stderr and then dies with the original exit status. |
| `0`, `off`, `false`, `no` | Handler not installed. Crashes are silent again, exactly as before v0.6.2 (the release that introduced the handler). |

Disabling it does not change the exit status either way — the handler always
re-raises with the default disposition, so the container still exits 139
(`SIGSEGV`) or 134 (`SIGABRT`) and Kubernetes CrashLoopBackOff accounting is
unaffected.

Unix only. On Windows the variable is ignored: memory faults arrive as SEH
exceptions rather than signals, and that path is not implemented.

See [Diagnosing crashes](/guides/diagnosing-crashes/) for how to read the
report.

## Other non-config environment variables

These are read directly, not through the `EPHPM_` config mapping.

| Variable | Read by | Effect |
|----------|---------|--------|
| `EPHPM_KV_PASSWORD` | `ephpm kv`, `ephpm deploy`, `ephpm cache` | Fallback for `--password`: sent as the RESP `AUTH` password when the flag is absent. An empty value counts as unset. See [`ephpm kv` → Authentication](/reference/cli/kv/#authentication). |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, `OTEL_EXPORTER_OTLP_ENDPOINT` | `serve`, `dev` | Enable OTLP trace export to this endpoint. **They take precedence over `[server.diagnostics] otlp_endpoint`** (the traces-specific one first). Release binaries include the exporter (`otlp` cargo feature). A binary built without it logs a startup warning and exports nothing. See [OTLP tracing](/guides/otlp-tracing/). |
| `EPHPM_SERVICE_LOG_FILE` | `serve`, `dev` | Sends the main log output to this file (no ANSI colours) instead of stdout, creating its parent directory. The Windows service sets it to `C:\ProgramData\ephpm\logs\ephpm.log`, because a Windows service has no console. It is meant for that, but any `serve`/`dev` run honours it. It does not affect the separate `[server.logging] access` log. |
| `EPHPM_MIDDLEWARE_DIR` | `serve`, `dev` (at startup, when loading `[[middleware]]`) | Extra directory searched for shared-library middleware named by a bare `library` value: after the process working directory and before the platform default (`/usr/local/lib/ephpm/middleware` on Linux/macOS; `<directory of ephpm.exe>\middleware`, then `%ProgramData%\ephpm\middleware` on Windows). Ignored when empty. Builtin names and explicit paths never consult it. Set it in the service's environment when ePHPm runs as a service. See [Native middleware → library resolution](/guides/native-middleware/#library-resolution). |
| `MIMALLOC_PURGE_DELAY` (and other `MIMALLOC_*`) | the mimalloc allocator (Linux/macOS builds) | Not an ePHPm setting: ePHPm uses [mimalloc](https://github.com/microsoft/mimalloc) as its allocator on Unix, and mimalloc reads its own `MIMALLOC_*` options from the environment. `MIMALLOC_PURGE_DELAY` (milliseconds, mimalloc's default is 1000) controls how long freed memory is kept before it is returned to the OS. That is the lever if resident memory looks high at steady state. On Linux hosts where transparent huge pages are enabled (`/sys/kernel/mm/transparent_hugepage/enabled` is `madvise` or `always`), mimalloc asks for 2 MiB huge pages, which can raise resident memory. `MIMALLOC_ALLOW_THP=0` turns that off; note mimalloc implements it with `prctl(PR_SET_THP_DISABLE)`, which disables THP for the whole process. **Scope differs by platform.** On Linux, mimalloc replaces `malloc`/`free` for the whole process: Rust code, PHP itself (persistent allocations, per-thread globals), the bundled C libraries, glibc's own internal allocations, and shared extensions or middleware loaded with `dlopen`. So these variables tune PHP's C-side memory too, and glibc's `MALLOC_*` variables (e.g. `MALLOC_ARENA_MAX`) no longer have any effect. On macOS only ePHPm's Rust allocations use mimalloc; PHP and extensions keep the system `malloc`. Windows builds use the system allocator and ignore these variables. A Linux binary built with `cargo build -p ephpm --no-default-features` (drops the default `malloc-override` cargo feature) keeps glibc `malloc` for C code, like macOS. |

## See also

- [Configuration](config/) — every key with type and default
- [`Config::load`](https://github.com/ephpm/ephpm/blob/main/crates/ephpm-config/src/lib.rs) — the figment merge logic
