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

- Local mode only, today: a pod with an **empty** disk (a Deployment
  without a volume) starts cold. Object-storage snapshots and peer handoff
  in a cluster are the next two pieces.
- A snapshot is node-local. In a cluster each node writes its own; nothing
  merges them, and a restored value can be briefly stale next to a peer that
  kept serving. The next replicated write to the key corrects it.
- Hash fields' own TTLs are carried by the format, but the store does not
  track per-field expiry yet, so there is nothing to carry.
- Windows: the shutdown snapshot runs on `Ctrl+C` and on a service stop; a
  process kill is not graceful anywhere.
