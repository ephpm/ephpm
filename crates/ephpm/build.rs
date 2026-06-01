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
        // PHP is linked statically from php8embed.lib (the php-sdk Windows
        // tarball is a static-php-cli `--build-embed` build — no DLL). There
        // is no delay-loaded DLL to set up, and MSVC link.exe does not
        // support GNU ld's `--wrap`, so the zend_signal_* SIGPROF wrapping
        // applied on Linux is a no-op here (ephpm enforces max_execution_time
        // at the tokio layer, so PHP's SIGPROF handler should never fire).
        //
        // /FORCE:MULTIPLE: libintl_a and libiconv_a are both whole-archived
        // (see ephpm-php/build.rs::link_windows_static_deps) and each bundles
        // an identical copy of libcharset's `locale_charset`, so it ends up
        // multiply-defined. There is no per-object exclude for whole-archive;
        // this tells link.exe to keep the first definition and continue.
        // `locale_charset` is the only duplicate in the whole link and both
        // copies are byte-identical, so first-wins is correct. Emits LNK4006
        // warnings (visible in the build log) which we accept.
        println!("cargo::rustc-link-arg=/FORCE:MULTIPLE");
        return;
    }

    let lib_dir = sdk_path.join("lib");

    if target_os == "macos" {
        // Apple's ld64 doesn't understand GNU ld's --wrap, --start-group/
        // --end-group, or -Bstatic/-Bdynamic. ld64 does multi-pass
        // symbol resolution by default, so the group flags aren't needed
        // — just list the archives directly and let it sort circular
        // deps out.
        //
        // The zend_signal_* SIGPROF wrapping that --wrap provides on
        // Linux is currently NOT applied on macOS. This is a known gap
        // (TODO: implement via -Wl,-alias_list or by overriding the
        // symbols via a sibling .a built specifically to win symbol
        // resolution order). It only matters if SIGPROF on tokio
        // worker threads ever fires — PHP installs the handler in
        // zend_signal_startup but typically only delivers it under
        // max_execution_time, which ephpm enforces at the tokio layer
        // instead and shouldn't trigger.
        println!("cargo::rustc-link-arg={}", lib_dir.join("libphp.a").display());
        for static_lib in macos_static_libs() {
            let archive = lib_dir.join(format!("lib{static_lib}.a"));
            if archive.exists() {
                println!("cargo::rustc-link-arg={}", archive.display());
            }
        }
        // ICU on macOS is built against libc++ (Apple's C++ stdlib);
        // libphp's intl extension drags in std::logic_error,
        // std::length_error, ___gxx_personality_v0, etc. Rust doesn't
        // auto-add -lc++ when we override the link line with explicit
        // rustc-link-arg directives, so add it explicitly.
        //
        // Order matters: must come AFTER the static archives so ld64
        // sees the unresolved references first, then resolves them
        // from libc++.
        println!("cargo::rustc-link-arg=-lc++");
        return;
    }

    // Linux path: GNU ld supports --wrap, --start-group/--end-group, and
    // -Bstatic/-Bdynamic. We rely on all three.

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
    println!("cargo::rustc-link-arg=-Wl,-Bstatic");
    println!("cargo::rustc-link-arg=-Wl,--start-group");
    println!("cargo::rustc-link-arg={}", lib_dir.join("libphp.a").display());
    for static_lib in linux_static_libs() {
        let archive = lib_dir.join(format!("lib{static_lib}.a"));
        if archive.exists() {
            println!("cargo::rustc-link-arg={}", archive.display());
        }
    }
    println!("cargo::rustc-link-arg=-Wl,--end-group");
    println!("cargo::rustc-link-arg=-Wl,-Bdynamic");
}

/// Static lib set the Linux SDK ships (musl-built).
///
/// Kept in sync with `crates/ephpm-php/build.rs`'s probe list — both
/// paths need to know about the same SDK contents. If you add a lib
/// here, mirror it there (and vice versa).
fn linux_static_libs() -> &'static [&'static str] {
    &[
        "ssl", "crypto", "curl", "z", "xml2", "xslt", "exslt", "lzma", "sodium", "iconv",
        "charset", "intl", "png16", "gd", "jpeg", "freetype", "onig", "zip", "bz2", "gmp",
        "sqlite3", "pq", "pgcommon", "pgport", "edit", "ncurses", "menu", "form", "panel", "tic",
        "icui18n", "icuuc", "icudata", "icuio", "icutu",
        // ImageMagick (imagick extension) + codec chain
        "MagickWand-7.Q16HDRI", "MagickCore-7.Q16HDRI", "Magick++-7.Q16HDRI",
        "heif", "de265", "tiff", "webp", "webpdecoder", "webpdemux", "webpmux", "sharpyuv",
        "aom", "jxl", "jxl_cms", "jxl_threads", "hwy",
        "brotlienc", "brotlidec", "brotlicommon",
        // libstdc++ last: all C++ libs above reference std::* / __cxa_* symbols
        "stdc++",
    ]
}

/// Static lib set the macOS SDK ships.
///
/// macOS's libphp.a bundles some deps differently than Linux's (system
/// libiconv lives in libSystem, libz/libxml2 are system frameworks, etc.).
/// Start with the same list as Linux and tighten as we discover what's
/// actually needed.
fn macos_static_libs() -> &'static [&'static str] {
    linux_static_libs()
}
