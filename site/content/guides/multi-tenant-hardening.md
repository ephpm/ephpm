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
authenticates with the same per-site derived password as `pdo_mysql`. A tenant's
credential only ever reaches its own store; the **process-global** store (what
native middleware reads) is reachable only through the operator path — a
one-argument `AUTH` matching `[kv.redis_compat] password`, which no tenant holds
(see [Reaching the process-global store](/reference/kv-resp/#reaching-the-process-global-store-multi-tenant)).
Multi-tenant mode with the RESP listener on and no `[kv] secret` is a startup
error. See [KV RESP Protocol → Authentication](/reference/kv-resp/#authentication).

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

### 10. Environment scrubbing (finding F1)

A PHP request reads the **process environment** through `getenv()` (including
the no-argument form that dumps everything), the `$_ENV` superglobal and
`phpinfo()` — all three read libc's `environ` directly. In a multi-tenant
process that environment is a shared surface: it holds `[kv] secret` and
`[cluster] secret` (passed as `EPHPM_KV__SECRET` / `EPHPM_CLUSTER__SECRET`),
`AWS_*` credentials, and any other secret the operator exported. Left intact,
any tenant's PHP could read them — and because every tenant's database and KV
password is `HMAC(secret, site_key)`, reading `[kv] secret` lets one tenant mint
**every** other tenant's credential. That is a full cross-tenant breakout, and
it is what this layer closes.

ePHPm scrubs the environment once, on the single-threaded startup path, **before
`php_embed_init()` and before any PHP pool thread exists**, so every variable
not on the allowlist is gone before any tenant code — or `phpinfo()` — can read
it. It is on by default (`[php] scrub_environment = true`) and **cannot be
disabled under `[server] sites_dir`**: `false` there is a startup error.

```toml
[php]
# on by default; mandatory in multi-tenant mode
scrub_environment = true
# the four safe families kept by default; widen to the exact non-secret
# variables an app legitimately reads, never a secret
env_allowlist = ["PATH", "TZ", "LANG", "LC_*"]
```

What survives the scrub: the `env_allowlist` names (default `PATH`, `TZ`,
`LANG`, `LC_*`), ePHPm's own operational variables (the PHP-pool dispatch
tuning, `EPHPM_MIDDLEWARE_DIR`, `OTEL_*`, the Rust/tokio runtime vars), and any
middleware `env:NAME` reference the config consumes. Secrets ePHPm itself needs
are captured *before* the scrub — figment has already read the `EPHPM_*` config
into memory, and `AWS_*` are cached into a process-global — so the scrub never
breaks the server.

This mirrors what `ephpm exec` already does for the children it spawns
(`env_clear` + a fixed allowlist), applied here to the server's own process.
The net model is a container's: **the host environment is hidden, and each
tenant gets only what its operator declared** — which it declares per-vhost via
the [`[env]`](/guides/virtual-hosts/#env-per-site-environment-the-container-model)
table in the site's override file, injected into that tenant's `$_SERVER`
per request (never into the process `environ`).

**Residual:** a secret an operator *chooses* to pass to a `[[middleware]]` mount
via an `env:NAME` reference stays in the environment (ePHPm resolves it lazily,
after the scrub, when the module is built), so it remains readable by that
node's PHP. Prefer a `file:/abs/path` indirection for those secrets — the
`preview-gate` and `github-auth` `session_secret` both accept it — so the
literal never sits in the environment at all.

**`/proc/self/environ`:** removing a variable from the live `environ` leaves its
exec-time `NAME=VALUE` string in the kernel env region, which `/proc/self/environ`
exposes. On glibc Linux the scrub therefore also zeroes each removed variable's
**value** bytes in place (leaving `NAME=`), so after startup
`/proc/self/environ` shows an empty value for every scrubbed variable — the
secret is gone from the exec block, not just the live array. On non-glibc
platforms (musl, macOS, Windows) only the live `environ` is cleared, so there the
`open_basedir` layer (§1) that keeps PHP out of `/proc` is what closes this.

**CLI tenant scripts:** `ephpm php --site` (the lane cron jobs and
`wp` / `artisan` run through) applies the same `env_allowlist` scrub before any
tenant PHP runs — keeping only that site's own injected `DB_*` credentials — so a
tenant script cannot `getenv()` the operator's secrets any more than a request's
PHP can.

### 11. AF_UNIX connect fence (Linux ≥ 6.7, on by default)

Everything above is about IP. **Unix sockets are a separate door**, and
`open_basedir` does not gate it (multi-tenant breakout audit finding F2): one
process, one uid, so tenant PHP can `connect()` to `/var/run/docker.sock`, a
database's auth socket, D-Bus or journald through `unix://` streams,
`socket_connect()`, PDO `unix_socket=` or curl `CURLOPT_UNIX_SOCKET_PATH`.
ePHPm attaches `cgroup/connect_unix` + `cgroup/sendmsg_unix` programs that let a
tenant thread reach **only the unix sockets allowlisted for its site** — the
default allowlist is empty, so tenants reach none. It rides on the same
per-thread tag as §9 but is an independent switch.

**This is on by default.** `unix_socket_policy` defaults to `"auto"`, which
fences a multi-tenant Linux ≥ 6.7 per-request node (and is inert on
single-tenant, `ephpm dev`, non-Linux and worker nodes, so those are
unaffected). A node that needs a socket an app legitimately uses must
allowlist it — otherwise the app's unix connects fail with `EPERM`; that is the
intended "secure by default, break-then-allowlist" posture. On a kernel older
than 6.7 (or without BTF / `CAP_BPF`) `"auto"` logs a loud ERROR and runs
unfenced — it never silently claims a protection it does not have. Set
`unix_socket_policy = "required"` to turn that into a hard startup refusal, or
`unix_socket_policy = "off"` to opt out. **Requires Linux 6.7 or newer** (the
hooks do not exist before); see the
[eBPF guide](/guides/ebpf-per-vhost-network/#af_unix-connect-fence-unix_socket_policy)
for the kernel floor, the fail-closed modes, and the symlink caveat on
allowlisted paths.

**What §11 is — a reach control, not a breakout backstop.** Like `open_basedir`
(§1), the fence constrains what ordinary tenant **PHP userland** can reach, not
what code that has already escaped PHP can do. It tags the request thread and
filters by that tag from *inside the same process that holds the map fds*, so
in-process native code running in the data plane can delete its own TID from the
tag map (or otherwise untag) before it `connect()`s, and an `io_uring`
submission with `IOSQE_ASYNC` is punted to an untagged `io-wq` kernel worker that
the tag never covers. Both reach a socket the tag would deny. So §11 closes the
easy door that `unix://` streams, PDO `unix_socket=` and curl walk through — it
is **not** a sandbox boundary that contains a tenant who has already broken out
of PHP. For a true backstop you still need the per-uid mitigations (run the data
plane as its own uid, kept out of every socket's group/ACL) and, where
available, a `CONFIG_BPF_LSM` policy. (This qualification is the same one §8/§9
carry for their cross-tenant IP controls.)

### 12. TCP egress fence (IMDS/SSRF, on by default)

§8 (nftables) and §9 (eBPF sidecar policy) are operator-supplied or opt-in, so
**out of the box tenant PHP had unrestricted TCP egress** (audit finding
F-SSRF-1). The sharp edge on a cloud host: a tenant `connect()`s to
`169.254.169.254` (the instance metadata service), reads the node's instance-role
credentials, and uses them to reach the S3 KV-snapshot bucket — every tenant's
snapshot — and other internal services. This is the classic SSRF-to-IMDS
credential-theft path, and nothing in §1–§4 closes it (they are function- and
filesystem-granular, not address-granular).

`tcp_egress_policy` is the in-kernel fix and is **on by default** (`"auto"`). On
a multi-tenant Linux per-request node the `cgroup/connect4+6` programs deny a
tagged tenant's connect to **`169.254.0.0/16` (IMDS, always)**, **loopback except
ePHPm's own `pdo_mysql`/KV service ports**, and **RFC-1918** (`10/8`,
`172.16/12`, `192.168/16`). **Public egress stays allowed** — a tenant calling a
public API is normal. It rides the same per-thread tag as §9/§11 and is an
independent switch.

Relax the RFC-1918 tier wholesale with `tcp_egress_deny_rfc1918 = false`, or
permit a site a specific internal destination with
`[server.tenant_network.tcp_egress_allow]` (an allowlist hit overrides every deny
category, including IMDS — grant narrowly). Set `tcp_egress_policy = "required"`
to refuse startup if the fence cannot attach, or `"off"` to opt out.

> **It needs `CAP_BPF` + `CAP_NET_ADMIN` + `LimitMEMLOCK` and fails OPEN without
> them.** Under an unprivileged service user the fence cannot attach, so on
> `"auto"`/`"warn"` it logs a loud ERROR and serves with tenant IMDS egress still
> open (the gauge `ephpm_tenant_tcp_egress_active` reads `0` — alert on it). If
> you cannot grant the capabilities, use `"required"` to fail closed and, the
> single most important mitigation, **block `169.254.0.0/16` at the host
> firewall / require IMDSv2 with a hop limit of 1**. The same §11 "reach control,
> not a breakout backstop" qualification applies. See the
> [eBPF guide](/guides/ebpf-per-vhost-network/#tcp-egress-fence-tcp_egress_policy)
> for the capabilities, the IPv6 gap, and the fail-closed modes.

## What ePHPm self-enforces

The layers above confine what a tenant's PHP *can express*. This section is
about the privilege the engine runs with in the first place, and it draws a
sharp line between what ePHPm now enforces on itself and what remains the
operator's job (multi-tenant breakout audit finding **F3**).

The problem F3 named: nothing used to *force* ePHPm off root. `[server]
run_as_user` is a no-op when unset, and the in-process PHP engine had no
self-applied kernel sandbox — so a process started as root with no drop ran
every tenant's PHP, and anything that broke out of the engine, as **host root**,
with every health check green.

### 1. Fail closed on a root-with-no-drop posture — `require_privilege_drop`

In multi-tenant mode (`sites_dir` set), if the process is running as root
(`euid == 0`) and no `run_as_user` is configured, ePHPm **refuses to start**:

```text
refusing to serve as root without a privilege drop (multi-tenant breakout
audit finding F3). Set [server] run_as_user to an unprivileged account, or set
[server] require_privilege_drop = false to acknowledge running as root with no
drop (only safe when an external sandbox already confines the process).
```

Set [`run_as_user`](/reference/config/#server) (recommended — ePHPm then binds
privileged ports, opens its files, and drops the whole process to that
unprivileged uid before serving), or set
[`require_privilege_drop`](/reference/config/#server)` = false` to acknowledge
the posture when something outside ePHPm already confines it. A root process
with no drop always logs a WARN regardless of the flag; the flag only decides
whether it is fatal. `ephpm dev` downgrades the failure to a warning so local
iteration is never blocked. **Still a single non-root uid for the whole
process, not a per-tenant uid** — cross-tenant isolation remains §1–§6, not this.

### 2. Self-applied Landlock sandbox on the PHP engine — `self_sandbox`

After the privilege drop, ePHPm arms a **Landlock** ruleset on every thread that
executes tenant PHP (the per-request pool, the worker pool, the Xdebug debug
lane). This is the same kernel mechanism the out-of-process
[`ephpm exec`](/reference/cli/exec/) sandbox uses, now applied to the long-running
server. Each PHP thread is restricted — enforced by the kernel **even against
root** — to exactly:

| Access | Paths |
|---|---|
| read + write | the site trees (`sites_dir`, `document_root`), the per-vhost temp/session root, the embedded-database dir (`[db.sqlite] dir`), and `/dev/{null,zero,full}` |
| read only | the operator-owned per-site overrides, the public CA trust store (so a tenant's HTTPS client can verify certificates), a minimal non-secret slice of `/etc` (name resolution, the loader cache, timezone) plus the systemd-resolved / NetworkManager DNS dirs (`/run/systemd/resolve`, `/run/NetworkManager`, so resolution survives a `resolv.conf` regeneration), `/dev/{u,}random`, and a **curated set of `/proc/self` leaves** for self-inspection (`stat`, `status`, `maps`, `fd`, …). It grants **neither** the `/proc/self` directory **nor** `/proc/self/task`: the per-thread `task` subtree (`/proc/self/task/<tid>/environ`, `.../mem`) would re-expose the very secrets the curation hides, so it is excluded. `/proc/self/environ` (the service env, incl. `AWS_*`) and `/proc/self/mem` therefore stay unreachable — blocked by **both** the environment scrub above (finding F1) **and** this sandbox's exclusion of the `/proc/self` directory and `/proc/self/task`, not by a curated-leaf grant alone. |
| read + execute | `/usr`, `/lib`, `/lib64`, `/bin`, `/sbin`, `/opt` (loader + shared libraries) |

Everything else becomes unreachable, including the files a tenant breakout
would otherwise read with the process uid: **`/etc/ephpm`** (the cluster
secret), **`/etc/ssl/private`** (TLS private keys), **`/etc/shadow`**,
**`/root`**, other tenants' home directories, and any host path outside the
granted set. Landlock governs the filesystem only — it does not
restrict the network, and already-open descriptors (listeners, the loaded
config, the generated `php.ini`, TLS keys) keep working, which is why the TLS
private key is **never** granted to the sandboxed engine.

> **The read+execute grant covers `/opt`, `/usr`, and `/usr/local` — so do not
> put config or secrets there.** The loader and shared libraries live under
> `/usr`, `/lib*`, `/bin`, `/sbin`, and `/opt`, so the sandbox grants those trees
> **read**. A secret placed inside one of them — `/opt/ephpm/ephpm.toml`,
> `/usr/local/etc/ephpm.toml`, an app `.env` under `/opt` — is therefore readable
> by **every tenant's PHP thread**, defeating the sandbox for that file. Keep the
> ePHPm config, the `[kv]`/`[cluster]` secret, and any operator credential under
> **`/etc/ephpm`** (which is *not* granted and stays unreachable), never under
> `/opt` or `/usr/local/etc`. The sandbox narrows "any host file the uid can read"
> to "the granted set" — but the granted read+execute system trees are part of
> that set, which is exactly why secrets must not live in them. The same rule
> applies to the embedded database: in whole-database mode the **parent
> directory** of `[db.sqlite] path` is granted **read-write** to PHP threads (so
> Turso's deferred `-wal`/`-shm` open is allowed), so do not point `[db.sqlite]
> path` inside a secret-bearing directory such as `/etc/ephpm` — keep the
> database in its own data dir.

**This is not the cross-tenant boundary.** A PHP pool thread serves many
tenants over its lifetime and a Landlock domain can only be set once per thread
and only narrowed, so the ruleset necessarily grants the *whole* `sites_dir`,
embedded-DB dir and per-vhost state root — one tenant's PHP can reach another's
files, database and sessions *through the filesystem* under Landlock alone.
Keeping tenants out of each other's trees is the job of §1–§6
(`open_basedir` + `disable_functions` + the per-site database/wire credential),
not this sandbox. What Landlock adds is the kernel backstop against reaching
**host** secrets and escaping the ePHPm tree entirely, even if `open_basedir`
is bypassed.

Controlled by [`self_sandbox`](/reference/config/#server):

- `"auto"` (default) — **secure by default**: armed in multi-tenant mode when
  the kernel enforces Landlock (Linux ≥ 5.13, ABI 1), and **fails startup
  closed** if the kernel lacks it (the same posture `require_privilege_drop`
  takes — a node that isolates tenants must not run the shared-uid engine
  unsandboxed). There is no "warn and serve unsandboxed" tier; use `"off"` to
  accept that explicitly. Off in single-tenant mode.
- `"enforce"` — require it: startup fails closed without kernel support, and a
  PHP thread that cannot arm the sandbox refuses to serve.
- `"off"` — disabled.

Linux-only (Windows/macOS are single-tenant/development targets); ignored under
`ephpm dev`. It does **not** replace §1–§6: `open_basedir` is still the
cross-tenant boundary, and the sandbox grants the whole embedded-database dir, so
it hardens against *host* escape, not against one tenant reading another tenant's
`.db` file (that remains open_basedir + the single-uid model).

### What the operator must still do

The kernel-enforced guarantees above stop at the process boundary. These remain
your responsibility (they are §7–§9 above, restated as a checklist):

- **Run under a dedicated, unprivileged, non-root uid** — set `run_as_user` (or
  start ePHPm already as that uid). ePHPm now *refuses* the dangerous posture but
  cannot invent the account for you. For the strongest split, use the two-uid
  (`ephpm-web` / `ephpm-ctl`) model in §7.
- **Harden the service manager / container** — the systemd sandbox directives in
  §7 (`NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`,
  a minimal `CapabilityBoundingSet`, `IPAddressDeny`) or the equivalent container
  security context. Landlock is defense in depth *inside* the process, not a
  substitute for confining the process itself.
- **Enforce egress** at the network/kernel layer (§8 nftables, §9 eBPF) — Landlock
  does not restrict the network.

> **Self-enforced (this section) vs. operator-supplied (§7–§9).** `require_privilege_drop`
> and `self_sandbox` are produced and enforced by ePHPm and on by default in
> multi-tenant mode. The uid you drop *to*, the systemd/container hardening, and
> the egress policy are yours to provide — ePHPm cannot create or verify them.

## Turning it on

The layers divide into two groups, and it matters which is which:

| Layers | Who provides them | State |
|---|---|---|
| §1–§6 — `open_basedir`, the function denylist, persistence off, per-vhost temp/session, per-site database + wire credential, per-site KV keyspace | ePHPm | **On by default** whenever `[server] sites_dir` is set |
| [F3 self-enforcement](#what-ephpm-self-enforces) — refuse the root-with-no-drop posture (`require_privilege_drop`); self-apply a Landlock sandbox to the PHP engine (`self_sandbox`) | ePHPm (Linux) | **On by default** in multi-tenant mode; still needs a `run_as_user` uid to drop *to* |
| §7–§8 — the two-uid split, the systemd sandbox extras (`PrivateTmp`, `IPAddressDeny`), the nftables egress table | **You**, on the host | Not shipped, not written by `ephpm install`, not on by default |
| §9 — eBPF per-vhost network policy | ePHPm (Linux only) | Off by default, opt in with `ebpf_policy = true`, plus host prerequisites |
| §11 — AF_UNIX connect fence | ePHPm (Linux ≥ 6.7 only) | **On by default** (`unix_socket_policy = "auto"`) on a multi-tenant node; allowlist the sockets a site needs, set `"required"` to also fail closed, or `"off"` to opt out |
| §12 — TCP egress fence (IMDS/SSRF) | ePHPm (Linux only) | **On by default** (`tcp_egress_policy = "auto"`) on a multi-tenant node — denies IMDS / loopback-except-infra / RFC-1918; needs `CAP_BPF` + `CAP_NET_ADMIN` + `LimitMEMLOCK` or it **fails open** (set `"required"` to fail closed, allowlist per site, or `"off"` to opt out) |

A recommended production config makes the ePHPm-side intent explicit:

```toml
[server]
sites_dir = "/srv/ephpm/sites"
run_as_user = "ephpm-web"         # drop root → unprivileged uid before serving (F3)
# require_privilege_drop defaults to true in multi-tenant: startup fails if root
# with no run_as_user. self_sandbox defaults to "auto" (Landlock engine sandbox
# on Linux ≥ 5.13). Make the intent explicit / require the sandbox:
self_sandbox = "enforce"          # fail startup if the kernel can't enforce Landlock

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
# Linux >= 6.7 — tenant PHP may reach NO unix socket except a site's allowlist:
unix_socket_policy = "required"
# [server.tenant_network.unix_socket_allow]
# "shop.example" = ["/run/ephpm/shop/php-worker.sock"]
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
- **Host unix sockets (without §11).** On a kernel older than 6.7, or with
  `unix_socket_policy = "off"`, tenant PHP reaches every unix socket the
  service uid can — `docker.sock` is root-equivalent if the uid is in the
  `docker` group. Keep the data-plane uid out of every socket's group/ACL, and
  turn §11 on where the kernel allows. With §11 on, the residual is the
  allowlist itself: an allowlisted path in a tenant-writable directory can be
  swapped for a symlink.
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
  raise). See [Upgrading → v0.13.1](/upgrading/#v0131).
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
  INI setting). *Finding F5:* this ceiling now also applies to **tenant cron and
  `ephpm php --site`** — previously the CLI skipped it, so a tenant cron script
  could `ini_set('memory_limit', '-1')` and OOM the host. A per-site cron child
  additionally runs under a configurable OS CPU-time cap (`RLIMIT_CPU`) and, for
  a *binary* job, an `RLIMIT_AS` memory cap; see
  [`[cron] memory_max`/`cpu_max`](/reference/config/#cron) and the
  [cron guide](/guides/cron-and-scheduled-scripts/#native-distributed-cron-cron).
  The bare operator `ephpm php` (no `--site`) stays unbounded, as php-cli is.
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
