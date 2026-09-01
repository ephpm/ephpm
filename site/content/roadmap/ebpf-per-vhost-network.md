# eBPF Per-Vhost Network Policy & Transparent Sidecar Port-Mapping

> **Status: shipped experimental (v0.8.2), Linux-only, off by default.** This
> page is the design record; for deployment see the
> **[Per-vhost network policy (eBPF) guide](/guides/ebpf-per-vhost-network/)**
> and the `[server.tenant_network]` keys in the
> [configuration reference](/reference/config/#servertenant_network).
>
> Phases 0–2 below all ship: `crates/ephpm-server/src/tenant_ebpf.rs` (the
> loader and per-thread tagging) and `crates/ephpm-server/bpf/vhostnet.bpf.c`
> (the `cgroup/bind4+6` and `cgroup/connect4+6` programs). Enable with
> `[server.tenant_network] ebpf_policy = true`, which is fail-closed — it
> refuses to start rather than serve with the policy silently absent.
>
> **Still open:** close-time sidecar port reclamation (a `sock_release` program
> returning ports to the pool) is *not* implemented — the Rust BPF loader
> cannot load an `SK_STORAGE` map and reading a socket's source port in
> `sock_release` is verifier-rejected on current kernels, so a closed sidecar
> port returns to the pool only on restart. Worker mode is unsupported (the tag
> is written on the fpm per-request path); multi-node/at-scale operation is not
> validated.
>
> The design was first proven by a PoC on Linux 6.18 (see **Proof of concept**),
> which confirmed the kernel mechanics end-to-end: two mock vhosts both binding
> `127.0.0.1:8080`, transparently remapped to private real ports, each
> reaching only its own listener, with a cross-vhost connect denied by the
> kernel (`EPERM`).

ePHPm is **one process, one uid** (`ephpm-web`), serving every vhost on ZTS
threads out of tokio's `spawn_blocking` pool. The kernel cannot tell vhosts
apart at `bind()`/`connect()` time — the same reason tenant isolation rides on
`open_basedir`/`disable_functions` rather than kernel primitives. The static
egress floor already shipped to the hardened preview host (nftables +
`systemd IPAddressDeny`) is per-uid / per-cgroup and therefore coarse: it can
seal loopback, block the metadata endpoint and the LAN, and pin DNS, but it
**cannot** express per-vhost policy or let a vhost run a sidecar that it — and
only it — can reach.

eBPF closes exactly that gap. It adds policy to a *shared* process by reading a
per-syscall identity the kernel otherwise lacks, which is precisely ePHPm's
shape. Network namespaces solve the same problem structurally but are built for
one-process-per-context (containers) and fight the shared-process model; see
**Why eBPF, not netns**.

---

## The foundation: a per-thread vhost tag

ePHPm already derives one canonical site key per request
(`Router::resolve_site`, `crates/ephpm-server/src/router.rs`). Before
dispatching a request's PHP on worker thread *T* for vhost *V*, ePHPm writes
`tag[tid] = V` into a BPF hash map and clears it when the request finishes.
That single map write — a syscall, tens to low-hundreds of nanoseconds — is the
missing per-request kernel identity. Every hook below reads it.

Two cgroup-attached programs consume the tag:

- **`cgroup/bind4`** — when a *tagged* task binds a loopback virtual port,
  rewrite the bind to that vhost's private real port and record ownership.
- **`cgroup/connect4`** — when a *tagged* task connects to a loopback virtual
  port, rewrite to its own real port; **deny** connects to a real port owned by
  a *different* vhost.

The three payoffs, from that one foundation:

1. **Per-vhost egress policy.** `connect4` reads the calling thread's vhost and
   enforces that vhost's allowlist — replacing the blanket loopback DROP with
   per-vhost loopback rules while keeping the DNS / public / metadata policy.
2. **Sidecar ownership.** `bind4` records `port_owner[real_port] = V`; a
   loopback connect is allowed iff the destination is owned by the *same*
   vhost. Cross-vhost is denied; a port you never bound is denied (falls to the
   drop floor).
3. **Transparent port-rewrite (the DX win).** The app hardcodes
   `127.0.0.1:8080`. `bind4` rewrites the bind to a private real port (e.g.
   `18080`) and records `map[(V,8080)] = 18080`; `connect4` rewrites the
   matching connect. Each vhost gets the *illusion* of its own private
   `127.0.0.1:8080`, isolated, no clashes, **no app changes, no netns**. This
   is the address-rewrite technique Cilium and the service meshes use.

---

## Feasibility: confirmed, with evidence

### Kernel mechanics

`BPF_CGROUP_INET4_BIND` / `BPF_CGROUP_INET4_CONNECT` (SEC `cgroup/bind4`,
`cgroup/connect4`) receive a writable `struct bpf_sock_addr` and may rewrite
both **address and port** before the socket operation proceeds:

- `ctx->user_ip4` — destination/bind IPv4, network byte order (`__be32`).
- `ctx->user_port` — destination/bind port, network byte order (`__be16` in a
  `__u32`). Writing it changes the actual bound/connected port.
- Return `1` = allow, `0` = deny (surfaces to userspace as `EPERM`).

Only these **UAPI** context fields are touched, so no CO-RE relocation is
needed for the rewrite itself. These are stable-ABI fields Cilium relies on in
production.

The caller identity comes from `bpf_get_current_pid_tgid()` (high 32 bits =
tgid, low 32 = tid). **Caveat proven in the PoC — see the pid-namespace
finding below:** this returns the *global* pid/tid. When ePHPm runs inside a
pid namespace (containers, Kubernetes, even WSL2-with-systemd), that will not
match the `gettid()` ePHPm itself sees, so the tag lookup misses. The fix is
`bpf_get_ns_current_pid_tgid(dev, ino, &info, sizeof(info))` (kernel ≥ 5.7),
which returns the pid/tgid *as seen in a target namespace* — ePHPm passes the
`{dev, ino}` of its own `/proc/self/ns/pid`. On a bare host pid-ns this is the
identity mapping.

### Maps

| Map | Type | Key → Value | Written by | Read by |
|-----|------|-------------|-----------|---------|
| `tag` | HASH | `tid` → `vhost_id` | ePHPm (per request) | both hooks |
| `redirect` | HASH | `(vhost<<32 \| vport)` → `real_port` | ePHPm (provision) | both hooks |
| `port_owner` | HASH | `real_port` → `vhost_id` | `bind4` | `connect4` |
| `nscfg` | ARRAY[1] | `0` → `{dev, ino}` of ePHPm's pid-ns | ePHPm (startup) | both hooks |

### Map lifecycle / cleanup on close

`port_owner` and any `(vhost,vport)` binding must be released when the sidecar's
socket closes, or a dead vhost's port ownership lingers. Two production-grade
options:

- **`bpf_sk_storage`** (`BPF_MAP_TYPE_SK_STORAGE`) — attach the ownership state
  to the *socket* itself; the kernel frees it automatically on socket destroy.
  This is the preferred home for per-socket state and eliminates manual GC.
- **`cgroup/sock_release`** (`BPF_CGROUP_INET_SOCK_RELEASE`, kernel ≥ 5.9) — a
  close-time hook that deletes the `port_owner` entry for the released socket.

The `tag` map is tid-scoped and is cleared by ePHPm at request end (not by the
kernel) — see the thread-reuse risk below.

### Minimum kernel & portability

- `cgroup/bind4` + `connect4` with port rewrite: kernel **≥ 4.17**.
- `bpf_get_ns_current_pid_tgid`: kernel **≥ 5.7**.
- `sock_release` cgroup hook: kernel **≥ 5.9**.
- **Recommended floor: 5.10** (Ubuntu 20.04-HWE / Debian 11 — a widely
  deployed LTS that covers all of the above and ships stable BTF).
- **Require `CONFIG_DEBUG_INFO_BTF=y`** (`/sys/kernel/btf/vmlinux`) for CO-RE;
  fall back to feature-disabled + WARN when absent.

---

## Rust integration: aya as loader, C programs embedded at build

**Recommendation: aya for the loader; keep the BPF programs in C, compiled by
`clang` in `build.rs` and embedded with `include_bytes!`.**

| | aya | libbpf-rs |
|--|-----|-----------|
| Loader language | pure Rust | Rust over C `libbpf` |
| Runtime C dependency | **none** (bpf() syscall only) | links `libbpf.so`/`.a` |
| Build-time C dependency | none *for the loader* | `libbpf` + `bpftool` skeleton gen |
| CO-RE / BTF | yes (aya does BTF relocation) | yes (via libbpf) |
| Fits single-static-binary ethos | **yes** | adds a C lib |

aya wins on ePHPm's defining constraint: a single self-contained binary with no
C runtime dependency — the same reason ePHPm chose `rustls` over OpenSSL. aya
loads *any* ELF BPF object, so we do **not** have to write the programs in Rust
(which would pull in nightly + `bpf-linker` at build time). Instead:

- The BPF programs stay in C (`vhostnet.bpf.c`), compiled with
  `clang -target bpf` in `build.rs`. **`clang`/`libclang` is already a required
  build prerequisite** (bindgen for the PHP SAPI), so this adds *no new build
  dependency*.
- The resulting `.o` is embedded via `include_bytes!` and loaded by aya at
  runtime with `CAP_BPF` + `CAP_NET_ADMIN` (or `CAP_SYS_ADMIN` pre-5.8).
- Everything is gated behind `#[cfg(target_os = "linux")]`; other targets get a
  no-op stub (see **Linux-only**).

Attach flow: open the cgroup ePHPm's process tree lives in (the systemd unit's
cgroup, or a dedicated child cgroup), attach `bind4`/`connect4`/`sock_release`
to it, pin nothing (links held by the running process; dropped on exit).

> The PoC uses `bpftool` to load/attach and `clang` to compile — the fastest
> path to validating kernel behavior. The production loader is aya; the C
> program is identical.

---

## Hot-path cost

- **Per request that touches a sidecar:** one `tag` map update at dispatch +
  one delete at completion — two syscalls, tens–low-hundreds of ns each. This
  can be skipped entirely for vhosts with no network policy configured.
- **Per `bind()`/`connect()`:** the hook fires **once per socket operation**,
  not per packet or per byte. Work is a handful of `O(1)` hash lookups plus at
  most one map update — tens of ns. There is **zero per-packet / per-byte
  cost**, unlike an in-path proxy.
- **Requests that open no socket** pay only the tag write/clear (or nothing, if
  the vhost has no policy). A pure static-file or cache-hit request is
  unaffected.

Contrast with per-vhost network namespaces, whose hot path is a `setns()` per
request (plus `CAP_SYS_ADMIN`) and whose per-vhost setup is veth + routing +
NAT + DNS and a full network stack in memory per tenant.

---

## Why eBPF, not netns

| | netns (per-vhost) | eBPF |
|--|------------------|------|
| Hot path | `setns()` per request + `CAP_SYS_ADMIN` | fires on bind/connect; ~1 cheap map write/req |
| Per-vhost setup | veth + routing + NAT + DNS each vhost | none (maps fill as it binds) |
| Memory | full network stack × N tenants | few-KB programs once + tiny maps |
| Isolation | stronger (structural) | logical (BPF-enforced) |
| Upfront engineering | lower (standard tooling) | higher (BPF/CO-RE) |

For ePHPm's density pitch (N sites per instance, ephemeral churn, 1 GB Nanode),
eBPF wins on runtime and memory; you pay once in engineering. netns only wins
at 2–3 low-traffic tenants.

---

## Risks & gaps

1. **PID-namespace / global tid (proven).** `bpf_get_current_pid_tgid` returns
   the global pid; inside a pid-ns it will not match ePHPm's `gettid()`. Use
   `bpf_get_ns_current_pid_tgid` with ePHPm's `/proc/self/ns/pid` `{dev,ino}`
   (written to `nscfg` at startup). This is *the* container/K8s correctness
   requirement, not an edge case.
2. **Thread reuse without clearing the tag.** A pooled `spawn_blocking` thread
   retains a stale `tag[tid]` after a request. A subsequent socket op on that
   thread would inherit the previous vhost's policy → cross-tenant leak. The
   tag **must** be set at request start and deleted at request end, bracketing
   the *entire* PHP execution including the timeout/error paths. The map ops are
   syscalls, not PHP calls, so a Rust RAII guard is safe here (it does not cross
   the PHP `setjmp`/`longjmp` boundary).
3. **Blocking vs non-blocking connect.** `connect4` fires on the `connect()`
   syscall regardless of mode; a non-blocking connect (`EINPROGRESS`) still
   passes through it exactly once. No special handling needed.
4. **IPv6.** Needs parallel `cgroup/bind6` + `connect6` programs (`user_ip6[4]`,
   same `user_port`; loopback `::1`). `getaddrinfo("localhost")` may return
   `::1` first, so a v4-only policy can be bypassed — ship v6 parity or pin
   `localhost` to v4 in managed vhosts. Document either way.
5. **Legitimate real-loopback services.** ePHPm's own infra listens on
   loopback: the stock-`pdo_mysql` MySQL wire listener (`127.0.0.1:3306`,
   `site_wire_auth.rs`) and the KV RESP listener. These are shared, per-request
   credential-authed infra — **not** per-vhost sidecars. `connect4` must
   allow-list ePHPm's infra ports *before* the ownership check (an `infra_ports`
   map), or scope the cgroup to exclude them.
6. **Handoff from the static nftables loopback-drop.** The cgroup/connect hook
   runs *before* the packet reaches nftables. Today's `/etc/ephpm-egress.nft`
   floor DROPs loopback for the `ephpm-web` uid; a BPF-rewritten connect to
   `18080` would then still be dropped by nft. So for the vhosts eBPF manages,
   the blanket loopback DROP must be **replaced** by the BPF policy: nft flips
   from "drop all loopback" to "loopback managed by BPF", while keeping the
   floor for metadata / LAN / DNS pinning. This is a coordinated change —
   **the eBPF layer and the nft rule change ship together, behind the same
   config flag.** Never design them independently.
7. **Capabilities.** Loading/attaching needs `CAP_BPF` + `CAP_NET_ADMIN`
   (`CAP_SYS_ADMIN` pre-5.8). The systemd unit must grant them
   (`AmbientCapabilities`); this is in tension with a fully unprivileged run and
   must be documented.
8. **Verifier / BTF portability.** CO-RE + BTF required; kernels without BTF
   disable the feature and WARN.

---

## Linux-only — accepted, and the docs-must-match-code obligation

eBPF is Linux-only. That is already the line for the hardened rig:
`crash_guard.c` (Unix-only), `ZEND_MAX_EXECUTION_TIMERS` (Linux-only),
systemd/cgroup. "Hardened multi-tenant hosting is a Linux feature;
Windows/macOS are single-tenant/dev" is a coherent *existing* stance, not a new
constraint.

Obligation (per **Truthfulness: Docs Must Match Code**):

- Compile as a **no-op on non-Linux** (`#[cfg(target_os = "linux")]`), with a
  stub that always builds.
- If this feature (or `multi_tenant_hardening` / `sites_dir`) is configured on a
  platform or kernel where it cannot be enforced, **startup must `tracing::warn!`
  and refuse** — never silently no-op. A config knob that isn't acted upon is a
  documentation lie.

---

## Two adjacent pieces for the full "vhost runs a sidecar" story (separable)

- **Controlled process spawning.** `proc_open`/`exec` are disabled today; a
  sidecar needs a way to launch. That is its own attack-surface decision, made
  separately from the network layer.
- **Per-vhost namespace/cgroup for the spawned process.** A spawned sidecar
  still runs as `ephpm-web` (shared uid), so eBPF gives it *network* isolation
  from sibling sidecars but not *process* isolation. Ideally spawn it into a
  per-vhost cgroup/namespace.

---

## Phased implementation plan

**Phase 0 — Tag foundation (low risk, independently useful).**
aya loads a `tag` map + `nscfg`; `router.rs` writes `tag[gettid()] = vhost_id`
before PHP dispatch and deletes it after (RAII guard bracketing execution).
No enforcement yet. Ship `nscfg` (pid-ns dev/ino) and optional observability
(current tags in metrics/admin). This is the reusable substrate.

**Phase 1 — Per-vhost egress policy.**
Attach `cgroup/connect4`: read the tag, enforce the vhost's allowlist, and
**replace** the static loopback-drop for managed vhosts (coordinated nft
handoff, same flag). Keep DNS/metadata/public policy. Includes the
`infra_ports` allow-list for ePHPm's own loopback services.

**Phase 2 — Sidecar ownership + transparent port-rewrite.**
Add `cgroup/bind4` (rewrite + `port_owner`), `connect4` rewrite/deny, and
socket-close cleanup (`bpf_sk_storage` and/or `sock_release`). IPv6 parity
(`bind6`/`connect6`). Then the adjacent controlled-spawn + per-vhost-cgroup
decision, tracked separately.

---

## Proof of concept

A complete, runnable PoC lives in the session scratchpad
(`scratchpad/ebpf-poc/`): `vhostnet.bpf.c` (the two cgroup programs + maps),
`harness.py` (mock vhosts + test matrix), `run.sh` (build → load → attach →
provision → exercise → cleanup). Built with `clang -target bpf`, loaded and
attached with `bpftool`, exercised with two Python "vhosts".

**Ran on:** Linux **6.18** (WSL2 Ubuntu, throwaway), `CONFIG_CGROUP_BPF=y`,
BTF present, cgroup2. **Result: 6/6 checks pass.**

```
LISTENER vhost=1 asked=8080 real_bound=18080   # bind rewrite
LISTENER vhost=2 asked=8080 real_bound=18081   # bind rewrite, no clash
[PASS] vhost1 -> :8080 reaches vhost1's sidecar
[PASS] vhost2 -> :8080 reaches vhost2's sidecar (no clash)
[PASS] vhost1 -> :18080 (its own real port) allowed
[PASS] vhost2 -> :18080 (vhost1's real port) DENIED   # EPERM, kernel-enforced
[PASS] vhost1 -> :18081 (vhost2's real port) DENIED   # EPERM
[PASS] untagged -> :18080 passes through unchanged     # no interference
port_owner: {18080: 1, 18081: 2}
```

Both mock vhosts hardcoded `127.0.0.1:8080`; the kernel bound them to distinct
private real ports and routed each vhost's `:8080` connect to its own listener,
denying cross-vhost access with a real `EPERM` (not `ECONNREFUSED`). The PoC
also surfaced — and fixed — the pid-namespace finding above.
