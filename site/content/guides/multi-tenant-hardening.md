---
title: "Multi-tenant hardening"
weight: 60
---

# Multi-tenant hardening — running other people's code in one process

ePHPm can host many independent tenants (virtual hosts) out of a single
process. That is the density win — N sites on one small VM — but it also means
**untrusted PHP from different tenants runs in the same process, under the same
OS user, on shared threads.** This guide is the canonical description of the
isolation model that makes that safe, every knob it involves, and — just as
importantly — the residual risks it does *not* close.

> **Supported and validated on Linux only.** The hardened multi-tenant profile
> depends on mechanisms that only exist on Linux: the eBPF per-vhost network
> policy, systemd process sandboxing, cgroup accounting, the Unix stack-overflow
> crash-containment guard (`crash_guard.c`), and per-thread execution timers
> (`ZEND_MAX_EXECUTION_TIMERS`). **Windows and macOS builds are single-tenant /
> development targets** — they run PHP fine, but the cross-tenant isolation
> guarantees below are neither implemented nor validated there. Do not host
> untrusted multi-tenant workloads on anything but Linux.

## Threat model

- **In scope (what we defend):** cross-tenant **confidentiality and integrity**
  — one tenant's PHP must not read or write another tenant's files, database,
  KV data, credentials, sockets, or memory-resident state, and must not reach
  the control plane's secrets (the GitHub App key, per-process master secret).
- **Out of scope (accepted):** cross-tenant **availability**. All tenants share
  one process; a tenant that crashes the process (e.g. exhausting the C stack)
  takes everyone down until restart. This is a shared-fate *availability*
  problem, not a confidentiality one; closing it needs per-tenant process
  isolation, which the single-process density model deliberately does not
  provide. See [Residual risks](#residual-risks-honest-limits).

The core difficulty: because it is **one process, one uid, fungible ZTS
threads**, the kernel cannot tell tenants apart on its own. Every layer below
either (a) confines what a tenant's PHP *can express*, or (b) re-introduces
per-tenant identity the kernel otherwise lacks.

## The layers (defense in depth)

Isolation is not one switch — it is a stack. Each layer closes a channel a
hostile-PHP-userland pen test proved reachable.

### 1. Filesystem — `open_basedir`

Each request runs with `open_basedir` set to its vhost's **container directory**
(the whole checkout under `sites_dir`, not just the web root) plus that site's
own private temp/session state root — never the shared system temp. A tenant
cannot read a sibling vhost's files, the config, `/etc/ephpm/secrets`, another
tenant's database file, or `/proc/<pid>/environ`. `..`, symlink, `realpath`,
`chdir`, and `glob` escapes are all blocked by the realpath check. Controlled by
`[server.security] open_basedir` (default on in multi-tenant mode).

### 2. The function denylist — `disable_functions`

The `multi_tenant_hardening` preset (default on when `sites_dir` is set) removes
a denylist of PHP functions at MINIT. Each group closes a specific cross-tenant
channel:

| Group | Functions | Channel it closes |
|---|---|---|
| Shell execution | `exec`, `passthru`, `shell_exec`, `system`, `proc_open`, `popen`, `pcntl_exec` | escape out of `open_basedir` / run arbitrary programs as the shared uid |
| Process control | `pcntl_fork`/`pcntl_signal`/`pcntl_alarm`/`pcntl_wait`/`pcntl_waitpid`/`pcntl_async_signals`/`pcntl_signal_dispatch`/`pcntl_sigprocmask`/`pcntl_sigwaitinfo`/`pcntl_sigtimedwait`, `posix_kill`, `posix_setuid`/`posix_setgid`/`posix_seteuid`/`posix_setegid` | fork-bomb + fd/secret inheritance into a child; signal/kill the shared process; change its credentials |
| Persistent raw socket | `pfsockopen` | `EG(persistent_list)` is keyed `host:port` with no tenant component and survives request end on a shared ZTS worker — one tenant could reuse (and read/write) another tenant's live, authenticated socket. This is a **persistence** leak (see §3) |
| Reachability | `fsockopen` | a *non-persistent* raw socket. Blocked by default as an egress-reachability control; **lifted** by `network_egress_externally_managed` when egress is enforced below PHP (see §4) |
| SysV IPC | `shm_attach`/`shm_get_var`/`shm_put_var`/`shm_remove`/`shm_detach`/`shm_has_var`, `sem_get`/`sem_acquire`/`sem_release`/`sem_remove`, `msg_get_queue`/`msg_send`/`msg_receive`/`msg_remove_queue`/`msg_set_queue`/`msg_stat_queue` | a global kernel IPC namespace keyed by integer; one shared uid ⇒ full cross-tenant read/write |
| Misc | `dl`, `mail` | runtime extension loading; mail relay from the shared identity |
| OPcache | `opcache_reset`, `opcache_compile_file` (always); `opcache_invalidate`/`opcache_get_status`/`opcache_get_configuration`/`opcache_is_script_cached` (when `[opcache] cluster_invalidation` is off) | `opcache_reset()` flushes **every** tenant's bytecode; `opcache_compile_file()` compiles arbitrary files into the shared cache; the introspection API leaks aggregate metadata |

It is composed as a **union** with any operator `disable_functions` you supply
in `[php] ini_overrides` — your additions are never clobbered. `open_basedir`
and `disable_functions` are the reason a tenant cannot simply shell out or
`dl()` its way around every other layer.

> The full per-function rationale (with the pen-test that justified each) lives
> in [Virtual Hosts → the hardening preset](/guides/virtual-hosts/#multi-tenant-hardening-preset).

#### Per-site capabilities — grant one tenant a hardened function (experimental)

`disable_functions` is process-global and only acts at MINIT: `zend_disable_functions`
**removes** each function from the internal function table, once, and under ZTS
that table is shared across every worker thread. So the same preset applies to
every tenant, and the only way to relax it used to be
`multi_tenant_hardening = false` — which turns it off for *all* tenants at once.

Per-site capabilities let the operator grant **one** virtual host a function the
preset otherwise disables, without weakening any other tenant. The migration
case it exists for: a legacy app (e.g. a mailing-list tool) that needs `mail()`
and `fsockopen()` for SMTP, co-located with a fully-hardened tenant.

Because the function cannot be both removed-at-MINIT and callable, a granted
function is instead **left registered and gated per request**. ePHPm takes it
out of the process-global `disable_functions`, swaps its internal handler for a
gate, and on every call checks whether *this request's site* was granted it —
calling the real builtin if so, or raising the identical "has been disabled for
security reasons" warning if not.

Grant it in the site's [operator override file](/reference/config/#site_overrides_dir)
(`<site_overrides_dir>/<site-key>.toml`, the same operator-owned file that sets
`document_root`):

```toml
# /var/lib/ephpm/site-overrides/sendy.example.toml
document_root = "."
[security]
capabilities = ["mail", "fsockopen"]
```

Rules, all fail-closed:

* **Curated allowlist only.** The grantable set is `mail`, `fsockopen`,
  `pfsockopen` — nothing else. The shell-execution family and every
  process/IPC/`dl`/OPcache function stays hard-off and can never be granted. An
  **unknown capability name fails startup**, naming the file and value.
* **A site with no grant is exactly as restricted as today.** The default is no
  capabilities; an unmatched host (no site) gets none; the gate denies unless
  the current request's site explicitly holds the capability.
* **Zero change when unused.** If no site grants anything, the generated php.ini
  and the function table are byte-identical to a build without the feature — no
  function is un-disabled, no gate is installed.
* **Grants are startup-fixed.** The set of gated functions is decided before PHP
  starts (same as `disable_functions`), so changing a grant needs a restart.
* **`pfsockopen` caveat.** A persistent socket lives in the shared per-thread
  `EG(persistent_list)` keyed `host:port` (see §3). Granting `pfsockopen` to a
  single site is safe because no other tenant can call it; granting it to **two**
  sites lets them inherit each other's live sockets on a reused worker thread.
  ePHPm warns at startup if more than one site holds it. Prefer `fsockopen`
  (non-persistent) unless a persistent socket is genuinely required.

Startup logs, at INFO, exactly which functions moved from global-disable to
per-site gating and which site holds which capability; a grant that cannot take
effect (a function this configuration does not disable anyway, or one the
operator explicitly disabled in `[php] ini_overrides`) is logged at WARN rather
than silently ignored.

**Observable residual.** Because a gated function stays *registered* (only its
handler is swapped), `function_exists('mail')` returns `true` for **every**
tenant, not just the granted one — unlike a hard-disabled function, which is
removed from the table and reports `false`. A non-granted tenant that calls it
still gets the identical "disabled for security reasons" warning and a `null`
return, so nothing becomes *callable*; only the existence probe differs. Code
that branches on `function_exists()` to pick a transport should not rely on that
difference.

> **This is cooperative-tenant isolation at the ePHPm layer, not a kernel
> boundary.** The gate is a handler check inside one shared process; a
> memory-safety break in PHP or an extension, or any way to reach the original
> handler directly, bypasses it. It does not add a sandbox. What it guarantees is
> that granting one tenant a function never makes that function callable from
> another tenant's request. As with every per-site override, the grant is only as
> trustworthy as the operator-owned overrides directory: whoever can write that
> directory chooses the grants. Capabilities are honoured in per-request mode
> (the default); in worker mode the gate has no per-request site and fails
> closed.

### 3. Persistent connections — off

The preset sets `mysqli.allow_persistent = 0` (and `pgsql.allow_persistent = 0`,
`odbc.allow_persistent = 0`) and disables `pfsockopen`.

> **A tenant cannot re-enable this.** `mysqli.allow_persistent` is a
> `PHP_INI_SYSTEM` setting — the strictest access level — written into the
> generated php.ini and applied once at MINIT. A tenant's `ini_set()` on it
> returns `false` (SYSTEM settings are not runtime-changeable), and a per-vhost
> `.user.ini` can only carry `PHP_INI_PERDIR`/`PHP_INI_USER` settings, never
> SYSTEM ones. The same is true of `disable_functions` (also SYSTEM, applied at
> MINIT — a disabled function cannot be brought back at runtime). `open_basedir`
> is `PHP_INI_ALL`, but PHP only permits *narrowing* it at runtime, never
> widening. So the security-critical hardening is locked at the top level; there
> is no per-vhost override.

The reason persistence is off is subtle and specific to the shared-thread model: a **persistent**
connection lives in `EG(persistent_list)`, which survives request end on a long-
lived ZTS worker thread. Because threads are **fungible** — the same thread
serves tenant A now and tenant B later — a persistent connection A opened can be
handed to B if it is keyed the same (same backend/creds). Non-persistent
connections (PDO, `stream_socket_client`, curl, the in-process `ephpm_db_*`
bridge) die at request end and are unaffected. The cost is that Redis
`pconnect`, mysqli `p:` hosts, and `pfsockopen` do not work — reconnect per
request instead.

> **Residual:** the preset disables persistence for every driver that exposes an
> ini for it — `mysqli`, `pgsql` (`pg_pconnect`), and `odbc`. The one thing it
> **cannot** force off is **PDO `ATTR_PERSISTENT`**: PDO has no global
> `allow_persistent` ini, so a tenant passing `PDO::ATTR_PERSISTENT => true` to a
> *shared* external backend can still open a persistent handle. ePHPm's own
> per-site database path (the `ephpm_db_*` bridge and credential-fixed
> `pdo_mysql`) is unaffected — this only matters for a tenant deliberately
> holding a persistent PDO connection to an external DB shared with other
> tenants. If that's in your threat model, keep such tenants off a shared
> backend (per-tenant DB credentials already do this for the built-in path).

### 4. Egress — network layer + the `fsockopen` knob

`curl`, `stream_socket_client`, `file_get_contents('http://…')` and
`allow_url_fopen` stay **enabled** — tenants legitimately call external services
(composer, npm, plugin updates, APIs). Egress is therefore controlled *below*
PHP, at the network/kernel layer (see §7/§8), not by blocking functions.

Because of that, blocking `fsockopen` at the PHP layer while `curl`/
`stream_socket_client` reach the same destinations is redundant and bypassable.
When you enforce egress at the network layer, set:

```toml
[server.security]
network_egress_externally_managed = true
```

and the preset stops adding `fsockopen` to the denylist. It does **not** lift
`pfsockopen` or the persistence blocks (those are a *persistence* concern, not a
reachability one — see §3), nor any process/IPC/`dl`/`mail`/OPcache block.
Default is `false` — ePHPm cannot verify an external egress control exists, so
the block stays on unless you explicitly assert otherwise.

### 5. Per-site databases

With `[db.sqlite]` in `sites_dir` mode, **each vhost gets its own Turso database
file** (`<db.sqlite.dir>/<site-key>.db`) — Turso has no per-schema ACL, so the
file *is* the tenant boundary. The `ephpm_db_*` bridge resolves each request's
own database via a per-thread session that swaps when the request's site
changes; a tenant's queries reach only its own `.db`. `ATTACH`/`DETACH`/`VACUUM`
and path-`PRAGMA`s are rejected on the tenant path, so a tenant cannot `ATTACH`
another tenant's file. Assignments to `PRAGMA journal_mode` are rejected too, so
a tenant cannot move its file onto Turso's experimental MVCC mode. On Turso 0.8.2
doing that inside a transaction destroyed the database. Stock `pdo_mysql` also works per-site: one MySQL wire
listener serves everyone, but a connection's database is fixed by the credential
it authenticates with — `DB_USER` = the site key, `DB_PASSWORD` =
`HMAC-SHA256(per-process master secret, site_key)`, injected per request.
Verification happens *before* the backend is resolved, so a wrong credential
never opens the target's file.

### 6. Per-site KV keyspace

The embedded KV store is namespaced per site key, so `ephpm_kv_*` from one tenant
cannot read or overwrite another tenant's keys. The RESP listener (if enabled)
authenticates with the same per-site derived password as `pdo_mysql`, and in
this mode `[kv.redis_compat] password` is ignored. Multi-tenant mode with the
RESP listener on and no `[kv] secret` is a startup error. See
[KV RESP Protocol → Authentication](/reference/kv-resp/#authentication).

### 7. Two-uid process model + systemd sandbox

> **Operator-supplied.** Unlike §1–§6, nothing in this section is produced by
> ePHPm. [`ephpm install`](/reference/cli/#ephpm-install) writes a single-uid systemd unit
> (`crates/ephpm/src/service/systemd.rs`): with `--user` it emits `User=`,
> `Group=`, `WorkingDirectory=` (the data dir), `NoNewPrivileges=true`,
> `ProtectSystem=strict`, `ProtectHome=true`, a `CapabilityBoundingSet` of
> `CAP_NET_BIND_SERVICE`, and `ReadWritePaths` (data, log and document-root
> dirs, `/tmp`, `/var/tmp`, and the database/ACME directories named in the
> config at install time).
> It does **not** emit `PrivateTmp`, `IPAddressDeny`, or a second unit, and
> there is no `ephpm-ctl` binary. The split below is a deployment pattern you
> build on the host — the one the ePHPm preview fleet runs — described here so
> it can be reproduced, not a default you inherit.

To run a tenant's non-PHP commands (build or seed steps) under the tenant uid
with a Landlock filesystem scope, use [`ephpm exec`](/reference/cli/exec/)
(Linux only).

Two OS users split the trust boundary:

- **`ephpm-web`** — the data plane. Runs tenant PHP out of `sites_dir`. Its
  systemd unit carries **no** GitHub credentials and its uid cannot read the
  control plane's home, the secrets dir, or `/proc/<ephpm-ctl-pid>/environ`.
- **`ephpm-ctl`** — the control plane. Owns the GitHub App key and mints
  short-lived tokens in memory. Never runs tenant PHP.

The data-plane unit runs under a systemd sandbox: `NoNewPrivileges=true`,
`ProtectSystem=strict`, `ProtectHome`, `PrivateTmp=true`, `ReadWritePaths`
scoped to exactly its data dirs, and a minimal `CapabilityBoundingSet`
(`CAP_NET_BIND_SERVICE`; plus `CAP_BPF`/`CAP_NET_ADMIN` only when the eBPF layer
is on — §9). `IPAddressDeny` blocks the cloud metadata endpoint and RFC1918/ULA
at the socket layer as a coarse floor.

### 8. nftables egress policy

> **Operator-supplied.** ePHPm ships no firewall rules and applies no egress
> policy of its own — see [the egress residual in the security
> model](/architecture/security/#what-ephpm-does-not-protect-against). Without
> the table below, tenant PHP reaches anything the host can reach.
> `[server.security] network_egress_externally_managed` is your *assertion*
> that this layer exists (it drops `fsockopen` from the denylist); it adds no
> enforcement and ePHPm cannot verify the claim.

A per-uid nftables table (scoped to `skuid ephpm-web`) is the comprehensive
egress floor that `IPAddress*` can't express: loopback sealed, DNS pinned to
fixed resolvers, cloud metadata (`169.254.0.0/16`), RFC1918/ULA/link-local all
dropped, all UDP except DNS dropped, ICMP dropped. **Public TCP egress stays
open** by design (previews need it) — see the residual on exfil below.

### 9. eBPF per-vhost network policy (Linux, opt-in)

The layers above are per-*uid* (coarse). eBPF re-introduces per-*vhost*
identity: ePHPm tags each serving thread with the request's canonical site key,
and `cgroup/bind4+6` / `cgroup/connect4+6` programs enforce, per vhost:

- **loopback authorization** — a vhost may reach a loopback service only if the
  *same* vhost bound it; cross-vhost loopback connects are denied (`EPERM`);
- **transparent sidecar port-rewrite** — every vhost can hardcode the same
  loopback port; each is remapped to a private real port, so there are no
  clashes and no way to reach another vhost's sidecar;
- **per-vhost sidecar port quota** — an in-kernel cap so one tenant cannot
  port-bomb the shared pool.

This is **experimental and Linux-only**, off by default. See
[Per-vhost network policy (eBPF)](/guides/ebpf-per-vhost-network/) for the full
deployment requirements (kernel ≥ 5.10 + BTF, `CAP_BPF`/`CAP_NET_ADMIN`,
`LimitMEMLOCK=infinity`, and the nftables loopback handoff).

## Turning it on

The layers divide into two groups, and it matters which is which:

| Layers | Who provides them | State |
|---|---|---|
| §1–§6 — `open_basedir`, the function denylist, persistence off, per-vhost temp/session, per-site database + wire credential, per-site KV keyspace | ePHPm | **On by default** whenever `[server] sites_dir` is set |
| §7–§8 — the two-uid split, the systemd sandbox extras (`PrivateTmp`, `IPAddressDeny`), the nftables egress table | **You**, on the host | Not shipped, not written by `ephpm install`, not on by default |
| §9 — eBPF per-vhost network policy | ePHPm (Linux only) | Off by default, opt in with `ebpf_policy = true`, plus host prerequisites |

A recommended production config makes the ePHPm-side intent explicit:

```toml
[server]
sites_dir = "/srv/ephpm/sites"

[server.security]
open_basedir = true               # per-vhost filesystem sandbox
disable_shell_exec = true         # shell-exec family
multi_tenant_hardening = true     # the full denylist + persistence off + OPcache lockdown
# Set true ONLY if you enforce egress at the network/kernel layer (nftables/eBPF/SG):
network_egress_externally_managed = true

[db.sqlite]                       # per-vhost database isolation
engine = "turso"
dir = "/var/lib/ephpm-web/db"
max_open_dbs = 16

# Experimental, Linux-only — per-vhost kernel network policy:
[server.tenant_network]
ebpf_policy = true
sidecar_port_range = "20000-32767"
max_sidecar_ports_per_vhost = 8
```

Enabling `ebpf_policy` additionally requires, on the host (see the eBPF guide):

```ini
# /etc/systemd/system/ephpm.service.d/ebpf.conf
[Service]
AmbientCapabilities=CAP_BPF CAP_NET_ADMIN
CapabilityBoundingSet=CAP_NET_BIND_SERVICE CAP_BPF CAP_NET_ADMIN
LimitMEMLOCK=infinity
```

and handing loopback off from the nftables floor to the eBPF `connect4` arbiter.
`ebpf_policy` is **fail-closed**: on a kernel that cannot load the programs (or
without the memlock limit / capabilities) ePHPm refuses to start rather than run
with the policy silently absent.

The whole preset is opt-out: set `multi_tenant_hardening = false` to keep
persistent connections at the cost of the cross-tenant channels it closes
(ePHPm logs a warning when you do).

## Residual risks (honest limits)

Isolation here is defense-in-depth, not a proof. What it does **not** close:

- **Shared-fate crash (availability).** A tenant that crashes the process — e.g.
  a deeply recursive object-graph free that overflows the C stack — takes all
  tenants down. On Linux, `crash_guard.c` contains stack-overflow crashes and
  retires the poisoned thread, but a whole-process crash is a shared-fate
  availability problem, not a confidentiality one. True isolation needs
  per-tenant processes.
- **Public egress / exfil.** ePHPm applies no egress policy at all, so with no
  §8 table in place *every* outbound destination is reachable from tenant PHP —
  `curl_*`, `stream_socket_client`, the `http://` stream wrappers, and PDO are
  all open, and `open_basedir` does not gate any of them. With the §8 table in
  place, metadata/LAN/loopback are blocked but the chain ends with
  `tcp accept`, so a hostile tenant can still reach a *public* address for
  off-box exfil. eBPF (§9) governs loopback/sidecar ownership, not public
  egress. Close it with a destination allow-list if your threat model requires
  it (trades off preview convenience).
- **Cross-tenant observability endpoints (enforced, with one limit).**
  `/metrics` (every tenant's normalized SQL digests, vhost names, upstream
  addresses) and `/_ephpm/requests` (every vhost's recent request paths) are
  node-wide. Tenant PHP can reach loopback, internal ports and unix sockets, so
  an IP ACL or a separate metrics address cannot keep tenants out. On a
  multi-site node both endpoints therefore answer `403` until a credential is
  configured: `[server.metrics] token` / `[server.diagnostics] token`, or an
  mTLS `[server.metrics] listen` with `client_ca`. Startup logs an ERROR while
  either is unconfigured. The limit: the token is a shared secret. A tenant
  that can read the config file, or the environment variable it came from, can
  use it. Keep it out of anything tenant PHP can read; `open_basedir` (§1) is
  what stops tenant PHP reading the config file. The KV snapshot controls
  (`/_ephpm/kv/snapshot[/load]`, node-wide like the others) follow the same
  rule, and their state-changing `POST`s need the diagnostics token on every
  node, single-site included. See
  [access to internal endpoints](/reference/config/#access-to-internal-endpoints).
- **PostgreSQL/PDO persistence** — see §3.
- **`ini_set` of resource limits.** *Upgrading to v0.13.1:* runtime raises
  above `memory_limit_max` are now refused, and an explicit
  `[php] memory_limit` is now honoured in serve mode, so a server configured
  with `memory_limit = "128M"` refuses WordPress admin's raise to `256M` — set
  `memory_limit_max = "512M"` to allow it (one WARN is logged per refused
  raise). See [Upgrading to v0.13.1](/getting-started/install/#upgrading-to-v0131).
  A tenant cannot raise `memory_limit` above
  [`[php] memory_limit_max`](/reference/config/#php), which defaults to the
  `memory_limit` the server started with: `ini_set('memory_limit', '-1')` and
  any raise above the ceiling return `false` and leave the limit unchanged
  (through v0.13.0 there was no ceiling, so a tenant could lift its limit and
  OOM the shared process). Lowering it, and raising it back up to the ceiling,
  still work. The ceiling is server-wide, like every INI setting. Size it with
  the next point in mind: `concurrency` × the hard limit of the ceiling is the
  worst case. On PHP 8.3.36/8.4.27/8.5.12+ a tenant can still deliberately
  cross PHP's
  [hard memory limit](/reference/config/#phps-hard-memory-limit), whose
  `_exit(137)` ePHPm contains to that request on Linux but which ends the whole
  process on macOS and Windows. What a tenant cannot do is move that limit
  itself: ePHPm makes `memory_hard_limit_percent` system-only, so `ini_set()` on
  it returns `false`; only the operator's `[php] ini_file` and
  `[php] ini_overrides` set it, and both are server-wide (there is no per-site
  INI setting).
- **Server endpoints are process-wide.** See the next section.

### Server endpoints are not tenant-scoped

The metrics path (`[server.metrics]`) and the `/_ephpm/*` endpoints answer for
**every** `Host`. They are matched before `trusted_hosts`, `blocked_paths`, the
per-site preview gate and all middleware, so none of the layers above can
restrict them. Two of them carry cross-tenant data:

- `/metrics` (when enabled): every tenant's normalized SQL in the
  `ephpm_query_*{digest}` label (table, column and query shape, never literal
  values), vhost names in `ephpm_opcache_invalidations_total{vhost}`, and DB
  proxy upstream addresses;
- `/_ephpm/requests` (when the request timeline is on): the last 256 request
  paths across **all** vhosts, in one buffer with no host field. It is off
  under `ephpm serve` unless you set `[server.diagnostics] request_log = true`.

Every tenant's PHP can reach loopback and every port the server listens on, so
an IP ACL or a private listener address cannot keep a tenant out; only a
credential can. On a multi-site node both endpoints therefore answer `403`
until one is configured, and startup logs an ERROR naming the setting:

1. Set `[server.metrics] token` (and `[server.diagnostics] token` if you use
   the timeline) **in the config file**, not through `EPHPM_SERVER__*__TOKEN`:
   tenant PHP can read the process environment with `getenv()`.
2. Or move both endpoints to an mTLS listener with `[server.metrics] listen`,
   `tls_cert`/`tls_key` and `client_ca`; a verified client certificate counts
   as the credential.
3. Leave `request_log` unset (off) on shared nodes unless you need it.

The probes (`/_ephpm/health`, `/ready`, `/primary`) stay open and return only a
status. See [access to internal endpoints](/reference/config/#access-to-internal-endpoints).

Full details are in [HTTP Endpoints → Exposure](/reference/http-endpoints/#exposure-read-this-first).

## Validation status

The confidentiality/integrity model has been exercised by an adversarial
hostile-tenant pen test on a live Linux host — **the fully hardened preview
host, with every layer of this guide in place**, including the operator-supplied
§7 two-uid split and §8 nftables table and with the §9 eBPF policy active. The
result below therefore describes *that* configuration, not a default
`ephpm install` on a stock host: a default install has §1–§6 and nothing else,
which leaves the egress and loopback findings open. With the full stack, an
attacker vhost could read **no** secret (control-plane key dir, sibling files,
another tenant's database file/rows, its KV keys, `/proc` environ) and could
reach **no** loopback port it did not own (every cross-vhost connect denied at
`connect4` with `EPERM`, the per-vhost quota enforced at `bind4`). The residuals
above were the only findings. **This validation is Linux-only** — the same
guarantees are not implemented or tested on Windows/macOS.
