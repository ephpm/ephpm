# Nightly Test Suite

Comprehensive tests that run on a daily schedule (`cron: "0 4 * * *"`). These are too slow, resource-heavy, or infrastructure-dependent for PR-level CI but critical for ongoing confidence in the project.

**Estimated wall-clock: ~10 minutes** (all jobs run in parallel; bottleneck is app smoke tests).

---

## Architecture

```
nightly.yml (schedule: cron "0 4 * * *", workflow_dispatch for manual runs)
  |
  +-- 1. release-build (PHP 8.4 + 8.5 × Linux + macOS)   ~4m
  +-- 2. fuzz (4 targets in parallel, 5 min each)         ~5m
  +-- 3. kv-stress (concurrent writers, TTL, compression)  ~5m
  +-- 4. mysql-proxy-integration (real MySQL container)    ~3m
  +-- 5. sqlite-cluster-e2e (3-node docker-compose)        ~5m
  +-- 6. gossip-stress (10-node convergence + churn)       ~2m
  +-- 7. query-stats-load (100 threads, regression guard)  ~1m
  +-- 8. app-smoke-tests (WordPress + Laravel)             ~10m
  +-- 9. windows-cross-compile (PHP 8.4 + 8.5)            ~4m
  +-- 10. dependency-audit (cargo audit + cargo outdated)  ~1m
```

All stress/integration tests use `#[ignore]` so they never run during `cargo test --workspace` in PR CI. The nightly workflow runs them with `cargo nextest run --run-ignored ignored-only`.

---

## 1. Full Release Build Matrix

**What:** Build `cargo xtask release` for PHP 8.4 and 8.5 on Linux and macOS using pre-built `libphp.a` artifacts. Then run all `#[ignore]` integration tests against the built binary.

**Why nightly:** With pre-built `libphp.a` from the external PHP build project, this drops from ~20 min (compiling static-php-cli) to ~4 min (just Rust compilation + linking). Still too heavy for every PR push, and the stub-mode tests in PR CI already catch Rust compilation errors.

**What it catches:**
- FFI linking failures against real `libphp.a`
- PHP SAPI integration regressions (the `#[ignore]` tests in `kv_sapi_integration.rs`)
- Conditional compilation bugs — code gated behind `#[cfg(php_linked)]` that never runs in stub mode
- Platform-specific issues (Linux musl vs macOS)

**Implementation:**
- Matrix: `{php: [8.4, 8.5]} × {os: [ubuntu-latest, macos-latest]}`
- Download pre-built `libphp.a` from the PHP build project's artifacts
- Set `PHP_SDK_PATH` and run `cargo build --release`
- Run `cargo nextest run --run-ignored ignored-only` with the built binary
- Upload binary as workflow artifact for downstream smoke tests

**Test files:** `crates/ephpm-php/tests/kv_sapi_integration.rs`, any future `#[ignore]` tests

---

## 2. Fuzz Testing

**What:** Run `cargo fuzz` targets for 5 minutes each, all in parallel. Four targets covering the main parser surfaces:

### Target: RESP Protocol Parser
- **Crate:** `ephpm-kv`
- **Entry point:** `src/resp/parse.rs`
- **Input:** Arbitrary bytes fed as a RESP2 stream
- **Invariant:** Must never panic. Malformed input returns parse errors gracefully.
- **Why it matters:** The KV store accepts TCP connections from PHP and potentially from RESP-compatible clients. A panic in the parser crashes the server.

### Target: SQL Normalizer
- **Crate:** `ephpm-query-stats`
- **Entry point:** `src/digest.rs` → `normalize()`
- **Input:** Arbitrary UTF-8 strings treated as SQL
- **Invariant:** Must never panic. Must always return a valid `String`. Output length must be ≤ input length + constant overhead (no unbounded growth).
- **Why it matters:** Every query flowing through the DB proxy hits this normalizer. A panic or OOM here is a production crash.

### Target: HTTP Path Traversal
- **Crate:** `ephpm-server`
- **Entry point:** Router path resolution logic
- **Input:** Arbitrary URL paths (percent-encoded, `../`, null bytes, Unicode)
- **Invariant:** Resolved path must never escape the document root. Must never serve files outside `sites_dir`.
- **Why it matters:** Security-critical. Path traversal = arbitrary file read.

### Target: MySQL Wire Protocol Parser
- **Crate:** `ephpm-db`
- **Entry point:** `read_packet()`, `classify_mysql_query()`, `parse_stmt_id()`
- **Input:** Arbitrary bytes as MySQL packet stream
- **Invariant:** Must never panic. Malformed packets return errors or disconnect gracefully.
- **Why it matters:** The DB proxy sits between PHP and MySQL. A parser panic drops all database connections.

**Implementation:**
- Add `fuzz/` directory with `cargo-fuzz` targets
- Each target runs as a separate parallel job (5 min × 4 targets = 5 min wall-clock)
- Corpus stored as CI artifacts, seeded from existing test inputs
- On crash: upload reproducer as artifact, fail the workflow, open a GitHub issue

---

## 3. KV Store Stress Tests

**What:** Hammer the KV store with concurrent load to verify DashMap correctness, TTL accuracy, and compression integrity under pressure.

**Crate:** `ephpm-kv/tests/stress.rs`

### Sub-tests:

#### Concurrent Writer Storm
- 100 concurrent TCP connections (using the `redis` crate, same as `resp_compat.rs`)
- Each connection: 10,000 SET/GET operations with unique keys
- Verify: all 1,000,000 keys are readable after completion, no data loss, no cross-key corruption
- Tests DashMap's lock-free concurrent write correctness

#### TTL Accuracy Under Load
- Set 1,000 keys with TTLs between 100ms and 500ms (randomized)
- Continuously poll until all keys have expired
- Verify: every key expires within ±50ms of its TTL (accounting for the 100ms reaper interval)
- Verify: no key survives past TTL + 200ms
- Tests the expiry reaper task under memory pressure

#### Compression Round-Trip at Scale
- Set 10,000 keys with compressible values (repeated patterns, JSON payloads)
- Use each compression algorithm: gzip, zstd, brotli
- GET all keys back and verify byte-for-byte equality
- Measure compression ratio (informational, not a pass/fail criterion)
- Tests that compression/decompression is deterministic under concurrent access

#### Multi-Tenant Isolation Under Load
- 10 tenants, each authenticated via RESP `AUTH` with their site-specific password
- Each tenant: 1,000 concurrent SET operations with keys named `tenant-{id}:key-{n}`
- After all writes: each tenant reads all their keys and attempts to read other tenants' keys
- Verify: zero cross-tenant reads succeed, all own-tenant reads return correct values
- Tests the `MultiTenantStore` + HMAC-based auth under race conditions

#### Memory Pressure
- Configure `max_memory` to a low value (e.g., 10MB)
- Fill the store until writes start failing
- Verify: errors are clean (not panics), existing data is still readable
- Tests graceful degradation, not just happy-path behavior

**Implementation:** Uses the existing `TestServer` harness from `resp_compat.rs`. All tests marked `#[ignore]`.

---

## 4. MySQL Proxy Integration Tests

**What:** Boot a real MySQL instance and run the full ephpm DB proxy against it, testing connection pooling, R/W splitting, prepared statements, and session isolation.

**Crate:** `ephpm-db/tests/proxy_integration.rs`

### Sub-tests:

#### Basic Query Correctness
- Connect through the proxy, execute: `CREATE TABLE`, `INSERT`, `SELECT`, `UPDATE`, `DELETE`
- Verify all results match direct MySQL execution
- Tests that the proxy faithfully forwards MySQL wire protocol

#### R/W Split Verification
- Configure proxy with 1 primary + 1 replica (two MySQL containers, or simulated via separate databases)
- Execute `SELECT` queries → verify they hit the replica (check `@@hostname` or connection ID)
- Execute `INSERT` → verify it hits the primary
- Execute `SELECT` immediately after `INSERT` → verify sticky routing to primary (within `sticky_duration`)
- Wait for sticky window to expire → verify reads return to replica

#### Prepared Statement Routing
- `PREPARE` a `SELECT` → verify compiled on replica
- `EXECUTE` the prepared statement → verify executed on the same replica (not primary, not a different replica)
- `PREPARE` an `INSERT` → verify compiled on primary
- `EXECUTE` the insert → verify executed on primary
- `CLOSE` both statements → verify cleanup

#### Connection Pooling
- Open 50 concurrent client connections through the proxy
- Each runs a simple query
- Verify proxy multiplexes to ≤ `max_connections` backend connections (check pool metrics)
- Verify `COM_RESET_CONNECTION` is sent between clients reusing the same backend

#### Session State Isolation
- Client A: `SET @myvar = 42`, then disconnect
- Client B (gets the recycled backend connection): `SELECT @myvar` → must return `NULL`
- Tests that `COM_RESET_CONNECTION` properly clears session state

#### Transaction Integrity
- `BEGIN` → `INSERT` → `SELECT` (within txn, must see the insert) → `ROLLBACK`
- `SELECT` after rollback → must not see the insert
- All queries within a transaction must route to the same backend (primary)

**Implementation:** Use `testcontainers-rs` to spin up MySQL 8.0. Proxy started in-process. Tests use `mysql_async` crate for client connections. All tests `#[ignore]`.

---

## 5. SQLite Clustering E2E

**What:** Test the full clustering lifecycle: primary election, write replication, and failover recovery with 3 ephpm nodes running sqld sidecars.

**Infrastructure:** docker-compose with 3 ephpm containers on a shared Docker network.

### Sub-tests:

#### Primary Election
- Start 3 nodes simultaneously with `replication.role = "auto"` and `cluster.enabled = true`
- Wait for gossip convergence (≤15s)
- Verify exactly one node claims `kv:sqlite:primary` in the gossip KV tier
- Verify the primary is the node with the lowest ordinal (consistent with `sqlite_election.rs` algorithm)

#### Write Replication
- Write 100 rows to the primary's litewire MySQL endpoint
- Wait for replication lag (poll replicas every 500ms, timeout 30s)
- Read all 100 rows from each replica
- Verify data integrity: all rows present with correct values

#### Failover
- Kill the primary container (`docker stop`)
- Wait for gossip failure detection (heartbeat TTL = 10s, so ≤15s)
- Verify a new primary is elected (gossip KV updated)
- Verify the new primary's sqld sidecar restarted in primary mode
- Write new rows to the new primary → verify they replicate to the remaining replica

#### Split-Brain Prevention
- Partition the network: isolate one node from the other two (via Docker network disconnect)
- Verify the isolated node does NOT become primary (it can't reach quorum)
- Verify the two connected nodes maintain a single primary
- Reconnect the network → verify the cluster reconverges to a single primary

#### Role Change sqld Restart
- Verify that when a node transitions from replica → primary, its sqld process is SIGTERMed and restarted with `--primary` args
- Check logs for the expected lifecycle: `"stopping sqld"` → `"starting sqld as primary"`
- Verify the new sqld instance passes health checks

**Implementation:** docker-compose file with 3 ephpm services, shared network, volume mounts for config. Test runner is a 4th container or host-side script using `curl`/`mysql` CLI. Requires a release build with sqld embedded.

---

## 6. Gossip Protocol Stress

**What:** Test chitchat gossip convergence, failure detection, and KV replication under scale and churn.

**Crate:** `ephpm-cluster/tests/stress.rs`

### Sub-tests:

#### 10-Node Convergence
- Start 10 `ClusterHandle` instances on different ports
- Each node has 1 seed peer (daisy-chained: node N seeds on node N-1)
- Verify all 10 nodes discover all other nodes within 15s
- Verify `live_nodes()` returns 10 on every node

#### Node Failure Detection
- Start 5 nodes, wait for convergence
- Kill node 3 (drop the `ClusterHandle`)
- Verify remaining 4 nodes remove node 3 from `live_nodes()` within 30s (chitchat failure detection)
- Verify gossip KV entries from node 3 are no longer refreshed (TTL expires)

#### KV Replication Under Churn
- Start 5 nodes
- Node 1 sets 100 KV entries with 60s TTL
- While replication is ongoing: add node 6, kill node 3, add node 7
- After churn settles (30s): verify all surviving nodes have all 100 KV entries
- Tests that membership changes don't corrupt the KV replication protocol

#### Large KV Tier
- 5 nodes, each setting 2,000 unique KV entries (10,000 total)
- Wait for full replication (poll until all nodes have 10,000 entries, timeout 60s)
- Verify no entry corruption (value matches expected for each key)
- Tests gossip bandwidth and digest efficiency at scale

**Implementation:** All in-process using `ClusterHandle::start_gossip()` on localhost ports. No Docker needed. Tests marked `#[ignore]`.

---

## 7. Query Stats Under Load

**What:** Verify `QueryStats` DashMap correctness and measure normalization throughput under concurrent access.

**Crate:** `ephpm-query-stats/tests/stress.rs`

### Sub-tests:

#### Concurrent Recording Accuracy
- 100 threads, each recording 1,000 queries from a pool of 50 distinct SQL patterns
- After all threads complete: verify total execution count across all digests = 100,000
- Verify each digest's `count` matches the expected frequency
- Tests DashMap's atomic update correctness under high contention

#### Normalization Throughput Regression Guard
- Single-threaded: normalize 100,000 realistic SQL queries (mix of SELECT, INSERT, UPDATE with varying literal counts)
- Measure wall-clock time
- Assert throughput > 100,000 queries/second (baseline: current performance is ~500k/sec)
- Fail if throughput drops below threshold → catches accidental O(n²) regressions in the state machine

#### Prometheus Metric Consistency
- Record 10,000 queries across 100 distinct digests
- Fetch Prometheus metrics output
- Verify `ephpm_query_active_digests` gauge matches `entries.len()`
- Verify `ephpm_query_total` counter matches sum of all digest counts
- Tests that metrics and internal state don't drift under concurrent updates

#### Max Digest Cap
- Configure `max_digests = 100`
- Record 200 distinct query patterns
- Verify `entries.len() <= 100` at all times
- Verify no panic or corruption when cap is hit
- Tests the eviction/rejection behavior at the configured limit

**Implementation:** Direct Rust tests against `QueryStats::new()`. No I/O needed. Tests marked `#[ignore]`.

---

## 8. Application Smoke Tests

**What:** Full application lifecycle tests: install a real PHP application, run it against ephpm with litewire SQLite, and verify it renders correctly.

### WordPress
- **Setup:** Download WordPress, run `wp-cli core install` with SQLite via litewire
- **Tests:**
  - Front page renders with `<!DOCTYPE html>` and expected `<title>`
  - Admin login page loads (`/wp-login.php`)
  - Admin dashboard accessible after login (cookie auth)
  - Create a post via wp-cli → verify it appears on the front page
  - Pretty permalinks work (`/sample-post/` resolves to the correct post)
- **Why it matters:** WordPress is the primary target application. If WP works, most PHP apps work.

### Laravel
- **Setup:** Fresh `laravel new` project, `artisan migrate` against litewire SQLite
- **Tests:**
  - Welcome page renders with 200 and expected content
  - `artisan route:list` works (CLI mode verification)
  - API route returns JSON with correct content-type
  - Database migration creates expected tables (verify via SQL query)
- **Why it matters:** Laravel is the second major PHP framework. Tests the full stack: routing, ORM (Eloquent), migrations, artisan CLI.

**Implementation:** Docker images with pre-installed applications (cached in container registry to avoid download time). ephpm binary mounted into the container. Tests run via `curl` + response body assertions. Alternatively, extend the existing Kind/Tilt e2e infrastructure with app-specific manifests.

**Estimated time:** ~10 min (WordPress install + test: ~6 min, Laravel: ~4 min). This is the nightly bottleneck.

---

## 9. Windows Cross-Compilation

**What:** Verify that `cargo xtask release --target windows --no-sqld` produces a valid Windows executable for PHP 8.4 and 8.5.

**Why nightly:** Requires `cargo-xwin` + MSVC cross-toolchain. Slow to set up, and Windows-specific breakage is rare. The PR CI's stub-mode compile already catches most Rust issues.

**Tests:**
- Build completes without errors
- Output file exists at `target/x86_64-pc-windows-msvc/release/ephpm.exe`
- `file` command confirms it's a PE32+ executable
- Binary size is within expected range (sanity check — not too small, not unexpectedly large)

**What it catches:**
- Windows-specific `#[cfg(target_os = "windows")]` compilation errors
- Linker issues with the Windows PHP SDK
- Missing sqld guard (must bail gracefully, not compile error)

**Implementation:** Single job, `cargo install cargo-xwin` (cached), matrix over PHP versions. Upload `.exe` as artifact.

---

## 10. Dependency Audit

**What:** Check for known security vulnerabilities, license violations, and outdated dependencies.

### cargo deny (already in PR CI, extended here)
- `cargo deny check advisories` — RUSTSEC advisory database
- `cargo deny check licenses` — license compatibility
- `cargo deny check bans` — banned crate detection
- `cargo deny check sources` — verify all crates from crates.io

### cargo audit (nightly-only addition)
- `cargo audit` — cross-reference `Cargo.lock` against RustSec advisory DB
- Hits the network (advisory DB fetch), which can be flaky — not suitable for PR CI

### cargo outdated (informational)
- `cargo outdated --root-deps-only` — report outdated direct dependencies
- Does not fail the workflow — output is informational
- Posted as a workflow summary for visibility

**Implementation:** Single job, sequential commands. `cargo deny` is the authoritative check (fails on issues); `cargo audit` is a secondary signal; `cargo outdated` is FYI.

---

## PR CI Changes

With the nightly suite covering heavy testing, consider simplifying PR CI:

| Current PR CI | Proposed PR CI |
|---|---|
| fmt + clippy + test + cargo-deny + e2e (Kind/Tilt) | fmt + clippy + test + cargo-deny |

The E2E tests (`e2e.yml`) would move to nightly-only + `workflow_dispatch` for on-demand runs. This cuts PR CI from ~8 min to ~3 min while maintaining the same coverage cadence.

---

## Failure Handling

- **Fuzz crash:** Upload reproducer artifact, fail workflow, auto-open GitHub issue with label `fuzz-crash`
- **Stress test failure:** Retry once (timing-sensitive tests may flake on shared CI runners). Fail on second attempt.
- **Release build failure:** No retry — this indicates a real linking or compilation problem.
- **App smoke test failure:** Upload full ephpm logs + HTTP response bodies as artifacts for debugging.
- **Dependency audit:** Advisory failures block; outdated reports are informational only.

All failures post to a GitHub Actions summary with direct links to logs and artifacts.
