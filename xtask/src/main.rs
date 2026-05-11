use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::{env, fs};

/// Pinned full PHP versions per supported minor.
///
/// The build flow downloads pre-built `libphp.a` archives from
/// `github.com/ephpm/php-sdk` releases. Each entry maps a minor-version
/// shorthand (e.g. "8.5") to the specific patch release we publish SDKs for.
/// Users may also pass a full version explicitly (e.g. "8.5.2") and the
/// resolver will accept it as-is.
const PHP_SDK_VERSIONS: &[(&str, &str)] = &[("8.3", "8.3.29"), ("8.4", "8.4.19"), ("8.5", "8.5.2")];

/// Default PHP minor when no version is specified on the command line.
const DEFAULT_PHP_MINOR: &str = "8.5";

/// Pinned version of sqld (libsql-server) for clustered SQLite.
const SQLD_VERSION: &str = "0.24.32";

/// Pinned version of Hugo (extended) for the documentation site.
/// Newer versions dropped darwin tarballs in favor of .pkg installers.
const HUGO_VERSION: &str = "0.150.0";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str);

    match cmd {
        Some("release") => require_unix(|| release(&args[1..])),
        // php-sdk is a pure download — works on any platform with curl + tar.
        Some("php-sdk") => php_sdk(&args[1..]),
        Some("e2e") => e2e(&args[1..]),
        Some("e2e-up") => e2e_up(&args[1..]),
        Some("e2e-down") => e2e_down(),
        Some("e2e-install") => e2e_install(),
        Some("docs") => docs(&args[1..]),
        Some("help" | "--help" | "-h") | None => {
            print_usage();
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("unknown command: {other}");
            print_usage();
            ExitCode::FAILURE
        }
    }
}

fn print_usage() {
    eprintln!(
        "\
Usage: cargo xtask <command> [options]

Commands:
  release [8.5] [--target windows]  Build ephpm with PHP linked (default: 8.5)
  php-sdk [8.5]                     Download the PHP SDK (libphp.a + headers) for the current platform
  e2e [--php-version 8.5]           Run E2E tests (creates Kind cluster, builds images, tilt ci)
  e2e-up [--php-version 8.5]        Start E2E dev environment (tilt dashboard at localhost:10350)
  e2e-down                          Tear down Kind cluster and all resources
  e2e-install                       Download kind, tilt, kubectl to ./bin (no global install needed)
  docs <subcommand>                 Build/serve the Hugo + Hextra documentation site

The PHP SDK is downloaded from github.com/ephpm/php-sdk releases. Pass a minor
shorthand (e.g. \"8.5\") to use the pinned patch release, or a full version
(e.g. \"8.5.2\") for an explicit pin.

Cross-compilation:
  --target windows    Cross-compile a Windows .exe from WSL/Linux (requires cargo-xwin).
                      Downloads the same prebuilt SDK as Linux/macOS, but for windows-x86_64.

SQLite clustering:
  --sqld-binary PATH  Override: embed a specific sqld binary (default: auto-download v{SQLD_VERSION}).
  --no-sqld           Skip sqld embedding entirely (single-node SQLite only)."
    );
}

/// Parse `--php-version <ver>` from args, defaulting to "8.5".
fn parse_php_version(args: &[String]) -> &str {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--php-version" {
            if let Some(ver) = args.get(i + 1) {
                return ver;
            }
        }
    }
    "8.5"
}

/// Parse `--target <value>` from args. Only "windows" is currently supported.
fn parse_target(args: &[String]) -> Option<&str> {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--target" {
            return args.get(i + 1).map(String::as_str);
        }
    }
    None
}

/// Extract `--sqld-binary <path>` from release args.
fn parse_sqld_binary(args: &[String]) -> Option<&str> {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--sqld-binary" {
            return args.get(i + 1).map(String::as_str);
        }
    }
    None
}

/// Extract the PHP version from release args, skipping `--target` and its value.
/// Falls back to "8.5" if no positional version argument is found.
fn parse_release_php_version(args: &[String]) -> &str {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--target" || args[i] == "--sqld-binary" {
            i += 2; // skip flag and its value
            continue;
        }
        if args[i] == "--no-sqld" {
            i += 1;
            continue;
        }
        if !args[i].starts_with('-') {
            return &args[i];
        }
        i += 1;
    }
    "8.5"
}

/// Dispatch release builds based on `--target` flag.
fn release(args: &[String]) -> ExitCode {
    match parse_target(args) {
        None => release_native(args),
        Some("windows") => release_windows(args),
        Some(other) => {
            eprintln!("error: unsupported target '{other}' (supported: windows)");
            eprintln!("       omit --target for the default native build");
            ExitCode::FAILURE
        }
    }
}

/// Build the PHP SDK and then compile the release binary for the host.
///
/// On Linux, the prebuilt `libphp.a` is musl-linked (built by static-php-cli
/// in the php-sdk release pipeline), so the Rust binary must target musl too
/// or linking fails with sigsetjmp / `__flt_rounds` style errors. The result
/// is a fully static, self-contained binary.
///
/// On macOS, the prebuilt SDK was built with Homebrew clang against
/// `aarch64-apple-darwin`, so we build for the host target directly. Only
/// Apple Silicon is supported — there are no x86_64-darwin SDK artifacts.
fn release_native(args: &[String]) -> ExitCode {
    let php_version = match resolve_php_version(parse_release_php_version(args)) {
        Some(v) => v,
        None => return ExitCode::FAILURE,
    };

    let sdk_path = php_sdk_dir(&php_version);

    if ensure_php_sdk(&php_version).is_err() {
        return ExitCode::FAILURE;
    }

    // Pick the Rust target. On Linux we cross-link against musl libphp.a,
    // so we must build the Rust crate against the same libc.
    let host_target = if cfg!(target_os = "macos") {
        if let Err(code) = require_macos_arm64() {
            return code;
        }
        format!("{}-apple-darwin", std::env::consts::ARCH)
    } else {
        // Linux — match the libc (musl) of the prebuilt libphp.a.
        format!("{}-unknown-linux-musl", std::env::consts::ARCH)
    };

    eprintln!("==> Ensuring Rust target {host_target} is installed...");
    let status = Command::new("rustup").args(["target", "add", &host_target]).status();
    if !ran_ok(&status) {
        eprintln!("error: failed to add Rust target {host_target}");
        return ExitCode::FAILURE;
    }

    eprintln!("==> Building ephpm (release, target: {host_target})...");
    let mut cmd = Command::new("cargo");
    cmd.args(["build", "--release", "--package", "ephpm", "--target", &host_target])
        .env("PHP_SDK_PATH", &sdk_path);

    // Embed sqld binary: use --sqld-binary if provided, otherwise auto-download.
    // Pass --no-sqld to skip embedding entirely.
    if !args.iter().any(|a| a == "--no-sqld") {
        let sqld_path = if let Some(manual_path) = parse_sqld_binary(args) {
            let abs = std::path::Path::new(manual_path)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(manual_path));
            Some(abs)
        } else {
            download_sqld()
        };

        if let Some(path) = sqld_path {
            eprintln!("==> Embedding sqld binary from {}", path.display());
            cmd.env("SQLD_BINARY_PATH", &path);
        } else {
            eprintln!("warning: sqld binary not available — clustered SQLite will not work");
        }
    }

    let status = cmd.status();

    if !ran_ok(&status) {
        eprintln!("error: cargo build failed");
        return ExitCode::FAILURE;
    }

    eprintln!("==> Binary ready: target/{host_target}/release/ephpm");
    ExitCode::SUCCESS
}

/// Reject builds on macOS Intel — the php-sdk release pipeline only ships
/// `macos-aarch64` artifacts, so x86_64 darwin would have nothing to link
/// against. Apple Silicon is the only supported macOS target.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn require_macos_arm64() -> Result<(), ExitCode> {
    if cfg!(target_arch = "aarch64") {
        return Ok(());
    }
    eprintln!("error: macOS Intel (x86_64) is not supported.");
    eprintln!("       The php-sdk release only publishes macos-aarch64 artifacts.");
    eprintln!("       Build on Apple Silicon, or use a Linux machine.");
    Err(ExitCode::FAILURE)
}

/// Cross-compile a Windows .exe from WSL/Linux using cargo-xwin.
///
/// The Windows SDK is the same `php-sdk` GitHub release used for Linux/macOS,
/// just the `windows-x86_64` artifact: it contains `php8embed.dll`,
/// `php8embed.lib`, and the headers under `include/php/`. cargo-xwin handles
/// the MSVC link.
///
/// The resulting binary requires `php8embed.dll` at runtime, which is also
/// embedded into the binary via `include_bytes!()` in `windows_dll.rs` and
/// extracted at startup.
fn release_windows(args: &[String]) -> ExitCode {
    // sqld has no Windows binary — error if user tries to embed it.
    if parse_sqld_binary(args).is_some() {
        eprintln!("error: sqld is not available for Windows.");
        eprintln!("       Clustered SQLite requires Linux or macOS.");
        eprintln!("       Use WSL to build a Linux binary with sqld embedded.");
        return ExitCode::FAILURE;
    }
    if !args.iter().any(|a| a == "--no-sqld") {
        eprintln!("note: skipping sqld embedding (not available for Windows)");
        eprintln!("      The Windows build supports single-node SQLite only.");
    }

    let Some(php_version) = resolve_php_version(parse_release_php_version(args)) else {
        return ExitCode::FAILURE;
    };
    let target = "x86_64-pc-windows-msvc";

    eprintln!("==> Checking prerequisites...");
    if !has_command("cargo-xwin") {
        eprintln!("error: cargo-xwin not installed");
        eprintln!("       cargo install cargo-xwin");
        return ExitCode::FAILURE;
    }

    if ensure_php_sdk_for(&php_version, "windows", "x86_64").is_err() {
        return ExitCode::FAILURE;
    }
    let sdk_dir = php_sdk_dir_for(&php_version, "windows", "x86_64");

    eprintln!("==> Ensuring Rust target {target} is installed...");
    let status = Command::new("rustup").args(["target", "add", target]).status();
    if !ran_ok(&status) {
        eprintln!("error: failed to add Rust target {target}");
        return ExitCode::FAILURE;
    }

    eprintln!("==> Building ephpm.exe (release, target: {target})...");
    let status = Command::new("cargo")
        .args(["xwin", "build", "--release", "--package", "ephpm", "--target", target])
        .env("PHP_SDK_PATH", &sdk_dir)
        .status();

    if !ran_ok(&status) {
        eprintln!("error: cargo xwin build failed");
        return ExitCode::FAILURE;
    }

    // Copy php8embed.dll next to the .exe so the binary is runnable as a pair
    // (the DLL is also embedded via include_bytes!() and extracted at runtime,
    // but shipping it side-by-side keeps things obvious).
    let exe_dir = workspace_root().join("target").join(target).join("release");
    let dll_dest = exe_dir.join("php8embed.dll");
    let dll_src = sdk_dir.join("lib").join("php8embed.dll");
    if dll_src.exists() {
        if let Err(e) = fs::copy(&dll_src, &dll_dest) {
            eprintln!("warning: failed to copy php8embed.dll: {e}");
        }
    }

    eprintln!();
    eprintln!("==> Windows binary ready:");
    eprintln!("    {}", exe_dir.join("ephpm.exe").display());
    eprintln!("    {}", dll_dest.display());
    eprintln!();
    eprintln!("    Deploy both files together. php8embed.dll must be next to ephpm.exe.");
    ExitCode::SUCCESS
}

/// Resolve a user-supplied PHP version into a full pinned version string.
///
/// Accepts either a minor shorthand ("8.5") or a full version ("8.5.2").
/// Minor shorthands are mapped via `PHP_SDK_VERSIONS`; full versions are
/// returned as-is so users can pin to a specific patch even if it isn't
/// in the table (in that case, the download will simply fail with a 404
/// and the error message will point at the missing release tag).
fn resolve_php_version(input: &str) -> Option<String> {
    if let Some((_, full)) = PHP_SDK_VERSIONS.iter().find(|(short, _)| *short == input) {
        return Some((*full).to_string());
    }
    if input.matches('.').count() == 2 {
        return Some(input.to_string());
    }
    eprintln!("error: unknown PHP version '{input}'");
    eprintln!("       supported minors:");
    for (short, full) in PHP_SDK_VERSIONS {
        eprintln!("         {short}  → {full}");
    }
    eprintln!("       or pass a full version like 8.5.2 to use that release tag directly");
    None
}

/// Download the prebuilt PHP SDK for the host platform.
///
/// `cargo xtask php-sdk [version]` — pulls `libphp.a` (Linux/macOS) or
/// `php8embed.{dll,lib}` (Windows) plus the PHP headers from
/// github.com/ephpm/php-sdk releases and extracts them into
/// `<workspace>/php-sdk/<full-version>/`.
fn php_sdk(args: &[String]) -> ExitCode {
    let input = args.first().map_or(DEFAULT_PHP_MINOR, String::as_str);
    let Some(version) = resolve_php_version(input) else {
        return ExitCode::FAILURE;
    };

    let (os, arch) = match host_php_sdk_platform() {
        Some(p) => p,
        None => return ExitCode::FAILURE,
    };

    if ensure_php_sdk_for(&version, os, arch).is_err() {
        return ExitCode::FAILURE;
    }

    eprintln!("==> PHP SDK ready at {}", php_sdk_dir_for(&version, os, arch).display());
    ExitCode::SUCCESS
}

/// Detect the host's `(os, arch)` pair as named in php-sdk release assets.
///
/// Returns `None` and prints an error for unsupported platforms (notably
/// macOS Intel — only `macos-aarch64` is published).
fn host_php_sdk_platform() -> Option<(&'static str, &'static str)> {
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        if !cfg!(target_arch = "aarch64") {
            eprintln!("error: macOS Intel (x86_64) is not supported.");
            eprintln!("       The php-sdk release only publishes macos-aarch64 artifacts.");
            return None;
        }
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        eprintln!("error: unsupported host OS for the php-sdk download");
        return None;
    };

    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        eprintln!("error: unsupported host architecture for the php-sdk download");
        return None;
    };

    if os == "windows" && arch != "x86_64" {
        eprintln!("error: only windows-x86_64 SDK artifacts are published");
        return None;
    }

    Some((os, arch))
}

/// Download and extract the SDK for the host platform unless already cached.
fn ensure_php_sdk(version: &str) -> Result<(), ()> {
    let (os, arch) = host_php_sdk_platform().ok_or(())?;
    ensure_php_sdk_for(version, os, arch)
}

/// Download and extract the SDK for the given platform unless already cached.
///
/// The cache is keyed by `(version, os, arch)` so cross-compiled builds can
/// hold multiple SDKs side-by-side without trampling each other.
fn ensure_php_sdk_for(version: &str, os: &str, arch: &str) -> Result<(), ()> {
    let dest = php_sdk_dir_for(version, os, arch);

    // Layout we expect inside `dest/`:
    //   lib/libphp.a            (Linux + macOS)
    //   lib/php8embed.{dll,lib} (Windows)
    //   include/php/{main,Zend,TSRM,sapi,ext}/...
    let already_present = if os == "windows" {
        dest.join("lib").join("php8embed.lib").exists()
    } else {
        dest.join("lib").join("libphp.a").exists()
    };

    if already_present {
        eprintln!(
            "==> PHP SDK {version} ({os}-{arch}) already cached at {} — skipping download",
            dest.display()
        );
        return Ok(());
    }

    fs::create_dir_all(&dest).map_err(|e| {
        eprintln!("error: failed to create {}: {e}", dest.display());
    })?;

    let asset = format!("php-sdk-{version}-{os}-{arch}.tar.gz");
    let url = format!("https://github.com/ephpm/php-sdk/releases/download/v{version}/{asset}");

    eprintln!("==> Downloading {asset}...");
    if !download_and_extract_full_tarball(&url, &dest) {
        eprintln!("error: failed to download or extract PHP SDK from {url}");
        eprintln!("       Verify that release v{version} exists at:");
        eprintln!("         https://github.com/ephpm/php-sdk/releases/tag/v{version}");
        return Err(());
    }

    Ok(())
}

/// Workspace-relative cache path for a PHP SDK pinned to `(version, os, arch)`.
fn php_sdk_dir_for(version: &str, os: &str, arch: &str) -> PathBuf {
    workspace_root().join("php-sdk").join(format!("{version}-{os}-{arch}"))
}

/// Convenience for the host platform — cache path for the SDK that
/// `release_native()` will link against.
fn php_sdk_dir(version: &str) -> PathBuf {
    let (os, arch) = host_php_sdk_platform()
        .expect("host platform was already validated by ensure_php_sdk before calling php_sdk_dir");
    php_sdk_dir_for(version, os, arch)
}

/// Stream a tarball through curl + tar to extract every entry into `dest`.
///
/// Uses `tar --strip-components=1` if the archive nests under a top-level
/// directory (the php-sdk archives use `./lib/...`, so strip is harmless).
fn download_and_extract_full_tarball(url: &str, dest: &Path) -> bool {
    let curl =
        Command::new("curl").args(["-fSL", url]).stdout(std::process::Stdio::piped()).spawn();

    let Ok(curl) = curl else {
        eprintln!("error: failed to spawn curl");
        return false;
    };

    let status =
        Command::new("tar").args(["xz", "-C"]).arg(dest).stdin(curl.stdout.unwrap()).status();

    ran_ok(&status)
}

// ── E2E testing (Kind + Tilt) ────────────────────────────────────────────────

const KIND_CLUSTER_NAME: &str = "ephpm-dev";
const KIND_VERSION: &str = "0.27.0";
const TILT_VERSION: &str = "0.33.21";
const KUBECTL_VERSION: &str = "1.32.0";

/// Resolve the path for a tool: check `<workspace>/bin/<name>` first, then PATH.
fn find_tool(name: &str) -> String {
    let local = workspace_root().join("bin").join(name);
    if local.exists() {
        return local.to_str().unwrap().to_string();
    }
    // On unix, also check without extension; on windows check .exe
    #[cfg(windows)]
    {
        let local_exe = workspace_root().join("bin").join(format!("{name}.exe"));
        if local_exe.exists() {
            return local_exe.to_str().unwrap().to_string();
        }
    }
    name.to_string()
}

/// Check if a tool is available (local bin or PATH).
fn has_e2e_tool(name: &str) -> bool {
    // If the binary exists in ./bin/, trust it without running it — some tools
    // (e.g. tilt) use `tool version` subcommands rather than `--version` flags.
    let local = workspace_root().join("bin").join(name);
    if local.exists() {
        return true;
    }
    Command::new(name).arg("--version").output().is_ok_and(|o| o.status.success())
}

/// Detect OS and architecture for download URLs.
fn platform() -> (&'static str, &'static str) {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    };

    let arch = if cfg!(target_arch = "aarch64") { "arm64" } else { "amd64" };

    (os, arch)
}

/// Download kind, tilt, and kubectl to `<workspace>/bin/`.
fn e2e_install() -> ExitCode {
    let bin_dir = workspace_root().join("bin");
    fs::create_dir_all(&bin_dir).expect("failed to create bin directory");

    let (os, arch) = platform();

    eprintln!("==> Installing E2E tools to {}", bin_dir.display());
    eprintln!("    Platform: {os}/{arch}");

    // ── kind ─────────────────────────────────────────────────
    let kind_path = bin_dir.join("kind");
    if kind_path.exists() {
        eprintln!("==> kind already installed, skipping (delete bin/kind to reinstall)");
    } else {
        let url = format!(
            "https://github.com/kubernetes-sigs/kind/releases/download/v{KIND_VERSION}/kind-{os}-{arch}"
        );
        eprintln!("==> Downloading kind v{KIND_VERSION}...");
        if !download_binary(&url, &kind_path) {
            eprintln!("error: failed to download kind");
            return ExitCode::FAILURE;
        }
    }

    // ── kubectl ──────────────────────────────────────────────
    let kubectl_path = bin_dir.join("kubectl");
    if kubectl_path.exists() {
        eprintln!("==> kubectl already installed, skipping (delete bin/kubectl to reinstall)");
    } else {
        let url = format!("https://dl.k8s.io/release/v{KUBECTL_VERSION}/bin/{os}/{arch}/kubectl");
        eprintln!("==> Downloading kubectl v{KUBECTL_VERSION}...");
        if !download_binary(&url, &kubectl_path) {
            eprintln!("error: failed to download kubectl");
            return ExitCode::FAILURE;
        }
    }

    // ── tilt (tarball) ───────────────────────────────────────
    let tilt_path = bin_dir.join("tilt");
    if tilt_path.exists() {
        eprintln!("==> tilt already installed, skipping (delete bin/tilt to reinstall)");
    } else {
        // Tilt uses different naming: tilt.0.33.21.linux.x86_64.tar.gz
        let tilt_arch = if arch == "amd64" { "x86_64" } else { "arm64" };
        let url = format!(
            "https://github.com/tilt-dev/tilt/releases/download/v{TILT_VERSION}/tilt.{TILT_VERSION}.{os}.{tilt_arch}.tar.gz"
        );
        eprintln!("==> Downloading tilt v{TILT_VERSION}...");
        if !download_and_extract_tarball(&url, &bin_dir, "tilt") {
            eprintln!("error: failed to download/extract tilt");
            return ExitCode::FAILURE;
        }
    }

    eprintln!();
    eprintln!("==> E2E tools installed to {}", bin_dir.display());
    eprintln!("    kind:    {}", bin_dir.join("kind").display());
    eprintln!("    kubectl: {}", bin_dir.join("kubectl").display());
    eprintln!("    tilt:    {}", bin_dir.join("tilt").display());
    eprintln!();
    eprintln!("    The e2e/e2e-up/e2e-down commands will use these automatically.");
    ExitCode::SUCCESS
}

/// Download a single binary file via curl.
fn download_binary(url: &str, dest: &PathBuf) -> bool {
    let status = Command::new("curl").args(["-fSL", "-o"]).arg(dest).arg(url).status();

    if !ran_ok(&status) {
        return false;
    }

    make_executable(dest);
    true
}

/// Download a tarball via curl, pipe through tar, extract a specific binary.
fn download_and_extract_tarball(url: &str, dest_dir: &PathBuf, binary_name: &str) -> bool {
    // curl -fSL <url> | tar xz -C <dest_dir> <binary_name>
    let curl =
        Command::new("curl").args(["-fSL", url]).stdout(std::process::Stdio::piped()).spawn();

    let Ok(curl) = curl else {
        return false;
    };

    let status = Command::new("tar")
        .args(["xz", "-C"])
        .arg(dest_dir)
        .arg(binary_name)
        .stdin(curl.stdout.unwrap())
        .status();

    if !ran_ok(&status) {
        return false;
    }

    make_executable(&dest_dir.join(binary_name));
    true
}

/// Download a `.tar.xz` archive via curl, extract a specific binary.
fn download_and_extract_tar_xz(url: &str, dest_dir: &Path, binary_name: &str) -> bool {
    let curl =
        Command::new("curl").args(["-fSL", url]).stdout(std::process::Stdio::piped()).spawn();

    let Ok(curl) = curl else {
        eprintln!("error: failed to run curl");
        return false;
    };

    // xz -d | tar x -C <dest_dir> <binary_name>
    let xz = Command::new("xz")
        .arg("-d")
        .stdin(curl.stdout.unwrap())
        .stdout(std::process::Stdio::piped())
        .spawn();

    let Ok(xz) = xz else {
        eprintln!("error: failed to run xz (is xz-utils installed?)");
        return false;
    };

    let status = Command::new("tar")
        .args(["x", "-C"])
        .arg(dest_dir)
        .arg(binary_name)
        .stdin(xz.stdout.unwrap())
        .status();

    if !ran_ok(&status) {
        eprintln!("error: failed to extract {binary_name} from archive");
        return false;
    }

    make_executable(&dest_dir.join(binary_name));
    true
}

/// Download the sqld binary for the current platform.
///
/// Downloads from Turso's GitHub releases and caches in `sqld-cache/`.
/// Returns the path to the sqld binary, or `None` on failure.
fn download_sqld() -> Option<PathBuf> {
    let cache_dir = workspace_root().join("sqld-cache");
    let sqld_path = cache_dir.join("sqld");

    if sqld_path.exists() {
        eprintln!("==> sqld {SQLD_VERSION} already cached, skipping download");
        return Some(sqld_path);
    }

    let target = match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
        ("x86_64", "macos") => "x86_64-apple-darwin",
        ("aarch64", "macos") => "aarch64-apple-darwin",
        (arch, os) => {
            eprintln!("error: no pre-built sqld binary for {arch}-{os}");
            return None;
        }
    };

    let url = format!(
        "https://github.com/tursodatabase/libsql/releases/download/\
         libsql-server-v{SQLD_VERSION}/libsql-server-{target}.tar.xz"
    );

    eprintln!("==> Downloading sqld {SQLD_VERSION} for {target}...");
    fs::create_dir_all(&cache_dir).ok();

    if !download_and_extract_tar_xz(&url, &cache_dir, "sqld") {
        eprintln!("error: failed to download sqld from {url}");
        return None;
    }

    eprintln!("==> sqld {SQLD_VERSION} cached at {}", sqld_path.display());
    Some(sqld_path)
}

/// chmod +x on unix, no-op on windows.
fn make_executable(_path: &PathBuf) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = fs::metadata(_path) {
            let mut perms = metadata.permissions();
            perms.set_mode(perms.mode() | 0o755);
            fs::set_permissions(_path, perms).ok();
        }
    }
}

/// Run E2E tests headless: ensure cluster, build images, `tilt ci`, teardown.
fn e2e(args: &[String]) -> ExitCode {
    let php_version = parse_php_version(args);

    // Check prerequisites
    for tool in ["kind", "tilt", "kubectl"] {
        if !has_e2e_tool(tool) {
            eprintln!("error: {tool} not found. Run `cargo xtask e2e-install` to download it.");
            return ExitCode::FAILURE;
        }
    }

    let ce = container_engine();

    if ensure_kind_cluster() != ExitCode::SUCCESS {
        return ExitCode::FAILURE;
    }

    if build_and_load_images(&ce, php_version) != ExitCode::SUCCESS {
        return ExitCode::FAILURE;
    }

    let k8s_dir = workspace_root().join("k8s");

    eprintln!("==> Running E2E tests (tilt ci, PHP {php_version})...");
    let status = Command::new(find_tool("tilt"))
        .args(["ci"])
        .env("EXPECTED_PHP_VERSION", php_version)
        .current_dir(&k8s_dir)
        .status();

    if ran_ok(&status) {
        eprintln!("==> E2E tests passed");
        ExitCode::SUCCESS
    } else {
        eprintln!("==> E2E tests failed — dumping pod logs...");
        dump_pod_logs();
        ExitCode::FAILURE
    }
}

/// Start E2E dev environment: ensure cluster, build images, `tilt up --stream`.
/// The web dashboard is available at http://localhost:10350.
/// Ctrl+C to stop.
fn e2e_up(args: &[String]) -> ExitCode {
    let php_version = parse_php_version(args);

    for tool in ["kind", "tilt", "kubectl"] {
        if !has_e2e_tool(tool) {
            eprintln!("error: {tool} not found. Run `cargo xtask e2e-install` to download it.");
            return ExitCode::FAILURE;
        }
    }

    let ce = container_engine();

    if ensure_kind_cluster() != ExitCode::SUCCESS {
        return ExitCode::FAILURE;
    }

    if build_and_load_images(&ce, php_version) != ExitCode::SUCCESS {
        return ExitCode::FAILURE;
    }

    let k8s_dir = workspace_root().join("k8s");

    eprintln!("==> Starting Tilt (dashboard at http://localhost:10350, PHP {php_version})...");
    eprintln!("    Press Ctrl+C to stop.");
    let status = Command::new(find_tool("tilt"))
        .args(["up", "--stream"])
        .env("EXPECTED_PHP_VERSION", php_version)
        .current_dir(&k8s_dir)
        .status();

    if ran_ok(&status) { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

/// Tear down Tilt resources and delete the Kind cluster.
fn e2e_down() -> ExitCode {
    let k8s_dir = workspace_root().join("k8s");

    // tilt down (ignore errors — cluster may already be gone)
    if has_e2e_tool("tilt") {
        eprintln!("==> Removing Tilt resources...");
        Command::new(find_tool("tilt")).args(["down"]).current_dir(&k8s_dir).status().ok();
    }

    if has_e2e_tool("kind") {
        eprintln!("==> Deleting Kind cluster '{KIND_CLUSTER_NAME}'...");
        let status = Command::new(find_tool("kind"))
            .args(["delete", "cluster", "--name", KIND_CLUSTER_NAME])
            .status();

        if !ran_ok(&status) {
            eprintln!("warning: kind delete failed (cluster may not exist)");
        }
    }

    eprintln!("==> E2E environment torn down");
    ExitCode::SUCCESS
}

/// Create the Kind cluster if it doesn't already exist.
fn ensure_kind_cluster() -> ExitCode {
    let kind = find_tool("kind");

    // Check if cluster already exists
    let output = Command::new(&kind).args(["get", "clusters"]).output();

    if let Ok(output) = output {
        let clusters = String::from_utf8_lossy(&output.stdout);
        if clusters.lines().any(|line| line.trim() == KIND_CLUSTER_NAME) {
            eprintln!("==> Kind cluster '{KIND_CLUSTER_NAME}' already exists");
            return ExitCode::SUCCESS;
        }
    }

    eprintln!("==> Creating Kind cluster '{KIND_CLUSTER_NAME}'...");

    let root = workspace_root();
    let config_path = root.join("k8s").join("kind-config.yaml");

    let mut cmd = Command::new(&kind);
    cmd.args(["create", "cluster", "--name", KIND_CLUSTER_NAME]);

    if config_path.exists() {
        cmd.arg("--config").arg(&config_path);
    }

    let status = cmd.status();

    if ran_ok(&status) {
        eprintln!("==> Kind cluster ready");
        ExitCode::SUCCESS
    } else {
        eprintln!("error: failed to create Kind cluster");
        ExitCode::FAILURE
    }
}

/// Build the ephpm and E2E test runner container images, then load them into Kind.
fn build_and_load_images(ce: &str, php_version: &str) -> ExitCode {
    let root = workspace_root();
    let kind = find_tool("kind");
    let dockerfile = root.join("docker").join("Dockerfile");
    let dockerfile_e2e = root.join("docker").join("Dockerfile.e2e");

    // The Dockerfile takes PHP_SDK_VERSION as a full version (e.g. "8.5.2")
    // so its php-sdk stage layer is keyed deterministically. Resolve any
    // shorthand the caller passed (e.g. "8.5") via the same table xtask uses.
    let Some(php_sdk_version) = resolve_php_version(php_version) else {
        return ExitCode::FAILURE;
    };

    // Build path differs by container engine:
    //
    //  - docker: write the result as a docker-format tarball via
    //    `buildx build --output type=docker,dest=<file>`, then feed it to
    //    `kind load image-archive`. We *cannot* use `--load` (docker
    //    buildx's standard "load image into local daemon") because the
    //    self-hosted runner's container engine (ephemerd) doesn't
    //    implement the `/images/load` endpoint that --load POSTs to —
    //    builds succeed and then crash at the very end with
    //    "POST /v1.45/images/load is not yet implemented". And we *cannot*
    //    just drop --load either: the docker-container buildx driver
    //    keeps the result in its own cache, so `kind load docker-image`
    //    later can't find the tag in the daemon. Tarball + image-archive
    //    sidesteps the daemon entirely.
    //
    //  - podman: plain `podman build` writes the image into podman's
    //    storage directly and `kind load docker-image` reads from there.
    let target_dir = root.join("target");
    std::fs::create_dir_all(&target_dir).ok();
    let ephpm_tar = target_dir.join("ephpm-image.tar");
    let e2e_tar = target_dir.join("ephpm-e2e-image.tar");

    let docker = ce == "docker";

    // Build ephpm image with the specified PHP version
    if dockerfile.exists() {
        eprintln!("==> Building ephpm container image (PHP {php_sdk_version})...");
        let mut cmd = Command::new(ce);
        if docker {
            cmd.args([
                "buildx",
                "build",
                "--output",
                &format!("type=docker,dest={}", ephpm_tar.display()),
            ]);
        } else {
            cmd.arg("build");
        }
        cmd.args(["-f"])
            .arg(&dockerfile)
            .args([
                "--build-arg",
                &format!("PHP_SDK_VERSION={php_sdk_version}"),
                "-t",
                "ephpm:dev",
                ".",
            ])
            .current_dir(&root);

        let status = cmd.status();

        if !ran_ok(&status) {
            eprintln!("error: failed to build ephpm image");
            return ExitCode::FAILURE;
        }
    } else {
        eprintln!("warning: docker/Dockerfile not found, skipping ephpm image build");
    }

    // Build E2E test runner image
    if dockerfile_e2e.exists() {
        eprintln!("==> Building E2E test runner image...");
        let mut cmd = Command::new(ce);
        if docker {
            cmd.args([
                "buildx",
                "build",
                "--output",
                &format!("type=docker,dest={}", e2e_tar.display()),
            ]);
        } else {
            cmd.arg("build");
        }
        cmd.args(["-f"]).arg(&dockerfile_e2e).args(["-t", "ephpm-e2e:dev", "."]).current_dir(&root);

        let status = cmd.status();

        if !ran_ok(&status) {
            eprintln!("error: failed to build ephpm-e2e image");
            return ExitCode::FAILURE;
        }
    } else {
        eprintln!("warning: docker/Dockerfile.e2e not found, skipping E2E image build");
    }

    // Load images into Kind. With docker we have tarballs from the build;
    // with podman the images already live in podman storage.
    eprintln!("==> Loading images into Kind cluster...");
    if docker {
        // Single-node Kind cluster — the control-plane container is named
        // `<cluster>-control-plane` by convention. We pass it explicitly via
        // --nodes because the default code path runs
        // `docker ps --filter label=io.x-k8s.kind.cluster=<name>` to enumerate
        // nodes, and the self-hosted runner's container engine (ephemerd)
        // returns an empty line instead of an empty list, which kind then
        // tries to `docker inspect ''` and fails:
        //
        //     ERROR: failed to get role for node: ...
        //         "docker inspect ... ''" failed with error: exit status 1
        //         invalid container name or ID: value is empty
        //
        // Naming the node explicitly bypasses that lookup.
        let control_plane = format!("{KIND_CLUSTER_NAME}-control-plane");
        for tarball in [&ephpm_tar, &e2e_tar] {
            if !tarball.exists() {
                continue;
            }
            let status = Command::new(&kind)
                .args(["load", "image-archive"])
                .arg(tarball)
                .args(["--name", KIND_CLUSTER_NAME, "--nodes", &control_plane])
                .status();

            if !ran_ok(&status) {
                eprintln!("warning: failed to load {} into Kind", tarball.display());
            }
        }
    } else {
        for image in ["ephpm:dev", "ephpm-e2e:dev"] {
            let status = Command::new(&kind)
                .args(["load", "docker-image", image, "--name", KIND_CLUSTER_NAME])
                .status();

            if !ran_ok(&status) {
                eprintln!("warning: failed to load {image} into Kind (image may not exist yet)");
            }
        }
    }

    ExitCode::SUCCESS
}

/// Dump pod logs for debugging failed E2E tests.
fn dump_pod_logs() {
    let kubectl = find_tool("kubectl");

    eprintln!("--- ephpm pod logs (current container) ---");
    Command::new(&kubectl).args(["logs", "-l", "app=ephpm", "--tail=200"]).status().ok();

    // For CrashLoopBackOff, the previous container's tail is what shows the
    // actual cause of death (panic, signal, etc).
    eprintln!("--- ephpm pod logs (previous container, if any) ---");
    Command::new(&kubectl)
        .args(["logs", "-l", "app=ephpm", "--previous", "--tail=200"])
        .status()
        .ok();

    eprintln!("--- e2e job logs ---");
    Command::new(&kubectl).args(["logs", "job/ephpm-e2e", "--tail=200"]).status().ok();

    eprintln!("--- pod status ---");
    Command::new(&kubectl).args(["get", "pods", "-o", "wide"]).status().ok();

    // describe surfaces termination reason (OOMKilled, exit code, etc) and
    // events (FailedScheduling, CrashLoopBackOff, probe failures).
    eprintln!("--- ephpm pod describe ---");
    Command::new(&kubectl).args(["describe", "pod", "-l", "app=ephpm"]).status().ok();

    eprintln!("--- recent cluster events ---");
    Command::new(&kubectl).args(["get", "events", "--sort-by=.lastTimestamp", "-A"]).status().ok();
}

/// Determine which container engine to use (podman or docker).
fn container_engine() -> String {
    env::var("CONTAINER_ENGINE")
        .unwrap_or_else(|_| if has_command("podman") { "podman".into() } else { "docker".into() })
}

// ── workspace + platform helpers ─────────────────────────────────────────────

/// Find the workspace root (directory containing the root Cargo.toml).
fn workspace_root() -> PathBuf {
    let mut dir = env::current_dir().expect("cannot read current directory");
    loop {
        if dir.join("Cargo.toml").exists() && dir.join("crates").exists() {
            return dir;
        }
        if !dir.pop() {
            // Fallback to current directory
            return env::current_dir().unwrap();
        }
    }
}

/// Building ephpm requires a Unix toolchain (musl-gcc on Linux, Apple clang
/// on macOS). On Windows, re-execute the same command inside WSL.
///
/// The PHP SDK itself is now a plain tarball download and works on any host
/// with `curl` + `tar`, so `cargo xtask php-sdk` doesn't go through here —
/// only `cargo xtask release` does, since `cargo build` for a musl target
/// from native Windows isn't supported.
fn require_unix(f: impl FnOnce() -> ExitCode) -> ExitCode {
    if !cfg!(windows) {
        return f();
    }

    if !has_command("wsl") {
        eprintln!("error: ephpm release builds require a Unix toolchain (musl-gcc on Linux).");
        eprintln!("       Install WSL: wsl --install");
        return ExitCode::FAILURE;
    }

    let args: Vec<String> = env::args().skip(1).collect();
    // Source cargo env since `bash -c` does not load login profiles.
    let xtask_cmd =
        format!("source \"$HOME/.cargo/env\" 2>/dev/null; cargo xtask {}", args.join(" "),);

    eprintln!("==> Windows detected, running via WSL...");
    let status = Command::new("wsl").args(["--", "bash", "-c", &xtask_cmd]).status();

    if ran_ok(&status) {
        ExitCode::SUCCESS
    } else {
        eprintln!();
        eprintln!("WSL build failed. Make sure WSL has the required tools:");
        eprintln!(
            "  wsl -- bash -c 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh'"
        );
        eprintln!(
            "  wsl -- bash -c 'sudo apt update && sudo apt install -y build-essential pkg-config libclang-dev musl-tools curl git'"
        );
        ExitCode::FAILURE
    }
}

fn has_command(name: &str) -> bool {
    Command::new(name).arg("--version").output().is_ok_and(|o| o.status.success())
}

fn ran_ok(result: &Result<std::process::ExitStatus, std::io::Error>) -> bool {
    matches!(result, Ok(s) if s.success())
}

// ---------------------------------------------------------------------------
// docs: build/serve/manage the Hugo + Hextra documentation site at site/
// ---------------------------------------------------------------------------

fn docs(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("serve") => docs_serve(&args[1..]),
        Some("build") => docs_build(),
        Some("new") => docs_new(&args[1..]),
        Some("check") => docs_check(),
        Some("deps") => docs_deps(),
        Some("install") => docs_install(),
        Some("help" | "--help" | "-h") | None => {
            print_docs_usage();
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("unknown docs subcommand: {other}");
            print_docs_usage();
            ExitCode::FAILURE
        }
    }
}

fn print_docs_usage() {
    eprintln!(
        "\
Usage: cargo xtask docs <subcommand> [args]

Subcommands:
  install           Download hugo extended v{HUGO_VERSION} into ./bin (no global install needed).
  serve [PORT]      Run hugo serve with live reload (default port 1313).
                    Includes draft pages so unfinished sections are visible.
  build             Build the static site to site/public (with --minify).
  new <PATH>        Scaffold a new content page.
                    Example: cargo xtask docs new docs/guides/redis.md
  check             Run a link-checker (lychee) over the built site.
                    Requires `cargo xtask docs build` first.
  deps              Verify hugo is installed and the hextra theme submodule is present."
    );
}

/// Resolve which `hugo` to invoke: prefer the pinned `./bin/hugo` we install,
/// fall back to whatever's on PATH (so contributors with their own install
/// still work).
fn hugo_command() -> Command {
    let local = workspace_root().join("bin").join(hugo_binary_name());
    if local.exists() { Command::new(local) } else { Command::new("hugo") }
}

fn hugo_binary_name() -> &'static str {
    if cfg!(windows) { "hugo.exe" } else { "hugo" }
}

fn site_dir() -> PathBuf {
    workspace_root().join("site")
}

fn ensure_hugo() -> Result<(), ExitCode> {
    let local = workspace_root().join("bin").join(hugo_binary_name());
    if local.exists() {
        return Ok(());
    }
    if has_command("hugo") {
        return Ok(());
    }
    eprintln!("error: hugo not found");
    eprintln!("       run: cargo xtask docs install");
    eprintln!("       (downloads pinned hugo extended v{HUGO_VERSION} to ./bin/)");
    Err(ExitCode::FAILURE)
}

fn ensure_theme() -> Result<(), ExitCode> {
    let theme_marker = site_dir().join("themes").join("hextra").join("hugo.toml");
    if theme_marker.exists() {
        return Ok(());
    }
    eprintln!("error: hextra theme not initialized at site/themes/hextra");
    eprintln!("       run: git submodule update --init --recursive");
    Err(ExitCode::FAILURE)
}

fn docs_serve(args: &[String]) -> ExitCode {
    if let Err(c) = ensure_hugo() {
        return c;
    }
    if let Err(c) = ensure_theme() {
        return c;
    }

    let port = args.first().cloned().unwrap_or_else(|| "1313".into());

    eprintln!("==> hugo serve on http://127.0.0.1:{port} (drafts included)");
    let status = hugo_command()
        .args(["server", "-D", "--bind", "127.0.0.1", "--port", &port, "-s"])
        .arg(site_dir())
        .status();

    if ran_ok(&status) { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

fn docs_build() -> ExitCode {
    if let Err(c) = ensure_hugo() {
        return c;
    }
    if let Err(c) = ensure_theme() {
        return c;
    }

    eprintln!("==> hugo --minify -s site");
    let status = hugo_command().args(["--minify", "-s"]).arg(site_dir()).status();

    if ran_ok(&status) {
        eprintln!("==> Built to site/public/");
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn docs_new(args: &[String]) -> ExitCode {
    if let Err(c) = ensure_hugo() {
        return c;
    }

    let Some(path) = args.first() else {
        eprintln!("error: missing path");
        eprintln!("usage: cargo xtask docs new <path>");
        eprintln!("example: cargo xtask docs new docs/guides/redis.md");
        return ExitCode::FAILURE;
    };

    let status = hugo_command().args(["new", "content", path, "-s"]).arg(site_dir()).status();

    if ran_ok(&status) { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

fn docs_check() -> ExitCode {
    let public = site_dir().join("public");
    if !public.exists() {
        eprintln!("error: site/public not found — run `cargo xtask docs build` first");
        return ExitCode::FAILURE;
    }
    if !has_command("lychee") {
        eprintln!("error: lychee not found in PATH");
        eprintln!("       install: cargo install lychee");
        return ExitCode::FAILURE;
    }

    eprintln!("==> lychee link check on site/public/");
    let pattern = format!("{}/**/*.html", public.display());
    let status =
        Command::new("lychee").args(["--no-progress", "--include-fragments", &pattern]).status();

    if ran_ok(&status) { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

fn docs_deps() -> ExitCode {
    let mut all_ok = true;

    let local_hugo = workspace_root().join("bin").join(hugo_binary_name());
    if local_hugo.exists() {
        eprintln!("==> hugo: ok ({})", local_hugo.display());
    } else if has_command("hugo") {
        eprintln!("==> hugo: ok (system PATH — pin with `cargo xtask docs install`)");
    } else {
        eprintln!(
            "==> hugo: MISSING — run `cargo xtask docs install` (downloads v{HUGO_VERSION} to ./bin/)"
        );
        all_ok = false;
    }

    let theme_marker = site_dir().join("themes").join("hextra").join("hugo.toml");
    if theme_marker.exists() {
        eprintln!("==> hextra theme: ok");
    } else {
        eprintln!("==> hextra theme: MISSING — run: git submodule update --init --recursive");
        all_ok = false;
    }

    if has_command("lychee") {
        eprintln!("==> lychee (link checker): ok");
    } else {
        eprintln!(
            "==> lychee: not installed — run `cargo install lychee` if you want `docs check`"
        );
    }

    if all_ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

/// Download a pinned hugo extended binary into `./bin/`.
///
/// Mirrors `e2e_install`: pinned version, OS/arch detection, idempotent.
fn docs_install() -> ExitCode {
    let bin_dir = workspace_root().join("bin");
    if let Err(e) = fs::create_dir_all(&bin_dir) {
        eprintln!("error: failed to create {}: {e}", bin_dir.display());
        return ExitCode::FAILURE;
    }

    let (os, arch) = platform();
    // Hugo names darwin builds "darwin-universal" regardless of cpu arch.
    let asset_arch = if os == "darwin" { "universal" } else { arch };
    let ext = if os == "windows" { "zip" } else { "tar.gz" };
    let asset = format!("hugo_extended_{HUGO_VERSION}_{os}-{asset_arch}.{ext}");
    let url = format!("https://github.com/gohugoio/hugo/releases/download/v{HUGO_VERSION}/{asset}");

    let bin_name = hugo_binary_name();
    let dest = bin_dir.join(bin_name);

    if dest.exists() {
        let already_pinned = Command::new(&dest)
            .arg("version")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(HUGO_VERSION))
            .unwrap_or(false);
        if already_pinned {
            eprintln!("==> hugo v{HUGO_VERSION} already installed at {}", dest.display());
            return ExitCode::SUCCESS;
        }
        eprintln!("==> hugo at {} is a different version, replacing", dest.display());
        if let Err(e) = fs::remove_file(&dest) {
            eprintln!("error: failed to remove old hugo: {e}");
            return ExitCode::FAILURE;
        }
    }

    eprintln!("==> Downloading hugo v{HUGO_VERSION} ({os}/{asset_arch})...");

    let success = if ext == "zip" {
        download_and_extract_zip(&url, &bin_dir, bin_name)
    } else {
        download_and_extract_tarball(&url, &bin_dir, bin_name)
    };

    if !success {
        eprintln!("error: failed to download/extract hugo from {url}");
        return ExitCode::FAILURE;
    }

    eprintln!("==> hugo v{HUGO_VERSION} installed at {}", dest.display());
    ExitCode::SUCCESS
}

/// Download a `.zip` archive via curl, extract a single file to `dest_dir`.
///
/// Uses `tar -xf` which on Windows 10+ ships as bsdtar (libarchive) and
/// natively reads zip; on macOS bsdtar is the default `tar`.
fn download_and_extract_zip(url: &str, dest_dir: &Path, file_name: &str) -> bool {
    let tmp = std::env::temp_dir().join(format!("ephpm-xtask-{}.zip", std::process::id()));
    let _ = fs::remove_file(&tmp);

    let status = Command::new("curl").args(["-fSL", "-o"]).arg(&tmp).arg(url).status();
    if !ran_ok(&status) {
        let _ = fs::remove_file(&tmp);
        return false;
    }

    let status =
        Command::new("tar").arg("-xf").arg(&tmp).arg("-C").arg(dest_dir).arg(file_name).status();

    let _ = fs::remove_file(&tmp);

    if !ran_ok(&status) {
        return false;
    }

    let dest_path = dest_dir.join(file_name);
    make_executable(&dest_path);
    true
}
