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

**Status:** Research
**Impact:** High — enables true parallel PHP execution

The current MVP uses NTS (non-thread-safe) PHP behind a `Mutex<Option<PhpRuntime>>`. All PHP execution is serialised through a single lock, with tokio's `spawn_blocking` used to avoid blocking the async runtime. This means concurrent requests queue at the PHP layer.

Switching to ZTS PHP would allow multiple PHP contexts to execute simultaneously, one per `spawn_blocking` thread. The main challenges:

- spc must be told to build ZTS PHP (`--build-embed-zts` or equivalent flag)
- All FFI boundary code in `ephpm_wrapper.c` must be audited for thread-safety
- Per-request TSRM context management must be implemented
- The global `Mutex<Option<PhpRuntime>>` is replaced by a thread-local or per-context handle

FrankenPHP uses ZTS PHP via the same embed SAPI approach and is the reference implementation for this pattern.

---

## Clustering

### Prebuilt SDK Tarballs in Release Images

When publishing release container images (e.g. `ghcr.io/…/ephpm:8.5`), the PHP SDK should be compiled once in CI and baked in, rather than compiled during `docker build`. This is a natural consequence of the prebuilt SDK tarball work above and reduces the release image build matrix from O(minutes) to O(seconds) per combination.
