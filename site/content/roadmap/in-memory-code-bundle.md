# In-Memory Application Code Bundle

> **Status: PLANNED — EXPERIMENTAL POC, not production-hardened, `off` by
> default.** A working proof-of-concept exists behind `[php] code_bundle`
> (`crates/ephpm-php/src/code_bundle.rs`, `code_bundle_hooks.c`).
>
> ## Round 3 (2026-08-22): measured against a REAL Composer application
>
> Everything before this section was measured against a **synthetic** 400-class
> fixture whose autoloader probed with `is_file`. Real Composer probes with
> `file_exists`, so those numbers never described a real application. This round
> replaces them with **Laravel 13.26.1** installed by real Composer (163
> packages, 8104 `.php` files, 38 MB of source, the genuine
> `vendor/composer/ClassLoader.php` whose `findFileWithExtension` probes with
> `file_exists` at line 506).
>
> ### 1. The `VCWD_*` wall is not a wall — `file_exists` IS frontable
>
> The previous round concluded that `file_exists`, `is_readable`, `realpath` and
> `glob` were unreachable without patching PHP, because they call the `VCWD_*`
> macros directly rather than going through a swappable stream-wrapper op. That
> reasoning was about the *streams* layer and missed the layer above it: **PHP
> stores every internal function's `handler` as a plain function pointer in
> `CG(function_table)`, and it can be swapped at runtime.** OPcache itself does
> exactly this in `zend_accel_override_file_functions()` to implement
> `opcache.enable_file_override`. No SDK patch is involved.
>
> ePHPm now does the same for **`file_exists` and `realpath`**, installed once on
> the single-threaded startup path *after* every MINIT — so the handler we save
> and delegate to on a miss is whatever OPcache installed, and the two
> accelerations compose instead of clobbering each other. ZTS is safe because the
> function table stores *pointers* to shared `zend_internal_function` structs, so
> one swap at startup is process-wide; a new thread's table copy points at the
> same struct.
>
> **This overturns the previous round's headline finding that 0 % of a real
> Composer request was frontable.** Isolated on a single binary with a single
> env var, against real Laravel on Linux:
>
> | | file syscalls / request | p50 |
> |---|---|---|
> | `sealed`, override **off** | 1222.6 | 7.24 ms |
> | `sealed`, override **on** | **160.6** | 6.38 ms |
> | `lazy`, override **off** | 1222.6 | 7.42 ms |
> | `lazy`, override **on** | **396.6** | 6.65 ms |
>
> The overrides are **on** whenever the bundle is on, with
> `EPHPM_BUNDLE_FRONT_FILE_EXISTS=0` as a field kill switch. (They were briefly
> suspected of causing a Windows crash; §7b has the control run that exonerated
> them.)
>
> `is_readable`/`is_writable`/`is_executable` are deliberately **not** overridden:
> their answer depends on the process's effective uid, the only correct way to
> get it is `access(2)` — which is the syscall we are trying to remove — and
> answering from the index would claim a mode-000 file is readable.
> `glob`/`opendir` are analysed below and are **not** implemented.
>
> ### 2. Lazy read-through cache (`code_bundle = "lazy"`)
>
> The index is no longer built up front. A lookup that misses performs exactly
> the filesystem operation PHP was about to perform, answers from it, and keeps
> the result. Consequences:
>
> * `code_bundle_max_bytes` is an **LRU eviction bound**, not an
>   all-or-nothing refusal. Memory is bounded by the working set.
> * The boot scan survives as an **optimization** (`code_bundle_boot_scan`,
>   default on) that publishes entries progressively as it walks. It is no longer
>   a correctness dependency: "not scanned yet" and "not cached yet" are the same
>   state and both mean "fall through to disk". A scan failure warns and stops.
> * **`sealed` is impossible under `lazy` and the combination is a startup
>   error.** A cache that fills on demand and can evict cannot prove anything
>   from absence. `lazy` therefore never eliminates an autoloader's *miss*
>   probes — it accelerates hits only, and a miss costs a fresh syscall every
>   time.
>
> ### 3. Correctness fix that lands regardless: `FileEntry::canon`
>
> The index stored the **raw config-spelled** document-root path, not the
> OS-canonical one, and that string becomes `__FILE__`, `__DIR__`,
> `get_included_files()`, the `require_once` de-dup key and OPcache's
> `opened_path`. Varying *only* the `document_root` spelling: a forward-slash
> spelling made `require_once` run a file **twice** ("Cannot redeclare"), and with
> timestamp validation on produced **402 OPcache misses / 0 hits per request**
> (~11× *slower* than leaving the bundle off) — silently. Now canonicalized once
> at the walk root, pinned by `canon_is_independent_of_docroot_spelling` across
> eight docroot spellings plus a relative and a symlinked one.
>
> ### 4. Measured: real Laravel 13, warm p50 and filesystem ops per request
>
> Same host, quiet machine, one server **process per cell** (running cells in one
> harness process reproducibly killed every server after the first), 60 warm +
> 60 measured keep-alive requests, and the measured window never starts before
> the index is live. Windows "ops" = per-process `OtherOperationCount` delta per
> request; Linux "ops" = `strace -c -e trace=%file` calls per request. Response
> body md5 verified **identical** across `off`/`lazy`/`scan`/`sealed` on both
> platforms.
>
> **Default Composer autoloader (`composer dump-autoload`, PSR-4 probing):**
>
> | | Windows p50 / ops | Linux p50 / ops | Windows RSS |
> |---|---|---|---|
> | `code_bundle = "off"` | 25.50 ms / 1433 | 6.65 ms / 1283 | 58 MB |
> | `sealed`, **without** the function override | 25.40 ms / 1313 | 6.79 ms / 1223 | 116 MB |
> | `opcache.enable_file_override=1`, bundle **off** | 14.07 ms / 603 | 5.19 ms / 457 | 58 MB |
> | `lazy` (boot scan on) | 14.52 ms / 297 | 5.93 ms / 397 | 109 MB |
> | `lazy` (boot scan **off**) | 13.52 ms / 297 | 5.98 ms / 397 | 65 MB |
> | `scan` | — | 5.78 ms / 397 | — |
> | `sealed` (`vendor`) | **13.07 ms / 189** | **5.63 ms / 161** | 102 MB |
> | `sealed` + `enable_file_override` | 12.64 ms / 189 | 5.52 ms / 161 | 102 MB |
>
> The `sealed`-without-override row is the control that proves the point: it is
> indistinguishable from `off` (25.40 vs 25.50 ms). **Every bit of the bundle's
> effect on a real Composer application comes from the `file_exists` handler
> override.** With it, Windows drops 25.50 → 13.07 ms (−49 %) and 1433 → 189 ops
> (−87 %).
>
> ### 5. …but two zero-code alternatives already get you there
>
> **`composer dump-autoload -o`** replaces PSR-4 probing with a classmap, so
> there is almost nothing left to front:
>
> | Autoloader | mode | Windows p50 / ops | Linux p50 / ops |
> |---|---|---|---|
> | default PSR-4 | `off` | 25.50 ms / 1433 | 6.65 ms / 1283 |
> | default PSR-4 | `sealed` | 13.07 ms / 189 | 5.63 ms / 161 |
> | **`-o` classmap** | **`off`** | **12.54 ms / 321** | **5.03 ms / 233** |
> | `-o` classmap | `lazy` | see below | 5.13 ms / 161 |
> | `-o` classmap | `sealed` | see below | **4.62 ms / 161** |
> | classmap-authoritative | `off` | — | 4.88 ms / 233 |
> | classmap-authoritative | `sealed` | — | 4.61 ms / 161 |
> | classmap-authoritative | `enable_file_override` | — | 4.57 ms / 221 |
>
> On Windows, **one `composer dump-autoload -o` (12.54 ms) matches the entire
> feature in its best configuration (13.07 ms) and costs 40 MB less RAM.** On
> Linux it is outright faster (5.03 vs 5.63 ms). `opcache.enable_file_override=1`
> — one stock ini line — gets most of the rest.
>
> ### 6. What lazy actually bought, measured
>
> * **Memory bounded by the working set.** `lazy` with the boot scan **off** used
>   65 MB on Windows / 114 MB on Linux versus 102–116 MB / 153–183 MB for the
>   eager index, and produced **identical** ops per request (297 Windows,
>   397 Linux). The full 38 MB index is ~55 MB of RSS that the request path never
>   needed.
> * **No cold-start cliff.** The eager scan of this 8104-file tree took
>   **78–111 s on Windows with a cold OS file cache** (1–2 s warm), during which
>   `scan`/`sealed` serve entirely unaccelerated. `lazy` with the boot scan off is
>   ready in 4 ms and warms as it serves.
> * **The cost of never caching a negative.** Because absence proves nothing,
>   `lazy` re-checks every non-existent path on every request. Per-operation on
>   Linux (`file_exists` on a path that does not exist): 1.055 µs `off` →
>   1.270 µs `lazy` — **20 % slower**, since the bundle lookup is added on top of
>   the syscall that still happens. This is exactly why `lazy` lands at 397 ops
>   and `sealed` at 161.
> * **Read-path cost of making the index mutable**, measured not assumed:
>   a `file_exists` hit costs **0.080 µs** against the immutable `HashMap`
>   (`scan`) and **0.146 µs** against the sharded concurrent map (`lazy`) —
>   **+66 ns per lookup**. Against a 14–50 µs Windows metadata syscall that is
>   noise; against a 1 µs Linux one it is 7 %.
> * **The `require_once` tax is real and slightly worse under lazy**: 0.016 µs
>   `off` → 0.107 µs `scan` → 0.188 µs `lazy`, because `zend_resolve_path` runs
>   before the already-included short-circuit. Zero syscalls, so syscall-based
>   measurement misses it entirely; ~0.17 µs/call only matters at 10^5 calls.
>
> ### 7. Correctness: what got fixed, and what is still true
>
> Fixed and pinned by tests: the `canon` docroot-spelling bug (§3); `fileperms()`
> reporting `0100444` while `is_writable()` said writable (the index now carries
> the real read-only bit, and reports `0644` when it does not know); `filemtime()`
> on a directory returning 0 (directories now fall through to a real stat);
> symlinked directories being skipped by the scan entirely (now followed, with a
> cycle guard); and a `sealed` root refusing to arm when the scan could not
> enumerate it exhaustively (it contains a symlinked directory, or a `.php` file
> that exists but could not be read — which, now that `file_exists` is fronted,
> would otherwise be a lie visible to every autoloader).
>
> **Still true, and inherent:** a bundled file replaced by an out-of-process
> deploy keeps serving the bytes and mtime captured when it was first read.
> **Lazy does not fix this — it relocates the freeze from "boot" to "first
> touch", which is arguably worse**, because different files freeze at different
> times and a deploy can be observed half-applied. The escape hatch is the
> existing `ephpm deploy` / `ephpm cache reset` path, which now clears the code
> cache **before** invalidating OPcache (reversed, an in-flight request
> repopulates OPcache from bytes about to be discarded and
> `validate_timestamps=0` never corrects it). Verified end-to-end. An in-process
> write to a `.php` file invalidates just that entry, which the immutable index
> could never do.
>
> ### 7b. A Windows tracing-JIT crash found along the way — **not** the bundle's
>
> Worth recording in full, because it was wrongly attributed twice before the
> control run settled it, and because it is a live robustness problem for ePHPm
> on Windows independent of this feature.
>
> **Symptom.** `0xC0000005` in `ephpm.exe` after **exactly 3 requests** of a real
> Laravel application on Windows. Deterministic. ePHPm logs nothing — the only
> evidence is Windows Event Log `Application` / Id 1000 (faulting module and
> offset), matching the known "ePHPm logs NOTHING on a segfault" behaviour.
>
> **The control that settled it:** `code_bundle = "off"` — every line of the
> bundle inert, no hooks armed, no handler overrides installed — **crashes 3/3
> on the same binary**, while the pre-change binary (`902fec1`) runs the same
> tree and config **3/3 clean at 150 requests each**.
>
> | binary | config | app | result |
> |---|---|---|---|
> | `902fec1` | `sealed`, JIT on | `-o` classmap | 3/3 alive, 150 req |
> | this build | **`off`**, JIT on | `-o` classmap | **3/3 crashed after 3 req** |
> | this build | `off`, JIT on | PSR-4 | alive, 150 req |
> | this build | `sealed`/`scan`/`lazy`, **JIT off** | `-o` classmap | alive, 150 req |
> | this build | any mode, JIT on/off, overrides on/off | Linux | alive, 300 req, answers identical |
>
> So it is **not** the code bundle, **not** the handler overrides, and **not**
> Linux. It depends on (a) the JIT being on, (b) which application code goes hot,
> and (c) *which binary* — i.e. it is a code-layout/codegen lottery, the same
> family as the already-known per-binary Windows failure where some ePHPm builds
> die at startup with PHP's *"Opcode handlers are unusable due to ASLR"*. A
> release build can lose this lottery and crash-loop on a customer's application
> with the default `opcache_jit = "tracing"`. **That deserves its own issue.**
>
> Every performance number in this document was taken from a run verified alive
> at the end of its measured window, and the JIT-off cross-check agrees with the
> JIT-on figures (`-o` classmap, JIT off: `sealed` 189 ops vs `off` 321).
>
> **Hypotheses eliminated on the way (all cost real time):**
> * *Hand-rolled frame access.* The first cut of the overrides read the argument
>   with `ZEND_NUM_ARGS()` + `ZEND_CALL_ARG(execute_data, 1)`. Switching to
>   `ZEND_PARSE_PARAMETERS` — what OPcache's own `accel_common_file_func()` does
>   — **did not change the crash**. The change was kept anyway: it is the correct
>   way to write an internal-function handler.
> * *Frameless internal calls* (PHP 8.4+ `ZEND_FRAMELESS_ICALL_*`). `file_exists`
>   has no frameless variant in this build; only `class_exists` and
>   `property_exists` do.
> * *Calling convention.* `zif_handler` carries `ZEND_FASTCALL` (`__vectorcall`
>   on MSVC); a mismatch is a hard compile error there, which is how the first
>   version was caught by the Windows PHP-linked CI gate.
>
> **Method note worth keeping:** the first A/B (pre-change binary, n=1) pointed
> the wrong way, and a config bisect *within* the feature (sealed/scan/lazy) kept
> agreeing with the wrong conclusion because every one of those cells has the
> bundle on. **The control that mattered was turning the feature off entirely on
> the same binary.** When bisecting inside a feature keeps confirming your
> hypothesis, test the feature-off case on the same binary before believing it.
>
> The overrides remain **on** by default when the bundle is on (without them the
> bundle measurably does nothing), with `EPHPM_BUNDLE_FRONT_FILE_EXISTS=0` as a
> field kill switch.
>
> ### 8. Go / no-go
>
> **No-go as a default, no-go as a headline feature. Keep it experimental and
> off.** The mechanism now works — the function-handler override is a real and
> reusable discovery, and 1433 → 189 filesystem ops on a real Laravel request is
> not a small number. But the honest comparison is against what an operator can
> do for free:
>
> * On a **correctly configured** app (`composer dump-autoload -o`, which is
>   standard production practice) the bundle's remaining headroom is small, and
>   on Windows a plain `-o` already matches the bundle's best case using 40 MB
>   less memory.
> * The one configuration where the bundle wins decisively — `sealed` — is
>   exactly the one that trades correctness for speed, cannot be combined with
>   lazy population, and has to refuse to arm in several real-world layouts.
> * `lazy` is the right *shape* for a cache (bounded memory, no cold-start
>   cliff, no all-or-nothing cliff) but by construction cannot eliminate the miss
>   probes that are the whole cost.
>
> **What is worth keeping regardless of the feature's fate:** the `canon` fix
> (a live correctness bug), the function-handler override mechanism (useful
> anywhere ePHPm wants to interpose on a PHP builtin), and the finding that
> `composer dump-autoload -o` plus `opcache.enable_file_override=1` is the
> advice to give Windows users today.
>
> ### POC findings from earlier rounds (synthetic fixture — superseded above)
>
> - **The source-read path works and is opcache-safe.** `require`/`include` and
>   the cold compile serve from RAM even with OPcache enabled — proven by
>   deleting every autoloaded source off disk after boot and still cold-compiling
>   a 400-class autoloader (`files_ok=1`, so `__FILE__`/`__DIR__` still resolve).
>   This required overriding the plain-files **`stream_opener`**, not just
>   `zend_stream_open_function`: OPcache captures the *original*
>   `zend_stream_open_function` at startup and calls its saved copy.
> - **`is_file`/`stat`/`filemtime` are frontable** via the plain-files `url_stat`
>   op. **`file_exists()` is not** — on *both* Windows and Linux it short-circuits
>   through `VCWD_ACCESS` (an access check), which never reaches the stream
>   wrapper. Real Composer's PSR-4 autoloader probes with `file_exists`, so that
>   path still needs the SDK patch; the earlier claim that this was a
>   Windows-only quirk was wrong.
> - **The overlay's dominant cost was misses, not hits.** On a warm 400-class
>   autoload with two decoy directories, **88% of the request was filesystem
>   probes for paths that do not exist**. `"scan"` accelerated the 400 hits and
>   did nothing for the 800 misses. Answering those from the index — sealed
>   roots — is what actually pays.
> - **OPcache was silently refusing to cache anything on Linux.** The file handle
>   the bundle handed the compiler claimed `ZEND_HANDLE_STREAM` but was not a
>   `php_stream`, so `zend_get_file_handle_timestamp()` could not obtain a
>   timestamp and (via `file_update_protection`, which applies even with
>   `validate_timestamps=0`) refused to cache the script at all:
>   `num_cached_scripts=0`, every file recompiled on every request. Serving a
>   real `php_stream` whose `stat` op reports the index's mtime fixed it, and
>   removed a latent wild-pointer dereference at the same time.
>
> ### Measured, warm p50 (400-class Composer-model autoload, 2 decoy dirs)
>
> Same host, quiet machine, 40 warm + 60 measured keep-alive requests per cell,
> one server process per cell. "syscalls" = Windows per-process I/O operations
> per request / Linux `strace -c` file-metadata calls per request.
>
> | | bundle **off** | `"scan"` (overlay) | `"sealed"` (`vendor`) |
> |---|---|---|---|
> | Windows, `is_file` probe | 20.60 ms / 2414 | 11.03 ms / 814 | **1.47 ms / 14** |
> | Windows, `file_exists` probe | 17.96 ms / 1614 | 18.17 ms / 1614 | 18.30 ms / 1614 |
> | Linux (WSL2, ext4), `is_file` | 2.51 ms / 2403 | 3.75 ms / 1603 | **0.93 ms / 3** |
> | Linux, `file_exists` | 2.36 ms | 4.06 ms | 2.42 ms |
>
> - **Windows overtakes the Linux baseline.** On the frontable probe the gap to
>   Linux went from 8.2× (20.60 vs 2.51 ms) to 1.6× (1.47 vs 0.93 ms), and
>   Windows-sealed is *faster than Linux with the bundle off*. Metadata syscalls
>   per request fall to 14 on Windows and 3 on Linux — the floor.
> - **`file_exists` does not move at all**, on either platform. That row is
>   gated on the SDK patch, not on anything the portable hooks can do.
> - The `"scan"` column above is the *fixed* build. Before the OPcache fix,
>   `"scan"` on Linux was **net-negative** (3.75 ms vs 2.51 ms off) purely
>   because nothing was being cached; `num_cached_scripts` was 0 and
>   `opcache_get_status()` misses climbed by one per file per request (81 804
>   over a 203-request run) instead of staying flat at 403. Windows was never
>   affected, because its OPcache reads the timestamp from the real file first
>   (`zend_get_file_handle_timestamp_win`) — an accidental dependency that a
>   prebuilt-bundle mode, with no sources on disk, would fall straight off.
>
> The original design text below is preserved. `"file"` (prebuilt `.ebundle`)
> and multi-site bundles remain unimplemented.

## Problem

PHP on Windows is slow in a way that a warm OPcache does **not** fix: the
win32 filesystem metadata/open chain. Every `stat`, `file_exists`, `is_file`,
`realpath`, `opendir`, and cold source read funnels through
UTF-8 → UTF-16 conversion → `CreateFileW` / `GetFileAttributesExW` /
`FindFirstFileExW` → the NTFS path parser → the Defender minifilter stack.
Measured on this host that is **~50 µs per metadata syscall** (see the ceiling
section) versus ~1–3 µs for Linux `statx`.

OPcache caches compiled *bytecode* in shared memory, but the filesystem
syscalls survive a warm opcache:

- **Timestamp validation** — with the default `opcache.validate_timestamps=1`,
  opcache `stat`s every cached file on every request to compare mtime.
- **Autoloader probing** — PSR-4 autoloaders `file_exists`/`is_file` several
  candidate paths (including misses) before the class hit.
- **realpath resolution** — `require __DIR__.'/x.php'`, `realpath()`, and the
  engine's own path canonicalization walk the directory chain.
- **Cold-compile source read** — the first `open()`+read of each file when
  the opcache is empty (fresh container boot).

The proposal: bundle the document root's **`.php` code** into an
ePHPm-managed, indexed archive with an in-memory index, load it once at
startup, and back PHP's file-discovery / file-open / include path with it so
source discovery and reads resolve **from Rust memory, never NTFS**. Because a
container image is immutable per deploy, cache invalidation is trivial: the
bundle is fixed until the next deploy. Combined with OPcache and
`opcache.validate_timestamps=0`, nothing touches disk for code — warm or cold.

## The ceiling: what is actually reclaimable

Measured on this Windows 11 host (Defender active), PHP 8.5.7 ZTS, via the
shipped `ephpm php` embed interpreter against a synthetic 400-file vendor tree
(one class per file). Per-op cost, mean of 3 rounds with `clearstatcache(true)`
between phases so each op is a real syscall, not PHP's realpath cache:

| Operation (existing file)             | Per-op (Windows) | Linux `statx` (ref) |
|---------------------------------------|------------------|---------------------|
| `file_exists` / `is_file` / `stat`    | **~50–55 µs**    | ~1–3 µs             |
| `realpath` (per-component walk)       | **~73–76 µs**    | ~2–5 µs             |
| `open` + read (cold-compile source)   | **~48–72 µs**    | ~3–8 µs             |
| `file_exists` MISS (non-existent)     | **~7.3 µs**      | ~1 µs               |

The ~50 µs metadata cost is **robust to path depth** — a shallow path
(`C:\Users\luther\ceilx\vendor\...`, 6 components) and a deep temp path
(~12 components) gave the same figure, so it is Win32 + Defender per-file
overhead, not path-parser length. The MISS cost (~7 µs) matters because
autoloaders probe misses before hits.

**Method + confidence.** Single host, Defender on, synthetic tiny files (so
the open+read number is dominated by the `open` syscall, which is exactly the
tax we care about). Per-op variance across rounds was tight (~50–55 µs), so
confidence in the per-op figure is **medium-high**. The per-*request*
extrapolation below depends on a file-count assumption and is **medium**.

**Per-request ceiling.** A cold Symfony/Laravel request compiles and probes on
the order of ~300 files. Extrapolating:

- **Cold start** (empty opcache): ~300 stat + ~300 open ≈ 300×50 µs + 300×48 µs
  ≈ **~29 ms of pure filesystem syscall time**, all eliminable by the bundle.
- **Warm request, `validate_timestamps=1`** (the default): ~300 opcache mtime
  stats × ~50 µs ≈ **~15 ms per request** of stat tax, on *every* request —
  this is the steady-state killer.
- **Warm request, `validate_timestamps=0`**: opcache skips the mtime stat, but
  userland `file_exists`/`is_file`/`realpath`/`glob` in framework code still
  hit disk — typically a few ms, app-dependent.

So on this Windows host the bundle can reclaim **~15 ms/request warm** (default
opcache) down to a few ms (with `validate_timestamps=0`), and **~29 ms** off a
cold start. That is a large enough number to justify building — on Windows.

## Mechanism options + recommendation

ePHPm links `libphp.a` / `php8embed.lib` **statically into the same binary**,
and already interposes on PHP internals via three proven levers:
`--wrap` linker stubs (neutering `zend_signal_*` / `zend_set_timeout`,
`crates/ephpm-php/build.rs`), whole-archive symbol forcing, and a C shim that
*defines* symbols PHP references (`crates/ephpm-php/resolver_shim.c`). Any VFS
hook should reuse those levers rather than invent a new one.

### Option A — userland PHP stream wrapper. Rejected.
Registering a `file://` wrapper from PHP userland (`stream_wrapper_register`)
does **not** catch the hot path. The engine's `include`/`require`,
`zend_stream_open_function`, and `tsrm_virtual_cwd` realpath resolution all
use the *internal* plain-files wrapper and bypass userspace wrappers entirely;
and userland cannot override the built-in `file://`. This misses precisely the
compiler + realpath path that costs the most.

### Option B — C-level override of PHP's own indirection hooks. **Recommended (primary).**
PHP exposes several C-level function pointers *designed* to be overridden
(opcache itself overrides `zend_compile_file`). Set from `ephpm_wrapper.c`
after `php_embed_init`, reading through a leaked `Arc<Bundle>` raw pointer
(the same global-state pattern ePHPm already uses):

- **`zend_stream_open_function`** — the top-level script open and, transitively,
  every `include`/`require`. Serve the source bytes from the bundle blob.
- **`zend_resolve_path`** — path resolution for includes; return the bundle's
  canonical absolute path so `__DIR__`/`__FILE__` math stays correct.
- **A C-registered replacement for `php_plain_files_wrapper`'s stat/opendir
  ops** — so userland `file_exists`/`is_file`/`stat`/`fopen`/`opendir`/`glob`
  consult the bundle. Registered at the C level (not userland), so it *can*
  replace the built-in.

Why this is the primary lever: it needs **no SDK-source patch**, lives entirely
in ePHPm's own C wrapper (like `resolver_shim.c`), and works identically on
MSVC and GNU/lld — important because `--wrap` is a GNU/lld feature that
`link.exe` does **not** support, so an interposition strategy that depends on
`--wrap` of the win32 ioutil layer would not port to the MSVC Windows build,
which is the build that needs this most.

### Option C — `opcache.preload`. Complement, not a substitute.
Preload (PHP 7.4+) compiles a fixed file set into SHM at startup, resident
across requests, making their `include` a no-op. It is the existing **Linux**
answer and pairs well with the bundle, but it does not answer
`file_exists`/`realpath`/`glob` probing, does not help files outside the
preload set, and has app-compat constraints (preloaded files can't be
unlinked; some frameworks don't preload cleanly). Preload fixes the *compile*
layer for a subset; the bundle fixes the *filesystem* layer for everything.

### Option B2 — override the internal function *handlers*. **Implemented; this is what unlocked real Composer.**

Option B's stream-wrapper hooks cannot reach `file_exists`, `is_readable`,
`is_writable`, `is_executable`, `realpath` or `glob`, because those call the
`VCWD_*` macros directly. An earlier round concluded that closing them required
patching the PHP SDK. **That was wrong.** There is a swappable layer *above* the
streams layer: every internal function's `handler` is a plain function pointer
in `CG(function_table)`, and OPcache already swaps exactly these functions in
`zend_accel_override_file_functions()` to implement
`opcache.enable_file_override`.

ePHPm now installs the same kind of override for **`file_exists` and
`realpath`**, in `ephpm_bundle_install_hooks()`:

* **Ordering.** Installed *after* `php_embed_init()`, i.e. after every MINIT, so
  the handler saved for miss-delegation is whatever OPcache (or any other
  extension) installed. Bundle → OPcache's SHM-cache-first version → the real
  syscall. The two accelerations compose; neither clobbers the other.
* **ZTS.** The function table stores *pointers* to shared
  `zend_internal_function` structs and a new thread's table is copied by
  pointer, so a single swap on the single-threaded startup path is
  process-wide — and no other thread can observe a torn pointer.
* **Fail-safe conditions.** The fast path is taken only when a bundle is
  published, exactly one already-string argument was passed, and `open_basedir`
  is not in force (the real handlers enforce it; answering from RAM would leak
  the existence of paths outside it). Everything else delegates unchanged, so a
  stream URL, a relative path, or anything outside the document root is
  byte-for-byte untouched.
* **Deliberately excluded.** `is_readable`/`is_writable`/`is_executable` depend
  on the process's effective uid; the only correct implementation is `access(2)`,
  which is the syscall we are removing, and an index answer would call a mode-000
  file readable. Real Composer probes with `file_exists`, so nothing measurable
  is lost.

**`glob` and `opendir` are viable but NOT implemented, and are incompatible with
`lazy`.** The earlier "unreachable by any mechanism" verdict on `glob` was also
wrong: this SDK builds PHP's *bundled* glob (`php_glob`/`php_globfree` are
`PHPAPI`-exported on both Linux and Windows — `HAVE_GLOB` is not defined), and it
supports `PHP_GLOB_ALTDIRFUNC` with `gl_opendir`/`gl_readdir`/`gl_stat`
callbacks that `ext/standard/dir.c` simply never sets. Overriding `zif_glob` to
call `php_glob` with that flag and our own directory callbacks would work. The
blocker is not the mechanism, it is the *data*: serving a directory listing
requires the index to have enumerated that directory **exhaustively**, which a
lazily populated cache by definition has not. So directory fronting is a
`scan`/`sealed`-only feature. On the measured real-app workload `glob` did not
appear in the request path at all, so it is not on the critical path for a
Composer application.

### The SDK patch is no longer required.
With B2 in place there is no remaining function on the measured Composer request
path that needs a patched PHP. The `win32/ioutil.c` / `tsrm_virtual_cwd.c` patch
idea is **withdrawn as a requirement** — it remains an option only for
`is_readable`-class functions we chose not to front and for the realpath cache's
internal canonicalization, neither of which showed up as a cost on a real app.
Avoiding it also avoids a recurring per-PHP-minor maintenance cost.

**Recommendation:** Option B (stream-wrapper hooks) + Option B2 (function-handler
overrides) as core, driven by `opcache.validate_timestamps=0`. Option C
(preload) remains a complement. See the go/no-go at the end for whether the
result justifies shipping any of it.

## Correctness model — the make-or-break

Real apps do `require __DIR__.'/x.php'`, `realpath()`, `dirname(__FILE__)`,
`glob()`, `SplFileInfo`, `is_dir()`. The virtual FS must answer these with
consistent, plausible paths that resolve back into the bundle, or
Laravel/Symfony/WordPress break. Design rules:

1. **Mirror real absolute paths — do not invent synthetic ones.** The bundle
   keys on the canonical absolute path as PHP would compute it
   (`C:\app\public\index.php`, `C:\app\vendor\...`). `__FILE__`/`__DIR__`/
   `realpath()` return these real-looking paths, because that is what apps
   concatenate. `realpath()` of a bundle entry is the identity (the bundle is
   pre-canonicalized; no symlink resolution needed).

2. **Index directories, not just files.** Carry directory nodes and child
   lists (`HashMap<dir_path, Vec<child_name>>`) so `is_dir`, `scandir`,
   `opendir`/`readdir`, `glob`, and `SplFileInfo::isDir` work and return the
   same set. Preserve PHP's ordering (`scandir` sorts; `glob` has its own
   order) so cache keys derived from directory listings stay stable.

3. **Overlay semantics: bundle-first for code, fall through to disk, writes
   always go to disk.** The bundle is code-only and read-only. A lookup under
   the docroot consults the bundle first; a **miss falls through to the real
   filesystem** (config `.env`/`.yaml`, templates, translations, uploads,
   session files, generated caches all live on disk). All writes go to disk.

   **Sealed mode (`code_bundle = "sealed"`) deliberately breaks this rule** for
   one narrow class of path, and that makes it a *correctness* setting rather
   than a speed setting. See the sealed-mode contract below.

4. **Stable synthetic stat fields.** Carry a stable `mtime` per entry (deploy
   timestamp or original mtime) and a synthetic-stable `inode`. Apps use
   `filemtime` for cache-busting (asset versioning, Twig cache keys); a
   wrong/zero mtime causes perpetual cache-miss or serves-stale.

5. **Case-fold on Windows.** NTFS is case-insensitive; the index **must** be
   case-folded on Windows or `require 'Foo.php'` vs `foo.php` misses. This is a
   real footgun that will not show on a Linux dev box.

6. **Honor `open_basedir` and multi-tenant isolation.** Bundle entries are
   inside the docroot so `open_basedir` is satisfied, but the lookup must still
   respect it. In multi-site mode (`[server] sites_dir`) each site needs its
   **own** bundle keyed by the *resolved site root* (`Router::resolve_site`),
   never re-derived from the `Host` header — same invariant as per-site DBs.

### The sealed-root contract (`code_bundle = "sealed"`)

Overlay's "a miss falls through" rule is what makes the bundle safe — but it is
also why plain `"scan"` reclaims so little. A PSR-4 autoloader's probes are
mostly **misses** (one per candidate directory that does not hold the class),
and under overlay every one of them still costs a real `stat`. Measured on a
400-class Composer-model autoload, those misses were **88% of the warm
request**; the bundle sped up the hits and did nothing at all for the misses.

Sealed mode fixes that by making absence authoritative *inside subtrees you name
explicitly*: within a sealed root the scan has enumerated every `.php` file, so
"not in the index" means "does not exist" — answered from RAM with **zero
syscalls**.

Three properties keep that from being a footgun.

**1. Declared roots, not the whole docroot.** The win and the risk live in
*disjoint directories*. The misses worth eliminating are decoy probes under
`vendor/`; every framework write that could break us goes to `var/cache/`,
`bootstrap/cache/`, `storage/framework/views/`. Sealing only `vendor/` removes
the failure mode **by construction** rather than by detection.
`[php] code_bundle_sealed_paths` is **empty by default**, and with it empty
`"sealed"` behaves exactly like `"scan"` — the half of the feature that can
change an answer is unreachable without a second, explicit opt-in. A declared
path that resolves outside the document root is a **hard startup error**.

Within a declared root, the predicate is still narrow: authoritative only for a
path carrying the one extension the scan enumerates exhaustively (`.php`), and
only via a component-aware prefix test (`vendor-backup/x.php` is not inside
`vendor`). Everything else falls through exactly as under `"scan"`. The
predicate lives in one function (`Bundle::lookup`), and the scan filter and the
scope predicate share one constant, pinned by a test.

**2. Authority is a one-way latch.** A sealed root starts armed and is
**permanently disarmed** the first time anything proves the index could be wrong
about it — a write open of a `.php` file inside it, or a negative that a
disk-confirmation showed to exist. A disarmed root reverts to overlay semantics:
correct, slower, forever. Because there is no re-arm there is no
stale-generation race and no lost-event window, and the index itself is never
mutated, so every FFI pointer-lifetime contract stays valid unchanged.

**3. It fails loudly.** `include`/`require` resolution and source opens
**always confirm a negative against disk before returning it**, regardless of
settings — these are rare, and a wrong answer there is a hard failure, so the
syscall is worth paying. A mismatch logs `WARN` naming the path, disarms the
root, and falls through to the correct answer.
`[php] code_bundle_verify_negatives = true` extends confirmation to the hot
`is_file`/`file_exists`/`stat` probes too (diagnostic only — it gives back the
syscalls sealed mode exists to remove).

### Build and publication: async, atomic, fail-safe

The scan is **not** on the startup critical path — it measured 45 ms warm but
**3.7 s cold** on Windows, which is a lot of dead time on a container's first
boot. The C hooks are armed synchronously at startup (they are inert while no
index exists: every hook delegates to the saved original, which is byte-for-byte
`code_bundle = "off"` behaviour), and one low-priority background thread scans
and publishes the finished index with a single atomic `OnceLock::set`.

The resulting state machine is one-way and every pre-final state is fail-safe:

```
not-ready (fall through to disk)  →  scanned (overlay)  →  sealed roots armed
                                                              ↓ (never back)
                                                           disarmed (overlay)
```

There is **no incremental fill**. That matters most for sealed roots: a
half-built index answering negatives authoritatively would report "does not
exist" for files it merely had not reached yet — non-deterministic
class-not-found errors during warmup, the worst possible bug class. Roots are
armed only inside the constructor of a *completed* scan. If the scan fails
(permissions, tree changed mid-scan), nothing is ever published, a `WARN` is
logged, and the process stays on the fall-through path for its whole life.

### Known POC defect: `"scan"` freezes mtimes as well as bytes

Independent of the fixes above, `"scan"` mode is less safe than the original
design text implies. `url_stat` reports the **scan-time** mtime and the source
hooks serve **scan-time bytes**, so an in-place edit to a bundled file is
invisible for the life of the process — and with
`opcache.validate_timestamps = 1`, OPcache stats *through our hook*, sees the
frozen mtime, and never recompiles. The net effect is that turning the bundle on
silently disables timestamp validation. Two consequences:

- **`ephpm dev` refuses to start with the bundle enabled** (hard startup error).
  A dev server that ignores your edits is far worse than a message saying why.
- Startup warns when the bundle is on together with
  `opcache.validate_timestamps = 1`, because that combination is paying for
  `stat`s that can never observe a change.

A real fix (invalidating on write, or serving the live mtime for files whose
bytes we did not freeze) belongs with the watcher work, not here.

### Failure modes to test explicitly

- **Write to a bundled path** (app writes a compiled-template cache next to
  source): the overlay write lands on disk but a later read may see the stale
  bundled copy. Mitigation: scope the bundle to curated code dirs
  (`vendor/`, `src/`) and always fall through for framework `var/`/`cache/`
  dirs; or track a per-deploy write-set that reads check first.
- **realpath of a symlinked / `..`-traversing source path** that resolves
  outside the bundle → fall through to disk (correct) but slower; must not
  error.
- **Relative include resolved against cwd/`include_path`** → resolve exactly as
  PHP would (cwd = docroot, then `include_path`), then look up the canonical
  result.
- **File added at runtime then included** (generated config, session) → bundle
  miss → disk fall-through. Correct as long as bundle-first-miss is cheap.
  **This holds everywhere except inside a declared sealed root**, where a
  runtime-created `.php` file is reported missing by metadata probes until the
  first write or wrong negative disarms that root. Keep sealed roots disjoint
  from anything the app writes into (the reason the recommended value is
  `["vendor"]`).
- **`glob()`/`scandir` set or order mismatch** → framework cache invalidation
  or asset manifests break silently.
- **Zero/one mtime** → asset-version or Twig cache-key churn.

The correctness surface here — case-folding, overlay writes, mtime stability,
glob ordering — is the riskiest part of the whole feature, because a subtle
wrong answer breaks a framework in production silently and app-specifically.
The validation gate is the WordPress/Laravel/Symfony test suites run with the
bundle on, not unit tests.

## Bundle format + lifecycle

**Format.** A single length-prefixed blob (concatenated `.php` source) plus a
sidecar index:

```
Bundle {
  blob: Mmap | Vec<u8>,                        // concatenated source
  files: HashMap<NormalizedPath, Entry>,       // Entry { offset, len, mtime, inode }
  dirs:  HashMap<NormalizedPath, Vec<Name>>,   // directory child lists
}
```

`mmap` the blob for zero-copy reads (Windows `CreateFileMapping`) or heap-load
for small apps. **Immutable after load**, shared as `Arc<Bundle>` across every
`spawn_blocking` PHP thread — no per-request mutation, so it is trivially
ZTS-safe (this is the same read-only-after-init discipline the rest of
ephpm-php global state follows). The C hooks read it through a leaked raw
pointer.

**When it's built.**
- **`ephpm bundle <docroot>` subcommand** → emits `app.ebundle` at
  **image-build time** (recommended for containers: deterministic, part of the
  immutable image).
- **First-boot scan** (`code_bundle = "scan"`) → convenience for dev; adds a
  one-time boot scan and re-scans every start.

**Memory footprint.** The whole `.php` corpus is resident. Typical: a Laravel
skeleton is ~30–40 MB of vendor `.php`; a large app ~100–200 MB. Bounded and
fine. The code-only scope boundary keeps this sane; flag pathological monorepos
/ `node_modules` under docroot (exclude non-`.php`) and provide a size cap +
`warn`.

## Scope boundary

**Core = `.php` code only** — source discovery and reads for the compiler,
autoloader, and userland file functions.

**Follow-on (optional): static assets.** ePHPm's Rust static-file handler
(`crates/ephpm-server/src/router.rs`, `probe_path` / the fallback resolver at
~line 2054, all `is_file`/`is_dir`/`exists` calls) **also** pays the win32
stat/open tax on every static request. Those assets could be served zero-copy
from the same index. Scope this as a **follow-on extension**, not core, because
static assets are more likely to be written/rotated at runtime and the overlay
semantics get harder.

## Config knob (proposed — follows the add-config-knob rules)

Not yet in code. When implemented it must land field + enforcement + docs +
default **in the same PR** (per the repo's add-config-knob checklist), with a
startup `warn` if set-but-unenforced.

- **Key:** `[php] code_bundle` — values `"off"` (**default**), `"scan"` (build
  the index by scanning the docroot at boot, overlay semantics), `"sealed"`
  (same scan plus authoritative negatives inside declared roots), `"file"` (load
  a prebuilt `.ebundle` — **not implemented**). Companions:
  `[php] code_bundle_sealed_paths` (default `[]`, recommended `["vendor"]`),
  `[php] code_bundle_verify_negatives` (diagnostic), `[php] code_bundle_path`
  for the `"file"` form.
- **Default off**, and `"sealed"` with the default empty root list is
  behaviourally `"scan"`. Use `"scan"` for prod/container images; add sealed
  roots only for trees the app never writes into.
- **Rejected by `ephpm dev`** with a startup error.
- **Interaction:** bundle mode should **imply/recommend
  `opcache.validate_timestamps=0`** — otherwise opcache keeps stat-ing and the
  main warm-path win is left on the table. Startup **must `warn`** when
  `code_bundle` is on but `validate_timestamps=1`.
- **Multi-site:** per-site bundle keyed by the resolved site root; Phase 1 may
  ship single-docroot only and treat multi-site as `off`.
- **Startup log line (proposed):**
  `info!(entries, bytes, source = "scan|file", validate_timestamps, "code bundle loaded: {entries} files, {bytes} resident; code reads served from memory")`.

## Benchmark plan

Separate the **filesystem-syscall win** (this feature) from the **VM-dispatch
win** (TAILCALL) by running the full matrix:

| Platform | VM        | Bundle |
|----------|-----------|--------|
| Windows  | MSVC/CALL | off / on |
| Windows  | TAILCALL  | off / on |
| Linux    | CALL      | off / on |

**Workloads.** A stat-heavy real app — Symfony demo or a Composer-autoload-heavy
skeleton — measured **two ways**:
- **Cold start** (fresh container, empty opcache) — isolates the cold-compile
  + discovery win.
- **Warm request** (opcache hot) — isolates the steady-state stat-tax win,
  run once with `validate_timestamps=1` and once with `=0`.

**Metrics.** p50/p99 request latency; cold-start-to-first-byte; and — the
direct evidence — **syscall count + aggregate syscall time per request** via
ETW / Process Monitor on Windows (`strace -c` on Linux). The ceiling table
above is the pre-registered expectation the syscall count should confirm.

**The ceiling measurement is already taken** (see above): ~50 µs/metadata op,
~48 µs cold open+read, ~76 µs realpath, ~7 µs miss on this Windows host,
extrapolating to ~15 ms/request warm (default opcache) and ~29 ms cold for a
~300-file app.

**Linux expectation — measured, and partly wrong.** The prediction was
"sub-millisecond to low-single-digit ms per request, i.e. usually not worth the
correctness risk." The absolute-win half held (2.51 ms → 0.93 ms on the
frontable probe); the *relative* half did not. Sealed roots cut warm latency
**2.7×** on Linux and took file-metadata syscalls from 2403 per request to 3,
because the cost that dominates is not the price of one `statx` but the *count*
— an autoloader with two decoy directories issues 2400 of them. Linux is
cheaper per syscall, not cheaper per syscall-count. The honest statement is: the
win is proportionally similar on both platforms and much larger in absolute
terms on Windows, and `opcache.preload` still covers the compiled-class case on
Linux while doing nothing for the probe count.

## Prior art

- **FrankenPHP embed mode** — `go:embed` bakes the app into the Go binary and
  runs PHP against an embedded FS via a custom `php_stream` + patched
  `zend_resolve_path`. Same core idea (embed app, serve from memory). ePHPm
  differs: the bundle is a **separate artifact loaded at boot**, not compiled
  into the ePHPm binary — deployable per-app without recompiling ePHPm — and
  ePHPm targets **Windows**, where the FS tax is worst, whereas FrankenPHP is
  Linux-first. Borrow: the C-level `zend_resolve_path` + stream-wrapper
  override technique is proven there.
- **Phar** — bundles code into an archive behind a `phar://` wrapper.
  Undercut by its own per-access stat + signature validation and the stream
  layer's overhead, and — decisively — `phar://` prefixes leak into
  `__FILE__`, breaking apps that do path math expecting real paths. ePHPm
  avoids exactly this by mirroring real absolute paths.
- **`opcache.preload`** — compiles a fixed file set into SHM at startup. The
  existing Linux tool and a **complement**: preload fixes the compile layer for
  a subset; the bundle fixes the filesystem layer (discovery + read) for
  everything. Best together.

## Feasibility verdict: YELLOW (green-leaning for the POC)

Worth building **on Windows**, where the measured ceiling (~15 ms/request warm,
~29 ms cold) is large — and, on the measured evidence above, worth it on Linux
too: the win is proportionally similar there (2.7× on the frontable probe), just
smaller in absolute milliseconds. Still yellow, because the value depends on an
`is_file`-style probe (real Composer uses `file_exists`, which needs the SDK
patch) and because the correctness surface is wide. Sealed roots narrow that
surface deliberately — declared subtrees only, one-way authority latch, disk
confirmation on every source-path negative — but they do not eliminate it.

- **Effort.** POC ~1–2 weeks (override `zend_stream_open_function` +
  `zend_resolve_path` + a C `file://` stat/open replacement reading from
  `Arc<Bundle>`; prove syscalls drop on one insertion point via Process
  Monitor). Correctness hardening ~3–6 weeks (case-fold, overlay writes, mtime
  stability, glob ordering, run WP/Laravel/Symfony suites with bundle on).
  Static-asset extension +1–2 weeks.
- **Riskiest unknown.** The realpath / virtual-FS **correctness** — Windows
  case-insensitivity, overlay-write semantics, and mtime stability for cache
  keys. A subtle wrong answer breaks a framework silently in production. This,
  not the syscall interposition, is the make-or-break.
- **SDK-patch maintainability.** The portable C-hook lever (Option B) needs no
  SDK patch and is MSVC-safe; keep the bulk of the win there. The optional
  `win32/ioutil.c` + `tsrm_virtual_cwd.c` patch that closes the realpath-cache
  leak is a recurring per-PHP-minor cost carried in the php-sdk pipeline (same
  model as the TAILCALL patch) — bounded and known, but reserve it for the last
  few percent.
- **Phased plan.**
  1. **POC** — portable C hooks, one app, Process Monitor proof that code-path
     syscalls drop to ~0. Proves the syscall-elimination thesis.
  2. **Correctness hardening** — the failure-mode matrix above against real
     framework test suites; `ephpm bundle` subcommand + `.ebundle` format.
  3. **Config + integration** — the `[php] code_bundle` knob, multi-site
     per-root bundles, `validate_timestamps=0` / `opcache.preload` wiring.
  4. **(Optional) SDK patch + static extension** — close the realpath leak;
     serve static assets zero-copy from the same index.
