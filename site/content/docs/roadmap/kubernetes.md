# Kubernetes Deployment

Guide for running ePHPm in Kubernetes, covering container images, health probes,
gossip clustering, and environment-based configuration.

---

## Container Image

ePHPm ships as a single static binary. A minimal Dockerfile:

```dockerfile
FROM debian:bookworm-slim
COPY ephpm /usr/local/bin/ephpm
COPY ephpm.toml /etc/ephpm/ephpm.toml
COPY /var/www/html /var/www/html
EXPOSE 8080
ENTRYPOINT ["ephpm", "serve", "--config", "/etc/ephpm/ephpm.toml"]
```

The binary includes PHP and (optionally) sqld — no external PHP-FPM or database
sidecar is needed.

---

## Health Probes

ePHPm exposes two built-in probe endpoints on the main HTTP port:

| Endpoint | Purpose | Response |
|----------|---------|----------|
| `/_ephpm/health` | Liveness probe | `200 {"status":"ok"}` — always succeeds if the process is running |
| `/_ephpm/ready` | Readiness probe | `200 {"status":"ready"}` when PHP runtime is initialized; `503 {"status":"not_ready","reason":"PHP runtime not initialized"}` otherwise |

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
the SQLite primary. On failover, the role-change watcher restarts the sqld
sidecar in the new mode.

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
