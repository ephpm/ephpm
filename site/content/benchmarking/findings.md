+++
title = "Findings"
type = "docs"
weight = 3
+++

The technical discoveries behind the numbers — including the ones where
the data contradicted the intuition.

## Nagle's algorithm hides everywhere

The single most productive class of finding was **TCP_NODELAY on
small-response paths**. ePHPm speaks several small-frame protocols, and
each accepted-connection socket needs `TCP_NODELAY` or Nagle +
delayed-ACK adds a ~40 ms stall to multi-segment responses under
keep-alive:

- **Database wire (litewire MySQL frontend)** — the big one. The stall
  was on the *response*: multi-packet result sets deadlocked against the
  php `mysqlnd` client's Nagle. Server-side `set_nodelay` alone did **not**
  fix it — the client's Nagle mattered — so the real fix was **coalescing
  the whole result set into a single write** in litewire. 208× on
  point-SELECTs.
- **KV RESP listener** — set from the start, with an explicit "~40 ms
  stall" comment. This is the precedent that made the other gaps obvious.
- **DB proxy, cluster data-plane** — fixed in the same pass.
- **The main HTTP listener** — *missed initially* on the assumption that
  "hyper sets nodelay itself." It does not. Found by a hot-path audit for
  v0.4.2; contributes the −13% p99 / −8.6% c=1 p50 above.

Lesson: any `accept()` loop that serves sub-MSS responses under keep-alive
should set `TCP_NODELAY`. Don't assume the framework does it.

## The INSERT-fast / SELECT-slow fingerprint

When a database benchmark shows single-row INSERTs fast (~1 ms) but
SELECTs pinned at a fixed ~44 ms, that fixed timer is delayed-ACK, and
the asymmetry localizes it precisely: a single-packet response (INSERT
OK) can't trigger the deadlock, a multi-packet response (a result set)
can. The fingerprint pointed straight at the response-write path.

## SHA-NI was off for the life of the project

Every 8.3/8.4 build shipped without SHA-NI (hardware sha256), because a
`-fvisibility-inlines-hidden` flag (C++-only) leaked into the C compiler
flags, produced a stderr warning, and made an autoconf
function-attribute probe fail — which undefined the macro that gates the
SHA-NI code path. sha256 ran at ~2.7× its potential cost. The fix was an
SDK build change plus a **hard build guard** (`nm | grep
SHA256_Transform_shani`) so it can never silently regress again. A config
field existing, or a feature "being enabled," does not mean the machine
code is present — grep the symbol.

## When measurement caught a bug

Twice, the release verification pass caught a "shipped win" that wasn't:

- **The reverted nodelay.** A rebase conflict during a stacked-PR merge
  silently dropped the litewire `set_nodelay` lines (the commit was in
  history; its changes were overwritten by a `--theirs` resolution). The
  DB benchmark on the release candidate still showed the full 44 ms
  stall. Had we tagged on "the code merged, CI is green," we'd have
  shipped a headline that was false on the flagship path.
- **The wrong SDK in the matrix.** The release workflow pinned an older
  PHP patch version in three of four build jobs, so the artifacts would
  have shipped the pre-SHA-NI SDK under the new version string. Caught by
  re-measuring sha256 on the built image, not the tarball.

This is why rule 2 (*verify on the artifact*) exists. Correct source and
green CI are necessary, not sufficient.

## Things the data ruled OUT

Equally valuable: changes that "should" have helped and didn't.

### JIT made a builtin-heavy workload 17% slower

Enabling `opcache.jit=tracing` on `cpu.php` produced **−17% RPS** (p50
+45%). `cpu.php` is dominated by the `hash()` C builtin; JIT compiles PHP
*bytecode*, so it can't touch the hot code and its tracing/compilation
overhead is pure cost. Conclusions:

- **Never auto-enable JIT.** ePHPm's resource-aware autotuning sizes the
  JIT buffer but leaves JIT *off* by default — this result is the
  justification. Auto-on would regress builtin-heavy apps.
- JIT is a per-application decision that helps *pure-PHP compute*
  (arithmetic, arrays, tight interpreter loops). Bench your app.
- **JIT is not the lever for the cpu-vs-Swoole gap** — see below.

### mimalloc + fat LTO barely moved CPU-bound work

A global-allocator swap (mimalloc) plus fat LTO gave ~+2% on `cpu.php`
and ~+6% on `hello`. Real, kept, no regression — but it also means the
allocator was *not* the bottleneck on those paths, and it **retired a
backlog of allocation-shaving micro-optimizations**: if a whole new
allocator buys 2%, hand-trimming individual `String` clones buys less. A
profile would have to justify that work now.

### The Swoole cpu gap is the ZTS tax, not JIT or allocation

Swoole leads ePHPm worker-mode on `cpu.php` (~206 vs ~149 RPS in the lab).
Neither runtime JITs by default, and allocation isn't the bottleneck
(above). The gap maps to **ZTS overhead** — thread-safe PHP measured
~50% slower than NTS on an isolated hash loop (1.65 ms vs 1.10 ms). The
lever is therefore an **NTS-prefork mode**, gated on a post-PGO
measurement — not anything in the v0.4.2 line.

## Throughput vs latency, again

The `TCP_NODELAY` HTTP win looked like "nothing" (+6% RPS) in a
throughput-bound c=16 test and like a clear win (−8.6% p50, −13% p99) once
measured latency-bound at c=1. Same change, same build — the *test* was
the variable. If a latency optimization reads as a no-op, check whether
the test is saturated before concluding it didn't work.

## `SQLITE_BUSY` is the clustered write ceiling

Benchmarking the two clustered SQLite paths against each other for
v0.6.0 turned up a hard limit in the **shipping** one. Clustered SQLite
(the sqld sidecar) does not degrade gracefully under concurrent writes —
it falls off a cliff:

| concurrency | RPS | p50 | p99 | HTTP 500s |
|---|---|---|---|---|
| 1 | 458 | 2.12 ms | 2.99 ms | 0 |
| 2 | 635 | 3.04 ms | 4.63 ms | 0 |
| **4** | **744** ← peak | 4.99 ms | 12.96 ms | 2 |
| 8 | 453 | 9.6 ms | 29.8 ms | 8 |
| 16 | **37** | 20.4 ms | **5.04 s** | 0 (stalls instead) |

Throughput peaks at **c=4**, halves by c=8, and collapses at c=16. In one
20 s run at c=16, *zero* requests completed — all sixteen connections hung
to the deadline. The error behind it, captured from a request body under
load:

```
SQLSTATE[HY000]: General error: 1205
  SQLite error: [SQLITE_BUSY] SQLite error: database is locked
```

That is single-writer serialization: SQLite takes one write lock, sqld
serializes writers behind it, and past a handful of concurrent writers
connections either time out into a 500 or wait indefinitely.

**Two things make this worse than the raw numbers suggest.** First, the
failure is **silent server-side** — the primary logs nothing at all: no
lock, busy, timeout, or error line. The only evidence is on the client.
Second, the mode it degrades into is *hanging*, not erroring, so a health
check that only looks for 5xx sees a healthy node.

**The MVCC claim, verified rather than assumed.** The [Turso engine
roadmap](/roadmap/turso-engine/) listed a concurrent-writers benchmark as
a Phase 1 deliverable, with the explicit caveat that MVCC beating
WAL + `busy_timeout` was "the headline claim to verify, not assume."
Measured on the same harness, same machine, same fixture: CDC-native
clustered Turso sustained **694 RPS at c=16 with no `SQLITE_BUSY` and no
stalls**, against 37 RPS for clustered SQLite. That is the claim
verified — on this fixture and this quota, with the engine still Beta
upstream and the remaining gates still open.

**Lesson:** benchmark the *write* path at concurrency before trusting a
replication design. Lane C looks fine at c=1 (458 RPS, 2 ms p50) and only
reveals the ceiling above c=4 — and a read-only benchmark would never
have found it at all, because on a primary a `SELECT` never touches
replication.

## From zero to a plateau: write admission for sqld

The section above ends at a diagnosis. This one is what came of it, and
the short version is that the fix was a semaphore in the right place —
but finding the right place, and the right *size*, took the measurement
apart in ways worth writing down. Shipped in v0.6.1 as
`[db.sqlite.sqld] write_permits` ([ephpm#217](https://github.com/ephpm/ephpm/issues/217),
litewire side at [litewire#16](https://github.com/ephpm/litewire/pull/16)).

### The failure is a hang, and that is the whole problem

Re-measured for v0.6.1 across a five-value permit sweep — twenty cells,
two reps each, two independent cluster bring-ups — there were **zero
HTTP 500s**. Not "few". None, anywhere in the matrix.

Every cell that completed anything reported `Success rate: 100.00%`.
Every cell that failed reported an *empty* status-code distribution,
`NaN` percentiles, and all N clients "aborted due to deadline". The
server never answers, so it never gets to answer wrongly.

This is the observation that reframes the bug. An error-rate dashboard
sees a clean 100% success rate while the database serves no one. A
throughput dashboard sees `1.07 RPS` — a small number, not obviously a
different *kind* of number from `600`. Only a completed-request count
distinguishes "slow" from "dead", which is why every table in this
section carries one.

The earlier v0.6.0 pass caught the same behaviour at c=16 ("zero requests
completed — all sixteen connections hung"). What the wider sweep shows is
that this is the *normal* failure mode past the cliff, not the extreme
tail of it.

### Why the cap belongs at the Hrana backend and nowhere else

The obvious temptation is a global write limiter in ePHPm. The data says
that would be a mistake: **sqld is the only path with this problem, and
every other path is actively rewarded by write concurrency.**

| write path (write.php, RPS) | c=1 | c=16 | shape |
|---|---|---|---|
| Single-node SQLite (in-process rusqlite) | 648 | **1130** | scales up |
| Turso engine, single-node | 666 | **1120** | scales up |
| CDC-native cluster | 558 | **876** | scales up |
| **Clustered sqld** | 458 | **collapse** | falls off a cliff |

Three of the four *gain* 1.6–1.7× from c=1 to c=16. A cap sized to rescue
the fourth would take that back from all of them for nothing — none of
them has a single lock behind an HTTP round trip, which is the specific
shape that makes sqld fragile.

So the permit lives in litewire's **Hrana backend**, the one component
that exists only when sqld is the store. Single-node SQLite, the Turso
engine, CDC replication and the MySQL proxy never see it. That is not
caution; it is the measurement telling you where the boundary is.

### The sweep

2 nodes, `--cpus 1` each, `write.php` (one autocommit INSERT per
request), 15 s cells, 2 reps, 2 independent bring-ups, replication
verified before every measurement. Ranges span all reps and runs.

| `write_permits` | c=1 | c=4 | c=8 | c=16 |
|---|---|---|---|---|
| **0** (off — v0.6.0 behaviour) | 442–455 | 118–552 *erratic* | **0 completed** | **0 completed** |
| **1** | 415–445 | 561–595 | 527–593 | **551–598** |
| 2 | 443–445 | 497–532 | 520–573 | 565–574 |
| 4 | 410–442 | 171–228 | 531–534 | 494–517 |
| 8 | 444–451 | 232–266 | **0 completed** | **0 completed** |

Reads (`db.php`, c=16) measured **229–240 RPS across every row**,
baseline included. Reads never take a permit — WAL lets readers run
alongside the writer, so admitting them would throttle traffic that was
never the problem.

A note on the `0` row versus the c=8 = 453 RPS in the table above: these
are different sessions, and per [how to read these](/benchmarking/results/#how-to-read-these)
absolute numbers do not transfer between them. The *shape* reproduced
exactly — healthy at c=1, erratic at c=4, dead past it. The c=4 cell is
worth calling out as an unstable knee rather than an operating point: the
baseline measured 118 and 552 RPS on successive reps of the same cell. A
single c=4 number from the unpatched path means nothing.

### Why 1, and why 8 is as bad as off

Two results in that table do the work.

**The cliff sits between 4 and 8 concurrent writes reaching sqld.**
`write_permits = 8` is *above* it, so it admits enough writers to
reproduce the collapse exactly — same zero completions as no cap at all.
A permit count only helps if it is below the threshold of the resource it
protects, and here the useful range turns out to be narrow. The value is
not a free dial where more is safer.

**One permit already saturates the engine.** At c=16, `permits = 1`
sustains ~598 writes/s, which is **~1.67 ms per serialized write**. The
c=1 lane — a single client, no contention, nothing to queue behind —
costs ~2.2 ms per request end to end, of which the write is the same
~1.7 ms. The queue is *full*: sqld is doing back-to-back writes with no
idle gap, and the semaphore is feeding it exactly as fast as it can
swallow.

That is why the ordering is monotone — **1 > 2 > 4 >> 8**. Additional
permits cannot raise a ceiling set by a single writer; SQLite serializes
regardless. All they can do is move contention from litewire's orderly
FIFO queue into sqld's lock, which is the thing that degrades badly. The
correct size for a semaphore in front of a single-writer engine is one.

### What the permit has to know about transactions

The subtle part is not the semaphore, it is deciding when to let go of
it. An explicit transaction holds SQLite's write lock from its first
write until `COMMIT`, so its permit must live exactly that long.

Three cases were wrong in the dangerous direction before tests found
them:

- **`COMMIT` must never acquire.** If committing needed a permit, then
  every permit being held by a transaction waiting to commit is a
  deadlock. `COMMIT` only releases — and it carries the permit *across*
  its own round trip rather than dropping it on entry, because sqld holds
  the write lock until the commit lands.
- **`END` is SQLite's synonym for `COMMIT`**, and litewire's statement
  classifier had no entry for it. Treated as an ordinary statement, a
  session that wrote inside `BEGIN … END` parked a permit and never gave
  it back — a slow leak that ends with every write blocked forever. A
  test caught it; review had not.
- **`ROLLBACK TO savepoint` does not end a transaction.** It reads like a
  rollback and classifies like one, but the transaction — and the write
  lock — continue. Releasing there hands the permit to another writer
  while the first still holds the lock.

Acquisition is **lazy**, which falls out of the same reasoning: a plain
`BEGIN` is deferred, and SQLite takes no write lock until the first
write, so a read-only transaction takes no permit at all. That matters
more than it sounds — wrapping reads in a transaction is something ORMs
do constantly, and an eager implementation would have spent the whole
permit budget on transactions that never write.

### The honest bound

This converts a collapse into a plateau. It does not make clustered sqld
fast:

| write path, c=16 | RPS |
|---|---|
| Single-node SQLite (rusqlite) | ~1130 |
| Turso engine, single-node | ~1120 |
| CDC-native cluster | ~876 |
| **Clustered sqld, `write_permits = 1`** | **~598** |
| Clustered sqld, default (`0`) | **0 completed** |

Roughly half the single-node ceiling, and that gap is not tuning debt —
it is one writer plus an HTTP round trip per statement. No permit count
moves it, because the arithmetic above shows the writer is already
saturated at one.

The structural answer is a different replication design, not a better
semaphore: [CDC-native clustering](/roadmap/turso-engine/) sustains ~876
RPS at c=16 on the same fixture because it does not funnel writes through
a single remote lock at all. Admission control makes the shipping
clustered path **dependable**; v0.7 is where it gets fast.

**Lesson:** when a system collapses instead of plateauing, the fix is
usually to stop offering it work it cannot take, not to make the work
cheaper — and the correct amount to offer is a property of the resource,
which means it has to be measured rather than guessed. Sized by
intuition, this knob would have been set to the CPU count and would have
done nothing at all.

## Lazily-created tables make "absent" look like "broken"

The same v0.6.0 pass found a cold-start defect in CDC replication worth
generalizing. Turso creates its `turso_cdc` log table **lazily, on the
first captured write** — not when CDC is enabled, and not when a session
connects. A freshly provisioned cluster that has served no traffic
therefore has no log table, and the primary's tailer was treating "table
does not exist" as a fatal poll error: it dropped the subscriber, the
replica redialed 2 s later, and the cycle repeated indefinitely.

Two nodes idling with **zero** requests for 40 s produced 21 subscriber
attach/disconnect cycles on the primary and 24 resubscribes on the
replica. A single write ended it permanently.

Nothing was lost — replication converges as soon as any write lands — but
an operator standing up a cluster sees a continuous error loop and
reasonably concludes replication is broken.

Two lessons. **"Absent" and "empty" are the same answer** when the
absence is just laziness: zero rows to ship either way. The snapshot path
in the same module already encoded exactly that (returning watermark 0 on
a missing table); only the subscriber path disagreed. **And a test that
re-implements the code under test proves nothing about it** — the 2-node
e2e tests hand-rolled their own copy of the primary's serving loop, so
the production function had no direct coverage, which is precisely why
this survived a full security-review pass. Making it testable found the
bug the copy could not.

## Meta-lesson

The wins that mattered were structural and cheap (a socket option, a
single coalesced write, a restored compiler flag). The "obviously fast"
levers (JIT, a faster allocator) were marginal or negative on real
workloads. Intuition ranked these exactly backwards; measurement
corrected it every time.
