use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(php_linked)");
    println!("cargo::rerun-if-changed=wrapper.h");
    println!("cargo::rerun-if-changed=ephpm_wrapper.c");
    println!("cargo::rerun-if-env-changed=PHP_SDK_PATH");

    println!(
        "cargo::warning=ephpm-php build.rs running. PHP_SDK_PATH={:?}",
        env::var_os("PHP_SDK_PATH")
    );

    let Some(sdk_path) = env::var_os("PHP_SDK_PATH").map(PathBuf::from) else {
        // No PHP SDK available — build in stub mode.
        // The Rust code uses #[cfg(php_linked)] to gate FFI calls.
        println!("cargo::warning=PHP_SDK_PATH not set — building in stub mode (no libphp)");
        return;
    };

    let lib_dir = sdk_path.join("lib");
    let include_dir = sdk_path.join("include").join("php");
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    validate_sdk(&lib_dir, &include_dir, &target_os);

    println!("cargo::rustc-cfg=php_linked");

    link_php(&lib_dir, &target_os);
    compile_wrapper(&include_dir, &target_os);
    generate_bindings(&include_dir, &target_os);

    // Note: --wrap flags for zend_signal_* are in the binary crate's
    // build.rs (crates/ephpm/build.rs). rustc-link-arg only works for
    // binary/cdylib targets, not library crates.
}

fn validate_sdk(lib_dir: &Path, include_dir: &Path, target_os: &str) {
    // static-php-cli produces libphp.a on Unix, php8embed.lib on Windows
    let lib_exists = match target_os {
        "windows" => lib_dir.join("php8embed.lib").exists(),
        _ => lib_dir.join("libphp.a").exists(),
    };
    assert!(
        lib_exists,
        "PHP static library not found in {}. Build the PHP SDK first (see CLAUDE.md).",
        lib_dir.display()
    );
    assert!(
        include_dir.exists(),
        "PHP headers not found at {}. Build the PHP SDK first (see CLAUDE.md).",
        include_dir.display()
    );
}

/// Link libphp and its platform-specific system library dependencies.
fn link_php(lib_dir: &Path, target_os: &str) {
    println!("cargo::rustc-link-search=native={}", lib_dir.display());
    if target_os == "windows" {
        // The php-sdk Windows tarball ships a *static* `php8embed.lib`
        // (static-php-cli's `--build-embed` is a static build — a fat lib
        // of PHP's objects, no DLL). Link it statically, exactly as we link
        // `libphp.a` on Linux/macOS. This keeps ephpm a true single static
        // binary on Windows too — no DLL to embed, extract, or delay-load.
        println!("cargo::rustc-link-lib=static=php8embed");
    } else {
        println!("cargo::rustc-link-lib=static=php");
    }

    // libphp depends on system libraries that vary by platform.
    // static-php-cli bundles most deps into libphp.a, but some system
    // libs are still needed for final linking.
    link_system_libs(target_os);

    // Link additional static libraries from the SDK that static-php-cli
    // built. We probe for each library since the set varies by config.
    //
    // Order matters because rustc emits these to the linker in sequence
    // and ld is single-pass. Dependencies must come AFTER the libraries
    // that need them:
    //   - extension libs (curl, xml2, intl, etc.) reference symbols in
    //     the lower-level libs (ssl/crypto, z, icudata, …)
    //   - libicui18n needs libicuuc which needs libicudata
    //   - libstdc++ has to be last on Linux because ICU is C++ — its
    //     archives reference std::__throw_bad_alloc, __cxa_begin_catch,
    //     etc., which only libstdc++.a provides
    //
    // `libz` is emitted with `+whole-archive` because both libphp's
    // `zlib_fopen_wrapper.o` and libxml2's `xmlIO.c.o` reference gz*
    // symbols. Single-pass ld resolves the first set of refs from libz,
    // then won't re-scan libz for the second set — so debug test builds
    // fail with `undefined reference to gzread / gzwrite / gzclose / ...`.
    // (The release build's `--gc-sections` masks this by stripping the
    // unused PHP zlib wrapper.) `whole-archive` forces every object from
    // libz into the link output, so all gz* symbols are present regardless
    // of who references them or when.
    //
    // This is the modern rustc-blessed alternative to wrapping the lib
    // group in `-Wl,--start-group`/`--end-group`, which can't be emitted
    // from a library crate's build.rs (rustc-link-arg only applies to
    // binary/cdylib targets).
    if target_os == "windows" {
        // The Windows SDK ships php8embed.lib's static dependency archives
        // (libcrypto, libssl, libxml2, icu*, libsqlite3, libsodium, ...) but
        // with MSVC-toolchain names that don't follow the Unix `lib<name>.a`
        // convention the probe list below assumes (`libssl.lib` not
        // `ssl.lib`, `libsqlite3_a.lib` not `sqlite3.lib`, `icuin.lib` not
        // `icui18n.lib`, etc.). Rather than maintain a fragile name map,
        // link every `.lib` in the SDK lib dir. MSVC link.exe pulls objects
        // lazily and resolves circular static-lib deps across passes, so
        // order doesn't matter the way it does for single-pass GNU ld.
        link_windows_static_deps(lib_dir);
    } else {
        // Unix: static-php-cli emits `lib<name>.a`; link the known support
        // libs in dependency order (single-pass ld needs deps last).
        println!("cargo::warning=probing for static support libs in {}", lib_dir.display());
        for static_lib in &[
            // High-level extension support libs first; they reference the
            // lower-level libs below.
            "ssl", "crypto", "curl", "z", "xml2", "xslt", "exslt", "lzma", "sodium", "iconv",
            "charset", "intl", "png16", "gd", "jpeg", "freetype", "onig", "zip", "bz2", "gmp",
            "sqlite3",
            // PostgreSQL libs (pdo_pgsql). Order: libpq depends on pgcommon
            // and pgport.
            "pq", "pgcommon", "pgport",
            // Readline / line-editing ecosystem (pulled in by pdo_sqlite and
            // a few CLI-facing extensions on the embed build).
            "edit", "ncurses", "menu", "form", "panel", "tic",
            // ICU — needed by the intl extension. Order matters: i18n →
            // uc → data, with io/tu as auxiliary modules that may pull
            // from any of them.
            "icui18n", "icuuc", "icudata", "icuio", "icutu",
            // libstdc++ last: ICU is C++ and references std::* / __cxa_*
            // symbols that only the C++ runtime provides.
            "stdc++",
        ] {
            let unix_path = lib_dir.join(format!("lib{static_lib}.a"));
            let found = unix_path.exists();
            println!(
                "cargo::warning=probe lib{static_lib}.a at {}: found={found}",
                unix_path.display()
            );
            if found {
                if *static_lib == "z" {
                    println!("cargo::rustc-link-lib=static:+whole-archive={static_lib}");
                } else {
                    println!("cargo::rustc-link-lib=static={static_lib}");
                }
            }
        }
    }
}

/// Link php8embed.lib's static dependency archives from the Windows SDK lib
/// dir. The php-sdk tarball ships PHP's deps with MSVC-toolchain names that
/// don't follow the Unix `lib<name>.a` convention, so we enumerate the dir
/// rather than probe known names. Two subtleties (both diagnosed by dumpbin
/// against the real tarball):
///
///   - **Import stubs.** A few deps ship both a real static archive
///     (`libiconv_a.lib`, ~3.5 MB) and a tiny DLL import stub
///     (`libiconv.lib`, ~3 KB). Linking the stub makes link.exe resolve that
///     lib's *functions* from the DLL thunks, which then blocks it from
///     pulling the matching object out of the static archive — leaving that
///     object's data symbols (e.g. `_libiconv_version`) unresolved. Skip the
///     stubs: when both `<name>.lib` and `<name>_a.lib` exist, drop the
///     bare one. Also skip obvious stubs by size.
///   - **whole-archive for gettext/iconv.** libintl (gettext) and libiconv
///     have a circular dependency and pack the needed symbols into objects
///     link.exe won't pull under its single forward pass. Force every object
///     from those two archives in with `+whole-archive`. (We do NOT
///     whole-archive the giant libs like ICU/crypto — only these two small
///     ones — to keep the binary lean.)
fn link_windows_static_deps(lib_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(lib_dir) else {
        println!(
            "cargo::warning=could not read SDK lib dir {} for Windows dep libs",
            lib_dir.display()
        );
        return;
    };

    // Collect candidate static-archive stems (file name without `.lib`),
    // skipping php8embed (already linked by link_php) and tiny import stubs.
    let mut stems: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("lib") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem.eq_ignore_ascii_case("php8embed") {
            continue;
        }
        // Import stubs are a few KB; real static archives are tens of KB up
        // to hundreds of MB. Drop anything implausibly small for a static lib.
        let size = std::fs::metadata(&path).map_or(0, |m| m.len());
        if size < 16_384 {
            println!("cargo::warning=windows dep lib: skipping import stub {stem} ({size} bytes)");
            continue;
        }
        stems.push(stem.to_string());
    }

    // When both `<name>.lib` and `<name>_a.lib` are present, the bare one is
    // the import stub and `_a` is the static archive — drop the bare name.
    let stem_set: std::collections::HashSet<String> = stems.iter().cloned().collect();
    stems.retain(|s| !stem_set.contains(&format!("{s}_a")));

    stems.sort();

    // libintl (gettext) and libiconv both pack their public symbols into
    // objects that link.exe won't pull under a single lazy forward pass
    // (gettext's circular libintl_* refs; libiconv's iconv functions +
    // the `_libiconv_version` data symbol). Both therefore need
    // whole-archive. The catch: each GNU lib bundles its OWN identical copy
    // of libcharset's `locale_charset`, so whole-archiving both makes that
    // one symbol multiply-defined (LNK2005). There is no per-object exclude
    // for whole-archive, so the binary crate's build.rs passes
    // `/FORCE:MULTIPLE` to keep the first definition — safe here because
    // `locale_charset` is the ONLY duplicate in the entire link and both
    // copies are byte-identical libcharset.
    let whole_archive: [&str; 2] = ["libintl_a", "libiconv_a"];

    for stem in &stems {
        if whole_archive.contains(&stem.as_str()) {
            println!("cargo::warning=windows dep lib (whole-archive): {stem}");
            println!("cargo::rustc-link-lib=static:+whole-archive={stem}");
        } else {
            println!("cargo::warning=windows dep lib: {stem}");
            println!("cargo::rustc-link-lib=static={stem}");
        }
    }
}

fn link_system_libs(target_os: &str) {
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();

    match target_os {
        "linux" if target_env == "musl" => {
            // static-php-cli builds PHP against musl. When we target musl,
            // resolv/dl/m/pthread/rt are all part of musl's libc — no
            // separate linking is needed. Rust links musl libc statically.
            //
            // PHP's JIT (opcache) uses GCC CPU feature detection builtins
            // (__cpu_indicator_init, __cpu_model, __cpu_features2) for AVX
            // and CLDEMOTE checks. These live in libgcc.a from the musl
            // cross-compiler toolchain.
            if let Some(gcc_dir) = find_musl_libgcc() {
                println!("cargo::rustc-link-search=native={}", gcc_dir.display());
            } else {
                println!(
                    "cargo::warning=Could not find libgcc.a for musl target. \
                     Install a musl cross-compiler (e.g. `apt install musl-tools`). \
                     The linker may fail with \
                     'could not find native static library `gcc`'."
                );
            }
            println!("cargo::rustc-link-lib=static=gcc");
        }
        "linux" => {
            for lib in &["resolv", "dl", "m", "pthread", "rt"] {
                println!("cargo::rustc-link-lib=dylib={lib}");
            }
        }
        "macos" => {
            // macOS bundles dl, pthread, rt into libSystem (always present).
            // resolv is a separate dylib. iconv is handled by the static
            // probe (static-php-cli bundles libiconv.a).
            for lib in &["resolv", "m"] {
                println!("cargo::rustc-link-lib=dylib={lib}");
            }
            println!("cargo::rustc-link-lib=framework=SystemConfiguration");
        }
        "windows" => {
            for lib in &["ws2_32", "crypt32", "advapi32", "bcrypt", "userenv"] {
                println!("cargo::rustc-link-lib=dylib={lib}");
            }
        }
        other => {
            println!(
                "cargo::warning=Unknown target OS '{other}' — system library linking may fail"
            );
            for lib in &["m", "pthread"] {
                println!("cargo::rustc-link-lib=dylib={lib}");
            }
        }
    }
}

/// Compile the C wrapper that provides `zend_try`/`zend_catch` guards.
fn compile_wrapper(include_dir: &Path, target_os: &str) {
    let include_main = include_dir.join("main");
    let include_zend = include_dir.join("Zend");
    let include_tsrm = include_dir.join("TSRM");
    let include_sapi = include_dir.join("sapi");

    let mut build = cc::Build::new();
    build
        .file("ephpm_wrapper.c")
        // Bundles the __cpu_indicator_init_local → __cpu_indicator_init
        // thunk for GCC 13.x hosts linking against an SDK compiled with
        // GCC 14+. See cpu_compat.c for the full rationale.
        .file("cpu_compat.c")
        .include(include_dir)
        .include(&include_main)
        .include(&include_zend)
        .include(&include_tsrm)
        .include(&include_sapi);

    // PHP headers use GNU extensions (memrchr, mempcpy). Define _GNU_SOURCE
    // on every Unix target so the libc headers expose the declarations.
    //
    // musl always needs it. glibc DOES too on an older baseline: glibc 2.28
    // (manylinux_2_28 / RHEL8) does not expose mempcpy/memrchr at the default
    // feature-test level, so zend_operators.h fails to compile with
    // "implicit declaration of function 'mempcpy'". Newer glibc (2.39 on
    // ubuntu:24.04) happened to leak them without _GNU_SOURCE, which hid the
    // gap while the build environment was Ubuntu 24.04. Defining it
    // unconditionally on Unix keeps the wrapper compiling against the
    // glibc-2.28 floor and is harmless on newer glibc/musl.
    if target_os != "windows" {
        build.define("_GNU_SOURCE", None);
    }

    // ZTS (Zend Thread Safety) — enables thread-local storage in PHP headers.
    // All non-Windows builds use ZTS=1 for concurrent PHP execution.
    // Windows uses NTS (ZTS=0) because the Windows PHP DLL is NTS.
    if target_os == "windows" {
        build.define("ZEND_WIN32", None);
        build.define("PHP_WIN32", None);
        build.define("ZEND_DEBUG", Some("0"));
        build.define("ZTS", Some("0"));
    } else {
        build.define("ZTS", Some("1"));
        build.define("ZEND_ENABLE_STATIC_TSRMLS_CACHE", Some("1"));
    }

    build.compile("ephpm_wrapper");
}

/// Generate Rust FFI bindings from PHP headers via bindgen.
fn generate_bindings(include_dir: &Path, target_os: &str) {
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let include_main = include_dir.join("main");
    let include_zend = include_dir.join("Zend");
    let include_tsrm = include_dir.join("TSRM");
    let include_sapi = include_dir.join("sapi");

    let mut builder = bindgen::Builder::default().header("wrapper.h").clang_args([
        format!("-I{}", include_dir.display()),
        format!("-I{}", include_main.display()),
        format!("-I{}", include_zend.display()),
        format!("-I{}", include_tsrm.display()),
        format!("-I{}", include_sapi.display()),
    ]);

    // When targeting musl, libclang defaults to glibc headers in /usr/include/
    // which then fail to find stddef.h. We need to:
    //  1. Suppress default system includes (-nostdlibinc)
    //  2. Add musl headers explicitly (-isystem)
    //  3. Add clang's internal headers for stddef.h, stdarg.h, etc.
    //  4. Define _GNU_SOURCE for memrchr/mempcpy declarations
    if target_env == "musl" {
        builder = builder.clang_arg("-D_GNU_SOURCE").clang_arg("-nostdlibinc");

        // Add clang's resource directory (contains stddef.h, stdarg.h, etc.)
        if let Some(clang_include) = find_clang_resource_include() {
            builder = builder.clang_arg(format!("-isystem{}", clang_include.display()));
        }

        // Add musl libc headers
        let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "x86_64".into());
        let musl_include = format!("/usr/include/{arch}-linux-musl");
        if Path::new(&musl_include).exists() {
            builder = builder.clang_arg(format!("-isystem{musl_include}"));
        }
    }

    // Platform-specific bindgen defines must match what compile_wrapper
    // passes to cc::Build, or bindgen sees the wrong branch of every
    // platform #ifdef in PHP's headers (and reports "unknown type
    // sigjmp_buf" / "'syslog.h' not found" because it's parsing the
    // Unix branches with no Unix headers present).
    if target_os == "windows" {
        builder = builder
            .clang_arg("--target=x86_64-pc-windows-msvc")
            .clang_arg("-DZEND_WIN32")
            .clang_arg("-DPHP_WIN32")
            .clang_arg("-DZEND_DEBUG=0")
            .clang_arg("-DZTS=0")
            // MSVC's <stddef.h> does not define C11 `max_align_t` — a real,
            // long-standing gap in the Windows CRT (it's missing even under
            // /std:c17 on current Windows SDKs). zend_portability.h's
            // `#if __STDC_VERSION__ >= 201112L` branch then does
            // `typedef max_align_t zend_max_align_t` and bindgen fails with
            // "unknown type name 'max_align_t'".
            //
            // PHP's real Windows build compiles with cl.exe, whose C mode is
            // pre-C11 by default, so it takes zend_portability.h's *fallback*
            // union branch and never references max_align_t. We make bindgen's
            // libclang parse succeed the same way by defining max_align_t to
            // `double` — the exact type MSVC uses for max_align_t when it does
            // define it, and 8-byte aligned on x64 (identical to both the
            // fallback union and the real ABI). `zend_max_align_t` is not in
            // ephpm's allowlisted binding surface, so this only needs to parse.
            //
            // We do NOT pass `-resource-dir` to point at clang's own stddef.h:
            // with the MSVC headers forwarded as -isystem, -resource-dir takes
            // priority over the /imsvc search and breaks system-header
            // resolution rather than helping (it also did not resolve
            // max_align_t in practice — see nightly 27326138898).
            .clang_arg("-Dmax_align_t=double")
            // PHP 8.5+ added overflow-checked integer arithmetic in
            // zend_operators.h. On Windows it calls the intsafe.h intrinsics
            // LongLongAdd / LongLongSub, which MSVC's cl.exe has but bindgen's
            // libclang does not — so bindgen fails with "call to undeclared
            // function 'LongLongAdd'". Define the PHP_HAVE_BUILTIN_*_OVERFLOW
            // macros so zend_operators.h takes its clang `__builtin_*_overflow`
            // branch instead (the path PHP itself uses for clang on Windows,
            // php-src#17472). Harmless on 8.4, which has no such code. These
            // are inline helpers, not part of ephpm's allowlisted binding
            // surface, so they only need to parse — the cl.exe wrapper compile
            // is unaffected and keeps using the intsafe path.
            .clang_arg("-DPHP_HAVE_BUILTIN_SADDL_OVERFLOW=1")
            .clang_arg("-DPHP_HAVE_BUILTIN_SADDLL_OVERFLOW=1")
            .clang_arg("-DPHP_HAVE_BUILTIN_SSUBL_OVERFLOW=1")
            .clang_arg("-DPHP_HAVE_BUILTIN_SSUBLL_OVERFLOW=1")
            .clang_arg("-DPHP_HAVE_BUILTIN_SMULL_OVERFLOW=1")
            .clang_arg("-DPHP_HAVE_BUILTIN_SMULLL_OVERFLOW=1")
            // Force-include <intrin.h> for the bindgen parse. PHP 8.3's
            // zend_call_stack.h calls the MSVC intrinsic
            // `_AddressOfReturnAddress()` without including <intrin.h>; cl.exe
            // knows it implicitly, but bindgen's libclang needs the
            // declaration and fails with "call to undeclared library function
            // '_AddressOfReturnAddress'". PHP 8.4 fixed the header to include
            // <intrin.h> itself (under `#ifdef _MSC_VER`); force-including it
            // here makes 8.3 behave the same. Harmless and redundant on
            // 8.4/8.5 (their header already includes it, and `<intrin.h>` is
            // include-guarded) — and bindgen already parses intrin.h cleanly
            // there, so this adds no new risk.
            .clang_arg("-include")
            .clang_arg("intrin.h");

        // bindgen runs libclang directly and does NOT consume the MSVC
        // INCLUDE env var (cl.exe does, libclang doesn't). Without this,
        // system headers like intsafe.h and windows.h fail to resolve
        // even though they're on disk. Read INCLUDE (set by vcvars64.bat
        // in the workflow) and forward each path to clang as -isystem.
        //
        // Two normalizations matter:
        //   1. vcvars64 emits some path segments with doubled backslashes
        //      (`\\include\\10.0.x\\um`). clang on Windows treats `\\` in
        //      -isystem paths inconsistently and ends up not finding the
        //      headers. Collapse `\\` -> `\` first.
        //   2. Convert all `\` -> `/`. clang accepts forward slashes on
        //      Windows everywhere and they avoid any escape-sequence
        //      ambiguity in the argv string.
        match env::var("INCLUDE") {
            Ok(include) => {
                let paths: Vec<String> = include
                    .split(';')
                    .filter(|p| !p.is_empty())
                    .map(|p| p.replace("\\\\", "\\").replace('\\', "/"))
                    .collect();
                println!(
                    "cargo::warning=bindgen: forwarding {} INCLUDE paths to clang",
                    paths.len()
                );
                for path in paths {
                    println!("cargo::warning=bindgen: -isystem{path}");
                    builder = builder.clang_arg(format!("-isystem{path}"));
                }
            }
            Err(_) => {
                println!(
                    "cargo::warning=bindgen: INCLUDE env var not set; \
                     Windows SDK headers won't resolve"
                );
            }
        }
    } else {
        // ZTS builds: define ZTS for bindgen so PHP headers use thread-safe macros.
        builder = builder.clang_arg("-DZTS=1").clang_arg("-DZEND_ENABLE_STATIC_TSRMLS_CACHE=1");
    }

    let bindings = builder
        // PHP embedding functions
        .allowlist_function("php_embed_init")
        .allowlist_function("php_embed_shutdown")
        .allowlist_function("php_request_startup")
        .allowlist_function("php_request_shutdown")
        .allowlist_function("php_execute_script")
        .allowlist_function("php_register_variable_safe")
        .allowlist_function("sapi_startup")
        .allowlist_function("sapi_shutdown")
        .allowlist_function("sapi_activate")
        .allowlist_function("sapi_deactivate")
        .allowlist_function("php_module_startup")
        .allowlist_function("php_module_shutdown")
        .allowlist_function("zend_eval_string")
        .allowlist_function("zend_stream_init_filename")
        // TSRM (Thread Safe Resource Manager) functions — ZTS builds
        .allowlist_function("tsrm_startup")
        .allowlist_function("tsrm_shutdown")
        .allowlist_function("ts_resource_ex")
        .allowlist_function("tsrm_set_new_thread_begin_handler")
        .allowlist_function("tsrm_set_new_thread_end_handler")
        // SAPI module struct and related types
        .allowlist_type("sapi_module_struct")
        .allowlist_type("sapi_header_struct")
        .allowlist_type("sapi_headers_struct")
        .allowlist_type("sapi_request_info")
        .allowlist_type("sapi_globals_struct")
        .allowlist_type("zend_file_handle")
        .allowlist_type("zval")
        // Variables
        .allowlist_var("sapi_module")
        .allowlist_var("SG")
        .derive_debug(true)
        .derive_default(true)
        .layout_tests(false)
        .generate()
        .expect("failed to generate PHP bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings.write_to_file(out_path.join("php_bindings.rs")).expect("failed to write PHP bindings");
}

/// Find clang's resource directory containing compiler-internal headers
/// (stddef.h, stdarg.h, float.h, etc.).
///
/// Tries `clang --print-resource-dir` first, then probes common LLVM
/// install locations on Debian/Ubuntu.
fn find_clang_resource_include() -> Option<PathBuf> {
    // Try clang --print-resource-dir
    if let Ok(output) = Command::new("clang").arg("--print-resource-dir").output() {
        if output.status.success() {
            let dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let include = PathBuf::from(&dir).join("include");
            if include.join("stddef.h").exists() {
                return Some(include);
            }
        }
    }

    // Probe common LLVM locations (Debian/Ubuntu: /usr/lib/llvm-<ver>/lib/clang/<ver>/include/)
    for version in (11..=20).rev() {
        let base = PathBuf::from(format!("/usr/lib/llvm-{version}/lib/clang"));
        if let Ok(entries) = std::fs::read_dir(&base) {
            for entry in entries.flatten() {
                let include = entry.path().join("include");
                if include.join("stddef.h").exists() {
                    return Some(include);
                }
            }
        }
    }

    None
}

/// Find the directory containing `libgcc.a` for musl target builds.
///
/// `apt install musl-tools` provides the `musl-gcc` wrapper around the host
/// GCC; the libgcc.a symbols PHP's JIT needs live there. We add that lib
/// directory to the linker search path so `-lgcc` resolves.
fn find_musl_libgcc() -> Option<PathBuf> {
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "x86_64".into());
    let musl_triple = format!("{arch}-linux-musl");
    // On Ubuntu/Debian, `musl-tools` provides a `musl-gcc` wrapper around the
    // host GCC. The symbols PHP's JIT needs (__cpu_indicator_init, __cpu_model)
    // live in the *host* GCC's libgcc.a, not in a musl-specific copy.
    let gnu_triple = format!("{arch}-linux-gnu");

    // Common locations where musl-cross or host toolchains install libgcc.a:
    //   /usr/lib/gcc/<musl-triple>/<ver>/libgcc.a   (musl cross-compiler)
    //   /usr/lib/gcc/<gnu-triple>/<ver>/libgcc.a    (host GCC via musl-tools wrapper)
    let search_roots = [
        PathBuf::from(format!("/usr/lib/gcc/{musl_triple}")),
        PathBuf::from(format!("/usr/lib/gcc/{gnu_triple}")),
    ];

    for root in &search_roots {
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                let candidate = entry.path().join("libgcc.a");
                if candidate.exists() {
                    return Some(entry.path());
                }
            }
        }
    }

    None
}
