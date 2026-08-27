# Virtual Hosts

ePHPm supports multi-tenant hosting through directory-based virtual hosts. Each domain gets its own document root, its own isolated KV store, its own private temp/session directory, and — when `[db.sqlite] dir` is configured — its own database file. No per-site configuration files needed — the directory structure IS the config.

## How It Works

```toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/default"   # fallback site (optional)
sites_dir = "/var/www/sites"         # vhost directory
```

When a request comes in, ePHPm matches the `Host` header against directories in `sites_dir`:

```
Request: Host: alice-blog.com
  → Look for /var/www/sites/alice-blog.com/
  → Found? Serve from that directory (or the web root its override
           declares), with that site's own KV store,
           temp/session directory and (with [db.sqlite] dir) database
  → Not found? Fall back to server.document_root (or 404 if not configured).
               An unmatched host is NOT a tenant: it gets no per-site database
               and no per-site DB credentials.
```

### Directory Convention

```
/var/www/
  default/                        # fallback site (marketing, signup page)
    index.php
    wp-content/
  sites/
    alice-blog.com/               # site container for alice-blog.com
      index.php                   # ...which is also its docroot, because
      wp-content/                 #    WordPress's web root IS its app root
    bobs-recipes.com/             # site container for bobs-recipes.com
      index.php
      wp-content/
    cool-photos.net/              # site container for cool-photos.net
      index.php
      wp-content/
```

Adding a site: create a directory named after the domain, drop WordPress in it.
Removing a site: delete the directory. Requests to that domain hit the fallback.

### Per-site document root (frameworks with a `public/` directory)

By default a vhost directory **is** the web root, which is right for WordPress — its web root and its application root are the same directory. It is wrong for every framework that keeps code above the web root. A Laravel or Symfony checkout served this way publishes `composer.json`, `vendor/`, `config/` and `storage/logs/laravel.log` over HTTP, and a Laravel log routinely contains stack traces carrying env values and database credentials. (`.env` and `.git` happen to be covered because `[server.static_files] hidden_files` defaults to `"deny"`; nothing else is.)

Point `[server] site_overrides_dir` at a directory and drop one file per site into it:

```toml
[server]
sites_dir          = "/var/www/sites"
site_overrides_dir = "/var/lib/ephpm/site-overrides"   # NOT inside sites_dir
```

```toml
# /var/lib/ephpm/site-overrides/alice-blog.com.toml
document_root = "public"    # relative to the site container
```

```
/var/www/sites/alice-blog.com/     ← the site CONTAINER
  composer.json                    ← no longer reachable over HTTP
  vendor/                          ← no longer reachable over HTTP
  storage/logs/laravel.log         ← no longer reachable over HTTP
  public/                          ← the WEB ROOT
    index.php
```

A site with no override file is completely unaffected: the container stays its document root, exactly as before. Nothing changes for an existing deployment until you write an override.

**`open_basedir` stays the site container — this is the point.** PHP served from `public/index.php` still does `require __DIR__.'/../vendor/autoload.php'` on its first line, and that must keep working. So an override moves the **HTTP surface** only; the **PHP sandbox** remains the whole container:

| | Without an override | With `document_root = "public"` |
|---|---|---|
| Served over HTTP | the whole container | `public/` only |
| `$_SERVER['DOCUMENT_ROOT']` | the container | `<container>/public` |
| `open_basedir` | the container + its private state root | **the container** + its private state root (unchanged) |
| `require '../vendor/autoload.php'` | works | **works** |
| Per-site database, KV keyspace, sessions, temp | keyed by site key | unchanged — they follow the tenant, not its web root |

The per-vhost temp/session state root is derived from the container too, so adding an override to a live site does **not** orphan its existing sessions and uploads.

#### The override directory must not be tenant-writable

This is the whole security property. A vhost's `open_basedir` includes its own container by design, so a file placed *inside* a site container can be rewritten by that site's own PHP — a tenant would be choosing its own routing. ePHPm therefore **refuses to start** if `site_overrides_dir` is inside `sites_dir`, and never reads anything from inside a tenant's checkout to decide routing. Keep the directory owned by the operator (or the provisioning daemon), not by the deployed application.

For the same reason ePHPm does not read an application's own manifest (`ephpm.yaml` or similar). A provisioning daemon that consumes such a manifest is welcome to *derive* these override files from it; ePHPm only ever reads the derived, operator-owned artifact.

#### Naming: the filename is the canonical site key

The file must be `<site-key>.toml`, where `<site-key>` is the [canonical site key](#site-identity-the-canonical-site-key) — the same validated `[a-z0-9._-]` string that names the vhost directory, selects `<dir>/<key>.db` and derives the `pdo_mysql` credential. For `Host: alice-blog.com` served from `/var/www/sites/alice-blog.com/`, that is `alice-blog.com.toml`.

**An override under any other name is silently ignored** and the site serves its container. If you are generating these files from a provisioning system, make sure it uses the same identifier it used for the vhost directory — a daemon writing `preview-1234.toml` for a vhost named `pr-42-owner-repo` produces a site that works but ignores its override, with no error anywhere.

#### Failure modes, all of which serve the container

Every way an override can go wrong degrades to "serve the site container" — the pre-override behaviour — with a `WARN` naming the site and the reason:

| Situation | Result |
|---|---|
| No override file for this site | Container is the web root (the normal case) |
| Provisioning daemon down or lagging, override not yet written | **Container is the web root** — the site serves its whole checkout until the override lands |
| Override is malformed TOML, or half-written | Container, with a warning |
| `document_root` is absolute, contains `..`, or has a drive prefix | Container, with a warning |
| `document_root` names a missing directory, or a file | Container, with a warning |
| `document_root` is a symlink resolving outside the container | Container, with a warning |
| `document_root = "."` | Container (the explicit spelling of "no separate web root") |
| Override contains keys this ePHPm version doesn't know | Ignored; known keys still apply |

The second row is the one to recognise in production: **a site unexpectedly serving `composer.json` and `vendor/` means its override is missing**, not that the feature is broken. Startup logs every site whose root an override moved, so `grep` the boot log for `per-site document root overrides applied` to see what is actually in effect.

Overrides are re-read at most every 2 seconds, so writing, editing or removing one takes effect on a running server within that window — on sites discovered at startup as well as previews created later. No restart, no filesystem watcher.

Note that the declared path is validated as if hostile — relative only, no `..`, canonicalized and required to resolve inside the container — even though the writer is trusted. "The provisioning daemon validated it" is a claim about another program's current behaviour, not something ePHPm can enforce.

### Other per-site configuration

Beyond the document root, per-site configuration is intentionally minimal. What's discovered per site from `sites_dir` is the site container plus that site's `index_files` and `fallback`. Settings — PHP limits, timeouts, security rules — come from the global `ephpm.toml` and apply to every site. Per-site *state* (database, KV keyspace, temp and session storage) is separated automatically; it is not something you configure per site.

A richer per-site override system (a `site.toml` dropped into the site directory with `[php]` overrides) is planned for [Phase 2](#phase-2-per-site-overrides-future). Until then, if one site needs a larger `memory_limit`, raise the global value in `ephpm.toml`; if one site needs longer to run, raise the global `[php] max_execution_time` (natively enforced on Linux ZTS builds — see [Signal handling and `max_execution_time`](/architecture/http/#signal-handling-and-max_execution_time)) and, above it, the `[server.timeouts] request` hard 504 backstop.

### SQLite Database Location

Set `[db.sqlite] dir` and each virtual host gets its **own** database file at `<dir>/<site-key>.db`, opened lazily on that site's first query — the tenant-isolation boundary, since Turso has no per-schema ACL. `dir` is **required** in multi-site single-node mode; ePHPm fails closed rather than share one database between tenants. Both routes reach it: the native `ephpm_db_*` bridge (routed by the request's site) and stock `pdo_mysql` (routed by a per-site credential — see [Multi-tenant `pdo_mysql`](/guides/multi-tenant-pdo-mysql/)).

Per-site databases are **single-node only**. With `[cluster]` enabled the database is clustered and shared across tenants, and startup warns about it.

### Host Matching

| Request Host | Directory checked | Result |
|-------------|-------------------|--------|
| `alice-blog.com` | `/var/www/sites/alice-blog.com/` | Serve from site directory |
| `www.alice-blog.com` | `/var/www/sites/www.alice-blog.com/` | Serve if exists, else fallback |
| `unknown.com` | `/var/www/sites/unknown.com/` | Not found → fallback to `document_root` |
| No Host header | — | Fallback to `document_root` |

Port numbers and trailing dots are stripped before matching, and the host is lowercased. The match is exact — no wildcard or regex patterns. For `www.` handling, either create a symlink or handle the redirect in your fallback site.

### Site Identity (the canonical site key)

The key that matched above is the tenant's **whole** identity, not just its document-root lookup. Everything per-tenant is derived from that one value:

| Derived from the site key | Where it lands |
|---|---|
| Site container | `<sites_dir>/<site-key>/` |
| Document root | the container, or the subdirectory its [override](#per-site-document-root-frameworks-with-a-public-directory) declares |
| Per-site override file | `<[server] site_overrides_dir>/<site-key>.toml` |
| Database file | `<[db.sqlite] dir>/<site-key>.db` |
| Private temp + session directory | `<system temp>/ephpm-vhosts/<label>-<digest>` (from the site container) |
| `pdo_mysql` credential | `DB_USER = <site-key>`, `DB_PASSWORD` derived per site |
| KV keyspace and RESP credential | `EPHPM_REDIS_USERNAME = <site-key>` |

Two consequences worth stating explicitly:

- **Every name that reaches a site is the same tenant.** With `[server] sites_domain_suffix = ".localhost"`, `Host: blog.localhost`, `Host: blog` and `Host: BLOG.LOCALHOST:8080` all resolve to `blog` — one document root, one `blog.db`, one session directory, one set of credentials. (Until issue #290 the database key was derived separately and kept the suffix, so a tenant addressed both ways silently used two database files.)
- **An unmatched host is not a tenant.** It serves `document_root`, but it has no site key, so it gets no per-site database and no `DB_*` credentials — a client cannot create `<anything>.db` by inventing a `Host` header (issue #291). `ephpm_db_*` on the fallback docroot reports `no per-site database context for this request`. If you want the fallback site to have a database, give it a real vhost directory.

### Host Sanitization

The `Host` header is used to pick a directory under `sites_dir`, so it is validated before it is ever joined onto that path. A host is only accepted as a vhost key if — after port/trailing-dot stripping and lowercasing — it is a non-empty series of DNS-style labels drawn from `[a-z0-9._-]` with no empty label. Anything else (a `..` segment, a `/` or `\`, a NUL, or any other non-DNS character) is rejected with **404 Not Found** before routing.

This holds independently of `[server.request] trusted_hosts` (which is empty by default), so it cannot be bypassed by leaving that list unset. It prevents a crafted header such as `Host: ../../../../../etc` from escaping `sites_dir` and serving arbitrary host files, and `Host: ../some-dir` from pointing the document root — and PHP execution — at an arbitrary directory. Well-formed but unmatched hosts are unaffected: they still fall back to `document_root` as shown above.

Single-site deployments (no `sites_dir`) never join the `Host` header onto the filesystem, so no host validation is applied there.

## Architecture

### Single Process, Shared Thread Pool

All sites share one ephpm process and tokio's `spawn_blocking` thread pool. A request to `alice-blog.com` and a request to `bobs-recipes.com` are handled by the same threads — the router sets the correct document root and database before dispatching to PHP.

```
   ┌──────────────────── ePHPm (single process) ────────────────────┐
   │                                                                │
   │   ┌──────────────────────────────┐                             │
   │   │ Router                       │ ──── no match ──────────────┼──► Fallback site
   │   │ (Host → site directory)      │                             │    /var/www/default
   │   └──────────────┬───────────────┘                             │
   │                  │                                             │
   │                  ▼                                             │
   │   ┌──────────────────────────────┐                             │
   │   │ PHP Threads (ZTS)            │                             │
   │   │ (shared spawn_blocking pool) │                             │
   │   └──────────────┬───────────────┘                             │
   │                  │                                             │
   │   ┌──────────────┴───── Shared Backend ─────────────┐          │
   │   │                                                 │          │
   │   │   litewire → Turso → global [db.sqlite].path    │          │
   │   │   (all sites, one database)                     │          │
   │   │                                                 │          │
   │   └─────────────────────────────────────────────────┘          │
   │                                                                │
   └────────────────────────────────────────────────────────────────┘
```

This is efficient — 20 sites don't need 20x the threads. Any `spawn_blocking` thread can serve any site.

### One litewire Listener, One Database Per Site

One litewire MySQL frontend serves every site; the *backend* is per site. PHP on any site connects to `127.0.0.1:3306` as usual, and the connection's database is fixed by the credential it authenticates with — the MySQL handshake carries no `Host`, so the tenant is proven by a per-site password rather than claimed by a name. A listener per site was rejected deliberately: every tenant runs in one process as one OS user, so neither a port (enumerable) nor a unix socket (identical permissions) separates them. See [Multi-tenant `pdo_mysql`](/guides/multi-tenant-pdo-mysql/).

## Resource Usage

### Memory (single-node, all sites share workers)

| Sites | Typical memory | Notes |
|-------|---------------|-------|
| 1 | ~270 MB | Baseline (4 workers) |
| 5 | ~300 MB | Small per-site overhead (KV store, file cache) |
| 10 | ~330 MB | Idle sites use near-zero extra memory |
| 20 | ~390 MB | The thread pool doesn't grow with site count |

All sites share one SQLite database and one thread pool, so memory grows only with actively cached data — not with the number of directories in `sites_dir`.

### CPU

Shared across all sites. A 2 vCPU machine handles ~20-40 total req/s across all sites combined, regardless of how many sites exist. Individual site throughput depends on how the traffic is distributed.

### Disk

| Component | Per site |
|-----------|----------|
| WordPress installation | 60-80 MB |
| SQLite database (typical blog) | 10-100 MB |
| Uploads (images, media) | Varies |

20 WordPress sites fit comfortably on a 40 GB SSD.

## Clustered Mode with Virtual Hosts

Virtual hosts work with clustered SQLite (experimental Turso CDC replication). Because every site shares the single global database, each node replicates exactly one database's CDC stream in-process regardless of how many vhosts exist — replication cost does not grow with site count, and there is no separate database-server process.

The tradeoff is granularity: the whole shared database replicates as a unit. You can't cluster one site and leave the rest single-node.

**Recommendation:** Use single-node SQLite for multi-tenant hosting. Back up with volume snapshots or Litestream. If you need more, consider:

- Enable clustering — the shared database replicates across nodes as one unit
- Move high-traffic sites to an external MySQL via the DB proxy
- Run a separate ephpm instance (with its own database) for sites with different HA needs

## Filesystem Isolation (temp & sessions)

In multi-tenant mode each virtual host also gets its own private temp and session storage, so tenants cannot read, enumerate, or overwrite each other's temp files, uploads, or PHP session files.

### How It Works

For each vhost, ephpm derives a private state root `<system-temp>/ephpm-vhosts/<label>-<digest>` from the resolved (traversal-safe) **site container** — stable per site across restarts, distinct for every site, and unchanged by a [document-root override](#per-site-document-root-frameworks-with-a-public-directory). Its `tmp/` and `sessions/` subdirectories are created once per site (`0700` on Unix), and every request routed to that vhost runs with:

| PHP directive | Value | Effect |
|---|---|---|
| `open_basedir` | `<site-container>` + `<state-root>` | the **only** temp path in the sandbox is this vhost's own state root — never the shared system temp. The container, not the web root, so PHP can `require` from above the web root |
| `sys_temp_dir` / `upload_tmp_dir` | `<state-root>/tmp` | `tempnam()` and file uploads land in the site's own temp |
| `session.save_path` | `<state-root>/sessions` | the default `files` session handler writes each site's sessions to its own directory |

Because `open_basedir` no longer contains the shared system temp dir, even an absolute-path read of another tenant's temp or `sess_*` file is denied by PHP. `session.save_path` and `upload_tmp_dir` are re-read per request, so sessions persist correctly within a site across its own requests while staying invisible to other sites. This is the fix for the shared-`/tmp` cross-tenant read/write and session-hijack issue (#276).

Single-site deployments (no `sites_dir`) are unchanged: `open_basedir` stays off and PHP keeps its default system-temp behaviour for temp files and sessions.

> Note: `open_basedir` is an in-process boundary, not a kernel/container boundary. It is the right control for cooperating tenants and defence-in-depth; to host **untrusted** per-PR code you still want a real per-preview isolation boundary (container/VM or per-uid + namespace).

## Multi-tenant hardening preset

`open_basedir` closes cross-tenant *filesystem* reads, but several other PHP userland channels can cross the tenant boundary inside one shared ZTS process. A hostile-PHP-userland pentest confirmed that, on top of the shell-exec baseline (`disable_shell_exec`), a specific denylist closes **every** cross-tenant confidentiality/integrity channel it found. That denylist is the `multi_tenant_hardening` preset, and it is **on by default in multi-tenant mode** (whenever `[server] sites_dir` is set — same defaulting as `open_basedir` / `disable_shell_exec`).

### What it disables

On top of the shell-exec family, the preset extends the generated `php.ini`'s `disable_functions` with:

| Group | Functions | Channel it closes |
|---|---|---|
| Persistent socket | `pfsockopen` | `EG(persistent_list)` is keyed `host:port` with no tenant component and survives request end on the shared ZTS worker — one tenant could reuse (and read/write) another tenant's live, authenticated persistent socket (Redis `pconnect`, mysqli `p:`). This is a **persistence** leak; it stays disabled regardless of any external egress control. |
| Raw-socket reachability | `fsockopen` | A **non-persistent** raw socket — no cross-tenant persistence risk; blocking it is purely a reachability control (stop a tenant dialing arbitrary hosts). **Lifted when [`network_egress_externally_managed`](/reference/config/#serversecurity) is set**, because egress is then enforced below PHP and `stream_socket_client`/`curl` reach the same destinations anyway. Blocked by default. |
| SysV IPC | `shm_attach`, `shm_get_var`, `shm_put_var`, `shm_remove`, `shm_detach`, `shm_has_var`, `sem_get`, `sem_acquire`, `sem_release`, `sem_remove`, `msg_get_queue`, `msg_send`, `msg_receive`, `msg_remove_queue`, `msg_set_queue`, `msg_stat_queue` | A global kernel IPC namespace keyed by integer; one shared uid ⇒ full cross-tenant read/write. |
| Process control | `pcntl_fork`, `pcntl_signal`, `pcntl_alarm`, `pcntl_wait`, `pcntl_waitpid`, `pcntl_async_signals`, `pcntl_signal_dispatch`, `pcntl_sigprocmask`, `pcntl_sigwaitinfo`, `pcntl_sigtimedwait`, `posix_kill`, `posix_setuid`, `posix_setgid`, `posix_seteuid`, `posix_setegid` | Fork-bomb + fd/secret inheritance into a child, whole-process signals, and process-credential changes. |
| OPcache flush | `opcache_reset`, `opcache_compile_file` | `opcache_reset()` flushes **every** tenant's bytecode from the shared cache; `opcache_compile_file()` compiles arbitrary files into it. |
| Misc | `dl`, `mail` | Runtime extension loading; mail relay from the shared identity. |

It also sets two INI directives:

- `mysqli.allow_persistent = 0` — closes the mysqli persistent path the same way disabling `pfsockopen` closes the raw-socket one.
- `opcache.restrict_api = <unreachable sentinel>` — refuses **all** OPcache userland API calls (including `opcache_invalidate` / `opcache_get_status`, which the `disable_functions` list also removes). **This is emitted only when `[opcache] cluster_invalidation` is off.** ePHPm's own cluster invalidator calls those two functions through the function table, and `restrict_api` keys its check on the executing script path, so it would block ePHPm too. With cluster invalidation **on**, `restrict_api` is not set and `opcache_invalidate`/`opcache_get_status` stay callable by tenants — a metadata/per-file-invalidation residual (never a full `opcache_reset`, which stays disabled). ePHPm logs this residual at startup.

### It composes with your `disable_functions` — it never clobbers it

The effective `disable_functions` is the **union** of the preset (and `disable_shell_exec`) with any `disable_functions` you supply in `[php] ini_overrides`. Add your own entries and they stay disabled *alongside* ePHPm's:

```toml
[php]
ini_overrides = [
  # Disable pcntl_fork AND your own additions; both survive.
  ["disable_functions", "phpinfo,dl_local"],
]
```

> Historically ePHPm appended its own `disable_functions` line *after* the operator's, and PHP's last-wins INI semantics silently discarded the operator's list. That is fixed: a single composed `disable_functions` line is emitted, so operator additions are always kept.

### The cost: persistent connections

Disabling `pfsockopen` and setting `mysqli.allow_persistent = 0` **turns off persistent connections** — Redis `pconnect`, mysqli `p:` hosts, and any raw persistent socket. Non-persistent connections are unaffected: `stream_socket_client`, ordinary PDO/mysqli connections, and curl all keep working, and ePHPm's own per-request KV bridge and per-site `pdo_mysql` are unchanged. (`fsockopen` is *also* a non-persistent connection — it is blocked by default only as a reachability control, and [`network_egress_externally_managed`](/reference/config/#serversecurity) lifts it when egress is enforced at the network/kernel layer.) For most WordPress/Laravel workloads the practical loss is Redis object-cache persistence (each request reconnects). To keep persistent connections at the cost of the cross-tenant channels above, opt out:

```toml
[server.security]
multi_tenant_hardening = false
```

> This is **not** a denylist against bugs real apps depend on: `unserialize`, `preg_*`, etc. are *not* disabled. The one structural residual the denylist cannot close is a whole-process crash (e.g. a deep recursive object-graph free overflowing the C stack) — that is a shared-fate availability problem, not a confidentiality one, and needs per-tenant process isolation, which the single-process model does not provide.

## Dropping root (`run_as_user`)

Every tenant's PHP runs in one process. By default that process keeps whatever uid it was started with — and if you start it as root (to bind :80/:443), **all tenants run as root**, so `open_basedir` is the only confidentiality wall with no kernel behind it. `run_as_user` removes the root-escalation blast radius: ePHPm binds privileged ports, starts the DB proxies, opens the generated `php.ini`, and creates its runtime directories **as root**, then permanently drops the whole process to an unprivileged uid/gid before it serves a single request.

```toml
[server]
run_as_user = "www-data"   # numeric uid ("1000") or a username
run_as_group = "www-data"  # optional; defaults to the user's primary group
```

Mechanics and limits:

- **Process-wide, not per-thread.** The drop is `setgroups` + `setgid` + `setuid`; glibc broadcasts it to every thread, so all tokio/PHP threads change together. It happens before any request runs, so no request-carrying thread can race it. ePHPm verifies the drop took (fails closed if euid is still 0) and that root cannot be regained.
- **Single non-root uid — NOT per-tenant.** This removes root escalation, but every tenant still shares this one uid. Cross-tenant isolation still rests on `open_basedir` + the hardening denylist above, not on kernel permissions. Per-tenant uids require per-tenant **processes**, which this model does not have.
- **Directory ownership.** Before dropping, ePHPm `chown`s the directories it keeps writing to afterwards — `[db.sqlite] dir` (per-site database files), the per-vhost temp/session base (`<tmpdir>/ephpm-vhosts`), and the ACME cache directory — to the target uid/gid.
- **Unix only.** On Windows, or when the process is not started as root, the setting is ignored with a startup warning (a drop can only happen from root).

## Resource limits (run ePHPm under a cgroup)

The denylist and privilege drop cover cross-tenant confidentiality/integrity. **Availability** (one tenant starving or OOM-killing the shared process) is a resource-limit problem, and because all tenants share one process it must be bounded from **outside** PHP:

- **Memory.** PHP's `memory_limit` is per-request. The aggregate ceiling is `memory_limit × concurrent PHP executions`, which can exceed the pod's memory and OOM-kill everyone. Set a cgroup `memory.max` on the pod/container and size `[php] workers` (the concurrency cap, below) and `[php] memory_limit` so their product stays under it.
- **Concurrency.** `[php] workers` caps concurrent PHP executions process-wide via a dedicated semaphore (php-fpm `max_children` semantics — requests past the cap queue, subject to the request timeout). This is a **global** cap, not per-tenant: it bounds total memory/CPU but does not stop one busy tenant from monopolizing the slots. A per-tenant concurrency cap is not yet implemented; until it is, give hostile-adjacent tenants their own ePHPm process/pod.
- **Request timeout.** `[server.timeouts] request` and `[php] max_execution_time` bound how long any one request holds a worker slot, so a slow-loop tenant cannot hold the pool indefinitely.
- **Process limits.** A cgroup `pids.max` and an `RLIMIT_NOFILE` (`nofile`) ceiling bound fork/fd exhaustion at the OS layer (the process-shared fd table is not per-tenant).

## KV Store Isolation

In multi-tenant mode, each virtual host gets its own physically separate KV store. Not key prefixing — a completely separate `DashMap`. PHP applications don't need any code changes, and RESP (Redis protocol) connections are also isolated per-site via AUTH.

### How It Works

When `sites_dir` is configured, ephpm creates a `MultiTenantStore` that manages per-site `Store` instances. Each site's store is created lazily on the first request — same pattern as the vhost directory discovery.

```php
// PHP on alice-blog.com:
ephpm_kv_set("cache:page:home", $html);
// Stored in alice-blog.com's DashMap as "cache:page:home"

// PHP on bobs-recipes.com:
ephpm_kv_get("cache:page:home");
// Looks in bobs-recipes.com's DashMap → not found (physically separate)
```

Keys are stored exactly as PHP sends them — no prefixes, no munging. The isolation is physical, not logical. A site's store is a completely separate data structure.

### Per-Site Memory Limits

Each site store has its own memory limit and eviction policy. One site filling its cache doesn't evict another site's data:

```toml
[kv]
memory_limit = "64MB"   # per-site limit (each site gets up to this much)
```

### Single-Site Mode

When `sites_dir` is not configured, all KV operations go to the global store. No `MultiTenantStore` is created. Zero overhead.

### RESP Protocol (Redis-Compatible) with AUTH

The RESP protocol listener supports per-site isolation via the Redis `AUTH` command. Multi-tenant auth requires a `[kv] secret` — and in multi-tenant mode it is **mandatory**: enabling `[kv.redis_compat]` with `sites_dir` set but no `[kv] secret` is a **hard startup error** (fail closed). Without the secret, per-site AUTH scoping can't be derived and the listener would serve one shared global store to every tenant, so ePHPm refuses to start rather than expose it. Set the secret, or leave `[kv.redis_compat] enabled = false` (the default) and use the per-vhost `ephpm_kv_*` PHP functions.

```toml
[kv]
secret = "generate-a-long-random-string"   # enables multi-tenant HMAC auth

[kv.redis_compat]
enabled = true
listen = "127.0.0.1:6379"
```

A RESP connection authenticates with **two** arguments — the site's hostname plus a password derived from the secret: `HMAC-SHA256(kv.secret, hostname)`. ePHPm injects that derived password into each site's PHP environment as `EPHPM_REDIS_PASSWORD`, so PHP Redis clients can authenticate without hardcoded credentials:

```
redis-cli -p 6379
AUTH alice-blog.com <derived_password>
SET cache:page:home "<html>..."
GET cache:page:home   → "<html>..."
```

When `[kv] secret` is set, unauthenticated connections are rejected with `NOAUTH` — they do **not** fall back to the default store. The single-argument form `AUTH <password>` is only the legacy plain-password mode (no site isolation); it does not select a site store.

### Architecture

```
PHP (alice-blog.com)                PHP (bobs-recipes.com)
  │                                   │
  ├─ ephpm_kv_set("key", "val")       ├─ ephpm_kv_set("key", "val")
  │                                   │
  ▼                                   ▼
SAPI bridge                         SAPI bridge
  ├─ site store = alice's DashMap     ├─ site store = bob's DashMap
  ├─ store.set("key", "val")          ├─ store.set("key", "val")
  │                                   │
  ▼                                   ▼
MultiTenantStore
  ├─ "alice-blog.com" → DashMap { "key" → "val" }
  ├─ "bobs-recipes.com" → DashMap { "key" → "val" }
  └─ default → DashMap (global, single-site fallback)
```

### RESP connection flow

```
RESP client connects → AUTH alice-blog.com <derived_password>
  → verify password == HMAC-SHA256(kv.secret, "alice-blog.com")
  → MultiTenantStore.auth_site("alice-blog.com")
  → returns alice's Store
  → all subsequent commands operate on alice's DashMap only
(no AUTH while [kv] secret is set → NOAUTH error)
```

## Fallback Site as Marketing Funnel

The fallback `document_root` serves requests for any domain not matched by `sites_dir`. This is useful for hosting businesses:

```
/var/www/default/
  index.php    → "Start your blog today! Sign up at hosting.example.com"
```

When a customer cancels and their site directory is removed, traffic from existing backlinks, bookmarks, and search engine rankings flows to your marketing page instead of a dead 404. Free inbound traffic to your signup funnel.

### Lifecycle

```
1. Customer signs up for alice-blog.com
   → Create /var/www/sites/alice-blog.com/
   → Install WordPress
   → Site is live within ~2 seconds (no restart — the router re-checks
     unknown hosts after a short negative-lookup TTL)

2. Customer is active
   → Requests to alice-blog.com served from site directory
   → SQLite database grows organically

3. Customer cancels
   → Archive /var/www/sites/alice-blog.com/ (backup the .db file)
   → Delete directory
   → Traffic to alice-blog.com hits fallback marketing page

4. Domain expires / new customer
   → Create directory again for new owner
   → Fresh WordPress install
```

## Configuration Reference

```toml
[server]
listen = "0.0.0.0:8080"

# Fallback document root for unmatched Host headers.
# Omit to return 404 for unknown domains.
document_root = "/var/www/default"

# Virtual host directory. Each subdirectory is named after a domain.
# Omit to disable vhosting (single-site mode).
sites_dir = "/var/www/sites"

# Global PHP settings (shared by all sites)
[php]
workers = 4
memory_limit = "128M"

# Per-site databases: one <site-key>.db per virtual host (required in
# multi-site mode — ePHPm will not share one database between tenants).
[db.sqlite]
dir = "/var/lib/ephpm/dbs"
max_open_dbs = 256

# One MySQL listener for every tenant; the database is fixed by the
# per-site credential injected into that site's $_SERVER.
[db.sqlite.proxy]
mysql_listen = "127.0.0.1:3306"
```

Environment variable overrides:

```bash
EPHPM_SERVER__SITES_DIR=/var/www/sites
EPHPM_SERVER__DOCUMENT_ROOT=/var/www/default
```

## Deployment Example: 20-Blog Hosting on Hetzner CAX11

**VM:** Hetzner CAX11 — 2 ARM vCPUs, 4 GB RAM, 40 GB SSD — $3.69/mo

```toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/marketing"
sites_dir = "/var/www/sites"

[php]
workers = 4
memory_limit = "64M"

[kv]
memory_limit = "64MB"
```

Put a reverse proxy (Caddy recommended — automatic HTTPS per domain) in front for TLS termination, or use ePHPm's built-in ACME with every hostname listed explicitly in `[server.tls] domains`. **Wildcard certificates are not possible** — ePHPm's ACME implementation uses TLS-ALPN-01 only, and wildcard issuance requires DNS-01, which is not implemented.

**Capacity:**
- 20 WordPress blogs
- ~390 MB memory used
- ~20-40 req/s total throughput
- $0.18/mo per site
- Zero ops: one binary, one config file, automated backups via Hetzner snapshots

## Implementation Phases

### Phase 1: Directory-Based Routing (implemented)

Host header → site directory mapping with per-site document roots, plus per-site KV store isolation (`MultiTenantStore`). All sites share the PHP thread pool. (Per-site databases followed in a later release — see below.)

| Step | Change | File |
|------|--------|------|
| 1 | Add `sites_dir: Option<PathBuf>` to `ServerConfig` | `ephpm-config/src/lib.rs` |
| 2 | Add `SiteConfig` struct and site registry `HashMap<String, SiteConfig>` to `Router` | `ephpm-server/src/router.rs` |
| 3 | Scan `sites_dir` at startup, populate registry from directory names | `ephpm-server/src/router.rs` |
| 4 | Add `resolve_site()` — extract Host header, strip port/trailing dot, lowercase, lookup in registry | `ephpm-server/src/router.rs` |
| 5 | Thread per-site `document_root` through `resolve_fallback()`, `probe_path()`, `handle_php()` | `ephpm-server/src/router.rs` |
| 6 | Unit tests: site resolution, fallback, port stripping, case insensitivity | `ephpm-server/src/router.rs` |

When `sites_dir` is not configured, the router behaves identically to today (single-site mode). Zero cost path — the `sites` HashMap is empty and `resolve_site()` returns the global defaults.

### Phase 1b: Per-Site Databases (implemented)

| Feature | Description |
|---------|-------------|
| Per-site SQLite | Each site gets its own database file at `<[db.sqlite] dir>/<site-key>.db`, opened lazily and bounded by an LRU (`max_open_dbs`). Single-node only |
| Per-site `pdo_mysql` | One MySQL listener, per-site credentials (`DB_USER` / `DB_PASSWORD` injected per request); the connection's database is fixed by the credential it authenticates with |
| Per-site temp + sessions | Each vhost gets a private state root; `open_basedir` contains only that vhost's own directories |

### Phase 2: Per-Site Overrides (future)

| Feature | Description |
|---------|-------------|
| Per-site `site.toml` | Optional overrides for `index_files`, `fallback`, `php.memory_limit`, etc. Merged with global config |
| Per-site metrics | Add `host` label to Prometheus metrics for per-site traffic visibility |

### Phase 3: Operational Features (future)

| Feature | Description |
|---------|-------------|
| Hot reload | Detect new/removed site directories without restart (via `notify` or periodic rescan) |
| Per-site resource limits | Memory and CPU quotas per site to prevent noisy neighbors |
| Site provisioning API | REST API to create/delete sites, manage domains |
