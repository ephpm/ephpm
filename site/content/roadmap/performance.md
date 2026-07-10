# Performance Roadmap — The Master List

> **Status: LIVING DOCUMENT.** Every performance improvement found by the
> July 2026 measurement campaign (the five-way runtime comparison, the DB
> access-path matrix, the KV latency matrix, and three code audits), with
> its measured or estimated win and current state. Items move from
> "backlog" to "shipped" with receipts — numbers measured on the release
> artifact, per the [benchmarks discipline](/roadmap/benchmarks/).
> Last full revision: 2026-07-10, around the v0.4.1 release.

## Shipped in v0.4.1

| Item | Measured win | Where |
|---|---|---|
| `TCP_NODELAY` on every proxy, KV, and data-plane socket | **~200× per DB query** (44 ms → sub-ms; every SELECT and connect paid a Nagle + delayed-ACK stall) | [ephpm#161](https://github.com/ephpm/ephpm/pull/161), [litewire#3](https://github.com/ephpm/litewire/pull/3) |
| Hardware intrinsics restored in the PHP SDK (SHA-NI sha256, PCLMUL crc32, AVX2 base64 — disabled since the project began by one C++-only compiler flag) | sha256 **2.7×**, crc32c 2.2×; every HMAC, cache key, and ETag benefits | [php-sdk#36](https://github.com/ephpm/php-sdk/pull/36) + build guard |
| Quota-aware `worker_count` derivation + recycle default 500 → 10,000 | **+24%** worker throughput at container CPU quotas; recycle churn (one reboot per worker per ~0.25 s at 2k req/s) eliminated | [ephpm#159](https://github.com/ephpm/ephpm/pull/159) |
| litewire translate cache (LRU by query text) | **139×** on repeated queries (38.6 µs → 277 ns per translate) | [litewire#5](https://github.com/ephpm/litewire/pull/5) |
| litewire `prepare_cached` + removal of the LIMIT-0 metadata probe | prepare round-trips halved | [litewire#5](https://github.com/ephpm/litewire/pull/5) |
| litewire per-connection backends (WAL, real concurrent readers) | **52×** on hot selects vs the reopen path; also fixes cross-connection transaction isolation | [litewire#6](https://github.com/ephpm/litewire/pull/6) |
| Lazy-vhost negative-cache TTL 60 s → 2 s | regression fix: freshly deployed sites go live in seconds, not up to a minute | [ephpm#164](https://github.com/ephpm/ephpm/pull/164) |
| WordPress worker per-request lifecycle (`init`/`wp_loaded` replay) | correctness unblock for worker-mode WooCommerce; ~2 ms/request cost | [wordpress-worker v0.1.1](https://github.com/ephpm/wordpress-worker/releases/tag/v0.1.1) |

Release verification (post-fix DB matrix, sha256 ns/op on the shipped
image, KV RESP-lane parity, before/after charts) accompanies the v0.4.1
release notes.

## Backlog — v0.4.2 candidates, by estimated value

| Item | Est. win | State |
|---|---|---|
| `opcache.validate_timestamps = 0` in serve mode — deploys become events (`ephpm deploy` / `ephpm cache reset`), not per-request stat polling. Dev mode keeps instant reload; free win in immutable containers where files can't change anyway | large on real apps (WordPress stats hundreds of files); biggest on container/network filesystems | design agreed: dev/serve split, startup log naming the contract, RESP-listener requirement surfaced, `revalidate_freq=60` as the middle setting |
| DB proxy buffering: `BufReader` per stream half + one coalesced write per response (today a 1-row SELECT costs ~24 syscalls; 4 are achievable) | ~25% of the ~190 µs proxy floor | audited with file:line ([proxy audit](https://github.com/ephpm/ephpm/pull/161) era); not started |
| Rust build tuning: `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, mimalloc as `#[global_allocator]` (currently: cargo defaults, system malloc) | 5–15% across all Rust hot paths | identified; not started |
| Lazy Envelope + interned header keys (worker dispatch Phase 1 — today five PHP arrays are built per request, most never read) | 30–50% of the 120 µs worker dispatch cost | draft exists: [ephpm#160](https://github.com/ephpm/ephpm/pull/160); needs containerized e2e + A/B before promotion |
| KV store internals ([#142](https://github.com/ephpm/ephpm/issues/142)): coarse-clock LRU touch, O(1) eviction, TTL-only expiry index | ~15% of native GET (271 ns path) + latency-spike removal under memory pressure | measurement-justified by the KV matrix; not started |
| Query-stats overhead: cache metric labels per digest, short-circuit slow-path formatting | 5–10 µs per query on the SQLite lane | audited; not started |
| Proxy micro-trims: routing-loop double uppercase, fresh-connect challenge generation, pool checkout fast path | µs-class each | audited; not started |
| Worker dispatch Phases 2–3: per-worker SPSC slot ring, single-worker fast path, shared alloc sweep ([#140](https://github.com/ephpm/ephpm/issues/140)) | toward the 60–80 µs/request target | [worker dispatch fast path](/roadmap/worker-dispatch-fastpath/); gated on the Phase-0 flamegraph |

## Gated on external milestones

| Item | Est. win | Gate |
|---|---|---|
| PGO for libphp | 10–15% across all PHP execution | upstream static-php-cli `feat/pgo-v3` merging to v3 ([php-sdk#35](https://github.com/ephpm/php-sdk/issues/35)) |
| NTS prefork | removes the ZTS tax (~50% on allocation-heavy loops, 5–10% typical) | decision rule runs **after** PGO lands — [nts-prefork](/roadmap/nts-prefork/) |
| Turso engine (MVCC concurrent writes, native async I/O, sqld sidecar elimination) | removes SQLite's single-writer wall; deletes an IPC layer from clustered writes | five gates, upstream GA first — [turso-engine](/roadmap/turso-engine/) |

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
