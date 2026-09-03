# Worker dispatch fairness — the #442 tail-latency investigation

> **Terminology note (v0.10):** this measurement record predates the
> concurrency-config rename and the removal of the `spawn_blocking` execution
> engine. Read `worker_count` as `[php] concurrency`, `worker_backlog` as
> `[php] queue_depth`, and `fpm_engine = "pool"` as the (now only) per-request
> execution pool; the `spawn_blocking`-engine caveats no longer apply.
> Historical content is otherwise preserved as written.

Issue #442: in the third-party Laravel runtime comparison (wrk `-t10 -c100
-d30s`, `worker_count = 2`, v0.8.7-php8.4, array sessions), ePHPm worker mode
had the **highest throughput** of the five Octane-class runtimes (2,227 req/s
on `/api/static`, ~19% over FrankenPHP) and the **worst P99 of the fast group**
(157 ms vs FrankenPHP's 57 ms). This document is the root-cause narrative, the
instrumentation evidence, and the reasoning behind the fix. The fix itself
lives in `worker_pool.rs` / `fpm_pool.rs` (the two pools shared the defect);
the design doc amendment is in `worker-mode-design.md` §2.1(a).

## 1. Why that signature is a contradiction worth chasing

At 2,227 req/s with 100 open connections, Little's law gives the mean sojourn
time of an ideal 2-server FIFO queue: `100 / 2227 ≈ 45 ms`. A closed-loop
FIFO system at deep saturation is nearly deterministic — every request waits
behind the same ~96 predecessors — so P50, P90 and P99 should all sit near
that 45 ms. FrankenPHP's numbers behave exactly like that: P99 57 ms at
1,874 req/s ≈ 1.1× its own ideal sojourn (53 ms).

ePHPm's P99 sat at ~3.5× ideal while its *throughput led the field*. Execution
speed cannot produce that shape: fast execution lowers both the mean and the
tail. Only **waiting** can decouple them — specifically, waiting that is
unfair, where some requests are served promptly and others repeatedly lose
their place.

## 2. Where a request waits in worker mode

```text
hyper conn task ──► Router::handle_php_worker ──► WorkerPool::dispatch
                                                       │
                                   async_channel::bounded(worker_backlog)
                                                       │        (backlog = 2 by default:
                                                       ▼         worker_backlog 0 => worker_count)
                                        worker thread recv_blocking()
                                                       │
                                            PHP executes, oneshot reply
```

`worker_backlog` defaults to `worker_count`, so in the bench the dispatch
channel held **2** jobs. Two more were executing. The other ~96 in-flight
requests were all parked inside `async_channel`'s `Sender::send().await`.
That send future is where the entire anomaly lives.

## 3. The mechanism: bounded-channel `send()` is a barging lottery

`async-channel`'s `Send` future (v2.5.0, `SendInner::poll_with_strategy`) is a
try/listen/retry loop:

1. On first poll it calls `try_send` **before** ever queueing behind the
   already-parked senders. A freshly arriving request can therefore steal a
   just-freed slot from waiters that have been parked for hundreds of
   milliseconds ("barging").
2. On `Full` it registers an `event-listener` at the **back** of the
   `send_ops` waiter queue.
3. When a worker pops a job, exactly one waiter is notified. The woken task
   must be scheduled by tokio and then *retry* `try_send`. If a barging sender
   (or another woken waiter) took the slot first, the retry fails and the
   loser **re-registers at the back** of the waiter queue — behind ~95 others.

Step 1 is what maximizes throughput: a freed slot never sits idle waiting for
a parked task to be scheduled; whoever is runnable first takes it. The queue
is perfectly work-conserving, which is why ePHPm out-throughputs everything.

Step 3 is what fattens the tail: each lost race costs a full **lap** of the
waiter queue. At 2,227 req/s a lap of ~96 waiters takes `96 / 2227 ≈ 43 ms`.
One lost race ≈ P75, two ≈ P90, five to six ≈ P99. The defect converts
scheduling jitter into integer multiples of 43 ms.

The same structure exists in the experimental fpm pool engine
(`[php] fpm_engine = "pool"`, `FpmPool::dispatch`/`try_dispatch`) — same
channel, same default backlog, same barging lottery.

## 4. The instrumentation evidence

Reproduced bare-process on WSL2 (32 CPUs), the same wrk shape and config as
the bench (`worker_count = 2`, array sessions, PHP 8.4). Official
v0.8.7-php8.4 release binary. Three rounds:

| round | req/s | P50 | P75 | P90 | P99 |
|-------|-------|-----|-----|-----|-----|
| 1 | 2,339 | 47.5 ms | 79.6 ms | 131.1 ms | 275.9 ms |
| 2 | 2,331 | 46.5 ms | 80.6 ms | 132.9 ms | 275.5 ms |
| 3 | 2,321 | 47.0 ms | 81.0 ms | 134.4 ms | 285.1 ms |

Note the shape: P50 ≈ 47 ms — almost exactly the ideal sojourn (100/2330 ≈
43 ms) — then P75/P90/P99 climbing in ~43 ms steps. Those are the laps.

The `ephpm_worker_request_wait_seconds` histogram (measures exactly the time
spent inside `dispatch().await`) over a 70,083-request instrumented run:

| wait | requests | cumulative |
|------|----------|------------|
| ≤ 1 ms | 29,934 | **42.7%** |
| ≤ 25 ms | 33,437 | 47.7% |
| ≤ 50 ms | 46,350 | 66.1% |
| ≤ 100 ms | 60,360 | 86.1% |
| ≤ 250 ms | 69,240 | 98.8% |
| ≤ 500 ms | 70,025 | 99.9% |

- Mean **wait** = 2,886 s / 70,083 = **41.2 ms**. Mean **execution** (derived:
  `ephpm_php_execution_duration_seconds` includes the wait; subtracting sums)
  = **1.6 ms**. Uncontended `-c1` latency: 0.85 ms mean, 1.14 ms P99. The
  wait *is* the latency — 96% of total request time.
- The distribution is **bimodal**, and that is the smoking gun. In a fair
  FIFO at this saturation the queue is never empty, so *every* request would
  wait ≈ one lap (~40 ms); sub-millisecond waits would be impossible. Yet
  **42.7% of requests were admitted in under 1 ms** — those are the barging
  winners of step 1 — while the balancing cost shows up as the 13.9% of
  requests waiting 2-12× a lap. The barging winners and the multi-lap losers
  are the same distribution seen from both ends.

This also rules out the other suspects from the issue: there are no
per-worker mailboxes (one shared MPMC queue, so no head-of-line blocking
across workers), no admission batching (no sawtooth in the histogram), and
the response write-back is a non-blocking `oneshot` fulfilment (a slow client
cannot hold a worker; buffered bodies are handed off whole).

## 5. The fix: move the waiting into a FIFO-fair semaphore

`tokio::sync::Semaphore` is documented fair: permits are granted to waiters
in acquire order, and a new acquirer queues behind existing waiters even when
a permit is technically free — precisely the two properties `send().await`
lacks. The change:

- `WorkerPool` (and `FpmPool`) gain an admission semaphore with
  `worker_backlog` permits — one permit == one dispatch-queue slot.
- `dispatch()` acquires a permit (the only wait, strictly arrival-ordered),
  then `try_send`s the job. The channel capacity equals the permit count and
  every enqueued job holds its permit, so the `try_send` can never find the
  channel full: no retry loop, nothing to barge into.
- The worker releases the permit (`WorkerJob::admission`) the moment it pulls
  the job off the channel — the same instant a slot freed under the old
  scheme — so the queue-refill pipeline, and therefore throughput, is
  unchanged. The freed slot simply goes to the longest-waiting dispatcher
  instead of the luckiest one.
- `drain()` closes the semaphore alongside the channel, so parked dispatchers
  still wake into the 503 path. A request cancelled by the outer timeout
  while parked leaves no trace (tokio unlinks the waiter), and the
  depth-gauge accounting now has no await point between increment and
  enqueue, closing a small pre-existing leak where a request cancelled
  mid-`send().await` left the gauge permanently high.
- `FpmPool::try_dispatch` (shed) maps onto the same gate: `grace == 0` is
  `try_acquire` (backlog full → 503 immediately), non-zero grace is
  `timeout(grace, acquire)` — shed-mode waiters now also queue fairly.

What is traded: admission adds one semaphore acquire/release per request
(two uncontended atomics; a mutex-protected waitlist push under saturation).
Strict FIFO also gives up a micro-locality benefit barging had — the barging
winner was usually cache-hot on the connection that just completed. Both
effects are at measurement-noise level in the verification runs; the
throughput delta was ≈1% (2,282 → 2,256 req/s over four rounds, per-round
spread wider than the delta).

Fairness invariant pinned by `worker_pool::tests::
admission_is_strictly_fifo_and_barge_proof`: waiters are admitted in arrival
order and a dispatcher arriving while others are parked cannot steal a
freshly freed slot.

### 5.1 The `[php] admission` knob

Both admission disciplines are selectable per deployment:
`[php] admission = "fifo"` (**default** — the semaphore path above) or
`"barge"` (the pre-fix racing admission: wait in the bounded channel's own
`send().await`, `admission: None` on the job). The knob exists as an operator
escape hatch; it applies to both `WorkerPool` and `FpmPool` and is inert
(startup WARN) on the default `fpm_engine = "spawn_blocking"`, which has no
ePHPm-owned admission queue. The barge path keeps the depth-gauge honest
with a rollback guard (`DepthRollback`) instead of reintroducing the
cancellation leak noted above, and its behaviour is pinned by the mirror
tests `admission_barge_lets_a_newcomer_steal_a_freed_slot` in both pools.

The knob was built to settle whether per-request (fpm) dispatch wants a
different default than worker mode — a cross-recording benchmark had hinted
the fpm path regressed under #443. **Measured: it does not, and FIFO stays
the default for both pools.** Same-host rotated A/B (Laravel bench, wrk
10t/100c/30s, 3 rounds, array sessions, nginx+fpm control in-session, pool
engine sized 2 threads / backlog 2, zero errors in every cell):

| fpm pool, mean of 3 rounds | health | static | cpu | db |
| --- | ---: | ---: | ---: | ---: |
| req/s fifo → barge | 1030.8 → 1030.1 | 1018.9 → 1019.1 | 1008.8 → 1007.7 | 732.4 → 730.9 |
| P50 ms fifo → barge | 96.7 → 96.9 | 97.9 → 97.6 | 98.9 → 99.0 | 135.6 → 135.1 |
| P90 ms fifo → barge | 98.1 → **187.4** | 99.3 → **189.8** | 100.2 → **192.3** | 140.1 → **223.3** |
| P99 ms fifo → barge | 105.0 → **354.4** | 103.0 → **375.0** | 104.6 → **367.7** | 152.1 → **425.2** |

Throughput is identical (every delta ≤0.2%, inside the ±2% round spread;
ratio-to-control matches to 0.2 points), while barge costs ~2x at P90 and
~3-3.5x at P99. The lap quantization is visible in the numbers: with ~96
requests in flight one queue lap ≈ 96/1030 ≈ 93 ms, and barge's P90 sits
exactly one lap above its P50 (+90 ms) with P99 near three laps — while
FIFO's P50/P90/P99 collapse to a single sojourn (spread 8 ms). Barge shows
no throughput win because at saturation the queue is never empty: FIFO
hands a freed slot to an already-parked waiter with no idle window, so both
disciplines are equally work-conserving; barging's only theoretical edge
(filling a slot during the wakeup gap) is microseconds against a
millisecond-scale service time. A worker-mode leg in the same session
reproduced §6 in reverse (barge: throughput flat, P99 59 → 158 ms), so the
behaviour is symmetric across modes — the workload shape (connections ≫
slots) is what matters, not the execution model. The suspected fpm
regression was closed as host drift: the recording that showed it ran the
`spawn_blocking` engine, whose admission (the `[php] workers` tokio
semaphore) #443 never touched, and re-measuring that exact config in this
session put it back at 110% of nginx+fpm on `/api/db` — the pre-#443
recording's ratio, not the suspect one's.

## 6. Before/after (same host, same build toolchain)

Locally built binaries from the fix's parent commit vs the fix (identical
toolchain/profile), same bare-process WSL2 setup as §4. Four 30-second
rounds each, interleaved across two sessions; the serving process's
`/proc/<pid>/exe` was asserted before every round (see §7):

| | req/s (avg) | P50 | P75 | P90 | P99 (per round) | max |
|---|---|---|---|---|---|---|
| parent (barging) | 2,282 | 49.3 ms | 80.2 ms | 131 ms | 259 / 280 / 303 / 267 ms | 1.08 s |
| fix (FIFO) | 2,256 | 43.9 ms | 44.4 ms | 44.9 ms | 46 / 66 / 62 / 62 ms | 67 ms |

The model's prediction held exactly: with strict FIFO the latency
distribution collapsed to a near-deterministic single lap — P50 *dropped*
from 49 to 44 ms (the sojourn no longer carries lost-race laps), P90 sits
within 1 ms of P50, and P99 landed at 1.3× the ideal sojourn — FrankenPHP's
shape (its P99 ran 1.1× ideal) at a throughput cost of −1.1%, within
round-to-round noise. The uncontended path is untouched: `-c1` mean
0.87 → 0.91 ms and P99 1.19 → 1.21 ms (noise). The fix's dispatch-wait
histogram confirms the mechanism end-to-end: 98.5% of waits in the
25–50 ms bucket, none above 100 ms — versus the parent's 42.7% sub-1 ms /
1.2% above-250 ms split.

## 7. A measurement post-mortem (why this doc exists)

Mid-investigation the A/B numbers went incoherent — the fix appeared to
work only when Prometheus metrics were enabled, then only on one port, then
on neither. Every one of those phantom correlations traced to a single
methodology bug: the A/B binaries were named `ephpm-parent` / `ephpm-fix`,
and the harness's `pkill -x ephpm` matched neither, so a stale *parent*
server kept port 8000 for an hour while freshly started servers died on
`Address already in use` and wrk kept measuring the stale process. Fat
tail on 8000 (stale barging binary), tight tail on 8001 (stale fix
binary) — regardless of which binary the harness believed it was testing.

The rule that fixed it, worth keeping: **a benchmark result is only
attributable to a server after asserting which process served it** — check
the listener's PID and `/proc/<pid>/exe` after startup, and fail the run on
a bind error instead of proceeding.
