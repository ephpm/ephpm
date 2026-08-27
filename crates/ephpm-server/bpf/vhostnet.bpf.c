// SPDX-License-Identifier: GPL-2.0
//
// ePHPm per-vhost network policy — v0.8.1 BPF programs.
//
// Compiled by crates/ephpm-server/build.rs (clang -target bpf) into
// $OUT_DIR/vhostnet.bpf.o and embedded into the ePHPm binary via
// include_bytes!; loaded by the aya loader in
// crates/ephpm-server/src/tenant_ebpf.rs. Linux-only; on other targets the
// object is never built and the loader is a no-op stub.
//
// Model (proven in scratchpad/ebpf-poc on Linux 6.18, extended here):
//   * A per-thread "tag" (TID -> vhost id) is written by ePHPm before each
//     request's PHP runs and cleared at request end (RAII TagGuard). It is the
//     un-forgeable, ePHPm-set kernel network identity for the request.
//   * cgroup/bind4+bind6: when a tagged task binds a loopback virtual port,
//     pop a private real port from a pool ePHPm owns, rewrite the bind to it,
//     and record the assignment/ownership/count. Idempotent on rebind. A
//     per-vhost quota is enforced in-kernel BEFORE the pool is popped.
//   * cgroup/connect4+connect6: when a tagged task connects to loopback,
//     rewrite its own virtual port to its private real port; allow ePHPm infra
//     ports; allow a direct connect only to a real port THIS vhost owns; deny
//     everything else on loopback (the loopback floor for tagged traffic).
//
// NOTE (v0.8.1): real ports and per-vhost counts are reclaimed only when the
// process exits (the maps are freed with it). Reclaiming a port when a sidecar
// LISTENER socket closes needs either a cgroup/sock_release program reading
// per-socket ownership from bpf_sk_storage (reading ctx->src_port there is
// verifier-rejected on 6.18) OR another close signal — and aya 0.13 cannot load
// a BPF_MAP_TYPE_SK_STORAGE map. So close-time GC is deferred to v0.8.2; size
// `sidecar_port_range` and `max_sidecar_ports_per_vhost` for steady-state
// concurrency, not churn. The `assigned` map still makes rebinds idempotent, so
// a sidecar that restarts on the SAME virtual port keeps its real port and does
// not consume a second slot.
//
// Only UAPI context fields are touched (user_ip4/user_ip6/user_port), which are
// stable ABI — so no CO-RE relocation and no vmlinux.h are required.

#include <linux/bpf.h>
#include <linux/in.h>
#include <linux/in6.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_endian.h>

char LICENSE[] SEC("license") = "GPL";

#define LOOPBACK4_BE bpf_htonl(0x7f000001) // 127.0.0.1, network byte order

// Compile-time ceiling for the real-port pool/ownership maps. ePHPm fills only
// the configured sidecar_port_range (default 20000-32767 ~= 12768) at load; this
// bound just has to be >= the largest range an operator can configure.
#define POOL_CAP 13000

// TID -> vhost id. Written by ePHPm before each request's PHP runs, deleted at
// request end (RAII TagGuard). The per-request kernel identity.
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 65536);
    __type(key, __u32);   // tid (ns-local; see nscfg)
    __type(value, __u32); // vhost id
} tag SEC(".maps");

// (vhost << 32 | virtual_port) -> assigned real_port, host order. Written by
// bind on assignment; read by connect for the virtual->real rewrite; deleted by
// sock_release. Enables idempotent rebind.
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 65536);
    __type(key, __u64);
    __type(value, __u32);
} assigned SEC(".maps");

// Free real ports. Pre-filled by ePHPm from sidecar_port_range. bind pops;
// sock_release pushes back. A QUEUE => O(1) pop/push, no scan.
struct {
    __uint(type, BPF_MAP_TYPE_QUEUE);
    __uint(max_entries, POOL_CAP);
    __type(value, __u32); // real port (host order)
} port_pool SEC(".maps");

// Owner of an assigned real port. Carries both the vhost (connect-side
// authorization) and the virtual port (so sock_release can reverse-map and
// delete the `assigned` entry). real_port (host order) -> {vhost, vport}.
struct owner_t {
    __u32 vhost;
    __u32 vport;
};
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, POOL_CAP);
    __type(key, __u32);
    __type(value, struct owner_t);
} port_owner SEC(".maps");

// Per-vhost live sidecar-port count (quota enforcement). vhost -> count.
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 65536);
    __type(key, __u32);
    __type(value, __u32);
} sidecar_count SEC(".maps");

// [0] = max_sidecar_ports_per_vhost (config). Single-entry ARRAY so ePHPm can
// set it at load without recompiling.
struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, __u32);
} quota SEC(".maps");

// ePHPm's OWN loopback infra ports (host order) every tagged vhost may reach:
// the stock-pdo_mysql wire listener and the KV RESP listener. Consulted before
// the ownership deny.
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 64);
    __type(key, __u32);
    __type(value, __u8);
} infra_ports SEC(".maps");

// PID-namespace of the tagging process (ePHPm): {dev, ino} of
// /proc/self/ns/pid. Lets the programs resolve the caller's TID *as seen in
// that namespace* even though cgroup-BPF hooks otherwise observe the GLOBAL
// tid. Proven necessary in the PoC. dev==0 => host pid-ns, use the raw tid.
struct nscfg_t {
    __u64 dev;
    __u64 ino;
};
struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, struct nscfg_t);
} nscfg SEC(".maps");

static __always_inline __u32 resolve_tid(void)
{
    __u32 zero = 0;
    struct nscfg_t *c = bpf_map_lookup_elem(&nscfg, &zero);
    if (c && c->dev) {
        struct bpf_pidns_info ns = {};
        if (bpf_get_ns_current_pid_tgid(c->dev, c->ino, &ns, sizeof(ns)) == 0)
            return ns.pid; // .pid == TID in that namespace
    }
    return (__u32)bpf_get_current_pid_tgid(); // global TID (low 32 bits)
}

static __always_inline __u32 *current_vhost(void)
{
    __u32 tid = resolve_tid();
    return bpf_map_lookup_elem(&tag, &tid);
}

static __always_inline __u32 quota_max(void)
{
    __u32 zero = 0;
    __u32 *m = bpf_map_lookup_elem(&quota, &zero);
    return m ? *m : 8; // fail-safe default matches the config default
}

// ---- bind: pool allocation + quota + transparent rewrite -------------------

static __always_inline int do_bind(struct bpf_sock_addr *ctx)
{
    __u32 *vhostp = current_vhost();
    if (!vhostp)
        return 1; // untagged: bind unchanged
    __u32 vhost = *vhostp;

    __u32 vport = bpf_ntohs(ctx->user_port);
    __u64 key = ((__u64)vhost << 32) | vport;

    // Idempotent rebind: same (vhost, vport) reuses its real port.
    __u32 *ex = bpf_map_lookup_elem(&assigned, &key);
    if (ex) {
        ctx->user_port = bpf_htons((__u16)*ex);
        return 1;
    }

    // Per-vhost quota (anti port-bomb), enforced in-kernel on the tag.
    __u32 *cntp = bpf_map_lookup_elem(&sidecar_count, &vhost);
    __u32 cnt = cntp ? *cntp : 0;
    if (cnt >= quota_max())
        return 0; // DENY: this vhost is at its sidecar cap

    // Pop a free real port. No scan: ePHPm owns the range, so a popped port is
    // free by construction — no collision is possible.
    __u32 rport = 0;
    if (bpf_map_pop_elem(&port_pool, &rport) != 0)
        return 0; // DENY: pool exhausted (box-wide sidecar cap reached)

    // Record assignment, ownership, and bump the count.
    bpf_map_update_elem(&assigned, &key, &rport, BPF_ANY);
    struct owner_t o = {.vhost = vhost, .vport = vport};
    bpf_map_update_elem(&port_owner, &rport, &o, BPF_ANY);
    __u32 nc = cnt + 1;
    bpf_map_update_elem(&sidecar_count, &vhost, &nc, BPF_ANY);

    ctx->user_port = bpf_htons((__u16)rport);
    return 1;
}

SEC("cgroup/bind4")
int vhost_bind4(struct bpf_sock_addr *ctx) { return do_bind(ctx); }

SEC("cgroup/bind6")
int vhost_bind6(struct bpf_sock_addr *ctx) { return do_bind(ctx); }

// ---- connect: rewrite virtual port, or authorize by ownership --------------

static __always_inline int do_connect(struct bpf_sock_addr *ctx, int is_loopback)
{
    __u32 *vhostp = current_vhost();
    if (!vhostp)
        return 1; // untagged: connect unchanged
    __u32 vhost = *vhostp;

    if (!is_loopback)
        return 1; // non-loopback egress handled by the per-vhost allowlist layer

    __u32 dport = bpf_ntohs(ctx->user_port);

    // 1) Virtual port for this vhost -> rewrite to its private real port.
    __u64 key = ((__u64)vhost << 32) | dport;
    __u32 *rport = bpf_map_lookup_elem(&assigned, &key);
    if (rport) {
        ctx->user_port = bpf_htons((__u16)*rport);
        return 1;
    }

    // 2) Shared ePHPm infra port (MySQL wire, KV RESP) -> always allow.
    if (bpf_map_lookup_elem(&infra_ports, &dport))
        return 1;

    // 3) Direct connect to a real port -> authorize by ownership.
    struct owner_t *o = bpf_map_lookup_elem(&port_owner, &dport);
    if (o)
        return (o->vhost == vhost) ? 1 : 0; // own sidecar allow; cross-vhost DENY

    // 4) Unowned loopback port: DENY for managed vhosts. This is the loopback
    //    floor that REPLACES the static nft loopback-drop for tagged traffic.
    return 0;
}

SEC("cgroup/connect4")
int vhost_connect4(struct bpf_sock_addr *ctx)
{
    return do_connect(ctx, ctx->user_ip4 == LOOPBACK4_BE);
}

SEC("cgroup/connect6")
int vhost_connect6(struct bpf_sock_addr *ctx)
{
    int lo = ctx->user_ip6[0] == 0 && ctx->user_ip6[1] == 0 &&
             ctx->user_ip6[2] == 0 && ctx->user_ip6[3] == bpf_htonl(1); // ::1
    return do_connect(ctx, lo);
}
