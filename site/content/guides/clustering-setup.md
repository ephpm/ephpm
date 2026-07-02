+++
title = "Clustering Setup"
weight = 5
+++

ePHPm clusters via SWIM gossip ([chitchat](https://github.com/quickwit-oss/chitchat)). Nodes discover each other, share state, replicate the KV store, and elect a SQLite primary — all over the same gossip layer. There's no separate coordinator (no Consul, no etcd, no ZooKeeper).

## Minimum viable cluster

Three nodes, all reachable from each other on UDP 7946 (gossip) and TCP 7947 (KV data plane):

```toml
# Same on every node — only `node_id` should differ (or be left empty for auto)
[cluster]
enabled = true
bind    = "0.0.0.0:7946"
join    = ["10.0.1.10:7946", "10.0.1.11:7946", "10.0.1.12:7946"]
cluster_id = "ephpm-prod"          # only nodes with the same id will pair
```

> **Security note:** `cluster.secret` exists in the config schema but is currently parsed and **not used** — gossip traffic is unauthenticated and unencrypted today. Run the cluster on a trusted private network (VPC, WireGuard, Tailscale) and firewall ports 7946/7947 from the public internet. Different `cluster_id`s let you run multiple independent clusters on the same network.

`join` only needs to list a few seeds — once a node joins, it discovers the rest.

## Clustered SQLite

Add `[db.sqlite.replication]` so ePHPm spawns sqld and elects a primary via gossip:

```toml
[db.sqlite]
path = "/var/lib/ephpm/app.db"

[db.sqlite.replication]
role = "auto"                       # primary chosen by gossip (lowest ordinal alive node wins)
# role = "primary"                  # force this node — for static topologies
# role = "replica"                  # force this node, set primary_grpc_url

[db.sqlite.sqld]
http_listen = "127.0.0.1:8081"     # litewire → sqld
grpc_listen = "0.0.0.0:5001"       # primary streams WAL frames here
```

How it works:

- **Primary election** uses the gossip KV (`kv:sqlite:primary`) with a TTL heartbeat. The lowest-ordinal alive node wins. On failure, the next-lowest takes over within ~10s.
- The primary spawns sqld in primary mode; replicas spawn sqld in replica mode pointed at the primary's gRPC URL.
- A role-change watcher SIGTERMs and re-spawns sqld when the role flips.

> Clustered SQLite isn't supported on Windows — Turso doesn't ship a Windows sqld binary. Use single-node SQLite or a real MySQL backend on Windows.

## KV replication

The KV store is two-tiered automatically when clustering is on:

- **Small values** (≤ 512 bytes by default) ride the gossip protocol and end up on **every** node. Eventually consistent, fast convergence (~hundreds of ms).
- **Large values** are routed via consistent hashing — each key lives on a single "owner" node, reached over the TCP data plane on port 7947. Get requests fetch from the owner; hot keys promote to a local cache. If the owner node dies, its large values are lost.

```toml
[cluster.kv]
small_key_threshold = 512           # bytes — boundary between tiers
hot_key_cache       = true
hot_key_threshold   = 5             # remote fetches before local cache
hot_key_local_ttl_secs = 30
hot_key_max_memory  = "64MB"
data_port           = 7947
```

> `replication_factor` and `replication_mode` appear in the config schema but are parsed and **not implemented** yet — large values are not replicated to peers. If a value must survive node loss, keep it under `small_key_threshold` (it will gossip everywhere) or store it in the clustered SQLite database instead.

## Kubernetes

The gossip seeds use DNS — point them at a headless Service:

```toml
[cluster]
enabled = true
join = ["ephpm-headless.default.svc.cluster.local:7946"]
```

`StatefulSet` + headless `Service` works well — pod-stable DNS gives consistent ordinals, which the primary election prefers. There's a sample chart in the repo's `deploy/` directory (TODO: link when published).

## Verify the cluster

Watch the logs on each node — gossip membership changes are logged at `info` level as nodes discover each other and the SQLite primary is elected:

```bash
ephpm logs -f          # when running as a service
# or read the process's stdout/stderr directly
```

You should see membership lines for every peer and one node logging that it won the primary election.

To poke at the KV store with `ephpm kv`, first enable the RESP listener (it's off by default):

```toml
[kv.redis_compat]
enabled = true
listen = "127.0.0.1:6379"
```

```bash
ephpm kv ping                       # PONG — the KV store is up
ephpm kv set probe hello --ttl 60   # small value → gossips to every node
ephpm kv get probe                  # run on another node once it converges
```

Prometheus metrics on `/metrics` cover HTTP, PHP, DB, and KV behaviour. Cluster-specific metrics are limited today — there is no dedicated gauge of live peers or replication lag yet; use the gossip log lines for membership. (See [Reference → Metrics](/reference/metrics/) for what exists.)

## Failure modes worth knowing

- **Network partition** — gossip detects partitions in seconds. The minority side will see itself as a smaller cluster; the majority retains the SQLite primary.
- **Primary crash** — replicas detect via gossip TTL expiry on `kv:sqlite:primary`, the next-lowest-ordinal node grabs it, and sqld restarts in primary mode. Window is ~10s.
- **Brief Replica-with-empty-URL window** — between primary death and re-election, `evaluate_role` returns `Replica` with an empty primary URL for one tick. The role watcher sees two transitions (Replica-empty → Primary). Harmless, but logs are loud.

## See also

- [Architecture → Clustering](/architecture/clustering/) — design rationale and protocol details
- [Architecture → Database](/architecture/database/) — sqld lifecycle and election internals
- [TLS / ACME](tls-acme/) — how cert distribution works in a cluster
