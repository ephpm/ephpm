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
