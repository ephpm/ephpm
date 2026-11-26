//! Live test of the AF_UNIX connect fence (`[server.tenant_network]
//! unix_socket_policy`, multi-tenant breakout audit finding F2).
//!
//! Loads the real `vhostnet` + `unixsock` objects into a throwaway cgroup v2
//! leaf, moves this process into it, and exercises the fence with real
//! sockets:
//!
//! * a **tagged** thread with no allowlist entry is denied every unix connect
//!   (`EPERM` straight from the `connect()` syscall — the default deny-all);
//! * a tagged thread may reach exactly the destination allowlisted for *its*
//!   site, whether the client passes `addr_len` with or without the trailing
//!   NUL, and not a destination allowlisted for a *different* site;
//! * an abstract-namespace destination (`@name`) is allowlistable too;
//! * a datagram `sendto()` without `connect()` is fenced by the `sendmsg_unix`
//!   hook;
//! * an **untagged** thread in the same cgroup (ePHPm's own) is never fenced.
//!
//! ## Running
//!
//! Ignored by default — it needs `CAP_BPF`/`CAP_NET_ADMIN` (i.e. root), a
//! writable cgroup v2 hierarchy, and **Linux ≥ 6.7** with BTF. Run it as root:
//!
//! ```text
//! cargo test -p ephpm-server --test tenant_unix_fence --no-run
//! sudo ./target/debug/deps/tenant_unix_fence-<hash> --ignored --nocapture
//! ```
//!
//! (On this project's WSL box: build as the unprivileged user, run the test
//! binary via `wsl -u root`.)

#![cfg(target_os = "linux")]
// One raw `connect(2)` with a hand-built `sockaddr_un` (to pass the
// NUL-inclusive `addr_len` convention std never uses). SAFETY notes inline.
#![allow(unsafe_code)]

use std::io::{Read, Write};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ephpm_config::UnixSocketTarget;
use ephpm_server::tenant_ebpf::{LoadOptions, TenantEbpf};
use ephpm_server::tenant_unix_fence::UnixFence;

/// EPERM — the errno the kernel returns when a cgroup sockaddr program denies.
const EPERM: i32 = 1;

/// Create a dedicated cgroup v2 leaf and move this whole process into it, so the
/// cgroup-attached BPF programs observe our syscalls. `None` when the
/// environment can't support the test (not root, no cgroup v2, kernel too old).
fn enter_test_cgroup() -> Option<PathBuf> {
    if let Err(e) = UnixFence::probe_support() {
        eprintln!("SKIP: {e}");
        return None;
    }
    let root = Path::new("/sys/fs/cgroup");
    if !root.join("cgroup.controllers").exists() {
        eprintln!("SKIP: no cgroup v2 unified hierarchy at /sys/fs/cgroup");
        return None;
    }
    let leaf = root.join(format!("ephpm-ebpf-unix-fence-test-{}", std::process::id()));
    if std::fs::create_dir_all(&leaf).is_err() {
        eprintln!("SKIP: cannot create {} (need root)", leaf.display());
        return None;
    }
    if std::fs::write(leaf.join("cgroup.procs"), std::process::id().to_string()).is_err() {
        eprintln!("SKIP: cannot move process into {} (need root)", leaf.display());
        let _ = std::fs::remove_dir(&leaf);
        return None;
    }
    Some(leaf)
}

fn leave_test_cgroup(leaf: &Path) {
    let _ = std::fs::write("/sys/fs/cgroup/cgroup.procs", std::process::id().to_string());
    let _ = std::fs::remove_dir(leaf);
}

/// Serve `marker` to every client of `listener`, forever, on its own thread.
fn serve_marker(listener: UnixListener, marker: &'static str) {
    std::thread::spawn(move || {
        for s in listener.incoming().flatten() {
            let mut s = s;
            let _ = s.write_all(marker.as_bytes());
        }
    });
}

/// Connect to `addr` and read the marker: `Ok(Some(marker))` on success,
/// `Err(errno)` when the syscall failed (EPERM = fence deny; ENOENT /
/// ECONNREFUSED = fence allowed, nothing there).
fn probe(addr: &SocketAddr) -> Result<Option<String>, i32> {
    match UnixStream::connect_addr(addr) {
        Ok(mut s) => {
            s.set_read_timeout(Some(Duration::from_millis(500))).ok();
            let mut buf = [0u8; 128];
            let n = s.read(&mut buf).unwrap_or(0);
            Ok((n > 0).then(|| String::from_utf8_lossy(&buf[..n]).into_owned()))
        }
        Err(e) => Err(e.raw_os_error().unwrap_or(0)),
    }
}

fn is_denied<T: std::fmt::Debug>(r: &Result<T, i32>) -> bool {
    matches!(r, Err(e) if *e == EPERM)
}

#[test]
#[ignore = "needs root (CAP_BPF/CAP_NET_ADMIN) + writable cgroup v2 + Linux >= 6.7; run manually"]
fn tenant_unix_connects_are_denied_unless_allowlisted_per_site() {
    let Some(leaf) = enter_test_cgroup() else {
        return;
    };
    let leaf_path = leaf.clone();
    let _cg = scopeguard(move || leave_test_cgroup(&leaf_path));

    let dir = tempfile::tempdir().expect("tempdir");
    let host_sock = dir.path().join("host.sock"); // never allowlisted (docker.sock stand-in)
    let alice_sock = dir.path().join("alice.sock"); // allowlisted for alice only
    let abstract_name = format!("ephpm-unix-fence-test-{}", std::process::id());

    serve_marker(UnixListener::bind(&host_sock).expect("bind host.sock"), "HOST-SECRET");
    serve_marker(UnixListener::bind(&alice_sock).expect("bind alice.sock"), "ALICE-OK");
    let abstract_addr =
        SocketAddr::from_abstract_name(abstract_name.as_bytes()).expect("abstract addr");
    serve_marker(UnixListener::bind_addr(&abstract_addr).expect("bind abstract"), "ABSTRACT-OK");

    // A datagram listener for the sendmsg_unix hook.
    let dgram_path = dir.path().join("dgram.sock");
    let dgram_srv = UnixDatagram::bind(&dgram_path).expect("bind dgram");
    dgram_srv.set_read_timeout(Some(Duration::from_millis(500))).ok();

    let cgroup = leaf.to_str().unwrap();
    let opts =
        LoadOptions { tcp_policy: false, tcp_egress: false, deny_rfc1918: false, unix_fence: true };
    let ebpf = TenantEbpf::load_and_attach(Some(cgroup), &[], &opts)
        .expect("load vhostnet BPF (need CAP_BPF and a real, non-placeholder object)");

    // Allowlist BEFORE attaching, as serve() does.
    let alice_allow = vec![
        UnixSocketTarget::parse(alice_sock.to_str().unwrap()).unwrap(),
        UnixSocketTarget::parse(&format!("@{abstract_name}")).unwrap(),
    ];
    ebpf.set_unix_allow("alice", &alice_allow).expect("write alice allowlist");
    ebpf.attach_unix_fence().expect("attach the AF_UNIX fence (Linux >= 6.7, BTF, CAP_BPF)");
    assert!(ebpf.unix_fence_active());

    let host_addr = SocketAddr::from_pathname(&host_sock).unwrap();
    let alice_addr = SocketAddr::from_pathname(&alice_sock).unwrap();

    // --- Untagged (ePHPm's own thread, same cgroup): never fenced ---
    let r = probe(&host_addr);
    assert_eq!(
        r.as_ref().ok().cloned().flatten().as_deref(),
        Some("HOST-SECRET"),
        "untagged: {r:?}"
    );

    // --- Bob (tagged, no allowlist): deny-all by default ---
    {
        let _bob = ebpf.tag_current_thread("bob");
        let r = probe(&host_addr);
        assert!(is_denied(&r), "bob -> host.sock must be EPERM (default deny-all), got {r:?}");
        let r = probe(&alice_addr);
        assert!(
            is_denied(&r),
            "bob -> alice.sock must be EPERM (allowlisted for alice, not bob), got {r:?}"
        );
        let r = probe(&abstract_addr);
        assert!(is_denied(&r), "bob -> abstract must be EPERM, got {r:?}");
        // Datagram sendto() without connect(): the sendmsg_unix hook.
        let dg = UnixDatagram::unbound().unwrap();
        let r = dg.send_to(b"hi", &dgram_path).map_err(|e| e.raw_os_error().unwrap_or(0));
        assert!(is_denied(&r), "bob sendto() dgram must be EPERM via sendmsg_unix, got {r:?}");
    }

    // --- Alice (tagged, allowlisted): her sockets only ---
    {
        let _alice = ebpf.tag_current_thread("alice");
        let r = probe(&alice_addr);
        assert_eq!(
            r.as_ref().ok().cloned().flatten().as_deref(),
            Some("ALICE-OK"),
            "alice -> alice.sock: {r:?}"
        );
        let r = probe(&abstract_addr);
        assert_eq!(
            r.as_ref().ok().cloned().flatten().as_deref(),
            Some("ABSTRACT-OK"),
            "alice -> abstract: {r:?}"
        );
        let r = probe(&host_addr);
        assert!(is_denied(&r), "alice -> host.sock must be EPERM (not allowlisted), got {r:?}");
        let dg = UnixDatagram::unbound().unwrap();
        let r = dg.send_to(b"hi", &dgram_path).map_err(|e| e.raw_os_error().unwrap_or(0));
        assert!(is_denied(&r), "alice sendto() dgram must be EPERM (not allowlisted), got {r:?}");

        // addr_len normalisation: std passes strlen+offset (no trailing NUL);
        // a raw connect() with the trailing NUL included must match the same
        // allowlist entry.
        let r = raw_connect_with_nul(&alice_sock);
        assert_eq!(
            r,
            Ok(()),
            "alice -> alice.sock with NUL-inclusive addr_len must be allowed: {r:?}"
        );
        let r = raw_connect_with_nul(&host_sock);
        assert!(
            is_denied(&r),
            "alice -> host.sock with NUL-inclusive addr_len must be EPERM, got {r:?}"
        );
    }

    // --- Untagged again after the guards dropped: still unfenced ---
    let r = probe(&host_addr);
    assert_eq!(
        r.as_ref().ok().cloned().flatten().as_deref(),
        Some("HOST-SECRET"),
        "untagged after: {r:?}"
    );
}

/// `connect()` to a pathname socket with `addr_len` covering the terminating
/// NUL (`offsetof(sun_path) + strlen + 1`), the other common client convention.
fn raw_connect_with_nul(path: &Path) -> Result<(), i32> {
    use std::os::fd::AsRawFd;
    let sock = UnixDatagram::unbound().map_err(|e| e.raw_os_error().unwrap_or(0))?;
    // A SOCK_STREAM fd via the std constructor isn't exposed unconnected, so
    // build one with libc.
    drop(sock);
    // SAFETY: plain socket(2); the fd is closed below on every path.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
    }
    // SAFETY: `sockaddr_un` is plain data; zeroed is valid.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = u16::try_from(libc::AF_UNIX).unwrap();
    let bytes = path.as_os_str().as_encoded_bytes();
    assert!(bytes.len() < addr.sun_path.len());
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        // `c_char` is `i8` on x86_64 and `u8` on aarch64.
        *dst = libc::c_char::from_ne_bytes([*src]);
    }
    let len = std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1; // + NUL
    // SAFETY: `addr` is a fully initialised sockaddr_un and `len` is within it.
    let rc = unsafe {
        libc::connect(
            fd,
            std::ptr::from_ref(&addr).cast::<libc::sockaddr>(),
            len as libc::socklen_t,
        )
    };
    let result = if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    };
    // SAFETY: fd came from socket(2) above and is closed exactly once.
    unsafe { libc::close(fd) };
    let _ = fd.as_raw_fd();
    result
}

/// Minimal drop-guard so cgroup teardown runs even if an assertion panics.
fn scopeguard<F: FnMut()>(f: F) -> impl Drop {
    struct G<F: FnMut()>(F);
    impl<F: FnMut()> Drop for G<F> {
        fn drop(&mut self) {
            (self.0)();
        }
    }
    G(f)
}
