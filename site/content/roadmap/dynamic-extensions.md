# Dynamic PHP Extensions

ePHPm ships with ~45 PHP extensions statically linked into the
binary via static-php-cli — enough to run WordPress, Laravel, and
Symfony out of the box without any external dependencies. That
covers the common case but leaves the long tail unaddressed:
MongoDB, Swoole, ev, igbinary, msgpack, in-house extensions, anything
released after our last SDK build.

This page describes the design for **dynamic extension loading**:
the same mechanism PHP itself uses to load extensions from
`extension=foo.so` entries in `php.ini`, exposed in ePHPm via
`site.toml` so any standard PHP extension can be loaded at startup
without rebuilding the ePHPm binary.

The static set stays the baseline. Dynamic loading is the escape
hatch.

---

## What we do today

ePHPm consumes a prebuilt PHP SDK from `github.com/ephpm/php-sdk`,
which is built by static-php-cli. The SDK includes a single
`libphp.a` with every extension in `PHP_EXTENSIONS` statically
compiled in:

```
bcmath, bz2, calendar, ctype, curl, dom, exif, fileinfo, filter,
ftp, gd, gettext, gmp, hash, iconv, intl, mbstring, mysqli, mysqlnd,
opcache, openssl, pcntl, pcre, pdo, pdo_mysql, pdo_pgsql, pdo_sqlite,
pgsql, phar, posix, session, simplexml, soap, sodium, sqlite3,
sysvmsg, sysvsem, sysvshm, tokenizer, xml, xmlreader, xmlwriter,
xsl, zip, zlib
```

Adding an extension requires:

1. Adding it to `PHP_EXTENSIONS` in `ephpm/php-sdk`'s workflow.
2. Cutting a new SDK release.
3. Bumping the pinned SDK version in `ephpm/xtask`.
4. Rebuilding ePHPm.

That's the right model for the baseline because it gives us a
single binary with no runtime dependencies, deterministic builds,
and a controlled compatibility surface (all extensions verified
ZTS-safe). It's the wrong model for users who need one extension
we don't ship.

---

## What PHP itself does

Vanilla PHP supports both static and shared extensions:

- **Static** — compiled into the binary via `--enable-foo` at
  `./configure` time. Always loaded, can't be disabled.
- **Shared** — built as a `.so` (`--enable-foo=shared`), loaded at
  startup via `extension=foo.so` in `php.ini`. The engine calls
  `php_load_extension()` for each, which is a thin wrapper around
  `dlopen` plus symbol lookup for the extension's `zend_module_entry`.

PHP-FPM, php-cli, and mod_php all support both. ePHPm currently
only supports the first. There's nothing about the embed SAPI that
prevents the second — `php_load_extension` is part of the same
embed API surface we already use.

---

## What ePHPm has that makes this easy

The Zend Engine already exposes the function we need:

```c
int php_load_extension(char* filename, int type, int start_now);
```

It's a public symbol in `libphp.a`, callable from Rust via FFI like
any other PHP API. We're already calling `zend_execute_scripts`,
`zend_eval_string`, and dozens of others through `ephpm_wrapper.c`.

The remaining work is:

1. Config plumbing: a list of `.so` paths in `site.toml`.
2. A loader pass between `php_module_startup` and `php_request_startup`
   that calls `php_load_extension` for each declared `.so`.
3. Documentation of the ABI requirements so users can build
   compatible extensions.

That's it. The mechanism is small. The interesting work is the
ABI documentation and the build helper.

---

## Design

### Configuration

Per-vhost or top-level in `site.toml`:

```toml
[php]
extensions = [
    "ext/mongodb.so",
    "ext/swoole.so",
    "/opt/myco/internal.so",
]
```

Paths are resolved relative to the directory containing `site.toml`,
or absolutely if they start with `/` (or a drive letter on Windows).
Same search-path convention as middleware modules: site dir first,
then `$EPHPM_EXTENSION_DIR`, then `/usr/local/lib/ephpm/ext`.

Order matters for some extensions (OPcache must load early; some
extensions depend on others). v1 loads in the order declared.
Optional priority field can come later if needed:

```toml
[[php.extension]]
path     = "ext/mongodb.so"
priority = 100   # lower = earlier; OPcache reserves <50
```

### Loading lifecycle

In `ephpm-php/src/sapi/lifecycle.rs`, after `php_module_startup` and
before the first request:

```rust
for ext in config.php.extensions {
    let cstr = CString::new(ext)?;
    let rc = unsafe {
        ephpm_wrapper_load_extension(cstr.as_ptr())
    };
    if rc == 0 {
        return Err(anyhow!("failed to load PHP extension: {ext}"));
    }
    tracing::info!(path = %ext, "loaded PHP extension");
}
```

`ephpm_wrapper_load_extension` is a thin C wrapper around
`php_load_extension` that runs under `zend_try` / `zend_catch` so a
failed load doesn't longjmp through Rust.

Each extension's `MINIT` runs at load time, `RINIT` runs at each
request, exactly as in standard PHP.

### Introspection

A new SAPI builtin so PHP code can see what's loaded:

```php
ephpm_loaded_extensions(): array
// Returns: [{ name, version, source: 'static' | 'dynamic', path?: string }, ...]
```

Already covered by PHP's `get_loaded_extensions()` for the most
part — this adds the source field so users can tell what came from
where.

---

## The ABI contract

PHP extensions are not portable across builds. The ABI is sensitive
to:

| Dimension | Why it matters |
|---|---|
| PHP version | Major changes between 8.3 / 8.4 / 8.5 in internal structs (`zend_function`, `zval` layout) |
| ZTS vs NTS | TSRM symbols differ; ZTS-built extension won't load into NTS PHP and vice versa |
| Debug vs release | Debug builds expose extra fields used by Xdebug etc. |
| Compiler / libc | Linux musl vs glibc, macOS deployment target |

So we need to **publish** what our build is, and ship a
build helper that produces extensions matching it. The same way
PECL packagers test against multiple PHP builds today.

### What we publish

A `phpinfo()`-style manifest at `/etc/ephpm/build-info.json` in
each release tarball:

```json
{
  "ephpm_version": "0.30.0",
  "php_version":   "8.5.2",
  "php_zts":       true,
  "php_debug":     false,
  "php_api_no":    20240924,
  "php_zend_api":  20240924,
  "php_module_api":20240924,
  "build_target":  "x86_64-unknown-linux-musl",
  "libc":          "musl-1.2.5",
  "compiler":      "gcc-12.3.0",
  "spc_version":   "2.8.5",
  "php_sdk_version": "8.5.2-3",
  "loaded_static_extensions": ["bcmath", "bz2", "..."]
}
```

The same info is also queryable via a new `ephpm php-build-info`
subcommand and reachable from PHP via `ephpm_build_info()`.

### What we provide for building extensions

A new xtask: `cargo xtask php-ext`. Two operations:

```bash
# Print the PHP SDK headers + flags an extension needs to build against
cargo xtask php-ext flags --php=8.5.2
# → CPPFLAGS=-I/path/to/sdk/include/php -DZTS=1 ...
# → LDFLAGS=...
# → PHP_API_NO=20240924

# Build an extension from a PECL-style source dir
cargo xtask php-ext build --php=8.5.2 --src ./mongodb-1.21.0
# → out/mongodb.linux-x86_64.so
```

Internally, the `build` op downloads the matching PHP SDK (same
mechanism `cargo xtask release` uses), invokes `phpize` from the
SDK, and runs `./configure && make` with the right env. The
output `.so` is ABI-matched to the corresponding ePHPm release.

For the common case where users want an extension we don't ship,
we maintain a small CI-built catalog of known-good extensions at
`github.com/ephpm/php-ext-catalog`, prebuilt per PHP version /
target / arch. Drop the `.so` next to `site.toml`, point at it,
done. (Out of scope for v1 — list contents below.)

---

## Constraints and trade-offs

### ZTS

ePHPm is ZTS-only on non-Windows. Several PECL extensions assume
NTS and have undefined behavior under ZTS (the canonical examples
are extensions that hold global state in C statics without
TSRM_ALLOC, or that fork child processes unsafely).

We can't load NTS-only extensions. The doc has to be honest about
this and the catalog has to mark each extension's ZTS status.
Known-good ZTS extensions include all of PECL's "core" set
(mongodb, redis, igbinary, msgpack, swoole-with-coroutines-off,
yaml, uuid, etc).

### Crash isolation

Same as middleware: a buggy extension segfaults the entire ePHPm
process. PHP extensions have always had this property and the
ecosystem accepts it. We document it and move on.

### Version pinning

When an ePHPm release ships with PHP 8.5.2, every dynamic extension
loaded into it must be built against the same PHP 8.5.2. Upgrading
ePHPm means rebuilding (or re-downloading from the catalog) every
extension `.so`. The build-info manifest plus the xtask makes this
mechanical, not painful.

### Static set stays primary

Nothing about dynamic loading reduces the value of the static set.
WordPress, Laravel, Symfony all run zero-config against the
baseline. Dynamic loading is purely additive — for the user who
needs MongoDB and doesn't want to rebuild ePHPm.

---

## Phases

### Phase 1 — Loader + introspection

- `ephpm_wrapper_load_extension` C wrapper with `zend_try` guard.
- Config schema: `[php] extensions = [...]` in `ephpm-config`.
- Loader pass in `ephpm-php` lifecycle, runs between module startup
  and first request.
- `ephpm_loaded_extensions()` SAPI builtin.
- Tracing log entry per successful load + clear error per failure.
- Tests with a known-good ZTS extension (igbinary is a small, well-
  behaved candidate).

~3 days. The C wrapper plus the config slot plus the loader call
is genuinely small.

### Phase 2 — Build helper + publishing

- `cargo xtask php-ext flags` and `cargo xtask php-ext build`.
- `ephpm php-build-info` CLI subcommand.
- `/etc/ephpm/build-info.json` shipped in release tarballs.
- `ephpm_build_info()` SAPI builtin.
- Docs: "How to build a PHP extension for ePHPm".

~2-3 days. Mostly plumbing existing tools.

### Phase 3 — Prebuilt catalog

- `github.com/ephpm/php-ext-catalog`: a separate repo with a GHA
  matrix that builds known-good PECL extensions against each
  pinned ePHPm PHP version × target × arch combination.
- Initial catalog (community-driven from there):
  - `mongodb` — MongoDB driver
  - `redis` — Redis client (relevant even when KV does most of the job; many apps use Redis directly)
  - `igbinary` — fast binary serializer
  - `msgpack` — MessagePack encoder
  - `yaml` — YAML parser
  - `uuid` — RFC 4122 UUIDs
  - `apcu` — in-process user cache (orthogonal to KV)
  - `ds` — efficient data structures
- Documented per-extension status (ZTS-safe?, tested-against-versions).

Iterative; first catalog cut takes a couple of days, then it grows.

### Phase 4 — Per-vhost extension scoping

- Today's `[php] extensions` is process-wide. Real multi-tenant
  setups want per-vhost extension sets ("tenant A needs mongodb,
  tenant B doesn't, don't load it for B").
- PHP doesn't natively support per-request extension loading;
  this requires us to either (a) load every declared extension
  globally and gate access at the SAPI layer, or (b) prerun
  separate PHP module-startup contexts per vhost.
- (a) is cheap and v3-ish. (b) is a much bigger architectural
  change and probably doesn't pay back.

Deferred unless real demand surfaces.

---

## Relationship to the static set

These two paths coexist forever:

| | Static (baseline) | Dynamic (escape hatch) |
|---|---|---|
| How added | Bake into php-sdk, rebuild ePHPm | Drop `.so`, list in `site.toml` |
| Cost to add one | New SDK release + ePHPm release | Zero rebuild |
| Suitable for | Common extensions every PHP app uses | Niche / in-house / community extensions |
| ABI risk | Tested on every ePHPm release | User-managed against published build-info |
| Crash isolation | Same address space | Same address space |
| Distribution | In the ePHPm binary | Separate `.so` files |

The static baseline grows slowly and deliberately — extensions
graduate from "available in the dynamic catalog" to "shipped in
the static set" when usage warrants it.

---

## Why this matters

The single biggest friction point for adopting any PHP runtime is
"does it have the extension I need?" Static-only is the cleanest
deployment story but the worst answer to that question. PHP itself
solved this 25 years ago by supporting both static and shared
extensions. ePHPm should too.

Once this ships, ePHPm becomes the first PHP application server
that's both a single-binary deployment **and** extensible in the
standard PHP way. Drop the binary, drop your extension, point
`site.toml` at it, done — no `phpize`, no `apt install
php-mongodb`, no Dockerfile dance, no separate PHP-FPM rebuild.

It also enables the natural pairing with middleware: the
[Native Middleware](/guides/native-middleware/) loader (shipped) and this loader are
the same shape (dlopen + lifecycle + ABI contract), differing only
in which surface they target (the request pipeline vs the Zend
Engine's module table). Both turn ePHPm into a small,
batteries-included core that grows by accepting native `.so`
files at runtime — a familiar, well-trodden extension model.
