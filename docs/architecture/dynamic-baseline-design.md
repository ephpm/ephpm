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

### 2.1 The glibc floor is 2.28 (deliberate: manylinux_2_28 / RHEL8 baseline)

The shipped Linux binary's glibc requirement is the highest `GLIBC_x.y`
symbol version pulled in at final link, across **all** objects — `libphp.a`
(built in php-sdk) plus the Rust/system objects linked when `cargo xtask
release` builds ephpm. Both build environments therefore set the floor, and
both are pinned to **`docker.io/library/almalinux:8`** (glibc 2.28) with
**gcc-toolset-13**: a modern GCC over an old glibc symbol floor, the same
model Python's manylinux wheels use. glibc forward-compatibility means a
2.28-linked binary runs on every newer glibc.

almalinux:8 is used rather than `quay.io/pypa/manylinux_2_28_*` (which shares
the same glibc 2.28 floor): the manylinux images are OCI-format image indexes
that the runner fleet's Docker cannot pull, whereas almalinux:8 is a
Docker-format image. gcc-toolset-13 (not 14) is used because GCC 14 makes
implicit-function-declaration a hard error and to keep one GCC version across
the whole pipeline (php-sdk's SDK toolchain).

**Verified floor = glibc 2.28** (`objdump -T ephpm | grep GLIBC_ | sort -V |
tail -1` → `GLIBC_2.28`; the only 2.28 symbols are `fcntl64` and `statx`).
No Rust crate (`ring`/`aws-lc`/`rustls`, Rust std) forces anything higher —
Rust's prebuilt `x86_64-unknown-linux-gnu` std targets glibc 2.17.

Two changes were needed to reach 2.28:
1. **Build environments moved off `ubuntu:24.04` (glibc 2.39)** to
   almalinux:8 (glibc 2.28). On glibc 2.39, `libphp.a` picked up **GLIBC_2.38**
   `__isoc23_*` scanf/strtol variants and the link added weak **GLIBC_2.39**
   `pidfd_*` refs — both vanish on the 2.28 toolchain. This spans the
   php-sdk gnu build jobs, ephpm's `docker/Dockerfile` builder stage, and the
   `ephpm/ephpm-ci` image (`docker/Dockerfile.gha`) that release.yml /
   nightly.yml link inside.
2. **`_GNU_SOURCE` for the C wrapper on all Unix targets**
   (`crates/ephpm-php/build.rs::compile_wrapper`, previously musl-only).
   glibc 2.28 does not expose `mempcpy`/`memrchr` at the default feature
   level, so `zend_operators.h` failed to compile until `_GNU_SOURCE` was
   defined; glibc 2.39 had leaked those declarations, hiding the gap.

Distro coverage at floor 2.28: RHEL8/AlmaLinux8/Rocky8 (2.28), Ubuntu 20.04+
(2.31+), Debian 10+ (2.28+), Amazon Linux 2023 (2.34), Fedora 40+ (2.39+).
**Out of scope:** Amazon Linux 2 ships glibc 2.26 (below 2.28) and reaches
EOL in 2026; AL2023 is the supported Amazon target. The container runtime
stage is `debian:12-slim` (bookworm, glibc 2.36); any glibc ≥ 2.28 host works.

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

Caveat found: `ephpm php -d extension=...` is a **no-op** (silently —
PHP is initialized before CLI args are parsed, and `extension=` only
takes effect at MINIT). The CLI does not read `ephpm.toml` either; use
`PHPRC` pointing at an ini with the `extension=` line. **Planned:** either
honor `-d extension=` by deferring init, or warn when it is passed.

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
| Linux x86_64 | Shipped: glibc-dynamic gnu binary, floor glibc 2.28 (§2.1). All of §3–§5 verified. |
| Linux aarch64 | Same design; **pending the arm64 gnu SDK asset** (in flight). Not yet validated. |
| Alpine / musl hosts | Not a binary target — run the container image (`debian:12-slim` base). A self-built fully static musl binary still cannot dlopen; that use case is `forge` territory (`build-compose-design.md`). |
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

1. **arm64 gnu SDK** — publish `linux-aarch64-gnu` tarballs; validate
   the aarch64 release lane end to end. (In flight.)
2. **8.4 / 8.3 gnu SDKs** — same validation per minor once the assets
   land. (In flight.)
3. **Glibc floor** — DONE: floor lowered to **glibc 2.28** (RHEL8 baseline)
   by rebuilding the php-sdk gnu jobs, ephpm's `docker/Dockerfile` builder,
   and the `ephpm/ephpm-ci` image (`docker/Dockerfile.gha`) on the
   `almalinux:8` base with gcc-toolset-13, plus the `_GNU_SOURCE` wrapper fix
   (§2.1). Requires a php-sdk gnu-tarball rebuild for the new floor to reach
   shipped releases.
4. **ZTS extension distribution** — php-sdk extension catalog
   (§3.4); until then docs steer users to self-compiled ZTS builds.
5. **Windows/macOS shared-extension smoke tests** — close the "not yet
   validated" rows in §6.
6. **macOS runner group** — bring macos-arm64 release validation onto
   the native runner pool.
7. **CLI ergonomics** — `ephpm php -d extension=` no-op (§3.2).
