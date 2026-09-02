# Worker dispatch fairness — the #442 tail-latency investigation

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
effects are below measurement noise in the verification runs; the throughput
delta was within ±1%.

Fairness invariant pinned by `worker_pool::tests::
admission_is_strictly_fifo_and_barge_proof`: waiters are admitted in arrival
order and a dispatcher arriving while others are parked cannot steal a
freshly freed slot.

## 6. Before/after (same host, same build toolchain, 3 rounds each)

Locally built binaries from the fix's parent commit vs the fix (identical
toolchain/profile), same bare-process WSL2 setup as §4:

| | req/s (avg) | P50 | P75 | P90 | P99 |
|---|---|---|---|---|---|
| parent (barging) | _see PR_ | | | | |
| fix (FIFO) | _see PR_ | | | | |

(The PR body carries the final table; this doc records the method.)

The prediction from the model: with strict FIFO the wait distribution should
collapse to a near-deterministic ~1 lap for every request — P50 rising
slightly (the sub-millisecond barging winners no longer exist), P99 falling
to ~1.2× the ideal sojourn, throughput unchanged. That is FrankenPHP's
shape, achieved without giving up the throughput lead.
