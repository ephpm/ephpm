+++
title = "ephpm deploy"
weight = 4
+++

Trigger a cluster-wide OPcache invalidation. The command writes
`opcache:version:<vhost>` (or the broadcast key `opcache:version:_all`)
to the running server's RESP listener; gossip replicates the write to
every peer within seconds, and each node's watcher invalidates its
OPcache under the vhost's docroot on the next PHP request.

This is the Phase-1 OPcache clustering interface. See the
[design page](/roadmap/opcache-clustering/) for the full mechanism.

## Synopsis

```bash
ephpm deploy --site <NAME> [--rev SHA] [--host HOST] [--port PORT]
ephpm deploy --all           [--rev SHA] [--host HOST] [--port PORT]
ephpm deploy                 [--rev SHA] [--host HOST] [--port PORT]
```

| Flag | Default | Purpose |
|------|---------|---------|
| `--site` | (none) | Vhost to invalidate. Mutually exclusive with `--all`. |
| `--all` | `false` | Invalidate every vhost via the broadcast key. |
| `--rev` | (none) | Optional revision tag (e.g. a git SHA). Recorded at `opcache:revision:<vhost>` for observability; does not itself trigger invalidation. |
| `--host` | `127.0.0.1` | RESP server host |
| `--port` | `6379` | RESP server port |

Neither `--site` nor `--all` means "the default vhost" (`_default`),
which is what a single-node deployment with no `sites_dir` uses.

The running server must have `[kv.redis_compat] enabled = true`; the
CLI is a separate process from the server, so it cannot poke the
in-process KV `DashMap` directly. If the RESP listener is not reachable
the CLI prints a hint pointing at the config knob.

## Requirements

- `[opcache] cluster_invalidation` must be `true` on every node (or
  unset with `[cluster] enabled = true`, which auto-defaults to
  `true`). Otherwise the watcher stays off and no invalidation runs.
- `[php] mode = "fpm"` — worker mode is a Phase-1 gap and the watcher
  is skipped there. Startup logs a WARN when cluster invalidation is
  enabled under worker mode so the no-op is never silent.

## Examples

```bash
# Single vhost, no revision tag
ephpm deploy --site blog

# Same, with a git SHA for the deploy log
ephpm deploy --site blog --rev a8f13d2

# Fan out to every vhost (blog, shop, docs, ...) with one write
ephpm deploy --all --rev v3.2.1

# Single-node deployment (no sites_dir)
ephpm deploy

# Remote node
ephpm deploy --site blog --host 10.0.1.5 --port 6379
```

## What actually gets written

```
opcache:version:<vhost>  →  <epoch_ms>       (SET, no TTL)
opcache:revision:<vhost> →  <rev> if --rev   (SET, no TTL)
```

The watcher does not require the version to strictly increase — any
change wins the `current_version > last_invalidated_version` check
after gossip lands, so ordering across nodes is not an issue in
practice.

## See also

- [`ephpm cache`](../cache/) — local reset without the deploy semantics
- [OPcache clustering roadmap](/roadmap/opcache-clustering/) — full design
- [`[opcache]` config](../../config/#opcache) — the `cluster_invalidation` knob
