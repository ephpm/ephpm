# Future Improvements

Planned improvements and optimisations that are not yet implemented. Each entry includes the motivation, the expected impact, and rough implementation notes.

---

## Build System

### Prebuilt PHP SDK Tarballs

**Status:** Not started
**Impact:** High — reduces first-build time from ~15 minutes to ~30 seconds

Currently every fresh machine (developer laptop, CI runner, Docker build) compiles PHP and ~15–20 C libraries from source via `static-php-cli`. The compiled output — `libphp.a` plus headers — never changes unless the PHP version or the extension list changes.

**Why it is slow:** 95%+ of `cargo xtask release` time is spent in `cc` and `make`, not in xtask or spc itself. A rewrite of spc in Rust or Go would not meaningfully help; the bottleneck is the C compiler.

**Proposed approach:**

1. CI builds the PHP SDK via `spc` after any change to the extension list or PHP version, and uploads the result as a GitHub release artifact:
   ```
   php-sdk-8.5-linux-x86_64-musl.tar.gz
   php-sdk-8.4-linux-x86_64-musl.tar.gz
   php-sdk-8.5-darwin-aarch64.tar.gz
   ...
   ```

2. `cargo xtask release` checks for a matching prebuilt tarball (keyed on PHP version + a hash of the extension list) before invoking spc. If found, it downloads and unpacks instead of compiling.

3. The xtask already skips the spc build when `libphp.a` is present (`xtask/src/main.rs`). The download step slots in before the spc invocation.

4. Developers who need a custom extension set can still force a full spc build with `--force-rebuild`.

**CI cache fallback:** Even without prebuilt tarballs, caching the `php-sdk/` directory in CI keyed on `{php_version}-{hash(extensions)}` avoids recompilation on every run when the extension set hasn't changed.

---

## Docker / E2E Build

### `Dockerfile.gha` Modernisation

**Status:** Not started
**Impact:** Low — only affects GHA release builds

`docker/Dockerfile.gha` still uses the old `ondrej/php` PPA + `php8.4-cli` + `composer` approach to drive spc. It is hardcoded to PHP 8.4 regardless of the `PHP_VERSION` build arg, and installs ~150 MB of packages that are no longer needed now that spc ships as a standalone binary.

Should be updated to match `docker/Dockerfile`: remove the PPA, remove `php-cli` and `composer`, add `unzip`, and drop `rm -rf /var/lib/apt/lists/*` so `spc doctor --auto-fix` can run.

---

## Runtime

### ZTS PHP (Thread-Safe)

**Status:** Implemented
**Impact:** High — enables true parallel PHP execution

---

#### Architecture

PHP is compiled with `--enable-zts` via static-php-cli. Each `spawn_blocking` thread auto-registers with TSRM (Thread Safe Resource Manager) on first use, getting its own isolated PHP context — symbol tables, memory arena, extension state, and per-request globals. Multiple threads execute PHP concurrently without interference.

```
Concurrent requests → spawn_blocking → TSRM thread init (once) → PHP executes concurrently
```

Key design decisions:
- **No dedicated worker pool** — tokio's `spawn_blocking` pool is the thread pool.
- **Mutex only for lifecycle** — `Mutex<Option<PhpRuntime>>` protects one-time `init()`/`shutdown()`, never held during request execution.
- **`AtomicBool` fast path** — `execute()` checks "is PHP initialized?" without touching the mutex.
- **`__thread` C statics** — per-request state in `ephpm_wrapper.c` uses `__thread` for thread isolation (Option A from the original plan; simpler and well-supported on all target compilers).
- **Windows stays NTS** (`ZTS=0`) due to DLL constraints; serialized execution via mutex on Windows only.

**Known ZTS caveats from spc/upstream PHP:**

| Issue | Scope | Status |
|-------|-------|--------|
| OpenSSL `openssl_encrypt()` segfaults under load | x86_64 + ARM64 | Upstream PHP bug (#13648) |
| `zend_mm_heap corrupted` on ARM64 | PHP 8.5.2+ only | Upstream PHP bug (#21029) |
| OPcache static link — TLS transition linker errors | Linux musl | Upstream PHP bug (#15074) |
| ~5% throughput overhead vs NTS | All platforms | Expected; TSRM bookkeeping |
| `ext-imap` incompatible with ZTS | If used | c-client library limitation |

#### Future: Worker Mode

The next evolution is worker mode — boot the PHP app once per thread, then handle multiple requests in a loop without re-executing the bootstrap on each request (same model as FrankenPHP worker mode). ZTS makes this possible since each thread already has its own persistent PHP context.

---

## Clustering

### Prebuilt SDK Tarballs in Release Images

When publishing release container images (e.g. `ghcr.io/…/ephpm:8.5`), the PHP SDK should be compiled once in CI and baked in, rather than compiled during `docker build`. This is a natural consequence of the prebuilt SDK tarball work above and reduces the release image build matrix from O(minutes) to O(seconds) per combination.
