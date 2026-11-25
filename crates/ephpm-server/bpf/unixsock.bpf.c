// SPDX-License-Identifier: GPL-2.0
//
// ePHPm AF_UNIX connect fence — `[server.tenant_network] unix_socket_policy`.
//
// Multi-tenant breakout audit finding F2: ePHPm is one process and one uid, so
// a tenant's PHP can connect() to ANY unix socket the service uid can reach —
// docker.sock, a database's auth socket, D-Bus — and `open_basedir` gates none
// of the ways to do it (`unix://` streams, PDO `unix_socket=`, curl
// `CURLOPT_UNIX_SOCKET_PATH`). All of them end in the same connect(2) /
// sendto(2) syscall, which is where this program sits.
//
// Program (one body, attached twice):
//   * cgroup/connect_unix  (BPF_CGROUP_UNIX_CONNECT, Linux >= 6.7): every
//     AF_UNIX connect() — stream, seqpacket and datagram.
//   * cgroup/sendmsg_unix  (BPF_CGROUP_UNIX_SENDMSG, Linux >= 6.7): a
//     sendto()/sendmsg() with an explicit destination on an UNconnected
//     datagram socket, which never goes through connect() — the syslog(3) /
//     journald path (`/dev/log`, `/run/systemd/journal/socket`).
//
// Decision: an UNTAGGED thread (one of ePHPm's own, or a request that matched
// no vhost) passes untouched. A TAGGED (tenant) thread may reach the
// destination only if (vhost, normalised destination) is in `unix_allow`; an
// empty allowlist denies everything. The kernel turns a 0 return into EPERM.
//
// Reading the destination needs the kernel-side sockaddr, which the UAPI
// `struct bpf_sock_addr` does not expose for AF_UNIX: `bpf_cast_to_kern_ctx()`
// yields `struct bpf_sock_addr_kern` (whose `uaddrlen` member exists only
// since 6.7 — the same series that added the unix hooks), and
// `bpf_core_cast()` re-types its `uaddr` as `struct sockaddr_un` so the path
// bytes can be read directly. Both are "common" kfuncs available to every
// program type. Field offsets and the `sockaddr_un` type id come from CO-RE
// relocations against the running kernel's BTF, so this object carries no
// vmlinux.h and no hardcoded struct layout.
//
// LOADING: aya 0.13 can neither attach the unix sockaddr hooks nor resolve
// kfunc calls, so this object is loaded by ePHPm's own raw loader
// (crates/ephpm-server/src/tenant_unix_fence.rs), which uses aya-obj to parse
// and relocate it, resolves the kfunc ids itself, and drives BPF_PROG_LOAD /
// BPF_LINK_CREATE directly. Two consequences for this file:
//   1. The section name below is `cgroup/connect4` ONLY so aya-obj's parser
//      classifies the function as a program and runs CO-RE relocation over it.
//      The loader overrides the attach type at BPF_PROG_LOAD; the name is not
//      the hook. Do not "fix" it to `cgroup/connect_unix` — aya-obj rejects the
//      whole object on an unknown section name.
//   2. Every helper must be `__always_inline`: the loader assumes the program
//      is a single function starting at section offset 0 (no subprogram
//      linking) when it maps kfunc relocation offsets onto instructions.
//
// Maps (`tag`, `nscfg`, `unix_allow`) are the ones vhostnet.bpf.c creates;
// this object only references them. Keep vhostnet_common.h the single source
// of their definitions.

#include <linux/bpf.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_core_read.h>

#include "vhostnet_common.h"

char LICENSE[] SEC("license") = "GPL";

// Kernel-side context behind `struct bpf_sock_addr` (include/linux/filter.h).
// Only `uaddr` and `uaddrlen` are read; CO-RE relocates both by NAME against
// the running kernel, so the other members exist only to document the shape
// and need not be laid out exactly. `uaddrlen` was added in 6.7 together with
// the AF_UNIX hooks — on an older kernel the relocation fails and the loader
// refuses the object (fail closed).
struct bpf_sock_addr_kern {
    void *sk;
    void *uaddr;
    __u64 tmp_reg;
    void *t_ctx;
    __u32 uaddrlen;
} __attribute__((preserve_access_index));

// <linux/un.h>. Declared locally (same name, same shape) so
// bpf_core_type_id_kernel() can look the kernel's type up by name.
struct sockaddr_un {
    unsigned short sun_family;
    char sun_path[EPHPM_UNIX_PATH_MAX];
} __attribute__((preserve_access_index));

extern void *bpf_cast_to_kern_ctx(void *ctx) __ksym;

// offsetof(struct sockaddr_un, sun_path)
#define SUN_PATH_OFFSET 2

static __always_inline int unix_fence(struct bpf_sock_addr *ctx)
{
    __u32 *vhostp = current_vhost();
    if (!vhostp)
        return 1; // untagged: ePHPm's own threads, unmatched hosts — pass

    struct bpf_sock_addr_kern *kctx = bpf_cast_to_kern_ctx(ctx);
    if (!kctx)
        return 0; // DENY: cannot see the destination

    // The kernel has already run unix_validate_addr(): 2 <= len <= 110. Deny
    // anything outside that anyway rather than trust the caller's length.
    __u32 ulen = kctx->uaddrlen;
    if (ulen < SUN_PATH_OFFSET || ulen > SUN_PATH_OFFSET + EPHPM_UNIX_PATH_MAX)
        return 0;
    __u32 plen = ulen - SUN_PATH_OFFSET; // sun_path bytes the caller supplied
    if (plen == 0)
        return 0; // DENY: an unnamed destination can never be allowlisted

    struct sockaddr_un *un = bpf_core_cast(kctx->uaddr, struct sockaddr_un);
    if (!un)
        return 0;

    struct unix_allow_key_t key = {};
    key.vhost = *vhostp;

    // Copy the supplied bytes. Every access uses a CONSTANT offset: the verifier
    // rejects variable-offset loads through a BTF-typed pointer, so the loop is
    // fully unrolled and each iteration is guarded by `i < plen`. Bytes past
    // `plen` are never read (they are uninitialised kernel stack beyond the
    // copied sockaddr) and stay zero in the key.
#pragma clang loop unroll(full)
    for (__u32 i = 0; i < EPHPM_UNIX_PATH_MAX; i++) {
        if (i < plen)
            key.path[i] = un->sun_path[i];
    }

    __u32 n = plen;
    if (key.path[0] != 0) {
        // Pathname socket: the kernel reads sun_path as a C string bounded by
        // addr_len (unix_mkname_bsd -> strnlen), so a client that includes the
        // terminating NUL in addr_len and one that does not name the SAME
        // socket. Normalise to strlen and zero the tail so both hash alike.
        //
        //
        // Two passes, each shaped so clang can fully unroll it within the
        // 512-byte BPF stack. `n` is a pure AND/ADD chain over the bytes: no
        // selects and nothing kept live across iterations (a select chain
        // spilled all 108 bytes and blew the stack). The zeroing is BRANCHLESS
        // on purpose: a loop whose body is guarded by a monotone condition on
        // `i` (`if (i >= n) ...`) gets split by LLVM into two variable-bound
        // loops, the full unroll then fails, and -Werror=pass-failed turns that
        // into a build error. An unconditional AND with a 0x00/0xff mask has no
        // branch to split on.
        __u8 live = 1;
        n = 0;
#pragma clang loop unroll(full)
        for (__u32 i = 0; i < EPHPM_UNIX_PATH_MAX; i++) {
            live &= (key.path[i] != 0); // bytes >= plen are already 0: ends there at the latest
            n += live;
        }
#pragma clang loop unroll(full)
        for (__u32 i = 0; i < EPHPM_UNIX_PATH_MAX; i++) {
            __u8 keep = (__u8)0 - (__u8)(i < n); // 0xff while i < n, else 0x00
            key.path[i] &= (char)keep;
        }
    }
    // Abstract socket (path[0] == 0): every supplied byte is significant and
    // the kernel compares the full addr_len, so `n = plen` as copied.
    key.len = n;

    return bpf_map_lookup_elem(&unix_allow, &key) ? 1 : 0;
}

// Section name is for aya-obj's parser only — see the file header. The loader
// attaches this function as BPF_CGROUP_UNIX_CONNECT and, loaded a second time,
// as BPF_CGROUP_UNIX_SENDMSG.
SEC("cgroup/connect4")
int vhost_unix_fence(struct bpf_sock_addr *ctx) { return unix_fence(ctx); }
