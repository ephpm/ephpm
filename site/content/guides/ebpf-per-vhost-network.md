---
title: "Per-vhost network policy (eBPF)"
weight: 80
---

# Per-vhost network policy (eBPF)

**Status: experimental, Linux-only, off by default.** Enabling it requires
two out-of-band changes to the host (a firewall handoff and extra systemd
capabilities) that ePHPm cannot make for you. Read this whole page before
setting `ebpf_policy = true`.

## What it does

ePHPm is one process, one uid, serving every vhost on pooled ZTS threads — the
kernel cannot normally tell vhosts apart at `bind()`/`connect()` time. This
feature closes that gap by tagging each serving thread with the request's
canonical site key and attaching `cgroup/bind4+6` and `cgroup/connect4+6` BPF
programs to ePHPm's cgroup. From that one per-thread tag it delivers:

1. **Per-vhost loopback authorization** — a vhost may reach a loopback service
   only if the same vhost bound it. Cross-vhost loopback is denied.
2. **Transparent sidecar port-rewrite** — every vhost can hardcode the *same*
   loopback port (e.g. `127.0.0.1:8080`); `bind4` rewrites each to a private
   **real** port and `connect4` rewrites the matching connect, so each vhost
   gets the illusion of its own private `:8080` with no clashes and no app
   changes.
3. **Per-vhost sidecar port quota** — an in-kernel cap on how many real ports a
   single vhost may hold, so one tenant cannot port-bomb the shared pool.

Untagged traffic (anything that is not a per-vhost request) passes through
untouched.

Two separate switches in the same section harden tenant egress and are **on by
default**, independent of `ebpf_policy`:

- `tcp_egress_policy` — the **TCP egress fence** (audit finding F-SSRF-1): deny a
  tenant thread's connect to cloud IMDS (`169.254.0.0/16`), loopback-except-infra,
  and RFC-1918. Described in its own section below.
- `unix_socket_policy` — fence tenant **AF_UNIX** connects behind a per-site
  allowlist (empty = deny all). Needs Linux ≥ 6.7; described below.

## Requirements

- **Linux ≥ 5.10** with `CONFIG_CGROUP_BPF` and BTF (`/sys/kernel/btf/vmlinux`).
  Setting `ebpf_policy = true` on any other platform is a hard startup error.
- **Multi-tenant mode** (`[server] sites_dir` set). Per-vhost tagging is keyed
  by the canonical site key, which only exists with vhosts. Per-request mode only —
  worker mode is not yet supported.
- **`CAP_BPF` + `CAP_NET_ADMIN`** on the ePHPm process (see systemd below).
- The **firewall loopback handoff** below.

## Configuration

```toml
[server]
sites_dir = "/srv/ephpm/sites"

[server.tenant_network]
ebpf_policy = true
# cgroup_path defaults to ePHPm's own cgroup (from /proc/self/cgroup).
# sidecar_port_range MUST sit below the kernel ephemeral range
# (net.ipv4.ip_local_port_range, default 32768-60999) — ePHPm reads that at
# startup and refuses to start on overlap.
sidecar_port_range = "20000-32767"
max_sidecar_ports_per_vhost = 8
```

`ebpf_policy` is **fail-closed**: if the programs cannot load/attach (old
kernel, no BTF, missing capability) or the port range overlaps the ephemeral
range, ePHPm refuses to start rather than come up with the policy silently
absent.

## The firewall loopback handoff (required)

The eBPF `connect4` hook runs at the socket layer, *before* nftables. If your
egress firewall blanket-drops loopback for the ePHPm cgroup (the hardened
default — see the egress-hardening guide), that DROP still kills a
BPF-authorized sidecar connect one step later. So when `ebpf_policy` is on you
**must hand loopback off to BPF**: stop dropping loopback for the ePHPm cgroup
and let the `connect4` program be the arbiter. Keep every other block
(metadata, RFC1918/LAN, DNS pinning) exactly as it was.

```diff
  # /etc/ephpm-egress.nft  — output chain, ePHPm (skuid ephpm-web)
- # Loopback sealed: nothing on 127.0.0.0/8 / ::1.
- ip  daddr 127.0.0.0/8 drop
- ip6 daddr ::1/128     drop
+ # Loopback handed to the eBPF connect4 policy (per-vhost ownership).
+ # (No loopback DROP here; the BPF program decides reachability.)
  # metadata / LAN / DNS-pin blocks below stay UNCHANGED.
```

ePHPm cannot see or enforce your firewall state, so it logs a warning at startup
when `ebpf_policy` is on reminding you this handoff must be in place. **If you
skip it, every sidecar connection fails** while the app sees only a refused
connection.

## systemd capabilities (required)

The hardened unit ships with only `CAP_NET_BIND_SERVICE`. Loading/attaching BPF
and rewriting socket addresses additionally need `CAP_BPF` and `CAP_NET_ADMIN`.
Add a drop-in (`/etc/systemd/system/ephpm.service.d/ebpf.conf`):

```ini
[Service]
AmbientCapabilities=CAP_NET_BIND_SERVICE CAP_BPF CAP_NET_ADMIN
CapabilityBoundingSet=CAP_NET_BIND_SERVICE CAP_BPF CAP_NET_ADMIN
LimitMEMLOCK=infinity
```

`LimitMEMLOCK=infinity` is required, not optional. BPF maps are charged against
`RLIMIT_MEMLOCK`, and the default limit (8 MB) is too small for the policy's
maps. ePHPm tries to raise the limit itself at load time, but under
`NoNewPrivileges` (which the hardened unit sets) it cannot without
`CAP_SYS_RESOURCE` — so the raise is refused and map creation fails with
`failed to create map ... Operation not permitted` **even when `CAP_BPF` and
`CAP_NET_ADMIN` are both present**. Setting the limit in the unit sidesteps the
in-process raise entirely.

Then `systemctl daemon-reload && systemctl restart ephpm`. These are granted
only to the hardened multi-tenant profile that opts into `ebpf_policy`; a
single-site deployment neither needs nor should have them.

## Teardown / lifecycle

The loader attaches via fd-based `bpf_link` held in process memory and **pins
nothing to bpffs**. On graceful shutdown the links/maps drop (RAII → kernel
detaches); on a hard `kill -9` the process fds close and the kernel refcounts
every program/map/attachment to zero and auto-reaps. A cold restart therefore
has no stale BPF state to reconcile.

## AF_UNIX connect fence (`unix_socket_policy`)

**Status: Linux ≥ 6.7 only, on by default (`unix_socket_policy = "auto"`) on a
multi-tenant node, independent of `ebpf_policy`.**

The policy above governs TCP/UDP. It leaves one door open that `open_basedir`
cannot close (multi-tenant breakout audit finding F2): ePHPm is one process and
one uid, so tenant PHP can `connect()` to **any unix socket the service uid can
reach** — `/var/run/docker.sock`, a database server's auth socket, the system
D-Bus, journald — through `stream_socket_client('unix://…')`,
`socket_connect()`, PDO `mysql:unix_socket=…`, or curl's
`CURLOPT_UNIX_SOCKET_PATH`. All of those end in the same `connect(2)` (or, for
an unconnected datagram socket, `sendto(2)`), which is where the fence sits:

- ePHPm attaches `cgroup/connect_unix` and `cgroup/sendmsg_unix` programs to
  its cgroup. Both hooks exist **only on Linux 6.7 and later**.
- A thread carrying a tenant tag (the same per-request tag `ebpf_policy` uses)
  may reach a unix socket only if that socket is listed for **its** site in
  `unix_socket_allow`. **The default allowlist is empty, which denies every
  unix socket to every tenant.** Denied connects fail with `EPERM` at the
  syscall — PHP sees "Permission denied" / "Operation not permitted" instantly,
  never a hang.
- ePHPm's own threads (and a request that matched no vhost) carry no tag and
  are never fenced, so the KV unix listener, DNS, the service manager and so on
  keep working.

```toml
[server]
sites_dir = "/srv/ephpm/sites"

[server.tenant_network]
unix_socket_policy = "required"     # default is "auto" (on where it can run);
                                    # "warn" (start unfenced on failure), "off" to opt out

# Per-site allowlist. Absent site (or []) = no unix socket at all.
[server.tenant_network.unix_socket_allow]
"shop.example" = ["/run/ephpm/shop/php-worker.sock", "@shop-bus"]
```

Entries are absolute pathname sockets or abstract-namespace sockets written
`@name`. A pathname matches whether the client passes the terminating NUL in
`addr_len` or not, exactly as the kernel interprets it; an abstract name is
matched bytewise.

### Fail-closed behaviour

`unix_socket_policy` has four values. `"auto"` is the default and the choice of
the others is the failure policy:

| Value | Fence attaches | Fence cannot attach (kernel < 6.7, no BTF, missing capability) |
|---|---|---|
| `"auto"` (default) | tenant unix connects fenced on a node where the fence can run (Linux, multi-tenant, per-request, not `ephpm dev`); inert elsewhere | ERROR logged, node starts **unfenced** — same as `"warn"` |
| `"required"` | tenant unix connects fenced | **startup refused** with the reason — the hard-guarantee posture for a multi-tenant node |
| `"warn"` | tenant unix connects fenced | ERROR logged, node starts **unfenced** |
| `"off"` | nothing loaded (explicit opt-out) | a node where the fence would otherwise run logs one WARN naming the exposure |

`"auto"` is the secure default: a multi-tenant Linux ≥ 6.7 node fences tenant
unix connects without any configuration, and single-tenant, `ephpm dev`,
non-Linux and worker nodes are left untouched.

ePHPm probes the preconditions before touching the kernel and names the one
that fails ("kernel 6.1.0 is older than 6.7", "kernel BTF has no kfunc
`bpf_cast_to_kern_ctx`", `EPERM` on attach). A typo'd value, an allowlist with
the policy off, a relative path or a path over 108 bytes are all startup
errors, never silent no-ops.

### Requirements (in addition to the list above)

- **Linux ≥ 6.7** with `CONFIG_DEBUG_INFO_BTF` and the BPF JIT enabled
  (`net.core.bpf_jit_enable = 1`, the default everywhere). The fence reads the
  destination through the `bpf_cast_to_kern_ctx` / `bpf_rdonly_cast` kfuncs
  against `struct bpf_sock_addr_kern.uaddrlen`, which landed with the hooks in
  6.7.
- The same `CAP_BPF` + `CAP_NET_ADMIN` and `LimitMEMLOCK=infinity` as
  `ebpf_policy`. The fence can run **without** `ebpf_policy = true` (and so
  without the nftables loopback handoff or the sidecar port range).
- Multi-tenant mode (`[server] sites_dir`), per-request PHP mode.

**Older kernels.** There is no fallback inside ePHPm: on a kernel before 6.7
the hooks do not exist, so `"required"` refuses to start while `"auto"` (the
default) and `"warn"` log an ERROR and run unfenced. A kernel built with
`CONFIG_BPF_LSM` (and `bpf` in the `lsm=` boot
list) could host an equivalent LSM-BPF policy on `unix_stream_connect` /
`socket_connect`, which also sees the *resolved* socket and so is immune to the
symlink caveat below — that is a possible future lane, not shipped. Without
either, the only mitigations are the per-uid ones: run the data plane as its
own uid and keep that uid out of every socket's group/ACL (`docker` group,
database admin sockets).

### Caveats

- **Literal match before path resolution.** The allowlist compares the string
  the client passed, before the kernel follows symlinks. A tenant that can
  write the directory holding an allowlisted path can replace the socket with a
  symlink to `docker.sock` and connect "to" the allowlisted name. Allowlist only
  paths in directories the tenant cannot write (root-owned `/run/ephpm/<site>/`
  provisioned by you, never under the site's document root), or use abstract
  sockets, which have no filesystem path. Abstract names are global to the
  network namespace: a neighbouring tenant can still *bind* the name first.
- **What breaks under deny-all.** Anything in tenant PHP that spoke to a unix
  socket now fails with `EPERM`: `syslog()` and `error_log()` to the system log
  (`/dev/log`), nss-resolve name resolution through
  `/run/systemd/resolve/io.systemd.Resolve` (glibc then falls through to the
  `dns` module in `nsswitch.conf`, so lookups still work where a DNS resolver
  is reachable), MySQL/PostgreSQL clients configured with a socket instead of a
  TCP address. Each is a deliberate allowlist decision; `/dev/log` is one shared
  socket for every tenant, so allowlisting it hands every tenant the same
  channel.
- **Scope is the server's request path.** `ephpm php --site` and the
  distributed cron run tenant code in a separate process ePHPm does not tag,
  and worker mode is not supported. `bind()` of unix sockets by tenants is not
  restricted.
- **Verifying it is on.** The startup log carries
  `tenant_network: AF_UNIX connect fence attached (cgroup/connect_unix +
  sendmsg_unix)`; `bpftool cgroup show /sys/fs/cgroup/<ephpm cgroup>` lists two
  `ephpm_connect_unix` / `ephpm_sendmsg_unix` programs. Nothing is pinned to
  bpffs; the programs vanish with the process. For a **machine-readable** check
  a fleet can assert on, `GET /_ephpm/health` carries
  `{"unix_fence_policy": "<configured value>", "unix_fence_active": true|false}`
  — `policy = "auto"` with `active = false` is a node that degraded to unfenced
  — and the gauge
  [`ephpm_tenant_unix_fence_active{policy}`](/reference/metrics/) is `1` when the
  fence is enforcing and `0` when it was intended but degraded. This matters
  because the ordinary systemd hardening (a `CapabilityBoundingSet` without
  `CAP_BPF`/`CAP_NET_ADMIN`) trips the degrade path on day one, and under `auto`
  that is only an ERROR log otherwise.
- **Architecture.** `build.rs` compiles the fence object for both `x86_64` and
  `aarch64`, and the raw loader handles both, but the raw-`bpf_attr` loader path
  has been developed and exercised on **x86_64 only**; treat live enforcement on
  arm64 as experimental and verify it (`bpftool`, a denied connect, the health
  field above) before relying on it there.

## TCP egress fence (`tcp_egress_policy`)

**Status: Linux only, on by default (`tcp_egress_policy = "auto"`) on a
multi-tenant node, independent of `ebpf_policy`.**

Out of the box tenant PHP had **unrestricted TCP egress** (multi-tenant breakout
audit finding F-SSRF-1). The dangerous consequence on a cloud host: a tenant can
`connect()` to `169.254.169.254` — the instance metadata service (IMDS) — read
the node's instance-role credentials, and with them reach the S3 KV-snapshot
bucket (every tenant's snapshot) and other internal services. `open_basedir`,
`disable_functions` and the AF_UNIX fence do nothing against a plain outbound TCP
connect. The `connect4/6` hooks sit exactly there, and for a tagged tenant thread
deny:

- **`169.254.0.0/16` (link-local / IMDS) — always, and un-allowlistable.** The
  critical fix; there is no legitimate tenant reason to reach it, so a
  `tcp_egress_allow` entry can **not** override this deny — the only way to reach
  IMDS is `tcp_egress_policy = "off"`. A v4-mapped `::ffff:169.254.x.x` is caught
  the same way.
- **Loopback (`127.0.0.0/8`, `::1`) except ePHPm's own service ports** — the
  stock `pdo_mysql` wire listener and the KV RESP listener, which `pdo_mysql` and
  phpredis legitimately use. Every *other* loopback connect (co-located services,
  the diagnostics/metrics/snapshot control ports, another process) is denied.
- **RFC-1918 (`10/8`, `172.16/12`, `192.168/16`)** — by default, closing
  internal-network SSRF and cluster-peer reach. Relax this one tier with
  `tcp_egress_deny_rfc1918 = false`, or permit specific destinations per site.

**Public-internet egress stays allowed** — a tenant calling a public API is
normal; this is an SSRF fence, not a deny-all egress wall. Denied connects fail
with `EPERM` at the syscall (the hook runs before routing, so even an unroutable
IMDS address returns `EPERM`, not a hang). ePHPm's own threads carry no tag and
are never fenced.

```toml
[server]
sites_dir = "/srv/ephpm/sites"

[server.tenant_network]
tcp_egress_policy = "required"       # default is "auto" (on where it can run);
                                     # "warn" (start unfenced on failure), "off" to opt out
# tcp_egress_deny_rfc1918 = false    # keep IMDS + loopback fence, allow private networks

# Per-site allowlist: let a site reach a specific internal API. A match overrides
# the loopback-floor and RFC-1918 denies — but NOT IMDS (169.254.0.0/16), which
# is un-allowlistable. Grant narrowly.
[server.tenant_network.tcp_egress_allow]
"shop.example" = ["10.20.0.0/16", "10.9.9.9:6379"]
```

Allowlist entries are an IPv4 address (treated as `/32`), an IPv4 CIDR (any
port), or `ip:port` (that exact port). IPv6 allowlisting is unsupported — the
deny ranges are IPv4 concepts, and a native-IPv6 destination is never fenced (a
documented gap; a tenant on a v6-routed internal network is not covered).

### Fail-closed behaviour

Same four values and failure policy as `unix_socket_policy`:

| Value | Fence attaches | Fence cannot attach (missing `CAP_BPF`/`CAP_NET_ADMIN`, `LimitMEMLOCK` too low, no BTF) |
|---|---|---|
| `"auto"` (default) | tenant TCP egress fenced on a node where the fence can run; inert elsewhere | ERROR logged, node starts **unfenced** — same as `"warn"` |
| `"required"` | tenant TCP egress fenced | **startup refused** — the hard-guarantee posture for a node that must never leak IMDS |
| `"warn"` | tenant TCP egress fenced | ERROR logged, node starts **unfenced** |
| `"off"` | nothing loaded (explicit opt-out) | a node where the fence would otherwise run logs one WARN naming the IMDS exposure |

> **It fails OPEN without privileges.** Like every eBPF fence here, this needs
> `CAP_BPF` + `CAP_NET_ADMIN` + `LimitMEMLOCK=infinity`. Under an unprivileged
> service user BPF map creation returns `EPERM`, so on `"auto"`/`"warn"` the
> fence logs an ERROR and **serves with tenant IMDS egress wide open**. For a
> security-sensitive deploy use `"required"` (refuse to serve without the fence),
> grant the capabilities (systemd / container settings above), **and** block
> `169.254.0.0/16` at the host firewall or require IMDSv2 with a hop limit of 1
> as defence-in-depth — the single most important mitigation if you cannot grant
> the capabilities.

### Verifying it is on

The startup log carries `tenant_network: TCP egress fence attached
(cgroup/connect4+6)`. The gauge
[`ephpm_tenant_tcp_egress_active{policy}`](/reference/metrics/) is `1` when
enforcing and `0` when intended but degraded to unfenced — **alert on `== 0`**,
since the ordinary systemd hardening (a `CapabilityBoundingSet` without
`CAP_BPF`/`CAP_NET_ADMIN`) trips the degrade path on day one. The counter
`ephpm_tenant_tcp_egress_denied_total{reason="imds"|"loopback"|"rfc1918"}` shows
what is being blocked; a rising `imds` rate is a tenant probing the metadata
service.

### Caveats

- **IPv6 is not fenced.** Only IPv4 (and v4-mapped IPv6) destinations are
  classified; a native-IPv6 egress is allowed. IMDS and RFC-1918 are IPv4
  concepts, but an internal service on a v6 ULA is not covered here — use the
  host firewall for v6.
- **Scope is the server's request path.** `ephpm php --site` and the distributed
  cron run tenant code in a process ePHPm does not tag; worker mode is not
  supported.
- **The allowlist matches the connect address, not a hostname.** A tenant
  resolving `metadata.google.internal` still hits the fence at the *resolved*
  `169.254.169.254`, so DNS tricks do not bypass it — but an allowlist entry must
  be the IP/CIDR, not a name.

## Known limitation

**Closed sidecar ports return to the pool only on ePHPm restart.** The
close-time reclaim path (`sock_release` + `bpf_sk_storage`) is **not yet
implemented** — as of v0.11.2 it remains open, with no target release: the
current Rust BPF loader cannot load an `SK_STORAGE` map, and reading
the source port in `sock_release` is verifier-rejected on current kernels. In
the meantime, a vhost re-binding the *same* virtual port reuses its existing
real port (the assignment is idempotent), so a restarting sidecar does not
consume a second slot. An app that opens *many different* sidecar ports over one
process lifetime can reach its `max_sidecar_ports_per_vhost` cap until ePHPm is
restarted. For the typical one-stable-sidecar-per-app model this is a non-issue.
