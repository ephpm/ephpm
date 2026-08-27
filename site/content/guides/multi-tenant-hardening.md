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
  provide. See [Residual risks](#residual-risks).

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
another tenant's file. Stock `pdo_mysql` also works per-site: one MySQL wire
listener serves everyone, but a connection's database is fixed by the credential
it authenticates with — `DB_USER` = the site key, `DB_PASSWORD` =
`HMAC-SHA256(per-process master secret, site_key)`, injected per request.
Verification happens *before* the backend is resolved, so a wrong credential
never opens the target's file.

### 6. Per-site KV keyspace

The embedded KV store is namespaced per site key, so `ephpm_kv_*` from one tenant
cannot read or overwrite another tenant's keys. The RESP listener (if enabled)
authenticates with the same per-site derived password as `pdo_mysql`.

### 7. Two-uid process model + systemd sandbox

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

In multi-tenant mode (`[server] sites_dir` set) the confidentiality/integrity
layers (§1–§8, minus eBPF) are **on by default**. A recommended production
config makes the intent explicit:

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
- **Public egress / exfil.** The nftables floor blocks metadata/LAN/loopback but
  ends with `tcp accept`, so a hostile tenant can still `curl`/
  `stream_socket_client` to a *public* address for off-box exfil. eBPF governs
  loopback/sidecar ownership, not public egress. Close it with a destination
  allow-list if your threat model requires it (trades off preview convenience).
- **PostgreSQL/PDO persistence** — see §3.
- **`ini_set` of resource limits.** A tenant can `ini_set('memory_limit', -1)`
  and OOM the shared process (another availability, not confidentiality, gap).

## Validation status

The confidentiality/integrity model has been exercised by an adversarial
hostile-tenant pen test on a live Linux host with the eBPF policy active: an
attacker vhost could read **no** secret (control-plane key dir, sibling files,
another tenant's database file/rows, its KV keys, `/proc` environ) and could
reach **no** loopback port it did not own (every cross-vhost connect denied at
`connect4` with `EPERM`, the per-vhost quota enforced at `bind4`). The residuals
above were the only findings. **This validation is Linux-only** — the same
guarantees are not implemented or tested on Windows/macOS.
