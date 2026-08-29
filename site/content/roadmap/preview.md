# Preview Deployments

> **Status: PARTLY SHIPPED.** Every pull request gets a live preview URL with
> its own database, deployed in seconds and torn down on merge.
>
> - **The ePHPm runtime half has shipped.** Per-site databases
>   ([`[db.sqlite] dir`](/reference/config/#dbsqlite)), per-site document roots
>   ([`[server] site_overrides_dir`](/reference/config/#server)), lazy vhost
>   discovery, per-site KV keyspaces and the
>   [`[server] preview`](/reference/config/#server) limits preset are all in the
>   binary. See [Virtual Hosts](/guides/virtual-hosts/) for how they behave.
> - **The bot exists and is public.** [`ephpm/switchboard`](https://github.com/ephpm/switchboard)
>   is a separate Rust project (not part of this workspace) that receives
>   GitHub webhooks, clones, builds, and writes into ePHPm's `sites_dir`. It is
>   young — several contract pieces below are open issues on that repo.
> - **What is still design:** wildcard-certificate issuance from inside ePHPm,
>   multi-PHP-version routing, teardown/GC policy, and scale-out past one VM.
>
> This page is the plan and the open questions. It is not a reference for the
> shipped knobs — those live in [Configuration](/reference/config/) and
> [Virtual Hosts](/guides/virtual-hosts/) — and it is not the app-developer
> guide, which lives in switchboard alongside the schema it documents.

## The two halves

| Component | Repo | Language | Role |
|-----------|------|----------|------|
| **ephpm** | `ephpm/ephpm` | Rust | Serves every preview as a vhost. Owns TLS, routing, per-site DB/KV/sessions. |
| **switchboard** (daemon) | [`ephpm/switchboard`](https://github.com/ephpm/switchboard) | Rust | Clones the PR, runs `build:`, materializes env, atomically swaps the checkout into `sites_dir`, runs `seed:`, health-gates. |
| **switchboard-api** | `ephpm/switchboard-api` | PHP | **In progress.** The webhook receiver and GitHub App surface, split out of the daemon so the HTTP/GitHub side is a PHP app running *on* ePHPm. |

The daemon is being reduced to `deployer.rs` / `manifest.rs` / `secrets.rs` —
the deployment mechanics — with the webhook, signature verification and PR
commenting moving to the PHP API. Dogfooding: the thing that ships previews
should itself be a PHP app on ePHPm.

Both halves share the filesystem. Switchboard writes a directory under
`sites_dir`; ePHPm discovers it on the next request. No IPC, no reload signal,
no coordination protocol — the filesystem is the interface. Removal is
symmetric: delete the directory and the host falls back to
`[server] document_root`.

## Architecture as settled

One ePHPm process, PHP 8.5, serving every preview as a virtual host, with TLS
terminating in ePHPm. **No reverse proxy.** Multi-version support is deferred;
if it comes back it will be port-based (`:8083`/`:8084`/`:8085`), one process
per PHP version, rather than a proxy in front.

```
GitHub webhook ──► switchboard-api (PHP, on ePHPm)
                        │
                        ▼
                   switchboard daemon
                        ├─ git clone --depth 1
                        ├─ detect framework / load ephpm.yaml
                        ├─ run build:
                        ├─ materialize env:
                        ├─ atomic swap ──► /var/www/sites/<host>/
                        ├─ run seed:
                        └─ poll health:  ─┐
                                          │
Browser ──► ephpm (:443, wildcard cert) ──┘
                        ├─ Host header → site key → vhost
                        ├─ site_overrides_dir → document root
                        ├─ per-site Turso file, KV keyspace, sessions
                        └─ X-Ephpm-Preview: 1 on every response
```

### Preview host format

```
<owner>-<repo>-pr-<N>.preview.ephpm.dev
```

for example `ephpm-wordpress-sample-pr-1.preview.ephpm.dev`.

The PR identity is a **single DNS label** — this is deliberate and load-bearing.
A wildcard certificate for `*.preview.ephpm.dev` covers exactly one label; a
multi-label host like `pr-42.my-blog.preview.ephpm.dev` would need a
per-hostname certificate. Switchboard sanitizes the label and appends a short
identity hash when sanitization would collide or the label runs long
(`preview_label` in switchboard's `webhook.rs`).

That hostname is also the vhost directory name, which makes it the
[canonical site key](/guides/virtual-hosts/) — the string that selects
`<dir>/<key>.db`, the KV keyspace, the private temp/session root, the
`pdo_mysql` credential, and the name any `site_overrides_dir` file must use.
Leave [`[server] sites_domain_suffix`](/reference/config/#server) unset on a
preview host: switchboard names directories with the full FQDN, and setting a
suffix would strip it, producing a key that no longer matches the directory.

## What ePHPm already gives a preview host

Rather than re-describe shipped behaviour, the short version with links:

- **Lazy vhost discovery** — a directory that appears after startup is served
  on the next request. A missing host is negatively cached for 2 seconds, so a
  fresh deploy goes live within that window. No restart, no reload, no signal.
- **Per-site database** — `[db.sqlite] dir` gives every vhost its own Turso
  file at `<dir>/<site-key>.db`, opened lazily and bounded by an LRU
  (`max_open_dbs`, default 256). `dir` is **required** in multi-site mode;
  ePHPm fails closed rather than share one database between tenants.
  Reachable from both the native `ephpm_db_*` bridge and stock `pdo_mysql`
  via injected per-site `DB_*` credentials
  ([Multi-tenant `pdo_mysql`](/guides/multi-tenant-pdo-mysql/)).
- **Per-site document root** — `[server] site_overrides_dir` and one
  operator-owned `<site-key>.toml` per site move the HTTP surface to `public/`
  while `open_basedir` stays the whole container, so
  `require '../vendor/autoload.php'` keeps working
  ([Virtual Hosts](/guides/virtual-hosts/)).
- **Preview preset** — `[server] preview = true` resolves every unset
  `[server.limits]` knob to a preview default (`max_connections = 256`,
  `per_ip_max_connections = 32`, `per_ip_rate = 10.0`, `per_ip_burst = 50`,
  `per_site_rate = 5.0`, `per_site_burst = 20`), switches an unset
  `[php] overload_policy` from `wait` to `shed`, and stamps
  `X-Ephpm-Preview: 1` on every response so a preview instance can never be
  mistaken for production. Explicit operator values always win, and startup
  logs exactly which values the preset supplied.

Coverage for the discovery lifecycle is real, not aspirational:
`vhost_lazy_discovery_finds_new_directory` and `vhost_lazy_discovery_teardown`
(unit, `ephpm-server`'s `router.rs`), plus `unknown_host_returns_fallback`,
`lazy_discovered_site_serves_content` and `multiple_sites_isolated` (e2e,
`ephpm-e2e/tests/vhosts.rs`).

## The app contract: `ephpm.yaml`

An app repo ships an `ephpm.yaml` at its root describing how its preview is
built and served. **ePHPm does not parse it.** The schema is owned by
switchboard — `src/manifest.rs` is the authority, and its module docs are
explicit that the schema is a contract: parse to these fields, do not invent
new ones. The app-developer guide lives there too.

```yaml
version: 1                  # required; only version 1 is understood
php: "8.5"
docroot: "."                # relative to the repo root
build:                      # list, run in the checkout BEFORE the site goes live
  - "composer install --no-dev --optimize-autoloader --no-interaction"
services:
  database: "turso"         # "turso" | false
  kv: true
  websocket: true           # default: auto-detect websocket.php at the docroot
seed:                       # list, run AFTER the site serves and its DB exists
  - "wp core install --url=$PREVIEW_URL ..."
env:
  WP_ENVIRONMENT_TYPE: "staging"
  SOME_KEY: "${secret.some_key}"
health: "/"                 # polled for a 200 before the deploy reports ready
ini:
  memory_limit: "256M"      # advisory in v1 — surfaced, not enforced
```

A repo with no manifest is not rejected: switchboard synthesizes one from the
detected framework (WordPress, Laravel, Symfony, Drupal, or generic), so an
unconfigured repo still gets a useful preview.

### Why ePHPm refuses to read it

The manifest lives inside the tenant's checkout, and a vhost's `open_basedir`
includes its own container by design — so a file in there is writable by that
site's own PHP. Letting ePHPm route on it would let a tenant choose its own
routing. ePHPm therefore reads only the **operator-owned** derived artifact in
`site_overrides_dir`, and refuses to start if that directory is inside
`sites_dir`. Untrusted YAML never reaches the server: nothing in the workspace
parses YAML at all.

Switchboard is welcome to *derive* an override file from a manifest it has
validated. Wiring that up is the open work below.

## Known gaps

Filed on switchboard:

- **[`switchboard#3`](https://github.com/ephpm/switchboard/issues/3) —
  `docroot:` is not honoured.** The repo root is served, so front-controller
  apps 404 and `vendor/` plus `storage/logs/` are publicly reachable. The fix
  is now available on the ePHPm side: write
  `<site_overrides_dir>/<site-key>.toml` with `document_root = "<docroot>"` at
  deploy time. The filename must be the canonical site key — an override under
  any other name is silently ignored and the site serves its whole container
  with no error anywhere.
- **[`switchboard#4`](https://github.com/ephpm/switchboard/issues/4) —
  `env:` does not reach PHP when `docroot: "."`.** Switchboard generates a PHP
  auto-prepend file and (when the docroot is not the project root) a `.env`,
  but the prepend is not actually auto-loaded. ePHPm's PHP ini is
  process-global (`[php] ini_file` / `ini_overrides`), so an
  `auto_prepend_file` set there would apply to every tenant at a fixed path. A
  per-site prepend needs either per-site ini support in ePHPm or a different
  injection point — the same Phase 2 dependency as `ini:` below.

Documented but unfiled:

- **`build:` and `seed:` have no database access.** Both run as plain `sh -c`
  children of the daemon. The per-site database lives inside the ePHPm process
  and is reachable only over that site's HTTP surface or its `pdo_mysql`
  credential, neither of which a bare shell has. `php artisan migrate` cannot
  work from a seed script as written. The current workaround, visible in
  [`ephpm/wordpress-sample`](https://github.com/ephpm/wordpress-sample)'s
  `ephpm.yaml`, is to have `seed:` drive in-docroot PHP generators **over
  HTTP** so every insert runs inside the site. That works but is a workaround,
  not a design. Candidates: hand the seed step the site's `DB_*` credentials so
  a CLI can connect over `pdo_mysql`, or give switchboard a first-class
  "run this PHP inside the site" primitive.
- **`ini:` is advisory.** The manifest field is surfaced in the sidecar and
  otherwise ignored. ePHPm's PHP settings are global, so honouring it needs
  per-site `[php]` overrides — see the Phase 2 note in
  [Virtual Hosts](/guides/virtual-hosts/).

## TLS: the wildcard certificate problem

The settled architecture wants one wildcard certificate for
`*.preview.ephpm.dev`, obtained via a DNS-01 challenge. **ePHPm cannot issue
that itself today.** Its built-in ACME (`rustls-acme`) solves **TLS-ALPN-01
only** — there is no DNS-01 solver, and without DNS-01 there is no wildcard.
Worse for previews, the ACME domain set is the fixed
[`[server.tls] domains`](/reference/config/#servertls) list read at startup, so
a hostname invented by a webhook five minutes ago cannot get a certificate on
demand.

So the shipped path is: obtain the wildcard out of band (certbot, lego, or any
DNS-01 client), and point `[server.tls] cert` / `key` at the resulting PEMs —
ePHPm's manual TLS mode. Two consequences to plan around:

- ePHPm does not watch those files, so a renewal needs a restart to take
  effect.
- Clustered ACME is not the answer either. Certificate *distribution* through
  the gossip KV is implemented, but challenge-token propagation is not, and a
  follower does not pick up a renewed certificate while running
  ([TLS & ACME](/guides/tls-acme/)).

**Planned — not yet implemented:** a DNS-01 solver behind a provider trait
(Cloudflare first), which would make the wildcard self-service and remove the
restart-on-renewal step. This is the single largest missing piece for a
self-contained preview host.

## Deployment shape

One VM runs ePHPm and switchboard. Wildcard DNS `*.preview.ephpm.dev → VM IP`;
`git` and `composer` on the VM; the GitHub App private key in switchboard's
config.

```
/var/www/
  default/                                       # fallback (marketing page)
  sites/                                         # written by switchboard, read by ephpm
    ephpm-wordpress-sample-pr-1.preview.ephpm.dev/
    ephpm-my-blog-pr-42.preview.ephpm.dev/
/var/lib/ephpm/
  site-overrides/                                # operator-owned, NOT under sites_dir
    ephpm-my-blog-pr-42.preview.ephpm.dev.toml   # document_root = "public"
  db/
    ephpm-my-blog-pr-42.preview.ephpm.dev.db     # one Turso file per preview
```

The override directory must live outside `sites_dir` — ePHPm refuses to start
otherwise, because a tenant can rewrite anything inside its own container.

### Capacity

Idle previews cost disk and nothing else: no process, no connection, no
resident PHP state. A preview only consumes memory while a request is in
flight, out of the shared pool. Disk is therefore the scaling constraint, at
roughly 70 MB per WordPress checkout — a few hundred previews on a small VM.

Deploy time is dominated by `git clone` plus `composer install`; teardown is an
`rm -rf`. Concrete per-worker memory figures deliberately are not quoted here:
`[php] workers` defaults to `0` (unlimited, bounded by the tokio blocking
pool), and worker mode's `worker_count` is derived from the cgroup CPU quota or
host parallelism, so there is no fixed worker count to multiply by. See
[Hosting & Resource Requirements](/roadmap/hosting/) for the memory model.

### Scaling past one VM — future

Multiple VMs, each running ePHPm plus a switchboard; the API routes a deploy to
the VM with the most free disk; GeoDNS or a load balancer spreads requests.
Each VM keeps its own `sites_dir`, so no shared filesystem is required.

Note the constraint this design has to respect: by default **multi-site mode
combined with clustered replication does not give per-site databases** — every
vhost shares the clustered database. A preview fleet therefore scales by
sharding previews across independent nodes, not by clustering one preview host.

`[db.sqlite.replication] per_site = true` (**experimental**) relaxes this: each
vhost keeps its own database and replicates it across the cluster, with writes
forwarded to the site's owner. It is not yet a basis for the preview fleet —
the Turso engine is Beta upstream and the forwarding path covers the
`ephpm_db_*` bridge only, not stock `pdo_mysql`.

## Multi-PHP versions — deferred

The manifest carries `php:` and switchboard records it, but nothing routes on
it: one running instance serves one PHP version. If this returns it will be
port-based — `ephpm-85` on 443, `ephpm-84` on 8084, `ephpm-83` on 8083, all
sharing one `sites_dir`, with the preview URL carrying the port for
non-default versions. That is strictly cheaper than a reverse proxy and keeps
the "ePHPm terminates TLS" property.

Two things would need solving first: certificate sharing across the instances
(the same wildcard PEM can simply be pointed at by each, once the DNS-01 story
lands), and the fact that each instance is a separate process with its own PHP
memory footprint, which is what makes this a poor default for a small VM.

## Security

| Concern | Where it stands |
|---------|-----------------|
| Webhook spoofing | HMAC-SHA256 signature verification on every webhook (switchboard). |
| Malicious code in a PR | Previews run as the ePHPm process user. All tenants share one process and one uid — the isolation boundary is the per-site database file, KV keyspace and `open_basedir`, not an OS boundary. |
| Cross-tenant data access | One Turso file per site; `ATTACH`/`DETACH`/`VACUUM`/path-`PRAGMA` rejected on the tenant path; unknown hosts get no database and no credentials. |
| Tenant self-routing | ePHPm never reads routing config from inside a tenant's checkout; `site_overrides_dir` must be outside `sites_dir` or startup fails. |
| Resource exhaustion | `[server] preview = true` caps connections and request rate per IP and per site, and sheds rather than queues under overload. |
| Secrets | Resolved by switchboard from its own store; `${secret.NAME}` references are logged by name, never by value. |

**Planned — not yet implemented:** running `build:` in a container or namespace
(today it is a plain `sh -c` child of the daemon with the daemon's privileges),
per-preview disk quotas, stale-preview GC, and per-installation deploy rate
limiting.

## Open questions

1. **Who writes the override file?** The daemon has the manifest and the site
   key; wiring `docroot:` → `<site-key>.toml` closes `switchboard#3` with no
   ePHPm change. Worth doing before anything else on this list.
2. **How does a seed step reach the database?** Handing it `DB_*` credentials
   is the smallest change; a "run this inside the site" primitive is the
   better one.
3. **DNS-01 in ePHPm, or permanently out of band?** Out of band works and is
   shipping; in-band removes a moving part and a restart.
4. **What is the teardown policy for a PR that is never closed?** Nothing
   currently reaps abandoned previews.
