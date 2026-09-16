# Performance Roadmap — The Master List

> **Status: LIVING DOCUMENT.** Every performance improvement found by the
> July 2026 measurement campaign (the five-way runtime comparison, the DB
> access-path matrix, the KV latency matrix, and three code audits), with
> its measured or estimated win and current state. Items move from
> "backlog" to "shipped" with receipts — numbers measured on the release
> artifact, per the [benchmarks discipline](/roadmap/benchmarks/).
> Last full revision: 2026-09-15 — the v0.5.0 → v0.10.8 sweep, six minors
> of perf work reconciled against the code and the
> [measured results](/benchmarking/results/). The v0.4.1 table below is
> preserved verbatim; each later minor gets its own section.

## Shipped in v0.4.1

| Item | Measured win | Where |
|---|---|---|
| Single-node SQLite query latency: `TCP_NODELAY` on all sockets **plus** coalescing the litewire result-set response into one write (PHP's mysqlnd client sets no nodelay, so multi-segment result sets deadlocked on its Nagle + delayed-ACK) | **208× — measured on the v0.4.1 release image**: point-SELECT p50 44.010 ms → 0.211 ms; INSERT 1.07 ms → 0.267 ms | [ephpm#161](https://github.com/ephpm/ephpm/pull/161), [litewire#3](https://github.com/ephpm/litewire/pull/3), [litewire#7](https://github.com/ephpm/litewire/pull/7) |
| Hardware intrinsics restored in the PHP SDK (SHA-NI sha256, PCLMUL crc32, AVX2 base64 — disabled since the project began by one C++-only compiler flag) | **sha256 2.3× — measured on the release image**: 306 → 133 ns/digest; every HMAC, cache key, and ETag benefits | [php-sdk#36](https://github.com/ephpm/php-sdk/pull/36) + build guard |
| Quota-aware `worker_count` (now `concurrency`) derivation + recycle default 500 → 10,000 | **+24%** worker throughput at container CPU quotas; recycle churn (one reboot per worker per ~0.25 s at 2k req/s) eliminated | [ephpm#159](https://github.com/ephpm/ephpm/pull/159) |
| litewire translate cache (LRU by query text) | **139×** on repeated queries (38.6 µs → 277 ns per translate) | [litewire#5](https://github.com/ephpm/litewire/pull/5) |
| litewire `prepare_cached` + removal of the LIMIT-0 metadata probe | prepare round-trips halved | [litewire#5](https://github.com/ephpm/litewire/pull/5) |
| litewire per-connection backends (WAL, real concurrent readers) | **52×** on hot selects vs the reopen path; also fixes cross-connection transaction isolation | [litewire#6](https://github.com/ephpm/litewire/pull/6) |
| Expression-column typing (`SELECT 1`, `SELECT a+b`) | correctness: untyped columns typed by value, fixing a `2006 server has gone away` on real-prepared table-less queries | [litewire#8](https://github.com/ephpm/litewire/pull/8) |
| Lazy-vhost negative-cache TTL 60 s → 2 s | regression fix: freshly deployed sites go live in seconds, not up to a minute | [ephpm#164](https://github.com/ephpm/ephpm/pull/164) |
| WordPress worker per-request lifecycle (`init`/`wp_loaded` replay) | correctness unblock for worker-mode WooCommerce; ~2 ms/request cost | [wordpress-worker v0.1.1](https://github.com/ephpm/wordpress-worker/releases/tag/v0.1.1) |

All headline numbers above were re-measured on the built v0.4.1 release
image (not a dev build) before shipping — the DB verification caught,
and this table reflects, that the original server-side nodelay alone did
not cure the single-node SQLite path (the mysqlnd client's Nagle needed
the result-set write coalescing in litewire#7). Release verification
(post-fix DB matrix, sha256 ns/op on the shipped
image, KV RESP-lane parity, before/after charts) accompanies the v0.4.1
release notes.

## Shipped in v0.4.2

| Item | Measured win | Where |
|---|---|---|
| Rust release build tuning: `lto = "fat"`, `codegen-units = 1`, and **mimalloc** as `#[global_allocator]` (non-Windows) | **~+2% cpu.php, ~+6% hello — measured** ([findings](/benchmarking/findings/)); real, kept, no regression. The modest size is itself the finding: the allocator was *not* the bottleneck, which retired the allocation-shaving micro-op backlog | [ephpm#171](https://github.com/ephpm/ephpm/pull/171) |
| HTTP-listener `TCP_NODELAY` + wave-1 hot-path trims | **−13% hello c=16 p99, −8.6% c=1 p50** (v0.4.2-dev vs published v0.4.1, `--cpus 1`) — the nodelay tail signature | [results](/benchmarking/results/#v042-in-progress) |

`panic = "abort"` was evaluated as part of #171 and **deliberately not
set**: the middleware runtime uses `std::panic::catch_unwind` to fail a
panicking middleware `invoke` **closed** (500) and fail startup on a
panicking `init`; under `panic = "abort"` those become no-ops and any
middleware panic takes the whole process down. This row is why the
"build tuning" backlog item below is retired rather than left open.

## Shipped in v0.5.x

| Item | Measured win | Where |
|---|---|---|
| **Resource-aware autotuning** — on boot in serve mode, ePHPm reads the cgroup CPU/memory limits and derives an opcache / memory / realpath / assertions profile with **`opcache.validate_timestamps = 0`** (deploys become events via `ephpm deploy` / `ephpm cache reset`). This is the v0.4.2 backlog's #1 candidate, shipped | **+34% RPS, p99 38.9 → 20.0 ms (−49%) — measured against the published `v0.4.2` release image** on a 300-include stat-heavy app, `--cpus 1 --memory 512m`. Honest bound: this is a near-upper-bound case; `hello.php` shows ~0% and a real framework lands between | [ephpm#175](https://github.com/ephpm/ephpm/pull/175), [results](/benchmarking/results/#v050--resource-aware-autotuning) |
| KV store internals: unbiased sampled LRU eviction, O(1)-ish touch, hot-path micro-opts | shipped; est. ~15% of the native GET path + latency-spike removal under memory pressure. **Needs an on-artifact re-measurement** — the KV matrix motivated it but the shipped delta was not separately benchmarked | [ephpm#142](https://github.com/ephpm/ephpm/issues/142), [ephpm#197](https://github.com/ephpm/ephpm/pull/197) |
| Skip the per-request timeout arm when disabled; static (per-digest) metric labels | shipped; µs-class on the request and query-stats paths. Retires the "query-stats overhead" + "static metric labels" backlog rows | [ephpm#196](https://github.com/ephpm/ephpm/pull/196) |

## Shipped in v0.6.x

| Item | Measured win | Where |
|---|---|---|
| Turso engine as an **experimental, opt-in** backend + first Turso-vs-SQLite matrix (the parity evidence behind the v0.7.0 default swap) | **single-node reads +15% RPS** (371 vs 322 c=1); **clustered CDC 2.5× reads** vs the sqld sidecar (601 vs 240 RPS) and **876 vs 0 completed** on c=16 writes — measured, release build | [ephpm#183](https://github.com/ephpm/ephpm/pull/183), [ephpm#186](https://github.com/ephpm/ephpm/pull/186), [results](/benchmarking/results/#v060--the-turso-engine-measured-against-sqlite) |
| litewire session workers + backend handle reuse + connection cap | reuse saves ~400 µs per connect+10-query cycle (backend A/B); verified active in the shipped startup log | [ephpm#218](https://github.com/ephpm/ephpm/pull/218), litewire `d1c0b341` |
| Router: cache canonicalized PHP script paths (traversal check kept) | shipped; removes a per-request `canonicalize` syscall on the PHP dispatch path | [ephpm#212](https://github.com/ephpm/ephpm/pull/212) |
| DB proxy: two pool defects fixed (`COM_QUIT` forwarded to pooled backend; `recycle` consuming a permit) — both were invisible in throughput (served 100% HTTP 500 that `oha` counts as success) | correctness; unblocks the proxy hop measurement — **hop cost 1.3–2.2 ms/request**, pooling **+21% to +117% at c=16** once correct | [ephpm#221](https://github.com/ephpm/ephpm/pull/221), [results](/benchmarking/results/#v061--the-database-proxy-two-pool-defects-and-what-the-hop-costs) |
| Thin LTO for Windows release builds (fat-LTO link OOMed the worker VMs); `reuse_stats()` handle-reuse instrumentation | build/observability; enables the v0.6.2 finding that **zero handles are discarded** in every connect/disconnect shape | [ephpm#229](https://github.com/ephpm/ephpm/pull/229) |

The v0.6.1 `[db.sqlite.sqld] write_permits` knob (converting the clustered
sqld write-collapse from "0 completed" to a ~598 RPS plateau) shipped and
was then **removed in v0.7.0** with the sqld sidecar itself. Kept in the
[results archive](/benchmarking/results/#v061--the-clustered-sqld-write-collapse-fixed)
as the record of why the sqld path was retired.

## Shipped in v0.7.x

| Item | Measured win | Where |
|---|---|---|
| **v0.7.0 Turso-only** — the rusqlite backend and the **sqld sidecar were removed**; clustered replication is now the in-process Turso CDC path over the cluster channel. Deletes a child process and a gRPC WAL-frame transport (an IPC layer) from every clustered write | **CDC-native cluster ~876 RPS c=16 writes** where clustered sqld completed **0**; single-writer collapse is gone (MVCC). File-format compatible — 0.6.x `.db` files open in place, no dump/reload | [ephpm#278](https://github.com/ephpm/ephpm/pull/278), [turso-engine](/roadmap/turso-engine/) |
| Multi-tenant: per-site DB registry locking — drop the global open-DB mutex | shipped; removes a process-wide lock from every per-site DB resolve on the hot path | [ephpm#303](https://github.com/ephpm/ephpm/pull/303) |
| litewire: end the 1 MiB `memset` per MySQL packet | shipped; a per-packet allocation/zero removed from the wire path (the "DB proxy buffering" backlog item, addressed engine-side) | [ephpm#322](https://github.com/ephpm/ephpm/pull/322), [ephpm#323](https://github.com/ephpm/ephpm/pull/323), litewire `344d2c23` |
| Native `max_execution_time` via per-thread execution timers (Unix); opt-in dedicated FPM thread-pool engine (`[php] fpm_engine`, experimental — default flipped in v0.9.0) | correctness/foundation; the pool engine is measured at its v0.9.0 default flip (below) | [ephpm#277](https://github.com/ephpm/ephpm/pull/277), [ephpm#296](https://github.com/ephpm/ephpm/pull/296) |
| **v0.7.3 TAILCALL Windows VM** — a clang-cl-built PHP 8.5 embed SDK whose interpreter is the fast TAILCALL dispatch (MSVC can only build the slow CALL VM). Ships as a suffixed `-tailcall` artifact | **1.72× on the pure interpreter — measured end-to-end on Windows** (CPU loop, JIT off: 2.79 ms vs 4.80 ms); **+3–5% on the Symfony demo** end-to-end (real apps are filesystem-bound on Windows) | [ephpm#349](https://github.com/ephpm/ephpm/pull/349), [ephpm#350](https://github.com/ephpm/ephpm/pull/350), [Windows performance guide](/guides/windows-performance/#the-tailcall-build) |

## Shipped in v0.8.x

| Item | Measured win | Where |
|---|---|---|
| **FIFO-fair dispatch admission** — the worker/fpm dispatch queue moved from a barge-prone `send().await` retry loop to a tokio `Semaphore` that grants permits in strict arrival order, killing the multi-lap starvation tail | **P99 280 ms → 58 ms at unchanged throughput — measured** (before/after in the #442 write-up); refill pipeline and RPS unchanged | [ephpm#443](https://github.com/ephpm/ephpm/pull/443) |
| Per-site clustered Turso replication + per-vhost KV replication (experimental) — each vhost gets its own CDC-replicated database, ownership by rendezvous hashing | **measured on the v0.8.7 image** (2026-09-01, `--cpus 1`/node): whole-DB clustering costs **≤~13% at c=1** vs single-node and CDC **does not collapse writes** (where the retired sqld path hit 0 completed); the owner-serving `sql/<site>` forward hop on a non-owner costs **~255–275 µs per statement** (read −69%, write −54% at c=1). Turso is Beta upstream, so this stays experimental | [ephpm#416](https://github.com/ephpm/ephpm/pull/416), [lab per-site cluster suite](https://github.com/ephpm/lab) |

## Shipped in v0.9.0

| Item | Measured win | Where |
|---|---|---|
| **Owned FPM thread-pool is now the default execution engine**; the `spawn_blocking` engine was removed and `[php] workers` renamed to `[php] concurrency` (+ `queue_depth`). tokio's blocking pool no longer runs PHP, so a slow script cannot starve static-file serving by construction | **+8–11% RPS across endpoints** (+10.8% health, +10.9% static, +10.5% cpu, +8.0% db), **P50 −7..−10%, P99 −4..−8%** — measured A/B, Laravel per-request, nginx-fpm control. **The decisive result is overload: the old default spawned 486 threads / ~1.05 GiB RSS under a 200-conn flood; the pool held 65 threads / ~193 MiB while serving more requests at a better P99** | [ephpm#460](https://github.com/ephpm/ephpm/pull/460), [ephpm#462](https://github.com/ephpm/ephpm/pull/462), [ephpm#458](https://github.com/ephpm/ephpm/pull/458) |
| Worker/fpm dispatch: eliminate the per-request `$_SERVER` rebuild (`&'static CStr` for invariant keys) and the response-header text round-trip (structured `(ptr, len)` across FFI) — the realized part of the worker-dispatch fast-path Phase 1/2 | **~3–4 µs off a trivial request — measured** (honestly ~40× smaller than the #133/#134 estimates; the PR corrects the framing). Also closes two response-header-injection bugs latent in the old text framing | [ephpm#133](https://github.com/ephpm/ephpm/pull/133), [ephpm#134](https://github.com/ephpm/ephpm/pull/134) |

## v0.10.x (current release line)

v0.10.8 is the current release. The v0.10 line shipped no new **runtime**
performance lever — its work was the `ephpm analyze` static-analysis
suite (baselines, diff-aware scans, result cache), security hardening, and
docs. The perf levers still in flight (TAILCALL-on-Linux, JIT re-enablement,
the deploy/build lane) are in the backlog below, labeled Planned.

## Backlog — open, by estimated value

| Item | Est. win | State |
|---|---|---|
| Worker-dispatch fast path Phases 2–3: per-worker SPSC slot ring, single-worker fast path, shared alloc sweep ([#140](https://github.com/ephpm/ephpm/issues/140)) | toward the 60–80 µs/request target | [worker dispatch fast path](/roadmap/worker-dispatch-fastpath/); Phase 1's easy wins landed in v0.9.0 (#133/#134), the ring/SPSC work is not started |
| DB proxy syscall trims: `BufReader` per stream half + one coalesced write per response (a 1-row SELECT still costs more syscalls than the ~4 achievable) | ~25% of the ~190 µs proxy floor | the engine-side 1 MiB memset is fixed (v0.7.1 #322); the proxy's own stream buffering is still audited-but-not-started |
| Static-file serving internals: `sendfile`, pre-compressed asset cache | unmeasured | next audit sweep — no claims until measured |
| TLS session resumption configuration; HTTP/2 tuning; KV compression-threshold behavior | unmeasured | next audit sweep — no claims until measured |
| PGO/BOLT for the **Rust** binary itself (distinct from libphp PGO below) | unmeasured | next audit sweep |

## Planned — new levers (labeled, not yet measured on-artifact)

| Item | Est. win | Notes |
|---|---|---|
| **TAILCALL on Linux** — the same clang-built SDK that gave Windows its 1.72× interpreter win. Linux ships the GCC HYBRID VM today, which is already fast, so the headroom is smaller than Windows's, but this is the biggest untapped **runtime** lever on non-Windows | Planned; expect a smaller multiplier than Windows's 1.72× because the Linux baseline is HYBRID, not CALL — must be measured, no number claimed | requires a clang PHP 8.5 SDK build in the php-sdk pipeline; PHP 8.5 only |
| **PHP JIT re-evaluation** — the tracing JIT is **off by default on every platform** ([#365](https://github.com/ephpm/ephpm/pull/365)) because of an upstream use-after-free that kills the process when a side trace compiles in a later request ([php-src#21710](https://github.com/php/php-src/pull/21710), still open). Measured effect is sharply workload-shaped | JIT measured **~2.4× on a pure-PHP CPU loop** (5.56 ms → 2.33 ms, Windows v0.7.3) but **−17% RPS on a builtin-heavy workload** (`cpu.php`, which JIT can't touch). `opcache_jit = "function"` is the mode not exposed to the crash; ~0% on filesystem-bound web apps | Planned re-enable once the upstream fix lands; note the TAILCALL interaction — JIT'd hot code sidesteps the interpreter, so on warm loops MSVC+JIT ≈ TAILCALL+JIT |
| **arm64 / Graviton tuning** | Planned; unmeasured | LTO is currently disabled on `linux-aarch64` release builds ([#198](https://github.com/ephpm/ephpm/pull/198)/[#203](https://github.com/ephpm/ephpm/pull/203)) because fat-LTO OOMed the build VMs — re-enabling it (larger builder, or thin LTO) is the first arm64 item |
| **`opcache.preload` for worker mode** | Planned; unmeasured | preload compiles a warm class/function set into SHM once at startup; pairs naturally with worker mode's boot-once lifecycle |

## Planned — Deploy / build performance

A new lane for the *deploy* half of the request lifecycle, distinct from
per-request serving.

| Item | Est. win | Notes |
|---|---|---|
| **`ephpm composer`** — an embedded, pure-Rust Composer-compatible installer ([vivacity](https://github.com/Adelagric/vivacity)) built into the binary, so dependency install needs no PHP runtime and works in stub mode and on Windows | **Planned — not yet released** (on a feature branch, not in v0.10.8). Any install-speed figure is **vivacity's own claim as an external project**, not measured on/by ePHPm; no ePHPm-side benchmark exists yet, so no number is stated here | when it ships, the number to publish is an ePHPm-measured install of a real app's `composer install`, against `composer.phar`, on a pinned runner — per the benchmarks discipline |

## Gated on external milestones

| Item | Est. win | Gate |
|---|---|---|
| PGO for libphp | 10–15% across all PHP execution | upstream static-php-cli `feat/pgo-v3` merging to v3 ([php-sdk#35](https://github.com/ephpm/php-sdk/issues/35)); still open |
| NTS prefork | removes the ZTS tax (~50% on allocation-heavy loops, 5–10% typical) | decision rule runs **after** PGO lands — [nts-prefork](/roadmap/nts-prefork/). Still DESIGN, gated on a measurement; PGO plausibly returns 5–10% to *both* ZTS and NTS, which is why the ZTS-vs-NTS call is made post-PGO |
| Turso engine **GA label** | (structural wins already shipped) | The engine itself **shipped in v0.7.0** — the sqld sidecar is gone and CDC clustering is in-process; the sidecar-elimination win is banked. What is still gated is the **GA label**: Turso is Beta upstream, and of its five decision gates the file-format round-trip (gate 3) is **MET** while gates 1/4/5 remain open — [turso-engine](/roadmap/turso-engine/) |

## Unexplored — next audit sweep

Static-file serving internals (sendfile, pre-compressed asset cache),
TLS session resumption configuration, PGO/BOLT for the Rust binary
itself, KV compression threshold behavior, HTTP/2 tuning. No claims
until measured.

## Ground rules (how items earn their place)

1. **Measured before merged** — every row above traces to a benchmark,
   a profile, or an audited code path with file:line. Estimates are
   labeled as estimates.
2. **Verified on the artifact** — headline numbers are re-measured on
   the built release image before they appear in release notes.
3. **Guarded after shipping** — silent-regression classes get CI
   guards (the SHA-NI symbol check, the opcache-enabled e2e) so wins
   can't quietly evaporate.
