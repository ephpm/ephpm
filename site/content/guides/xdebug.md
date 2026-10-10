+++
title = "Debugging with Xdebug"
weight = 12
+++

[Xdebug](https://xdebug.org/) step debugging works with ePHPm's embedded PHP,
in both execution modes. The fastest path is one flag on the dev server:

```bash
cd my-app/public
ephpm dev --xdebug
```

then start your IDE listening for Xdebug on port 9003 and load any page with
`?XDEBUG_TRIGGER=1`.

In per-request mode (the default, and what `ephpm dev` runs) nothing about
Xdebug is ePHPm-specific. In **worker mode** — where a worker boots your
framework once and serves many requests — ePHPm gives each request that asks
for the debugger its **own fresh worker and its own debug session**, while
the warm pool keeps serving everybody else; see
[Worker mode](#worker-mode-how-it-works).

> **Development only.** Never load Xdebug on a production or
> internet-facing server. It slows every request, and anyone who can send a
> trigger can make the server open a debugger connection — with
> `xdebug.discover_client_host` on, to a host of *their* choosing, where a
> debugger can run arbitrary code. `ephpm dev` binds `127.0.0.1` by default
> for this reason, and `ephpm serve` logs a warning at startup whenever the
> worker-mode debug lane is armed.

## Platform support

| Platform | `ephpm dev --xdebug` | Notes |
|---|---|---|
| Linux, glibc, x86_64 / aarch64 | Supported | Xdebug fetched from the ePHPm extension catalog (see below), or `--xdebug-path`. Verified with Xdebug 3.5.3 on PHP 8.5.7 and 8.5.11 (ZTS). |
| macOS, arm64 | Supported (untested live) | `--xdebug` downloads `ephpm-ext-<php>-macos-aarch64.tar.gz` from the catalog (published for PHP 8.3, 8.4 and 8.5), or use `--xdebug-path`. Needs an ePHPm macOS binary newer than v0.11.3: earlier ones were linked without `-export_dynamic`, so `-dead_strip` removed PHP functions that ePHPm never calls but Xdebug imports, and loading fails with `symbol not found in flat namespace`. |
| Windows x64, **shared-engine build** | Supported (built from source) | A `cargo xtask release --target windows --variant shared` build links the official `php8ts.dll` and loads `php_xdebug.dll` from the catalog's Windows entry (the official xdebug.org ZTS DLL for the PHP minor). See [Windows](#windows). |
| Windows x64, default build | Not available | The default Windows binary links PHP statically (`php8embed.lib`), and a static PHP cannot load extension DLLs; `--xdebug` refuses with that reason. Use the shared-engine build, or ePHPm under WSL. |

Xdebug must be a **ZTS** build for the PHP minor your ePHPm binary embeds
(`ephpm php -v` shows both). Distro and Sury packages are NTS and will not
load; build Xdebug from source against the matching php-sdk headers the same
way as any other shared extension — see [PHP Extensions](/guides/php-extensions/).

## `ephpm dev --xdebug`

```console
$ ephpm dev --xdebug
  xdebug: downloading ephpm-ext-8.5.7-linux-x86_64-gnu.tar.gz (catalog release ext-8.5.7)

  ePHPm <version> — dev server
    serving:  /home/me/my-app/public
    url:      http://127.0.0.1:8080
    php:      8.5.7
    press ctrl+c to stop

  Xdebug 3.5.3 loaded (dev only: never load Xdebug in production)
    library:  /home/me/.cache/ephpm/ext/20250925-zts-x86_64/xdebug-3.5.3.so (catalog ext-8.5.7)
    ide:      DBGp to 127.0.0.1:9003 (start the IDE listening first)
    trigger:  http://127.0.0.1:8080/?XDEBUG_TRIGGER=1
              or cookie XDEBUG_SESSION=1, or a browser "Xdebug helper" extension
    timeouts: request deadline and max_execution_time lifted while debugging
    vscode:   .vscode/launch.json → { "name": "Listen for Xdebug", "type": "php", "request": "launch", "port": 9003 }
              (no pathMappings needed when the IDE and ephpm share the filesystem)
```

What the flag does:

1. **Finds an ABI-matched Xdebug.** The binary knows its PHP version,
   `ZEND_MODULE_API_NO`, thread-safety mode, and CPU. It downloads the
   [extension catalog](/guides/php-extensions/#the-extension-catalog)
   tarball for its own PHP release (`ext-<php version>` on
   `github.com/ephpm/php-sdk`). If that release is missing or does not ship
   Xdebug, it falls back to the newest release of the **same PHP minor whose
   manifest declares the same API number** — patch releases of one minor
   share the extension ABI — and says so.
2. **Verifies before loading.** The manifest's `php_api_no`, `zts`, `os`,
   `libc` and `arch` must match the binary, and every file the manifest
   lists must be in the tarball and hash to its manifest sha256. Any
   mismatch is a hard refusal with the reason — never a silent fallback to
   another release. The hashes catch a corrupted or inconsistent download;
   they are not a signature (the manifest ships in the same tarball). What
   makes the download trustworthy is HTTPS to GitHub: the catalog URLs must
   be `https://` (plain `http://` is accepted only for a loopback mirror).
3. **Caches it** at `<user cache dir>/ephpm/ext/<api>-zts-<arch>/`
   (`$XDG_CACHE_HOME` or `~/.cache` on Linux, `%LOCALAPPDATA%` on Windows)
   with a small `xdebug.json`
   record. Later runs re-verify the cached file's sha256 and use it with no
   network access at all, so `--xdebug` works offline after the first run.
   A cached build is kept even after a newer catalog release appears (any
   release with the same API number is ABI-compatible); delete the directory
   to force a fresh download. On Linux and macOS a cache directory that is
   not owned by you or is world-writable is never used.
4. **Loads it as a Zend extension with an absolute path** and sets
   `xdebug.mode=debug,develop`, `xdebug.start_with_request=trigger`,
   `xdebug.client_host`, and `xdebug.client_port`.
5. **Lifts the timeouts** for the session: `[server.timeouts] request = 0`
   and, where PHP enforces it natively (Linux), `max_execution_time = 0`, so
   a request paused on a breakpoint is not killed.
6. **Checks that it worked.** After PHP starts, ePHPm confirms Xdebug
   registered as a Zend extension and refuses to start otherwise; PHP's own
   startup message (for example "Failed loading …") names the reason.

Flags:

| Flag | Default | Purpose |
|---|---|---|
| `--xdebug` | off | Enable, using the catalog build (cached after the first download). |
| `--xdebug-path <FILE>` | — | Use a local `xdebug.so` instead; implies `--xdebug`, no network. |
| `--xdebug-client-host <HOST>` | `127.0.0.1` | Where the IDE listens (see [WSL](#wsl)). |
| `--xdebug-client-port <PORT>` | `9003` | The IDE's port. |

`EPHPM_EXT_CATALOG_URL` (download base, laid out as `<base>/<tag>/<asset>`)
and `EPHPM_EXT_CATALOG_API` (a GitHub-releases-shaped listing) point the
download at a mirror; both must be `https://` URLs unless they point at a
loopback address. The release listing uses the unauthenticated GitHub
API (60 requests/hour); a rate-limited response is reported as such.

> **Catalog status:** the Linux (x86_64, aarch64) and macOS arm64 catalogs
> ship Xdebug 3.5.3 for PHP 8.3, 8.4 and 8.5 — in the current patch
> releases and in the ones the current binaries are built against
> (`ext-8.3.33`, `ext-8.4.23`, `ext-8.5.7`) — and 3.6.0alpha1 for the 8.6
> beta on Linux. Where no release for your PHP minor has a tarball for your
> platform, `--xdebug` stops with "the ePHPm extension catalog does not
> publish <platform> builds for PHP 8.x yet" — use `--xdebug-path` with your
> own ZTS build then.

`ephpm dev` runs PHP in the default per-request mode, where Xdebug's own
trigger handling applies unchanged.

## Starting a session

Send any trigger Xdebug understands:

- **Browser**: a helper extension (Xdebug helper for Chrome/Firefox) that
  sets the `XDEBUG_SESSION` cookie.
- **curl / scripts**: `?XDEBUG_TRIGGER=1` on the URL, or
  `-b XDEBUG_SESSION=1`.
- **Forms**: an `XDEBUG_TRIGGER` field in an
  `application/x-www-form-urlencoded` POST body.
- The legacy `XDEBUG_SESSION_START=<idekey>` query/form parameter.

With `xdebug.trigger_value` set, `XDEBUG_TRIGGER` / `XDEBUG_SESSION` must
carry one of its values, and `XDEBUG_SESSION_START` is refused — Xdebug's
own rules.

Requests without a trigger run normally and never contact the IDE.

## VS Code

Install the **PHP Debug** extension (`xdebug.php-debug`) and add
`.vscode/launch.json`:

```json
{
  "version": "0.2.0",
  "configurations": [
    {
      "name": "Listen for Xdebug",
      "type": "php",
      "request": "launch",
      "port": 9003
    }
  ]
}
```

Start "Listen for Xdebug", set a breakpoint, and load a page with the
trigger. No `pathMappings` are needed when VS Code and ePHPm see the same
filesystem paths (same machine, or VS Code Remote - WSL / Remote - SSH /
Dev Containers attached to where ePHPm runs). Add them only when the IDE sees
the code under a different path than PHP does (for example ePHPm in a
container with the project bind-mounted elsewhere):

```json
"pathMappings": { "/app": "${workspaceFolder}" }
```

PhpStorm: *Run → Start Listening for PHP Debug Connections* (port 9003).

## Windows

The default Windows `ephpm.exe` is one static file and cannot load any PHP
extension DLL. For step debugging on Windows itself, build the
**shared-engine** variant:

```powershell
cargo xtask release --target windows --variant shared
```

It links the official windows.php.net ZTS engine (`php8ts.dll`) from the
php-sdk `php-sdk-<ver>-windows-x86_64-shared.tar.gz` asset and copies the
runtime next to `ephpm.exe`: `php8ts.dll`, the DLLs it and the extensions
need (OpenSSL, ICU, …), and `ext\`. Ship that whole directory — the exe does
not start without `php8ts.dll`. At startup ePHPm loads, from that `ext\`
directory, the extensions the static build compiles in but the official
engine ships as DLLs (mbstring, openssl, curl, intl, `pdo_mysql`, …), so
the extension set matches the default build.

`ephpm dev --xdebug` then downloads `ephpm-ext-<php>-windows-x86_64.tar.gz`
from the catalog (manifest `libc` = `msvc`), verifies it the same way, and
caches `xdebug-<version>.dll` under `%LOCALAPPDATA%\ephpm\ext\`. With the IDE
on the same Windows machine the defaults (`127.0.0.1:9003`, no
`pathMappings`) work.

Status: the shared-engine build is opt-in and built from source; release
artifacts ship the default static build only.

## WSL

- **IDE inside WSL** (VS Code Remote - WSL, the recommended setup): the
  defaults work — Xdebug connects to `127.0.0.1:9003` inside the distro and
  the paths match.
- **IDE on Windows, ePHPm in WSL** (default NAT networking): Xdebug must
  connect *out* of WSL to the Windows host. Pass the host's address —
  typically the default gateway seen from WSL — and allow inbound TCP 9003
  in the Windows firewall:

  ```bash
  ephpm dev --xdebug --xdebug-client-host "$(ip route show default | awk '{print $3}')"
  ```

  With WSL **mirrored networking** (`networkingMode=mirrored` in
  `.wslconfig`), `127.0.0.1` reaches Windows and the default works. In either
  case the Windows IDE sees the files as `\\wsl.localhost\<distro>\…`, so add
  `pathMappings` from the Linux path to that one.

## Using Xdebug with `ephpm serve`

`ephpm dev` is configuration-free; for `ephpm serve` (or a dev setup you
want to keep in a config file) load Xdebug yourself. Get a ZTS build — the
cached catalog file from `ephpm dev --xdebug`, the catalog tarball, or your
own `phpize` build — and list it in `[php] extensions`. ePHPm detects that it
is a Zend extension and emits `zend_extension=` with an absolute path:

```toml
[php]
extensions = ["/opt/xdebug/xdebug.so"]   # loaded via zend_extension=
max_execution_time = 0                   # don't time out a paused request
ini_overrides = [
  ["xdebug.mode", "debug,develop"],
  ["xdebug.start_with_request", "trigger"],  # only when a trigger is sent
  ["xdebug.client_host", "127.0.0.1"],       # where your IDE listens
  ["xdebug.client_port", "9003"],
  # Optional: require a shared secret as the trigger value.
  # ["xdebug.trigger_value", "change-me"],
]

[server.timeouts]
# A request paused on a breakpoint must not be cut off.
request = 0
```

Loading it through `ini_overrides` works too —
`["zend_extension", "/opt/xdebug/xdebug.so"]` — but the path **must be
absolute** there: PHP resolves a relative `zend_extension` against
`extension_dir` (a baked-in placeholder in ePHPm), not the current directory.

Check that a build loads:

```console
$ ephpm php -d zend_extension=/opt/xdebug/xdebug.so -v
PHP 8.5.7 (cli) (built: …) (ZTS)
Copyright (c) The PHP Group
Zend Engine v4.5.7, Copyright (c) Zend Technologies
    with Xdebug v3.5.3, Copyright (c) 2002-2026, by Derick Rethans
    with Zend OPcache v8.5.7, Copyright (c), by Zend Technologies
```

A relative `zend_extension=xdebug.so` fails with `Failed loading Zend
extension 'xdebug.so' (tried: /lib/php/extensions/no-debug-zts-…/xdebug.so …)`.
At server startup in worker mode, ePHPm logs a warning when the worker-mode
debug lane is armed, or an info line saying why it is not (for example
`xdebug.mode` lacking `debug`).

`[server.timeouts] request` is the server's per-request deadline. Leave it
at a non-zero value while debugging and a request you sit on longer than
that is answered `504` — the debug session itself keeps going in your IDE,
but its response is discarded. ePHPm warns about this at startup when the
worker-mode debug lane is armed.

## Worker mode: how it works

`ephpm dev` does not run worker mode; this applies to `ephpm serve` with
`[php] mode = "worker"` and Xdebug loaded as in the previous section.

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
