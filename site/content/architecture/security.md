# ePHPm Security Model

This document describes the threat model, trust boundaries, and security design for ePHPm — a single-binary application server that embeds PHP via FFI.

---

## Threat Model

### What ePHPm protects against

| Threat | Mitigation |
|--------|------------|
| PHP fatal errors crashing the host process | C wrapper with `zend_try`/`zend_catch` guards; PHP errors never unwind into Rust |
| PHP memory exhaustion | PHP `memory_limit` INI enforced inside the runtime; Rust allocator is separate |
| Malformed HTTP requests | hyper's strict HTTP/1.1 parser rejects protocol violations before reaching PHP |
| Path traversal in static file serving | Canonicalize paths and reject any resolved path outside `document_root` |
| Slowloris / slow-read attacks | hyper's `header_read_timeout` plus tokio-level timeouts from `[server.timeouts]` (no tower middleware is involved) |
| DB credential exposure in config | Any config value can be overridden via `EPHPM_`-prefixed environment variables (figment), so secrets can come from the environment instead of the TOML |

### What ePHPm does NOT protect against

- **Vulnerabilities in PHP application code** — ePHPm executes whatever PHP code is deployed. SQL injection, XSS, etc. in the application are the application's responsibility.
- **PHP interpreter CVEs** — ePHPm statically links libphp. Users must rebuild with patched PHP releases. The version matrix and release pipeline are designed to make this fast.
- **Supply chain attacks on PHP extensions** — ePHPm bundles extensions at build time. Extension selection is a trust decision made at build time, not runtime.

### Implemented security controls

The controls that exist today, in one place:

- **Per-vhost `open_basedir`** — in multi-site mode, PHP filesystem access is restricted per-request to the site's directory (+ `/tmp`)
- **`disable_shell_exec`** — `exec`, `shell_exec`, `system`, `passthru`, `proc_open`, `popen`, `pcntl_exec` disabled via the php.ini generated at startup (default on in multi-site mode)
- **`blocked_paths`** — glob patterns matched against the URI path (patterns must start with `/`); matches return 403
- **`trusted_hosts`** — Host header validation; non-matching hosts get 421. Internal endpoints (`/_ephpm/health`, `/_ephpm/ready`, the metrics path) are exempt so Kubernetes probes and Prometheus scrapes can address the pod by raw IP
- **`trusted_proxies`** — CIDR-based proxy trust for `X-Forwarded-For` / `X-Forwarded-Proto` resolution
- **Hidden-file modes** — dotfile requests handled per `hidden_files` (`deny`=403, `ignore`=404, `allow`)
- **Percent-decode traversal hardening** — strict `%XX` decoding before routing; encoded `/` and `\`, truncated or non-hex escapes, and invalid UTF-8 are rejected with 400

---

## Trust Boundaries

```
┌─────────────────────────────────────────────────┐
│                   Internet                       │
└───────────────┬─────────────────────────────────┘
                │ untrusted
                ▼
┌─────────────────────────────────────────────────┐
│           Rust HTTP Server (hyper)               │
│  • TLS termination                               │
│  • Request parsing & validation                  │
│  • Static file serving (path-checked)            │
│  • Route dispatch                                │
└───────────────┬─────────────────────────────────┘
                │ sanitized request
                ▼
┌─────────────────────────────────────────────────┐
│         PHP Runtime (libphp via FFI)             │
│  • Runs inside zend_try/zend_catch guard         │
│  • Own memory_limit, max_execution_time          │
│  • $_SERVER populated by Rust (not raw headers)  │
│  • Output captured via SAPI callbacks            │
└───────────────┬─────────────────────────────────┘
                │ application-controlled
                ▼
┌─────────────────────────────────────────────────┐
│         Upstream Services (DB, cache, etc.)       │
│  • Connected via PHP application code             │
│  • Or via ePHPm DB proxy (future)                 │
└─────────────────────────────────────────────────┘
```

### Boundary rules

1. **Internet → Rust**: All input is untrusted. hyper validates HTTP framing. ePHPm enforces size limits on headers and bodies before any allocation.
2. **Rust → PHP**: The request is mapped to `$_SERVER`, `php://input`, etc. through SAPI callbacks. Rust controls what PHP sees — raw socket data never reaches PHP directly.
3. **PHP → Upstream**: PHP application code connects to databases/caches. ePHPm does not intercept these connections in the MVP. The planned DB proxy (future) will add a trust boundary here.

---

## FFI Safety

### The setjmp/longjmp problem

PHP uses `setjmp`/`longjmp` for error handling (fatal errors, bailouts). If a PHP function called directly from Rust triggers a `longjmp`, it will skip Rust destructors and corrupt the stack. This is the #1 safety hazard.

### Mitigation: C wrapper with zend_try

Every Rust→PHP call goes through `ephpm_wrapper.c`, which wraps the call in `zend_try`/`zend_catch`:

```c
int ephpm_execute_script(const char *filename) {
    int status = FAILURE;
    zend_try {
        // PHP execution happens here — longjmp-safe
        zend_file_handle file_handle;
        zend_stream_init_filename(&file_handle, filename);
        status = php_execute_script(&file_handle) ? SUCCESS : FAILURE;
    } zend_catch {
        status = FAILURE;
    } zend_end_try();
    return status;
}
```

### Rules for FFI code

1. **Never call PHP C API directly from Rust** — always go through the C wrapper
2. **Every `unsafe` block must have a `// SAFETY:` comment** explaining what invariants are upheld
3. **No Rust objects with destructors may be live across a PHP call** — if PHP longjmps, Rust destructors won't run. Collect all data before entering the wrapper, process results after.
4. **All FFI code is gated with `#[cfg(php_linked)]`** — stub mode compiles with zero `unsafe` blocks

---

## PHP Runtime Isolation

### Memory

- PHP's memory allocator (`emalloc`/`efree`) is separate from Rust's allocator
- `memory_limit` INI directive is enforced — PHP cannot exhaust host memory without hitting its own limit first
- On memory limit exceeded, PHP triggers a fatal error caught by `zend_catch`

### Execution time

- PHP's signal-based `max_execution_time` timer is **deliberately neutralized** — its process-wide `SIGPROF` handler would crash tokio worker threads, so the zend signal functions are no-op'd (`--wrap` on Linux)
- Enforcement happens at the HTTP layer: `tokio::time::timeout` (from `server.timeouts.request`) wraps the `spawn_blocking` PHP execution and surfaces a timeout as HTTP 504

### Process state

- ZTS PHP: Concurrent execution via `spawn_blocking` + TSRM. Each thread gets isolated globals (symbol tables, memory arena, extension state). Per-request C statics use `__thread` for thread isolation. Rust must ensure no cross-thread access to PHP data.
- Windows (NTS fallback): Serialized execution via `Mutex<Option<PhpRuntime>>`. One request at a time.

### Request isolation

- Each request calls `php_request_startup()` / `php_request_shutdown()`, resetting per-request state (`$_SERVER`, `$_GET`, `$_POST`, output buffers, etc.)
- Persistent resources (DB connections via `pconnect`, opcache) survive across requests by design — this matches PHP-FPM behavior

---

## Configuration Security

### Secrets in config

The `ephpm.toml` config file should never contain plaintext secrets in production. Supported alternatives:

- **Environment variable overrides**: any config value can be set via an `EPHPM_`-prefixed environment variable with `__` as the nesting separator (figment), e.g. `EPHPM_DB__MYSQL__URL`. There is no `${VAR}` interpolation syntax inside the TOML itself.
- **File permissions**: Config file should be readable only by the ePHPm process user
- **Future**: Secrets manager integration (Vault, AWS Secrets Manager, etc.)

There is no admin interface — ePHPm exposes no admin endpoints, so there is nothing to lock down there. The optional Prometheus `/metrics` endpoint is read-only.

---

## TLS / Certificate Handling (Planned)

- ACME automation via `rustls` + certificate management
- Private keys stored with restrictive file permissions (0600)
- No custom crypto — relies on `rustls` (audited, no OpenSSL C code)
- OCSP stapling for certificate revocation checking

---

## DB Proxy Security (Planned)

- Wire protocol parsing (MySQL/Postgres) in Rust — memory-safe by default
- Connection credentials stored in config (same secret handling as above)
- Query digest logging must not log sensitive parameter values — hash or redact by default
- Connection pooling must isolate session state between application connections

---

## Supply Chain

### Build-time

- `cargo deny` checks dependency licenses and known advisories (RUSTSEC database)
- PHP built from source via `static-php-cli` in a container — reproducible, auditable
- CI pins toolchain versions via `rust-toolchain.toml`

### Runtime

- Single binary — dynamic library loading happens only for what the operator's config explicitly lists (`[[middleware]]` shared-library mounts, `[php] extensions`); nothing is loaded from ambient search paths without a config entry
- The baseline ~45 PHP extensions are compiled in at build time; additional shared extensions load only via the `[php] extensions` config knob — runtime `dl()` from PHP code remains disabled by default in the embed SAPI

---

## Incident Response

### PHP fatal error

1. `zend_catch` in the C wrapper catches the longjmp
2. Wrapper returns `FAILURE` to Rust
3. Rust logs the error via `tracing` (PHP's `log_message` SAPI callback captures the error text)
4. HTTP 500 returned to client
5. PHP runtime remains usable for subsequent requests (request shutdown cleans up)

### PHP segfault

If PHP segfaults (e.g., buggy C extension), the entire process crashes. Mitigation:
- Process supervisor (systemd, container orchestrator) restarts the process
- Future: watchdog process or pre-fork model for isolation

### Resource exhaustion

- PHP memory limit and execution time provide first-line defense
- Rust-side `tokio::time::timeout` provides a hard backstop
- OS-level cgroup limits (when running in containers) provide final defense
