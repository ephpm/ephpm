// SPDX-License-Identifier: GPL-2.0
//
// ePHPm per-vhost network policy — map definitions and helpers SHARED by the
// two BPF objects ePHPm builds:
//
//   * vhostnet.bpf.c  — cgroup/bind4+6 + connect4+6 (loopback/sidecar policy),
//                       loaded by aya; CREATES every map declared here.
//   * unixsock.bpf.c  — cgroup/connect_unix + sendmsg_unix (AF_UNIX fence),
//                       loaded by ePHPm's own raw loader and relocated against
//                       the maps the vhostnet object already created.
//
// The second object references `tag`, `nscfg` and `unix_allow` BY FD: the raw
// loader hands it the fds of the maps aya created from the first object. That
// only works if both objects agree on each map's key/value SIZE, so the
// definitions live here, in one place, and both .c files include this header.
// Do not redefine any of these maps in either .c file.

#ifndef EPHPM_VHOSTNET_COMMON_H
#define EPHPM_VHOSTNET_COMMON_H

#include <linux/bpf.h>
#include <bpf/bpf_helpers.h>

// TID -> vhost id. Written by ePHPm before each request's PHP runs, deleted at
// request end (RAII TagGuard). The per-request kernel identity. Every program
// in both objects keys its "is this a tenant thread?" decision on this map.
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 65536);
    __type(key, __u32);   // tid (ns-local; see nscfg)
    __type(value, __u32); // vhost id
} tag SEC(".maps");

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

// ---- AF_UNIX connect fence ---------------------------------------------------
//
// Per-site allowlist of unix-socket destinations a tagged (tenant) thread may
// connect() / sendto(). Keyed on the vhost id plus the NORMALISED destination:
//
//   * pathname socket  ("/run/foo.sock"): `path` holds the C string WITHOUT a
//     trailing NUL, zero-padded; `len` = strlen. This is exactly how the kernel
//     itself interprets a pathname sockaddr_un (strnlen within addr_len), so a
//     client passing addr_len with or without the terminating NUL normalises to
//     the same key.
//   * abstract socket  ("\0name"): `path[0] == 0`, the name follows, `len` =
//     1 + name length, every byte significant (embedded NULs included) — the
//     kernel compares abstract names bytewise over the full addr_len.
//
// A tagged thread whose (vhost, destination) is NOT in this map is DENIED. An
// empty map therefore means: tenant PHP may reach NO unix socket at all — the
// default posture. ePHPm populates it at startup from
// `[server.tenant_network] unix_socket_allow`. UNIX_PATH_MAX is 108 in the
// UAPI (<linux/un.h>), spelled out here so this header stays self-contained
// under -target bpf.
#define EPHPM_UNIX_PATH_MAX 108

struct unix_allow_key_t {
    __u32 vhost;
    __u32 len;
    char path[EPHPM_UNIX_PATH_MAX];
};
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 4096);
    __type(key, struct unix_allow_key_t);
    __type(value, __u8);
} unix_allow SEC(".maps");

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

// The vhost id of the calling thread, or NULL when the thread is untagged
// (one of ePHPm's own threads, or a request that matched no vhost).
static __always_inline __u32 *current_vhost(void)
{
    __u32 tid = resolve_tid();
    return bpf_map_lookup_elem(&tag, &tid);
}

#endif // EPHPM_VHOSTNET_COMMON_H
