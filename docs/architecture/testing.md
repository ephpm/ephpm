# Testing Architecture

This document covers the end-to-end testing strategy, from single-node validation through multi-node cluster testing and high availability verification.

---

## Current State

### Test Layers

| Layer | Tool | Speed | What it covers |
|-------|------|-------|----------------|
| **Unit** | `cargo nextest` | Seconds | Config parsing, routing logic, static file serving, path traversal, glob matching |
| **Integration** | `cargo nextest` (ignored without libphp) | Seconds | PHP FFI calls, request/response mapping, superglobal population |
| **E2E** | `ephpm-e2e` crate in Kind cluster | Minutes | Full HTTP lifecycle: PHP execution, static serving, version/SAPI validation |
| **Benchmarks** | `cargo bench` | Minutes | Throughput measurement |

### Infrastructure

- **Kind** cluster (`ephpm-dev`) with single control-plane node
- **Tilt** orchestration for image build, deploy, and test execution
- **GitHub Actions** CI matrix: PHP 8.4 + 8.5 x Linux + macOS
- **xtask** commands: `e2e-install`, `e2e`, `e2e-up`, `e2e-down`

### Current E2E Coverage

The `ephpm-e2e` crate currently tests:
- `php_version_matches` — GET `/index.php`, verify HTTP 200, check PHP version string, verify embedded SAPI
- `health_check` — GET `/`, verify success

The `scripts/e2e-test.sh` bash script tests:
- PHP execution (index.php, info.php)
- GET parameters (query string)
- POST parameters
- Content-Type header validation
- 404 response for missing files

### Gaps

The current E2E suite is minimal. It validates that PHP runs and serves responses, but doesn't cover most of the HTTP server features we've built. The sections below define comprehensive test plans for single-node and multi-node scenarios.

---

## Single-Node Test Plan

These tests run against a single ephpm instance in Kind. They should all be implemented in the `ephpm-e2e` crate as async Rust tests using `reqwest`.

### PHP Execution

| Test | Method | Validates |
|------|--------|-----------|
| `php_hello_world` | GET `/index.php` | 200, response body contains greeting |
| `php_version` | GET `/index.php` | Response contains expected PHP version |
| `php_sapi_name` | GET `/index.php` | SAPI is `embed` |
| `phpinfo_renders` | GET `/info.php` | 200, body contains `<html`, `phpinfo()` output |
| `php_get_params` | GET `/test.php?foo=bar&baz=123` | `$_GET['foo'] == 'bar'`, `$_GET['baz'] == '123'` |
| `php_post_form` | POST `/test.php` (form-urlencoded) | `$_POST` contains submitted values |
| `php_post_json` | POST `/test.php` (application/json) | `php://input` contains raw JSON body |
| `php_post_multipart` | POST `/test.php` (multipart/form-data) | `$_FILES` populated, `$_POST` fields present |
| `php_cookies` | GET `/test.php` with `Cookie: foo=bar` | `$_COOKIE['foo'] == 'bar'` |
| `php_request_uri_preserved` | GET `/some/path?q=1` (via fallback to index.php) | `$_SERVER['REQUEST_URI'] == '/some/path?q=1'` |
| `php_script_name_after_rewrite` | GET `/blog/hello` (fallback rewrite) | `$_SERVER['SCRIPT_NAME'] == '/index.php'` |
| `php_content_type_header` | GET `/test.php` | `Content-Type` set by PHP script |
| `php_custom_status_code` | GET (script calls `http_response_code(404)`) | Response status is 404 |
| `php_custom_headers` | GET (script calls `header('X-Custom: value')`) | Response has `X-Custom: value` |
| `php_large_output` | GET (script outputs >1MB) | Full body received, Content-Length correct |
| `php_exit_with_output` | GET (script calls `echo 'hello'; exit;`) | 200, body is `hello` |
| `php_error_handling` | GET (script triggers E_WARNING) | Server doesn't crash, response returned |

### Static File Serving

| Test | Method | Validates |
|------|--------|-----------|
| `static_html` | GET `/test.html` | 200, `Content-Type: text/html`, body matches file |
| `static_css` | GET `/style.css` | 200, `Content-Type: text/css` |
| `static_js` | GET `/app.js` | 200, `Content-Type: application/javascript` |
| `static_image` | GET `/image.png` | 200, `Content-Type: image/png`, binary body matches |
| `static_content_length` | GET `/test.html` | `Content-Length` header matches file size |
| `static_unknown_extension` | GET `/data.xyz` | 200, `Content-Type: application/octet-stream` |
| `static_missing_file` | GET `/nonexistent.txt` | 404 |
| `static_nested_path` | GET `/subdir/file.html` | 200, correct body |

### ETag and Caching

| Test | Method | Validates |
|------|--------|-----------|
| `etag_present` | GET `/test.html` | Response has `ETag` header (weak format `W/"..."`) |
| `etag_304_on_match` | GET `/test.html` with `If-None-Match: <etag>` | 304 Not Modified, empty body |
| `etag_200_on_mismatch` | GET `/test.html` with `If-None-Match: "wrong"` | 200, full body |
| `etag_star_matches` | GET `/test.html` with `If-None-Match: *` | 304 |
| `etag_comma_list` | GET `/test.html` with `If-None-Match: "a", <real>, "b"` | 304 |
| `etag_consistent` | GET `/test.html` twice | Same ETag both times |
| `cache_control_header` | GET `/test.html` (with `cache_control` configured) | `Cache-Control` header present |

### Compression

| Test | Method | Validates |
|------|--------|-----------|
| `gzip_html_response` | GET `/test.html` with `Accept-Encoding: gzip` | `Content-Encoding: gzip`, body decompresses to original |
| `gzip_php_response` | GET `/info.php` with `Accept-Encoding: gzip` | `Content-Encoding: gzip` on large phpinfo output |
| `no_gzip_without_header` | GET `/test.html` (no Accept-Encoding) | No `Content-Encoding` header |
| `no_gzip_small_body` | GET small file with `Accept-Encoding: gzip` | No compression (below min size) |
| `no_gzip_image` | GET `/image.png` with `Accept-Encoding: gzip` | No compression (non-compressible type) |
| `vary_header_present` | GET with `Accept-Encoding: gzip` | `Vary: Accept-Encoding` header |

### Security

| Test | Method | Validates |
|------|--------|-----------|
| `dotfile_blocked` | GET `/.env` | 403 Forbidden |
| `dotdir_blocked` | GET `/.git/config` | 403 Forbidden |
| `htaccess_blocked` | GET `/.htaccess` | 403 Forbidden |
| `path_traversal_blocked` | GET `/../../../etc/passwd` | 403 or 404 |
| `blocked_path_exact` | GET `/wp-config.php` (when in blocked_paths) | 403 |
| `blocked_path_wildcard` | GET `/vendor/autoload.php` (when `/vendor/*` blocked) | 403 |
| `php_allowlist_blocks` | GET `/uploads/shell.php` (when allowed_php_paths set) | 403 |
| `php_allowlist_allows` | GET `/index.php` (in allowed_php_paths) | 200 |
| `body_size_limit` | POST with body exceeding `max_body_size` | 413 Payload Too Large |
| `trusted_host_valid` | GET with `Host: allowed.example.com` | 200 |
| `trusted_host_invalid` | GET with `Host: evil.example.com` | 421 Misdirected Request |
| `trusted_host_with_port` | GET with `Host: allowed.example.com:8080` | 200 (port stripped for comparison) |

### Custom Response Headers

| Test | Method | Validates |
|------|--------|-----------|
| `custom_header_static` | GET `/test.html` | Configured custom headers present |
| `custom_header_php` | GET `/index.php` | Configured custom headers present on PHP responses |
| `hsts_header` | GET any page | `Strict-Transport-Security` header if configured |
| `cors_headers` | GET any page | `Access-Control-Allow-Origin` etc. if configured |

### Fallback / URL Resolution

| Test | Method | Validates |
|------|--------|-----------|
| `uri_literal_file` | GET `/test.html` | Serves static file directly |
| `uri_directory_index` | GET `/` | Resolves to `/index.php` via index_files |
| `uri_subdirectory_index` | GET `/subdir/` | Resolves to `/subdir/index.html` |
| `fallback_to_index_php` | GET `/nonexistent/path` | Falls through to `/index.php` |
| `fallback_preserves_query` | GET `/path?key=val` | Fallback to `/index.php?key=val` |
| `fallback_404_config` | GET `/missing` (with `=404` fallback) | 404 Not Found |

### Trusted Proxies

| Test | Method | Validates |
|------|--------|-----------|
| `xff_trusted_proxy` | GET with `X-Forwarded-For` from trusted IP | `$_SERVER['REMOTE_ADDR']` is the client IP from XFF |
| `xff_untrusted_proxy` | GET with `X-Forwarded-For` from untrusted IP | `$_SERVER['REMOTE_ADDR']` is the connecting IP (XFF ignored) |
| `xfp_https_detection` | GET with `X-Forwarded-Proto: https` from trusted proxy | `$_SERVER['HTTPS'] == 'on'` |

### TLS (Manual Certs)

| Test | Method | Validates |
|------|--------|-----------|
| `tls_serves_https` | HTTPS GET `/index.php` | 200, valid TLS handshake |
| `tls_redirect_http` | HTTP GET (with `redirect_http = true`) | 301 redirect to HTTPS |
| `tls_server_var` | HTTPS GET `/test.php` | `$_SERVER['HTTPS'] == 'on'`, `$_SERVER['SERVER_PORT'] == '443'` |
| `tls_invalid_cert_rejected` | HTTPS GET with strict client | Handshake fails if cert doesn't match |

### Timeouts and Limits

| Test | Method | Validates |
|------|--------|-----------|
| `request_timeout` | GET (PHP script sleeps beyond `server.timeouts.request`) | Connection closed or 504 |
| `idle_timeout` | Open connection, send nothing for > idle timeout | Connection closed |
| `max_header_size` | Send request with oversized headers | 431 or connection closed |

### Graceful Shutdown

| Test | Method | Validates |
|------|--------|-----------|
| `inflight_request_completes` | Start slow PHP request, send SIGTERM | Response received before shutdown |
| `new_requests_rejected` | Send SIGTERM, then new request | Connection refused or 503 |
| `readiness_probe_fails` | Send SIGTERM | Kubernetes readiness probe fails, pod removed from service |

---

## Single-Node Test Infrastructure

### Test Config Variants

Different features need different ephpm.toml configurations. Use Kubernetes ConfigMaps to inject test-specific configs:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: ephpm-security-test-config
data:
  ephpm.toml: |
    [server]
    listen = "0.0.0.0:8080"
    document_root = "/var/www/html"

    [server.security]
    blocked_paths = ["/wp-config.php", "/vendor/*"]
    allowed_php_paths = ["/index.php", "/test.php", "/info.php"]
    trusted_proxies = ["10.0.0.0/8"]

    [server.request]
    max_body_size = 1024
    trusted_hosts = ["ephpm", "ephpm.default.svc.cluster.local"]

    [server.response]
    headers = [
        ["X-Frame-Options", "DENY"],
        ["Strict-Transport-Security", "max-age=63072000"],
    ]
```

### Test Docroot Fixtures

Expand the test docroot with purpose-built PHP scripts:

```
tests/docroot/
  index.php          # greeting + version + SAPI (existing)
  info.php           # phpinfo() (existing)
  test.php           # server vars dump (existing)
  test.html          # static file (existing)
  style.css          # CSS MIME type test
  app.js             # JS MIME type test
  image.png          # binary static file test
  large_output.php   # outputs >1MB for compression/body tests
  custom_status.php  # http_response_code(404)
  custom_headers.php # header('X-Custom: value')
  exit_test.php      # echo 'hello'; exit;
  sleep.php          # sleep($seconds) for timeout tests
  error_test.php     # triggers E_WARNING
  post_echo.php      # echoes $_POST, $_FILES, php://input
  cookie_echo.php    # echoes $_COOKIE
  server_vars.php    # JSON dump of $_SERVER for precise assertions
  subdir/
    index.html       # directory index test
```

### E2E Crate Structure

Organize tests by feature area using Rust test modules:

```
crates/ephpm-e2e/
  src/
    lib.rs           # shared helpers (HTTP client, assertions, env vars)
  tests/
    php_execution.rs  # PHP lifecycle tests
    static_files.rs   # static serving + MIME types
    etag.rs           # ETag + 304 tests
    compression.rs    # gzip tests
    security.rs       # dotfiles, blocked paths, allowlist, body limits
    fallback.rs       # URL resolution / try_files
    headers.rs        # custom response headers, trusted hosts
    proxy.rs          # X-Forwarded-For, X-Forwarded-Proto
    tls.rs            # HTTPS, redirects, $_SERVER['HTTPS']
    timeouts.rs       # request timeout, idle timeout, header size
    shutdown.rs       # graceful shutdown behavior
```

Each test file reads `EPHPM_URL` from the environment and issues HTTP requests. Tests that need specific config (security, TLS) use separate ephpm deployments with their own ConfigMaps.

### Tilt Orchestration

Extend the Tiltfile to manage multiple ephpm deployments with different configs:

```python
# Default ephpm instance (standard config)
k8s_yaml('k8s/base/ephpm-single.yaml')

# Security-focused instance (blocked paths, allowlist, trusted hosts)
k8s_yaml('k8s/tests/ephpm-security.yaml')

# TLS instance (manual certs)
k8s_yaml('k8s/tests/ephpm-tls.yaml')

# Timeout instance (short timeouts for testing)
k8s_yaml('k8s/tests/ephpm-timeouts.yaml')
```

E2E tests target the appropriate instance via different `EPHPM_URL` env vars.

---

## Multi-Node Cluster Test Plan

These tests validate the KV store, gossip protocol, and clustering features. They require multiple ephpm instances running simultaneously.

### Infrastructure

```yaml
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: ephpm-cluster
spec:
  serviceName: ephpm-headless
  replicas: 3
  template:
    spec:
      containers:
        - name: ephpm
          env:
            - name: EPHPM_CLUSTER__ENABLED
              value: "true"
            - name: EPHPM_CLUSTER__JOIN
              value: "ephpm-headless.default.svc.cluster.local"
            - name: EPHPM_CLUSTER__SECRET
              valueFrom:
                secretKeyRef:
                  name: ephpm-cluster
                  key: secret
---
apiVersion: v1
kind: Service
metadata:
  name: ephpm-headless
spec:
  clusterIP: None
  selector:
    app: ephpm-cluster
  ports:
    - name: http
      port: 8080
    - name: gossip
      port: 7946
      protocol: UDP
    - name: data
      port: 7947
---
# Load-balanced service for client-facing requests
apiVersion: v1
kind: Service
metadata:
  name: ephpm-cluster-lb
spec:
  selector:
    app: ephpm-cluster
  ports:
    - name: http
      port: 8080
```

### Cluster Formation

| Test | Method | Validates |
|------|--------|-----------|
| `cluster_forms` | Query Node API on all 3 pods | All pods see 3 members |
| `cluster_gossip_metadata` | Query `/api/kv/cluster` on each pod | Each pod reports memory usage, key count, health for all peers |
| `cluster_node_identity` | Query each pod | Each reports a unique node ID |
| `cluster_join_time` | Measure time from pod ready to cluster membership | Joins within 5 seconds |

### KV Store — Single Node

| Test | Method | Validates |
|------|--------|-----------|
| `kv_set_get` | PHP: `ephpm_kv_set("key", "val")` then `ephpm_kv_get("key")` | Value returned correctly |
| `kv_del` | Set, delete, get | Returns null after delete |
| `kv_ttl_expiry` | Set with TTL=2s, wait 3s, get | Returns null (expired) |
| `kv_ttl_not_expired` | Set with TTL=60s, get immediately | Returns value |
| `kv_overwrite` | Set key twice with different values | Second value returned |
| `kv_hash_operations` | HSET, HGET, HGETALL, HDEL | All hash operations work |
| `kv_incr_decr` | SET to "10", INCR, DECR | Correct arithmetic |
| `kv_large_value` | Set 1MB value, get | Full value returned |
| `kv_binary_value` | Set value with null bytes, get | Binary-safe storage |

### KV Store — RESP Protocol

| Test | Method | Validates |
|------|--------|-----------|
| `resp_ping` | Redis client PING | Returns PONG |
| `resp_set_get` | Redis client SET/GET | Round-trip works |
| `resp_del` | Redis client DEL | Key removed |
| `resp_expire_ttl` | Redis client EXPIRE/TTL | TTL counts down |
| `resp_mget_mset` | Redis client MGET/MSET | Batch operations work |
| `resp_incr` | Redis client INCR/DECR | Atomic counters |
| `resp_hash_ops` | Redis client HSET/HGET/HGETALL | Hash operations via RESP |
| `resp_unknown_command` | Redis client sends unsupported command | Returns ERR, doesn't crash |
| `resp_predis_compat` | Laravel app using predis/predis | Cache and session drivers work |
| `resp_phpredis_compat` | Laravel app using phpredis extension | Cache and session drivers work |

### KV Store — Cross-Node (Clustered)

| Test | Method | Validates |
|------|--------|-----------|
| `kv_write_read_same_node` | Set on pod-0, get on pod-0 | Fast path works |
| `kv_write_read_different_node` | Set on pod-0, get on pod-1 | Cross-node routing works |
| `kv_hash_ring_routing` | Set many keys, check distribution via Node API | Keys distributed across all nodes (not all on one) |
| `kv_replication_exists` | Set on pod-0, check replica count via Node API | Key replicated to N additional nodes |
| `kv_ttl_cross_node` | Set with TTL on pod-0, wait, get on pod-1 | TTL respected across nodes |
| `kv_large_keyspace` | Set 10,000 keys via load balancer | Even distribution across nodes (±20%) |

### Session Continuity (Clustered)

| Test | Method | Validates |
|------|--------|-----------|
| `session_create` | POST login to pod-0 | Session cookie returned |
| `session_read_same_node` | GET with session cookie to pod-0 | Session data present |
| `session_read_other_node` | GET with session cookie to pod-1 | Session data present (cross-node) |
| `session_update_propagates` | Update session on pod-0, read on pod-2 | Updated value visible |
| `session_expiry` | Create session, wait beyond `gc_maxlifetime` | Session expired on all nodes |

---

## High Availability Tests

These tests deliberately break things to verify the cluster recovers correctly.

### Node Failure

| Test | Method | Validates |
|------|--------|-----------|
| `node_crash_cluster_continues` | Kill pod-1, query pod-0 and pod-2 | Remaining nodes report 2 members, continue serving |
| `node_crash_keys_available` | Kill pod-1, read keys that were on pod-1 | Replicas serve the keys (no data loss) |
| `node_crash_detection_time` | Kill pod-1, measure time until other nodes detect failure | Detected within gossip failure timeout (~10-30s) |
| `node_crash_sessions_survive` | Kill pod-1 with active sessions | Sessions accessible via surviving nodes |
| `node_rejoin_rebalance` | Kill pod-1, restart it | Pod-1 rejoins cluster, keys rebalanced back |

### Rolling Restart

| Test | Method | Validates |
|------|--------|-----------|
| `rolling_restart_no_downtime` | Restart pods one at a time | HTTP requests succeed continuously (zero failed requests) |
| `rolling_restart_sessions_persist` | Create session, rolling restart all pods | Session still accessible after all pods restarted |
| `rolling_restart_kv_intact` | Set 1000 keys, rolling restart all pods | All keys still accessible after restart |

### Scale Up / Down

| Test | Method | Validates |
|------|--------|-----------|
| `scale_up_joins` | Scale from 3 to 5 replicas | New pods join cluster, all 5 visible in gossip |
| `scale_up_rebalances` | Scale up, check key distribution | Keys redistributed to include new nodes |
| `scale_down_graceful` | Scale from 5 to 3 replicas | Departing pods transfer keys before shutdown |
| `scale_down_keys_intact` | Scale down, verify all keys | No data loss after scale-down |

### Network Partition (Advanced)

These tests require network policy manipulation to simulate partitions:

| Test | Method | Validates |
|------|--------|-----------|
| `partition_both_sides_serve` | Isolate pod-0 from pod-1,2 | Both partitions continue serving local keys |
| `partition_heal_reconcile` | Create partition, write to both sides, heal | LWW conflict resolution merges correctly |
| `partition_no_split_brain_writes` | Partition, write same key on both sides, heal | One value wins (deterministic) |

### ACME Certificate HA (Clustered)

| Test | Method | Validates |
|------|--------|-----------|
| `acme_single_issuer` | 3 nodes, trigger cert issuance | Only one node contacts Let's Encrypt (check logs) |
| `acme_leader_failover` | Kill ACME leader node | New leader elected, takes over renewal duties |
| `acme_cert_propagation` | Issue cert on one node | All nodes serve HTTPS with the cert (check via TLS handshake to each pod) |
| `acme_challenge_any_node` | Initiate ACME on pod-0 | Challenge token servable from pod-1 and pod-2 |

---

## PHP Response Cache HA

| Test | Method | Validates |
|------|--------|-----------|
| `cache_miss_executes_php` | GET `/blog/hello` (first time) | PHP executes, response cached in KV |
| `cache_hit_skips_php` | GET `/blog/hello` with matching `If-None-Match` | 304, no PHP execution (verify via access log or metric) |
| `cache_hit_any_node` | Cache on pod-0, request with ETag to pod-1 | 304 from pod-1 (cache replicated) |
| `cache_invalidation` | Purge cache entry, request again | PHP re-executes, new ETag generated |
| `cache_bypass_auth` | GET with auth cookie | Cache bypassed, PHP always executes |

---

## Test Infrastructure for Cluster Tests

### Kind Cluster Configuration

Multi-node Kind cluster for realistic HA testing:

```yaml
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
  - role: control-plane
  - role: worker
  - role: worker
  - role: worker
```

Spreading ephpm pods across workers via pod anti-affinity ensures node failure tests are meaningful (killing a Kind worker takes down the ephpm pod on it).

### Network Policy for Partition Tests

```yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: partition-pod-0
spec:
  podSelector:
    matchLabels:
      statefulset.kubernetes.io/pod-name: ephpm-cluster-0
  policyTypes:
    - Ingress
    - Egress
  ingress: []   # block all inbound from other pods
  egress: []    # block all outbound to other pods
```

Apply to simulate partition, delete to heal. The e2e test applies/removes these policies programmatically via the Kubernetes API.

### Test Execution Phases

Cluster tests must run in a specific order since some tests are destructive:

```
Phase 1: Cluster formation (non-destructive)
  → cluster_forms, cluster_gossip_metadata, cluster_node_identity

Phase 2: KV operations (non-destructive)
  → kv_set_get, kv_cross_node, kv_hash_ring, kv_sessions

Phase 3: HA — node failure (destructive, pods restarted)
  → node_crash_*, rolling_restart_*

Phase 4: HA — scaling (destructive, replica count changes)
  → scale_up_*, scale_down_*

Phase 5: HA — network partition (destructive, network policies)
  → partition_*
```

Each phase waits for the cluster to be fully healthy before proceeding.

### E2E Crate — Cluster Test Modules

```
crates/ephpm-e2e/
  tests/
    # Single-node (existing + expanded)
    php_execution.rs
    static_files.rs
    etag.rs
    compression.rs
    security.rs
    fallback.rs
    headers.rs
    proxy.rs
    tls.rs
    timeouts.rs
    shutdown.rs

    # Cluster tests (new)
    cluster_formation.rs    # gossip, membership, metadata
    kv_single_node.rs       # local KV operations
    kv_resp.rs              # Redis protocol compatibility
    kv_cross_node.rs        # cross-node routing + replication
    kv_sessions.rs          # PHP session storage
    ha_node_failure.rs      # pod crash + recovery
    ha_rolling_restart.rs   # zero-downtime restarts
    ha_scaling.rs           # scale up/down
    ha_partition.rs         # network partition + heal
    ha_acme.rs              # certificate HA
    cache_response.rs       # PHP response cache
```

### Metrics and Assertions

Cluster tests need richer assertions than simple HTTP status checks. The Node API provides the data:

```rust
/// Query the Node API for cluster state.
async fn cluster_state(pod_url: &str) -> ClusterState {
    let resp = reqwest::get(format!("{pod_url}/api/kv/cluster")).await.unwrap();
    resp.json::<ClusterState>().await.unwrap()
}

/// Assert all pods see the expected number of cluster members.
async fn assert_cluster_size(pods: &[&str], expected: usize) {
    for pod in pods {
        let state = cluster_state(pod).await;
        assert_eq!(state.members.len(), expected, "pod {pod} sees wrong member count");
    }
}

/// Assert a key is accessible from a specific pod.
async fn assert_key_readable(pod_url: &str, key: &str, expected_value: &str) {
    let resp = reqwest::get(format!("{pod_url}/test-kv-get.php?key={key}")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), expected_value);
}

/// Wait for cluster to reach target size with timeout.
async fn wait_for_cluster(pods: &[&str], target_size: usize, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let mut all_ready = true;
        for pod in pods {
            let state = cluster_state(pod).await;
            if state.members.len() != target_size {
                all_ready = false;
                break;
            }
        }
        if all_ready { return; }
        assert!(Instant::now() < deadline, "cluster did not reach size {target_size} within {timeout:?}");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
```

### CI Pipeline

```yaml
# .github/workflows/e2e.yml (extended)
jobs:
  e2e-single:
    strategy:
      matrix:
        php: ["8.4", "8.5"]
    steps:
      - uses: actions/checkout@v4
      - run: cargo xtask e2e-install
      - run: cargo xtask e2e --php-version ${{ matrix.php }} --suite single

  e2e-cluster:
    needs: e2e-single
    strategy:
      matrix:
        php: ["8.5"]   # cluster tests on latest PHP only
    steps:
      - uses: actions/checkout@v4
      - run: cargo xtask e2e-install
      - run: cargo xtask e2e --php-version ${{ matrix.php }} --suite cluster --workers 3
```

Cluster tests only run on the latest PHP version to keep CI time reasonable. Single-node tests run on the full PHP matrix.

---

## Implementation Order

| Phase | Scope | Priority |
|-------|-------|----------|
| **1. Expand single-node E2E** | PHP execution, static files, security, fallback, compression, ETag, headers, timeouts | Now — validates everything we've already built |
| **2. Test docroot fixtures** | Add missing PHP scripts and static files | Now — required for Phase 1 |
| **3. Multi-config Tilt setup** | Multiple ephpm deployments with different configs | Now — needed for security/TLS/timeout tests |
| **4. TLS E2E** | Manual cert tests in Kind | After TLS is stable |
| **5. KV single-node tests** | SAPI functions, RESP protocol, sessions | After KV Phase 1-4 |
| **6. Cluster formation tests** | Gossip, membership, Node API | After clustering Phase 5-6 |
| **7. KV cross-node tests** | Routing, replication, cross-node reads | After clustering Phase 7 |
| **8. HA tests** | Node failure, rolling restart, scaling | After Phase 7 |
| **9. Network partition tests** | Split-brain, reconciliation | After Phase 8 |
| **10. ACME HA tests** | Cert coordination across nodes | After ACME on KV |
| **11. Response cache tests** | ETag interception, cache hit/miss | After PHP response cache |
