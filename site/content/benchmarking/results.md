+++
title = "Results"
type = "docs"
weight = 2
+++

Measured numbers by release. Each was taken on the built release image
(or, where noted, a release-candidate build), 100% `2xx` verified.

## v0.4.0 → v0.4.1

The v0.4.1 headline was **database latency**. Measured on the release
image; the v0.4.0 control reproduced the lab's recorded baselines, so the
comparison is apples-to-apples.

| Workload | v0.4.0 | v0.4.1 | Change |
|---|---|---|---|
| Single-node SQLite point-SELECT p50 | 44.010 ms | **0.211 ms** | **208×** |
| Single-node SQLite INSERT p50 | 1.07 ms | **0.267 ms** | 4× |
| `db.php` (10 queries) per request p50 | ~444 ms | **~4.4 ms** | **101×** |
| sha256 (per digest) | 306 ns | **133 ns** | **2.3×** |
| cpu.php c=16 RPS | 78.7 | **147.9** | 1.88× (flips a loss vs php-fpm to a win) |
| hello.php c=16 RPS | 730 | 781 | +7% |

**Where the DB number came from:** php's `mysqlnd` client does not set
`TCP_NODELAY`; the litewire MySQL frontend wrote each result-set packet
separately. The two together produced a Nagle + delayed-ACK deadlock on
every multi-packet response — a fixed ~44 ms stall. Coalescing the
result-set into a single write removed it. INSERTs (single OK packet)
were never affected, which is exactly why SELECT was slow and INSERT was
fast — the diagnostic fingerprint.

**Where the sha256 number came from:** a C++-only compiler flag in the SDK
build silently disabled the compiler's function-attribute detection,
which disabled the SHA-NI code path. Restoring it roughly halved sha256
cost and flipped `cpu.php` from a ~2× loss to php-fpm into a win.

Against php-fpm on the local runtimes suite, v0.4.1 wins every category
measured: cpu (was the clearest loss), database (by construction), and
small-script throughput.

## v0.4.2 (in progress)

Measured on a v0.4.2-dev image (wave-1 changes + the HTTP `TCP_NODELAY`
fix) vs published v0.4.1, `--cpus 1`.

| Cell | v0.4.1 | v0.4.2-dev | Change |
|---|---|---|---|
| hello c=1 p50 (latency-bound) | 1.79 ms | 1.64 ms | −8.6% |
| hello c=16 RPS (throughput) | 842 | 895 | +6.3% |
| hello c=16 p99 | 29.3 ms | 25.4 ms | **−13%** |
| cpu c=16 RPS | 559 | 570 | +2% |

The `−13%` p99 and `−8.6%` c=1 p50 are the `TCP_NODELAY` signature (tail
and single-request latency); the modest RPS gain is the combined effect
of that plus wave-1. Worker-dispatch and further items are still being
measured — see [Findings](findings/) for what the data ruled in and out.

## v0.5.0 — resource-aware autotuning

v0.5.0's headline is **container-derived PHP tuning**: on boot in serve
mode, ePHPm reads the cgroup CPU and memory limits and derives an opcache
/ memory / realpath / assertions profile, with `opcache.validate_timestamps`
off (deploys become events via `ephpm deploy` / `ephpm cache reset`).
Operator config overrides any derived value.

**Setup (reproducible):** a **300-file `require_once` app** (`index.php`
includes 300 tiny class files each request — deliberately stat-heavy),
`--cpus 1 --memory 512m`, `oha` at c=16, 15 s, warmup first, 100% `2xx`.
v0.4.2 runs stock PHP ini; v0.5.0 auto-derives.

**The profile v0.5.0 derived for this box (its own startup log):**

```
autotune (serve): cpu_quota=1.00 mem=512MiB (cgroup v2) ->
  workers=1[cgroup_quota] opcache.memory_consumption=92MB memory_limit=356M
  interned=8MB jit_buffer=32MB (buffer-only, jit off) max_files=20000
  realpath=16M/ttl=600 validate_timestamps=0 assertions=-1
```

(It also logs the deploys-are-events contract, and — because this bench
config left the RESP listener disabled — correctly WARNs that
`ephpm deploy` can't reach the server.)

> **Historical output.** That is v0.5.0's verbatim log. Since v0.9.0 the same
> box derives `concurrency=2[cgroup_quota]` — the derived value clamps to a
> floor of 2 on the quota path too (#461: a 1-thread pool deadlocks a PHP
> loopback subrequest) — and the `workers=` label is spelled `concurrency=`.

**Result (development builds, first measurement):**

| | v0.4.2 (stock ini) | v0.5.0 (autotuned) | Change |
|---|---|---|---|
| RPS | 874 | **1144** | **+31%** |
| p50 | 15.0 ms | 14.8 ms | −1% |
| p99 | 20.6 ms | 19.2 ms | −7% |

**Result (confirmation against the published `ephpm/ephpm:v0.4.2-php8.4`
release image, same fixture, v0.5.0 rebased on the v0.4.2 final tree):**

| | v0.4.2 (released image) | v0.5.0 (autotuned) | Change |
|---|---|---|---|
| RPS | 816 | **1096** | **+34%** |
| p50 | 15.8 ms | 15.5 ms | −2% |
| p99 | 38.9 ms | **20.0 ms** | **−49%** |

**+31–34% throughput with zero operator tuning**, driven mainly by
`validate_timestamps=0` eliminating ~300 `stat()` syscalls per request on
this include-heavy workload, plus the realpath cache and compiled-out
assertions. The p99 halving in the confirmation run is the same effect
seen through the tail: with per-request stats gone, the slow requests
stop queueing behind metadata I/O.

**Honest bounds:** this app is deliberately near the *upper* end of what
autotuning buys (300 includes/request). A single-file `hello.php` shows
~0% (it has nothing to stat and fits any opcache). A real framework app
lands **between** — wherever its file count and filesystem cost sit, and
higher on container overlay / network filesystems where `stat()` is
pricier. The number is a demonstrated ceiling-ish case, not a promise for
every app.

## v0.6.0 — the Turso engine, measured against SQLite

v0.6.0 ships the [Turso engine](/roadmap/turso-engine/) as an
**experimental, opt-in** backend (`[db.sqlite] engine = "turso"`) and an
experimental CDC-native clustered mode (`cdc_experimental = true`). This
is the first measurement of both against the shipping SQLite paths.

> **Historical (pre-v0.7.0).** These lanes are a snapshot of v0.6.0, when
> rusqlite (`engine = "sqlite"`) was the default and sqld backed the
> clustered path. In v0.7.0 rusqlite and the sqld sidecar were removed and
> Turso became the only engine; the `engine` and `cdc_experimental` knobs
> and the sqld lanes below no longer exist. The Turso-vs-SQLite numbers are
> retained as the parity evidence behind that switch.

**Setup:** one machine, one session, podman on WSL2, four lanes, each
node `--cpus 1`. `oha`, 8 s warmup then 2 × 20 s measured, best rep
reported with *its own* p50/p99. 100% `2xx` verified per cell. Fixtures
are the canonical [`db.php`](/benchmarking/methodology/#fixtures) (10
sequential PDO `SELECT`s) and a single-`INSERT` write fixture. Rate
limiting and `query_stats` were disabled identically in every lane.

| Lane | Engine | Mode | Replication |
|---|---|---|---|
| **A** | `sqlite` | single | — (rusqlite, the shipping default) |
| **B** | `turso` | single | — |
| **C** | `sqlite` | cluster | sqld sidecar, WAL frames over gRPC (the shipping clustered path) |
| **D** | `turso` | cluster | CDC-native over the cluster channel, no sidecar |

Two measurements were taken this cycle. The **mid-cycle** run (litewire
`6d135a68`) found the per-request costs that motivated litewire#15
(per-session workers + handle reuse); the **release** run below is the
one that matches shipped v0.6.0 — image built from `97e2a60`, litewire
pinned at `d1c0b341`. Where they differ, the release numbers are the
claim.

### Read path — `db.php`, 10 sequential SELECTs (release build)

| Lane | c=1 RPS | p50 | p99 | c=16 RPS | p50 | p99 |
|---|---|---|---|---|---|---|
| A sqlite single | 322 | 3.09 ms | 3.47 ms | 612 | 22.26 ms | 46.02 ms |
| **B turso single** | **371** | **2.65 ms** | **3.25 ms** | **646** | 24.62 ms | **30.93 ms** |
| C sqlite cluster | 147 | 6.71 ms | 8.19 ms | 240 | 82.56 ms | 92.07 ms |
| **D turso cluster** | **358** | **2.75 ms** | **3.38 ms** | **601** | 26.41 ms | **33.19 ms** |

### Write path — one INSERT per request (release build)

| Lane | c=1 RPS | p50 | p99 | c=16 RPS | p50 | p99 |
|---|---|---|---|---|---|---|
| A sqlite single | 648 | 1.51 ms | 1.93 ms | **1130** | **14.26 ms** | **18.68 ms** |
| B turso single | 666 | **1.47 ms** | 1.86 ms | 1120 | 14.38 ms | 20.77 ms |
| C sqlite cluster | 384 | 2.27 ms | 7.66 ms | **0 completed** | — | — |
| **D turso cluster** | **558** | **1.70 ms** | 3.00 ms | **876** | 17.71 ms | 26.34 ms |

### What the data says

**Single-node writes: the engines are now even.** 648 vs 666 RPS at
c=1; at c=16 SQLite actually edges ahead (1130 vs 1120, with the better
p99: 18.7 vs 20.8 ms). The mid-cycle run had Turso ahead here; the
server-side work that landed between the two runs closed it.

**Single-node reads: Turso's lead persists.** +15% RPS at c=1 (371 vs
322) with a ~440 µs p50 gap that was essentially unchanged between the
two measurements, and the steadier c=16 tail (30.9 vs 46.0 ms p99).
Notably, handle reuse is enabled on the SQLite lane in this build and
verified active in the startup log, yet the read gap did not move — see
the follow-up note under Honest bounds.

**Clustered: the gap is decisive and grew.** CDC-native replication
beats the sqld sidecar **2.5×** on read throughput (601 vs 240 RPS, p99
33 vs 92 ms). On concurrent writes there is no longer a ratio to report:
clustered SQLite completed **zero requests** in both 20 s reps at c=16,
while CDC sustained **876 RPS**.

**The cost of replication is the sharpest contrast.** Against each
engine's own single-node baseline at c=16 writes, clustered Turso
retains **78%** of its throughput (876 of 1120) while shipping every
change to a replica. Clustered SQLite retains **0%**.

**Clustered SQLite collapses under concurrent writes.** The mid-cycle
concurrency sweep located the cliff: 744 RPS at c=4 → 453 at c=8 →
**37 at c=16**, p99 5.04 s, `SQLITE_BUSY` surfacing to PHP as HTTP 500 —
and in the release run the c=16 cell completed nothing at all. See
[Findings](findings/#sqlite_busy-is-the-clustered-write-ceiling) and
[issue #217](https://github.com/ephpm/ephpm/issues/217). This is the
*shipping* clustered path, not the experimental one.

**CDC kept pace under sustained load.** The replica finished the write
lanes holding 57,634 rows after a ~57k-row benchmark. Treat that as
strong evidence CDC kept up, **not** as proven exact equality — the
instantaneous primary count was not captured at the same moment. The
5-row convergence gate run before every clustered lane *was* exact, and
a lane that failed it was discarded rather than reported.

### Honest bounds on these numbers

- **The engine is Beta upstream and experimental here.** These numbers
  are evidence for the roadmap's decision gates, not a recommendation to
  move production data onto Turso. Gates 1, 3, 4 and 5 remain open.
- **No lab control run was taken for these DB fixtures**, so per
  [Methodology](methodology/), the *absolute* RPS values do not transfer
  to other hardware. Every lane ran on the same machine in the same
  session, so the **A/B deltas are the durable claim**; the absolute
  ceilings are not.
- **The write fixture is one INSERT per request**, each its own implicit
  transaction. That is the shape a PHP app produces and the shape CDC
  batches on, but it is not a bulk-load or long-transaction benchmark.
- **`--cpus 1` per node.** Per methodology, a result at one quota need
  not hold at another.
- **No MySQL/PostgreSQL baseline** was measured in this pass, so this is
  a SQLite-lineage comparison only.
- **Handle reuse is enabled but its measured effect did not arrive.**
  litewire's backend-level A/B showed reuse saving ~400 µs per
  connect+10-query cycle, and the release build verifiably enables it —
  yet the end-to-end SQLite read p50 kept the same ~440 µs gap to Turso
  in both measurements. The working hypothesis was that handles were being
  discarded rather than returned on the wire frontend's disconnect path.

  **Answered in v0.6.1, and the hypothesis was wrong.** The v0.6.1 proxy
  matrix below puts a *pooled* connection in front of litewire, which
  holds one warm session across many PHP requests — the first
  configuration in which handle reuse has anything to reuse. The gap did
  not close: SQLite p50 3.97 ms vs Turso 3.32 ms through an identical
  pooled path, a **650 µs gap, wider than the 440 µs baseline**. The
  engine difference is genuinely engine-side, not an artifact of
  per-request connect cost, so removing that cost does not recover it.

  **Measured directly in v0.6.2 — no handles are being discarded.** The
  v0.6.1 answer was an inference from latency; the free-list counters
  themselves were unreachable, because the `Rusqlite` is moved into
  `TrackedBackend` and then erased to `Arc<dyn Backend>`. ePHPm now keeps
  a handle to the concrete backend and logs
  `litewire::backend::Rusqlite::reuse_stats()` at debug level every 60 s
  (`hits`/`misses`/`returned`/`discarded`/`expired`/`idle`) — litewire's
  backend crate has no logging of its own, so this was invisible.
  Driving ePHPm's exact single-node stack with a real MySQL client that
  connects and disconnects per request, the way `pdo_mysql` does:

  | workload | hits | misses | returned | discarded | expired |
  |---|---|---|---|---|---|
  | 30 sequential sessions, clean `COM_QUIT` | 29 | 3 | 31 | **0** | 0 |
  | 5 waves × 8 concurrent sessions | 28 | 14 | 42 | **0** | 0 |
  | 10 abrupt TCP closes, no `COM_QUIT` | 10 | 2 | 11 | **0** | 0 |

  Zero discards in every shape, and a 91% free-list hit rate on the
  sequential (single-threaded PHP) profile. Reuse engages exactly as
  designed, and a clean `COM_QUIT` and an abrupt client disconnect behave
  identically — there is no analogue here of the `COM_QUIT` mishandling
  fixed in the DB proxy's own pool
  ([#221](https://github.com/ephpm/ephpm/pull/221)).

  The one real inefficiency the counters expose is unrelated to
  correctness: under concurrency a third of connects **miss** with zero
  discards. Parking is asynchronous — dropping a session only posts an
  end-of-session message, and the session's own worker thread performs the
  hygiene pass and parks afterwards — so a wave of concurrent connects can
  outrun the previous wave's parks, spawn fresh workers, and drift the
  free-list toward its idle cap. That costs a `sqlite3_open` plus WAL-index
  attach on those connects but never corrupts anything. Regression tests:
  `crates/ephpm-server/tests/sqlite_handle_reuse.rs`.

## v0.6.1 — the clustered-sqld write collapse, fixed

> **Historical (pre-v0.7.0).** This section documents the sqld sidecar and
> its `[db.sqlite.sqld] write_permits` knob, both **removed in v0.7.0**
> when the sqld path was retired for the in-process Turso CDC path (which
> is MVCC and has no single-writer collapse). `write_permits` is no longer
> a config knob. Kept here as the record of why the sqld path was fragile.

v0.6.0's matrix found one outlier: **clustered SQLite via sqld collapsed
under concurrent writes** while every other path scaled up. v0.6.1 added
`[db.sqlite.sqld] write_permits`, an opt-in cap on writes in flight
against sqld. Full arc in
[From zero to a plateau](/benchmarking/findings/#from-zero-to-a-plateau-write-admission-for-sqld);
issue [ephpm#217](https://github.com/ephpm/ephpm/issues/217).

**Provenance:** ePHPm `bab470e` (+ this change), litewire `62636c4c`. Lane C of
the db matrix: 2 nodes, `--cpus 1` each, `write.php` (one autocommit
INSERT per request), 15 s cells, 2 reps, 2 independent cluster bring-ups
plus a same-binary confirmation run, replication verified before every
measurement.

### The failure was a hang, not an error

Across the entire matrix there were **zero 5xx**. Completed cells were
100% success; failed cells have an *empty* status-code distribution,
`NaN` percentiles, and every client timing out. A dashboard watching RPS
or error rate reads a total wedge as a small number — which is why the
tables below report completed responses, not only throughput.

### write.php throughput (RPS)

| `write_permits` | c=1 | c=4 | c=8 | c=16 |
|---|---|---|---|---|
| **0** (default, = v0.6.0) | 442–455 | 118–552 *erratic* | **0 completed** | **0 completed** |
| **1** (recommended) | 415–445 | 561–595 | 527–593 | **551–598** |
| 2 | 443–445 | 497–532 | 520–573 | 565–574 |
| 4 | 410–442 | 171–228 | 531–534 | 494–517 |
| 8 | 444–451 | 232–266 | **0 completed** | **0 completed** |

**Reads are unaffected:** `db.php` at c=16 measured 229–240 RPS across
*every* setting including baseline. Reads never take a permit — WAL lets
them run alongside the writer.

### Config-only A/B: same binary, knob off vs on

The sweep above changes the binary between the baseline row and the rest.
This pass does not — one image, two config files — which isolates the
knob from every other v0.6.1 change:

| `write.php` c=16, one binary | RPS | completed in 15 s | 5xx | hangs |
|---|---|---|---|---|
| knob absent (default `0`) | 1.07 | **0** | 0 | all 16 |
| `write_permits = 1` | **594–598** | **8,901–8,956** | 0 | none |

p50 15.8 ms, p99 ~68 ms at `write_permits = 1`. With the knob absent the
v0.6.1 binary reproduces the v0.6.0 collapse exactly — which is the
evidence that the default of `0` is genuinely inert.

### Why the recommended value is 1, not "as many as possible"

- **The cliff sits between 4 and 8 concurrent writes reaching sqld.**
  `write_permits = 8` is *above* it and reproduces the collapse exactly.
  A permit count only helps if it is below the threshold of the resource
  it protects.
- **One permit already saturates the single writer.** 598 writes/s is
  ~1.67 ms per serialized write — the same per-write cost the c=1 lane
  shows. Higher values cannot raise the ceiling because SQLite
  serializes regardless; they only move contention from litewire's FIFO
  queue into sqld's lock. Monotone: 1 > 2 > 4 >> 8.

### What this does not fix

Admission control converts a **collapse into a plateau**. It does not
make clustered sqld competitive:

| Path (write.php, c=16) | RPS |
|---|---|
| Single-node SQLite (rusqlite) | ~1130 |
| Turso engine, single-node | ~1120 |
| CDC-native cluster | ~876 |
| **Clustered sqld, `write_permits = 1`** | **~598** |
| Clustered sqld, default (`0`) | **0 completed** |

The remaining gap is single-writer physics plus an HTTP round trip per
statement. Roughly half the single-node ceiling is the realistic target
for this architecture, and no admission tuning moves it —
[CDC-native clustering](/roadmap/turso-engine/) is the structural answer.

### Caveats

- **The default is `0` in v0.6.x**, so a *default* clustered deployment
  still wedges at c≥8. Set `write_permits = 1` if you run clustered
  SQLite under write load. The default is planned to become `1` in
  v0.7.0.
- **`c=4` is an unstable knee**, not a stable operating point. The
  baseline measured 118 and 552 RPS on successive reps of the same cell.
  Treat any single c=4 number from the unpatched path as meaningless.
- **Autocommit workload.** One INSERT per request. A workload dominated
  by multi-statement *explicit* transactions behaves differently: a
  transaction holds its permit from first write to `COMMIT`, so values
  above `1` let two transactions contend inside sqld again — `1` is
  correct there for a second, independent reason.
- Per methodology, `--cpus 1` and podman/WSL RTT ceilings mean the
  absolute RPS does not transfer to a real cluster. The **deltas** —
  "0 completed → ~598" and "8 is as bad as off" — are the durable claim.


## v0.6.1 — the database proxy: two pool defects, and what the hop costs

ePHPm can put its own connection-pooling proxy (`[db.mysql]` /
`[db.postgres]`) between PHP and the database. That inserts an extra wire
hop in order to reuse authenticated backend sessions across PHP requests
— a trade worth measuring in both directions, because a PHP request
without persistent connections otherwise pays a fresh connect and
handshake every single time.

Measuring it first required fixing it. Two defects, one hiding the other,
both fixed in [ephpm#221](https://github.com/ephpm/ephpm/pull/221).

**Provenance:** ePHPm `bdc9861` (main, post-#221/#222/#224), litewire
`62636c4c`, PHP 8.5.7 ZTS. One host, podman; ePHPm containers `--cpus 1`,
`mysql:8` and `postgres:16` upstreams `--cpus 4`; `oha`, 8 s warmup,
2 × 15 s reps; every cell below verified 100% HTTP 200.

### The bugs were invisible in throughput

`COM_QUIT` was forwarded to the *pooled* backend, which closed; the dead
socket was parked as healthy; the resulting `BrokenPipe` was mapped to
`Ok(..)`, so the corpse was re-parked. v0.6.0 served roughly
`min_connections` requests and then returned
`[2006] MySQL server has gone away` to everything after — permanently, at
ordinary request rates, while looking fine under sustained load.

Fixing that uncovered a second defect it had been masking: `Pool::recycle`
parked the semaphore permit *inside* the idle slot, while `acquire` takes
a permit *before* consulting the idle queue. Returning a connection
therefore **consumed** a permit instead of releasing one. Once
`in-use + idle` reached `max_connections`, nothing on the healthy path
could free one again — a pool full of good connections it could not hand
out. Defect one had hidden it by killing a connection per request, whose
constant discards released permits continuously.

Neither showed up as an error rate. The pooled lanes measured **876 RPS
in which every response was an HTTP 500**, and `oha` reports those cells
as `Success rate: 100.00%` because it counts transport success, not HTTP
status. Read as throughput they said pooling was 2.9× faster than no
proxy at all.

### The hop costs 1.3–2.2 ms per request

Both rows of each pair dial a fresh backend per request, so the proxy is
the only difference between them.

| `db.php` (10 sequential SELECTs), c=1 | RPS | p50 | Hop cost |
|---|---|---|---|
| litewire, no proxy | 323 / 323 | 3.05 ms | — |
| litewire via proxy, no reuse | 218 / 217 | 4.53 ms | −33%, +1.48 ms |
| `mysql:8`, no proxy | 355 / 356 | 2.75 ms | — |
| `mysql:8` via proxy, no reuse | 241 / 241 | 4.09 ms | −32%, +1.34 ms |
| `postgres:16`, no proxy | 104 / 104 | 9.39 ms | — |
| `postgres:16` via proxy, no reuse | 85 / 85 | 11.62 ms | −19%, +2.23 ms |

### What pooling buys, and when it pays for the hop

Same build, same host, reuse the only variable:

| `db.php` | c=1 no-reuse → pooled | c=16 no-reuse → pooled |
|---|---|---|
| litewire | 218 → 249 (**+14%**) | 374 → 705 (**+89%**) |
| `mysql:8` | 241 → 287 (**+19%**) | 510 → 617 (**+21%**) |
| `postgres:16` | 85 → 125 (**+47%**) | 166 → 211 (**+27%**) |

Against connecting *directly*, the answer depends on concurrency and on
the wire protocol:

| vs. direct | c=1 | c=16 |
|---|---|---|
| litewire | 323 → 249 (**−23%**) | 490 → 705 (**+44%**) |
| `mysql:8` | 355 → 287 (**−19%**) | 454 → 617 (**+36%**) |
| `postgres:16` | 104 → 125 (**+20%**) | 97 → 211 (**+117%**) |

On the MySQL wire the proxy is a **net loss at c=1 and a net win at
c=16** — its value is concurrency headroom and connection multiplexing,
not single-request latency. PostgreSQL wins at both, for a different
reason: `pdo_pgsql` pays a full SCRAM-SHA-256 handshake on every request,
and the proxy answers its client with `AuthenticationOk` and never makes
PHP do that work. The direct PG path does not scale at all (104 → 97 RPS
from c=1 to c=16); the pooled path more than doubles.

### The PostgreSQL pool-exhaustion cliff is gone

v0.6.0 at the shipped default `[db.postgres] max_connections = 20`
collapsed past ~24 concurrent requests — 192 → 7 RPS with 41 of 74
responses 500, p50 pinned to exactly `pool_timeout`. That was the permit
deadlock, not session pinning. On v0.6.1, swept at the same default:

| concurrency | 16 | 20 | 24 | 32 |
|---|---|---|---|---|
| `db.php` RPS | 210 | 209 | 208 | 208 |
| non-2xx | none | none | none | none |

Flat and clean through 32 concurrent requests against a cap of 20 —
queueing, as intended, rather than failing.

### Still true in v0.6.1

- **`[db.mysql]` cannot be chained in front of the in-process
  `[db.sqlite]` litewire.** `start_db_proxies()` awaits the proxy's
  backend connect inline and the litewire branch runs after it, so the
  proxy spends its whole 10-attempt (~40 s, not configurable) backoff
  dialling a listener that cannot exist yet, then gives up. The failure
  is non-fatal and nearly silent: one `ERROR` line, then the server
  serves HTTP normally with nothing bound to the proxy's port and every
  database page returning `[2002] Connection refused`. Liveness and
  readiness both look healthy.
- **Reported numbers come with `[db.analysis] query_stats = false`.**
  v0.6.1 records query stats from the proxies
  ([ephpm#224](https://github.com/ephpm/ephpm/pull/224)); leaving it on
  taxes the wire path specifically, which is exactly what `db.php`
  measures. The cost of leaving it on is not characterised here.
## How to read these

- **Absolute numbers are environment-specific.** The db.php p50 was
  measured differently (single-node reproduction) from the raw
  point-SELECT p50; both are real, both are labeled. RPS ceilings under
  podman/WSL are RTT-capped and do not transfer to a cluster.
- **Deltas are the durable claim.** "208×" and "−13% p99" hold across
  environments; "895 RPS" does not.
- **php-fpm comparisons** use the official `php:8.4-fpm` image with an
  opcache+JIT ini overlay, nginx front, same fixtures. The fpm control
  also reproduces the lab's recorded fpm numbers.
