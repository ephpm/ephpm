//! Live regression test for the eBPF per-vhost loopback-classifier bypass.
//!
//! The `vhostnet` connect classifier once recognised only the exact literals
//! `127.0.0.1` / `::1` as loopback, so a tagged tenant could reach a neighbour
//! tenant's loopback sidecar by aliasing the destination — `127.0.0.2`,
//! `::ffff:127.0.0.1`, `::ffff:127.0.0.2` — which the classifier called
//! *non*-loopback, causing `do_connect` to skip the port-ownership check.
//!
//! This test stands up two tagged tenants (Alice, Bob) in one process/cgroup,
//! has Alice bind an unauthenticated loopback sidecar (its bind is rewritten to
//! a private real port Alice owns), and then has Bob try to reach that real port
//! through each aliased address. After the fix every alias is denied at the
//! `connect` hook (`EPERM`) and Alice's secret marker never crosses; before the
//! fix Bob's connect succeeds and reads the marker, so this test fails.
//!
//! It also checks the invariants the fix must NOT break: ePHPm's own infra port
//! (`3306`) stays reachable, a non-loopback egress is not denied by the loopback
//! floor, and Alice can still reach her own sidecar.
//!
//! ## Running
//!
//! Ignored by default — it needs `CAP_BPF`/`CAP_NET_ADMIN` (i.e. root) and a
//! writable cgroup v2 hierarchy, which a normal `cargo test` run does not have.
//! Run it as root on a Linux ≥ 5.10 host:
//!
//! ```text
//! cargo test -p ephpm-server --test tenant_ebpf_loopback --no-run
//! sudo ./target/debug/deps/tenant_ebpf_loopback-<hash> --ignored --nocapture
//! ```
//!
//! (On this project's WSL box: build as the unprivileged user, run the test
//! binary via `wsl -u root`.)

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use ephpm_server::tenant_ebpf::TenantEbpf;

/// EPERM — the errno a `cgroup/connect` BPF program returns to deny a connect.
const EPERM: i32 = 1;

/// Create a dedicated cgroup v2 leaf and move this whole process into it, so the
/// cgroup-attached BPF programs observe our bind/connect syscalls. Returns the
/// leaf path, or `None` when the environment can't support the test (not root,
/// no cgroup v2) — in which case the caller skips.
fn enter_test_cgroup() -> Option<PathBuf> {
    let root = std::path::Path::new("/sys/fs/cgroup");
    if !root.join("cgroup.controllers").exists() {
        eprintln!("SKIP: no cgroup v2 unified hierarchy at /sys/fs/cgroup");
        return None;
    }
    let leaf = root.join(format!("ephpm-ebpf-loopback-test-{}", std::process::id()));
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

/// Move back to the root cgroup and remove the leaf. Best-effort teardown.
fn leave_test_cgroup(leaf: &std::path::Path) {
    let _ = std::fs::write("/sys/fs/cgroup/cgroup.procs", std::process::id().to_string());
    let _ = std::fs::remove_dir(leaf);
}

/// Attempt a connect and report the outcome distinctly: the marker string if the
/// connection succeeded and data was read, `Err(errno)` for a syscall error
/// (EPERM = BPF deny, ECONNREFUSED = BPF allowed but nothing listening at dest).
fn probe(addr: SocketAddr) -> Result<Option<String>, i32> {
    match TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
        Ok(mut s) => {
            s.set_read_timeout(Some(Duration::from_millis(500))).ok();
            let mut buf = [0u8; 128];
            let n = s.read(&mut buf).unwrap_or(0);
            Ok((n > 0).then(|| String::from_utf8_lossy(&buf[..n]).into_owned()))
        }
        Err(e) => Err(e.raw_os_error().unwrap_or(0)),
    }
}

/// True when a connect result is a hard BPF denial (`EPERM`) with no data.
fn is_denied(r: &Result<Option<String>, i32>) -> bool {
    matches!(r, Err(e) if *e == EPERM)
}

#[test]
#[ignore = "needs root (CAP_BPF/CAP_NET_ADMIN) + writable cgroup v2; run manually"]
fn aliased_and_mapped_loopback_is_ownership_checked() {
    let Some(leaf) = enter_test_cgroup() else {
        return; // environment can't support the test; skip.
    };
    let leaf_path = leaf.clone();
    // Ensure teardown even on assertion panic.
    let _guard = scopeguard(move || leave_test_cgroup(&leaf_path));

    let cgroup = leaf.to_str().unwrap();
    let ebpf = TenantEbpf::load_and_attach(Some(cgroup), &[3306, 6379])
        .expect("load+attach vhostnet BPF (need CAP_BPF and a real, non-placeholder object)");
    ebpf.fill_pool((20000, 20100), 8).expect("fill port pool");

    // --- infra listener on 3306 (untagged bind => not rewritten) ---
    let infra = TcpListener::bind("127.0.0.1:3306").expect("bind infra 3306");
    std::thread::spawn(move || {
        for s in infra.incoming().flatten() {
            let mut s = s;
            let _ = s.write_all(b"INFRA-3306-OK");
        }
    });

    // --- Alice (tenant 1): sidecar bound SPECIFICALLY to 127.0.0.1 ---
    let (tx, rx) = mpsc::channel::<u16>();
    let alice_ebpf = ebpf.clone();
    std::thread::spawn(move || {
        let _tag = alice_ebpf.tag_current_thread("alice");
        let l = TcpListener::bind("127.0.0.1:8080").expect("alice bind (rewritten)");
        let real = l.local_addr().unwrap().port();
        tx.send(real).unwrap();
        for s in l.incoming().flatten() {
            let mut s = s;
            let _ = s.write_all(format!("ALICE-SECRET-{real}").as_bytes());
        }
    });
    let alice_port = rx.recv_timeout(Duration::from_secs(5)).expect("alice real port");

    // --- Alice sidecar #2 bound to 0.0.0.0 (reachable at any 127.x alias) ---
    let (tx2, rx2) = mpsc::channel::<u16>();
    let alice_ebpf2 = ebpf.clone();
    std::thread::spawn(move || {
        let _tag = alice_ebpf2.tag_current_thread("alice");
        let l = TcpListener::bind("0.0.0.0:8081").expect("alice wildcard bind");
        let real = l.local_addr().unwrap().port();
        tx2.send(real).unwrap();
        for s in l.incoming().flatten() {
            let mut s = s;
            let _ = s.write_all(format!("ALICE-WILDCARD-{real}").as_bytes());
        }
    });
    let alice_wild = rx2.recv_timeout(Duration::from_secs(5)).expect("alice wildcard port");

    std::thread::sleep(Duration::from_millis(200));

    // --- Bob (tenant 2): attempt to reach Alice ---
    let _bob = ebpf.tag_current_thread("bob");

    let v4 = |ip: Ipv4Addr, p: u16| SocketAddr::new(IpAddr::V4(ip), p);
    let v6 = |ip: Ipv6Addr, p: u16| SocketAddr::new(IpAddr::V6(ip), p);

    // Baseline: exact 127.0.0.1 to Alice's real port is denied (always was).
    let exact = probe(v4(Ipv4Addr::LOCALHOST, alice_port));
    assert!(is_denied(&exact), "127.0.0.1 must be ownership-denied, got {exact:?}");

    // THE BUG: mapped loopback ::ffff:127.0.0.1 to Alice's 127.0.0.1 sidecar.
    // Pre-fix: connects and reads "ALICE-SECRET-*". Post-fix: EPERM, no marker.
    let mapped = probe(v6("::ffff:127.0.0.1".parse().unwrap(), alice_port));
    assert!(
        is_denied(&mapped),
        "BYPASS: ::ffff:127.0.0.1 reached Alice's 127.0.0.1 sidecar: {mapped:?}"
    );

    // THE BUG: aliased loopback 127.0.0.2 to Alice's 0.0.0.0 sidecar.
    // Pre-fix: connects and reads "ALICE-WILDCARD-*". Post-fix: EPERM.
    let aliased = probe(v4(Ipv4Addr::new(127, 0, 0, 2), alice_wild));
    assert!(is_denied(&aliased), "BYPASS: 127.0.0.2 reached Alice's 0.0.0.0 sidecar: {aliased:?}");

    // THE BUG: mapped aliased ::ffff:127.0.0.2 to Alice's 0.0.0.0 sidecar.
    let mapped_alias = probe(v6("::ffff:127.0.0.2".parse().unwrap(), alice_wild));
    assert!(
        is_denied(&mapped_alias),
        "BYPASS: ::ffff:127.0.0.2 reached Alice's 0.0.0.0 sidecar: {mapped_alias:?}"
    );

    // Also confirm the 127.0.0.2 vector against the 127.0.0.1-only sidecar is
    // denied at the BPF layer (pre-fix it was ALLOWED — ECONNREFUSED because the
    // socket is 127.0.0.1-specific — which is still a policy bypass).
    let aliased_specific = probe(v4(Ipv4Addr::new(127, 0, 0, 2), alice_port));
    assert!(
        is_denied(&aliased_specific),
        "127.0.0.2 must be ownership-denied by the loopback floor, got {aliased_specific:?}"
    );

    // --- Invariants the fix must NOT break ---

    // Infra port stays reachable and serves its marker.
    let infra_probe = probe(v4(Ipv4Addr::LOCALHOST, 3306));
    assert_eq!(
        infra_probe.as_ref().ok().and_then(Clone::clone).as_deref(),
        Some("INFRA-3306-OK"),
        "infra port 3306 must stay reachable, got {infra_probe:?}"
    );

    // A non-loopback egress must NOT be EPERM'd by the loopback floor (it belongs
    // to the egress allowlist layer). 192.0.2.1 (TEST-NET-1) is unroutable here,
    // so we expect a timeout/unreachable, never EPERM.
    let egress = probe(v4(Ipv4Addr::new(192, 0, 2, 1), 9));
    assert!(
        !matches!(&egress, Err(e) if *e == EPERM),
        "public egress must not be denied by the loopback floor, got {egress:?}"
    );

    // Alice can still reach her OWN sidecar (ownership allow).
    let _alice_again = ebpf.tag_current_thread("alice");
    let own = probe(v4(Ipv4Addr::LOCALHOST, alice_port));
    assert_eq!(
        own.as_ref().ok().and_then(Clone::clone).as_deref(),
        Some(&format!("ALICE-SECRET-{alice_port}")[..]),
        "Alice must still reach her own sidecar, got {own:?}"
    );
}

/// Minimal drop-guard so cgroup teardown runs even if an assertion panics
/// (avoids a `scopeguard` crate dependency in dev-deps).
fn scopeguard<F: FnMut()>(f: F) -> impl Drop {
    struct G<F: FnMut()>(F);
    impl<F: FnMut()> Drop for G<F> {
        fn drop(&mut self) {
            (self.0)();
        }
    }
    G(f)
}
