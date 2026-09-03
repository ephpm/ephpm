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

## Known limitation

**Closed sidecar ports return to the pool only on ePHPm restart.** The
close-time reclaim path (`sock_release` + `bpf_sk_storage`) is **not yet
implemented** — as of v0.8.6 it remains open, with no target release: the
current Rust BPF loader cannot load an `SK_STORAGE` map, and reading
the source port in `sock_release` is verifier-rejected on current kernels. In
the meantime, a vhost re-binding the *same* virtual port reuses its existing
real port (the assignment is idempotent), so a restarting sidecar does not
consume a second slot. An app that opens *many different* sidecar ports over one
process lifetime can reach its `max_sidecar_ports_per_vhost` cap until ePHPm is
restarted. For the typical one-stable-sidecar-per-app model this is a non-issue.
