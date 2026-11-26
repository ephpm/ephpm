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
// NOTE: real ports and per-vhost counts are reclaimed only when the
// process exits (the maps are freed with it). Reclaiming a port when a sidecar
// LISTENER socket closes needs either a cgroup/sock_release program reading
// per-socket ownership from bpf_sk_storage (reading ctx->src_port there is
// verifier-rejected on 6.18) OR another close signal — and aya 0.13 cannot load
// a BPF_MAP_TYPE_SK_STORAGE map. So close-time GC is not implemented; size
// `sidecar_port_range` and `max_sidecar_ports_per_vhost` for steady-state
// concurrency, not churn. The `assigned` map still makes rebinds idempotent, so
// a sidecar that restarts on the SAME virtual port keeps its real port and does
// not consume a second slot.
//
// Only UAPI context fields are touched (user_ip4/user_ip6/user_port), which are
// stable ABI — so no CO-RE relocation and no vmlinux.h are required.
//
// The `tag`, `nscfg` and `unix_allow` maps are declared in vhostnet_common.h
// because the AF_UNIX fence (unixsock.bpf.c, a separately loaded object) shares
// them by fd — see that header. This object is the one that CREATES them.

#include <linux/bpf.h>
#include <linux/in.h>
#include <linux/in6.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_endian.h>

#include "vhostnet_common.h"

char LICENSE[] SEC("license") = "GPL";

// `struct bpf_sock_addr.type` is the socket type (UAPI-stable). The TCP egress
// fence governs TCP only: the knob is `tcp_egress_policy`, and the IMDS /
// control-plane / cluster destinations it protects are all TCP. Fencing UDP too
// would break DNS — glibc's resolver connect()s its UDP socket to the
// nameserver (systemd-resolved 127.0.0.53, Docker 127.0.0.11, k8s CoreDNS on
// 10.x), all of which the tiers below would deny. SOCK_STREAM is 1 in the UAPI.
#ifndef SOCK_STREAM
#define SOCK_STREAM 1
#endif

// ---- loopback classification (shared by bind + connect, v4 + v6) -----------
//
// Linux treats the ENTIRE 127.0.0.0/8 range as loopback (RFC 1122 §3.2.1.3),
// not just 127.0.0.1, and an IPv4-mapped IPv6 address `::ffff:a.b.c.d` reaches
// the same IPv4 stack as `a.b.c.d`. Classifying only the exact literals
// 127.0.0.1 / ::1 as loopback let a tagged tenant reach a neighbour's loopback
// sidecar by aliasing the destination (127.0.0.2, ::ffff:127.0.0.1, ...):
// do_connect() saw `is_loopback == 0`, skipped the port-ownership check, and
// allowed the connect. These helpers are the single source of truth for both
// hooks so the two sides can never drift apart again.
//
// Everything is compared in NETWORK byte order (the on-wire order of the UAPI
// user_ip4 / user_ip6 context fields), so masks/literals are wrapped in
// bpf_htonl(). All loads are of the aligned 32-bit context words, so the
// verifier is happy and there are no unaligned reads.

// 127.0.0.0/8 in network byte order, plus the all-zero address. The kernel
// rewrites a connect to 0.0.0.0 into a connect to 127.0.0.1 (INADDR_ANY as a
// destination means "this host"), so 0.0.0.0 reaches the same loopback services
// the floor denies — it must classify as loopback or it slips through the
// non-loopback allow branch (and, with the sidecar policy on, past the
// port_owner check). 0.0.0.5 etc. do NOT route to loopback, so the special case
// is kept narrow to the exact zero address.
static __always_inline int is_loopback4_be(__u32 ip_be)
{
    return ip_be == 0 || (ip_be & bpf_htonl(0xff000000)) == bpf_htonl(0x7f000000);
}

// Classify a 128-bit IPv6 address (four network-order 32-bit words) as loopback:
//   * ::1                         -> loopback
//   * ::ffff:a.b.c.d (v4-mapped)  -> loopback IFF a.b.c.d is in 127.0.0.0/8
// An IPv4-mapped address that embeds a NON-loopback v4 (e.g. ::ffff:1.1.1.1)
// is deliberately NOT loopback — it belongs to the egress allowlist layer, same
// as the bare IPv4 would. IPv4-COMPATIBLE addresses (::a.b.c.d, RFC 4291
// §2.5.5.1 deprecated) are intentionally not folded in: Linux does not route
// them onto the v4 loopback, so there is no reachable bypass, and treating the
// ::/96 prefix as "classify by embedded v4" would misclassify ::1 itself
// (embedded 0.0.0.1, outside 127/8) as non-loopback.
static __always_inline int is_loopback6(const __u32 ip6[4])
{
    // :: (all-zero) and ::1 both route to loopback (the kernel rewrites a
    // connect to :: into ::1), so classify both as loopback — otherwise :: and
    // ::ffff:0.0.0.0 slip past the floor onto the control-plane ports.
    if (ip6[0] == 0 && ip6[1] == 0 && ip6[2] == 0 &&
        (ip6[3] == 0 || ip6[3] == bpf_htonl(1)))
        return 1;
    // ::ffff:0:0/96 — IPv4-mapped: first 80 bits zero, next 16 bits 0xffff.
    // is_loopback4_be now also matches the embedded 0.0.0.0 (-> ::ffff:127.0.0.1).
    if (ip6[0] == 0 && ip6[1] == 0 && ip6[2] == bpf_htonl(0x0000ffff))
        return is_loopback4_be(ip6[3]);
    return 0;
}

// Compile-time ceiling for the real-port pool/ownership maps. ePHPm fills only
// the configured sidecar_port_range (default 20000-32767 ~= 12768) at load; this
// bound just has to be >= the largest range an operator can configure.
#define POOL_CAP 13000

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

// ---- TCP egress fence (audit finding F-SSRF-1) -----------------------------
//
// Runtime policy flags, set once at load by ePHPm from the resolved
// [server.tenant_network] config. [0] is a bitmask of POL_* below. The same
// connect4/6 programs serve both the sidecar feature (ebpf_policy) and the
// egress fence (tcp_egress_policy); which branches run is decided by these
// flags, so one object covers every combination without a recompile.
#define POL_SIDECAR      (1u << 0) // ebpf_policy: virtual-port rewrite + ownership + loopback floor
#define POL_EGRESS       (1u << 1) // tcp_egress_policy: IMDS / loopback-except-infra / (opt) RFC-1918 deny
#define POL_DENY_RFC1918 (1u << 2) // under POL_EGRESS, also deny RFC-1918
struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, __u32);
} policy SEC(".maps");

// Per-site egress CIDR allowlist (IPv4), an LPM trie. Key is {prefixlen, vhost
// (network order), addr (network order)}: the top 32 bits (vhost) are always
// matched exactly (every entry has prefixlen >= 32), the next up-to-32 bits are
// the CIDR. A hit means "this vhost may reach this destination on ANY port" and
// OVERRIDES the loopback-floor and RFC-1918 denies — but NOT IMDS
// (169.254.0.0/16), which is checked first and is un-allowlistable. ePHPm writes
// it at startup from [server.tenant_network] tcp_egress_allow.
struct egress_cidr_key_t {
    __u32 prefixlen;
    __u32 vhost; // network order, matched exactly (prefixlen >= 32)
    __u32 addr;  // network order (user_ip4)
};
struct {
    __uint(type, BPF_MAP_TYPE_LPM_TRIE);
    __uint(max_entries, 4096);
    __type(key, struct egress_cidr_key_t);
    __type(value, __u8);
    __uint(map_flags, BPF_F_NO_PREALLOC); // required for LPM_TRIE
} egress_cidr SEC(".maps");

// Per-site exact ip:port egress allowlist. Same override semantics as
// egress_cidr but for a specific destination port only.
struct egress_hostport_key_t {
    __u32 vhost; // host order (internal id)
    __u32 addr;  // network order (user_ip4)
    __u32 port;  // host order (dport)
};
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 4096);
    __type(key, struct egress_hostport_key_t);
    __type(value, __u8);
} egress_hostport SEC(".maps");

// Count of denied tenant connects by reason, sampled by ePHPm for the
// ephpm_tenant_tcp_egress_denied_total metric. Index = DENY_* below.
#define DENY_IMDS     0
#define DENY_LOOPBACK 1
#define DENY_RFC1918  2
struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 3);
    __type(key, __u32);
    __type(value, __u64);
} egress_denied SEC(".maps");

static __always_inline __u32 policy_flags(void)
{
    __u32 zero = 0;
    __u32 *f = bpf_map_lookup_elem(&policy, &zero);
    return f ? *f : 0;
}

static __always_inline void bump_deny(__u32 idx)
{
    __u64 *c = bpf_map_lookup_elem(&egress_denied, &idx);
    if (c)
        __sync_fetch_and_add(c, 1);
}

// 169.254.0.0/16 (link-local / cloud IMDS), network byte order.
static __always_inline int is_linklocal4_be(__u32 ip_be)
{
    return (ip_be & bpf_htonl(0xffff0000)) == bpf_htonl(0xa9fe0000);
}

// RFC-1918 private ranges, network byte order.
static __always_inline int is_rfc1918_be(__u32 ip_be)
{
    __u32 h = bpf_ntohl(ip_be);
    if ((h & 0xff000000) == 0x0a000000) // 10.0.0.0/8
        return 1;
    if ((h & 0xfff00000) == 0xac100000) // 172.16.0.0/12
        return 1;
    if ((h & 0xffff0000) == 0xc0a80000) // 192.168.0.0/16
        return 1;
    return 0;
}

// Is (vhost, addr_be, dport) explicitly allowlisted? CIDR (any port) OR exact
// ip:port. An allowlist hit overrides every deny category.
static __always_inline int egress_allowed(__u32 vhost, __u32 addr_be, __u32 dport)
{
    struct egress_cidr_key_t ck = {
        .prefixlen = 64,         // match vhost (32) + addr (32) fully; LPM picks the longest entry
        .vhost = bpf_htonl(vhost),
        .addr = addr_be,
    };
    if (bpf_map_lookup_elem(&egress_cidr, &ck))
        return 1;
    struct egress_hostport_key_t hk = {.vhost = vhost, .addr = addr_be, .port = dport};
    if (bpf_map_lookup_elem(&egress_hostport, &hk))
        return 1;
    return 0;
}

// Egress verdict for a NON-loopback IPv4 destination of a tagged tenant thread
// under POL_EGRESS: 1 = allow, 0 = deny (and a reason is counted). Public
// internet stays allowed; this is an SSRF fence, not a deny-all egress wall.
static __always_inline int egress_verdict4(__u32 vhost, __u32 addr_be, __u32 dport, __u32 flags)
{
    // IMDS (169.254.0.0/16) is checked FIRST and is UN-ALLOWLISTABLE: there is
    // no legitimate tenant reason to reach cloud metadata, so a per-site
    // tcp_egress_allow entry must NOT re-open the instance-role-cred-theft path.
    // The only way to reach it is tcp_egress_policy = "off".
    if (is_linklocal4_be(addr_be)) {
        bump_deny(DENY_IMDS); // the critical IMDS block, always on
        return 0;
    }
    if (egress_allowed(vhost, addr_be, dport))
        return 1; // explicit per-site allow overrides the denies below (not IMDS)
    if ((flags & POL_DENY_RFC1918) && is_rfc1918_be(addr_be)) {
        bump_deny(DENY_RFC1918);
        return 0;
    }
    return 1; // public egress allowed
}

// Extract the embedded IPv4 of an ::ffff:a.b.c.d v4-mapped address; returns 1
// and sets *v4 (network order) when mapped, else 0. A v4-mapped destination
// reaches the IPv4 stack, so it must run the same egress rules as the bare v4
// would — otherwise ::ffff:169.254.169.254 would bypass the IMDS block.
static __always_inline int v4_mapped(const __u32 ip6[4], __u32 *v4)
{
    if (ip6[0] == 0 && ip6[1] == 0 && ip6[2] == bpf_htonl(0x0000ffff)) {
        *v4 = ip6[3];
        return 1;
    }
    return 0;
}

// fe80::/10 link-local in network byte order (top 10 bits == 1111111010).
static __always_inline int is_linklocal6(const __u32 ip6[4])
{
    return (ip6[0] & bpf_htonl(0xffc00000)) == bpf_htonl(0xfe800000);
}

// The AWS IPv6 IMDS address fd00:ec2::254 — a specific ULA, treated as metadata
// (unconditional, un-allowlistable IMDS deny) rather than ordinary ULA.
static __always_inline int is_imds6(const __u32 ip6[4])
{
    // fd00:0ec2:0000:0000:0000:0000:0000:0254 -> words fd000ec2, 0, 0, 00000254.
    return ip6[0] == bpf_htonl(0xfd000ec2) && ip6[1] == 0 && ip6[2] == 0 &&
           ip6[3] == bpf_htonl(0x00000254);
}

// fc00::/7 unique-local (the IPv6 analogue of RFC-1918), top 7 bits 1111110.
static __always_inline int is_ula6(const __u32 ip6[4])
{
    return (ip6[0] & bpf_htonl(0xfe000000)) == bpf_htonl(0xfc000000);
}

// Egress verdict for a NATIVE (non-v4-mapped, non-loopback) IPv6 destination
// under POL_EGRESS. v6 is NOT allowlistable (tcp_egress_allow is IPv4 only).
static __always_inline int egress_verdict6(const __u32 ip6[4], __u32 flags)
{
    // Link-local (fe80::/10) and the AWS v6 IMDS are metadata-class: always
    // denied, un-allowlistable, same as v4 169.254.0.0/16.
    if (is_linklocal6(ip6) || is_imds6(ip6)) {
        bump_deny(DENY_IMDS);
        return 0;
    }
    if ((flags & POL_DENY_RFC1918) && is_ula6(ip6)) {
        bump_deny(DENY_RFC1918);
        return 0;
    }
    return 1; // public IPv6 egress allowed
}

static __always_inline __u32 quota_max(void)
{
    __u32 zero = 0;
    __u32 *m = bpf_map_lookup_elem(&quota, &zero);
    return m ? *m : 8; // fail-safe default matches the config default
}

// ---- bind: pool allocation + quota + transparent rewrite -------------------
//
// NOTE ON LOOPBACK CLASSIFICATION: unlike do_connect(), do_bind() does NOT gate
// on the destination address — it rewrites the port of EVERY bind by a tagged
// task (loopback, wildcard 0.0.0.0/::, or a specific address) to a private real
// port the vhost owns. So there is no exact-127.0.0.1/::1 classification here to
// keep in sync with the connect side; the is_loopback4_be/is_loopback6 helpers
// are a connect-side concern only. This is safe because the security boundary is
// the connect-side ownership check (port_owner keyed on the real port): whatever
// address a sidecar binds, a peer tenant reaching its real port is denied there.
// A tagged bind to a wildcard address is still rewritten and owned, so a peer
// aliasing it via 127.x now hits that same ownership deny once connect classifies
// the whole 127.0.0.0/8 + mapped range as loopback.
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

// ---- connect: sidecar rewrite/ownership (POL_SIDECAR) + egress fence
//      (POL_EGRESS) ---------------------------------------------------------
//
// `has_v4`/`v4_be` carry the destination's IPv4 address when one exists: the
// bare v4 (connect4) or the embedded v4 of an ::ffff:a.b.c.d v4-mapped address
// (connect6). Native IPv6 passes `ip6` (the four network-order words) and
// has_v4 == 0; connect4 passes ip6 == NULL (never dereferenced, has_v4 == 1).
static __always_inline int do_connect(struct bpf_sock_addr *ctx, int is_loopback,
                                      int has_v4, __u32 v4_be, const __u32 *ip6)
{
    __u32 *vhostp = current_vhost();
    if (!vhostp)
        return 1; // untagged (an ePHPm thread, or a request that matched no vhost)
    __u32 vhost = *vhostp;

    // TCP-only fence: a UDP connect (DNS, QUIC, ...) is never touched — not
    // rewritten, not floored, not denied. See the SOCK_STREAM note at the top.
    if (ctx->type != SOCK_STREAM)
        return 1;

    __u32 flags = policy_flags();
    __u32 dport = bpf_ntohs(ctx->user_port);

    if (is_loopback) {
        // 1) Sidecar virtual-port rewrite (ebpf_policy only).
        if (flags & POL_SIDECAR) {
            __u64 key = ((__u64)vhost << 32) | dport;
            __u32 *rport = bpf_map_lookup_elem(&assigned, &key);
            if (rport) {
                ctx->user_port = bpf_htons((__u16)*rport);
                return 1;
            }
        }

        // 2) Shared ePHPm infra port (stock pdo_mysql wire, KV RESP) -> allow.
        //    These are the loopback destinations a tenant is MEANT to reach.
        if (bpf_map_lookup_elem(&infra_ports, &dport))
            return 1;

        // 3) Direct connect to an owned sidecar real port (ebpf_policy only).
        if (flags & POL_SIDECAR) {
            struct owner_t *o = bpf_map_lookup_elem(&port_owner, &dport);
            if (o)
                return (o->vhost == vhost) ? 1 : 0; // own sidecar allow; cross-vhost DENY
        }

        // 4) Explicit per-site allowlist may permit a specific loopback target.
        if (has_v4 && egress_allowed(vhost, v4_be, dport))
            return 1;

        // 5) Loopback floor: any OTHER loopback connect is DENIED for a managed
        //    vhost — co-located services, the diagnostics/metrics/snapshot
        //    control ports, another process. Applies under EITHER the sidecar
        //    feature (replacing the static nft loopback-drop) or the egress
        //    fence. If neither governs loopback, leave it unchanged.
        if (flags & (POL_SIDECAR | POL_EGRESS)) {
            bump_deny(DENY_LOOPBACK);
            return 0;
        }
        return 1;
    }

    // Non-loopback destination.
    if (!(flags & POL_EGRESS))
        return 1; // egress fence off: historical allow-all non-loopback egress

    if (has_v4)
        return egress_verdict4(vhost, v4_be, dport, flags);

    // Native IPv6 non-loopback: fence link-local / AWS v6 IMDS (unconditional)
    // and ULA (under the RFC-1918 flag). Not allowlistable (v4-only allowlist).
    if (ip6)
        return egress_verdict6(ip6, flags);
    return 1;
}

SEC("cgroup/connect4")
int vhost_connect4(struct bpf_sock_addr *ctx)
{
    // Whole 127.0.0.0/8 + 0.0.0.0, not just 127.0.0.1 — see is_loopback4_be().
    return do_connect(ctx, is_loopback4_be(ctx->user_ip4), 1, ctx->user_ip4, (const __u32 *)0);
}

SEC("cgroup/connect6")
int vhost_connect6(struct bpf_sock_addr *ctx)
{
    __u32 v4 = 0;
    int has_v4 = v4_mapped(ctx->user_ip6, &v4);
    // :: / ::1 and IPv4-mapped loopback (::ffff:127.x, ::ffff:0.0.0.0) —
    // see is_loopback6(). Native-v6 egress ranges are handled in do_connect
    // via egress_verdict6(ctx->user_ip6).
    return do_connect(ctx, is_loopback6(ctx->user_ip6), has_v4, v4, ctx->user_ip6);
}
