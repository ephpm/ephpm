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

## v0.5.0 — resource-aware autotuning (in progress)

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

**Result:**

| | v0.4.2 (stock ini) | v0.5.0 (autotuned) | Change |
|---|---|---|---|
| RPS | 874 | **1144** | **+31%** |
| p50 | 15.0 ms | 14.8 ms | −1% |
| p99 | 20.6 ms | 19.2 ms | −7% |

**+31% throughput with zero operator tuning**, driven mainly by
`validate_timestamps=0` eliminating ~300 `stat()` syscalls per request on
this include-heavy workload, plus the realpath cache and compiled-out
assertions.

**Honest bounds:** this app is deliberately near the *upper* end of what
autotuning buys (300 includes/request). A single-file `hello.php` shows
~0% (it has nothing to stat and fits any opcache). A real framework app
lands **between** — wherever its file count and filesystem cost sit, and
higher on container overlay / network filesystems where `stat()` is
pricier. The number is a demonstrated ceiling-ish case, not a promise for
every app.

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
