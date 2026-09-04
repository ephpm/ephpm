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
| Host-header path traversal in multi-site mode | The `Host` header is validated against a strict DNS-label allowlist before it is joined onto `sites_dir`; separators, `..` segments, NUL, and non-DNS characters are rejected with 404 before routing (independent of `trusted_hosts`) |
| Slowloris / slow-read attacks | hyper's `header_read_timeout` plus tokio-level timeouts from `[server.timeouts]` (no tower middleware is involved) |
| DB credential exposure in config | Any config value can be overridden via `EPHPM_`-prefixed environment variables (figment), so secrets can come from the environment instead of the TOML |

### What ePHPm does NOT protect against

- **Vulnerabilities in PHP application code** — ePHPm executes whatever PHP code is deployed. SQL injection, XSS, etc. in the application are the application's responsibility.
- **PHP interpreter CVEs** — ePHPm statically links libphp. Users must rebuild with patched PHP releases. The version matrix and release pipeline are designed to make this fast.
- **Supply chain attacks on PHP extensions** — ePHPm bundles extensions at build time. Extension selection is a trust decision made at build time, not runtime.
- **Outbound network access from tenant PHP (egress).** ePHPm applies **no** egress policy of its own. The `multi_tenant_hardening` denylist disables `fsockopen`/`pfsockopen`, but it disables them because persistent sockets leak *between tenants* via `EG(persistent_list)` — not to close egress, and reading that one entry as an egress control is a misread. Every other outbound path stays open to tenant PHP: `curl_*` (ext-curl is compiled in), `stream_socket_client`, the HTTP/FTP stream wrappers behind `file_get_contents`/`fopen` (`open_basedir` gates the *files* wrapper, not `http://`), and PDO's own drivers. A hostile tenant can therefore reach any address the host itself can reach — the cloud metadata endpoint, the LAN, another tenant's loopback service, or a public collector for exfiltration. Closing this is **host configuration**, not an ePHPm setting: a per-uid nftables/`IPAddressDeny` floor, and optionally the experimental [per-vhost eBPF policy](/guides/ebpf-per-vhost-network/) for loopback ownership (which governs loopback and sidecar ports, not public egress). ePHPm's own `[server.security] network_egress_externally_managed` is an *assertion* that you have built such a floor — setting it drops `fsockopen` from the denylist, it does not add any enforcement. See [Multi-tenant hardening](/guides/multi-tenant-hardening/).
- **Cross-tenant availability in multi-tenant mode** — all tenants share one process, so one tenant can crash it (e.g. a deep recursive object-graph free overflowing the native C stack, which the VM stack guard does not bound) and take every tenant down. The `multi_tenant_hardening` denylist closes cross-tenant *confidentiality/integrity* channels but cannot close this shared-fate *availability* residual. Hosting mutually **untrusted** tenants that must not be able to DoS each other requires per-tenant process/uid isolation (separate ePHPm processes/pods), which the single-process model does not provide. `run_as_user` removes the root-escalation risk but does not add a per-tenant boundary.

### Implemented security controls

The controls that exist today, in one place:

- **Per-vhost `open_basedir`** — in multi-site mode, PHP filesystem access is restricted per-request to the site's directory plus that site's **own private state root** (see the next bullet). Entries are joined with the platform's `PATH_SEPARATOR` (`:` on Unix, `;` on Windows).
- **Per-vhost temp & session isolation** — in multi-site mode each vhost gets a private state directory `<system-temp>/ephpm-vhosts/<label>-<digest>` (base honours `TMPDIR`; the `<label>-<digest>` is derived from the resolved, traversal-safe document root, so it is stable per site and never collides across sites). Its `tmp/` and `sessions/` subdirectories are created once per site (`0700` on Unix) and wired into every request for that vhost as `sys_temp_dir` + `upload_tmp_dir` = `.../tmp` and `session.save_path` = `.../sessions`. Only that one state root — never the shared system temp — is in the vhost's `open_basedir`, so one tenant cannot read, enumerate, or overwrite another tenant's temp files, uploads, or PHP session files. This closes the shared-`/tmp` cross-tenant read/write and session-hijack hole (issue #276). `session.save_path` and `upload_tmp_dir` are re-read per request, so sessions and uploads are physically separated per tenant while the default `files` session handler keeps working. Single-site deployments (no `sites_dir`) are unaffected — `open_basedir` stays off and PHP keeps its default temp/session behaviour.
- **`disable_shell_exec`** — `exec`, `shell_exec`, `system`, `passthru`, `proc_open`, `popen`, `pcntl_exec` disabled via the php.ini generated at startup (default on in multi-site mode)
- **`multi_tenant_hardening`** — the confidentiality/integrity denylist preset, default on in multi-site mode. On top of `disable_shell_exec` it disables (as a **union** with any operator `disable_functions`, never clobbering it) the cross-tenant channels a hostile-PHP-userland pentest proved reachable in one shared ZTS process: `pfsockopen`/`fsockopen` (persistent-socket inheritance via `EG(persistent_list)` — this closes cross-tenant socket *inheritance*, **not** egress; `curl_*`, `stream_socket_client`, and the HTTP stream wrappers stay open, see [above](#what-ephpm-does-not-protect-against)), the SysV IPC family `shm_*`/`sem_*`/`msg_*`, `pcntl_*` + `posix_kill`/`posix_set*id` process control, `opcache_reset`/`opcache_compile_file`, `dl`, and `mail`; plus `mysqli.allow_persistent=0` and — when `[opcache] cluster_invalidation` is off — `opcache.restrict_api`. Cost: persistent DB/socket connections are disabled. See [Virtual Hosts → Multi-tenant hardening preset](/guides/virtual-hosts/#multi-tenant-hardening-preset). Residual (not closed by any denylist): a single tenant can still crash the shared process (e.g. deep recursive object-graph destruction overflowing the C stack), taking every tenant down — a shared-fate **availability** problem that needs per-tenant process isolation.
- **`run_as_user` / `run_as_group`** (Unix) — after binding privileged ports and opening root-owned files, ePHPm permanently drops the whole process from root to an unprivileged uid/gid (`setgroups`+`setgid`+`setuid`, verified, irreversible) before serving. Removes the root-escalation blast radius. This is a **single non-root uid for the entire process, not per-tenant** — cross-tenant isolation still rests on `open_basedir` + the denylist, not kernel permissions. Per-tenant uids would require per-tenant processes, which the single-process model does not provide.
- **`blocked_paths`** — glob patterns matched against the URI path (patterns must start with `/`); matches return 403
- **Multi-site `Host` sanitization** — in multi-site mode (`sites_dir` set) the `Host` header is normalized (port/trailing-dot stripped, lowercased) and validated against a strict DNS-label allowlist (`[a-z0-9._-]`, no empty label) **before** it is used to resolve a document root. Hosts containing `..`, `/`, `\`, NUL, or any other non-DNS character are rejected with 404. This runs independently of `trusted_hosts`, so it protects the default configuration (empty `trusted_hosts`) against Host-header path traversal
- **`trusted_hosts`** — additional exact-match Host allowlisting; non-matching hosts get 421. Empty by default. Internal endpoints (`/_ephpm/health`, `/_ephpm/ready`, `/_ephpm/primary`, `/_ephpm/requests` when the request timeline is enabled, the metrics path) are exempt so Kubernetes probes and Prometheus scrapes can address the pod by raw IP. Note: exact-match only (no wildcards), so it cannot cover dynamic per-PR preview hostnames — the `Host` sanitization above is what protects those, not `trusted_hosts`
- **`trusted_proxies`** — CIDR-based proxy trust for `X-Forwarded-For` / `X-Forwarded-Proto` resolution
- **Hidden-file modes** — dotfile requests handled per `hidden_files` (`deny`=403, `ignore`=404, `allow`)
- **Deploy-manifest deny** — a request whose path contains a segment named `ephpm.yaml`, `ephpm.yml` or `ephpm.json` (case-insensitive, any position) is refused per `deploy_manifests` (`deny`=403 — the default, `ignore`=404, `allow`). A deploy manifest is deployment metadata — build commands, enabled services, the seed sequence — and an application deployed with its document root at the repository root would otherwise serve it verbatim. Deploy tooling is expected to keep the manifest out of a served directory; this is the independent server-side guarantee that a manifest is not *servable* however the site was laid down. It applies to every virtual host in multi-site mode, runs before any filesystem lookup (so it leaks no existence information), and is deliberately independent of `hidden_files` — `hidden_files = "allow"` does not re-open it. It is **not** a general secret-file scanner: it knows these three names and nothing else, so an application's other deployment metadata (`Dockerfile`, `.github/`, framework config) still needs `blocked_paths` or a document root that does not contain it
- **Percent-decode traversal hardening** — strict `%XX` decoding before routing; encoded `/` and `\`, truncated or non-hex escapes, and invalid UTF-8 are rejected with 400
- **Encrypted cluster transport** — with `[cluster] secret` set, all inter-node traffic is authenticated and sealed with ChaCha20-Poly1305 keys derived per plane via HKDF-SHA256: gossip UDP, the KV data plane, and (when clustered SQLite is in use) the mutual-handshake [cluster channel](/architecture/clustering/#cluster-channel-tcp-opt-in) carrying Turso CDC replication with per-session keys. Clustering fails closed when the secret is empty. Symmetric PSK, not mTLS — see [Clustering → Inter-Node Security](/architecture/clustering/#2-inter-node-security).

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
│  • Or via the ePHPm DB proxy (shipped)            │
└─────────────────────────────────────────────────┘
```

### Boundary rules

1. **Internet → Rust**: All input is untrusted. hyper validates HTTP framing. ePHPm enforces size limits on headers and bodies before any allocation.
2. **Rust → PHP**: The request is mapped to `$_SERVER`, `php://input`, etc. through SAPI callbacks. Rust controls what PHP sees — raw socket data never reaches PHP directly.
3. **PHP → Upstream**: PHP application code connects to databases and caches. When `[db.mysql]` / `[db.postgres]` (the [DB proxy](/architecture/database/db-proxy/)) or `[db.sqlite]` (embedded) is configured, **ePHPm does intercept these connections** — it terminates the wire protocol in Rust, pools upstream connections, and parses every statement for [query stats](/architecture/query-stats/). Two consequences worth knowing:
   - **Your SQL is normalized and exported.** Literals are replaced with `?`, the result is truncated to 64 characters and emitted as the `digest` Prometheus label, and slow queries are logged at WARN. Parameter *values* are never included — the normalizer strips them — but table, column, and query shape are visible to anyone who can read `/metrics` or the logs.
   - **Session state is reset between application connections** according to `[db.*] reset_strategy` (default `"smart"` — reset after any non-SELECT).

   Whether the embedded-SQLite listeners authenticate depends on the mode. In **single-site** mode they do not — they assume only PHP inside this process reaches them, so bind them to loopback. In **multi-tenant** mode (`[server] sites_dir` with `[db.sqlite]`) the MySQL listener does: a connection must answer a `mysql_native_password` challenge for the site key it claims, and the connection's database is fixed by the credential it authenticated with, not by anything it asserts. See [DB Proxy Security](#db-proxy-security-implemented) below. The in-process [`ephpm_db_*` bridge](/guides/db-from-php/) skips the socket entirely and is reachable by any PHP code in the request.

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

Two layers bound how long a request can run:

- **`max_execution_time` (PHP-level, catchable).** On Linux ZTS builds with per-thread execution timers (`--enable-zend-max-execution-timers`, the shipped default), PHP arms a **per-thread POSIX timer** (`timer_create` + `SIGRTMIN` via `SIGEV_THREAD_ID`, on a wall-clock `CLOCK_BOOTTIME`) delivered only to the owning PHP thread — safe under the dedicated execution pool. Exceeding it raises the catchable "Maximum execution time exceeded" fatal (500, shutdown functions run, output flushed), and `set_time_limit()` re-arms it at runtime. On builds without per-thread timers (macOS, and Windows — ZTS but built without `ZEND_MAX_EXECUTION_TIMERS`), the inner timer cannot be armed safely, so `max_execution_time` is not enforced there (on Linux the unsafe process-wide `SIGPROF`/`setitimer` fallback is additionally neutralized via `--wrap`).
- **`[server.timeouts] request` (HTTP-level, hard backstop).** `tokio::time::timeout` wraps the pooled PHP execution and surfaces a timeout as HTTP 504 — the ceiling for a script wedged where the inner timer cannot interrupt it (a C extension or syscall that never returns to the VM). Keep `max_execution_time` below this value so the inner, graceful limit fires first.

### Process state

- ZTS PHP: Concurrent execution via the dedicated PHP execution pool + TSRM. Each thread gets isolated globals (symbol tables, memory arena, extension state). Per-request C statics use `__thread` for thread isolation. Rust must ensure no cross-thread access to PHP data.
- Windows: also ZTS (#326) — same concurrent execution model as Linux/macOS; the mutex only guards one-time init/shutdown there too.

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

## TLS / Certificate Handling (Implemented)

Both modes ship: manual `[server.tls] cert` + `key`, and ACME via
`[server.tls] domains`. See the [TLS / ACME guide](/guides/tls-acme/).

- **No custom crypto.** Everything goes through `rustls` — no OpenSSL C code
  in the TLS path.
- **Two ACME challenge types ship.** The default `challenge = "tls-alpn-01"`
  (via `rustls-acme`) answers inline on the TLS listener; HTTP-01 is not
  implemented. `challenge = "dns-01"` publishes a `_acme-challenge` TXT record
  through a DNS provider (Cloudflare, Linode, DigitalOcean, AWS Route 53, or
  Google Cloud DNS) and is **the only lane that can issue a wildcard
  certificate** (`*.example.com`). With TLS-ALPN-01 you must still name every
  host explicitly in `domains`.

Two gaps below apply to the **TLS-ALPN-01** lane in a cluster. The **DNS-01**
lane avoids both — the leader answers the challenge over DNS (nothing has to
reach a specific node) and its certificate resolver is hot-swappable, so a
follower installs a renewed certificate from the KV store without a restart —
which makes DNS-01 the better fit for clustered deployments:

- **Challenge tokens are not shared between nodes.** A node can serve
  `/.well-known/acme-challenge/<token>` from the KV store, but nothing
  populates those keys, and the TLS-ALPN-01 challenge material lives only in
  the ordering node's in-memory resolver. **Validation traffic must reach the
  ACME leader.**
- **Followers do not pick up renewed certificates while running.** The leader
  writes issued certs to the KV store and followers load them on cache miss at
  startup — but `rustls-acme` consults its cache once per state machine, so a
  *renewal* is not injected into a running follower. **Followers serve the
  certificate they loaded at startup until they are restarted.** Budget a
  restart inside the renewal window.

**Not implemented:** OCSP stapling. ePHPm does not staple revocation
responses, and does not set explicit restrictive permissions on the ACME
cache directory — it inherits the process umask.

---

## DB Proxy Security (Implemented)

The proxy ships (`[db.mysql]`, `[db.postgres]`). What that means for security:

- **Wire protocol parsing (MySQL/PostgreSQL) is in Rust** — memory-safe by
  construction, no C parser in the path.
- **Upstream credentials come from config**, with the same secret handling as
  above (prefer `EPHPM_DB__MYSQL__URL` over plaintext in `ephpm.toml`).
- **Query logging is redacted by normalization, not by hashing.** The
  normalizer replaces every literal with `?` before a statement is used as a
  metric label or written to the slow-query log, so parameter *values* never
  reach either. Query *shape* — tables, columns, structure — does reach both.
  Treat `/metrics` and the logs accordingly.
- **Pooled connections reset session state between application connections**
  per `[db.*] reset_strategy` (default `"smart"`: reset after any
  non-SELECT). `"never"` disables that isolation — do not use it when
  different tenants share a pool.

### Embedded-SQLite wire listeners

Authentication on the `[db.sqlite.proxy]` listeners depends on the deployment
mode — the two cases are genuinely different, and conflating them is how the
multi-tenant guarantee gets misread in both directions:

- **Multi-tenant (`[server] sites_dir` + `[db.sqlite]`): the MySQL listener is
  authenticated.** One listener serves every tenant. A connection's username is
  the site key and its password is `HMAC-SHA256(per-process master secret,
  site_key)`; the `mysql_native_password` response is verified **before** the
  backend registry is consulted (`crates/ephpm-server/src/site_wire_auth.rs`),
  so a caller that names a neighbour's site without its password gets
  `ER_ACCESS_DENIED` and never causes that site's database file to be opened.
  The master secret is 32 random bytes generated at startup, never written to
  disk and never exposed to PHP; each site's own password is injected into that
  site's `$_SERVER` per request. Hrana, PostgreSQL, and TDS stay **off** in this
  mode — they cannot bind a backend per connection.
- **Single-site: unauthenticated.** With no `sites_dir` there is one database
  and one tenant, so the listeners (MySQL, Hrana, PostgreSQL, TDS) perform no
  authentication and assume only PHP inside this process reaches them. A
  non-loopback bind logs a warning but is not blocked.

---

## Supply Chain

### Build-time

- `cargo deny` checks dependency licenses and known advisories (RUSTSEC database)
- PHP is **not** built by this repo. `cargo xtask release` downloads a prebuilt SDK tarball (`libphp.a` + headers) from [github.com/ephpm/php-sdk](https://github.com/ephpm/php-sdk) releases, pinned per PHP minor in `xtask/src/main.rs`. That separate pipeline is what uses `static-php-cli`; ePHPm has no dependency on it.
- CI pins toolchain versions via `rust-toolchain.toml`

### Runtime

- Single binary — dynamic library loading happens only for what the operator's config explicitly lists (`[[middleware]]` shared-library mounts, `[php] extensions`); nothing is loaded from ambient search paths without a config entry
- The baseline ~45 PHP extensions are compiled in at build time; additional shared extensions load only via the `[php] extensions` config knob. Note that ePHPm does **not** explicitly set `enable_dl` — runtime `dl()` availability is whatever the embed SAPI's own default gives you. If you need it guaranteed off, set `enable_dl=0` through `[php] ini_overrides`.

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
