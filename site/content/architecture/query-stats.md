# Query Stats

A shared query observability layer for ePHPm. Records timing, throughput, and error rates for every SQL query regardless of which runtime path handles it (DB Proxy or LiteWire).

## Problem

ePHPm has two SQL strategies (see [sql.md](sql.md)):

- **DB Proxy** (`ephpm-db`) -- forwards MySQL wire traffic to a real MySQL server
- **LiteWire** (`litewire`) -- translates MySQL wire traffic to SQLite

Both see every query PHP executes. Neither currently records query-level metrics. Without this, operators have no visibility into slow queries, hot tables, error patterns, or query mix. They have to bolt on external tools (slow query log on MySQL, APM agents in PHP) that don't exist in the LiteWire/SQLite path at all.

## Solution

A single `ephpm-query-stats` crate that both runtimes call into. One set of metrics, one slow query threshold, one Prometheus dashboard -- whether the backend is SQLite or MySQL.

```
   ┌───────────────────────────────── ePHPm ─────────────────────────────────┐
   │                                                                         │
   │     LiteWire Path                          DB Proxy Path                │
   │     ──────────────                         ──────────────                │
   │     PHP (pdo_mysql)                        PHP (pdo_mysql)              │
   │           │                                       │                     │
   │           ▼                                       ▼                     │
   │     LiteWire MySQL Frontend                MySQL Proxy ──┐              │
   │           │                                       │      │              │
   │           ▼                                       ▼      │              │
   │     TrackedBackend ──┐                       Real MySQL  │              │
   │           │          │                                   │              │
   │           ▼          │ record                     record │              │
   │      rusqlite        │                                   │              │
   │                      │                                   │              │
   │                      └───────► QueryStats ◄──────────────┘              │
   │                              (ephpm-query-stats)                        │
   │                                       │                                 │
   │                                       ▼                                 │
   │                              Prometheus /metrics                        │
   │                                                                         │
   └─────────────────────────────────────────────────────────────────────────┘
```

## Architecture

### Crate: `ephpm-query-stats`

New workspace crate. No dependency on `litewire`, `ephpm-db`, or any runtime-specific code. Depends only on `dashmap`, `parking_lot`, `tracing`, `metrics`.

```
crates/ephpm-query-stats/
    src/
        lib.rs          # QueryStats, DigestEntry, public API
        digest.rs       # SQL normalization and hashing
        prometheus.rs   # Metrics registration and recording
```

### Core API

```rust
use std::sync::Arc;
use std::time::Duration;

/// Shared query stats collector.
///
/// Thread-safe, cheaply cloneable. Create one per ePHPm process and pass
/// it to both the DB Proxy and LiteWire integration.
#[derive(Clone)]
pub struct QueryStats {
    entries: Arc<DashMap<u64, DigestEntry>>,
    config: StatsConfig,
}

pub struct StatsConfig {
    /// Queries slower than this are logged at WARN level.
    /// Default: 1 second.
    pub slow_query_threshold: Duration,

    /// Maximum number of distinct query digests to track.
    /// Prevents unbounded memory growth from unique queries.
    /// Default: 100,000.
    pub max_digests: usize,

    /// Whether to record per-digest Prometheus histograms.
    /// Default: true.
    pub prometheus_enabled: bool,
}

impl QueryStats {
    /// Record a completed query.
    ///
    /// Called by both the DB Proxy (after forwarding) and the TrackedBackend
    /// (wrapping LiteWire's rusqlite/hrana-client backend).
    pub fn record(&self, sql: &str, duration: Duration, success: bool);

    /// Get the digest entry for a normalized query, if tracked.
    pub fn get(&self, digest_id: u64) -> Option<DigestEntry>;

    /// Snapshot of all tracked digests, sorted by total time descending.
    pub fn top_queries(&self, limit: usize) -> Vec<DigestEntry>;

    /// Reset all counters. Useful for periodic reporting windows.
    pub fn reset(&self);
}
```

### Query Digest

The digest system normalizes SQL queries so that queries differing only in literal values map to the same digest. This is how MySQL's `performance_schema.events_statements_summary_by_digest` works.

```rust
/// Normalize a SQL query for digest computation.
///
/// Replaces literal values with `?` placeholders so that
/// `SELECT * FROM users WHERE id = 42` and
/// `SELECT * FROM users WHERE id = 99` produce the same digest.
pub fn normalize(sql: &str) -> String;

/// Compute a 64-bit hash of the normalized SQL.
pub fn digest_id(normalized: &str) -> u64;
```

#### Normalization Rules

| Input | Normalized |
|-------|-----------|
| `SELECT * FROM users WHERE id = 42` | `SELECT * FROM users WHERE id = ?` |
| `INSERT INTO t VALUES (1, 'hello', 3.14)` | `INSERT INTO t VALUES (?, ?, ?)` |
| `WHERE name = 'Alice' AND age > 30` | `WHERE name = ? AND age > ?` |
| `WHERE id IN (1, 2, 3, 4, 5)` | `WHERE id IN (?, ...)` |
| `SELECT 1` | `SELECT ?` |
| Comments stripped | `/* ... */` and `-- ...` removed |
| Whitespace collapsed | Multiple spaces/newlines become single space |

The normalizer operates on raw SQL strings (not parsed ASTs) for performance. It uses a simple state machine that walks the SQL character by character, detecting quoted strings, numbers, and comments. This is the same approach MySQL and ProxySQL use -- full parsing would be correct but expensive for a hot path.

#### Normalization Implementation

```rust
/// State machine for SQL normalization.
enum State {
    Normal,
    SingleQuotedString,
    DoubleQuotedString,
    BacktickIdentifier,
    LineComment,
    BlockComment,
    Number,
}
```

Walk the input character by character:
1. **Quoted strings** (`'hello'`, `"world"`) -- replace entire string with `?`
2. **Numbers** (`42`, `3.14`, `-1`, `0xFF`) -- replace with `?`
3. **IN lists** (`IN (?, ?, ?, ?)`) -- collapse to `IN (?, ...)`
4. **Comments** -- strip entirely
5. **Whitespace** -- collapse to single space
6. **Everything else** -- pass through (keywords, identifiers, operators)

Edge cases:
- Escaped quotes inside strings: `'it''s'` is one string, not two
- Negative numbers: `-42` -- the `-` is a unary operator, `42` is the number
- Hex literals: `0xDEAD` -- replace with `?`
- Backtick identifiers: `` `table_name` `` -- pass through (not a value)
- `NULL` keyword -- do NOT normalize (it's semantically meaningful, not a literal value)

### Digest Entry

```rust
/// Accumulated statistics for a single query digest.
#[derive(Clone, Debug)]
pub struct DigestEntry {
    /// The 64-bit digest hash.
    pub digest_id: u64,

    /// The normalized SQL string (with `?` placeholders).
    pub digest_text: String,

    /// An example of the original (non-normalized) SQL.
    /// Updated periodically, not on every call (to avoid allocation pressure).
    pub example_sql: String,

    /// Total number of executions.
    pub count: u64,

    /// Number of failed executions (backend returned an error).
    pub error_count: u64,

    /// Total wall-clock time spent executing this query.
    pub total_time: Duration,

    /// Minimum execution time observed.
    pub min_time: Duration,

    /// Maximum execution time observed.
    pub max_time: Duration,

    /// Sum of rows returned (for queries) or affected (for mutations).
    pub total_rows: u64,

    /// Timestamp of the first execution.
    pub first_seen: Instant,

    /// Timestamp of the most recent execution.
    pub last_seen: Instant,
}
```

### Internal Storage

```rust
/// DashMap keyed by digest_id (u64 hash).
///
/// Each shard holds a DigestEntry that is updated atomically.
/// DashMap provides concurrent read/write access without a global lock.
entries: Arc<DashMap<u64, DigestEntry>>
```

Why `DashMap` and not a `Mutex<HashMap>`:
- The DB Proxy and LiteWire run on separate tokio tasks. Both call `record()` concurrently.
- `DashMap` shards the map internally, so concurrent writes to different digests don't contend.
- ePHPm already depends on `dashmap` (used by `ephpm-kv`).

Memory bound: `max_digests` (default 100,000) caps the number of entries. When full, new digests are silently dropped (existing entries continue to update). This prevents OOM from applications that generate unique SQL per request (e.g., ORMs with inline literals).

### Slow Query Logging

When `record()` observes `duration > slow_query_threshold`:

```
WARN query.slow: sql="SELECT * FROM users WHERE email = ?" duration=2.3s rows=1 digest=0xABCD1234
```

The log includes the **normalized** SQL (no PII from literal values) and the duration. The `digest` field lets operators correlate with Prometheus metrics.

The threshold is configurable via TOML:

```toml
[db]
slow_query_threshold = "1s"
```

### Prometheus Metrics

Registered once at startup via the `metrics` crate (same pattern as existing ePHPm metrics):

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_query_duration_seconds` | Histogram | `digest`, `kind` | Per-digest execution time distribution |
| `ephpm_query_total` | Counter | `digest`, `kind`, `status` | Total query count (success/error) |
| `ephpm_query_rows_total` | Counter | `digest`, `kind` | Total rows returned/affected |
| `ephpm_query_slow_total` | Counter | | Queries exceeding the slow threshold |
| `ephpm_query_active_digests` | Gauge | | Number of distinct digests being tracked |

Labels:
- `digest` -- first 16 chars of the normalized SQL (truncated for cardinality control)
- `kind` -- `query` or `mutation` (determined by first keyword: SELECT vs INSERT/UPDATE/DELETE)
- `status` -- `ok` or `error`

**Cardinality control**: The `digest` label uses a truncated prefix, not the full SQL or hash. This caps Prometheus label cardinality at `max_digests`. Applications generating millions of unique queries won't explode the metrics store.

Alternative: use the digest hash as label instead of truncated SQL. More compact but harder to read in dashboards. Make this configurable.

### LiteWire Integration: TrackedBackend

A decorator that wraps any `litewire_backend::Backend` implementation, recording stats before delegating.

```rust
use litewire_backend::{Backend, BackendError, ExecuteResult, ResultSet, Value};

/// A backend wrapper that records query stats.
pub struct TrackedBackend<B> {
    inner: B,
    stats: QueryStats,
}

impl<B> TrackedBackend<B> {
    pub fn new(inner: B, stats: QueryStats) -> Self {
        Self { inner, stats }
    }
}

#[async_trait::async_trait]
impl<B: Backend> Backend for TrackedBackend<B> {
    async fn query(&self, sql: &str, params: &[Value]) -> Result<ResultSet, BackendError> {
        let start = std::time::Instant::now();
        let result = self.inner.query(sql, params).await;
        let duration = start.elapsed();
        let rows = result.as_ref().map_or(0, |rs| rs.rows.len() as u64);
        self.stats.record_query(sql, duration, result.is_ok(), rows);
        result
    }

    async fn execute(&self, sql: &str, params: &[Value]) -> Result<ExecuteResult, BackendError> {
        let start = std::time::Instant::now();
        let result = self.inner.execute(sql, params).await;
        let duration = start.elapsed();
        let rows = result.as_ref().map_or(0, |r| r.affected_rows);
        self.stats.record_mutation(sql, duration, result.is_ok(), rows);
        result
    }
}
```

This lives in `ephpm-server` (not in `ephpm-query-stats`) because it depends on both `litewire_backend::Backend` and `QueryStats`. The stats crate stays runtime-agnostic.

#### Wiring in ephpm-server

```rust
// In start_litewire():
let stats = query_stats.clone(); // shared with DB proxy
let db = Rusqlite::open(&config.path)?;
let tracked = TrackedBackend::new(db, stats);
LiteWire::new(tracked).mysql("...").serve().await
```

### DB Proxy Integration

The DB Proxy calls `stats.record()` directly from its query routing code. No wrapper needed -- the proxy already intercepts every query for R/W classification.

```rust
// In ephpm-db mysql proxy, after forwarding:
let start = Instant::now();
let result = forward_to_backend(sql, &mut conn).await;
let duration = start.elapsed();
stats.record(sql, duration, result.is_ok(), rows_affected);
```

The `QueryStats` instance is passed into the proxy at construction time (from `start_db_proxies`).

### Configuration

```toml
[db]
# Slow query threshold (applies to both DB Proxy and LiteWire).
# Default: "1s"
slow_query_threshold = "1s"

[db.analysis]
# Maximum distinct query digests to track.
# Default: 100000
digest_max_entries = 100000

# Include query stats in Prometheus /metrics endpoint.
# Default: true
query_metrics = true
```

The `[db.analysis]` section already exists in `DbConfig` (currently used for planned auto-explain features). Query stats configuration fits naturally here.

### Lifecycle

```
ephpm starts
    |
    v
QueryStats::new(config) -- creates empty DashMap
    |
    +---> passed to start_db_proxies() ---> DB Proxy calls stats.record()
    |
    +---> passed to start_litewire()   ---> TrackedBackend calls stats.record()
    |
    v
Prometheus scrapes /metrics -- reads from QueryStats
    |
    v
ephpm shuts down -- QueryStats dropped, all stats lost (ephemeral by design)
```

Stats are ephemeral -- they exist only in memory for the lifetime of the process. This is intentional:
- No disk I/O on the query hot path
- No persistence complexity
- Prometheus handles long-term storage and alerting
- Process restart gives a clean slate (same as MySQL's `performance_schema` after restart)

### Top Queries Endpoint

Optional: expose a `/__/queries` admin endpoint (alongside the existing `/__/metrics`) that returns the top N queries by total time. Useful for quick debugging without Prometheus.

```
GET /__/queries?limit=20&sort=total_time

[
  {
    "digest": "SELECT * FROM users WHERE id = ?",
    "count": 45821,
    "error_count": 3,
    "total_time_ms": 12340,
    "avg_time_ms": 0.27,
    "max_time_ms": 45.2,
    "rows_total": 45818,
    "first_seen": "2024-01-15T10:30:00Z",
    "last_seen": "2024-01-15T14:22:11Z"
  },
  ...
]
```

Sort options: `total_time` (default), `count`, `avg_time`, `max_time`, `error_count`.

### Testing Strategy

1. **Unit tests** (`ephpm-query-stats`):
   - Normalization: literals, strings, numbers, IN lists, comments, whitespace
   - Digest hashing: same SQL produces same hash, different SQL produces different hash
   - Stats recording: counts, timing aggregation, max/min tracking
   - Memory bound: max_digests enforcement
   - Slow query detection: threshold comparison

2. **Integration tests** (`ephpm-server`):
   - `TrackedBackend` wraps rusqlite, queries are recorded
   - Stats appear in Prometheus metrics output
   - Slow query threshold triggers warn log

3. **E2E tests** (`ephpm-e2e`):
   - PHP executes queries through LiteWire
   - `/metrics` endpoint contains `ephpm_query_duration_seconds`
   - Repeated queries aggregate under the same digest

### Dependencies

| Crate | Purpose | Already in workspace? |
|-------|---------|----------------------|
| `dashmap` | Concurrent digest map | Yes (ephpm-kv) |
| `parking_lot` | Atomic entry updates | Yes |
| `tracing` | Slow query logging | Yes |
| `metrics` | Prometheus integration | Yes |

No new external dependencies required.

### Implementation Phases

| Phase | Scope | Milestone |
|-------|-------|-----------|
| 1 | `ephpm-query-stats` crate: normalizer, digest, `QueryStats` struct, `record()` | Stats recording works |
| 2 | Slow query logging (tracing warn) | Operators get alerts |
| 3 | `TrackedBackend` in `ephpm-server`, LiteWire integration | LiteWire queries tracked |
| 4 | DB Proxy integration (pass `QueryStats` to proxy) | Both paths tracked |
| 5 | Prometheus metrics registration | Dashboards work |
| 6 | `/__/queries` admin endpoint | Quick debugging |
| 7 | Config integration (`[db.analysis]` fields) | User-configurable |

Phase 1-3 are the MVP. Phase 4 requires touching `ephpm-db` internals. Phase 5-7 are polish.

### Prior Art

| Project | Approach |
|---------|----------|
| MySQL `performance_schema` | In-process digest tracking, same normalization approach |
| ProxySQL | Proxy-level query digest with stats table, regex-based normalization |
| PgBouncer | No query-level stats (connection-level only) |
| pgDog | Query digest with Prometheus export |

The normalizer design follows MySQL's `performance_schema` approach (character-level state machine) rather than ProxySQL's regex approach (fragile with complex SQL).
