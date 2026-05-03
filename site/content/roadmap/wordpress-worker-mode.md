# WordPress Worker Mode

WordPress has no first-party "worker mode" story — every request reboots
`wp-load.php`, reloads every plugin, and rebuilds every internal cache. A
modest-sized site spends 50–200 ms in pure bootstrap before the first line
of theme code runs. ePHPm can amortize that across requests by booting
WordPress once per worker thread and resetting only the per-request state in
between.

This is harder than Laravel Octane or Symfony Runtime: WordPress has no
container, no service registry, no `kernel.reset` tag. State lives in a
sprawling forest of PHP globals that no framework abstraction can sweep.
Worker mode for WordPress is therefore an opinionated runtime, not a
mechanical adapter.

This is a Phase-3 item; see [PHP Worker Mode](/architecture/#php-worker-mode)
for the prerequisite. It shares the SAPI surface added for the
[Laravel Octane Driver](../laravel-octane-driver/#sapi-surface-rust--php) and the
[Symfony Runtime Adapter](../symfony-runtime-driver/#sapi-surface).

---

## Why a Native WordPress Runtime

WordPress runs roughly 40% of the public web. Its worker-mode story today is
a graveyard:

| Project | What it offers | Status |
|---|---|---|
| FrankenPHP worker mode | Documented WP bootstrap pattern, manual global reset | Active; the only credible reference. |
| Swoole + WordPress | Various community shims | Abandoned or stale; plugin breakage common. |
| RoadRunner + WordPress | Some experiments | None production-ready. |
| ngx_pagespeed / FastCGI cache | Page-level cache, not worker mode | Sidesteps the problem. |
| Object cache plugins (Redis, Memcached) | Caches DB queries, not bootstrap | Helps, doesn't eliminate the cost. |

There is essentially no competition for "WordPress worker mode that just
works." A working ePHPm runtime for WordPress is a marketing-grade
differentiator, not just a feature.

---

## Why WordPress Is Hard

Every other framework on our roadmap (Laravel, Symfony, Mezzio) provides a
service container and explicit reset semantics. WordPress provides:

- **A pile of global variables** (`$wp`, `$wp_query`, `$wpdb`, `$post`,
  `$current_user`, `$pagenow`, `$wp_locale`, `$wp_filter`, `$wp_actions`,
  `$wp_current_filter`, …) — most of these accumulate state during a
  request.
- **A hook (action/filter) system** that mutates global arrays — plugins
  register callbacks during boot and during request handling, and naive
  worker mode causes registrations to compound across requests.
- **A plugin ecosystem of ~60,000 plugins**, most of which were written
  assuming "fresh process per request" — many use module-level singletons,
  static class properties, and globals of their own.
- **A theme system** that runs arbitrary PHP at boot and during request
  handling, with the same statefulness assumptions.
- **No formal lifecycle** — there is no "request started" / "request ended"
  contract, only the conventional `init`, `wp`, `template_redirect`,
  `shutdown` action sequence that is fired during `wp()` execution and
  shutdown.

Worker mode for WordPress is therefore a question of **what subset of the
ecosystem we can safely run** and **how we surface the constraints to
operators**.

---

## Architecture

```
   ┌──────────────────────────────────────────────────────────────────────┐
   │                          ephpm process                                │
   │                                                                      │
   │   hyper ──► router ──► spawn_blocking ──► PHP worker thread          │
   │                                              │                       │
   │                                              ▼                       │
   │                              ┌──────────────────────────────┐        │
   │                              │ TSRM context (per thread)    │        │
   │                              │                              │        │
   │                              │ Phase A: BOOT (once)         │        │
   │                              │  ├─ define ABSPATH           │        │
   │                              │  ├─ require wp-config.php    │        │
   │                              │  ├─ require wp-settings.php  │        │
   │                              │  │   (loads core + plugins)  │        │
   │                              │  └─ snapshot global state    │        │
   │                              │                              │        │
   │                              │ Phase B: REQUEST LOOP        │        │
   │                              │   while (req = take()) {     │        │
   │                              │     reset_per_request_state()│        │
   │                              │     populate_superglobals()  │        │
   │                              │     do_action('init')        │        │
   │                              │     wp()                     │        │
   │                              │     template_redirect        │        │
   │                              │     send_response()          │        │
   │                              │   }                          │        │
   │                              └──────────────────────────────┘        │
   │                                              ▲                       │
   │                                              │ SAPI bindings         │
   │                              ┌───────────────┴───────────────┐       │
   │                              │ ephpm-kv (WP_Object_Cache)    │       │
   │                              │ ephpm-db (wpdb backend pool)  │       │
   │                              │ ephpm-cluster (multi-node)    │       │
   │                              └───────────────────────────────┘       │
   └──────────────────────────────────────────────────────────────────────┘
```

The runtime is split into a **boot phase** (runs once per worker thread, at
startup) and a **request phase** (runs per HTTP request). Most plugin code
runs in the boot phase. The request phase only resets and runs the parts
that should logically restart per request.

---

## State Taxonomy: What to Reset, What to Leave Alone

The hardest design question. Wrong answer in either direction: too much
reset = no benefit over FPM; too little = state leaks between requests.

### Always reset (per-request state)

| Variable / state | Why |
|---|---|
| `$wp_query` | Main WP_Query — populated from URL, must be fresh. |
| `$wp` | Request-routing object. |
| `$wp_the_query` | Initial query reference (used by `wp_reset_query()`). |
| `$post` | Current post in the loop. |
| `$pagenow` | Admin page identifier — derived from URL. |
| `$wp_current_filter` | Filter execution stack. |
| `$wp_actions` *(reset to baseline)* | Action invocation count — a snapshot at end of boot is the baseline; reset to baseline at start of each request. |
| `$_SERVER`, `$_GET`, `$_POST`, `$_COOKIE`, `$_REQUEST`, `$_FILES` | Standard superglobals; populated from the request envelope by ePHPm. |
| `$current_user` | Cleared via `wp_set_current_user(0)` to drop user cache. |
| Output buffer state | Flushed and reset. |
| `$wp_locale_switcher` | Reset to original locale if request switched. |

### Never reset (worker-lifetime state)

| State | Why |
|---|---|
| `$wp_filter` | Hook registry. Plugins registered hooks at boot; resetting wipes them. |
| `$wp_taxonomies` | Taxonomy registry. |
| `$wp_post_types` | Post-type registry. |
| `$wp_rewrite` | Rewrite rules — built once at boot. |
| `$wpdb` *(connection)* | Database connection. ephpm-db proxy means the wire connection is fake anyway, but the `$wpdb` object stays. |
| `$wp_object_cache` | Object cache instance — backed by ephpm-kv, lives forever. |
| Class definitions, function definitions, included files | PHP can't unload these. |

### Conditionally reset (the gray zone)

| State | Default | Notes |
|---|---|---|
| Plugin-registered globals | Leave alone | Most plugins assume their boot-time globals persist. Resetting breaks them. |
| Plugin static class properties | Leave alone | Same reason. Plugins that *do* mutate these per-request are buggy under worker mode and need a per-plugin shim or a denylist. |
| Transient cache entries | Leave alone | Backed by `WP_Object_Cache` → ephpm-kv. Persistence is the whole point. |
| `wp_cache_*` | Leave alone | Same — backed by object cache. |

**Rule of thumb:** reset only what WordPress core itself recomputes per
request from URL/headers/POST data. Everything else stays.

---

## Boot Phase

The boot phase runs once per worker thread, before the first request lands.

```php
// wp-ephpm-worker.php (worker entrypoint)

define('ABSPATH', __DIR__ . '/');
define('WP_USE_THEMES', true);
define('WPINC', 'wp-includes');

// Tell WordPress we are running in a long-lived worker.
// Some plugins / drop-ins check this to suppress per-request side effects
// (e.g. starting their own session per request).
define('EPHPM_WORKER_MODE', true);

require __DIR__ . '/wp-config.php';
require ABSPATH . WPINC . '/load.php';
require ABSPATH . WPINC . '/default-constants.php';
// … standard wp-settings.php load order …
require ABSPATH . 'wp-settings.php';

// Snapshot baseline state for per-request reset.
\Ephpm\WordPress\StateSnapshot::capture();

// Hand control to the request loop.
\Ephpm\WordPress\WorkerLoop::run();
```

Key constraint: `wp-settings.php` runs every plugin's top-level code. We
let it run *exactly once*, then never again. This is where the win comes
from — that file alone takes 30–80 ms on a non-trivial site.

---

## Request Phase

```php
// Inside WorkerLoop::run()

while ($request = \Ephpm\Octane\take_request()) {
    \Ephpm\WordPress\StateSnapshot::restoreBaseline();

    $_SERVER  = $request->serverVars();
    $_GET     = $request->query();
    $_POST    = $request->parsedBody();
    $_COOKIE  = $request->cookies();
    $_FILES   = $request->files();
    $_REQUEST = array_merge($_GET, $_POST, $_COOKIE);

    // Restart WP's own per-request init.
    wp_set_current_user(0);
    unset($GLOBALS['wp'], $GLOBALS['wp_query'], $GLOBALS['wp_the_query'], $GLOBALS['post']);
    $GLOBALS['wp']           = new WP();
    $GLOBALS['wp_query']     = new WP_Query();
    $GLOBALS['wp_the_query'] = $GLOBALS['wp_query'];

    ob_start();
    try {
        $GLOBALS['wp']->main();        // routing + main query
        do_action('template_redirect');
        wp_send_headers();
        if (defined('WP_USE_THEMES') && WP_USE_THEMES) {
            include get_query_template('index');
        }
    } catch (\Throwable $e) {
        // Translate to 500 response.
    }
    $body = ob_get_clean();

    \Ephpm\Octane\send_response(
        new Response(http_response_code(), headers_list(), $body)
    );

    // Reset shutdown hooks that WP would have fired on process death —
    // worker mode never dies, so we fire them per-request instead.
    do_action('shutdown');
}
```

This is the minimum-viable bootstrap. A production version handles admin
requests, AJAX endpoints, REST API endpoints, cron interception, and a
dozen other edge cases — but the loop shape is the same.

---

## ePHPm Integrations Specific to WordPress

### `WP_Object_Cache` backed by `ephpm-kv`

WordPress has had a pluggable object cache via `wp-content/object-cache.php`
since 2.5. ePHPm ships an `object-cache.php` drop-in that backs every
`wp_cache_*` call with `ephpm_kv_*`:

- **In-process speed** — `ephpm-kv` is a `DashMap` access in the same
  process, no socket hop.
- **Cluster replication for free** — gossip propagates writes across
  nodes; a multi-server WP install gets distributed object cache without
  installing Memcached or Redis.
- **Compression already supported** — `ephpm-kv` natively compresses values
  via gzip/zstd/brotli; large transient payloads (option blobs, menu trees)
  shrink automatically.

The drop-in is ~200 lines and ships with the runtime. Detection: if
`ephpm-kv` is available and `WP_USE_EPHPM_OBJECT_CACHE` is not explicitly
`false`, install the drop-in automatically on first deploy.

### `$wpdb` over `ephpm-db`

`define('DB_HOST', '127.0.0.1:3306')` in `wp-config.php` already routes
queries through ePHPm's MySQL proxy. Worker mode amplifies the win: a
single boot-phase connection authentication, then thousands of requests
share the pooled, multiplexed backend connections. No `wp_use_persistent_connection()`
hacks; no `mysql.allow_persistent` PHP ini fiddling.

### Multisite (WordPress Network)

Multisite uses `$current_blog`, `$current_site`, and `$blog_id` globals to
switch context per request. These go in the **always reset** bucket. The
boot phase loads core + network-active plugins; per-request reset switches
the active blog. ePHPm's vhost machinery already steers the request at the
hostname level — multisite simply sets the matching blog ID.

### WP-Cron

WP-Cron normally piggybacks on inbound request traffic (`spawn_cron()`
self-pings the site). In worker mode this becomes pathological — every
request can fire a cron self-ping. The runtime intercepts: `define('DISABLE_WP_CRON', true)`
in `wp-config.php`, then ePHPm registers a tokio interval that fires
WordPress cron in a dedicated worker thread (similar to Octane's tick
mechanism). Independent of request traffic, predictable timing.

---

## Plugin Compatibility

The fundamental question: how many of the top N WordPress plugins survive
worker mode?

### Compatibility tiers

| Tier | Definition | Strategy |
|---|---|---|
| Green | Works as-is. No worker-mode awareness needed. Most plugins that limit themselves to hooks. | Run unmodified. |
| Yellow | Works after a small shim. E.g., plugin uses `static::$instance` and assumes one request per process — needs a reset hook. | Ship per-plugin reset shims in the runtime; opt-in via plugin slug. |
| Red | Cannot work in worker mode. E.g., plugins that fork via `pcntl_fork`, mutate `dl()`, or assume process death triggers cleanup. | Detection at boot — log a warning, recommend running this site in non-worker mode. |

### Test corpus

Maintain a CI matrix that boots a WordPress instance with the top 100
plugins (by install count from wordpress.org/plugins) and runs a fixed set
of HTTP requests. Pass/fail per plugin determines tier assignment. Publish
the matrix as a public compatibility table. This is the same playbook
FrankenPHP uses; we steal it shamelessly.

### Notable known-hostile plugins

- **WooCommerce** — heavy use of static instances, action priorities, and
  per-request product caches. Probably yellow tier. Test coverage critical.
- **Yoast SEO** — historically aggressive global state. Yellow tier.
- **Backup plugins** (UpdraftPlus, BackWPup, …) — some `pcntl_fork`. Red
  tier; recommend running in non-worker mode for backup runs.
- **Page builders** (Elementor, Divi, Beaver Builder) — heavy autoloading
  and per-request state. Tier TBD, almost certainly yellow.

---

## Open Issues

### `wp-admin` worker mode

The admin area has its own state surface: screen options, list-table
filters, current user capabilities, nonces. Phase-1 worker mode covers the
front-end only; admin requests fall through to a non-worker code path. A
proper admin worker mode is Phase-5+ — too many dragons, too small a win
(admin is rarely the hot path).

### Persistent DB connections vs. proxy

`wpdb`'s `db_connect()` opens a connection on first query. Under ephpm-db,
that connection is to `127.0.0.1:3306` (the in-process proxy), so the
"connection" is essentially free — but `$wpdb->dbh` holds a reference. We
need to verify the proxy handles `$wpdb` reusing a closed-by-the-other-end
connection cleanly. Likely fine, but explicit testing required.

### Plugin updates without restart

A WP admin user clicks "update plugin." The plugin's PHP files change on
disk. Worker mode has the old version loaded. Strategies:

1. **Watch + restart** — file watcher triggers worker recycle. Adds
   complexity, racy with deploys.
2. **`max_requests` retire** — after N requests, recycle anyway. Plugin
   updates land within ~1 minute as workers cycle through.
3. **Manual reload** — admin UI shows a "Restart workers" button. Operator
   action.

Default to option 2 with `max_requests = 1000`; offer option 3 in the
admin UI as an opt-in for impatient operators. Avoid option 1 — file
watching across a network filesystem (NFS, EFS) is unreliable.

### `wp-config.php` constants vs. runtime config

Some `wp-config.php` defines (`WP_DEBUG`, `WP_HOME`, `WP_SITEURL`) are
read once per boot. Operators expecting "edit `wp-config.php`, refresh"
need to know they must restart workers. Document loudly.

### Theme switching

Switching the active theme is normally instant on FPM. Under worker mode,
the old theme's PHP is already loaded; new templates work because they're
loaded per-request, but `functions.php` from the old theme is still in
memory. Forced worker recycle on theme change.

---

## Phasing

### Phase 1 — Worker mode primitive (prerequisite)

Same as Octane / Symfony. Generic `take_request` loop. See
[PHP Worker Mode](/architecture/#php-worker-mode).

### Phase 2 — Front-end-only WP runtime

Boot WordPress, run a single canonical front-end test ("homepage renders
correctly"). No admin support, no AJAX, no REST API, no plugin matrix.

**Exit criteria:** stock `_s` theme on a default WP install serves the
homepage from a long-lived worker; second request reuses the same worker.

### Phase 3 — REST API + AJAX + cron

Add `/wp-json/*`, `admin-ajax.php`, and intercepted WP-Cron. These are all
front-end concerns even though they share infrastructure with admin.

**Exit criteria:** WP REST API (`/wp-json/wp/v2/posts`) and `admin-ajax.php`
endpoints both work under worker mode.

### Phase 4 — Object cache drop-in + `ephpm-kv` backend

Ship the `object-cache.php` drop-in. Per-site auto-install.

**Exit criteria:** `wp_cache_*` calls hit `ephpm-kv` and persist across
requests; `wp_options`-style transients survive worker reuse.

### Phase 5 — Plugin compatibility matrix

CI suite running top-100 plugins against worker mode. Tier assignment
published.

**Exit criteria:** ≥80 of top-100 plugins green-tier; documented shims for
the remainder.

### Phase 6 — Admin area worker mode

Worker support for `wp-admin/*` requests. Higher state surface.

**Exit criteria:** plugin install/update, theme switch, post edit all work
inside worker mode.

### Phase 7 — Multisite

Per-blog state switching, network plugin handling.

---

## Out of Scope

- **Old PHP versions.** ePHPm targets 8.4+; WordPress users on 7.4 stay on
  FPM. We do not backport.
- **Plugins using `pcntl_fork` / `pcntl_signal`.** Document as red tier;
  no shim attempted.
- **Custom drop-ins beyond `object-cache.php` and `advanced-cache.php`.**
  `db.php`, `sunrise.php`, `install.php`, `maintenance.php` keep their
  default behavior.
- **WP-CLI worker mode.** WP-CLI is one-shot by design; runs through the
  normal PHP CLI path.
- **Page caching.** That's a separate concern (handled at ephpm-server
  layer or via a plugin like W3 Total Cache). Worker mode and page cache
  are independent and complementary.

---

## References

- [FrankenPHP WordPress worker mode docs](https://frankenphp.dev/docs/worker/) — the only credible prior art
- [WordPress `wp-settings.php` source](https://developer.wordpress.org/reference/files/wp-settings.php/) — what runs at boot
- [WordPress object cache API](https://developer.wordpress.org/reference/classes/wp_object_cache/) — backing store for `wp_cache_*`
- [`WP_Query` reference](https://developer.wordpress.org/reference/classes/wp_query/) — main per-request state
- [ePHPm Laravel Octane Driver](../laravel-octane-driver/) — sister roadmap; shares the SAPI surface
- [ePHPm Symfony Runtime Adapter](../symfony-runtime-driver/) — sister roadmap; shares the SAPI surface
