+++
title = "Debugging with Xdebug"
weight = 12
+++

[Xdebug](https://xdebug.org/) step debugging works in both of ePHPm's
execution modes. In per-request mode nothing about it is ePHPm-specific. In
**worker mode** — where a worker boots your framework once and serves many
requests — ePHPm gives each request that asks for the debugger its **own
fresh worker and its own debug session**, while the warm pool keeps serving
everybody else.

> **Development only.** Never load Xdebug on a production or
> internet-facing server. It slows every request, and anyone who can send a
> trigger can make the server open a debugger connection — with
> `xdebug.discover_client_host` on, to a host of *their* choosing, where a
> debugger can run arbitrary code. ePHPm logs a warning at startup whenever
> the worker-mode debug lane is armed.

## Platform support

| Platform | Status |
|---|---|
| Linux (glibc release binaries) | Supported — verified with Xdebug 3.5.3 on PHP 8.5.7 (ZTS). |
| macOS | Untested. |
| Windows | Not possible with the current static builds. |

Xdebug must be a **ZTS** build for the PHP minor your ePHPm binary embeds
(`ephpm php -v` shows both). Distro and Sury packages are NTS and will not
load; build Xdebug from source against the matching php-sdk headers the same
way as any other shared extension — see [PHP Extensions](/guides/php-extensions/).

## Loading Xdebug

Xdebug is a *Zend* extension, so it is loaded with `zend_extension` through
`[php] ini_overrides`, not through `[php] extensions` (which writes plain
`extension=` lines). Use an **absolute path**: a relative path is resolved
against PHP's `extension_dir`, which in ePHPm is a baked-in placeholder, and
fails to load.

```toml
[php]
ini_overrides = [
  ["zend_extension", "/opt/xdebug/xdebug.so"],     # absolute path
  ["xdebug.mode", "debug"],                        # step debugging on
  ["xdebug.start_with_request", "trigger"],        # only when a trigger is sent
  ["xdebug.client_host", "127.0.0.1"],             # where your IDE listens
  ["xdebug.client_port", "9003"],
  # Optional but recommended: require a shared secret as the trigger value.
  # ["xdebug.trigger_value", "change-me"],
]

[server.timeouts]
# A request paused on a breakpoint must not be cut off.
request = 0
```

Check that the build loads:

```console
$ ephpm php -d zend_extension=/opt/xdebug/xdebug.so -r 'var_dump(phpversion("xdebug"));'
string(5) "3.5.3"
```

A relative `zend_extension=xdebug.so` fails here with `Failed loading Zend
extension 'xdebug.so' (tried: /lib/php/extensions/no-debug-zts-…/xdebug.so …)`.
At server startup, ePHPm logs a warning when the worker-mode debug lane is
armed, or an info line saying why it is not (for example `xdebug.mode` lacking
`debug`).

`[server.timeouts] request` is the server's per-request deadline. Leave it
at a non-zero value while debugging and a request you sit on longer than
that is answered `504` — the debug session itself keeps going in your IDE,
but its response is discarded. ePHPm warns about this at startup when the
worker-mode debug lane is armed.

## Starting a session

Send any trigger Xdebug understands:

- **Browser**: a helper extension (Xdebug helper for Chrome/Firefox) that
  sets the `XDEBUG_SESSION` cookie.
- **curl / scripts**: `?XDEBUG_TRIGGER=1` on the URL, or
  `-b XDEBUG_SESSION=PHPSTORM`.
- **Forms**: an `XDEBUG_TRIGGER` field in an
  `application/x-www-form-urlencoded` POST body.
- The legacy `XDEBUG_SESSION_START=<idekey>` query/form parameter.

With `xdebug.trigger_value` set, `XDEBUG_TRIGGER` / `XDEBUG_SESSION` must
carry one of its values, and `XDEBUG_SESSION_START` is refused — Xdebug's
own rules.

Your IDE needs nothing ePHPm-specific: listen on the client port (VS Code
`php-debug`, PhpStorm "Start Listening for PHP Debug Connections") and map
your project path if ePHPm runs in a container.

## Worker mode: how it works

To PHP, a persistent worker's whole life is *one* request: request startup
runs once when the worker boots, then `take_request()` hands it thousands of
HTTP requests, and request shutdown runs only when the worker retires.
Xdebug opens a debug session at request startup and closes it at request
shutdown, so on a warm worker it never sees your trigger, and a session could
only ever span the worker's whole life.

So ePHPm routes instead of resetting:

1. A request that Xdebug would start a session for is detected in the
   router — before any PHP runs — using Xdebug's own rules (below).
2. It is handed to a **fresh worker thread** whose PHP request startup is
   given *that* HTTP request. Xdebug sees the trigger exactly as it would
   under php-fpm and connects to your IDE before any PHP code runs.
3. Your worker script **boots inside the session** — breakpoints in boot
   code (container setup, config loading, service providers) hit.
4. The one request is served through the ordinary `take_request()` /
   `send_response()` loop, so the code you step through is the same code
   that serves production traffic.
5. The worker's loop ends after that request, the thread shuts its PHP
   request down, and Xdebug ends the session cleanly. The next triggered
   request gets a brand-new worker and session.

Every request without a trigger stays on the warm pool and never waits for
a debugged one — verified at under a millisecond per request while a debug
session sat paused on a breakpoint.

The cost: **one framework boot per debugged request** (what php-fpm pays on
every request), paid only by the request that asked for the debugger. And
the state is fresh: a bug that only reproduces on a warm worker's N-th
request (a leaked static, stale cached state) does not reproduce on a
debugged request — the lane cannot help with that class of bug.

### Which requests are routed

ePHPm reproduces Xdebug 3's start-of-request check against the startup
values of `xdebug.mode` (or the `XDEBUG_MODE` environment variable),
`xdebug.start_with_request` and `xdebug.trigger_value`:

- `xdebug.mode` must include `debug`, and `start_with_request` must not be
  `no` — otherwise the lane is not armed at all and startup logs why.
- `XDEBUG_TRIGGER`, else `XDEBUG_SESSION`, is looked up in the query string,
  then an urlencoded POST body, then cookies — the first place it is found
  decides, and with `trigger_value` set its trimmed value must match.
  `XDEBUG_SESSION_START` (query or POST) also counts unless `trigger_value`
  is set. `XDEBUG_IGNORE` vetoes. With `start_with_request = yes`, any
  request that names a trigger is routed (Xdebug ignores the value there).
- Names and values are decoded as PHP decodes them for `$_GET` / `$_POST` /
  `$_COOKIE`.

Not considered for routing: triggers in a `multipart/form-data` or streamed
(large, ≥ `[php.worker] stream_threshold`) body — put the trigger in the
query string or a cookie; triggers set as environment variables of the ePHPm
process (don't — they make every worker connect to the IDE at boot); and
per-request or per-vhost changes to the three settings.

Without a `trigger_value`, any client that can reach the server can force a
fresh boot by sending a trigger (bounded by the caps below). Set one if the
server is reachable by anyone but you — and keep `start_with_request =
trigger`: under `yes` Xdebug ignores trigger values (so does the routing),
and every warm pool worker also opens a debug session of its own when it
boots and keeps it for its lifetime.

The debug worker's `$_SERVER` is the triggering request's, so
`xdebug.discover_client_host` finds the client the same way it does under
php-fpm (`REMOTE_ADDR`, or the `xdebug.client_discovery_header` header) —
which is also why it must only be used on a trusted network.

ePHPm logs only the request *path* for debug-lane requests, never the query
string, since it can carry the `trigger_value` secret.

### Concurrency and timeouts

`[php.worker] debug_workers` (default `1`) caps how many debugged requests
run at once. It is separate from `[php] concurrency`, so a request paused on
a breakpoint never takes a serving slot.

- **Over the cap, a triggered request waits** for a slot instead of
  failing. With a browser helper, every PHP request from your tab — AJAX
  calls included — carries the trigger cookie, and failing them while you
  step through one would break the page you are debugging. The wait ends at
  `[server.timeouts] request` (504) or when the client gives up. At most
  `16 × debug_workers` requests may wait at once; beyond that a triggered
  request gets **503** immediately, so even with `request = 0` waiting
  requests stay bounded. Raise `debug_workers` to debug several requests in
  parallel; your IDE will show each as its own session.
- **An abandoned request frees its slot immediately.** If a debugged request
  times out or its client disconnects, the slot goes to the next triggered
  request at once. Its thread cannot be killed — it is paused inside Xdebug —
  so it keeps running until you resume or stop the session in the IDE, then
  exits on its own; its response is discarded.
- **At most `2 × debug_workers` debug threads** are alive at any time
  (serving, finishing their session, or abandoned and still paused). When
  that many exist, a triggered request waits two seconds for one to finish
  and otherwise gets **503**, with a log line telling you to resume or stop
  the abandoned sessions in your IDE.
- **Shutdown** refuses queued debug requests (503) and waits up to
  `[server.timeouts] shutdown` for debug threads to finish. A session still
  paused in an IDE then keeps its thread alive and ePHPm skips PHP's module
  teardown at exit (memory is reclaimed by the OS).

### Metrics

`ephpm_xdebug_debug_requests_total{outcome}` and `ephpm_xdebug_debug_workers`
— see the [metrics reference](/reference/metrics/#worker-mode).

### Known limitations

- `XDEBUG_SESSION_START` makes Xdebug set an `XDEBUG_SESSION` cookie when the
  session starts; in worker mode that happens before the request is handed
  to your script, and the worker clears pending headers when it picks the
  request up, so the browser never receives the cookie. Use a browser helper
  extension or `XDEBUG_TRIGGER` instead.
- Profiling and tracing triggers (`XDEBUG_PROFILE`, `XDEBUG_TRACE`, or
  `XDEBUG_TRIGGER` with `xdebug.mode=profile`/`trace` but not `debug`) are not
  routed; only step debugging is.
- Worker mode with `[server] sites_dir` is not supported (see the worker-mode
  guide), so the lane is single-site.

## Per-request mode

`[php] mode = "per_request"` (the default) runs a PHP request startup for
every HTTP request, so Xdebug's own trigger handling applies with no routing
involved, and `debug_workers` has no effect. Sessions start and breakpoints
hit as under php-fpm, and the session ends (`stopping`) as soon as the request
finishes.
