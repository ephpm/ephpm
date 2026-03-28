use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(php_linked)");
    println!("cargo::rerun-if-changed=wrapper.h");
    println!("cargo::rerun-if-changed=ephpm_wrapper.c");
    println!("cargo::rerun-if-env-changed=PHP_SDK_PATH");

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
    embed_windows_dll(&lib_dir, &target_os);
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
        // Windows PHP ships as php8embed.dll + import lib (.lib).
        // Use dylib linkage so the import lib resolves to the DLL at runtime.
        println!("cargo::rustc-link-lib=dylib=php8embed");
    } else {
        println!("cargo::rustc-link-lib=static=php");
    }

    // libphp depends on system libraries that vary by platform.
    // static-php-cli bundles most deps into libphp.a, but some system
    // libs are still needed for final linking.
    link_system_libs(target_os);

    // Link additional static libraries from the SDK that static-php-cli
    // built. We probe for each library since the set varies by config.
    for static_lib in &[
        "ssl", "crypto", "curl", "z", "xml2", "sodium", "iconv", "charset",
        "png16", "gd", "jpeg", "freetype", "onig", "zip", "bz2", "xslt", "exslt",
    ] {
        // Unix uses libfoo.a, Windows uses foo.lib
        let found = lib_dir.join(format!("lib{static_lib}.a")).exists()
            || lib_dir.join(format!("{static_lib}.lib")).exists();
        if found {
            println!("cargo::rustc-link-lib=static={static_lib}");
        }
    }
}

/// Copy `php8embed.dll` to `OUT_DIR` and set `PHP_EMBED_DLL_PATH` so that
/// `include_bytes!(env!("PHP_EMBED_DLL_PATH"))` in `windows_dll.rs` works.
///
/// Also links `delayimp.lib`, which is required by the MSVC linker when any
/// DLL is compiled with `/DELAYLOAD` (the flag itself is set in the binary
/// crate's `build.rs` because `rustc-link-arg` only takes effect there).
fn embed_windows_dll(lib_dir: &Path, target_os: &str) {
    if target_os != "windows" {
        return;
    }
    let dll_src = lib_dir.join("php8embed.dll");
    if !dll_src.exists() {
        // Validation already happened in validate_sdk for the .lib file.
        // The DLL should always accompany the import lib — warn and continue.
        println!(
            "cargo::warning=php8embed.dll not found in {} — DLL embedding skipped",
            lib_dir.display()
        );
        return;
    }
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let dll_dest = out_dir.join("php8embed.dll");
    std::fs::copy(&dll_src, &dll_dest).expect("failed to copy php8embed.dll to OUT_DIR");

    // Expose the absolute path so windows_dll.rs can use include_bytes!.
    println!("cargo::rustc-env=PHP_EMBED_DLL_PATH={}", dll_dest.display());
    // Rebuild if the source DLL changes.
    println!("cargo::rerun-if-changed={}", dll_src.display());

    // delayimp.lib is the MSVC runtime support library required whenever
    // /DELAYLOAD is used. It resolves __delayLoadHelper2 etc.
    println!("cargo::rustc-link-lib=delayimp");
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
                     Install a musl cross-compiler (e.g. `apt install musl-tools`) \
                     or run `spc doctor --auto-fix`. The linker may fail with \
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
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let include_main = include_dir.join("main");
    let include_zend = include_dir.join("Zend");
    let include_tsrm = include_dir.join("TSRM");
    let include_sapi = include_dir.join("sapi");

    let mut build = cc::Build::new();
    build
        .file("ephpm_wrapper.c")
        .include(include_dir)
        .include(&include_main)
        .include(&include_zend)
        .include(&include_tsrm)
        .include(&include_sapi);

    // PHP headers use GNU extensions (memrchr, mempcpy). Define _GNU_SOURCE
    // so musl's headers expose the declarations and suppress warnings.
    if target_env == "musl" {
        build.define("_GNU_SOURCE", None);
    }

    // PHP headers check these defines to select Windows-specific code paths.
    if target_os == "windows" {
        build.define("ZEND_WIN32", None);
        build.define("PHP_WIN32", None);
        build.define("ZEND_DEBUG", Some("0"));
        build.define("ZTS", Some("0"));
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

    let mut builder = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_args([
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
        builder = builder
            .clang_arg("-D_GNU_SOURCE")
            .clang_arg("-nostdlibinc");

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

    // When cross-compiling for Windows from Linux, tell bindgen to generate
    // bindings for the target platform instead of the host.
    if target_os == "windows" {
        builder = builder.clang_arg("--target=x86_64-pc-windows-msvc");
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
    bindings
        .write_to_file(out_path.join("php_bindings.rs"))
        .expect("failed to write PHP bindings");
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

/// Find the directory containing `libgcc.a` from the musl cross-compiler.
///
/// `spc doctor --auto-fix` installs a musl GCC toolchain (e.g. under
/// `/usr/local/musl/`). We need to add its lib directory to the linker
/// search path so `-lgcc` resolves.
fn find_musl_libgcc() -> Option<PathBuf> {
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "x86_64".into());
    let musl_triple = format!("{arch}-linux-musl");
    // On Ubuntu/Debian, `musl-tools` provides a `musl-gcc` wrapper around the
    // host GCC. The symbols PHP's JIT needs (__cpu_indicator_init, __cpu_model)
    // live in the *host* GCC's libgcc.a, not in a musl-specific copy.
    let gnu_triple = format!("{arch}-linux-gnu");

    // Common locations where musl-cross or host toolchains install libgcc.a:
    //   /usr/local/musl/lib/gcc/<triple>/<ver>/libgcc.a  (spc doctor)
    //   /usr/lib/gcc/<musl-triple>/<ver>/libgcc.a         (musl cross-compiler)
    //   /usr/lib/gcc/<gnu-triple>/<ver>/libgcc.a          (host GCC via musl-tools wrapper)
    let search_roots = [
        PathBuf::from(format!("/usr/local/musl/lib/gcc/{musl_triple}")),
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
