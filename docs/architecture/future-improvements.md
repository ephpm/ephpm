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

---

#### Current architecture (NTS + mutex serialization)

PHP's default build is Non-Thread-Safe (NTS). It uses process-wide global state — a single interpreter with globals, symbol tables, and memory managed as one shared blob. Calling into it from two threads simultaneously causes data corruption or crashes.

ePHPm works around this with a global `Mutex<Option<PhpRuntime>>` (`crates/ephpm-php/src/lib.rs:147`). Every HTTP request that needs PHP must acquire this lock before calling into `ephpm_execute_request()`. Requests queue and PHP executes them one at a time, serially. `tokio::task::spawn_blocking` is used so the blocking lock acquisition does not stall the async runtime's worker threads.

```
Concurrent requests → spawn_blocking → mutex acquire (one at a time) → PHP executes → mutex release
```

Under load, the PHP mutex is the throughput ceiling. CPU cores sit idle while other requests wait.

---

#### What ZTS enables

Thread-Safe (ZTS) PHP replaces those global variables with TSRM — the Thread-Safe Resource Manager. Each thread gets an isolated PHP context: its own symbol tables, memory arena, extension state, and per-request globals. Multiple threads can call into PHP simultaneously without interference.

With ZTS, instead of one global runtime behind a mutex, ePHPm can maintain a **pool of PHP execution contexts**, one per `spawn_blocking` thread. Each incoming request gets its own context, executes independently, and the context is reused for the next request on that thread. No queuing at the PHP layer.

---

#### spc support

static-php-cli supports ZTS via the `--enable-zts` flag:

```bash
./bin/spc build "bcmath,curl,openssl,..." --build-embed --enable-zts
```

The xtask passes the spc build command in `xtask/src/main.rs` — adding `--enable-zts` is the only spc change needed.

**Known ZTS caveats from spc/upstream PHP:**

| Issue | Scope | Status |
|-------|-------|--------|
| OpenSSL `openssl_encrypt()` segfaults under load | x86_64 + ARM64 | Upstream PHP bug (#13648) |
| `zend_mm_heap corrupted` on ARM64 | PHP 8.5.2+ only | Upstream PHP bug (#21029) |
| OPcache static link — TLS transition linker errors | Linux musl | Upstream PHP bug (#15074) |
| ~5% throughput overhead vs NTS | All platforms | Expected; TSRM bookkeeping |
| `ext-imap` incompatible with ZTS | If used | c-client library limitation |

The OpenSSL and ARM64 issues are the most significant. x86_64 Linux (the primary deployment target) avoids the ARM64 bug; the OpenSSL issue may require disabling `openssl_encrypt()` workarounds or waiting for the upstream fix.

---

#### Implementation plan

##### 1. spc build flag — trivial

Add `--enable-zts` to the spc invocation in `xtask/src/main.rs`. No other xtask changes needed.

##### 2. `build.rs` — small

`crates/ephpm-php/build.rs:203` currently defines `ZTS=0` for the C wrapper compilation:

```rust
build.define("ZTS", Some("0"));
```

Change to `Some("1")`. The TSRM header include directories are already present in the bindgen configuration (added speculatively for this milestone). Verify that `php/TSRM/TSRM.h` is reachable and that `tsrm_get_ls_cache()` and `tsrm_ls` bindings generate correctly.

##### 3. `ephpm_wrapper.c` — significant (~1–2 days)

This is the largest change. The C wrapper (`crates/ephpm-php/ephpm_wrapper.c`) currently stores all per-request state in plain C globals:

```c
// These are all process-global today — unsafe with ZTS
static char  output_buf[OUTPUT_BUF_SIZE];
static char  headers_buf[HEADERS_BUF_SIZE];
static int   response_status_code;
static const char *req_method, *req_uri, *req_query_string;
static const char *req_content_type, *req_cookie_data;
static const char *req_post_data;
static size_t req_post_data_len;
static server_var_t server_vars[MAX_SERVER_VARS];
static int   server_var_count;
static int   request_active;
```

In ZTS each executing thread needs its own copy of this state. Two approaches:

**Option A — C11 `_Thread_local`:** Mark each variable `_Thread_local`. Simple but requires C11 and verification that all compilers in the build matrix support it for these types (fine for musl/glibc gcc, needs checking for the Windows cross-compile path).

**Option B — Per-context struct passed through FFI:** Define a `EphpmRequestContext` struct containing all the above fields. Allocate one per `spawn_blocking` thread on the Rust side (using `thread_local!`) and pass a pointer into each C function call. More explicit, avoids C11 TLS, easier to reason about lifetime.

Option B is recommended: it keeps the ZTS coordination visible in Rust rather than hidden in C, and it makes the per-request state lifetime explicit.

##### 4. Per-thread request lifecycle — moderate (~1 day)

The current design intentionally avoids calling `php_request_startup` / `php_request_shutdown` per HTTP request. Instead it reuses the single embed request started by `php_embed_init()` (see comment at `ephpm_wrapper.c:475`). This works with the global mutex — only one request is ever active.

ZTS requires proper per-thread request lifecycle:

1. On first use of a `spawn_blocking` thread, call `php_request_startup()` with a new TSRM context.
2. For each PHP script execution, the context is already active — run `php_execute_script()`.
3. Between requests, optionally reset per-request state without a full `php_request_shutdown` / `php_request_startup` cycle (same optimisation as today, but per-thread).
4. When a thread exits (or the pool shrinks), call `php_request_shutdown()` and `tsrm_thread_end()`.

The embed SAPI's `php_embed_init()` still runs once at process startup. `php_embed_shutdown()` still runs once at process exit. The per-thread lifecycle sits between those two.

##### 5. Rust pool replacing the global mutex — moderate (~1 day)

Replace:

```rust
// lib.rs:147 — today
static PHP_RUNTIME: Mutex<Option<PhpRuntime>> = Mutex::new(None);
```

With a pool of per-thread contexts. The natural fit is `thread_local!` storage in the `spawn_blocking` thread pool:

```rust
// Sketch — not final API
thread_local! {
    static PHP_CTX: RefCell<Option<PhpContext>> = RefCell::new(None);
}
```

Each `spawn_blocking` call checks whether the current thread already has an initialised `PhpContext`. If not, it initialises one (calls `php_request_startup()` + TSRM setup). Subsequent requests on the same thread reuse the context without any global locking.

`PhpRuntime::execute()` becomes:

```rust
pub fn execute(request: PhpRequest) -> Result<PhpResponse, PhpError> {
    PHP_CTX.with(|ctx| {
        let mut ctx = ctx.borrow_mut();
        if ctx.is_none() {
            *ctx = Some(PhpContext::init()?);
        }
        ctx.as_mut().unwrap().execute_php(&request)
    })
}
```

No mutex, no contention between threads.

---

#### Reference implementation

FrankenPHP uses ZTS PHP via the same embed SAPI approach and has solved all of these problems in production. Its C embedding layer (`frankenphp.c`) is the best reference for:

- Per-worker TSRM context initialisation
- Thread-safe request lifecycle
- Signal handling with ZTS
- OPcache compatibility workarounds

---

#### Migration path

ZTS is a compile-time PHP build difference — the same `cargo xtask release` workflow applies, just with a different `libphp.a`. NTS and ZTS binaries are not ABI-compatible. There is no gradual migration; it is a flag day switch.

Suggested sequencing:

1. Add `--enable-zts` to spc build, get a ZTS `libphp.a` linking successfully
2. Flip `ZTS=0` → `ZTS=1` in `build.rs`, fix any bindgen breakage
3. Convert C globals to per-context struct (Option B above)
4. Implement per-thread request lifecycle in C
5. Replace global mutex with `thread_local!` pool in Rust
6. Stress test, particularly OpenSSL paths and OPcache

---

## Clustering

### Prebuilt SDK Tarballs in Release Images

When publishing release container images (e.g. `ghcr.io/…/ephpm:8.5`), the PHP SDK should be compiled once in CI and baked in, rather than compiled during `docker build`. This is a natural consequence of the prebuilt SDK tarball work above and reduces the release image build matrix from O(minutes) to O(seconds) per combination.
