# The Glibc-Dynamic Baseline — Linux Release Architecture (3.0 Pivot)

> **Status: DESIGN / ARCHITECTURE.** This document describes the shipped
> Linux release architecture after the dynamic pivot and the reasoning
> behind it. Every behavioral claim below was verified empirically on
> 2026-07-05 against the real `php-sdk-8.5.7-linux-x86_64-gnu.tar.gz`
> release asset (podman build of `docker/Dockerfile`, image
> `ephpm:pivot`), except where explicitly labeled **Planned** or **Not
> yet validated**. It supersedes
> [`build-compose-design.md`](build-compose-design.md) (the `forge`
> build-time composition design) for the common case; `forge` remains
> future tooling for users who want a custom fully static binary.

## 1. Motivation: static cannot dlopen

The original Linux releases were fully static musl binaries
(`crt-static`). A fully static binary has no runtime loader, so
`dlopen()` is structurally unavailable — proven during the native
middleware work, where the dlopen lane fails at startup with:

```
Dynamic loading not supported
```

That one property forecloses the two extensibility mechanisms users
actually ask for: loading standard shared PHP extensions
(`extension=<path>.so`) and loading out-of-tree middleware `.so`
modules. Build-time composition (`forge`, xcaddy-style) was designed as
the workaround; the pivot makes it unnecessary for the common case by
changing what we ship instead:

**Linux releases are now a single glibc-dynamic file** —
`<arch>-unknown-linux-gnu`, linked against the glibc (`-gnu`) variant of
the php-sdk `libphp.a`, with the binary's own Zend/TSRM symbols exported
to the dynamic symbol table.

## 2. The baseline binary

Two build-system facts define the baseline (both in
`crates/ephpm/build.rs` and `xtask/src/main.rs::release_native`):

1. **gnu target.** `cargo xtask release` builds
   `<arch>-unknown-linux-gnu` (the host default — the musl cross
   toolchain is gone) against the `-gnu` SDK tarball
   (`php-sdk-<ver>-linux-<arch>-gnu.tar.gz`, cached at
   `php-sdk/<ver>-linux-<arch>-gnu/`).
2. **`-Wl,--export-dynamic`.** Emitted by `crates/ephpm/build.rs` for
   the Linux link. Shared PHP extensions resolve `zend_*`/TSRM symbols
   against the *hosting process* at `dlopen` time; without this flag
   every extension load fails with unresolved Zend symbols.

The result is still operationally "one file". Verified `ldd` output of
the release binary inside the container (glibc family only, nothing
else):

```
linux-vdso.so.1
libm.so.6      => /lib/x86_64-linux-gnu/libm.so.6
libgcc_s.so.1  => /lib/x86_64-linux-gnu/libgcc_s.so.1
libc.so.6      => /lib/x86_64-linux-gnu/libc.so.6
/lib64/ld-linux-x86-64.so.2
```

`ephpm php -v` → `PHP 8.5.7 (ephpm)`; ZTS confirmed via
`ZEND_THREAD_SAFE` (`zts=1`) over HTTP in fpm mode.

### 2.1 The glibc floor is 2.39 (found during validation)

The pivot Dockerfile originally used `debian:12-slim` (glibc 2.36) as
the runtime stage. **That fails**: the loader refuses the binary with
`version 'GLIBC_2.38' not found` / `GLIBC_2.39 not found`. Root causes,
verified with `objdump -T`/`readelf`:

- `libphp.a` in the 8.5.7 gnu SDK references **GLIBC_2.38** symbols
  (`__isoc23_sscanf`, `__isoc23_strto*`, `strlcpy`, `strlcat`) — the SDK
  pipeline builds on a glibc ≥ 2.38 toolchain.
- Building on `ubuntu:24.04` (glibc 2.39) adds weak **GLIBC_2.39** refs
  (`pidfd_spawnp`, `pidfd_getpid`) from Rust std's process spawning.

So the shipped binary's floor is **glibc ≥ 2.39** (Ubuntu 24.04+,
Debian 13+, Fedora 40+). The container runtime stage is now
`debian:13-slim` (trixie, glibc 2.41) — verified working. **Planned:**
lowering the floor means rebuilding the php-sdk gnu tarball (and the CI
builder image) on an older baseline such as Debian 12/Ubuntu 22.04; the
floor is set by the build environment, not by the pivot itself.

## 3. Extension story

### 3.1 The `[php] extensions` knob

`[php] extensions = [...]` (`crates/ephpm-config/src/lib.rs`) emits
`extension=<entry>` lines into the startup-generated php.ini, *before*
`ini_file`/`ini_overrides` so extension ini settings can follow. Bare
names ride PHP's `extension_dir` search; paths load verbatim. Empty
entries fail `validate()` (no silent no-op).

### 3.2 Proof that dlopen + `--export-dynamic` works end to end

A minimal extension (`pivot_ping.c`: MINIT + `pivot_ping()` returning a
string) was compiled inside a Debian container with a single gcc
invocation against the headers from the same gnu SDK tarball:

```bash
gcc -shared -fPIC -DZTS=1 -DCOMPILE_DL_PIVOT_PING=1 \
    -I$SDK/include/php{,/main,/Zend,/TSRM,/ext} -o pivot_ping.so pivot_ping.c
```

`nm -D -u pivot_ping.so` shows it *must* import `_emalloc`,
`__zend_malloc`, and `zend_wrong_parameters_none_error` from the host —
i.e. the load only succeeds if the ephpm binary exports Zend symbols.
With `[php] extensions = ["/mw/pivot_ping.so"]`:

- HTTP: `curl /ping.php` → `string(22) "pong-from-dlopened-ext"`,
  `extension_loaded('pivot_ping')` → `true`.
- CLI: `PHPRC=/mw/cli-php.ini ephpm php -r 'var_dump(pivot_ping());'` →
  same string.

Caveat found: `ephpm php -d extension=...` does **not** load the
extension — PHP is initialized before CLI args are parsed, and
`extension=` only takes effect at MINIT. `ephpm php` now **warns** when a
runtime `-d extension=` is passed (rather than silently ignoring it) and
points at `[php] extensions`. The CLI does not read `ephpm.toml` either;
for a CLI-only load use `PHPRC` pointing at an ini with the `extension=`
line.

### 3.3 ABI rules

An extension must match the embedded PHP on every axis of the Zend build
ID — for the 8.5 Linux release that is **`API20250925` + ZTS +
non-debug + glibc** (the embedded default extension-dir placeholder is
`/lib/php/extensions/no-debug-zts-20250925/`). Mismatches are rejected
at startup with a clear PHP warning, not a crash (verified, §3.4).

### 3.4 Sury findings (honest status, checked 2026-07-05)

- The Sury repo (`packages.sury.org/php`) publishes **zero ZTS
  packages**: the bookworm *and* trixie amd64 `Packages` indexes contain
  no match for "zts" at all. There is no `php8.5-<ext>-zts`.
- `php8.5-igbinary` (installed in a derived image on top of the release
  container) lands only `/usr/lib/php/20250925/igbinary.so` — an NTS
  build.
- Loading that NTS `.so` into ePHPm fails **cleanly** at startup:
  `Unable to load dynamic library ... undefined symbol:
  compiler_globals` (in ZTS builds `compiler_globals` doesn't exist as a
  global — exactly the expected NTS/ZTS rejection).

So today there is **no apt-install path** for shared extensions on
Linux: users must compile ZTS builds themselves (phpize against a ZTS
PHP of the same minor, or the one-line gcc build above). **Planned:**
the php-sdk pipeline grows a ZTS extension catalog (prebuilt
`API20250925`-ZTS `.so` artifacts per release) as the fallback
distribution channel.

## 4. Middleware: both lanes in the same binary

Verified in one container from one config (`[[middleware]]` mounts):

- **Builtin lane** — `library = "security-headers"` resolves from the
  static registry (`middleware initialised (builtin)
  module=security-headers` in the log); response carries
  `strict-transport-security`, `x-frame-options: DENY`,
  `x-content-type-options: nosniff`, `referrer-policy`.
- **dlopen lane** — `libephpm_middleware_cors.so` (built with plain
  `cargo build --release -p ephpm-middleware-cors --target
  x86_64-unknown-linux-gnu` inside the builder image), mounted by path
  (`library = "/mw/libephpm_middleware_cors.so"`); log shows
  `middleware initialised module=/mw/libephpm_middleware_cors.so`, and a
  request with `Origin: https://example.com` gets
  `access-control-allow-origin: *` + `vary: Origin`.

Both header sets appeared on the same response — chain of two lanes in
one binary.

## 5. Worker mode is unaffected

Same image, `examples/worker/worker.php`, `worker_count = 1`:
consecutive requests return `boot #1, request #N` with N climbing
(2, 3, 4, …) — boot-once semantics intact under the gnu build.

## 6. Per-platform notes

| Platform | Story |
|---|---|
| Linux x86_64 | Shipped: glibc-dynamic gnu binary, floor glibc 2.39 (§2.1). All of §3–§5 verified. |
| Linux aarch64 | Same design; the `linux-aarch64-gnu` SDK tarball is **published** (8.5.7 / 8.4.22 / 8.3.31). End-to-end release-lane validation on aarch64 is still to be confirmed. |
| Alpine / musl hosts | Not a binary target — run the container image (`debian:13-slim` base). A self-built fully static musl binary still cannot dlopen; that use case is `forge` territory (`build-compose-design.md`). |
| Windows | **NTS**, PHP statically linked from `php8embed.lib`. `extension=` `.dll` loading is the intended mechanism but is **not yet validated** — stock PECL DLLs import `php8*.dll`, which a static embed does not provide; treat Windows shared-extension support as unproven until smoke-tested. |
| macOS (arm64) | ZTS `.dylib` loading is the intended mechanism; **not yet validated** (ld64 exports symbols by default, so no `--export-dynamic` analog should be needed). |

## 7. What changed in the release matrix

- Linux triples: `*-unknown-linux-musl` → `*-unknown-linux-gnu`
  (`.github/workflows/release.yml`); packaging copies from
  `target/<triple>/release/ephpm`.
- Toolchain: `musl-tools`/`musl-dev`, per-arch musl rustup targets, and
  the `CC_*`/`CARGO_TARGET_*_LINKER` env are gone from
  `docker/Dockerfile.gha` and the Dockerfiles.
- SDK assets gained the `-gnu` libc suffix
  (`php-sdk-<ver>-linux-<arch>-gnu.tar.gz`); the cache dir carries the
  suffix too, so stale musl-era caches are never reused.
- Container runtime base: `debian:13-slim` (was `debian:12-slim` in the
  pivot draft — changed for the glibc floor, §2.1).
- Windows and macOS release lanes are untouched by the pivot.

## 8. Phasing / remaining work

1. **gnu SDK tarballs — published.** All six `linux-{x86_64,aarch64}-gnu`
   tarballs now exist for 8.5.7 / 8.4.22 / 8.3.31 (`gh release view
   v8.5.7 --repo ephpm/php-sdk`). Remaining: confirm the aarch64 release
   lane end to end (x86_64 is verified, §3–§5).
2. **Glibc floor** — decide the supported floor and rebuild the SDK +
   CI builder on that baseline (currently 2.39 by accident of build
   environment, §2.1).
3. **ZTS extension distribution** — php-sdk extension catalog
   (§3.4); until then docs steer users to self-compiled ZTS builds.
4. **Windows/macOS shared-extension smoke tests** — close the "not yet
   validated" rows in §6.
5. **macOS runner group** — bring macos-arm64 release validation onto
   the native runner pool.
6. **CLI ergonomics** — `ephpm php -d extension=` now warns rather than
   silently ignoring the flag (§3.2).
