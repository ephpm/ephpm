# In-Memory Application Code Bundle

> **Status: EXPERIMENTAL POC (measured).** A working proof-of-concept exists
> behind `[php] code_bundle` (`off` by default) — C-level overrides of
> `zend_resolve_path`, `zend_stream_open_function`, and the plain-files
> `stream_opener` + `url_stat` ops, backed by an immutable Rust index
> (`crates/ephpm-php/src/code_bundle.rs`, `code_bundle_hooks.c`). It is **not
> production-hardened**.
>
> ### POC findings (Windows 11 + WSL2 Linux, same host, PHP 8.5.7 ZTS)
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

### The residual leak, and the optional SDK patch.
Even with Option B, `tsrm_virtual_cwd`'s realpath cache calls
`php_sys_lstat` / the win32 ioutil accessors directly in some canonicalization
paths (`VCWD_REALPATH`). To close realpath fully and squeeze the last few
percent on Windows, a **build-time patch to the shipped PHP SDK**
(`win32/ioutil.c` stat/open + `TSRM/tsrm_virtual_cwd.c` realpath) that consults
an ePHPm callback before hitting Win32 is the complete-coverage option. This is
the *same php-sdk patch pipeline* that already produces the TAILCALL VM (xtask
only *verifies* the VM kind by disassembling `zend_vm_kind`; the patch itself
lives upstream in `github.com/ephpm/php-sdk`). It is deferred to a late phase
because it is a recurring per-PHP-minor maintenance cost — reserve it for the
realpath-cache leak that Option B can't reach, not for the bulk of the win.

**Recommendation:** ship Option B (portable C hooks) as core, driven by
`opcache.validate_timestamps=0`, optionally fed into `opcache.preload`; treat
the Option C preload wiring as a complement and the Option B→SDK-patch upgrade
for realpath as a final hardening phase.

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
