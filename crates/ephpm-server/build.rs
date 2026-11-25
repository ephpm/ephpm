//! Build script for `ephpm-server`.
//!
//! On Linux it compiles the per-vhost network-policy BPF programs with
//! `clang -target bpf`:
//!
//! * `bpf/vhostnet.bpf.c` → `$OUT_DIR/vhostnet.bpf.o` — the `cgroup/bind4+6`
//!   and `connect4+6` loopback/sidecar policy, embedded by
//!   `src/tenant_ebpf.rs` and loaded with aya;
//! * `bpf/unixsock.bpf.c` → `$OUT_DIR/unixsock.bpf.o` — the AF_UNIX connect
//!   fence, embedded by `src/tenant_unix_fence.rs` and loaded by ePHPm's own
//!   raw loader (aya 0.13 cannot attach the unix sockaddr hooks).
//!
//! On every other target it is a no-op — the non-Linux stubs never
//! `include_bytes!` the objects, so they are never needed, and the
//! `[server.tenant_network]` eBPF knobs are a hard config error off Linux
//! anyway.
//!
//! `clang` is ALREADY a required build dependency (bindgen for the PHP SAPI in
//! `crates/ephpm-php/build.rs`), so this adds no new toolchain requirement.
//!
//! If `clang` is genuinely absent (or the compile fails) we must still let a
//! dev/stub build go through, so we emit an EMPTY placeholder object and a loud
//! `cargo::warning` rather than panicking. That placeholder is fail-closed at
//! load time (the loaders reject an empty object), but note that the AF_UNIX
//! fence (`unix_socket_policy`) now defaults to `auto` — ON for a multi-tenant
//! Linux >= 6.7 node — so a binary built without the real object comes up
//! UNFENCED under the default and only logs about it. Dev builds tolerate that;
//! a RELEASE must not. `release.yml` therefore makes the clang toolchain FATAL
//! on the Linux release legs and asserts both objects are non-empty after the
//! build (`.github/scripts/check-ebpf-objects.sh`) so a transient toolchain
//! failure can never ship an unfenced release.

fn main() {
    println!("cargo:rerun-if-changed=bpf/vhostnet.bpf.c");
    println!("cargo:rerun-if-changed=bpf/unixsock.bpf.c");
    println!("cargo:rerun-if-changed=bpf/vhostnet_common.h");
    println!("cargo:rerun-if-changed=bpf/include");
    println!("cargo:rerun-if-changed=build.rs");

    // Non-Linux hosts: nothing to compile. The Linux-only `cfg` in
    // `tenant_ebpf.rs` / `tenant_unix_fence.rs` means the objects are never
    // `include_bytes!`d off Linux.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo");
    compile_bpf("bpf/vhostnet.bpf.c", &format!("{out_dir}/vhostnet.bpf.o"), "ebpf_policy");
    compile_bpf("bpf/unixsock.bpf.c", &format!("{out_dir}/unixsock.bpf.o"), "unix_socket_policy");
}

/// Compile one BPF C source to `out` (BTF kept, DWARF stripped). `knob` names
/// the `[server.tenant_network]` key whose feature the object backs, for the
/// warning emitted when the compile cannot happen.
fn compile_bpf(src: &str, out: &str, knob: &str) {
    let clang = std::env::var("CLANG").unwrap_or_else(|_| "clang".to_string());
    // `llvm-strip` sits next to clang in every LLVM install; allow an override
    // to mirror `$CLANG`.
    let strip = std::env::var("LLVM_STRIP").unwrap_or_else(|_| "llvm-strip".to_string());

    // Multiarch include path (Debian/Ubuntu) for <asm/types.h> under -target bpf.
    // Only Debian-family puts the kernel UAPI asm headers under a multiarch
    // subdir; RHEL-family (almalinux -- our release image) has them on the
    // default path, where this `-idirafter` is a harmless no-op. Derived from the
    // target arch so cross-compiles pick the right tree.
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "x86_64".into());
    let (multiarch_inc, target_def) = match arch.as_str() {
        "aarch64" => ("/usr/include/aarch64-linux-gnu", "-D__TARGET_ARCH_arm64"),
        _ => ("/usr/include/x86_64-linux-gnu", "-D__TARGET_ARCH_x86"),
    };

    let run = std::process::Command::new(&clang)
        .args([
            "-O2",
            "-g", // BTF needs debug info
            "-Wall",
            "-Werror",
            "-target",
            "bpf",
            target_def,
            // libbpf program-side headers (<bpf/bpf_helpers.h> etc.) are VENDORED
            // under bpf/include, not taken from the build host: they ship with
            // libbpf-dev on Debian/Ubuntu but are not packaged for almalinux8
            // (our glibc-2.28 release image), so relying on the host silently
            // stubbed the eBPF object there. Vendoring makes the compile
            // distro-independent. See bpf/include/NOTICE for provenance/license.
            "-I",
            "bpf/include",
            // The shared map header (vhostnet_common.h) lives beside the sources.
            "-I",
            "bpf",
            "-idirafter",
            multiarch_inc,
            "-c",
            src,
            "-o",
            out,
        ])
        .status();

    // Build robustness: NEVER panic here. The `clang` BINARY is only present on
    // the PHP-linked release toolchain, not on stub-mode CI (which installs
    // `libclang-dev` for the PHP SAPI's bindgen but not the clang executable);
    // and a release leg for an unverified target could, in principle, hit an
    // arch-specific compile quirk. Neither may break/park a dev/stub
    // `ephpm-server` build. On ANY failure — clang missing, or clang present
    // but the compile errors — emit an EMPTY placeholder so
    // `include_bytes_aligned!` still compiles, log a loud `cargo::warning`, and
    // return. A binary built this way fails closed at load time (the loaders
    // reject the empty object).
    //
    // IMPORTANT: this is only safe for dev/stub builds. The `unix_socket_policy`
    // AF_UNIX fence now defaults to `auto` (ON for a multi-tenant Linux >= 6.7
    // node), so a RELEASE binary built from the placeholder would come up
    // UNFENCED under the default and merely log about it. The warning below is
    // deliberately loud about that. The RELEASE path does NOT rely on it:
    // `release.yml` makes the clang toolchain fatal on the Linux legs and
    // `.github/scripts/check-ebpf-objects.sh` asserts both objects are
    // non-empty after the build, so a release can never reach this fallback.
    if !matches!(&run, Ok(status) if status.success()) {
        std::fs::write(out, [])
            .expect("write placeholder eBPF object so include_bytes_aligned! compiles");
        match &run {
            Ok(status) => println!(
                "cargo::warning=clang compiled {src} with an error (exit {status}); \
                 [server.tenant_network] {knob} is UNAVAILABLE in this build — a multi-tenant node \
                 would run UNFENCED under the default (unix_socket_policy = auto). Dev builds only; \
                 releases are gated by check-ebpf-objects.sh."
            ),
            Err(e) => println!(
                "cargo::warning=clang ({clang}) not runnable ({e}); \
                 [server.tenant_network] {knob} is UNAVAILABLE in this build — a multi-tenant node \
                 would run UNFENCED under the default (unix_socket_policy = auto). Install the \
                 clang binary to enable it. Dev builds only; releases are gated by \
                 check-ebpf-objects.sh."
            ),
        }
        return;
    }

    // Strip DWARF debug info while keeping the `.BTF` sections the loader needs
    // (`llvm-strip -g` removes `.debug_*` but preserves `.BTF`/`.BTF.ext`). This
    // shrinks the embedded object by ~40% and avoids feeding aya's ELF parser the
    // DWARF sections `clang -g` emits — which are useless at load time. Non-fatal:
    // the unstripped object still loads, so a missing `llvm-strip` only forgoes
    // the size win.
    let stripped = std::process::Command::new(&strip).args(["-g", out]).status();
    match stripped {
        Ok(s) if s.success() => {}
        Ok(s) => {
            println!(
                "cargo:warning=llvm-strip exited with {s}; shipping unstripped BPF object {src}"
            );
        }
        Err(e) => {
            println!(
                "cargo:warning=could not run llvm-strip ({strip}): {e}; shipping unstripped BPF object {src}"
            );
        }
    }
}
