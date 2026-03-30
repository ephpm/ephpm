# E2E Test Coverage & Plan

Current state of end-to-end test coverage and the tests we still need to build.

---

## Current Coverage (72 tests, 14 files)

The `ephpm-e2e` crate lives in `crates/ephpm-e2e/` and runs inside a Kind cluster via Tilt. See [developer/testing.md](../developer/testing.md) for infrastructure details.

### Single-Node Tests (72 tests, 14 files)

| File | Tests | Coverage |
|------|------:|----------|
| `basic.rs` | 3 | 404 errors, PHP rendering, static file serving |
| `http.rs` | 9 | HEAD no body, POST body, content-type static, ETag 304, gzip compression, 413 body too large, cache-control, X-Forwarded-For, fallback to index.php |
| `php.rs` | 7 | `$_GET`, `$_SERVER` vars, `exit()` output, `http_response_code()`, `$_COOKIE`, `php://input`, custom response header |
| `phpinfo.rs` | 2 | PHP version matching, health check |
| `kv.rs` | 13 | set/get, TTL expiry, atomic incr, del/exists, incr_by, expire extends TTL, pttl, setnx, mset/mget, pttl missing key, empty values, large values (~10KB), overwrite |
| `errors.rs` | 3 | Fatal error 500, memory limit 500, syntax error 500 (all verify server recovery) |
| `security.rs` | 4 | Dotfile 403, PHP source not exposed, blocked_paths 403, path traversal blocked |
| `concurrency.rs` | 2 | 20 parallel PHP requests, atomic KV increments under load |
| `metrics.rs` | 9 | Prometheus format, build info, HTTP counters, handler labels, PHP execution metrics, in-flight gauge, body size histograms, metrics self-counting, status codes |
| `etag_cache.rs` | 6 | PHP ETag 200+header, matching ETag 304, mismatched ETag 200, POST bypass, no If-None-Match 200, independent query strings |
| `timeouts.rs` | 2 | PHP sleep exceeding timeout returns 504, server recovers after timeout |
| `http_edge.rs` | 4 | Percent-encoded paths, HEAD Content-Length + empty body, ~4KB query string, multiple query params |
| `php_extended.rs` | 5 | Empty PHP output 200, JSON content-type, multiple Set-Cookie headers, SERVER_SOFTWARE, PUT/DELETE methods |

### Cluster Tests

**Not yet implemented.** No cluster infrastructure (StatefulSet, headless service, gossip config) exists in the K8s manifests.

---

## PHP Fixtures (tests/docroot/)

| File | Purpose |
|------|---------|
| `index.php` | PHP version, SAPI name, "Hello from ePHPm" |
| `test.php` | Echoes `$_SERVER`, GET/POST/COOKIE params, request headers |
| `info.php` | `phpinfo()` — large output for compression testing |
| `exit_test.php` | `echo "bye"; exit(0);` — output before exit |
| `status_201.php` | `http_response_code(201)` — custom status |
| `server_test.php` | JSON dump of `$_SERVER` variables |
| `server_vars.php` | JSON dump of key `$_SERVER` variables |
| `custom_header.php` | `header('X-Custom: ok')` — custom response header |
| `error_test.php` | Undefined array key — non-fatal warning |
| `fatal_error.php` | Calls undefined function — fatal error |
| `memory_hog.php` | Allocates 100 MB with 2M limit — OOM fatal |
| `syntax_error.php` | `$x = ;` — parser error |
| `kv.php` | KV store router: set/get/del/exists/pttl/incr/incr_by/expire/setnx/mset/mget |
| `empty.php` | No output — empty response testing |
| `etag_test.php` | Sets `ETag` header via `header()` — PHP ETag cache testing |
| `json_response.php` | JSON `Content-Type` + `json_encode()` output |
| `multi_cookie.php` | Sets 3 `Set-Cookie` headers via `setcookie()` |
| `sleep.php` | `sleep(N)` via `?seconds=N` — timeout testing |
| `large_output.php` | ~1 MiB repeating output — body size / compression |
| `image.png` | 1x1 PNG (69 bytes) — binary content-type |
| `test.html` | Static HTML |
| `test.css` | Static CSS |
| `test.js` | Static JS |
| `.env` | Hidden file — blocked by security rules |
| `subdir/index.html` | Subdirectory index |
| `uploads/shell.php` | Allowlist test |
| `vendor/secret.php` | Blocked paths glob test |

---

## E2E Helpers (src/lib.rs)

One exported function:
- `required_env(name) -> String` — reads env var or panics

No cluster helpers, no poll_until, no optional_env.

---

## Infrastructure

| Component | File | Status |
|-----------|------|--------|
| Kind cluster | `k8s/kind-config.yaml` | Exists |
| Single-node deployment | `k8s/base/ephpm-single.yaml` | Exists (1 replica, port 8080, readiness probe) |
| E2E test job | `k8s/tests/e2e-job.yaml` | Exists (EPHPM_URL + EXPECTED_PHP_VERSION) |
| Tiltfile | `k8s/Tiltfile` | Exists (ci + dev modes) |
| Cluster StatefulSet | — | **Missing** |
| Cluster headless service | — | **Missing** |
| Per-pod services | — | **Missing** |
| Cluster env vars in e2e job | — | **Missing** |

---

## Feature Coverage Matrix

| Feature | Implemented | E2E Tested | Gap |
|---------|:-----------:|:----------:|:---:|
| HTTP/1.1 serving | Yes | Yes (9 tests) | — |
| HTTP/2 | Yes | **No** | Blocked — requires TLS; no certs in Kind env |
| TLS / HTTPS | Yes | **No** | Blocked — needs self-signed cert + CA trust in e2e pod |
| Static file serving | Yes | Yes (3 tests) | — |
| Request routing (fallback) | Yes | Yes (1 test) | — |
| Configuration (TOML + env vars) | Yes | Indirect | Medium |
| Embedded KV store (SAPI) | Yes | Yes (10 tests) | — |
| KV store CLI (`ephpm kv`) | Yes | **No** | Medium |
| PHP embedding (ZTS) | Yes | Yes (7+2 tests) | — |
| Compression (gzip) | Yes | Yes (1 test) | — |
| ETags / 304 (static) | Yes | Yes (1 test) | — |
| PHP ETag cache | Yes | Yes (6 tests) | — |
| Security (paths, dotfiles) | Yes | Yes (4 tests) | — |
| Sessions | Yes | **No** | Medium |
| Timeouts | Yes | Yes (2 tests) | — |
| PHP error recovery | Yes | Yes (3 tests) | — |
| Proxy headers | Yes | Yes (1 test) | Low |
| Graceful shutdown | Yes | **No** | Medium — needs kubectl |
| Concurrency / load | Yes | Yes (2 tests) | — |
| Cluster gossip | Yes | **No** | High — no cluster infra |
| Cluster KV replication | Yes | **No** | High — no cluster infra |
| Cluster resilience | Yes | **No** | High — no cluster infra |
| Observability (metrics) | Yes | Yes (9 tests) | — |
| Observability (tracing) | Partial | **No** | Low |
| CLI | Partial | **No** | Medium |
| ACME | Planned | — | — |
| DB proxy | Partial | — | — |
| Admin UI / API | Planned | — | — |

---

## Tests To Build

### High Priority — Missing coverage for implemented features

#### 1. Cluster Infrastructure + Discovery
Build the K8s resources and test cluster membership.

- [ ] Create `k8s/base/ephpm-cluster.yaml` (StatefulSet 3 replicas, ConfigMap, headless service, per-pod services)
- [ ] Add cluster env vars to `k8s/tests/e2e-job.yaml` (`EPHPM_CLUSTER_URL`, `EPHPM_CLUSTER_NODE{0,1,2}_URL`)
- [ ] Update `k8s/Tiltfile` to deploy cluster resources with dependency on single-node
- [ ] Add `optional_env()`, `cluster_url()`, `cluster_node_urls()`, `poll_until()` helpers to `src/lib.rs`
- [ ] Add `serde` + `serde_json` deps to Cargo.toml

**File: `cluster_discovery.rs`**
- [ ] All 3 nodes see full membership via `/api/nodes`
- [ ] Each node reports a unique ID
- [ ] `/api/nodes` response shape validation (JSON fields)
- [ ] `cluster_id` matches config value
- [ ] Gossip addresses are distinct across nodes
- [ ] All nodes report `alive` state

#### 2. Cluster KV Replication
- [ ] Small value (< 512B) replicates across all nodes via gossip
- [ ] Large value (> 512B) stays local to the node it was written on
- [ ] Gossip replication converges within 5s
- [ ] TTL expiry propagates to all nodes
- [ ] Delete propagates to all nodes
- [ ] Overwrite propagates new value
- [ ] Concurrent writes to different nodes don't conflict
- [ ] PHP `kv.php` routes through clustered store

#### 3. Cluster ETag Cache
- [ ] ETag cached on originating node
- [ ] ETag replicates to other nodes via gossip
- [ ] ETag mismatch on remote node returns 200

#### 4. Cluster Resilience (kubectl-gated)
- [ ] Node failure detected by remaining nodes
- [ ] KV data survives node loss
- [ ] Rejoining node receives gossip state
- [ ] Requests succeed during node failure

### Medium Priority — Gaps in single-node coverage

#### 5. PHP ETag Cache
- [ ] First PHP request returns 200 + ETag header
- [ ] Repeat request with `If-None-Match` returns 304
- [ ] Mismatched ETag returns 200
- [ ] Different query strings get different ETags
- [ ] POST requests are not cached
- [ ] No `If-None-Match` header returns 200

#### 6. Sessions
- [ ] Session persistence via Set-Cookie / Cookie round-trip
- [ ] Session isolation between different session IDs
- [ ] Session survives after PHP error
- [ ] New session created without cookie
- [ ] Invalid session ID handled gracefully

#### 7. Timeouts (**done** — `timeouts.rs`)
- [x] PHP `sleep.php?seconds=30` triggers 504 when server timeout is shorter
- [x] Server recovers and accepts new requests after timeout

#### 8. Graceful Shutdown (kubectl-gated)
- [ ] Server accepts requests before SIGTERM
- [ ] In-flight request completes during shutdown
- [ ] New connections refused after SIGTERM

#### 9. CLI (kubectl-gated)
- [ ] `ephpm --version` prints version string
- [ ] `ephpm --help` prints usage
- [ ] `ephpm serve --help` prints serve options
- [ ] Invalid flag returns error
- [ ] `ephpm kv --help` prints KV subcommand options

#### 10. HTTP Edge Cases (partially done — `http_edge.rs`)
- [x] Percent-encoded path resolves correctly
- [x] Multiple query parameters preserved
- [x] HEAD on static file returns Content-Length with empty body
- [ ] POST to static file returns 405
- [ ] Content-Length matches actual body length
- [ ] Duplicate headers handled
- [x] Very long query string (~4KB) accepted
- [ ] Empty User-Agent accepted
- [ ] Connection: close honored

#### 11. PHP Extended (partially done — `php_extended.rs`)
- [x] Multiple Set-Cookie headers preserved
- [x] Empty PHP response returns 200 with empty body
- [x] `Content-Type: application/json` on JSON response
- [x] `SERVER_SOFTWARE` contains "ephpm"
- [x] `REQUEST_METHOD` correct for GET/POST/PUT/DELETE
- [ ] Output after `header()` modification delivered correctly

#### 12. Additional Proxy Headers
- [ ] XFF trusted proxy sets REMOTE_ADDR
- [ ] X-Forwarded-Proto HTTPS
- [ ] X-Forwarded-Proto HTTP
- [ ] Multiple proxies — rightmost untrusted used
- [ ] No header preserves pod IP

### Lower Priority

#### 13. Configuration Edge Cases
- [ ] `EPHPM_SERVER__LISTEN` overrides TOML `[server] listen`
- [ ] `EPHPM_PHP__INI_OVERRIDES` JSON array parsed correctly
- [ ] Invalid config returns clear error
- [ ] Missing config uses defaults

#### 14. Observability
- [ ] Structured log output contains method, path, status, duration
- [ ] Log level filtering works

#### 15. Additional KV Tests (partially done — added to `kv.rs`)
- [x] Empty string values
- [x] Overwrite existing key
- [x] Large values (~10KB)
- [ ] Special characters in keys/values
- [ ] KV operations via CLI (`ephpm kv get/set/del`)

#### 16. Additional Concurrency / Performance
- [ ] 100 concurrent PHP requests all succeed
- [ ] Mixed static + PHP concurrent requests
- [ ] Sustained KV burst (50 concurrent ops)
- [ ] Request isolation (unique IDs survive concurrent load)

---

## Cluster E2E Infrastructure (To Build)

When cluster tests are implemented, the following resources are needed:

**`k8s/base/ephpm-cluster.yaml`:**
- ConfigMap with cluster-enabled `ephpm.toml` (cluster_id, gossip bind, join DNS, hot_key_threshold=3)
- StatefulSet (3 replicas, gossip port 7946 UDP)
- Headless Service for gossip peer DNS discovery
- Per-pod Services (ephpm-cluster-0, -1, -2) for targeting specific nodes
- ClusterIP Service for load-balanced access

**E2E job additions:**
```yaml
- name: EPHPM_CLUSTER_URL
  value: "http://ephpm-cluster:8080"
- name: EPHPM_CLUSTER_NODE0_URL
  value: "http://ephpm-cluster-0:8080"
- name: EPHPM_CLUSTER_NODE1_URL
  value: "http://ephpm-cluster-1:8080"
- name: EPHPM_CLUSTER_NODE2_URL
  value: "http://ephpm-cluster-2:8080"
```

**Tiltfile additions:**
```python
k8s_yaml("base/ephpm-cluster.yaml")
k8s_resource("ephpm-cluster", resource_deps=["ephpm"], objects=[...])
k8s_resource("ephpm-e2e", resource_deps=["ephpm", "ephpm-cluster"])
```

**Helper additions to `src/lib.rs`:**
- `optional_env(name) -> Option<String>`
- `cluster_url() -> Option<String>`
- `cluster_node_urls() -> Option<[String; 3]>`
- `poll_until(timeout, interval, check) -> bool`

**Cargo.toml additions:**
```toml
reqwest = { version = "0.12", default-features = false, features = ["json"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

---

## PHP Fixtures Needed

These fixtures need to be created in `tests/docroot/` for new tests:

| File | Purpose | Needed by |
|------|---------|-----------|
| `empty.php` | `<?php` with no output | PHP Extended (#11) |
| `json_response.php` | `Content-Type: application/json` + `json_encode()` | PHP Extended (#11) |
| `multi_cookie.php` | Two `setcookie()` calls | PHP Extended (#11) |
| `query.php` | Echo all `$_GET` as `key=value\n` | HTTP Edge Cases (#10) |
| `server_var.php` | Return single `$_SERVER[var]` via `?var=` | PHP Extended (#11) |
| `session.php` | `session_start()` + session read/write | Sessions (#6) |
| `timeout_test.php` | Alternative to `sleep.php` if needed | Timeouts (#7) |
