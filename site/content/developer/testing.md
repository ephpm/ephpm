# Testing Strategy

ePHPm uses a layered testing approach: fast unit tests for inner logic, a dedicated Rust E2E crate (`ephpm-e2e`) for integration assertions, and Tilt + Kind for orchestrating real infrastructure.

---

## Test Layers

| Layer | Tool | What it tests | Speed |
|-------|------|---------------|-------|
| Unit | `cargo nextest` | Config parsing, routing logic, SAPI mapping, response building | Seconds |
| Integration | `cargo nextest` (ignored by default) | PHP execution, WordPress lifecycle — requires libphp | Seconds (with SDK) |
| Local e2e | `cargo test -p ephpm --test <name>` | Real binary spawned as a child against the loopback listener — vhost routing, HTTP correctness | Sub-second per test |
| E2E (cluster) | `ephpm-e2e` crate + Tilt + Kind | Full stack against real K8s infrastructure, pod-to-pod networking | Minutes |
| Benchmarks | Criterion | Throughput, latency p99 — requires libphp | Minutes |

---

## Unit & Integration Tests

Run locally, no infrastructure needed (stub mode):

```bash
cargo nextest run --workspace                    # all unit tests
cargo nextest run -p ephpm-server                # single crate
cargo nextest run -p ephpm-server test_routing   # single test
```

Integration tests that require PHP are `#[ignore]` by default. Run them after building with `cargo xtask release`:

```bash
cargo nextest run --workspace --run-ignored all
```

---

## Virtual Host Testing (`*.localhost`)

ePHPm supports multi-tenant hosting: a `sites_dir` containing one subdirectory per virtual host, and incoming requests are routed by `Host` header to the matching directory's document root. This is wired in `crates/ephpm-server/src/router.rs::resolve_site` and exercised by both the Kind e2e suite and a fast local-process test.

### How routing works

Two paths populate the vhost registry:

- **Startup scan** (`scan_sites_dir`) — at server boot, every immediate subdirectory of `sites_dir` becomes a registered vhost keyed by the lowercased directory name.
- **Lazy filesystem fallback** — when a request arrives for an unknown `Host`, the router checks whether `<sites_dir>/<host>` exists on disk and serves from it if so. New sites appear without restarting.

When `server.sites_domain_suffix` is set (e.g. `.localhost`), the router strips that suffix from the cleaned `Host` value before both the registry lookup and the lazy check. That lets developers keep short directory names (`~/sites/blog/`) while their browser hits `http://blog.localhost:8080`. Hosts without the suffix (`Host: blog` directly) still resolve via the bare key.

If nothing matches, the request falls through to `server.document_root`.

### Dev-mode workflow with `*.localhost`

`ephpm dev --sites <DIR>` enables the suffix-stripping path so testing in a browser is friction-free. Per RFC 6761, **every** subdomain of `localhost` already resolves to 127.0.0.1 — no `/etc/hosts` edit, no DNS, no elevation. Chrome (since 2018), Firefox 65+, Safari, and curl all honor this.

```bash
$ mkdir -p ~/sites/{blog,shop,wiki}
$ echo '<h1>blog</h1>' > ~/sites/blog/index.html
$ echo '<h1>shop</h1>' > ~/sites/shop/index.html
$ ephpm dev --sites ~/sites
  ePHPm 0.1.0 — dev server
    sites:    /home/luther/sites
    routing:
              http://blog.localhost:8080  →  blog/
              http://shop.localhost:8080  →  shop/
              http://wiki.localhost:8080  →  wiki/
              http://localhost:8080       →  document_root fallback
    fallback: /home/luther/sites
    php:      8.5.2
    press ctrl+c to stop
```

A subdirectory created after startup is picked up by the lazy fallback on the next matching request — no restart needed:

```bash
$ mkdir ~/sites/admin && echo '<h1>admin</h1>' > ~/sites/admin/index.html
$ curl http://admin.localhost:8080/         # served from sites/admin/ immediately
```

### Local-process test (`vhost_routing`)

`crates/ephpm/tests/vhost_routing.rs` covers the same behavior in CI without needing Kind. It:

1. Builds a `tempfile::tempdir()` with `blog/`, `shop/`, `wiki/` subdirs.
2. Spawns `target/release/ephpm dev --sites <tempdir> --port <picked>` as a child process.
3. Drains stdout + stderr in threads so the piped child doesn't back-pressure (banner goes to stdout, tracing to stdout too — both must be read).
4. Waits for the `HTTP listening` log line before issuing requests.
5. Hits the loopback listener with custom `Host:` headers and asserts the served body matches the per-site `index.html`.
6. Adds a directory mid-test to confirm lazy discovery works.
7. A `Drop` guard kills the child even on panic so the listener doesn't leak.

```bash
cargo test -p ephpm --test vhost_routing --release -- --nocapture
```

Runs in well under a second on a warm cache. Use this as the template for any future local-process e2e test — same shape, different assertions.

### Kind counterpart (`vhosts.rs`)

`crates/ephpm-e2e/tests/vhosts.rs` exercises the same logic against a pod-deployed ephpm with `EPHPM_SITES_DIR` mounted from a hostPath that the test runner Job can write to. It's slower (it pays the Kind/Tilt orchestration cost) but verifies that the routing also works through K8s service DNS and that the multi-tenant `security_p0` policies (open_basedir, disable_functions, RESP auth) compose correctly with vhost selection.

**Rule of thumb**: prefer the local test for routing correctness assertions; keep the Kind path for the small set of assertions that genuinely need pod-to-pod networking or the in-pod filesystem layout. Don't duplicate — if a property is covered locally, the Kind test should focus on cluster-specific behavior, not re-assert the same routing logic.

---

## E2E Testing: ephpm-e2e Crate

E2E tests live in a dedicated Rust crate (`crates/ephpm-e2e/`) that runs inside a Kind cluster. The crate is **excluded from the workspace** — it has different dependencies and is only built inside the E2E test runner container.

### Current Tests

All tests read `EPHPM_URL` from the environment. `phpinfo.rs` additionally reads `EXPECTED_PHP_VERSION`.

**`tests/basic.rs`** — core request lifecycle:
- `missing_file_returns_404`
- `php_renders_correctly`
- `static_file_serving`

**`tests/phpinfo.rs`** — PHP version and SAPI identity:
- `php_version_matches` — GETs `/index.php`, checks `PHP Version: X.Y`, confirms `Server API: ephpm`
- `health_check` — GETs `/`, asserts success

**`tests/http.rs`** — HTTP protocol correctness:
- `head_request_has_no_body` — HEAD returns same headers as GET but empty body
- `post_body_reaches_php` — form POST reaches `$_POST`
- `content_type_for_static_files` — `.css` → `text/css`, `.js` → `application/javascript`
- `etag_304_not_modified` — ETag round-trip returns 304 with empty body
- `gzip_response_is_compressed` — `Accept-Encoding: gzip` triggers `Content-Encoding: gzip`
- `request_body_too_large_returns_413` — body > `max_body_size` returns 413
- `cache_control_present_on_static_files` — `Cache-Control` header present on static files
- `x_forwarded_for_header_reaches_php` — `X-Forwarded-For` appears as `HTTP_X_FORWARDED_FOR` in `$_SERVER`
- `fallback_chain_serves_index_php` — GET `/` resolves via fallback chain to `index.php`

**`tests/php.rs`** — PHP execution correctness:
- `query_string_available` — `$_GET` populated from query string
- `server_vars_populated` — `REQUEST_METHOD`, `REQUEST_URI`, `DOCUMENT_ROOT`, `REMOTE_ADDR` set
- `php_exit_returns_output` — output before `exit(0)` delivered to client
- `php_sets_custom_status` — `http_response_code(201)` propagates to HTTP status line
- `cookie_header_populates_cookie_superglobal` — `Cookie:` header reaches `$_COOKIE`
- `php_input_stream_readable` — `php://input` contains raw body for non-form POST
- `custom_response_header_reaches_client` — PHP `header()` appears in HTTP response

**`tests/errors.rs`** — PHP error recovery (zend_try/zend_catch correctness):
- `php_fatal_error_returns_500` — undefined function call → 500, server continues
- `php_memory_limit_exceeded_returns_500` — OOM → 500, server continues
- `php_syntax_error_returns_500` — parse error → 500, server continues

**`tests/kv.rs`** — KV store PHP native functions:
- `kv_set_get_roundtrip`, `kv_ttl_expiry`, `kv_incr_atomic`, `kv_del_and_exists`
- `kv_pttl_returns_minus_two_for_missing`, `kv_pttl_positive_for_live_key`
- `kv_incr_by_delta`, `kv_expire_extends_ttl`
- `kv_setnx_does_not_overwrite`, `kv_mset_mget_roundtrip`

**`tests/concurrency.rs`** — correctness under concurrent load:
- `concurrent_php_requests_all_succeed` — 20 parallel GETs all return correct output
- `concurrent_kv_increments_are_consistent` — 20 concurrent increments yield unique values 1–20

**`tests/security.rs`** — path and access controls:
- `dotfile_returns_403` — `/.env` returns 403
- `php_source_not_exposed` — `.php` response never contains `<?php`
- `blocked_path_pattern_returns_403` — `vendor/*` glob returns 403
- `path_traversal_is_blocked` — URL-encoded `%2e%2e` sequences don't escape docroot

### PHP Version Flow

The PHP version flows through the entire pipeline:

```
GHA matrix (php: "8.4")
  → cargo xtask e2e --php-version 8.4
    → podman build --build-arg PHP_VERSION=8.4  (Dockerfile)
    → EXPECTED_PHP_VERSION=8.4 tilt ci
      → Tiltfile replaces __EXPECTED_PHP_VERSION__ in e2e-job.yaml
        → E2E Job container env: EXPECTED_PHP_VERSION=8.4
          → Rust test asserts body contains "PHP Version: 8.4"
```

### Crate Structure

```
crates/ephpm-e2e/
├── Cargo.toml          # reqwest + tokio (no TLS needed in-cluster)
├── src/
│   └── lib.rs          # Shared helpers (required_env)
└── tests/
    └── phpinfo.rs      # PHP version + SAPI validation
```

---

## Tilt + Kind Orchestration

### Prerequisites

**Podman** or **Docker** is required — Kind needs a container runtime.

For kind, tilt, and kubectl, you have two options:

**Option A: Local install via xtask (recommended)**

```bash
cargo xtask e2e-install
```

Downloads kind, tilt, and kubectl to `./bin/`. No global install, no sudo. All `e2e*` commands check `./bin/` first, then fall back to PATH.

**Option B: Install globally yourself**

- **Kind**: https://kind.sigs.k8s.io/user/quick-start/#installation
- **Tilt**: https://docs.tilt.dev/install.html
- **kubectl**: https://kubernetes.io/tasks/tools/

### What Gets Deployed

```
┌────────────────────────────────────────────────┐
│  Kind cluster: ephpm-dev                       │
│                                                │
│  ┌──────────────┐                              │
│  │  ephpm        │  Deployment (1 replica)     │
│  │  :8080        │  Serves test docroot        │
│  └──────────────┘                              │
│         ▲                                      │
│         │ http://ephpm:8080                     │
│         │                                      │
│  ┌──────────────┐                              │
│  │  ephpm-e2e    │  Job — runs Rust test binary │
│  │  (test runner)│  Exits 0=pass, 1=fail       │
│  └──────────────┘                              │
└────────────────────────────────────────────────┘
```

### Directory Structure

```
k8s/
├── kind-config.yaml        # Kind cluster config (single control-plane node)
├── Tiltfile                # Tilt orchestration — builds, deploys, runs tests
├── base/
│   └── ephpm-single.yaml   # Deployment + Service for ephpm
└── tests/
    └── e2e-job.yaml        # Job that runs ephpm-e2e test binary

docker/
├── Dockerfile              # Multi-stage: build ephpm with PHP → minimal runtime
└── Dockerfile.e2e          # Multi-stage: build test binary → minimal runner
```

### Tiltfile

The Tiltfile (`k8s/Tiltfile`) handles:
- Building `ephpm:dev` image from `docker/Dockerfile`
- Building `ephpm-e2e:dev` image from `docker/Dockerfile.e2e`
- Deploying ephpm Deployment + Service
- Deploying the E2E test Job with `EXPECTED_PHP_VERSION` injected via string replacement
- In `tilt ci` mode: waits for Job completion, exits with Job's exit code

---

## Running Tests via xtask

### Run E2E tests (headless)

```bash
cargo xtask e2e --php-version 8.5
```

This does everything in one shot:
1. Creates the Kind cluster `ephpm-dev` (skips if it exists)
2. Builds `ephpm:dev` with `--build-arg PHP_VERSION=8.5`
3. Builds `ephpm-e2e:dev` test runner image
4. Loads both images into Kind
5. Runs `tilt ci` with `EXPECTED_PHP_VERSION=8.5`
6. On failure, dumps pod logs for debugging

### Start dev environment (interactive)

```bash
cargo xtask e2e-up --php-version 8.5
```

Same setup, but runs `tilt up --stream`:
- Streams logs to your terminal
- Tilt web dashboard at **http://localhost:10350**
- Watches for source changes and auto-rebuilds
- **Ctrl+C** to stop

### Tear down

```bash
cargo xtask e2e-down
```

Removes Tilt resources and deletes the Kind cluster.

### Container engine

Defaults to `podman` if available, otherwise `docker`:

```bash
CONTAINER_ENGINE=docker cargo xtask e2e --php-version 8.4
```

---

## GitHub Actions

The E2E workflow (`.github/workflows/e2e.yml`) runs a matrix of PHP 8.4 and 8.5:

```yaml
strategy:
  matrix:
    php: ["8.4", "8.5"]
steps:
  - cargo xtask e2e-install
  - cargo xtask e2e --php-version ${{ matrix.php }}
```

Each job builds ephpm with the specified PHP version, deploys it to a Kind cluster, and validates that `/index.php` reports the correct PHP version and embedded SAPI.

---

## Development Workflow

| Task | Command | Infrastructure needed |
|------|---------|----------------------|
| HTTP routing, config, CLI | `cargo build` + `cargo nextest` | None (stub mode) |
| PHP execution | `cargo xtask release` + `cargo nextest --run-ignored all` | PHP SDK |
| Local vhost routing | `cargo test -p ephpm --test vhost_routing -- --nocapture` | None — spawns the binary directly |
| Test a local site in a browser | `ephpm dev --sites ~/sites` | None — `*.localhost` resolves to 127.0.0.1 |
| E2E tests (headless) | `cargo xtask e2e --php-version 8.5` | Kind + Podman/Docker |
| E2E dev environment | `cargo xtask e2e-up --php-version 8.5` | Kind + Podman/Docker |
| Tear down E2E | `cargo xtask e2e-down` | — |

---

## Future E2E Tests (Planned)

These will be added as the corresponding features are implemented:

- **Cluster tests** — 3-node StatefulSet, KV gossip replication, node failure recovery
- **DB proxy tests** — MySQL/Postgres connection pooling, query digest, slow query detection
- **WordPress lifecycle** — Install wizard, post creation, plugin activation
- **External PHP mode** — Validate worker process management
