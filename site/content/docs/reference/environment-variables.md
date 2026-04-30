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
| `[cluster.kv] data_port = 7948` | `EPHPM_CLUSTER__KV__DATA_PORT=7948` |

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
```

## Logging-only env var

`RUST_LOG` is read directly by the `tracing` subscriber and **takes precedence** over `[server.logging] level` / `EPHPM_SERVER__LOGGING__LEVEL`. Use `RUST_LOG` for fine-grained control:

```bash
RUST_LOG=info,ephpm_query_stats=debug,ephpm_cluster=trace
```

## See also

- [Configuration](config/) — every key with type and default
- [`Config::load`](https://github.com/ephpm/ephpm/blob/main/crates/ephpm-config/src/lib.rs) — the figment merge logic
