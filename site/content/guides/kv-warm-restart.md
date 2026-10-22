+++
title = "Warm restarts (KV snapshots)"
weight = 7
+++

Out of the box a restarted ePHPm starts with an empty KV store — sessions,
cached queries, rate-limit windows all gone until the application rebuilds
them. Turning on `[kv.snapshot]` writes an encrypted copy of the store at
graceful shutdown and loads it at the next start, so the first request after
a deploy is served warm.

**This is a cache warm-up, not durability.** Nothing is written between
snapshots; a crash loses everything since the last one (unless you set
`interval`); and a snapshot is not atomic across keys. If data must survive
a restart, it belongs in the database. The design and its exact guarantees
are on the [KV snapshots](/architecture/kv-snapshots/) page.

## The minimum

```toml
[kv]
secret = "<long random string>"      # e.g. `openssl rand -base64 32`

[kv.snapshot]
mode = "local"
path = "/var/lib/ephpm/kv"
```

That is all. On `SIGTERM` the server drains requests, then writes
`/var/lib/ephpm/kv/<identity>.kvsnap`; on the next start it loads the file
before opening any listener. The log says what happened:

```
INFO KV warm-restart snapshots enabled (local file) file=/var/lib/ephpm/kv/web-1.kvsnap identity=web-1 …
INFO no KV snapshot to load (cold start) file=…            ← first start
INFO KV snapshot written file=… keys=212044 bytes=48213776 elapsed_ms=190
INFO KV snapshot loaded — serving warm file=… keys=212044 elapsed_ms=140
```

`[kv] secret` is required: every snapshot is sealed with a key derived from
it, and there is no plaintext mode. On a single-site server this secret
does nothing else unless the RESP listener is on (see
[`[kv] secret`](/reference/config/#kv)); on a multi-site server it is also
what derives the per-site RESP and `pdo_mysql` passwords, so you may already
have one.

`identity` names the file. It defaults to `[cluster] node_id` when set,
else the host name — stable for a VM, a systemd unit or a StatefulSet pod.
Set it explicitly wherever the host name is not stable (a Deployment pod):

```toml
[kv.snapshot]
identity = "web"      # every replica shares one file / object name
```

## systemd

Snapshots need a **graceful** stop. systemd's default `KillSignal=SIGTERM`
is exactly that; give the unit enough `TimeoutStopSec` for the request
drain (`[server.timeouts] shutdown`, default 30 s) plus the snapshot write
(seconds for a large store — see the benchmark on the architecture page):

```ini
[Service]
ExecStart=/usr/local/bin/ephpm serve --config /etc/ephpm/ephpm.toml
KillSignal=SIGTERM
TimeoutStopSec=90
StateDirectory=ephpm/kv        # creates /var/lib/ephpm/kv, owned by the service user
```

With `[server] run_as_user`, the directory must be writable by that user;
`StateDirectory=` handles it.

## Docker

Mount a volume at `path` and let Docker send `SIGTERM` with enough grace:

```bash
docker run -d --name app \
  -v ephpm-kv:/var/lib/ephpm/kv \
  --stop-signal SIGTERM --stop-timeout 90 \
  ghcr.io/ephpm/ephpm:latest
```

`docker stop` / `docker compose down` are graceful; `docker kill` is not.
In Compose, `stop_grace_period: 90s`.

## Kubernetes: StatefulSet + PVC

A StatefulSet gives each pod a stable name (`web-0`, `web-1`, …) and its own
PersistentVolumeClaim, so a pod that is rescheduled gets its own snapshot
back. The host name is the pod name, so `identity` needs no setting.

```yaml
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: web
spec:
  serviceName: web
  replicas: 3
  selector:
    matchLabels: { app: web }
  template:
    metadata:
      labels: { app: web }
    spec:
      # Request drain (30 s default) + snapshot write + the WAL checkpoint.
      terminationGracePeriodSeconds: 120
      containers:
        - name: ephpm
          image: ghcr.io/ephpm/ephpm:latest
          args: ["serve", "--config", "/etc/ephpm/ephpm.toml"]
          ports:
            - containerPort: 8080
          volumeMounts:
            - name: config
              mountPath: /etc/ephpm
              readOnly: true
            - name: kv
              mountPath: /var/lib/ephpm/kv
          readinessProbe:
            httpGet: { path: /_ephpm/ready, port: 8080 }
            periodSeconds: 3
      volumes:
        - name: config
          secret:
            secretName: ephpm-config      # ephpm.toml, which holds [kv] secret
  volumeClaimTemplates:
    - metadata:
        name: kv
      spec:
        accessModes: ["ReadWriteOnce"]
        storageClassName: standard        # any RWO class; the snapshot is one file
        resources:
          requests:
            storage: 2Gi                  # ≥ [kv.snapshot] max_size (default 1GB)
```

Notes:

- **Storage class.** Any `ReadWriteOnce` class works; the snapshot is a single
  sequential file, so cheap block storage is fine. Size the claim above
  `max_size`.
- **Rolling updates** stop one pod at a time with `SIGTERM`, which is the
  graceful path. `kubectl delete pod --grace-period=0` and OOM kills are not;
  set `interval` (say `"15m"`) if you want a snapshot that survives those.
- **Readiness.** Loading happens before the listeners open, so
  `/_ephpm/ready` is not answered until the snapshot is loaded or
  `load_timeout` passes — the pod does not take traffic cold-while-loading.
- **The config is a Secret** because it holds `[kv] secret`.

## Kubernetes: Deployment + object storage

A Deployment pod has no stable disk, so keep the snapshot in a bucket
instead. Any S3-compatible store works: S3, Cloudflare R2, MinIO, Google
Cloud Storage's S3 API.

```toml
[kv]
secret = "<long random string>"

[kv.snapshot]
mode = "object"
url = "s3://my-kv-snapshots/prod/web"   # bucket + prefix; object = prod/web/<identity>.kvsnap
identity = "web"                        # pod names are random: one shared identity per Deployment
# region = "us-east-1"                  # AWS default; endpoint defaults to https://s3.<region>.amazonaws.com
```

Non-AWS endpoints:

```toml
# Cloudflare R2
endpoint = "https://<account-id>.r2.cloudflarestorage.com"
region = "auto"

# MinIO (in-cluster)
endpoint = "http://minio.minio.svc:9000"

# Google Cloud Storage, S3-compatible API (HMAC key)
endpoint = "https://storage.googleapis.com"
```

Credentials come **only** from the environment — `AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`, optionally `AWS_SESSION_TOKEN` — and startup fails
if they are missing. Mount them from a Secret:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  replicas: 3
  selector:
    matchLabels: { app: web }
  template:
    metadata:
      labels: { app: web }
    spec:
      terminationGracePeriodSeconds: 120   # drain + snapshot write + upload
      containers:
        - name: ephpm
          image: ghcr.io/ephpm/ephpm:latest
          args: ["serve", "--config", "/etc/ephpm/ephpm.toml"]
          envFrom:
            - secretRef:
                name: kv-snapshot-s3      # AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY
          volumeMounts:
            - name: config
              mountPath: /etc/ephpm
              readOnly: true
          readinessProbe:
            httpGet: { path: /_ephpm/ready, port: 8080 }
      volumes:
        - name: config
          secret:
            secretName: ephpm-config
```

With one shared `identity`, every replica overwrites the same object when it
stops and a new pod loads whichever replica stopped last — for a cache
warm-up, any recent replica's store is as good as its own. On a multi-site
node the environment is visible to tenant PHP (`getenv()`), so the key must
be scoped to this one bucket and prefix. Least privilege, AWS:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    { "Effect": "Allow",
      "Action": ["s3:PutObject", "s3:GetObject", "s3:DeleteObject"],
      "Resource": "arn:aws:s3:::my-kv-snapshots/prod/web/*" },
    { "Effect": "Allow",
      "Action": ["s3:ListBucket"],
      "Resource": "arn:aws:s3:::my-kv-snapshots",
      "Condition": { "StringLike": { "s3:prefix": ["prod/web/*"] } } }
  ]
}
```

`ListBucket` is only needed by `ephpm kv snapshot list`; the server itself
uses `PutObject`, `GetObject` and (for `rm`) `DeleteObject`. Instance
profiles / IRSA are **not** supported — the client reads static keys only.
The bucket holds ciphertext sealed under `[kv] secret`; bucket-side
encryption is a second layer, not a substitute.

Operations against the bucket:

```bash
ephpm kv snapshot list --config /etc/ephpm/ephpm.toml
ephpm kv snapshot inspect s3://my-kv-snapshots/prod/web/web.kvsnap --config /etc/ephpm/ephpm.toml
ephpm kv snapshot rm web --config /etc/ephpm/ephpm.toml
```

## Cluster: peer handoff (rolling deploys)

On a clustered node (`[cluster] enabled = true`) warm restarts need no file
and no bucket: with `peer_handoff = true` (the default) a node that stops
gracefully hands the large KV values it holds to the peers that take over
their replica slots, and a node that starts pulls its share back from its
peers before it serves. Small values (at or below
`[cluster.kv] small_key_threshold`, the gossip tier) come back through
gossip on their own. The details are in
[KV snapshots → Peer handoff](/architecture/kv-snapshots/#peer-handoff-peer_handoff-on-by-default).

A rolling deploy of a 3-node StatefulSet therefore looks like this:

1. `web-2` gets `SIGTERM`. It drains requests, pushes its large values to
   `web-0`/`web-1` (seconds, logged as `KV handoff to peers done
   keys=… bytes=…`), writes its local/object snapshot if one is configured,
   and exits.
2. `web-2` restarts, joins gossip, pulls its share of large values from
   `web-0` and `web-1` (`KV warm-up pull from peers done`), then opens for
   traffic. Readiness is not reported until the pull finishes or
   `load_timeout` passes.
3. Repeat for `web-1`, then `web-0`.

The transfer in both directions is bounded by `load_timeout` (default
30 s); a starting pod with `[cluster] join` seeds first waits up to 10 s
for gossip to show an alive peer, and that wait is not shortened by
`load_timeout`. Size `terminationGracePeriodSeconds` for the drain plus
the push plus the snapshot. A pod killed without a graceful stop pushes nothing — its keys
survive only where `replication_factor` already put them, which is why the
default is 2.

On a clustered node the snapshot and the peers do **not** both apply.
With at least one alive peer, the snapshot is not restored at all: large
values come from the peers (the pull delivers exactly the keys this node
is a replica for now, with their write stamps) and small values arrive
through gossip over the next minutes (about one 64 KB gossip datagram per
second per peer), so the node misses on small keys until gossip has
caught up — and is never stale. The log says so:
`KV snapshot not restored ... reason="peers_present"`. Restoring the
snapshot next to live peers was measured and rejected: it served the
pre-restart value of every small key another node had rewritten while
this one was down (for minutes, on that node), and a stale session, lock
token or invalidation is worse than a miss.

Only a node that finds no peer within the join wait (10 s) restores its
snapshot — whole. Plainly: a whole-cluster restart where all nodes come
up together is cold; if warmth matters, stop every node and start one
first, then the others. A value that lone node restored stays in its own
store (it is not re-published into gossip); the nodes that join later
pull its large values and build the small tier from live writes.

```toml
[kv.snapshot]
peer_handoff = true        # the default; mode may stay "off" on a cluster node
load_timeout = "30s"       # bounds the pull on start and the push on stop
```

Metrics: `ephpm_kv_handoff_keys_total{direction}`,
`ephpm_kv_handoff_bytes_total{direction}`,
`ephpm_kv_handoff_duration_seconds{direction}`.

## Checking on it

```bash
ephpm kv snapshot status --config /etc/ephpm/ephpm.toml      # last save / last load
ephpm kv snapshot save   --config /etc/ephpm/ephpm.toml      # write one now
ephpm kv snapshot inspect /var/lib/ephpm/kv/web-1.kvsnap --config /etc/ephpm/ephpm.toml
ephpm kv snapshot verify  /var/lib/ephpm/kv/web-1.kvsnap --config /etc/ephpm/ephpm.toml
```

The server commands use the `/_ephpm/kv/snapshot`
[control endpoint](/reference/http-endpoints/#server-owned-paths), which is
gated like `/_ephpm/requests`: on a multi-site node set
`[server.diagnostics] token` and pass it (or `--config`, which reads it).

Metrics: `ephpm_kv_snapshot_save_total{outcome,trigger}`,
`ephpm_kv_snapshot_load_total{outcome,reason}` and friends — see
[Metrics → KV snapshots](/reference/metrics/#kv-snapshots). The alert worth
having is a cold start whose reason is anything but `missing`.

## Encryption and key rotation

Every snapshot is sealed under a key derived from `[kv] secret` with HKDF
(a fresh salt per file) and ChaCha20-Poly1305 per block; the header is
authenticated too. The secret is never written to the file, never logged.

**Changing `[kv] secret` makes every existing snapshot unreadable.** The
next start detects it before touching any data, logs one WARN with reason
`wrong_key`, and starts cold — it never crashes and never loads a file it
cannot authenticate. To rotate without a cold start: change the secret,
restart (cold), and the next graceful stop writes a snapshot under the new
one. Old files are simply overwritten.

The file is only as private as `[kv] secret` and the config file holding
it. Anyone with both can read every tenant's keys and sessions from a
snapshot — the same trust boundary the per-site RESP and database
passwords already have.

## What a load can report

| Log / `outcome` | `reason` | Meaning |
|---|---|---|
| loaded — serving warm / `ok` | — | Every block restored. |
| partially loaded / `partial` | `timeout` | `load_timeout` passed; what was restored stays. Raise it, or accept. |
| | `corrupt_blocks` | Some blocks failed to authenticate and were skipped. `ephpm kv snapshot verify` lists them. The disk is suspect. |
| | `memory_limit` | A store refused entries (`noeviction` and the snapshot is bigger than `[kv] memory_limit` now allows). |
| | `unknown_site` | Blocks for a vhost this node no longer has. |
| cold start / `cold` | `missing` | No file yet. Normal on the first start; INFO, not WARN. |
| | `wrong_key` | `[kv] secret` changed. |
| | `version` | Written by a newer ePHPm whose format this one does not read. |
| | `corrupt` | Header, trailer or index did not verify: truncated or damaged. |
| | `too_large` | Over `max_size`. |

## Limits, honestly

- Peer handoff moves data only on a graceful stop and a start; it does not
  rebalance on other membership changes, and hashes are not handed off (they
  are not replicated at all).
- Object mode reads static `AWS_*` credentials only; no instance profile,
  no IRSA, no multipart upload (one `PutObject` up to 5 GiB).
- A snapshot is node-local. In a cluster each node writes its own; nothing
  merges them, and a restored value can be briefly stale next to a peer that
  kept serving. The next replicated write to the key corrects it.
- Hash fields' own TTLs are carried by the format, but the store does not
  track per-field expiry yet, so there is nothing to carry.
- Windows: the shutdown snapshot runs on `Ctrl+C` and on a service stop; a
  process kill is not graceful anywhere.
