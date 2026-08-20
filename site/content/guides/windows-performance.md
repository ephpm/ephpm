+++
title = "Getting the Most from PHP on a Windows Box"
weight = 19
+++

PHP on Windows is measurably slower than PHP on Linux — roughly **2x on
pure CPU work** and **~10x on filesystem-metadata work** (`stat`,
`file_exists`), on the same hardware. The mechanisms are compiler- and
OS-architectural, not misconfiguration; the full source-verified analysis
with all measurements is at
[Why PHP Is Slower on Windows](/analysis/php-on-windows/).

This guide is the practical half, and it is deliberately **workload-honest**:
the levers that transform a CPU-bound benchmark do almost nothing for a
typical web app, and vice versa. Know which problem you have before you
reach for a lever.

**Triage in one table** (all our measurements, Ryzen 9 5950X, PHP 8.5.x —
see the [analysis page](/analysis/php-on-windows/) for setup):

| Your workload | Windows penalty | What helps | Measured effect |
|---|---|---|---|
| CPU-bound PHP (loops, crypto, image math, parsers) | ~2.0–2.2x | TAILCALL build, JIT | **1.77x / 2.39x** on our CPU loop |
| Typical web app (framework, autoloader, templates) | mixed, mostly filesystem | serve-mode defaults, fewer file ops | single digits from runtime levers; the win is cutting file operations |
| File-metadata-heavy (dev mode, deep `vendor/`, cache-miss storms) | ~10x on the metadata itself | fewer `stat`s: prod opcache settings, classmaps, Dev Drive | eliminates *repeat* cost; first touch stays expensive |

For calibration: on the Symfony demo (dev mode, c=16) the TAILCALL build was
worth **+3%** end-to-end and the JIT **~0%** — real apps are
filesystem-bound on Windows, and no interpreter lever changes that.

---

## CPU-bound work: two big levers

### The TAILCALL build

> **v0.7.3, experimental, PHP 8.5 only.** Tracked in
> [#329](https://github.com/ephpm/ephpm/issues/329); status and exact
> artifact names below may shift while the lane soaks — check the release
> notes of the version you download.

Every MSVC-built PHP — php.net's zips, FrankenPHP's Windows build, and
ePHPm's regular Windows artifact — runs the Zend VM's slow **CALL**
dispatch, because MSVC cannot compile the fast GCC-only HYBRID interpreter.
PHP 8.5 added a third dispatch, **TAILCALL**, that recovers HYBRID's speed
and compiles under clang-cl — which is MSVC-ABI-compatible, so a clang-built
`php8embed.lib` links into ePHPm's unchanged Rust pipeline.
([Full mechanism.](/analysis/php-on-windows/#2-the-interpreter-half-msvc-gets-a-slower-virtual-machine))

Measured end-to-end through ePHPm on Windows: **1.6–1.7x faster on
CPU-bound PHP** than the MSVC artifact (serve-mode CPU loop: 3.14 ms vs
5.56 ms; reference loops in #329 agree).

- It ships as a separate, suffixed download:
  `ephpm-vX.Y.Z+php8.5.7-windows-x86_64-tailcall.tar.gz` alongside the
  regular `ephpm-vX.Y.Z+php8.5.7-windows-x86_64.tar.gz`.
- Release builds from source select it with `cargo xtask release --target
  windows --variant clang` (pending the #329 integration; on a released
  v0.7.3 the prebuilt artifact is the supported route).
- **PHP 8.5 only.** The TAILCALL VM does not exist in PHP 8.3/8.4; those
  minors stay MSVC/CALL.
- Experimental means experimental: the clang lane is new, non-gating in CI,
  and has had less soak time than the MSVC artifact. Run your own test suite
  against it before production.

### The JIT

The opcache JIT compiles hot code to native machine code, which doesn't run
on the C-compiled interpreter loop at all — so the MSVC dispatch penalty
dissolves wherever the JIT lands. On our CPU loop it was the single biggest
lever: **2.39x** (5.56 ms → 2.33 ms), faster than Linux's HYBRID
*interpreter* on the same workload.

PHP leaves the JIT off by default (`opcache.jit=disable`), and so does ePHPm
— it helps CPU-bound work and can regress the I/O-bound request path typical
of web apps, so it is a deliberate opt-in. Enable it through
`[php] ini_overrides`:

```toml
[php]
ini_overrides = [["opcache.jit", "tracing"]]

# Optional: pin the JIT code buffer (MB). When unset, ePHPm pre-sizes a
# buffer automatically in serve mode (~1/64 of the memory budget, clamped
# 32–64 MB), so enabling JIT needs no other config change.
opcache_jit_buffer_size = 128
```

`ini_overrides` is a list of `[directive, value]` pairs applied after
`ini_file` (if set), so it wins over both. Two caveats, both measured or
verified:

- **Expect ~0% on filesystem-bound apps.** Symfony demo, dev mode, c=16:
  no measurable change. The JIT is a CPU lever, not a web-app lever.
- **Multi-tenant deploys never reclaim JIT buffer.** In multi-site mode,
  a per-vhost `ephpm deploy` / `opcache_invalidate()` invalidates the
  opcode cache entries but the JIT'd code they produced stays resident —
  the JIT buffer only grows until process restart (measured). On a
  many-tenant box with frequent deploys, either size
  `opcache_jit_buffer_size` for the accumulation and restart on a schedule,
  or leave the JIT off.

---

## Real web apps: cut file operations, don't chase the interpreter

Runtime levers move a framework app by single digits on Windows. The real
budget line is filesystem metadata — each `stat`/`file_exists` costs ~10x
Linux, and a dev-mode framework request performs hundreds of them. Every
item below works by *reducing how many you pay for*.

### 1. Run `ephpm serve`, not `ephpm dev`

The two modes resolve different PHP defaults, and on Windows the difference
is worth far more than on Linux because each avoided `stat` costs more:

| Setting | `ephpm dev` (default) | `ephpm serve` (default) |
|---|---|---|
| `opcache.validate_timestamps` | `1` — re-`stat` cached scripts so edits appear instantly | `0` — trust the cache; **no per-include stat storm** |
| `realpath_cache_size` | PHP default (`4M`) | `16M` |
| `opcache.memory_consumption` | PHP default (128 MB) | auto-sized ~18% of memory budget, clamped 64–512 MB |
| `opcache.max_accelerated_files` | PHP default | `20000` |
| `zend.assertions` | `1` (active) | `-1` (compiled out) |

All of these are `[php]` knobs (`opcache_validate_timestamps`,
`realpath_cache_size`, `opcache_memory_consumption`,
`opcache_max_accelerated_files`, `zend_assertions`) if you need to pin a
value explicitly in either mode — see the
[config reference](/reference/config/).

With `validate_timestamps` off, code changes go live via `ephpm deploy` /
`ephpm cache reset`, which invalidate OPcache through the RESP listener
(deploys-are-events). Note the coupling: if you disable the RESP listener
(`[kv.redis_compat] enabled = false`) there is no invalidation lever left
and cached code can only change with a restart — startup logs a WARN about
exactly this.

Also run your *framework* in prod mode (`APP_ENV=prod`, `config:cache`,
`route:cache`, …). Dev-mode frameworks stat aggressively by design; our
measured 5–7x Symfony-demo gap was dev mode. That is expectation-setting for
local development on Windows, not a production characteristic.

### 2. Authoritative autoloader classmaps

Composer's default autoloader probes the filesystem for classes it hasn't
seen. On Windows each miss is an order-of-magnitude more expensive, so make
the classmap authoritative — the autoloader then never stats for a class
that isn't in the map:

```
composer dump-autoload --classmap-authoritative
```

(or `--optimize-autoloader` if your app legitimately generates classes at
runtime).

### 3. Windows Defender folder exclusion: do it, expect ~3%

Excluding your application tree from Defender scanning recovered **~2.8%**
throughput on our metadata benchmark — measured with and without. Worth
doing, cheap, and honestly reported: it is nowhere near the main penalty,
because an exclusion skips *scanning* but the filter drivers stay attached
to the volume and still see every I/O request.

### 4. Dev Drive: the Microsoft-side option

[Dev Drive](https://learn.microsoft.com/en-us/windows/dev-drive/) is a ReFS
volume that attaches only a minimal set of filesystem filter drivers —
Microsoft's own acknowledgment that the filter stack costs real throughput.
Microsoft cites *"up to 30% better performance"* on file-heavy developer
workloads. We have not benchmarked PHP on a Dev Drive ourselves, so treat
that figure as Microsoft's, not ours — but putting the application tree
(especially `vendor/`) on one is the only lever on this page that attacks
the per-`stat` cost itself rather than the count.

---

## The floor: what you cannot fix

An NTFS metadata operation from PHP is structurally more expensive than its
Linux counterpart: per-call UTF-8→UTF-16 conversion, path canonicalization,
a full `CreateFileW` handle open through the NT filter stack — versus one
`statx()` served from the dentry cache
([the verified chain](/analysis/php-on-windows/#3-the-filesystem-half-why-metadata-ops-cost-10x)).
No PHP or ePHPm setting changes that cost. Everything above works by paying
it less often; the first touch of every file is full price. A Windows box
tuned with everything on this page runs CPU-bound PHP at or near Linux speed
(with JIT, sometimes past the Linux interpreter) — and still pays more than
Linux for every cache-missing file operation. Plan capacity accordingly.

---

## What Windows genuinely lacks in ePHPm

Set expectations before you deploy; none of these are silent — each logs a
startup WARN when the configuration touches it.

- **`[php] max_execution_time` is not natively enforced.** The Windows PHP
  SDK has no per-thread execution timers, and PHP's process-wide fallback
  timer is unsafe on tokio worker threads, so it stays disabled. The
  per-request ceiling actually in force is `[server.timeouts] request`
  (default 300 s), enforced at the HTTP layer as a 504. Keep it set to
  something sane — on Windows it is the *only* runaway-request backstop, and
  the catchable "Maximum execution time exceeded" fatal you may rely on on
  Linux never fires.
- **No crash containment.** The experimental stack-overflow containment
  (`[php] crash_containment`, `fpm_engine = "pool"`) is Unix-only: it is
  built on POSIX signal handling, and Windows delivers faults as SEH
  exceptions instead. On Windows the setting (and
  `EPHPM_PHP__CRASH_CONTAINMENT`) is ignored, and a native fault in an
  extension or the engine takes the process down with the usual fatal-signal
  diagnostic. See [Diagnosing Crashes](/guides/diagnosing-crashes/).
- **Clustered mode is untested on Windows.** Single-node embedded Turso and
  the MySQL/PostgreSQL proxy work; the clustered Turso CDC replication path
  is tested on Linux/macOS only. Treat Windows as a single-node platform.

What Windows does **not** lack anymore: ePHPm on Windows is fully ZTS with
concurrent request execution (the old one-worker clamp is gone, #326),
worker mode works, and opcache is statically compiled in and enabled with no
ini file. The "PHP on Windows can't cache opcodes" folklore is dead —
see [the analysis page](/analysis/php-on-windows/#4-what-does-not-explain-the-gap).

---

## A tuned production config, in one block

```toml
# ephpm.toml — Windows production box, single site
[server]
listen        = "0.0.0.0:8080"
document_root = "C:/apps/myapp/public"

[server.timeouts]
request = 60          # the only runaway-request ceiling on Windows

[php]
# serve mode already defaults validate_timestamps off; pinned here so the
# intent survives someone running the same config under `ephpm dev`:
opcache_validate_timestamps = false

# Opt-in JIT — only if your workload is CPU-bound (see caveats above):
# ini_overrides = [["opcache.jit", "tracing"]]
```

Run it with `ephpm serve --config ephpm.toml`, deploy code with
`ephpm deploy` (or `ephpm cache reset`), and for CPU-heavy workloads on
PHP 8.5, try the experimental `-tailcall` artifact.
