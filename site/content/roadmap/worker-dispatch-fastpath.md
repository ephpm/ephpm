# Worker Dispatch Fast Path — Chasing the Last 80 Microseconds

> **Status: DESIGN — not yet implemented.** Grounded in measurements from
> 2026-07-09 (five-way runtime comparison + worker-mode knob matrix, all
> at 0.25 CPU / 320 MiB, hey keep-alive, 100% HTTP 200 verified).

## The measured gap

Worker mode already cut per-request engine cost 3.2× versus fpm-dispatch
mode. What remains is the distance to the theoretical class ceiling,
represented by Swoole's in-process event loop:

| | per-request CPU | hello c=1 avg | hello c=16 |
|---|---:|---:|---:|
| ePHPm fpm mode | 386 µs | 2.0 ms | 648 rps |
| ePHPm worker (tuned: 1 worker, backlog 32, no recycle) | **120 µs** | 0.9 ms | 2,078 rps |
| Swoole (1 worker) | 38 µs | 0.4 ms | 6,539 rps |

The HTTP layer itself is not the constraint — the Rust edge serves 4,400
responses/sec on a *single connection* when PHP isn't involved (measured
on the 421 reject path). The ~82 µs gap is the cost of moving one request
across the Rust→PHP boundary and back.

## Where the 120 µs goes (suspected decomposition)

Swoole never leaves the PHP process; we pay, per request:

1. **Eager Envelope construction.** `take_request()` materializes five
   PHP arrays up front — `serverVars`, `headers`, `cookies`, `query`,
   uploads (`ephpm_wrapper.c`, the `ephpm_worker_set_prop_array` block).
   A tiny handler reads one of them. Suspected largest line item: zval
   and `zend_string` allocation for data nobody looks at.
2. **Two thread wakeups.** hyper task → dispatch channel → parked worker
   thread wakes → handler runs → response channel → hyper task wakes.
   Each hop is a syscall-class event plus per-request channel allocation.
3. **Response marshalling.** PHP header map → `HeaderMap` rebuild + body
   copy on every response.
4. **Rust-side allocation bundle.** The router hot path carries
   15–30 allocations/request (issue #140); several sit on the worker
   dispatch path too.

This decomposition is *inferred from black-box numbers*, not profiled.
Step 0 below exists because optimizing an unverified breakdown is how
effort gets wasted.

## Design

### Phase 0 — measure, then config wins ✅ config portion SHIPPED (v0.4.1)

The two config fixes landed within hours of this page being written
(PR #159):

- **Quota-aware `worker_count` derivation** — shipped. `worker_count =
  0` now reads the cgroup CPU quota (v2 `cpu.max`, v1
  `cfs_quota_us`/`cfs_period_us`) and derives `ceil(quota_cpus)`,
  falling back to host parallelism without a quota. The knob matrix
  that motivated it: 1 worker beat the derived 2 by ~24% at 0.25 CPU
  (2,100 vs 1,690 rps). Startup logs the derivation source.
- **Recycle default** — shipped. `worker_max_requests` default raised
  500 → 10,000 (500 forced a worker reboot every ~0.25 s at 2,000 rps);
  each recycle logs worker id, requests served, and uptime.

Still open from Phase 0: flamegraph the worker path under c=16 load
(perf inside the container; dhat for the Rust allocs) and validate the
decomposition above before investing in Phases 1–2.

### Phase 1 — lazy Envelope

Envelope keeps an internal handle to the Rust-owned request (already
alive for the request's duration) and materializes each property array
on first access, cached thereafter. `$envelope->serverVars()['REQUEST_URI']`
builds one array; the other four never exist. Header/server-var *keys*
become persistent interned `zend_string`s created once at worker boot —
`REQUEST_URI` is the same string every request and should never be
allocated per request.

### Phase 2 — handoff economics

- Per-worker SPSC slot ring (depth = backlog) instead of per-request
  channel allocations: zero steady-state allocation for the
  request/response handoff.
- Single-worker special case (`worker_count = 1`, the measured sweet
  spot at container quotas): skip MPMC dispatch entirely.
- Reuse a response builder per worker; skip `HeaderMap` reconstruction
  for the common small-header response shape.

### Phase 3 — shared-alloc sweep

Fold issue #140's router-hot-path bundle into the same effort where the
paths overlap (URI parts, header precompute already landed in 0.4.0;
the worker path has its own copies).

## Targets — stated before the work, per the benchmarks discipline

| Metric (0.25 CPU) | today | target |
|---|---:|---:|
| per-request engine CPU | 120 µs | 60–80 µs |
| hello c=1 avg | 0.9 ms | ~0.6 ms |
| hello c=16 | 2,078 rps | 3,100–4,100 rps |
| hello c=16 p95 | 83 ms | shrinks with queue drain rate |

## The honest ceiling

Parity with Swoole's 38 µs is **not** the target. Swoole's event loop
lives inside the PHP process; ours crosses an FFI boundary by design,
because the Rust edge (TLS/ACME, static files, security filtering,
clustering, KV) is the product. The design ceiling is roughly 1.5–2× on
empty-request microbenchmarks — and on any request doing ≥1 ms of real
PHP work, the residual boundary cost is under 5%. The [rate-160
pressure result](https://github.com/tinfoyle/ePHPm-lab) (worker mode
holding 159/160 req/s while fpm+Redis collapsed to 100) is what the
fast path is defending and extending.

## Non-goals

- Moving HTTP parsing into the PHP thread.
- Coroutine scheduling inside PHP.
- Bypassing hyper or the security middleware for "fast" routes.

## Relationship to other roadmap work

- **[NTS prefork](/roadmap/nts-prefork/)** multiplies with this: prefork
  removes the ZTS tax on handler code, the fast path removes boundary
  cost. Both are gated on measurement first.
- **[Benchmarks as a release artifact](/roadmap/benchmarks/)** provides
  the regression gate that keeps these gains from quietly eroding.
- **[OPcache clustering Phase 1.5](/roadmap/opcache-clustering/)**
  touches the same worker recycle machinery; coordinate the recycle
  default change with its recycle-on-deploy semantics.
