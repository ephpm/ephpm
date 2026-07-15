# Turso Engine — Phase 1 Gate Evidence

Evidence for decision gates 2–4 of the
[Turso engine roadmap](../site/content/roadmap/turso-engine.md).
Phase 1 delivers *data*, not adoption: the experimental backend exists to
be measured. Everything below is reproducible with the harness shipped in
litewire (`crates/litewire-turso/examples/phase1_gates.rs`).

## Environment

| | |
|---|---|
| Date | 2026-07-14 |
| Host | AMD Ryzen 9 3950X (16C), Linux 6.18.33 x86_64 (WSL2), native ext4 |
| Build | `cargo build --release`, litewire `feat/turso-backend` @ `1cff105` |
| Turso engine | `turso` crate **=0.7.0** (first non-pre release of the 0.7 line; engine is Beta upstream) |
| SQLite baseline | rusqlite 0.32 (`bundled` SQLite), litewire's production backend |
| Seam measured | litewire `Backend`/`BackendConn` — the exact layer ePHPm's `TrackedBackend` wraps |

Caveats, stated up front:

- The rusqlite numbers **include** its `spawn_blocking` thread-pool hop and
  per-session mutex. That is its real production execution path in
  litewire/ePHPm, so it is the right comparison for us — but it is *not* a
  raw engine-vs-engine measurement. The Turso backend is async-native and
  pays no such hop.
- Both engines run `synchronous=NORMAL` per session (see finding F1) and
  WAL journaling. Same machine, same harness, same SQL, back-to-back runs.
- WSL2, not bare metal; single run per cell, microbenchmark scale. Treat
  the numbers as strong directional evidence, not lab-grade.
- This is **not** Gate 5 (WordPress/Laravel e2e) and not an
  endurance/soak test.

## Gate 2 — latency matrix (single connection)

1000-row table, warm cache, prepared statements with parameters.

| op | engine | p50 µs | p95 µs | p99 µs | samples |
|----|--------|--------|--------|--------|---------|
| point SELECT | rusqlite | 81.2 | 144.8 | 183.8 | 5000 |
| point SELECT | **turso** | **2.9** | **3.0** | **3.7** | 5000 |
| INSERT (autocommit) | rusqlite | 138.5 | 228.6 | 308.1 | 2000 |
| INSERT (autocommit) | **turso** | **17.3** | **31.3** | **79.6** | 2000 |
| 10-query page | rusqlite | 840.1 | 1274.1 | 1521.5 | 500 |
| 10-query page | **turso** | **30.2** | **44.0** | **76.4** | 500 |

Turso is parity-or-better on every row including tails — at this seam. The
~28× point-SELECT gap is dominated by the rusqlite backend's
`spawn_blocking` round-trip (~80 µs on this host), which the async-native
engine simply doesn't pay. The INSERT gap (~8×) survives even though both
engines fsync at `NORMAL`.

## Gate 2 — concurrent writers (N=8 sessions, MVCC vs WAL+busy_timeout)

8 independent `BackendConn`s (one per simulated wire connection), each
inserting 250 rows (200-byte payload), autocommit. busy_timeout=5000 ms on
both engines.

| engine | conns | inserts | wall s | inserts/s | busy errors | other errors |
|--------|-------|---------|--------|-----------|-------------|--------------|
| rusqlite | 8 | 2000 | 0.55 | 3609 | 0 | 0 |
| **turso** | 8 | 2000 | **0.14** | **14451** | 0 | 0 |

The roadmap's headline claim ("MVCC should beat WAL + busy_timeout") holds
at this seam: ~4× throughput, zero busy errors on either engine (the
busy-handler absorbed all contention in both cases; the difference is pure
throughput, not error behavior).

## Finding F1 — the engine's synchronous default is FULL (and it matters)

First run of this matrix used the engine's defaults and produced *bad*
Turso write numbers:

| op (turso, `synchronous=FULL` — engine default) | p50 µs | p95 µs | p99 µs |
|---|---|---|---|
| INSERT (autocommit) | 1815.7 | 5310.2 | 8407.3 |
| concurrent writers | 351 inserts/s (41× slower than the final number) | | |

Root cause (verified in `turso_core` 0.7.0 source): the engine's default
sync mode is `Full`, while litewire's rusqlite backend has always set
`synchronous=NORMAL` per session. The Turso backend now sets
`PRAGMA synchronous = NORMAL` per session for parity, which is what the
final tables above measure. Anyone benchmarking Turso against a tuned
SQLite should check this first.

## Gate 3 — file-format round-trip

Harness: 500 rows of adversarial data (i64::MIN/MAX, negative floats near
±1e308, NULLs in every column, UTF-8 text with combining/CJK chars, blobs,
two indexes incl. UNIQUE), WAL mode, row-value checksums, both directions.

| step | rusqlite→turso→rusqlite | turso→rusqlite→turso |
|---|---|---|
| reader checksum vs writer | **MATCH** | **MATCH** |
| reader inserts 100 rows + UPDATEs | ok | ok |
| writer reopens: checksum + 600 rows | **MATCH** | **MATCH** |
| `PRAGMA integrity_check` (rusqlite) | ok | ok |
| `PRAGMA integrity_check` (turso) | ok | ok |

Both directions clean, including the case where the genuine SQLite C
engine must recover/read a database whose latest state lives in a
Turso-written WAL. Caveat: 600-row databases, one WAL cycle — this
verifies format compatibility, not migration of a multi-GB WordPress DB.

## Gate 4 — crash-recovery smoke (kill -9, 10 iterations)

A child process runs a tight Turso-engine insert loop; the parent SIGKILLs
it mid-loop at a varied interval (300–1200 ms), reopens the database with
the Turso engine (count + write probe), and cross-checks the file with the
SQLite C engine's `PRAGMA integrity_check`.

Result: **10/10 clean.** Row counts monotonically increasing
(15,552 → 214,223 across iterations), Turso reopen+write ok every time,
SQLite `integrity_check` = `ok` every time. Caveat: this is a smoke, not
the "soak" gate 4 ultimately requires; it exercises WAL-recovery after
process death, not power loss or torn writes.

## Unsupported / missing, as hit in practice

- **`VACUUM`** — incomplete upstream, gated behind an experimental flag we
  do not enable; the backend rejects it with a clear error.
- **Multi-process access** — not supported by the engine (experimental
  `multiprocess_wal` flag exists upstream, not enabled). One process owns
  the file. Fine for ePHPm's embedded model; fatal for sidecar-style tools
  (e.g. running `sqlite3` against a *live* database).
- **`ATTACH`/`DETACH`** — behind an experimental upstream flag, not enabled.
- **Non-UTF-8 `TEXT`** — the Rust API surfaces TEXT as `String`; the
  rusqlite backend round-trips invalid-UTF-8 text as blobs. Not observed in
  testing, noted as a semantic difference.
- `PRAGMA integrity_check` **is** supported by the engine (used above) —
  previously unverified.

## Gate status after Phase 1

| Gate | Status |
|---|---|
| 1. Upstream GA | **Not met.** 0.7.0 is a non-pre release but upstream still positions the engine as Beta; multiprocess + vacuum remain experimental/incomplete. |
| 2. Benchmarks parity-or-better incl. tails | **Met at the litewire seam** for this micro-matrix (with the caveats above). MySQL-baseline and PHP end-to-end comparisons not yet run. |
| 3. File-format round-trip | **Verified** at small scale, both directions, checksummed. |
| 4. Crash-recovery | **Smoke clean (10/10).** Full soak still required. |
| 5. WordPress + Laravel e2e | **Not attempted** (out of Phase 1 scope). |

Default engine remains `"sqlite"` — nothing here changes that.

## Reproducing

```bash
# in the litewire repo, branch feat/turso-backend
cargo build --release -p litewire-turso --example phase1_gates
B=target/release/examples/phase1_gates
$B bench   /tmp/gates        # gate 2 latency matrix
$B writers /tmp/gates 8      # gate 2 concurrent writers
$B gate3   /tmp/gates        # gate 3 round-trip
$B crash   /tmp/gates 10     # gate 4 crash smoke
```
