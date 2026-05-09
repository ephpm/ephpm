use std::env;
use std::path::PathBuf;

fn main() {
    // Declare php_linked as a known cfg so #[cfg(php_linked)] in this crate
    // doesn't produce "unexpected cfg" warnings. The cfg is set by
    // ephpm-php/build.rs when PHP_SDK_PATH is present.
    println!("cargo::rustc-check-cfg=cfg(php_linked)");
    println!("cargo::rerun-if-env-changed=PHP_SDK_PATH");

    // When PHP is linked (release builds), override PHP's zend_signal_*
    // functions with our no-op wrappers in ephpm_wrapper.c.
    //
    // PHP's signal handling installs process-wide SIGPROF handlers that
    // crash when delivered to non-PHP threads (tokio workers). The --wrap
    // linker flag redirects all calls to these functions to our __wrap_
    // versions, which are no-ops.
    //
    // This must be in the binary crate's build.rs because rustc-link-arg
    // only takes effect for binary/cdylib targets, not library crates.
    let Some(sdk_path) = env::var_os("PHP_SDK_PATH").map(PathBuf::from) else {
        return;
    };
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    if target_os == "windows" {
        // Delay-load php8embed.dll so the process can start without the
        // DLL on disk. windows_dll::extract_php_dll() extracts the
        // embedded bytes and registers the temp dir via SetDllDirectoryW
        // before the first PHP call triggers the delay-load resolver.
        println!("cargo::rustc-link-arg=/DELAYLOAD:php8embed.dll");
        return;
    }

    for func in &[
        "zend_signal_startup",
        "zend_signal_init",
        "zend_signal_deactivate",
        "zend_signal_activate",
        "zend_signal_handler_unblock",
        // zend_set_timeout directly calls sigaction(SIGPROF) +
        // setitimer(ITIMER_PROF), bypassing the zend_signal system.
        // Must also be wrapped to prevent SIGPROF on worker threads.
        "zend_set_timeout",
        "zend_unset_timeout",
        // zend_call_stack_init probes stack boundaries on each request
        // startup. Fails on tokio spawn_blocking threads with small/
        // non-standard stacks. We disable stack checking anyway.
        "zend_call_stack_init",
    ] {
        println!("cargo::rustc-link-arg=-Wl,--wrap={func}");
    }

    // Force-link libphp.a + the support libs SPC built into the SDK.
    // Workaround: rustc-link-lib emitted from ephpm-php's build.rs
    // doesn't propagate to the final musl-static link in this layout
    // (ephpm-php is a transitive lib crate without a `links =` key).
    //
    // Pass the absolute paths to each .a file (not -l:libfoo.a) because
    // libz-sys puts its own libz.a in an earlier -L path, which would
    // win the -l:libz.a lookup and supply a libz that's missing the gz*
    // file API symbols (gzerror, gzdopen, gzclose…). Hard-pinning the
    // path forces the linker to use the SDK's libz.a (verified to export
    // the full gz API).
    //
    // Wrapping in --start-group/--end-group forces multi-pass symbol
    // resolution: PHP's static archives have circular dependencies
    // (libphp.a → libz.a → ...; libcurl → libssl; libxml2 → libz; etc.).
    let lib_dir = sdk_path.join("lib");
    println!("cargo::rustc-link-arg=-Wl,-Bstatic");
    println!("cargo::rustc-link-arg=-Wl,--start-group");
    println!("cargo::rustc-link-arg={}", lib_dir.join("libphp.a").display());
    for static_lib in &[
        "ssl", "crypto", "curl", "z", "xml2", "sodium", "iconv", "charset", "png16", "gd", "jpeg",
        "freetype", "onig", "zip", "bz2", "xslt", "exslt",
    ] {
        let archive = lib_dir.join(format!("lib{static_lib}.a"));
        if archive.exists() {
            println!("cargo::rustc-link-arg={}", archive.display());
        }
    }
    println!("cargo::rustc-link-arg=-Wl,--end-group");
    println!("cargo::rustc-link-arg=-Wl,-Bdynamic");
}
