+++
title = "Clustering Setup"
weight = 5
+++

ePHPm clusters via SWIM gossip ([chitchat](https://github.com/quickwit-oss/chitchat)). Nodes discover each other, share state, replicate the KV store, and elect a SQLite primary — all over the same gossip layer. There's no separate coordinator (no Consul, no etcd, no ZooKeeper).

## Minimum viable cluster

Three nodes, all reachable from each other on UDP 7946 (gossip) and TCP 7947 (KV data plane). Clustered SQLite also uses TCP 7948, the [cluster channel](/architecture/clustering/#cluster-channel-tcp-opt-in) that carries Turso CDC replication.

```toml
# Same on every node — only `node_id` should differ (or be left empty for auto)
[cluster]
enabled = true
bind    = "0.0.0.0:7946"
join    = ["10.0.1.10:7946", "10.0.1.11:7946", "10.0.1.12:7946"]
secret  = "REPLACE_ME"             # required -- see the security note below
cluster_id = "ephpm-prod"          # only nodes with the same id will pair
```

> **Required: `cluster.secret`.** As of this release, clustering **fails to start** if `enabled = true` and `secret` is empty. Set `cluster.secret` (any high-entropy string, e.g. `openssl rand -base64 32`) -- the same value on every node. When set, all inter-node traffic -- gossip UDP **and** the TCP KV data plane -- is authenticated and encrypted with ChaCha20-Poly1305 keys derived from the secret via HKDF-SHA256. Nodes without the matching secret cannot join, read, or inject; their packets are silently dropped. Different `cluster_id`s let you run multiple independent clusters on the same network.
>
> **Breaking change:** older releases allowed an empty secret (plaintext, unauthenticated) with only a startup warning. That is now a hard startup error, because an empty secret lets any host on the cluster network forge KV writes (sessions, rate-limit counters, ACME/OPcache keys). If you must run without a secret on a fully trusted private network (VPC, WireGuard, Tailscale) with ports 7946/7947 firewalled from untrusted hosts, opt in explicitly with `allow_insecure_no_auth = true` -- a loud warning is still logged. Not recommended.

`join` only needs to list a few seeds — once a node joins, it discovers the rest.

## Clustered SQLite (experimental)

Add `[db.sqlite.replication]` so ePHPm elects a primary via gossip and replicates in-process via the Turso CDC path. As of v0.7.0 there is **no sqld sidecar** — clustering is the in-process Turso change-data-capture stream shipped over the cluster channel. Because the Turso engine is Beta upstream and this path is manually validated, treat clustered mode as **experimental** and run it on Linux/macOS.

```toml
[db.sqlite]
path = "/var/lib/ephpm/app.db"

[db.sqlite.replication]
role = "auto"                       # primary chosen by gossip (lowest node_id among alive nodes wins)
# role = "primary"                  # force this node — for static topologies
# role = "replica"                  # force this node, set primary_grpc_url to the primary's cluster channel address
```

There is no `cdc_experimental` opt-in flag anymore — enabling `[cluster]` with `[db.sqlite]` selects CDC replication unconditionally.

> **`role` values are checked.** `role` is matched exactly (case-sensitive) against `"auto"`, `"primary"`, `"replica"`, `"single"` and `"none"`; anything else, including a typo like `"Primary"`, fails startup naming the value. `role = "single"` (or `"none"`) is how you keep one node's database local even with `[cluster]` on. After startup, look for the `embedded SQLite mode selected` log line; its `mode` field must read `clustered` (or `per-site-clustered`).

"Lowest ordinal" means the alive node whose `[cluster] node_id` sorts lowest as a string. If `node_id` is left empty it is generated as `<hostname>-<random hex>`, so which node wins is effectively arbitrary; set `node_id` explicitly (`node-1`, `node-2`, …) if you want to choose the primary.

How it works:

- **Primary election** uses the gossip KV (`kv:sqlite:primary`) with a TTL heartbeat. The lowest-ordinal alive node wins. On failure, the next-lowest takes over within ~10s.
- The primary publishes its Turso `turso_cdc` stream over the cluster channel; replicas connect to the primary's cluster channel address and apply per-transaction batches. Before a replica dials that address it validates the advertised host against the live gossip membership list and refuses to connect to a host that is not a known cluster member -- defense in depth so a forged primary claim cannot redirect replication at an attacker-controlled peer.
- On a role flip the node reconfigures its CDC role in-process — there is no child process to restart.

> Clustered SQLite is tested on Linux/macOS; on Windows it is untested. Single-node SQLite and the MySQL/PostgreSQL proxy work fine on Windows.

### Routing writes: `/_ephpm/primary`

In this mode only the primary's writes replicate. A replica's wire listener still **accepts** writes (litewire has no read-only mode), but they are never replicated and silently diverge from the primary, so route every write to the primary. Every node answers `GET /_ephpm/primary` with `200 {"primary":true}` on the primary and `503 {"primary":false}` on a replica. Use it as the health check of a "writes" backend pool (or a Kubernetes Service backed by a readiness-style probe on that path) so write traffic follows the primary through a failover, and send read-only traffic to all nodes. A worked example is in the [Kubernetes roadmap → Active-passive writes](/roadmap/kubernetes/#active-passive-writes-_ephpmprimary).

## Per-site clustered databases (experimental)

On a multi-site node (`[server] sites_dir`), the clustered mode above shares **one** database between every site, and startup warns about it. To give each site its own database that replicates across the cluster, opt in:

```toml
[server]
sites_dir = "/srv/sites"

[db.sqlite]
dir = "/var/lib/ephpm/dbs"          # required: one <site-key>.db per site

[db.sqlite.replication]
per_site = true                     # role stays "auto"
```

How it differs from whole-database clustering:

- **No cluster-wide primary.** Each site has its own owner, chosen by rendezvous hashing (HRW) over the alive nodes, so sites spread across the cluster and a node failure moves only that node's sites.
- **Every node accepts writes for every site.** A node that does not own a site forwards that site's statements to the owner over the cluster channel — for both the `ephpm_db_*` bridge and stock `pdo_mysql`. `/_ephpm/primary` therefore returns `200` on every healthy node; use plain round-robin routing.
- **A connection is routed once, when it opens.** If ownership moves while it is open (a node joining is enough), the tenant sees a connection error until it reconnects.
- **A forwarded statement never waits forever.** While the owner runs a forwarded statement it sends a heartbeat every 5 s, so a slow statement is never cut off; but 30 s with no reply and no heartbeat (a stopped, overloaded or partitioned owner) fails the statement with a connection error, and the next statement re-resolves the site's owner. Channel sockets use TCP keepalive, so a peer whose host died is noticed in about 30 s. During a rolling upgrade, a statement forwarded to an owner still on an older release (which sends no heartbeats) fails if it runs longer than 30 s.
- **Ownership moves carry the data.** A new owner that holds no copy of a site does not serve it until it has one. It asks every alive peer in turn, starting with the previous owner, and pulls a snapshot from the first peer that holds the site's data. While it waits, requests for that site get an error saying why. The site starts with an empty database only when every alive peer says it holds no copy. If a peer that might hold a copy cannot be reached, the site stays unserved, the node logs a `WARN` and retries, and `ephpm_cdc_bootstrap_total{outcome="handoff_blocked"}` counts the retries. Serving the site empty would split its data, so the node refuses instead. A peer asked about a site it never held answers that it has no copy and does not create a database file for it.
- **Every node must use the same ownership hash.** Nodes compute owners independently, so they must all hash the same way. Each node advertises its ownership-hash scheme in gossip. While an alive peer advertises a different scheme (or none, as a v0.12.x node does) on two consecutive checks 10 s apart, a v0.13.0 node logs an `ERROR` naming the peer and **refuses to serve any site as its owner**, for reads and writes alike. It still forwards statements for sites other nodes own. The older nodes keep agreeing with each other, so each site keeps exactly one node accepting its writes. Once every alive peer advertises the same scheme, the node serves again with no restart needed.

> **Upgrading per-site clustered mode to v0.13.0 requires a full-cluster restart.** v0.13.0 replaced the ownership hash (scheme 2). The old one put sites whose keys share a prefix, such as `s001`, `s002` or `pr-12`, on a single node. The new scheme moves most sites to a different owner. A v0.12.x node and a v0.13.0 node disagree on who owns what, so in a mixed cluster the v0.13.0 nodes refuse to act as owner (see above) and only the v0.12.x nodes accept writes. Whole-database clustered mode and single-node mode are unaffected.
>
> 1. Stop writes and wait until every site's replicas have caught up: for every site, `max by (site) (ephpm_cdc_primary_head_change_id) - min by (site) (ephpm_cdc_applied_change_id)` is `0`. The owner's own `ephpm_cdc_replication_lag_changes` is not enough: it covers only replicas that are attached, so it reads `0` while another replica is still bootstrapping.
> 2. Stop every node, upgrade, then start them all.
> 3. Wait until every node has joined (each node sees all the others alive) before you send traffic again.
>
> After the restart, a site's new owner serves its own copy if it has one. A new owner with no copy pulls the site from whichever alive node holds one, so it does not matter which node the new hash ranks next. Either way the copy served is one that existed before the restart, and step 1 is what makes every copy current. Step 3 matters because a node that is down is not asked. A new owner waits 15 s after it starts before it decides that no peer has a copy, but a node that starts later than that is missed, and a site whose only copy is on that node would start empty.

This mode is **experimental**: the Turso engine is Beta upstream and multi-node ownership churn is unit-tested, not validated on a live cluster. The full rules are in the [`[db.sqlite.replication]` reference](/reference/config/#dbsqlitereplication-clustered-mode-only) and [Multi-tenant `pdo_mysql` → Clustered mode](/guides/multi-tenant-pdo-mysql/#clustered-mode).

## KV replication

The KV store is two-tiered automatically when clustering is on:

- **Small values** (≤ 512 bytes by default) ride the gossip protocol and end up on **every** node. Eventually consistent, fast convergence (~hundreds of ms). Sized for cluster-wide small state — sessions, counters, locks, control keys: gossip carries a few hundred entries per second per peer, so a node writing *distinct* small keys faster than that (a high-churn cache with unique keys) will shed replication for the excess (logged) rather than fall behind forever.
- **Large values** are routed by `hash(key) % alive_nodes` (a plain modulo of the sorted alive-node list — no consistent hash ring or virtual nodes). Each key has a primary owner reached over the TCP data plane on port 7947, and is **replicated** to `replication_factor` nodes total (default 2), with its TTL. A read on any other node fetches through to the owner, then the other replicas (2 s budget), so a large value is readable from every node and survives the loss of its owner. `EXPIRE` and `DEL` on a large value reach its replicas. Hot keys additionally promote to a local read cache.

```toml
[cluster.kv]
small_key_threshold = 512           # bytes — boundary between tiers
replication_factor  = 2             # copies of each large value (owner + N-1 peers)
replication_mode    = "async"       # async (default) or sync
hot_key_cache       = true
hot_key_threshold   = 5             # remote fetches before local cache
hot_key_local_ttl_secs = 30
hot_key_max_memory  = "64MB"
data_port           = 7947
```

- `replication_factor = 2` keeps each large value on its owner plus the next node on the ring; it is clamped to the number of alive nodes, so a factor larger than the cluster just keeps one copy per node.
- `replication_mode = "async"` (default) returns after the primary/local write and replicates in the background; `"sync"` also awaits every reachable replica (best-effort — a down replica is logged, not fatal). Any other value silently means `"async"`.
- `data_port` binds the IP of `[cluster] bind` (with the default `0.0.0.0` that is every interface — firewall it on multi-homed hosts, or bind a specific IP). Each node advertises its port over gossip, so nodes may use different ports; peers dial `<gossip IP>:<advertised port>`. Data-plane connections are reused between requests and every step is bounded (2 s connect, 10 s per request), so a frozen peer costs a timeout, not a growing pile of sockets.
- Counters (`INCR`/`DECR`, and so the `ratelimit` middleware) are approximate cluster-wide. Each increment replicates its resulting total, last writer wins, so concurrent increments on different nodes overwrite rather than add. Limits can under-count, up to roughly N times the allowance across N nodes in the worst case.

The KV is a cache, not a message broker: there are no lists, sets or `MULTI`/`EVAL`, so Redis-backed job queues (Laravel queues included) need a real Redis. See [Architecture → Clustering → Redis Protocol](/architecture/clustering/#1-redis-protocol-resp--redis-compatible-for-caching-not-a-redis-replacement) for what the RESP listener does and does not support.

PHP sessions are **not** stored in the KV unless you enable the native handler (`session.save_handler = ephpm`); by default they are node-local files. See [Architecture → Clustering → Session Storage](/architecture/clustering/#session-storage).

> **Membership caveat:** replication is write-time only — there's no active rebalancing yet. A node that was down during a write won't have that key until it's rewritten or fetched-through, and existing keys aren't re-replicated when membership changes. A value that must survive `replication_factor - 1` simultaneous node losses, or any topology churn, is safer under `small_key_threshold` (it gossips everywhere) or in the clustered SQLite database.

## Kubernetes

The gossip seeds use DNS — point them at a headless Service:

```toml
[cluster]
enabled = true
join = ["ephpm-headless.default.svc.cluster.local:7946"]
```

`StatefulSet` + headless `Service` works well. With `node_id` unset, the generated id starts with the pod name (`ephpm-0-…`), so the lowest-numbered pod tends to win the election — but ids compare as strings (`ephpm-10` sorts before `ephpm-2`), so set `node_id` explicitly if the primary's identity matters. There is no Helm chart; the worked manifests live in [`k8s/base/`](https://github.com/ephpm/ephpm/tree/main/k8s/base) — [`ephpm-cluster.yaml`](https://github.com/ephpm/ephpm/blob/main/k8s/base/ephpm-cluster.yaml) is the clustered StatefulSet + headless Service the E2E suite runs against, and [`ephpm-single.yaml`](https://github.com/ephpm/ephpm/blob/main/k8s/base/ephpm-single.yaml) is its single-node counterpart. They are test fixtures rather than a packaged release artifact, so treat them as a starting point.

### Give pods time to close their databases

Kubernetes sends `SIGKILL` `terminationGracePeriodSeconds` (default **30 s**) after `SIGTERM`. A node with embedded SQLite (`[db.sqlite]`) needs longer than that to stop cleanly:

1. Requests drain, bounded by `[server.timeouts] shutdown` (default 30 s on its own).
2. Every open database's write-ahead log gets a final `wal_checkpoint(TRUNCATE)`. This runs to completion **outside** that bound, because it is what spares the next start a WAL recovery. Its length depends on how much was written since the last checkpoint and on disk sync latency. In a 3-node per-site cluster (60 sites) stopped while its peers stayed under full write load on one shared disk, it took 45–74 s, and whole-process exit took 50–77 s.
3. The async runtime gets at most 15 s more.

A `SIGKILL` during step 2 loses no committed data: the engine recovers the log on the next open, the same as after a crash. But the next start then pays for that recovery on every database whose log was left full. For clustered deployments with an embedded database, set the grace period to cover all three steps with room to spare:

```yaml
spec:
  template:
    spec:
      # [server.timeouts] shutdown (30) + final checkpoint + runtime stop (15)
      terminationGracePeriodSeconds: 120
```

Watch the `closing embedded databases` lines in the pod's log (INFO once the checkpoint passes 3 s) to see what your workload actually needs.

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

Prometheus metrics on `/metrics` cover HTTP, PHP, DB, and KV behaviour. Cluster-specific metrics are limited today — there is no dedicated gauge of live peers or replication lag yet; use the gossip log lines for membership. (See [Reference → Metrics](/reference/metrics/) for what exists.) If the nodes are multi-site (`[server] sites_dir`), `/metrics` answers `403` until `[server.metrics] token` (or an mTLS metrics listener) is set. See [Scraping with Prometheus](/reference/metrics/#scraping-with-prometheus).

## Failure modes worth knowing

- **Network partition** — gossip detects partitions in seconds. The minority side will see itself as a smaller cluster; the majority retains the SQLite primary.
- **Primary crash** — replicas detect via gossip TTL expiry on `kv:sqlite:primary`, and the next-lowest-ordinal node promotes itself and begins publishing its CDC stream. Window is ~10s.
- **Brief Replica-with-empty-URL window** — between primary death and re-election, `evaluate_role` returns `Replica` with an empty primary URL for one tick. The role watcher sees two transitions (Replica-empty → Primary). Harmless, but logs are loud.
- **Primary restart with a wiped/replaced database** — a fast restart of the incumbent normally reclaims the primary role immediately (its own claim on `kv:sqlite:primary` is still unexpired within the ~10s TTL), which avoids bouncing the role. If that restart also *replaced the database* — a redeploy onto an empty volume — the node comes back with an empty database and a fresh CDC log identity. The primary claim carries the node's CDC log identity, and the fast reclaim requires it to match, so the wiped node **refuses** the reclaim and defers to the normal grace + election instead of re-rooting the cluster onto its empty database (issue #344). You will see a `WARN` naming the mismatched log identity. Persist the SQLite data directory across restarts (a `StatefulSet` volume claim) so a legitimate restart keeps its data and reclaims fast.

## See also

- [Architecture → Clustering](/architecture/clustering/) — design rationale and protocol details
- [Architecture → Database](/architecture/database/) — Turso CDC replication and election internals
- [TLS / ACME](tls-acme/) — how cert distribution works in a cluster
