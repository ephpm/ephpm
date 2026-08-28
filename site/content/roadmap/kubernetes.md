# Kubernetes Deployment

Guide for running ePHPm in Kubernetes, covering container images, health probes,
gossip clustering, and environment-based configuration.

---

## Container Image

ePHPm ships as a single self-contained binary (glibc-dynamic on Linux, glibc
floor 2.28 — any glibc ≥ 2.28 base works; this matches the official image's
`debian:12-slim` runtime). A minimal Dockerfile:

```dockerfile
FROM debian:12-slim
COPY ephpm /usr/local/bin/ephpm
COPY ephpm.toml /etc/ephpm/ephpm.toml
COPY /var/www/html /var/www/html
EXPOSE 8080
ENTRYPOINT ["ephpm", "serve", "--config", "/etc/ephpm/ephpm.toml"]
```

The binary includes PHP and the embedded Turso engine — no external PHP-FPM or
database sidecar is needed.

---

## Health Probes

ePHPm exposes three built-in probe endpoints on the main HTTP port:

| Endpoint | Purpose | Response |
|----------|---------|----------|
| `/_ephpm/health` | Liveness probe | `200 {"status":"ok"}` — always succeeds if the process is running |
| `/_ephpm/ready` | Readiness probe | `200 {"status":"ready"}` once the PHP runtime is initialized, a worker has finished booting (worker mode only), and every configured SQL proxy has reached its upstream at least once. Otherwise `503 {"status":"not_ready","reason":"..."}` naming which of those is outstanding. |
| `/_ephpm/primary` | Active-passive routing target for the writable clustered-SQLite node | `200 {"primary":true}` when this node accepts writes — the elected clustered-SQLite primary, **or any non-clustered/standalone node** (trivially writable). `503 {"primary":false}` when this node is a clustered-SQLite replica. On failover the new primary starts returning 200 within the election interval. |

Readiness gates on the SQL proxy's **first** upstream connect, not on live
database reachability — a shared-database outage must not fail every replica's
probe at once and empty the Service. See
[Readiness and the database proxy](/reference/config/#readiness-and-the-database-proxy).

### Active-passive writes: `/_ephpm/primary`

In clustered single-database mode (`is_clustered_sqlite` — `[db.sqlite]` with
`[cluster]` enabled), exactly one node is the elected SQLite **primary**; the
others are **replicas**. Writes must go to the primary: a write against a
replica silently diverges and is lost. A plain round-robin load balancer,
unaware of the role, sends some writes to replicas.

`/_ephpm/primary` lets an external load balancer route **active-passive** to the
current primary: point a dedicated backend/upstream health check at it and send
write traffic only to the node whose check passes. It returns `200` on the
primary and `503` on every replica, so at most one backend is ever "up". On
failover the newly elected primary flips to `200` on its next election tick
(and the old one to `503`), and the load balancer follows.

The endpoint is safe to health-check in **any** topology — it never 404s. On a
standalone or non-clustered node there is no election, so the node is trivially
writable and the check is a constant `200`. That means the same LB manifest
works whether or not clustering is enabled.

```yaml
# Example: an LB backend that only accepts the writable node.
# (HAProxy-style; adapt to your load balancer's health-check syntax.)
backend ephpm_writes
  option httpchk GET /_ephpm/primary
  http-check expect status 200
  # every ephpm pod is listed; only the primary passes the check
  server ephpm-0 10.0.1.10:8080 check
  server ephpm-1 10.0.1.11:8080 check
  server ephpm-2 10.0.1.12:8080 check
```

### Pod spec example

```yaml
livenessProbe:
  httpGet:
    path: /_ephpm/health
    port: 8080
  initialDelaySeconds: 5
  periodSeconds: 10
readinessProbe:
  httpGet:
    path: /_ephpm/ready
    port: 8080
  initialDelaySeconds: 3
  periodSeconds: 5
```

---

## Single-Node Deployment

A basic Deployment for single-node ePHPm (no clustering, embedded SQLite):

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: ephpm
spec:
  replicas: 1
  selector:
    matchLabels:
      app: ephpm
  template:
    metadata:
      labels:
        app: ephpm
    spec:
      containers:
        - name: ephpm
          image: your-registry/ephpm:latest
          ports:
            - containerPort: 8080
              name: http
          livenessProbe:
            httpGet:
              path: /_ephpm/health
              port: 8080
          readinessProbe:
            httpGet:
              path: /_ephpm/ready
              port: 8080
          env:
            - name: EPHPM_SERVER__LISTEN
              value: "0.0.0.0:8080"
          resources:
            requests:
              memory: "128Mi"
              cpu: "100m"
            limits:
              memory: "512Mi"
              cpu: "1000m"
```

---

## Gossip Clustering via Headless Services

ePHPm uses SWIM gossip (via chitchat) for cluster membership. In Kubernetes,
a **headless Service** provides DNS-based peer discovery.

### StatefulSet for Clustered SQLite

Clustered SQLite requires stable pod identities for primary election and
WAL frame replication. Use a StatefulSet:

```yaml
apiVersion: v1
kind: Service
metadata:
  name: ephpm-cluster
  labels:
    app: ephpm-cluster
spec:
  clusterIP: None  # headless — each pod gets a DNS record
  selector:
    app: ephpm-cluster
  ports:
    - name: http
      port: 8080
    - name: gossip
      port: 7946
      protocol: UDP
    - name: gossip-tcp
      port: 7946
      protocol: TCP
    - name: grpc
      port: 5001
---
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: ephpm-cluster
spec:
  serviceName: ephpm-cluster
  replicas: 3
  selector:
    matchLabels:
      app: ephpm-cluster
  template:
    metadata:
      labels:
        app: ephpm-cluster
    spec:
      containers:
        - name: ephpm
          image: your-registry/ephpm:latest
          ports:
            - containerPort: 8080
              name: http
            - containerPort: 7946
              name: gossip
              protocol: UDP
            - containerPort: 5001
              name: grpc
          env:
            - name: EPHPM_SERVER__LISTEN
              value: "0.0.0.0:8080"
            - name: EPHPM_CLUSTER__ENABLED
              value: "true"
            - name: EPHPM_CLUSTER__CLUSTER_ID
              value: "my-cluster"
            - name: EPHPM_CLUSTER__GOSSIP_ADDR
              value: "0.0.0.0:7946"
            - name: EPHPM_CLUSTER__JOIN
              value: "ephpm-cluster-0.ephpm-cluster:7946,ephpm-cluster-1.ephpm-cluster:7946,ephpm-cluster-2.ephpm-cluster:7946"
            - name: EPHPM_DB__SQLITE__REPLICATION__ROLE
              value: "auto"
          volumeMounts:
            - name: data
              mountPath: /data
          livenessProbe:
            httpGet:
              path: /_ephpm/health
              port: 8080
          readinessProbe:
            httpGet:
              path: /_ephpm/ready
              port: 8080
  volumeClaimTemplates:
    - metadata:
        name: data
      spec:
        accessModes: ["ReadWriteOnce"]
        resources:
          requests:
            storage: 1Gi
```

### How Gossip Discovery Works

Each pod in the StatefulSet gets a stable DNS name:
`ephpm-cluster-{0,1,2}.ephpm-cluster.<namespace>.svc.cluster.local`

The `join` list uses these addresses. On startup, each node contacts the seed
peers via gossip (UDP port 7946). Failure detection converges in ~10-30 seconds.

Primary election uses the gossip KV tier: the lowest-ordinal alive node becomes
the SQLite primary. On failover, the role-change watcher reconfigures the node's
Turso CDC role in-process — there is no sidecar to restart.

---

## Environment Variable Configuration

All ePHPm config can be set via environment variables with the `EPHPM_` prefix.
Nesting uses `__` as separator:

| TOML path | Environment variable |
|-----------|---------------------|
| `server.listen` | `EPHPM_SERVER__LISTEN` |
| `server.document_root` | `EPHPM_SERVER__DOCUMENT_ROOT` |
| `server.timeouts.request` | `EPHPM_SERVER__TIMEOUTS__REQUEST` |
| `php.memory_limit` | `EPHPM_PHP__MEMORY_LIMIT` |
| `db.sqlite.path` | `EPHPM_DB__SQLITE__PATH` |
| `cluster.enabled` | `EPHPM_CLUSTER__ENABLED` |
| `kv.memory_limit` | `EPHPM_KV__MEMORY_LIMIT` |

Environment variables override TOML config file values.

---

## Prometheus Metrics

When `server.metrics.enabled = true` (or `EPHPM_SERVER__METRICS__ENABLED=true`),
ePHPm exposes a Prometheus-compatible metrics endpoint. See
[metrics.md](metrics.md) for the full list of exported metrics.

Configure a `ServiceMonitor` or Prometheus scrape annotation:

```yaml
metadata:
  annotations:
    prometheus.io/scrape: "true"
    prometheus.io/port: "8080"
    prometheus.io/path: "/metrics"
```

---

## Helm Chart

A Helm chart is planned but not yet available. For now, use the raw manifests
above or adapt them to your deployment tooling (Kustomize, Pulumi, etc.).
