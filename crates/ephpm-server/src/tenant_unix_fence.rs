//! AF_UNIX connect fence for tenant PHP (`[server.tenant_network]
//! unix_socket_policy`) — multi-tenant breakout audit finding **F2**.
//!
//! ePHPm is one process and one uid. Tenant PHP could therefore `connect()` to
//! any unix socket the service uid can reach — `docker.sock`, a database's
//! auth socket, D-Bus, journald — and `open_basedir` gates none of the ways to
//! do it: `unix://` stream wrappers, PDO `unix_socket=`, curl
//! `CURLOPT_UNIX_SOCKET_PATH`, `socket_connect()`. Every one of them ends in the
//! same `connect(2)` (or, for an unconnected datagram socket, `sendto(2)`), so
//! the fence sits there: a `cgroup/connect_unix` + `cgroup/sendmsg_unix` BPF
//! program (`bpf/unixsock.bpf.c`) that passes untagged threads untouched and,
//! for a thread carrying a tenant tag, allows the destination only if the
//! `(vhost, normalised sun_path)` pair is in the `unix_allow` map. An empty
//! allowlist denies every unix socket — the default posture.
//!
//! # Why a raw loader (and not aya)
//!
//! The `BPF_CGROUP_UNIX_CONNECT` / `UNIX_SENDMSG` attach types exist since
//! **Linux 6.7**. aya 0.13 knows neither of them, and reading the destination
//! needs two kfuncs (`bpf_cast_to_kern_ctx`, `bpf_rdonly_cast`) whose call
//! sites aya-obj 0.2 cannot relocate. So this module does the last mile itself:
//!
//! 1. `aya_obj::Object::parse` the embedded object, CO-RE-relocate it against
//!    the running kernel's BTF (`relocate_btf` — field offsets of
//!    `bpf_sock_addr_kern` and the `sockaddr_un` type id), relocate its map
//!    references onto the **fds of the maps aya already created** from the
//!    `vhostnet` object (`tag`, `nscfg`, `unix_allow` — shared by fd, defined
//!    once in `bpf/vhostnet_common.h`), and link calls.
//! 2. Resolve the two kfunc call sites: walk the ELF relocations of the
//!    program section with the `object` crate, and for every relocation
//!    against an *undefined* symbol patch the `call` instruction's `imm` with
//!    the kfunc's id in vmlinux BTF (`off = 0` selects vmlinux).
//! 3. `BPF_PROG_LOAD` with `prog_type = CGROUP_SOCK_ADDR` and
//!    `expected_attach_type = UNIX_CONNECT` (then again with `UNIX_SENDMSG`),
//!    and `BPF_LINK_CREATE` each onto ePHPm's cgroup.
//!
//! The same no-leak rules as `tenant_ebpf` apply: the program and link fds are
//! held as [`OwnedFd`] in process memory and **nothing is pinned**, so dropping
//! [`UnixFence`] detaches, and a `kill -9` lets the kernel reap everything.
//!
//! # Fail-closed
//!
//! [`UnixFence::probe_support`] turns every known precondition into one
//! specific message (kernel older than 6.7, no BTF, kfuncs missing). Anything
//! that fails after it — the verifier, `EPERM` on load/attach — surfaces with
//! the verifier log attached. `serve()` decides what a failure means from
//! `unix_socket_policy`: `"required"` refuses to start, `"warn"` logs an ERROR
//! and runs unfenced.

#[cfg(target_os = "linux")]
pub use imp::{MapFds, UnixFence};

// ===========================================================================
// Linux implementation.
// ===========================================================================
#[cfg(target_os = "linux")]
mod imp {
    // Raw `bpf(2)` syscalls and `uname(2)`. Every unsafe block carries a SAFETY
    // note.
    #![allow(unsafe_code)]

    use std::collections::HashSet;
    use std::fs::File;
    use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};

    use anyhow::{Context, Result, anyhow, bail, ensure};
    use aya_obj::btf::{Btf, BtfKind};
    use aya_obj::generated::{
        BPF_CALL, BPF_JMP, BPF_PSEUDO_CALL, BPF_PSEUDO_KFUNC_CALL, bpf_attach_type, bpf_attr,
        bpf_cmd, bpf_insn, bpf_prog_type,
    };
    use object::{Object as _, ObjectSection as _, ObjectSymbol as _, RelocationTarget};

    /// Compiled fence object, produced by `build.rs` (`clang -target bpf`) and
    /// embedded. `aya::include_bytes_aligned!` for the same reason as the
    /// `vhostnet` object: the ELF parser needs aligned loads.
    static UNIX_OBJECT: &[u8] =
        aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/unixsock.bpf.o"));

    /// The one program in the object. Its ELF section is named
    /// `cgroup/connect4` purely so aya-obj's parser accepts it — see the C
    /// file; the attach type is set here, at load.
    const PROGRAM: &str = "vhost_unix_fence";
    /// The ELF section holding [`PROGRAM`] (and its relocations).
    const SECTION: &str = "cgroup/connect4";

    /// Kernel floor: the AF_UNIX sockaddr hooks and `bpf_sock_addr_kern.uaddrlen`
    /// both landed in 6.7.
    const MIN_KERNEL: (u32, u32) = (6, 7);

    /// Fds of the maps the fence shares with the `vhostnet` object (which
    /// created them). The caller keeps the owning handles alive for the
    /// process lifetime; these are borrowed only for the duration of the load
    /// (the kernel program holds its own references afterwards).
    #[derive(Debug, Clone, Copy)]
    pub struct MapFds {
        /// `tag`: TID → vhost id.
        pub tag: RawFd,
        /// `nscfg`: ePHPm's pid-namespace identity.
        pub nscfg: RawFd,
        /// `unix_allow`: per-site destination allowlist.
        pub unix_allow: RawFd,
    }

    /// One loaded program attached to the cgroup by an fd-based link.
    struct Attached {
        _prog: OwnedFd,
        _link: OwnedFd,
    }

    /// The live fence: both hooks, attached. Dropping it detaches both.
    pub struct UnixFence {
        _connect: Attached,
        _sendmsg: Attached,
    }

    impl UnixFence {
        /// Check every precondition that can be decided without touching the
        /// cgroup, each with its own message: Linux ≥ 6.7, BTF exposed, the two
        /// kfuncs present.
        ///
        /// # Errors
        ///
        /// Returns an error naming the missing precondition.
        pub fn probe_support() -> Result<()> {
            let (release, major, minor) = kernel_release()?;
            ensure!(
                (major, minor) >= MIN_KERNEL,
                "kernel {release} is older than {}.{}: the AF_UNIX cgroup hooks \
                 (cgroup/connect_unix, cgroup/sendmsg_unix) do not exist before Linux 6.7, \
                 so the fence cannot be attached here",
                MIN_KERNEL.0,
                MIN_KERNEL.1
            );
            let btf = Btf::from_sys_fs().context(
                "reading /sys/kernel/btf/vmlinux: the fence needs kernel BTF \
                 (CONFIG_DEBUG_INFO_BTF) to resolve kfuncs and struct layouts",
            )?;
            for kfunc in ["bpf_cast_to_kern_ctx", "bpf_rdonly_cast"] {
                btf.id_by_type_name_kind(kfunc, BtfKind::Func)
                    .map_err(|e| anyhow!("kernel BTF has no kfunc {kfunc}: {e}"))?;
            }
            btf.id_by_type_name_kind("bpf_sock_addr_kern", BtfKind::Struct)
                .map_err(|e| anyhow!("kernel BTF has no struct bpf_sock_addr_kern: {e}"))?;
            Ok(())
        }

        /// Parse, relocate, load and attach both hooks to `cgroup`.
        ///
        /// # Errors
        ///
        /// Returns an error if a precondition fails ([`Self::probe_support`]),
        /// the object cannot be parsed or relocated (an empty placeholder
        /// object from a build without `clang` fails here), a kfunc cannot be
        /// resolved, the verifier rejects the program (its log is attached), or
        /// the link cannot be created (`CAP_BPF` + `CAP_NET_ADMIN` missing).
        pub fn load_and_attach(cgroup: &File, maps: &MapFds) -> Result<Self> {
            Self::probe_support()?;
            let insns = relocated_program(maps)?;
            let connect = load_and_link(
                &insns,
                bpf_attach_type::BPF_CGROUP_UNIX_CONNECT,
                "connect_unix",
                cgroup,
            )?;
            let sendmsg = load_and_link(
                &insns,
                bpf_attach_type::BPF_CGROUP_UNIX_SENDMSG,
                "sendmsg_unix",
                cgroup,
            )?;
            Ok(Self { _connect: connect, _sendmsg: sendmsg })
        }
    }

    /// Steps 1 and 2 of the module docs: the program's final instruction
    /// stream, CO-RE-relocated, map-relocated onto `maps`, with its kfunc call
    /// sites resolved against vmlinux BTF.
    fn relocated_program(maps: &MapFds) -> Result<Vec<bpf_insn>> {
        ensure!(
            !UNIX_OBJECT.is_empty(),
            "the embedded unixsock.bpf.o is empty: this binary was built without a working \
             `clang`, so the AF_UNIX fence is unavailable in it (see the build warning)"
        );
        // `.context(..)` throughout, never `anyhow!("{e}")`: aya-obj's errors
        // carry the real reason one level down (`source()`), and `{e}` on the
        // outer value prints only "error relocating `vhost_unix_fence`".
        let mut obj =
            aya_obj::Object::parse(UNIX_OBJECT).context("parsing the embedded unixsock.bpf.o")?;

        let vmlinux = Btf::from_sys_fs().context("reading /sys/kernel/btf/vmlinux")?;
        obj.relocate_btf(&vmlinux).context(
            "CO-RE relocation of unixsock.bpf.o against the running kernel failed. The fence \
             reads bpf_sock_addr_kern.uaddrlen, which exists from Linux 6.7.",
        )?;

        // Every map the object declares must be one we were handed an fd for;
        // the definitions are shared (vhostnet_common.h), so a mismatch is a
        // build bug, not a runtime condition.
        let fd_for = |name: &str| -> Option<RawFd> {
            match name {
                "tag" => Some(maps.tag),
                "nscfg" => Some(maps.nscfg),
                "unix_allow" => Some(maps.unix_allow),
                _ => None,
            }
        };
        let declared: Vec<(String, aya_obj::Map)> =
            obj.maps.iter().map(|(n, m)| (n.clone(), m.clone())).collect();
        let mut with_fds = Vec::with_capacity(declared.len());
        for (name, map) in &declared {
            let fd = fd_for(name).ok_or_else(|| {
                anyhow!("unixsock.bpf.o declares map {name:?}, which the vhostnet object does not create")
            })?;
            with_fds.push((name.as_str(), fd, map));
        }
        let text_sections: HashSet<usize> = obj.functions.keys().map(|(s, _)| *s).collect();
        obj.relocate_maps(with_fds.into_iter(), &text_sections)
            .context("relocating unixsock.bpf.o map references")?;
        // Deliberately NO `relocate_calls`: it exists to link BPF-to-BPF
        // subprogram calls (`src_reg == BPF_PSEUDO_CALL`) and their func/line
        // info, and this program has none (every helper is `__always_inline`,
        // checked below). It would also mistake the kfunc call sites for
        // subprogram calls — in the ELF they ARE `src_reg == BPF_PSEUDO_CALL`
        // until a loader retags them — and fail; `resolve_kfuncs` handles them.

        let prog = obj
            .programs
            .get(PROGRAM)
            .ok_or_else(|| anyhow!("program {PROGRAM} missing from unixsock.bpf.o"))?;
        let func = obj
            .functions
            .get(&prog.function_key())
            .ok_or_else(|| anyhow!("function body of {PROGRAM} missing from unixsock.bpf.o"))?;
        // The kfunc pass below maps ELF relocation offsets straight onto
        // instruction indices, which holds only while the program is the
        // single function at offset 0 of its section (every helper in the C is
        // `__always_inline`).
        ensure!(
            func.section_offset == 0,
            "{PROGRAM} does not start at offset 0 of its section (offset {}); the kfunc \
             relocation pass assumes a single-function section",
            func.section_offset
        );
        let mut insns = func.instructions.clone();
        resolve_kfuncs(&mut insns, &vmlinux)?;
        // After the kfunc pass every remaining pseudo-call is a real
        // BPF-to-BPF subprogram call, which this loader does not link.
        let call_code = u8::try_from(BPF_JMP | BPF_CALL).expect("BPF_JMP|BPF_CALL fits a u8");
        let pseudo_call = u8::try_from(BPF_PSEUDO_CALL).expect("pseudo-call tag fits a u8");
        ensure!(
            !insns.iter().any(|i| i.code == call_code && i.src_reg() == pseudo_call),
            "{PROGRAM} contains a BPF-to-BPF subprogram call; the fence must be a single \
             function (mark every helper __always_inline)"
        );
        Ok(insns)
    }

    /// Patch every kfunc call site. In the ELF, clang emits a call to an
    /// `extern __ksym` function exactly like a subprogram call — `call -1` with
    /// `src_reg = BPF_PSEUDO_CALL` — plus a relocation against an *undefined*
    /// symbol; it is the loader's job (libbpf's `RELO_EXTERN_CALL`) to turn
    /// that into `src_reg = BPF_PSEUDO_KFUNC_CALL`, `imm` = the function's id
    /// in vmlinux BTF, `off` = 0 (vmlinux, not a module). aya-obj skips these
    /// relocations (no section ⇒ "not a map"), which is why they are resolved
    /// here from the raw ELF.
    fn resolve_kfuncs(insns: &mut [bpf_insn], vmlinux: &Btf) -> Result<()> {
        let elf =
            object::File::parse(UNIX_OBJECT).context("re-parsing unixsock.bpf.o for kfuncs")?;
        let section = elf
            .section_by_name(SECTION)
            .ok_or_else(|| anyhow!("section {SECTION:?} missing from unixsock.bpf.o"))?;
        let call_code = u8::try_from(BPF_JMP | BPF_CALL).expect("BPF_JMP|BPF_CALL fits a u8");
        let pseudo_call = u8::try_from(BPF_PSEUDO_CALL).expect("pseudo-call tag fits a u8");
        let kfunc_call = u8::try_from(BPF_PSEUDO_KFUNC_CALL).expect("kfunc tag fits a u8");

        let mut resolved = 0usize;
        for (offset, reloc) in section.relocations() {
            let RelocationTarget::Symbol(sym_idx) = reloc.target() else { continue };
            let sym = elf.symbol_by_index(sym_idx).context("kfunc relocation symbol")?;
            if !sym.is_undefined() {
                continue; // a map reference, already relocated by aya-obj
            }
            let name = sym.name().context("kfunc symbol name")?;
            let idx = usize::try_from(offset / 8).context("kfunc relocation offset")?;
            let insn = insns.get_mut(idx).ok_or_else(|| {
                anyhow!("kfunc relocation for {name} at insn {idx} is out of range")
            })?;
            ensure!(
                insn.code == call_code && insn.src_reg() == pseudo_call,
                "relocation for {name} at insn {idx} is not an extern call (code {:#x}, src_reg {})",
                insn.code,
                insn.src_reg()
            );
            let id = vmlinux
                .id_by_type_name_kind(name, BtfKind::Func)
                .map_err(|e| anyhow!("kernel BTF has no kfunc {name}: {e}"))?;
            insn.set_src_reg(kfunc_call);
            insn.imm = i32::try_from(id).context("kfunc BTF id exceeds i32")?;
            insn.off = 0; // 0 = vmlinux BTF (a module would index fd_array)
            resolved += 1;
        }
        ensure!(resolved == 2, "expected 2 kfunc call sites in {PROGRAM}, resolved {resolved}");
        Ok(())
    }

    /// Step 3: load `insns` as a `CGROUP_SOCK_ADDR` program for `attach_type`
    /// and attach it to `cgroup` with an fd-based link.
    fn load_and_link(
        insns: &[bpf_insn],
        attach_type: bpf_attach_type,
        label: &str,
        cgroup: &File,
    ) -> Result<Attached> {
        let prog = load_program(insns, attach_type, label)
            .with_context(|| format!("BPF_PROG_LOAD of the AF_UNIX fence as cgroup/{label}"))?;
        let link = link_create(&prog, cgroup, attach_type).with_context(|| {
            format!(
                "BPF_LINK_CREATE attaching cgroup/{label} (needs CAP_BPF + CAP_NET_ADMIN, or \
                 CAP_SYS_ADMIN, and a cgroup v2 path)"
            )
        })?;
        Ok(Attached { _prog: prog, _link: link })
    }

    /// `BPF_PROG_LOAD`. Loaded first without a log buffer; on failure, loaded
    /// again with one so the error carries the verifier's reason (the libbpf
    /// strategy — a log buffer on a successful load can itself fail with
    /// `ENOSPC` when the log outgrows it).
    fn load_program(
        insns: &[bpf_insn],
        attach_type: bpf_attach_type,
        label: &str,
    ) -> Result<OwnedFd> {
        match try_load_program(insns, attach_type, label, None) {
            Ok(fd) => Ok(fd),
            Err(first) => {
                let mut log = vec![0u8; 1 << 20];
                match try_load_program(insns, attach_type, label, Some(&mut log)) {
                    Ok(fd) => Ok(fd),
                    Err(e) => {
                        let end = log.iter().position(|&b| b == 0).unwrap_or(log.len());
                        let text = String::from_utf8_lossy(&log[..end]);
                        // The tail is where the verifier states its verdict.
                        let lines: Vec<&str> = text.trim().lines().collect();
                        let tail = lines[lines.len().saturating_sub(12)..].join("\n");
                        Err(anyhow!("{e} (first attempt: {first}); verifier log tail:\n{tail}"))
                    }
                }
            }
        }
    }

    fn try_load_program(
        insns: &[bpf_insn],
        attach_type: bpf_attach_type,
        label: &str,
        log: Option<&mut [u8]>,
    ) -> Result<OwnedFd> {
        // SAFETY: `bpf_attr` is a plain-data union of integers; all-zero is a
        // valid (empty) value for every variant.
        let mut attr: bpf_attr = unsafe { std::mem::zeroed() };
        // SAFETY: we exclusively own `attr` and select the PROG_LOAD variant;
        // the union is written through this one view only.
        let u = unsafe { &mut attr.__bindgen_anon_3 };
        u.prog_type = bpf_prog_type::BPF_PROG_TYPE_CGROUP_SOCK_ADDR as u32;
        u.expected_attach_type = attach_type as u32;
        u.insns = insns.as_ptr() as u64;
        u.insn_cnt = u32::try_from(insns.len()).context("instruction count exceeds u32")?;
        u.license = LICENSE.as_ptr() as u64;
        // A 16-byte, NUL-padded name shows up in `bpftool prog show`.
        let name = format!("ephpm_{label}");
        for (dst, src) in u.prog_name.iter_mut().zip(name.bytes().take(15)) {
            // `c_char` is `i8` on x86_64 and `u8` on aarch64; the name is
            // ASCII, so the conversion is lossless on both.
            *dst = libc::c_char::try_from(src).unwrap_or(0);
        }
        if let Some(log) = log {
            u.log_level = 1;
            u.log_buf = log.as_mut_ptr() as u64;
            u.log_size = u32::try_from(log.len()).context("log buffer exceeds u32")?;
        }
        sys_bpf(bpf_cmd::BPF_PROG_LOAD, &mut attr)
    }

    /// NUL-terminated for the kernel, which copies the string.
    static LICENSE: &[u8] = b"GPL\0";

    /// `BPF_LINK_CREATE` of `prog` onto `cgroup` for `attach_type`, exclusive
    /// mode (flags 0) like the `vhostnet` programs.
    fn link_create(prog: &OwnedFd, cgroup: &File, attach_type: bpf_attach_type) -> Result<OwnedFd> {
        // SAFETY: see `try_load_program` — zeroed plain-data union.
        let mut attr: bpf_attr = unsafe { std::mem::zeroed() };
        // SAFETY: exclusive ownership; LINK_CREATE variant written only.
        let lc = unsafe { &mut attr.link_create };
        lc.__bindgen_anon_1.prog_fd = u32::try_from(prog.as_raw_fd()).context("program fd")?;
        lc.__bindgen_anon_2.target_fd =
            u32::try_from(cgroup.as_fd().as_raw_fd()).context("cgroup fd")?;
        lc.attach_type = attach_type as u32;
        lc.flags = 0;
        sys_bpf(bpf_cmd::BPF_LINK_CREATE, &mut attr)
    }

    /// One `bpf(2)` call that returns a new fd.
    fn sys_bpf(cmd: bpf_cmd, attr: &mut bpf_attr) -> Result<OwnedFd> {
        let size = libc::c_ulong::try_from(std::mem::size_of::<bpf_attr>()).expect("size fits");
        // SAFETY: `attr` is a valid, exclusively borrowed `bpf_attr` for the
        // duration of the call, every pointer stored in it (instructions,
        // license, log buffer) outlives the call, and `size` is its true size.
        // Both commands return a new fd on success and -1 with errno otherwise.
        let ret = unsafe {
            libc::syscall(libc::SYS_bpf, cmd as libc::c_long, std::ptr::from_mut(attr), size)
        };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            bail!("bpf({cmd:?}) failed: {err}");
        }
        let fd = RawFd::try_from(ret).context("bpf() returned a non-fd value")?;
        // SAFETY: a successful PROG_LOAD/LINK_CREATE returns a fresh fd that no
        // one else owns; wrapping it transfers that ownership exactly once.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// `(release string, major, minor)` of the running kernel from `uname(2)`.
    fn kernel_release() -> Result<(String, u32, u32)> {
        // SAFETY: `utsname` is plain data; all-zero is a valid initial value,
        // and `uname` fully writes it on success (the only failure is EFAULT,
        // impossible with a valid pointer).
        let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::uname(&raw mut uts) };
        ensure!(rc == 0, "uname(2) failed: {}", std::io::Error::last_os_error());
        let bytes: Vec<u8> = uts
            .release
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| u8::try_from(c).unwrap_or(b'?'))
            .collect();
        let release = String::from_utf8_lossy(&bytes).into_owned();
        let (major, minor) = parse_major_minor(&release)
            .ok_or_else(|| anyhow!("cannot parse a major.minor kernel version from {release:?}"))?;
        Ok((release, major, minor))
    }

    /// `"6.18.33.2-microsoft-standard-WSL2"` → `(6, 18)`.
    fn parse_major_minor(release: &str) -> Option<(u32, u32)> {
        let mut parts = release.split(['.', '-']);
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        Some((major, minor))
    }

    #[cfg(test)]
    mod tests {
        use super::parse_major_minor;

        #[test]
        fn kernel_release_parsing() {
            assert_eq!(parse_major_minor("6.18.33.2-microsoft-standard-WSL2"), Some((6, 18)));
            assert_eq!(parse_major_minor("6.7.0"), Some((6, 7)));
            assert_eq!(parse_major_minor("5.15.0-1045-azure"), Some((5, 15)));
            assert_eq!(parse_major_minor("6.1"), Some((6, 1)));
            assert_eq!(parse_major_minor("garbage"), None);
        }
    }
}
