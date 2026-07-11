+++
title = "Methodology"
type = "docs"
weight = 1
+++

How to run an ePHPm benchmark that produces a number you can trust.

## The harness

Two complementary setups are used:

- **Local single-node** (podman/Docker): one container per runtime, a load
  generator container on the same network, fixtures mounted read-only.
  Fast to iterate; used for A/B comparisons between ePHPm versions and
  against php-fpm. The [ePHPm-lab](https://github.com/tinfoyle/ePHPm-lab)
  `RUNTIMES-BENCH` recipe is the reference form.
- **Kubernetes (LKE)**: the lab's k6 suites for realistic app workloads
  (Krayin CRM, Laravel, WordPress/WooCommerce). Not locally reproducible;
  these carry the worker-mode application story.

A local control run of a known version should **reproduce the lab's
recorded baseline** (e.g. hello p50 within noise, cpu RPS within a few
percent). If it doesn't, the hardware isn't comparable and cross-machine
claims are invalid — check this before trusting any A/B.

## Fixtures

Three canonical workloads, each measuring a different subsystem:

| Fixture | Code | Measures |
|---|---|---|
| `hello.php` | one `json_encode` | HTTP dispatch + SAPI round-trip overhead (allocation, request setup) |
| `cpu.php` | `hash('sha256', …)` × 5000 | PHP execution + the hash builtin (**not** pure PHP bytecode — see below) |
| `db.php` | 10 sequential PDO `SELECT`s | the database wire path (proxy/litewire + PHP `pdo_mysql`) |

**`cpu.php` measures the hash builtin, not the interpreter.** `hash()` is
a C function; most of its time is in the (SHA-NI-accelerated) C
implementation, not PHP bytecode. This makes `cpu.php` a good proxy for
"is the CPU intrinsic wired up" but a **poor proxy for JIT** (which
compiles bytecode, not builtins). Use a pure-PHP-compute fixture to
evaluate JIT.

## Load generation

- Tools: `oha` or `hey` (keep-alive HTTP load). Match whatever the lab
  recorded with for comparability.
- **Warmup first**, then a 15–30 s measured run, repeated ≥2×, report best.
- Record **RPS, p50, p99** — never RPS alone. p99 is where tail-latency
  bugs (Nagle stalls, GC pauses, worker recycling) show up while p50 and
  RPS look fine.

## Container CPU quota matters

Run under the CPU quota you care about (`--cpus 0.25` locally mirrors a
`250m` k8s pod). ePHPm derives `worker_count` from the cgroup quota, and
the whole worker-vs-request tradeoff changes with it. A win measured at
`--cpus 1` may not hold at `0.25` and vice versa — state the quota with
every number.

## Throughput-bound vs latency-bound

This distinction has caused more misreadings than any other:

- At **high concurrency on a small CPU quota**, the workload is
  **throughput-bound**: p50 ≈ concurrency ÷ RPS (it's queue time, not
  service time). A latency optimization (e.g. `TCP_NODELAY`) barely moves
  p50 here — it shows up in **p99** and in **low-concurrency (c=1) p50**.
- To measure a *latency* change, run **c=1** (or below saturation). To
  measure a *throughput* change (worker count, dispatch cost), run at
  saturation and read RPS.

Measuring a latency fix with a throughput-bound test will make a real win
look like nothing. Always run both c=1 and c=16.

## Traps that taint a run (check every time)

1. **The rate limiter.** ePHPm's default image config can enable a per-IP
   rate limiter. A single-IP load test then gets flooded with `429`s and
   the "throughput" number is meaningless. **Verify 100% `2xx` in every
   cell** before believing it. This has silently corrupted results more
   than once.
2. **The wrong image config.** The published image ships a config tuned
   for its e2e tests, not for benchmarking. Mount a known bench config;
   don't inherit the image default.
3. **musl vs glibc.** musl's allocator is markedly slower on
   allocation-heavy loops (~3–4× on some microbenchmarks). Compare
   glibc-to-glibc; ePHPm's Linux release is glibc-dynamic.
4. **ZTS vs NTS.** ePHPm runs ZTS (thread-safe) PHP; several competitors
   run NTS. The ZTS tax is real (~50% on tight hash loops in isolation,
   5–10% on realistic scripts). A ZTS-vs-NTS comparison is measuring the
   build mode as much as the server.
5. **RTT-bound ceilings.** Under nested virtualization (podman on WSL2),
   loopback RTT can cap absolute RPS well below a bare-metal or cluster
   number. Version-to-version *deltas* stay valid; absolute ceilings do
   not transfer.

## Tooling note (Windows hosts)

The MINGW/git-bash shell mangles some tool flags (`grep -E`, `head -n`),
which can silently produce empty or wrong filtered output — a benchmark
"failure" that is actually a shell bug. Parse results with `awk` or
PowerShell, and always sanity-check that a result file is non-empty.
