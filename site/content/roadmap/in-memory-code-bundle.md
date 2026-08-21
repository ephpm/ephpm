# In-Memory Application Code Bundle

> **Status: EXPERIMENTAL POC (measured).** A working proof-of-concept exists
> behind `[php] code_bundle = "scan"` (off by default) — C-level overrides of
> `zend_resolve_path`, `zend_stream_open_function`, and the plain-files
> `stream_opener` + `url_stat` ops, backed by an immutable `Arc`-style Rust index
> (`crates/ephpm-php/src/code_bundle.rs`, `code_bundle_hooks.c`). It is **not
> production-hardened**. See the POC-findings box below for what the portable
> hooks can and cannot front on Windows — the result revises the feasibility
> verdict.
>
> ### POC findings (Windows 11, PHP 8.5.7 ZTS, this host)
>
> - **The source-read path works and is opcache-safe.** `require`/`include` and
>   the cold compile read serve from RAM even with OPcache enabled — proven by
>   deleting the source off disk after boot and still serving a 400-class
>   autoloader (`files_ok=1`). This required overriding the plain-files
>   **`stream_opener`**, not just `zend_stream_open_function`: OPcache captures
>   the *original* `zend_stream_open_function` at startup and calls its saved
>   copy, bypassing a late override of the live global.
> - **`is_file`/`stat`/`filemtime` are frontable** via the plain-files `url_stat`
>   op — served from RAM, proven with disk deleted.
> - **`file_exists()` is NOT frontable by portable hooks on Windows.** It uses
>   `VCWD_ACCESS` (access-check), not the stream wrapper, so it bypasses
>   `url_stat` and still hits disk. **Real Composer's PSR-4 autoloader probes
>   with `file_exists`,** so its steady-state stat tax — the feature's main
>   motivation — is *not* reclaimed by the portable C-hook approach. Fronting it
>   requires the SDK patch (`win32/ioutil.c` / `TSRM/tsrm_virtual_cwd.c`) that
>   this doc had scoped as optional "last few percent" hardening; on Windows it
>   turns out to be load-bearing, not residual.
> - **Measured (stat-heavy 400-class Composer-model autoload, warm p50):** with
>   an `is_file`-based probe (frontable) the bundle roughly **halves** warm
>   latency (~24–33 ms → ~13 ms). With a `file_exists`-based probe (real
>   Composer) the bundle gives **no meaningful warm win**. TAILCALL ≈ CALL for
>   this FS/discovery-bound workload (the VM-dispatch win is orthogonal and does
>   not show here). Compression (zstd) leaves warm latency unchanged and adds
>   only a small one-time cold decompress cost. Full table in the POC PR.
>
> The original design text below is preserved. No `[php] code_bundle` value
> other than `"off"`/`"scan"` is accepted; `"file"` (prebuilt `.ebundle`) and
> multi-site bundles remain unimplemented.

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
  the index by scanning the docroot at boot), `"file"` (load a prebuilt
  `.ebundle`). Companion `[php] code_bundle_path` for the `"file"` form.
- **Default off.** On for prod/container images.
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

**Linux expectation.** Linux `statx` is ~10–50× cheaper, so the same 300 stats
are ~0.3–0.9 ms warm and single-digit-ms cold — a much smaller absolute win,
and `opcache.preload` already covers the compiled-class case. Predicted Linux
win: **sub-millisecond to low-single-digit ms per request**, i.e. usually not
worth the correctness risk. It becomes worth enabling on Linux only at **very
high RPS** (syscalls × request rate) or on **slow/networked filesystems**
(NFS/EFS, container overlayfs) where per-stat cost balloons back toward Windows
levels. State that honestly in the docs when shipping.

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
~29 ms cold) is large. Yellow overall because the value is concentrated on
Windows and the correctness surface is wide.

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
