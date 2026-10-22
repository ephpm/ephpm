use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(php_linked)");
    // Set when the linked libphp was built with --enable-zend-max-execution-timers
    // (per-thread POSIX timers). This is the ZTS-safe timeout mechanism ePHPm
    // relies on to enforce `max_execution_time` natively; when absent (macOS,
    // older SDKs) PHP falls back to the process-wide setitimer/SIGPROF timer,
    // which is unsafe on tokio worker threads, so the feature stays disabled.
    println!("cargo::rustc-check-cfg=cfg(php_max_exec_timers)");
    // Set when the SDK is the Windows shared-engine variant
    // (`php-sdk-<ver>-windows-x86_64-shared`): PHP lives in php8ts.dll next to
    // the executable instead of being linked in, so extension DLLs (Xdebug)
    // can load. See `link_php` and `src/shared.rs`.
    println!("cargo::rustc-check-cfg=cfg(php_shared)");
    // Set when the linked engine has PHP's hard memory limit (see below).
    println!("cargo::rustc-check-cfg=cfg(php_hard_memory_limit)");
    println!("cargo::rerun-if-changed=wrapper.h");
    println!("cargo::rerun-if-changed=ephpm_wrapper.c");
    println!("cargo::rerun-if-changed=resolver_shim.c");
    println!("cargo::rerun-if-changed=crash_guard.c");
    println!("cargo::rerun-if-env-changed=PHP_SDK_PATH");

    // The ePHPm build version, for $_SERVER['SERVER_SOFTWARE']. Resolved
    // exactly like crates/ephpm/build.rs resolves `ephpm --version`: the
    // release tag (EPHPM_RELEASE_VERSION, set by the release workflows from
    // the git tag with the leading "v" stripped) when present, otherwise
    // CARGO_PKG_VERSION. Emitted before the stub-mode early return so stub
    // builds and tests see the same value.
    println!("cargo::rerun-if-env-changed=EPHPM_RELEASE_VERSION");
    let version = env::var("EPHPM_RELEASE_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| env::var("CARGO_PKG_VERSION").unwrap_or_default());
    println!("cargo::rustc-env=EPHPM_VERSION={version}");

    println!(
        "cargo::warning=ephpm-php build.rs running. PHP_SDK_PATH={:?}",
        env::var_os("PHP_SDK_PATH")
    );

    // The stack-overflow crash-containment shim is pure libc (no PHP headers),
    // so it is compiled on every Unix build — stub AND php_linked — which lets
    // the mechanism be unit-tested without libphp. It is inert at runtime until
    // `crash_guard::set_enabled(true)`.
    let target_family = env::var("CARGO_CFG_TARGET_FAMILY").unwrap_or_default();
    if target_family == "unix" {
        cc::Build::new()
            .file("crash_guard.c")
            .define("_GNU_SOURCE", None)
            .compile("ephpm_crash_guard");
    }

    // PHP's hard memory limit ends in `_exit(137)`; crash_guard.c turns that
    // into a per-thread containment on Linux, but only in a target linked with
    // `--wrap=_exit`. The `ephpm` binary gets the flag from its own build
    // script: rustc-link-arg reaches only the linked targets (tests, benches,
    // examples) of the package that emits it, never a dependent crate. This
    // gives this crate's own test targets the same link, stub mode included,
    // so the mechanism is unit-tested without libphp and the linked tests can
    // trigger a real hard-limit exit without ending the test process. (Not
    // `rustc-link-arg-tests`: that one skips the lib's unit-test binary, where
    // the stub-mode tests of the mechanism live.)
    //
    // The flag reaches every test binary of this package, including ones that
    // never link the ephpm_php rlib (an integration test whose whole file is
    // `#![cfg(php_linked)]` is empty in stub mode). Those still reference
    // `_exit` through std, so the wrap leaves `__wrap__exit` undefined unless
    // the crash-guard archive is on their link line too. It is pure libc, so
    // naming it directly is safe in every binary; where the rlib already
    // brings it in, the member is extracted only once.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo::rustc-link-arg=-Wl,--wrap=_exit");
        let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set for build scripts"));
        println!("cargo::rustc-link-arg={}", out_dir.join("libephpm_crash_guard.a").display());
    }

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

    // Detect the per-thread execution-timer mechanism in the linked SDK. On a
    // ZTS Linux libphp built with `--enable-zend-max-execution-timers`, PHP's
    // php_config.h carries `#define ZEND_MAX_EXECUTION_TIMERS 1`. That build
    // arms a per-thread POSIX timer (timer_create/SIGEV_THREAD_ID + SIGRTMIN,
    // CLOCK_BOOTTIME) instead of the process-wide setitimer/SIGPROF timer, so
    // the timeout signal lands on the exact PHP execution thread and never on
    // an unrelated (e.g. tokio) thread.
    let has_exec_timers = php_config_has_max_exec_timers(&include_dir);
    if has_exec_timers {
        println!("cargo::rustc-cfg=php_max_exec_timers");
    }

    // Set when the SDK's engine has the hard memory limit (PHP 8.3.36 /
    // 8.4.27 / 8.5.12+, php-src 31c0ea7d7f): `memory_limit` is then soft and an
    // allocation past `memory_hard_limit_percent` calls `_exit(137)`. Detected
    // from the header, not the version number, so a patched dev build counts.
    // Lets the linked tests assert the contained outcome on a patched SDK and
    // the plain bailout on an older one.
    let zend_alloc_h = include_dir.join("Zend").join("zend_alloc.h");
    println!("cargo::rerun-if-changed={}", zend_alloc_h.display());
    if std::fs::read_to_string(&zend_alloc_h)
        .is_ok_and(|text| text.contains("zend_set_memory_hard_limit_percent"))
    {
        println!("cargo::rustc-cfg=php_hard_memory_limit");
    }

    let shared = is_shared_sdk(&lib_dir, &target_os);
    if shared {
        println!("cargo::rustc-cfg=php_shared");
        emit_shared_preload(&sdk_path);
    }

    link_php(&lib_dir, &target_os, shared);
    compile_wrapper(&include_dir, &target_os, has_exec_timers, shared);
    generate_bindings(&include_dir, &target_os, shared);

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

/// Whether the SDK is the Windows shared-engine variant.
///
/// Decided from the SDK's contents, never from an env var, so a build is
/// always self-describing: only the shared variant ships `php8ts.lib` (the
/// import library for `php8ts.dll`). The static variant's `php8embed.lib`
/// carries the whole engine and there is no `php8ts.*` in it at all.
fn is_shared_sdk(lib_dir: &Path, target_os: &str) -> bool {
    let import_lib = lib_dir.join("php8ts.lib");
    let shared = target_os == "windows" && import_lib.exists();
    // Only watch the file when it exists: Cargo treats a missing
    // rerun-if-changed path as always stale, which would rerun this script
    // (bindgen + the C wrapper) on every build with every other SDK. A
    // different SDK variant means a different PHP_SDK_PATH anyway.
    if shared {
        println!("cargo::rerun-if-changed={}", import_lib.display());
    }
    shared
}

/// Bake the shared SDK's extension preload list into the crate as
/// `EPHPM_PHP_PRELOAD`.
///
/// The official build compiles fewer extensions into `php8ts.dll` than the
/// static SDK has (mbstring, openssl, curl, intl, pdo_mysql, ... are
/// `ext\php_*.dll`). `ephpm-sdk.json`'s `preload` array lists them in a
/// dependency-safe load order; ePHPm loads them at startup so a shared build
/// has the same extension set as a static one (`src/shared.rs`). Encoded as
/// `e:<file>` / `z:<file>` (extension / `zend_extension`) joined by `;` so
/// the runtime side needs no JSON parser for a compile-time constant.
///
/// A shared SDK without the manifest fails the build: linking it anyway would
/// produce a binary that silently lacks a dozen extensions.
fn emit_shared_preload(sdk_path: &Path) {
    let manifest = sdk_path.join("ephpm-sdk.json");
    println!("cargo::rerun-if-changed={}", manifest.display());
    let text = std::fs::read_to_string(&manifest).unwrap_or_else(|e| {
        panic!(
            "{} is a shared-engine Windows SDK (lib/php8ts.lib) but has no readable \
             ephpm-sdk.json ({e}). Use the php-sdk `-windows-x86_64-shared` tarball \
             (`cargo xtask php-sdk --target windows --variant shared`).",
            sdk_path.display()
        )
    });
    let json: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{}: invalid JSON: {e}", manifest.display()));
    assert_eq!(
        json.get("linkage").and_then(serde_json::Value::as_str),
        Some("shared"),
        "{}: expected \"linkage\": \"shared\"",
        manifest.display()
    );
    let preload = json
        .get("preload")
        .and_then(serde_json::Value::as_array)
        .unwrap_or_else(|| panic!("{}: missing \"preload\" array", manifest.display()));
    let mut encoded = Vec::with_capacity(preload.len());
    for entry in preload {
        let file = entry
            .get("file")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("{}: preload entry without \"file\"", manifest.display()));
        // Baked into a `cargo::rustc-env` line: a line break would inject a
        // cargo directive, so accept only plain file-name characters.
        assert!(
            !file.is_empty()
                && !file.starts_with('.')
                && file.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')),
            "{}: preload file {file:?} must be a plain file name",
            manifest.display()
        );
        let zend =
            entry.get("zend_extension").and_then(serde_json::Value::as_bool).unwrap_or(false);
        encoded.push(format!("{}:{file}", if zend { 'z' } else { 'e' }));
    }
    println!("cargo::rustc-env=EPHPM_PHP_PRELOAD={}", encoded.join(";"));
}

/// Link libphp and its platform-specific system library dependencies.
fn link_php(lib_dir: &Path, target_os: &str, shared: bool) {
    println!("cargo::rustc-link-search=native={}", lib_dir.display());
    if shared {
        // Windows shared-engine SDK (official windows.php.net build). Its
        // php8embed.lib is a small *static* lib holding only the embed SAPI;
        // its objects import the engine from php8ts.dll through the
        // php8ts.lib import library. The engine's own dependencies
        // (OpenSSL, ICU, ...) are DLLs beside php8ts.dll, not link inputs,
        // so there are no dependency archives to link. The runtime DLLs ship
        // next to ephpm.exe (`cargo xtask release --variant shared`).
        println!("cargo::rustc-link-lib=static=php8embed");
        println!("cargo::rustc-link-lib=dylib=php8ts");
        link_system_libs(target_os);
        return;
    }
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
        if target_os == "macos" {
            // The macOS SDK's ICU (and ext/intl's C++ objects) are built
            // against Apple's libc++, which the SDK does not ship — php-config
            // `--libs` ends in `-lc++`. Emitted here, from the crate that
            // links libphp, so it reaches every target that links this crate:
            // the `ephpm` binary also passes `-lc++` from its own build.rs, but
            // rustc-link-arg never reaches a dependent package's targets, so
            // ephpm-server's linked test binaries (`http3_e2e` first) failed
            // with undefined `std::*` / `___cxa_*` / `___gxx_personality_v0`.
            println!("cargo::rustc-link-lib=dylib=c++");
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

    // THIS crate's test binaries link php8embed directly, without the ephpm
    // binary crate's build.rs in the loop — so they need the same
    // /FORCE:MULTIPLE the binary crate passes for the one known duplicate
    // (`locale_charset`, byte-identical libcharset copies bundled by both
    // whole-archived GNU libs above; see the whole_archive comment).
    // `rustc-link-arg-tests` scopes it to test targets only.
    println!("cargo::rustc-link-arg-tests=/FORCE:MULTIPLE");
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

/// Return `true` when the SDK's `php_config.h` declares
/// `ZEND_MAX_EXECUTION_TIMERS` — i.e. libphp was built with per-thread POSIX
/// execution timers (`--enable-zend-max-execution-timers`).
///
/// This is the single source of truth for whether ePHPm can enforce
/// `max_execution_time` natively. The binary crate's `build.rs` runs the same
/// probe to decide whether to `--wrap` (neuter) `zend_set_timeout`.
fn php_config_has_max_exec_timers(include_dir: &Path) -> bool {
    let cfg = include_dir.join("main").join("php_config.h");
    println!("cargo::rerun-if-changed={}", cfg.display());
    let Ok(text) = std::fs::read_to_string(&cfg) else {
        println!(
            "cargo::warning=could not read {} to detect ZEND_MAX_EXECUTION_TIMERS; \
             native max_execution_time enforcement disabled",
            cfg.display()
        );
        return false;
    };
    // Match a real `#define ZEND_MAX_EXECUTION_TIMERS 1` line (not a 0/comment).
    text.lines().any(|line| {
        let l = line.trim_start();
        l.starts_with("#define")
            && l.contains("ZEND_MAX_EXECUTION_TIMERS")
            && l.split_whitespace().last() == Some("1")
    })
}

/// Compile the C wrapper that provides `zend_try`/`zend_catch` guards.
fn compile_wrapper(include_dir: &Path, target_os: &str, has_exec_timers: bool, shared: bool) {
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

    // ext/standard/dns.c inside libphp.a references the underscored resolver
    // names (__dn_expand, __dn_skipname, __res_nsearch). Ubuntu 22.04+
    // (glibc 2.35+) drops the unversioned compat aliases; newer linkers
    // (rust-lld, GNU ld ≥ 2.38) won't silently match the unversioned refs to
    // the versioned `__dn_expand@GLIBC_2.2.5`. resolver_shim.c provides
    // tail-call wrappers on Linux only.
    if target_os == "linux" {
        build.file("resolver_shim.c");
    }

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

    // ZTS (Zend Thread Safety) — enables TSRM-based globals access in PHP
    // headers. EVERY platform is ZTS: the php-sdk's Windows `php8embed.lib`
    // is a ZTS build just like the Unix `libphp.a` (verified at the artifact
    // level, issue #326: it exports `tsrm_startup`/`ts_resource_ex`,
    // `executor_globals_id`/`_offset` and `_tsrm_ls_cache`, and has NO
    // NTS-style direct `executor_globals`/`core_globals` data symbols;
    // `ephpm php -v` reports ZTS and ZEND_THREAD_SAFE=1).
    //
    // History note: this used to pass `ZTS=0` on Windows on the (wrong)
    // belief the Windows lib was NTS. PHP's headers and ephpm_wrapper.c test
    // `#ifdef ZTS` — never `#if ZTS` — so `ZTS=0` still *defined* ZTS and
    // compiled every ZTS path; the Windows build was always ZTS-coherent
    // by accident. `ZTS=1` states the truth. Never test `#if ZTS` in new
    // code, and never "disable" ZTS by defining it to 0.
    //
    // ZEND_ENABLE_STATIC_TSRMLS_CACHE makes EG()/CG()/SG() resolve through
    // the extern thread-local `_tsrm_ls_cache` (which libphp defines,
    // exports, and updates in `ts_resource`) instead of a
    // `tsrm_get_ls_cache()` call per access — same as the Unix build. Safe
    // on Windows because php8embed.lib is linked statically into the same
    // module (no cross-DLL __declspec(thread) import involved).
    //
    // The Windows shared-engine SDK is the exception: there the engine is in
    // php8ts.dll, and a thread-local cannot be imported across a DLL
    // boundary, so the wrapper must resolve the TSRM cache through the
    // exported `tsrm_get_ls_cache()` instead (exactly what an extension DLL
    // built without the static cache does). The cost is a function call per
    // EG()/SG() access in the wrapper only; the engine keeps its own cache.
    build.define("ZTS", Some("1"));
    if shared {
        // Lets the wrapper tell the shared engine apart, e.g. SQLite lives in
        // libsqlite3.dll beside php_pdo_sqlite.dll there, not in this link,
        // even though the engine's config.w32.h defines HAVE_SQLITE3.
        build.define("EPHPM_PHP_SHARED", Some("1"));
    } else {
        build.define("ZEND_ENABLE_STATIC_TSRMLS_CACHE", Some("1"));
    }
    if target_os == "windows" {
        build.define("ZEND_WIN32", None);
        build.define("PHP_WIN32", None);
        build.define("ZEND_DEBUG", Some("0"));
    }

    // When the SDK has per-thread execution timers, ePHPm lets PHP arm its own
    // timeout (see the binary crate's build.rs, which then does NOT --wrap
    // zend_set_timeout). In that mode the __wrap_zend_set_timeout /
    // __wrap_zend_unset_timeout no-op stubs must NOT be compiled — nothing
    // references them, and defining them would be dead code. This define gates
    // them out to match.
    if has_exec_timers {
        build.define("EPHPM_NATIVE_EXEC_TIMER", None);
    }

    build.compile("ephpm_wrapper");
}

/// Generate Rust FFI bindings from PHP headers via bindgen.
fn generate_bindings(include_dir: &Path, target_os: &str, shared: bool) {
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
    // ZTS on every platform — the Windows SDK lib is ZTS too (see the
    // matching defines in compile_wrapper; issue #326).
    // The shared-engine SDK drops the static TSRM cache (see compile_wrapper).
    builder = builder.clang_arg("-DZTS=1");
    if !shared {
        builder = builder.clang_arg("-DZEND_ENABLE_STATIC_TSRMLS_CACHE=1");
    }
    if target_os == "windows" {
        builder = builder
            .clang_arg("--target=x86_64-pc-windows-msvc")
            .clang_arg("-DZEND_WIN32")
            .clang_arg("-DPHP_WIN32")
            .clang_arg("-DZEND_DEBUG=0")
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
    if let Ok(output) = Command::new("clang").arg("--print-resource-dir").output()
        && output.status.success()
    {
        let dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let include = PathBuf::from(&dir).join("include");
        if include.join("stddef.h").exists() {
            return Some(include);
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
