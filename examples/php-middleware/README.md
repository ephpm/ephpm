# PHP middleware example — a bearer-token gate (EXPERIMENTAL)

A worked example of the **PHP middleware lane**: `[[middleware]]` with
`library = "php:<path>"`. Middleware written in plain PHP — **no compiler, no
`.so`, no C**. This is the shim-free counterpart to the compiled
[Rust middleware example](../rust-middleware): where that one builds a cdylib
and loads it via `dlopen`, this one is just a `.php` file you drop next to your
app.

## What it demonstrates

`demo/public/middleware.php` is a bearer-token gate that exercises every
verdict the lane understands, each spelled in stock PHP:

| Verdict | How it looks in PHP | In this example |
|---|---|---|
| **RESPOND** | set a status, `echo`, `exit;` | 401 when the `Authorization: Bearer …` header is missing or malformed — the app never runs |
| **REWRITE** | assign to `$_SERVER` | injects the verified token as `$_SERVER['HTTP_X_TOKEN']` |
| **response header** | `header()` | adds `X-Auth-Checked: 1` to the client response |
| **CONTINUE** | fall off the end / `return;` | a valid token hands control to the app |

The mount's `config = { realm = "admin" }` is handed to the script verbatim as
JSON through `ephpm_middleware_config()`.

## Where it runs — and why that matters

A `php:` mount runs **inside the same PHP request as the application script**,
immediately before it (`auto_prepend_file`'s position). It therefore inherits
the request's superglobals, `open_basedir`, session/temp roots, OPcache vhost,
per-site database session and KV keyspace **by construction** — there is no
second PHP context to configure. That is the whole trade: this lane cannot
reject *before the request body is read* (the [compiled lane](../rust-middleware)
is for that), but it needs no build step.

**Fail-closed.** A missing file, parse error, uncaught `Throwable` or fatal in
the middleware answers **500** and the application script does not run.

## Run it

No build step — just point ephpm at the config:

```bash
cd demo
ephpm serve --config ephpm.toml
```

Then, from another shell:

```bash
# RESPOND — no token → 401, the app never runs
curl -i http://127.0.0.1:8099/api/v1/whoami

# CONTINUE + REWRITE + response header — valid token reaches the app,
# which echoes the token the middleware injected; note X-Auth-Checked: 1
curl -i -H 'Authorization: Bearer s3cr3t' http://127.0.0.1:8099/api/v1/whoami

# A path outside `match = "/api/*"` skips the lane entirely
curl -i http://127.0.0.1:8099/
```

Expected: the first request is `401` with a `WWW-Authenticate` header and never
touches `index.php`; the second is `200` with `X-Auth-Checked: 1` and a body
echoing `"token": "s3cr3t"`.

## Files

| File | Purpose |
|---|---|
| `demo/ephpm.toml` | Server + the `library = "php:middleware.php"` mount |
| `demo/public/middleware.php` | The gate — all four verdicts |
| `demo/public/index.php` | Front controller the request falls through to |

## See also

- The middleware guide's **"The PHP lane (experimental)"** section:
  `site/content/guides/native-middleware.md`
- The compiled C-ABI lane, for rejecting before the body is read:
  [`examples/rust-middleware`](../rust-middleware)
