# NTS Prefork Mode — Trading Features for the Last Few Percent

> **Status: DESIGN, gated on a measurement.** Not scheduled. This page
> exists so the "why not NTS?" question every RoadRunner/Swoole-aware
> evaluator asks has a considered answer — and so the decision, when
> made, is made on numbers.

## The question

ePHPm's Linux/macOS builds are ZTS: one process, concurrent PHP on
tokio's blocking pool, per-thread TSRM contexts. ZTS costs an
in-PHP tax — measured at roughly **5–10%** vs NTS php-fpm on tiny
scripts (July 2026, with `ZEND_ENABLE_STATIC_TSRMLS_CACHE` active) —
because every `EG()`/`CG()` access goes through thread-local
indirection.

Some users bring their own Redis and database, don't touch the
in-process KV, middleware, or clustering — and would trade all of it
for pure per-request PHP speed. Can we serve them an NTS build without
forking the codebase?

## Feasibility: the codebase already compiles both

**Windows ships NTS today** — `ZTS=0`, one request at a time behind a
mutex. The C wrapper, the `EPHPM_TLS` per-request statics, and the
build plumbing are all bi-modal already. The php-sdk pipeline builds
NTS for Windows; a `linux-<arch>-nts-gnu` tarball is one matrix entry.
Nothing about an NTS Linux artifact is destructive; it is additive
`cfg` + artifacts.

The hard part is not the build — it is that NTS means **one PHP
request at a time per process**, so a useful NTS Linux mode is
*prefork*:

## Design sketch

`ephpm serve --prefork N` (or `[server] prefork = N`, NTS builds only):

- N processes, each: full ephpm (async HTTP, static files) + one NTS
  PHP runtime, `SO_REUSEPORT` on the listen socket, kernel balances
  connections. No IPC, no master/worker protocol — each process is
  just ephpm with PHP concurrency 1.
- **Worker mode composes perfectly**: NTS + boot-once persistent app
  per process ≡ the classic RoadRunner/FrankenPHP worker shape. This
  is where prefork's numbers would be strongest.
- **Feature gating, honest by construction**: `[cluster]` and the RESP
  listener refuse to start in prefork mode (per-process KV across N
  processes is a correctness trap, not a feature); `[php] workers`
  ignored; startup banner states the mode's contract plainly.

### The two genuinely hard problems

1. **OPcache memory.** php-fpm children *share* one opcache SHM via
   fork-inherited mmap. N independent ephpm processes each pay a full
   opcache (128 MB × 16 processes is real memory). The fix — fork
   workers after PHP init so the SHM is inherited — collides with
   Rust: forking a threaded tokio process is UB territory. The safe
   variant (fork before any runtime/thread starts, init PHP after)
   loses the sharing. Options, in order of preference: measure whether
   per-process opcache is simply acceptable (it usually is at ≤ 16
   workers); explicit `opcache.file_cache` on tmpfs as a warm-start
   compromise; early-fork with post-fork PHP init and shared-nothing
   opcache. No fork-after-tokio under any circumstance.
2. **Ops story dilution.** "One process, one binary" is a core pitch;
   prefork makes ePHPm a process *family*. Signals, logs, metrics
   (per-process `/metrics` needs aggregation or a `--metrics-socket`
   design), graceful reload — each is small, together they are the
   real cost.

## The gate: measure before building

PGO for the SDK (php-sdk#35) plausibly returns 5–10% to **both** ZTS
and NTS builds at a fraction of this complexity — potentially erasing
the entire motivation. Therefore:

1. Cut one `linux-x86_64-nts-gnu` SDK tarball (one matrix entry).
2. Build ephpm against it with the existing Windows-style serialized
   path — no prefork work at all.
3. A/B single-stream `hello.php` / `cpu.php` / one framework profile,
   ZTS vs NTS, same host, after PGO lands.

**Decision rule:** real tax < 5% after PGO → close this page as
"considered, declined — PGO ate the motivation." Tax ≥ 10% → prefork
graduates to a scheduled milestone. In between → judgment call with
numbers on the table.

The experiment is an afternoon with the existing bench tooling; the
full prefork build-out is 1–2 focused weeks. Spend the afternoon first.
