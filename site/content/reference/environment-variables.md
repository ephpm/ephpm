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
| `[php] memory_limit = "256M"` | `EPHPM_PHP__MEMORY_LIMIT=256M` |
| `[db.sqlite] path = "/var/lib/app.db"` | `EPHPM_DB__SQLITE__PATH=/var/lib/app.db` |
| `[db.sqlite.replication] role = "primary"` | `EPHPM_DB__SQLITE__REPLICATION__ROLE=primary` |
| `[kv] compression = "zstd"` | `EPHPM_KV__COMPRESSION=zstd` |
| `[kv.redis_compat] enabled = true` | `EPHPM_KV__REDIS_COMPAT__ENABLED=true` |
| `[cluster] enabled = true` | `EPHPM_CLUSTER__ENABLED=true` |
| `[cluster.kv] data_port = 7950` | `EPHPM_CLUSTER__KV__DATA_PORT=7950` |

This works because ePHPm uses [figment](https://github.com/SergioBenitez/Figment) with `Env::prefixed("EPHPM_").split("__")`.

## Precedence

Highest to lowest:

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

## Common production overrides

```bash
# Container with a baked-in default config, overridden per env
EPHPM_SERVER__LISTEN=0.0.0.0:8080
EPHPM_SERVER__DOCUMENT_ROOT=/var/www/app
EPHPM_DB__SQLITE__PATH=/data/app.db

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

## See also

- [Configuration](config/) — every key with type and default
- [`Config::load`](https://github.com/ephpm/ephpm/blob/main/crates/ephpm-config/src/lib.rs) — the figment merge logic
