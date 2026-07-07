+++
title = "PHP Extensions"
weight = 11
aliases = ["/roadmap/dynamic-extensions/"]
+++

ePHPm ships with ~45 PHP extensions statically linked into the binary —
enough to run WordPress, Laravel, and Symfony out of the box. That covers
the common case but leaves a long tail: MongoDB, Swoole, igbinary,
msgpack, in-house extensions, anything released after the last SDK build.

For the long tail, ePHPm loads **standard shared PHP extensions** at
startup — the same `.so` files, loaded through the same `extension=`
mechanism, that php-fpm and php-cli use. No ePHPm rebuild: any glibc
**ZTS** build of an extension for the matching PHP minor loads as-is.

The easiest source of ZTS builds is the **ePHPm extension catalog** — a
set of common extensions prebuilt ABI-matched to each release (see
[The extension catalog](#the-extension-catalog) below). Debian and
[Sury](https://deb.sury.org/) packages are NTS-only and are rejected at
startup (see the ABI contract), so use the catalog, or `phpize`-compile
your own ZTS build against the matching php-sdk headers.

The static set stays the baseline. Shared loading is the escape hatch.

## Quick start

```toml
# /etc/ephpm/ephpm.toml
[php]
extensions = [
    "redis",                                # bare name: PHP's extension_dir search
    "/opt/exts/mongodb.so",                 # explicit path: loaded verbatim
]
```

Point the entries at ZTS builds of the extensions (explicit path, or a
bare name plus `extension_dir`) and restart. Verify with `phpinfo()` or
`extension_loaded('redis')`.

A minimal source build against the embedded PHP works like this
(validated with a hand-rolled extension on the 8.5.7 release binary —
compile on a distro no newer than your deployment target):

```bash
# headers: the include/php tree from the matching php-sdk gnu tarball
gcc -shared -fPIC -DZTS=1 -DCOMPILE_DL_MYEXT=1 \
    -I$SDK/include/php -I$SDK/include/php/main -I$SDK/include/php/Zend \
    -I$SDK/include/php/TSRM -I$SDK/include/php/ext \
    -o myext.so myext.c
```

For real-world PECL extensions, `phpize` from any ZTS PHP of the same
minor produces the same result. There is **no apt shortcut**: as of
July 2026 the Sury repo publishes no ZTS packages at all (verified
against the bookworm and trixie indexes — `php8.5-igbinary` etc. install
NTS-only `.so` files, which ePHPm rejects at startup with
`undefined symbol: compiler_globals`). The [extension catalog](#the-extension-catalog)
closes this gap with prebuilt ZTS `.so` files.

## The extension catalog

ePHPm publishes a curated set of common extensions, prebuilt **ZTS +
glibc + non-debug** and ABI-matched to each PHP minor, at
`github.com/ephpm/php-sdk` under the release tag `ext-<version>`. The
current catalog: `igbinary`, `msgpack`, `apcu`, `redis`, `mongodb`
(more to follow). Each tarball carries a `manifest.json` recording every
`.so`'s `php_api_no`, `zts` flag, and sha256.

Download, extract, and point `[php] extensions` at the files:

```bash
curl -fsSL -o ext.tgz \
  https://github.com/ephpm/php-sdk/releases/download/ext-8.5.7/ephpm-ext-8.5.7-linux-x86_64-gnu.tar.gz
mkdir -p /opt/ephpm-ext && tar xzf ext.tgz -C /opt/ephpm-ext
```

```toml
# ephpm.toml — match the catalog version to your ePHPm PHP minor
[php]
extensions = [
    "/opt/ephpm-ext/igbinary.so",
    "/opt/ephpm-ext/msgpack.so",
    "/opt/ephpm-ext/redis.so",
    "/opt/ephpm-ext/mongodb.so",
]
```

Restart and verify: `extension_loaded('redis')`, `phpinfo()`, or call a
function (`igbinary_serialize([1,2,3])`). The catalog is regenerated
whenever the pinned PHP SDKs are — use the `ext-<version>` tag matching
your release. In a container, the same works in a derived image (fetch
the tarball in a build stage, set `extensions` in the baked config).

## How it works

Entries in `[php] extensions` are written as `extension=<entry>` lines
into the php.ini that ePHPm generates at startup, **before** any
`ini_file` content and `ini_overrides`. PHP then loads them during module
startup (MINIT) exactly as vanilla PHP would:

- A **bare name** (`"redis"`) becomes `extension=redis` — PHP resolves it
  against its `extension_dir`. To point at a system package directory,
  set `extension_dir` via `ini_overrides`:

  ```toml
  [php]
  extensions = ["redis"]
  ini_overrides = [["extension_dir", "/opt/php-exts"]]
  ```

  (The embedded default `extension_dir` is a baked-in placeholder like
  `/lib/php/extensions/no-debug-zts-20250925/` — override it or use
  explicit paths.)

- A **path** (`"/opt/exts/mongodb.so"`, relative paths allowed) becomes
  `extension=/opt/exts/mongodb.so` and is loaded verbatim. Explicit paths
  are the most robust option.

Because the `extension=` lines precede `ini_file`/`ini_overrides` in the
generated ini, you can configure an extension's own ini settings in the
same config file.

An **empty string** entry fails validation at startup (it could never
load anything, and PHP would silently ignore it). A `.so` that doesn't
exist or doesn't match the ABI is reported by PHP at startup ("Unable to
load dynamic library ..." / an API-version mismatch message).

Extension loading is per-process and applies to all requests. Per-vhost
extension sets ("tenant A gets mongodb, tenant B doesn't") are
**planned — not yet implemented**.

## The ABI contract

PHP extensions are not portable across builds. A shared extension must
match the embedded PHP on all of these axes, and PHP refuses to load the
module (with an explicit API/build mismatch message at startup) when they
don't:

| Dimension | ePHPm's build |
|---|---|
| PHP minor version | The release you downloaded (8.3 / 8.4 / 8.5 — one binary per minor) |
| Thread safety | **ZTS** on Linux and macOS, **NTS** on Windows |
| libc (Linux) | **glibc** — release binaries are glibc-dynamic (`<arch>-unknown-linux-gnu`) |

In practice on Linux that means: a **ZTS** glibc build for the matching
PHP minor works; musl (Alpine) builds and NTS builds don't. Beware that
distro/Sury `php8.x-<ext>` packages are **NTS** and are rejected at
startup (verified July 2026: Sury's `php8.5-igbinary` fails with
`undefined symbol: compiler_globals`; no `-zts` package variants exist in
the Sury bookworm/trixie indexes). On Windows, use NTS x64 DLLs for the
matching PHP minor; on macOS, arm64 ZTS builds.

If you build a fully static musl ePHPm yourself, `dlopen()` is
unavailable there — `[php] extensions` (and shared middleware) cannot
work in that binary. The published release binaries are glibc-dynamic
precisely so that this mechanism works out of the box.

### ZTS caveats

ePHPm is ZTS on Linux/macOS. Most mainstream PECL extensions (mongodb,
redis, igbinary, msgpack, yaml, uuid, apcu, ds) are ZTS-clean; a few
assume NTS and misbehave under threads. Prefer extensions that publish
ZTS support, and test before production.

### Crash isolation

Same as vanilla PHP and native middleware: extensions run in-process. A
buggy extension can crash the whole server; a malicious one owns it. Only
load extensions you trust.

## Relationship to the static set

| | Static (baseline) | Shared (escape hatch) |
|---|---|---|
| How added | Baked into php-sdk, ships in the binary | Install/drop a `.so`, list it in `ephpm.toml` |
| Cost to add one | New SDK + ePHPm release | Zero rebuild |
| Suitable for | Extensions every PHP app uses | Niche / in-house / newer extensions |
| ABI risk | Tested on every ePHPm release | Yours to match (PHP minor + ZTS + glibc) |
| Crash isolation | Same address space | Same address space |

## Planned — not yet implemented

- **Per-vhost extension sets** — today the list is process-wide.
- **`cargo xtask php-ext`** build helper (phpize against the matching PHP
  SDK) for out-of-catalog extensions.
- **`ephpm_loaded_extensions()`** SAPI builtin (use PHP's own
  `get_loaded_extensions()` today).
- **Catalog expansion + Windows/macOS catalog variants** — today the
  catalog covers the common Linux set (igbinary, msgpack, apcu, redis,
  mongodb); more extensions and per-OS variants are planned.
