//! Build script for `ephpm-server`.
//!
//! On Linux it compiles the per-vhost network-policy BPF program
//! (`bpf/vhostnet.bpf.c`) with `clang -target bpf` into
//! `$OUT_DIR/vhostnet.bpf.o`, which `src/tenant_ebpf.rs` embeds via
//! `include_bytes!`. On every other target it is a no-op — the non-Linux
//! `tenant_ebpf` stub never `include_bytes!`s the object, so it is never needed,
//! and the `[server.tenant_network] ebpf_policy` knob is a hard config error off
//! Linux anyway.
//!
//! `clang` is ALREADY a required build dependency (bindgen for the PHP SAPI in
//! `crates/ephpm-php/build.rs`), so this adds no new toolchain requirement. If
//! `clang` is genuinely absent on a Linux host we fail the build with a clear
//! message rather than shipping a binary whose `ebpf_policy = true` would fault
//! at load time.

fn main() {
    println!("cargo:rerun-if-changed=bpf/vhostnet.bpf.c");
    println!("cargo:rerun-if-changed=bpf/include");
    println!("cargo:rerun-if-changed=build.rs");

    // Non-Linux hosts: nothing to compile. The Linux-only `cfg` in
    // `tenant_ebpf.rs` means the object is never `include_bytes!`d off Linux.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo");
    let out = format!("{out_dir}/vhostnet.bpf.o");
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
            "-idirafter",
            multiarch_inc,
            "-c",
            "bpf/vhostnet.bpf.c",
            "-o",
            &out,
        ])
        .status();

    // Build robustness: NEVER panic here. The `clang` BINARY is only present on
    // the PHP-linked release toolchain, not on stub-mode CI (which installs
    // `libclang-dev` for the PHP SAPI's bindgen but not the clang executable);
    // and a release leg for an unverified target could, in principle, hit an
    // arch-specific compile quirk. Neither may break/park the `ephpm-server`
    // build. On ANY failure — clang missing, or clang present but the compile
    // errors — emit an EMPTY placeholder so `include_bytes_aligned!` still
    // compiles, log a loud `cargo:warning`, and return. A binary built this way
    // fails closed at load time (aya rejects the empty object) and
    // `[server.tenant_network] ebpf_policy` is off by default, so nothing that
    // did not build the real object ever exercises it. The release is
    // smoke-tested afterwards to confirm the real object shipped.
    if !matches!(&run, Ok(status) if status.success()) {
        std::fs::write(&out, [])
            .expect("write placeholder eBPF object so include_bytes_aligned! compiles");
        match &run {
            Ok(status) => println!(
                "cargo:warning=clang compiled bpf/vhostnet.bpf.c with an error (exit {status}); \
                 [server.tenant_network] ebpf_policy is unavailable in this build"
            ),
            Err(e) => println!(
                "cargo:warning=clang ({clang}) not runnable ({e}); \
                 [server.tenant_network] ebpf_policy is unavailable in this build \
                 (install the clang binary to enable it)"
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
    let stripped = std::process::Command::new(&strip).args(["-g", &out]).status();
    match stripped {
        Ok(s) if s.success() => {}
        Ok(s) => {
            println!("cargo:warning=llvm-strip exited with {s}; shipping unstripped BPF object");
        }
        Err(e) => {
            println!(
                "cargo:warning=could not run llvm-strip ({strip}): {e}; shipping unstripped BPF object"
            );
        }
    }
}
