+++
title = "ephpm cache"
weight = 5
+++

Manage the OPcache from the CLI. Currently only the `reset` subcommand
is implemented; `status` is planned. See the
[OPcache clustering roadmap](/roadmap/opcache-clustering/).

## Synopsis

```bash
ephpm cache [--host HOST] [--port PORT] <subcommand>
```

| Flag | Default | Purpose |
|------|---------|---------|
| `--host` | `127.0.0.1` | RESP server host |
| `--port` | `6379` | RESP server port |

## Subcommands

### `reset [--site NAME | --all]`

Invalidate the OPcache for one vhost (or every vhost via the broadcast
key). Functionally identical to `ephpm deploy` — both write the same
`opcache:version:<vhost>` key via the RESP listener. The separate
command exists so operators can distinguish a local dev reset from a
deploy event in shell history or audit logs. On a cluster, both
propagate via gossip because the RESP write lands in the same in-process
KV.

```bash
# Reset a single vhost
ephpm cache reset --site blog

# Reset every vhost (broadcast)
ephpm cache reset --all

# Single-node / no sites_dir
ephpm cache reset
```

The running server must have `[kv.redis_compat] enabled = true` — the
CLI is a separate process from the server, so it cannot poke the
in-process KV `DashMap` directly. If the RESP listener is not reachable
the CLI prints a hint pointing at the config knob.

### `status` — planned, not yet implemented

Global and per-vhost OPcache stats (hit rate, script count, memory).
The design is in the [roadmap](/roadmap/opcache-clustering/) but the
subcommand is not shipped. Use `opcache_get_status()` from PHP for now.

## See also

- [`ephpm deploy`](../deploy/) — the deploy-shaped variant of `reset`
- [OPcache clustering roadmap](/roadmap/opcache-clustering/)
- [`[opcache]` config](../../config/#opcache)
