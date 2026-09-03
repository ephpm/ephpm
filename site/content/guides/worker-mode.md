+++
title = "Worker Mode (Write Your Own Worker)"
weight = 7
+++

Worker mode (`[php] mode = "worker"`) is ePHPm's persistent-execution model:
instead of bootstrapping your PHP application on every request (the per-request model),
a **worker script** boots it once per worker thread and then handles requests
in a loop. Cold-boot cost is paid once per worker, not once per request.

If you run Laravel, WordPress, or a PSR-15 framework, use the shipped
adapters — they implement everything on this page for you:

- [Laravel Octane (Worker Mode)](/guides/laravel-octane/)
- [WordPress Worker Mode](/guides/wordpress-worker/)
- [PSR-15 Apps (Worker Mode)](/guides/psr15-worker/)

This guide is for writing a worker **by hand** against the engine primitives —
for a custom app, a microservice, or your own framework adapter. The repo
ships runnable references at `examples/worker/worker.php` (minimal loop) and
`examples/worker/worker-stream.php` (streaming).

## The minimal worker

```php
<?php
// worker.php

// ── Boot-once section ─────────────────────────────────────────────
// Everything above the loop runs exactly ONCE per worker thread.
// Build your kernel/container/config here.
$booted_at = microtime(true);
$served    = 0;

// ── Request loop ──────────────────────────────────────────────────
while (($envelope = \Ephpm\Worker\take_request()) !== null) {
    $served++;
    $vars = $envelope->serverVars();

    \Ephpm\Worker\send_response(
        200,
        ['Content-Type' => 'text/plain'],
        "hello {$vars['REQUEST_URI']} (request #$served)\n",
    );
}
// take_request() returned null: graceful drain or max_requests
// recycle. Fall off the end; ePHPm respawns a fresh worker if needed.
```

Config:

```toml
[server]
listen        = "0.0.0.0:8080"
document_root = "/path/to/app"    # the worker script must resolve under this

[php]
mode   = "worker"

[php.worker]
script = "worker.php"             # relative to document_root
```

Static files are still served by ePHPm directly; every PHP-bound request is
dispatched to the worker pool.

## The contract

Three rules, all enforced by the engine:

1. **Exactly-once**: every request returned by `take_request()` must be
   answered by exactly one `send_response()` *or* `send_response_stream()`
   call before the next `take_request()`.
2. **`null` means stop**: when `take_request()` returns null (graceful drain,
   or the `max_requests` recycle threshold), end your loop and return.
3. **The process persists**: globals, statics, and singletons survive between
   requests. That is the point — and the foot-gun. Never stash per-request
   state (the current user, the current request) anywhere that outlives the
   iteration.

### What the `Envelope` gives you

| Method | Returns | Notes |
|---|---|---|
| `serverVars()` | `array` | `$_SERVER`-shaped variables. |
| `headers()` | `array` | Request headers. Duplicates arrive pre-joined with `", "` (`"; "` for `Cookie`). |
| `query()` / `cookies()` | `array` | Split on delimiters only — **not url-decoded**. Re-parse with `parse_str($vars['QUERY_STRING'], $q)` / `urldecode()` if you need decoded values or `a[]=` arrays. |
| `rawBody()` | `string` | The request body. For streamed bodies this drains the incremental reader into a string (re-buffers — prefer `bodyStream()` for large uploads). |
| `bodyStream()` | `resource` | A real readable stream over the incremental body — a multi-GB upload flows through in fixed-size reads with flat memory. The body is consumed **once**, shared between `rawBody()`, `bodyStream()`, and PHP's own POST reader: pick one. A stream resource stashed across requests reads EOF (it can never see the next request's body). |
| `parsedBody()` | `?array` | **Always `null`** — form/multipart parsing is your job, or enable `[php.worker] populate_superglobals` and read `$_POST`/`$_FILES` natively. |
| `files()` | `array` | **Always empty** — same deal. |

### Sending responses

```php
\Ephpm\Worker\send_response(int $status, array $headers, string $body);
\Ephpm\Worker\send_response_stream(int $status, array $headers, $resource);
```

- Header values may be **list arrays** — `'Set-Cookie' => [$c1, $c2]` emits
  one wire header per element. This is the only correct way to send repeated
  headers; never comma-join `Set-Cookie` (its `expires=` attribute contains
  commas).
- `send_response_stream()` pumps the resource to the client in 64 KiB chunks
  with backpressure — use it for large downloads so worker memory stays flat.
  A client that stops reading for longer than `[server.timeouts] idle`
  aborts the stream and frees the worker.
- Anything you `echo` while handling a request is captured and prepended to
  the body you pass to `send_response()` (or flushed as the first chunk of a
  streamed response).

### `exit()` / `die()`

Calling `exit()` mid-request does not lose the response: the engine
synthesizes it from the SAPI status, any `header()`/`setcookie()` calls, and
the captured output (including content still inside `ob_start()` buffers) —
then **recycles the worker** (a fresh boot). It works, but you pay a full
re-boot per request, so prefer `send_response()` in a loop. This is how the
WordPress adapter tolerates `wp_die()`.

## Superglobals

By default `$_GET`/`$_POST`/`$_SERVER` are **not** populated — adapters build
their own request objects from the `Envelope`. Set
`populate_superglobals = true` under `[php.worker]` for code that assumes
native superglobals (this also makes PHP's own POST reader parse forms and
multipart into `$_POST`/`$_FILES`, spooling file parts to disk).

## Tuning knobs

Pool sizing lives under `[php]`, worker-lifecycle knobs under `[php.worker]` —
see the [config reference](/reference/config/) for authoritative details:

| Knob | Default | What it does |
|---|---|---|
| `[php] concurrency` | `0` (cgroup-quota- or CPU-derived) | Persistent worker threads. `0` derives from the cgroup CPU quota when present (Linux — the sweet spot inside CPU-limited containers), else host parallelism — clamped 2–32 on both paths. |
| `[php.worker] max_requests` | `10000` | Recycle a worker after N requests (fresh boot reclaims slow memory growth). Pure leak guard — for a leak-free framework loop, prefer `0`. |
| `[php] queue_depth` | `0` (= `concurrency`) | Dispatch-queue depth; a full queue applies HTTP backpressure. |
| `[php.worker] boot_timeout` | `30` | Boots slower than this are logged as errors and counted (`ephpm_worker_boot_timeouts_total`). |
| `[php.worker] stream_threshold` | `1 MiB` | Request bodies at/above this (or chunked bodies) stream into the worker instead of buffering. |

## Failure behavior

- **A fatal/uncaught error** mid-request → the client gets a 500, the worker
  recycles with a fresh boot. The loop itself should still `try/catch` around
  per-request handling so ordinary exceptions don't cost you a re-boot.
- **A worker script that fails to boot** (exits before its first
  `take_request()`) is respawned with exponential backoff and counted in
  `ephpm_worker_boot_failures_total`. Its fatals appear in the engine log as
  `[PHP]` lines — worker mode defaults `log_errors=On` (overridable) so boot
  failures are never silent.
- **Shebang lines are fine**: `#!/usr/bin/env php` at the top of a worker
  script (including Composer `vendor/bin` proxies) is skipped by the engine.
- **Runaway recursion** is a normal error, not a crash. Every thread that runs
  PHP gets an 8 MiB stack — the same size a stock `php-fpm` worker gets from
  `ulimit -s` — and PHP's own C-stack guard raises a catchable
  `Error: Maximum call stack size of 8339456 bytes ... reached` before the
  thread runs out. Catch it in your loop and the worker keeps serving without
  even a recycle; let it escape and it is an ordinary fatal (500 + recycle).
  This matters for template/block renderers, which spend a C frame per nesting
  level whenever the recursion passes through an internal function
  (`array_map`, `preg_replace_callback`, `call_user_func_array`).

  ```php
  try {
      $body = $app->handle($envelope);
  } catch (\Throwable $e) {          // includes the stack-limit Error
      $body = '500';
  }
  ```

- **Known limitation.** One fault class is still fatal to the *process*:
  exhausting the stack inside PHP's **destructor cascade** (freeing a very large
  plain-object graph), which recurses purely in C and passes no checkpoint PHP
  can test. `[php] crash_containment` contains that, but it applies in
  per-request mode only and is therefore **not available in worker mode**. See
  [Diagnosing crashes](/guides/diagnosing-crashes/).

## Observability

Worker metrics on `/metrics` (see the [metrics reference](/reference/metrics/)):
pool size, busy/idle, boot duration/failures/timeouts, recycles by reason
(`max_requests` | `script_exit` | `fatal` | `hung`), dispatch-queue depth and
wait time.
