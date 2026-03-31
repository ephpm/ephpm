fn main() {
    // Allow `#[cfg(sqld_embedded)]` without warnings.
    println!("cargo::rustc-check-cfg=cfg(sqld_embedded)");
    println!("cargo::rerun-if-env-changed=SQLD_BINARY_PATH");

    if let Ok(path) = std::env::var("SQLD_BINARY_PATH") {
        let src = std::path::Path::new(&path);
        if !src.exists() {
            panic!("SQLD_BINARY_PATH points to non-existent file: {path}");
        }

        // Copy sqld binary to OUT_DIR so include_bytes! can find it.
        let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
        let dest = std::path::Path::new(&out_dir).join("sqld");
        std::fs::copy(src, &dest).expect("failed to copy sqld binary to OUT_DIR");

        println!("cargo::rustc-cfg=sqld_embedded");
        println!(
            "cargo::rustc-env=SQLD_BINARY_PATH={}",
            dest.display()
        );
        println!("cargo::warning=sqld binary embedded from {path}");
    } else {
        println!("cargo::warning=SQLD_BINARY_PATH not set — building without embedded sqld");
    }
}
