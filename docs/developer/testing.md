# Testing Strategy

ePHPm uses a layered testing approach: fast unit tests for inner logic, a dedicated Rust E2E crate (`ephpm-e2e`) for integration assertions, and Tilt + Kind for orchestrating real infrastructure.

---

## Test Layers

| Layer | Tool | What it tests | Speed |
|-------|------|---------------|-------|
| Unit | `cargo nextest` | Config parsing, routing logic, SAPI mapping, response building | Seconds |
| Integration | `cargo nextest` (ignored by default) | PHP execution, WordPress lifecycle — requires libphp | Seconds (with SDK) |
| E2E | `ephpm-e2e` crate + Tilt + Kind | Full stack against real K8s infrastructure | Minutes |
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

## E2E Testing: ephpm-e2e Crate

E2E tests live in a dedicated Rust crate (`crates/ephpm-e2e/`) that runs inside a Kind cluster. The crate is **excluded from the workspace** — it has different dependencies and is only built inside the E2E test runner container.

### Current Tests

**`tests/phpinfo.rs`** — validates that ephpm boots and serves PHP correctly:

- `php_version_matches` — GETs `/index.php`, asserts 200, checks `PHP Version: X.Y` matches the version linked at build time, confirms `Server API: embed`
- `health_check` — GETs `/`, asserts success response

The test reads two env vars:
- `EPHPM_URL` — base URL of the ephpm service (e.g. `http://ephpm:8080`)
- `EXPECTED_PHP_VERSION` — major.minor version to assert (e.g. `8.5`)

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

- **Kind**: https://kind.sigs.k8s.io/docs/user/quick-start/#installation
- **Tilt**: https://docs.tilt.dev/install.html
- **kubectl**: https://kubernetes.io/docs/tasks/tools/

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
