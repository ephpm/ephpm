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

### Read path — `db.php`, 10 sequential SELECTs

| Lane | c=1 RPS | p50 | p99 | c=16 RPS | p50 | p99 |
|---|---|---|---|---|---|---|
| A sqlite single | 331 | 2.98 ms | 3.48 ms | 566 | 26.95 ms | 42.58 ms |
| **B turso single** | **388** | **2.54 ms** | **3.06 ms** | **598** | 26.86 ms | **31.09 ms** |
| C sqlite cluster | 168 | 5.89 ms | 6.61 ms | 250 | 78.35 ms | 91.28 ms |
| **D turso cluster** | **344** | **2.81 ms** | 4.16 ms | **531** | 30.12 ms | **36.08 ms** |

### Write path — one INSERT per request

| Lane | c=1 RPS | p50 | p99 | c=16 RPS | p50 | p99 |
|---|---|---|---|---|---|---|
| A sqlite single | 596 | 1.64 ms | 2.06 ms | 879 | 18.37 ms | **24.34 ms** |
| B turso single | 618 | 1.57 ms | 2.27 ms | **899** | 17.80 ms | 32.33 ms |
| C sqlite cluster | 398 | 2.42 ms | 3.42 ms | **0.8 / 52** | — | ~1.5 s |
| **D turso cluster** | **523** | **1.82 ms** | 3.06 ms | **694** | 22.75 ms | 40.82 ms |

### What the data says

**Single-node reads: Turso wins, including the tail.** +17% RPS at c=1
with −12% p99, and at c=16 a **−27% p99** (31.09 vs 42.58 ms) for a
modest +6% RPS. Turso is faster *and* steadier on the read path.

**Single-node writes: throughput parity, worse tail.** +2–4% RPS is
inside the noise this harness resolves, and Turso's c=16 write p99 is
**worse** — 32.33 ms vs 24.34 ms, and worse in both reps (39.83 vs
27.25 ms). This is not a clean sweep, and the write tail is the cell to
watch as the engine matures.

**Clustered: the gap is large.** CDC-native replication beats the sqld
sidecar **2.1×** on read throughput (531 vs 250 RPS) with a p99 of
36 ms against 91 ms, and **~13×** on concurrent writes.

**The cost of replication is the sharpest contrast.** Measured against
each engine's *own* single-node baseline at c=16 writes, clustered Turso
retains **77%** of its throughput (694 of 899) while shipping every
change to a replica. Clustered SQLite retains **6%** (52 of 879).

**Clustered SQLite collapses under concurrent writes.** Lane C peaks at
c=4 and falls off a cliff: 744 RPS at c=4 → 453 at c=8 → **37 at c=16**,
with a p99 of 5.04 s, `SQLITE_BUSY` surfacing to PHP as HTTP 500, and
connections that never get served at all. See
[Findings](findings/#sqlite_busy-is-the-clustered-write-ceiling). This is
the *shipping* clustered path, not the experimental one.

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
