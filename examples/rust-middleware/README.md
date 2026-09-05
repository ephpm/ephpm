# `api-gate` — a native ePHPm middleware in Rust

A complete, loadable middleware module: an **API-key gate with a cluster-wide
rate limiter**, built as a `cdylib`, `dlopen`ed by a stock ePHPm binary, and
deciding real HTTP requests before PHP runs.

It is the counterpart to
[`ephpm/elephc-middleware-example`](https://github.com/ephpm/elephc-middleware-example),
which compiles PHP to a native module and needs ~180 lines of hand-written C
shim to bridge elephc's output to the ABI. **This example needs no shim.** In
Rust the ABI is used as designed: one `declare!` line generates the four
exported C symbols, the version handshake, config parsing, panic containment
and the response marshaling. There is not a single `unsafe` block in
[`src/lib.rs`](src/lib.rs) outside the unit tests.

Everything below was verified end to end — see
[Verified transcript](#verified-transcript).

---

## What it does

| Request | Verdict | Result |
|---|---|---|
| `GET /health.php` | `ACTION_CONTINUE` | PHP runs untouched; `X-Api-Gate: bypass` appended |
| `GET /api/v1/users` (no vhost matched, `require_vhost = true`) | `ACTION_RESPOND` | `404` JSON, PHP never runs — **off by default**, see [`require_vhost`](#require_vhost-and-why-the-default-is-false) |
| `GET /api/v1/users` (no key) | `ACTION_RESPOND` | `401` JSON, PHP never runs |
| `GET /api/v1/users` (revoked key) | `ACTION_RESPOND` | `403` JSON, PHP never runs |
| `GET /api/v1/users` (over budget) | `ACTION_RESPOND` | `429` + `Retry-After`, PHP never runs |
| `GET /api/v1/users` (valid key) | `ACTION_REWRITE` | `REQUEST_URI` → `/users`, `X-Api-Tenant` injected, PHP runs |

Host callbacks used: **`kv_incr_ttl`** (the fixed-window counter — one atomic
call; cluster-wide with no code change once KV replication is on),
**`kv_get`** (runtime revocation), and **`log`** (module diagnostics into
ePHPm's `tracing` stream).

---

## The ABI in one page

A middleware module is a shared library exporting four C symbols. The
canonical definition is
[`crates/ephpm-middleware/src/abi.rs`](../../crates/ephpm-middleware/src/abi.rs);
this is the summary.

```c
int32_t     ephpm_middleware_init(uint32_t abi_version,
                                  const char* config_json,
                                  const ephpm_host_v1* host);
int32_t     ephpm_middleware_invoke(const ephpm_request_t* request,
                                    ephpm_response_t* response_out);
void        ephpm_middleware_shutdown(void);
const char* ephpm_middleware_describe(void);   /* optional, nullable */
```

**The host table is passed by pointer**, not `dlsym`'d out of the host
executable. Exporting symbols from an executable needs `-rdynamic` on Linux
and has no clean Windows analogue; a table pointer is portable everywhere
`dlopen`/`LoadLibrary` is. The pointer is valid for the process lifetime.

**Request data comes from accessor function pointers on that table**, not a
flat struct, so fields can be added without an ABI break: additions append to
the end of the table under the same major version. `kv_incr_ttl` is itself
such an append — it sits after `log`, and a module built before it existed
simply never reads that far.

### Two rules you must honour

1. **Refuse a newer host major.** `ABI_V1` is `0x01_00_00_00`; the top byte
   gates compatibility. If `abi_version >> 24` exceeds what you were built
   for, return non-zero from `init` and do not load. A struct-layout
   disagreement that loads anyway is memory corruption, not a warning.
   `declare!` does this check and returns `-1`.

2. **Keep response pointers alive until `invoke` returns.** Everything you
   write into `EphpmResponse` — body, rewrite path, header names and values —
   must still be valid when `invoke` returns; the host copies before
   unwinding. You may not point at a temporary. `declare!` parks the marshaled
   buffers in a thread-local that lives until the next `invoke` on that
   thread, so returning an owned `Response` is always correct.

Both rules are also why the C example needs a shim and this one does not.

### The three verdicts

| Verdict | Meaning | `Response::header()` means | `Response::path()` |
|---|---|---|---|
| `ACTION_CONTINUE` | run the rest of the chain, then PHP | — (use `response_header()`) | ignored |
| `ACTION_REWRITE` | mutate the request, then continue | **request**-header override | rewrites `REQUEST_URI` |
| `ACTION_RESPOND` | short-circuit; PHP never runs | **response** header | ignored |

`RESPOND` stops the chain immediately. `REWRITE` accumulates: the path
override is last-writer-wins and header overrides apply in chain order, and
the router applies them **after** the whole chain has run — so a later module
still sees the original request, not an earlier module's rewrite.

**`REWRITE` semantics worth knowing before you design around it:** in fpm mode
the script has already been resolved by the time the chain runs, so a path
rewrite changes `REQUEST_URI` (what a front controller routes on) and **not**
which file executes. In worker mode every request goes through PHP and the
booted framework routes on the rewritten `REQUEST_URI`. This example is built
around a front controller precisely so the rewrite is meaningful in both.

---

## Build

The crate declares `crate-type = ["cdylib", "rlib"]`; the `cdylib` is the
module. The `rlib` exists only so `cargo test` can link the unit tests.

```bash
cargo build --release -p ephpm-middleware-example
```

| Platform | Artifact |
|---|---|
| Linux | `target/release/libapi_gate.so` |
| macOS | `target/release/libapi_gate.dylib` |
| Windows | `target/release/api_gate.dll` |

Add `--target <triple>` if you need a specific triple; the artifact then lands
under `target/<triple>/release/`. The module and the ePHPm binary loading it
must be built for the same platform and the same ABI major.

Run the unit tests (they build a `Request` against a real in-memory KV store,
no server needed — the `host` feature of `ephpm-middleware` is a dev-dependency
for exactly this):

```bash
cargo test -p ephpm-middleware-example
```

There is also a standalone probe that `dlopen`s the built module the way the
ePHPm loader does and checks the handshake — useful when bringing up your own
module, because it tells you whether the host will accept it without standing
up a server. Build both with the same flags:

```bash
cargo build --release -p ephpm-middleware-example
cargo run   --release -p ephpm-middleware-example --example abi_probe
```

---

## Mount it

```toml
[[middleware]]
library = "/abs/path/target/release/libapi_gate.so"
order = 10
config = { keys = { k-alpha = "alpha", k-beta = "beta" }, prefix = "/api/", strip_prefix = "/api/v1", requests_per_window = 5 }
```

`library` resolves in this order:

1. **A builtin name** (`jwt`, `cors`, `ratelimit`, `security-headers`) — the
   compiled-in static registry, no `dlopen` at all.
2. **A bare name** — resolved through the middleware search path with a
   platform suffix, e.g. `auth-jwt` → `auth-jwt.linux-x86_64.so`.
3. **An explicit path** — any value containing a path separator or a file
   extension is used as-is.

**Use an explicit path when you want to be sure you are testing the dlopen
lane.** A bare name is checked against the builtin registry first, so a
collision would quietly run something else.

`order` sets chain position (lower first). An optional `match = "/api/*"` glob
restricts the mount to matching paths. `config` is serialised to JSON and
handed to `init`; a mount whose `init` fails **aborts server startup** rather
than silently not running — which is the only safe default for an auth module.

### Config keys for this module

| key | default | meaning |
|---|---|---|
| `keys` (object) | **required**, non-empty | `"api key" -> "tenant"` |
| `prefix` (string) | `"/api/"` | only paths with this prefix are gated |
| `strip_prefix` (string) | unset | removed from the front of the path on `REWRITE` |
| `requests_per_window` (integer) | `100` | budget per tenant per 10-second window |
| `require_vhost` (bool) | `false` | deny requests that matched no virtual host — see below |

### `require_vhost`, and why the default is `false`

`req.vhost_id()` is `None` for **two** situations the ABI cannot tell apart
(issue [#453](https://github.com/ephpm/ephpm/issues/453)): a node with no
tenancy configured (no `sites_dir`, no `[[site]]` — nothing to match, so *every*
request is `None`), and a multi-site node that matched nothing. A module that
denies on `None` unconditionally therefore denies all traffic on the first
shape, which is the majority of deployments.

So this module treats "no tenant" as one untenanted bucket
(`ephpm_middleware::UNMATCHED_VHOST`) by default — the same choice the stock
`ratelimit` and `maintenance-mode` modules make — and denial is an operator
opt-in via `require_vhost = true`, set only by someone who knows the node is
multi-tenant. The three-way block that implements this is the one the
[native-middleware guide](https://ephpm.dev/guides/native-middleware/) quotes;
`tests/guide_snippet.rs` fails the build if the guide and `src/lib.rs` drift
apart, and `a_single_site_node_is_served_not_denied` in `src/lib.rs` asserts
the untenanted request is actually served.

---

## Run the demo

[`demo/`](demo) is a complete, self-contained site: a front controller, a
health endpoint outside the gate, and a page that revokes a tenant from PHP.

```bash
cargo build --release -p ephpm-middleware-example
cd examples/rust-middleware/demo
# edit the `library` path in ephpm.toml for your platform first
ephpm serve --config ephpm.toml
```

Then, against `http://127.0.0.1:8099`:

```bash
# CONTINUE — outside the gate
curl -i /health.php                                    # 200, X-Api-Gate: bypass

# RESPOND — no key
curl -i /api/v1/users                                  # 401 + WWW-Authenticate

# REWRITE — valid key
curl -i -H 'X-Api-Key: k-alpha' /api/v1/users          # 200, REQUEST_URI=/users, tenant=alpha

# RESPOND — over budget (budget is 5/10s in the demo config)
for i in $(seq 1 8); do curl -s -o /dev/null -w '%{http_code} ' \
  -H 'X-Api-Key: k-alpha' /api/v1/users; done          # 200×5 then 429

# RESPOND — revoked from PHP, enforced in native code
curl -s '/revoke.php?tenant=beta'                      # PHP writes the KV marker
curl -i -H 'X-Api-Key: k-beta' /api/v1/users           # 403
curl -s '/revoke.php?tenant=beta&restore=1'            # back to 200
```

`revoke.php` is the interesting one: PHP calls `ephpm_kv_set()`, the module
reads the same literal key through the host table's `kv_get`, and the next
request is rejected in native code with no restart and no config reload. TTL
is in **seconds** on both surfaces.

> **Multi-tenant too, since ABI minor 3** (issue #376). The middleware host
> table's `kv_*` callbacks are now bound per request to the *serving vhost's*
> store — the same one PHP is rebound to — so this demo works unchanged with
> `[server] sites_dir` set: `revoke.php` on `tenant-a.example` revokes for
> `tenant-a.example` and nowhere else. The rate-limit counter becomes
> per-tenant for free. For node-wide state (operator-owned config a tenant
> must not be able to rewrite) use the explicit `kv_*_global` accessors.

> `revoke.php` has no authentication. It is a demo, not a pattern.

---

## Verified transcript

Run against a freshly built PHP-linked `ephpm.exe` (PHP 8.5.7) on
**Windows 11 / x86_64-pc-windows-msvc**, mounting the `api_gate.dll` built from
this crate. The server confirms the dlopen at startup:

```text
INFO ephpm_server::middleware: middleware initialised
     module=…\release\api_gate.dll describe=api-gate 0.1.0 (rust middleware example)
INFO ephpm_server: middleware chain loaded count=1 modules=["…\release\api_gate.dll"]
```

**CONTINUE** — outside the gate; PHP runs and the gate only appends a header:

```text
$ curl -sS -D- http://127.0.0.1:8099/health.php
HTTP/1.1 200 OK
x-powered-by: PHP/8.5.7
content-type: application/json
x-api-gate: bypass

{ "status": "ok", "gated": false, "tenant": null }
```

**RESPOND 401** — gated path, no key. No `x-powered-by`: PHP never ran.

```text
$ curl -sS -D- http://127.0.0.1:8099/api/v1/users
HTTP/1.1 401 Unauthorized
content-type: application/json
x-api-gate: reject
www-authenticate: ApiKey realm="api-gate"

{"error":"missing or unknown X-Api-Key"}
```

An unknown key (`X-Api-Key: nope`) returns `401` the same way.

**REWRITE** — valid key. `REQUEST_URI` was rewritten from `/api/v1/users?limit=5`
to `/users?limit=5`, the tenant was injected as a request header, and the
front controller still executed:

```text
$ curl -sS -D- -H 'X-Api-Key: k-alpha' 'http://127.0.0.1:8099/api/v1/users?limit=5'
HTTP/1.1 200 OK
x-powered-by: PHP/8.5.7
x-api-gate: allow
x-ratelimit-limit: 5
x-ratelimit-remaining: 4

{ "script": "index.php", "request_uri": "/users?limit=5", "tenant": "alpha" }
```

**RESPOND 429** — budget is 5 per 10-second window; the 6th request trips it:

```text
$ for i in 1..8: curl -o /dev/null -w '%{http_code}' -H 'X-Api-Key: k-gamma' …/api/v1/x
200 200 200 200 200 429 429 429

HTTP/1.1 429 Too Many Requests
retry-after: 7
x-ratelimit-limit: 5
x-ratelimit-remaining: 0

{"error":"rate limit exceeded"}
```

**RESPOND 403** — PHP and the native module on one KV store:

```text
$ curl -o NUL -w 'before=%{http_code}' -H 'X-Api-Key: k-beta' …/api/v1/x
before=200

$ curl -sS 'http://127.0.0.1:8099/revoke.php?tenant=beta'      # PHP: ephpm_kv_set()
{"tenant":"beta","revoked":true,"key":"apigate:revoked:beta","ttl":300}

$ curl -sS -D- -H 'X-Api-Key: k-beta' …/api/v1/x
HTTP/1.1 403 Forbidden
{"error":"api key revoked"}

$ curl -sS 'http://127.0.0.1:8099/revoke.php?tenant=beta&restore=1'
$ curl -o NUL -w 'after_restore=%{http_code}' -H 'X-Api-Key: k-beta' …/api/v1/x
after_restore=200
```

The module's own `log` callback surfaced in ePHPm's `tracing` output during
that sequence, correctly levelled and targeted:

```text
INFO ephpm_middleware: api-gate: rejecting revoked tenant `beta`
```

**ABI handshake** — `cargo run -p ephpm-middleware-example --example abi_probe`,
against the same `api_gate.dll`:

```text
module:  …\release\api_gate.dll
symbols: init, invoke, shutdown, describe — all resolved
describe: api-gate 0.1.0 (rust middleware example)
handshake: host major 2 refused (rc = -1)
handshake: null host table refused (rc = -1)
handshake: config without `keys` refused (rc = -3)
handshake: ABI_V1 + valid config accepted (rc = 0)
invoke:  /health.php -> ACTION_CONTINUE
invoke:  /api/v1/users -> ACTION_RESPOND 401 "{\"error\":\"missing or unknown X-Api-Key\"}" (3 header(s))
shutdown: ok
```

The refusals are asserted **before** the accepted call on purpose: `declare!`
stashes the host table and instance in `OnceLock`s, so a successful `init`
first would make the refusal unfalsifiable.

Plus `cargo test -p ephpm-middleware-example` — 11 unit tests covering config
validation, all three verdicts, per-tenant budget isolation, revocation and the
untenanted (single-site) scope, and 2 integration tests keeping the guide's
quoted snippet in lockstep with this crate's source.

### Not verified here

* **Linux and macOS.** The module is platform-agnostic Rust with no
  conditional compilation, and the loader path is the same `libloading` call,
  but this transcript is Windows only — treat `.so`/`.dylib` as **unverified**
  until someone runs it. (`cargo xtask e2e` cannot complete on Windows —
  issue #367 — so the in-tree E2E suite was not the harness used here; the
  server above was driven directly.)
* **Clustered KV.** The rate limiter is cluster-wide *by construction* (one
  `kv_incr_ttl` against a replicated store), but this run was a single node.
  The cross-node behaviour is **unverified**.

---

## The honest caveats

* **A fully static binary cannot `dlopen`.** If you build a statically linked
  ePHPm (musl, no dynamic loader), this lane does not exist — not because of
  anything ePHPm does, but because there is no runtime linker to call. That is
  precisely why the **builtin lane** exists: a module compiled into the binary
  and dispatched through the static registry, same `Middleware` trait, same
  code, no FFI. The stock Linux release is glibc-dynamic, so `dlopen` works
  there; the four in-tree modules ship on the builtin lane so they work
  everywhere.

* **The chain only sees PHP-dispatched requests in fpm mode.** Static files
  and router-generated 403/404s do not pass through middleware. Worker mode
  routes everything through PHP, so there the chain sees everything.

* **The request body is not available in v1.** `request_body` returns length
  0. The chain deliberately runs *before* the body is read — rejecting before
  paying for the transfer is the point — so body inspection would defeat it.
  Reserved for a future buffered-body option.

* **Loading a module runs its initialisers with the server's privileges.**
  This is the documented v1 trust model: a middleware module is trusted code,
  the same as a PHP extension. There is no sandbox.

* **Failure posture is a choice, and this module makes both.**
  Authentication is fail-closed (unknown key rejected; a panic in `invoke` is
  converted by `declare!` to a 500). Rate limiting is fail-open (KV
  unavailable ⇒ allow, with a warning) — dropping all traffic because the
  counter tier hiccuped turns a soft protection into a hard outage.

## See also

* [`crates/ephpm-middleware/src/abi.rs`](../../crates/ephpm-middleware/src/abi.rs) — the ABI, with the design rationale in the module docs
* [`crates/ephpm-middleware/src/lib.rs`](../../crates/ephpm-middleware/src/lib.rs) — the `Middleware` trait, `Response` builder, `Host` services and the `declare!` macro
* [`crates/ephpm-middleware-builtins/src/`](../../crates/ephpm-middleware-builtins/src) — the four shipped modules, written against the same trait
* [`crates/ephpm-server/tests/middleware_dlopen.rs`](../../crates/ephpm-server/tests/middleware_dlopen.rs) — loader coverage: symbol resolution, version handshake, failure modes
