//! `cargo xtask doctor` — preflight checks for build prerequisites.
//!
//! Prints a check table grouped into sections (Core, Rust toolchain,
//! C toolchain, PHP SDK, Optional) and exits non-zero when any REQUIRED
//! check is missing. Output is ASCII-only by project convention.
//!
//! This is the future preflight for `ephpm forge` (see
//! `docs/architecture/build-compose-design.md`) — `spc doctor` plays the
//! same role in static-php-cli.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::{env, fs};

/// Minimum supported Rust version — keep in sync with `rust-version` in the
/// workspace `Cargo.toml`.
const MSRV: (u32, u32) = (1, 85);

/// Remedy shared by all Linux C-toolchain checks (matches the README and
/// CLAUDE.md prerequisites list).
const APT_REMEDY: &str = "apt install build-essential pkg-config libclang-dev";

/// Outcome of a single check.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    /// Present and usable.
    Ok,
    /// Not found / unusable. Fails `doctor` when the check is required.
    Miss,
    /// Not applicable on this host/target combination.
    Skip,
    /// Present-but-degraded or informational; never fails `doctor`.
    Warn,
}

/// One row of the doctor table.
struct Check {
    name: String,
    status: Status,
    detail: String,
    /// One-line fix, printed under the row when the check did not pass.
    remedy: String,
    /// Required checks fail the run when their status is [`Status::Miss`].
    required: bool,
}

impl Check {
    fn new(name: &str, status: Status, detail: impl Into<String>) -> Self {
        Check {
            name: name.to_string(),
            status,
            detail: detail.into(),
            remedy: String::new(),
            required: true,
        }
    }

    fn remedy(mut self, remedy: &str) -> Self {
        self.remedy = remedy.to_string();
        self
    }

    fn optional(mut self) -> Self {
        self.required = false;
        self
    }
}

/// Which target the user intends to build for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BuildTarget {
    /// Host-native build (`cargo xtask release`).
    Native,
    /// Windows build (`cargo xtask release --target windows`).
    Windows,
}

/// Entry point for `cargo xtask doctor [--target windows]`.
pub(crate) fn doctor(args: &[String]) -> ExitCode {
    let target = match crate::parse_target(args) {
        None => BuildTarget::Native,
        Some("windows") => BuildTarget::Windows,
        Some(other) => {
            eprintln!("error: unsupported target '{other}' (supported: windows)");
            return ExitCode::FAILURE;
        }
    };

    let sections: Vec<(&str, Vec<Check>)> = vec![
        ("Core", core_checks()),
        ("Rust toolchain", rust_checks()),
        ("C toolchain", c_toolchain_checks(target)),
        ("PHP SDK", php_sdk_checks(target)),
        ("Optional", optional_checks()),
    ];

    print!("{}", render(&sections));

    let all = || sections.iter().flat_map(|(_, checks)| checks);
    let count = |s: Status| all().filter(|c| c.status == s).count();
    println!(
        "{} ok, {} missing, {} warnings, {} skipped",
        count(Status::Ok),
        count(Status::Miss),
        count(Status::Warn),
        count(Status::Skip)
    );

    if has_required_failure(all()) {
        eprintln!("doctor: one or more REQUIRED checks failed");
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

// ── rendering ────────────────────────────────────────────────────────────────

/// Status tag as printed in the table. Fixed width (6 chars), ASCII-only.
fn tag(status: Status) -> &'static str {
    match status {
        Status::Ok => "[ OK ]",
        Status::Miss => "[MISS]",
        Status::Skip => "[SKIP]",
        Status::Warn => "[WARN]",
    }
}

/// Render all sections into the final table string.
fn render(sections: &[(&str, Vec<Check>)]) -> String {
    let width =
        sections.iter().flat_map(|(_, checks)| checks).map(|c| c.name.len()).max().unwrap_or(0);

    let mut out = String::new();
    for (title, checks) in sections {
        out.push_str(title);
        out.push('\n');
        for c in checks {
            out.push_str(&format!("  {} {:<width$}  {}\n", tag(c.status), c.name, c.detail));
            if !c.remedy.is_empty() && matches!(c.status, Status::Miss | Status::Warn) {
                // Align "remedy:" with the detail column (2 + 6 + 1 + width + 2).
                out.push_str(&format!("{:indent$}remedy: {}\n", "", c.remedy, indent = width + 11));
            }
        }
        out.push('\n');
    }
    out
}

/// True when any required check is missing — the failure condition.
fn has_required_failure<'a>(checks: impl Iterator<Item = &'a Check>) -> bool {
    let mut checks = checks;
    checks.any(|c| c.required && c.status == Status::Miss)
}

// ── generic probes ───────────────────────────────────────────────────────────

/// Run `program args...` and return the first non-empty output line on
/// success (stdout preferred, stderr as fallback).
fn version_of(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = if out.stdout.is_empty() { &out.stderr } else { &out.stdout };
    let line = String::from_utf8_lossy(text).lines().next().unwrap_or("").trim().to_string();
    if line.is_empty() { Some(format!("{program}: ok")) } else { Some(line) }
}

/// Required check for a tool that answers `--version`.
fn tool_check(name: &str, remedy: &str) -> Check {
    match version_of(name, &["--version"]) {
        Some(line) => Check::new(name, Status::Ok, line),
        None => Check::new(name, Status::Miss, "not found on PATH").remedy(remedy),
    }
}

/// Search PATH for an executable without running it. Never shells out to
/// `which`/`where` — iterates PATH entries with std (checks `.exe`/`.cmd`/
/// `.bat` variants on Windows).
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let plain = dir.join(name);
        if plain.is_file() {
            return Some(plain);
        }
        if cfg!(windows) {
            for ext in ["exe", "cmd", "bat"] {
                let candidate = dir.join(format!("{name}.{ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

// ── Core ─────────────────────────────────────────────────────────────────────

fn core_checks() -> Vec<Check> {
    vec![
        tool_check("git", "install git (https://git-scm.com)"),
        tool_check("curl", "install curl (ships with Windows 10+ and macOS)"),
        tool_check("tar", "install tar (bsdtar ships with Windows 10+ and macOS)"),
    ]
}

// ── Rust toolchain ───────────────────────────────────────────────────────────

/// Parse `(major, minor)` out of a `rustc --version` line, e.g.
/// `rustc 1.94.0 (4a4ef493e 2026-03-02)` or `rustc 1.95.0-nightly (...)`.
fn parse_rustc_version(line: &str) -> Option<(u32, u32)> {
    let version = line.strip_prefix("rustc ")?.split_whitespace().next()?;
    let version = version.split('-').next()?;
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// True when a parsed rustc version satisfies the workspace MSRV.
fn meets_msrv(version: (u32, u32)) -> bool {
    version >= MSRV
}

fn rustc_check() -> Check {
    let Some(line) = version_of("rustc", &["--version"]) else {
        return Check::new("rustc", Status::Miss, "not found on PATH")
            .remedy("install Rust via https://rustup.rs");
    };
    match parse_rustc_version(&line) {
        Some(v) if meets_msrv(v) => Check::new("rustc", Status::Ok, line),
        Some(_) => {
            Check::new("rustc", Status::Miss, format!("{line} - below MSRV {}.{}", MSRV.0, MSRV.1))
                .remedy("rustup update stable")
        }
        None => Check::new("rustc", Status::Warn, format!("could not parse version: {line}")),
    }
}

fn nightly_fmt_check() -> Check {
    match version_of("cargo", &["+nightly", "fmt", "--version"]) {
        Some(line) => Check::new("nightly rustfmt", Status::Ok, line).optional(),
        None => Check::new(
            "nightly rustfmt",
            Status::Warn,
            "cargo +nightly fmt unavailable (only needed for dev formatting)",
        )
        .remedy("rustup toolchain install nightly --component rustfmt")
        .optional(),
    }
}

fn rust_checks() -> Vec<Check> {
    // Linux release builds target the host-default gnu triple, so no extra
    // rustup target is needed beyond the stable toolchain itself.
    vec![
        rustc_check(),
        tool_check("cargo", "install Rust via https://rustup.rs"),
        nightly_fmt_check(),
    ]
}

// ── C toolchain ──────────────────────────────────────────────────────────────

fn c_toolchain_checks(target: BuildTarget) -> Vec<Check> {
    match target {
        BuildTarget::Windows if !cfg!(target_os = "windows") => {
            // release_windows() requires a native Windows host; the cargo-xwin
            // cross-compile path was removed (see release_windows in main.rs).
            vec![
                Check::new(
                    "windows host",
                    Status::Miss,
                    "--target windows requires a native Windows host \
                     (the cargo-xwin cross-compile path was removed)",
                )
                .remedy("run on a Windows machine or the [self-hosted, windows, x64] CI runner"),
            ]
        }
        _ if cfg!(target_os = "windows") => {
            // Native Windows: MSVC toolchain for cargo builds; WSL is the
            // path `cargo xtask release` (no --target) actually takes.
            let mut checks = vec![msvc_linker_check(), libclang_check_windows()];
            if target == BuildTarget::Native {
                checks.push(wsl_check());
            }
            checks
        }
        _ if cfg!(target_os = "linux") => linux_checks(),
        _ if cfg!(target_os = "macos") => macos_checks(),
        _ => vec![
            Check::new("host os", Status::Warn, "unsupported host OS - no C toolchain checks")
                .optional(),
        ],
    }
}

fn linux_checks() -> Vec<Check> {
    // The Linux release build is glibc-dynamic (gnu target, glibc-linked
    // libphp.a) — the host gcc is the linker driver; nothing beyond
    // cc + pkg-config + libclang (bindgen) is required.
    let cc = match version_of("cc", &["--version"]).or_else(|| version_of("gcc", &["--version"])) {
        Some(line) => Check::new("cc/gcc", Status::Ok, line),
        None => Check::new("cc/gcc", Status::Miss, "no C compiler found").remedy(APT_REMEDY),
    };
    vec![cc, tool_check("pkg-config", APT_REMEDY), libclang_check_linux()]
}

fn macos_checks() -> Vec<Check> {
    let xcode = match version_of("xcode-select", &["-p"]) {
        Some(path) => Check::new("xcode CLT", Status::Ok, path),
        None => Check::new("xcode CLT", Status::Miss, "xcode-select -p failed")
            .remedy("xcode-select --install"),
    };
    let clang = match version_of("clang", &["--version"]) {
        Some(line) => Check::new("clang", Status::Ok, line),
        None => {
            Check::new("clang", Status::Miss, "clang not found").remedy("xcode-select --install")
        }
    };
    vec![xcode, clang, libclang_check_macos()]
}

// ── libclang detection (bindgen) ─────────────────────────────────────────────

/// If `LIBCLANG_PATH` is set, judge the check by whether it exists.
fn libclang_from_env() -> Option<Check> {
    let path = env::var("LIBCLANG_PATH").ok().filter(|p| !p.is_empty())?;
    if Path::new(&path).exists() {
        Some(Check::new("libclang", Status::Ok, format!("LIBCLANG_PATH={path}")))
    } else {
        Some(
            Check::new("libclang", Status::Miss, format!("LIBCLANG_PATH={path} does not exist"))
                .remedy("fix or unset LIBCLANG_PATH"),
        )
    }
}

fn libclang_check_linux() -> Check {
    if let Some(check) = libclang_from_env() {
        return check;
    }
    // Best-effort: ldconfig cache, then the Debian/Ubuntu llvm layout.
    if let Ok(out) = Command::new("ldconfig").arg("-p").output() {
        if out.status.success() && String::from_utf8_lossy(&out.stdout).contains("libclang") {
            return Check::new("libclang", Status::Ok, "found via ldconfig -p");
        }
    }
    if let Some(dir) = find_llvm_lib_dir() {
        return Check::new("libclang", Status::Ok, dir.display().to_string());
    }
    Check::new("libclang", Status::Miss, "libclang not found (needed by bindgen)")
        .remedy(APT_REMEDY)
}

/// Scan `/usr/lib/llvm-*/lib` for a Debian/Ubuntu-packaged LLVM.
fn find_llvm_lib_dir() -> Option<PathBuf> {
    for entry in fs::read_dir("/usr/lib").ok()?.flatten() {
        if entry.file_name().to_string_lossy().starts_with("llvm-") {
            let lib = entry.path().join("lib");
            if lib.exists() {
                return Some(lib);
            }
        }
    }
    None
}

fn libclang_check_macos() -> Check {
    if let Some(check) = libclang_from_env() {
        return check;
    }
    // CI pins brew llvm@17 for bindgen; check the common brew prefixes.
    let prefixes = [
        "/opt/homebrew/opt/llvm@17/lib",
        "/opt/homebrew/opt/llvm/lib",
        "/usr/local/opt/llvm@17/lib",
        "/usr/local/opt/llvm/lib",
    ];
    for prefix in prefixes {
        if Path::new(prefix).join("libclang.dylib").exists() {
            return Check::new("libclang", Status::Ok, prefix);
        }
    }
    Check::new("libclang", Status::Miss, "libclang not found (needed by bindgen)")
        .remedy("brew install llvm@17 && export LIBCLANG_PATH=\"$(brew --prefix llvm@17)/lib\"")
}

fn libclang_check_windows() -> Check {
    if let Some(check) = libclang_from_env() {
        return check;
    }
    let mut candidates = vec![PathBuf::from("C:\\Program Files\\LLVM\\bin")];
    if let Some(vs) = vswhere_install_path() {
        candidates
            .push(Path::new(&vs).join("VC").join("Tools").join("Llvm").join("x64").join("bin"));
    }
    for dir in candidates {
        if dir.join("libclang.dll").exists() {
            return Check::new("libclang", Status::Ok, dir.display().to_string());
        }
    }
    Check::new(
        "libclang",
        Status::Miss,
        "libclang.dll not found (needed by bindgen for PHP-linked builds)",
    )
    .remedy("install LLVM (winget install LLVM.LLVM) or set LIBCLANG_PATH")
}

// ── MSVC / WSL (Windows host) ────────────────────────────────────────────────

/// Locate the VS Build Tools installation path via vswhere.
fn vswhere_install_path() -> Option<String> {
    let pf86 =
        env::var("ProgramFiles(x86)").unwrap_or_else(|_| "C:\\Program Files (x86)".to_string());
    let vswhere =
        Path::new(&pf86).join("Microsoft Visual Studio").join("Installer").join("vswhere.exe");
    if !vswhere.exists() {
        return None;
    }
    let out = Command::new(vswhere)
        .args([
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "installationPath",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() { None } else { Some(path) }
}

/// Find the MSVC linker. vswhere first — `where link.exe` alone is unreliable
/// because Git for Windows ships a GNU coreutils `link.exe` on PATH.
fn msvc_linker_check() -> Check {
    if let Some(path) = vswhere_install_path() {
        return Check::new("msvc link.exe", Status::Ok, format!("VS Build Tools at {path}"));
    }
    if let Some(path) = where_msvc_link() {
        return Check::new("msvc link.exe", Status::Ok, path);
    }
    Check::new("msvc link.exe", Status::Miss, "MSVC linker not found").remedy(
        "install Visual Studio Build Tools with the 'Desktop development with C++' workload",
    )
}

/// `where link.exe`, keeping only hits that look like MSVC (not Git's GNU link).
fn where_msvc_link() -> Option<String> {
    let out = Command::new("where").arg("link.exe").output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("visual studio") || lower.contains("\\vc\\")
        })
        .map(String::from)
}

/// `cargo xtask release` (no --target) on Windows re-executes inside WSL.
fn wsl_check() -> Check {
    let ok = Command::new("wsl").arg("--status").output().is_ok_and(|o| o.status.success());
    if ok {
        Check::new("wsl", Status::Ok, "available (native release builds re-run inside WSL)")
    } else {
        Check::new("wsl", Status::Miss, "WSL not found - `cargo xtask release` runs inside WSL")
            .remedy("wsl --install")
    }
}

// ── PHP SDK cache ────────────────────────────────────────────────────────────

/// Silent equivalent of `host_php_sdk_platform()` — returns `None` instead of
/// printing errors for unsupported hosts.
fn sdk_platform() -> Option<(&'static str, &'static str)> {
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        return None;
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        return None;
    };
    if os == "macos" && arch != "aarch64" {
        return None; // only macos-aarch64 SDK artifacts are published
    }
    Some((os, arch))
}

/// Informational only (WARN, never MISS) — the SDK is auto-downloaded by
/// `cargo xtask release`, so an empty cache is not a failure.
fn php_sdk_checks(target: BuildTarget) -> Vec<Check> {
    let platform = match target {
        BuildTarget::Windows => Some(("windows", "x86_64")),
        BuildTarget::Native => sdk_platform(),
    };
    let Some((os, arch)) = platform else {
        return vec![
            Check::new("php-sdk", Status::Skip, "no prebuilt PHP SDK for this host platform")
                .optional(),
        ];
    };

    crate::PHP_SDK_VERSIONS
        .iter()
        .map(|(minor, full)| {
            let dir = crate::php_sdk_dir_for(full, os, arch);
            let lib = if os == "windows" { "php8embed.lib" } else { "libphp.a" };
            let name = format!("php-sdk {minor} ({full})");
            if dir.join("lib").join(lib).exists() {
                Check::new(&name, Status::Ok, format!("cached at {}", dir.display())).optional()
            } else {
                Check::new(&name, Status::Warn, "not cached - will be downloaded by xtask release")
                    .optional()
            }
        })
        .collect()
}

// ── Optional tools ───────────────────────────────────────────────────────────

fn container_engine_check() -> Check {
    for engine in ["podman", "docker"] {
        if let Some(line) = version_of(engine, &["--version"]) {
            return Check::new("container engine", Status::Ok, line).optional();
        }
    }
    Check::new("container engine", Status::Warn, "podman/docker not found (needed for e2e tests)")
        .remedy("install podman or docker")
        .optional()
}

fn spc_check() -> Check {
    match find_in_path("spc") {
        Some(path) => Check::new(
            "spc",
            Status::Ok,
            format!("{} (planned: used by ephpm forge, not required today)", path.display()),
        )
        .optional(),
        None => Check::new(
            "spc",
            Status::Skip,
            "not found (planned: used by ephpm forge, not required today)",
        )
        .optional(),
    }
}

fn gh_check() -> Check {
    match version_of("gh", &["--version"]) {
        Some(line) => Check::new("gh", Status::Ok, line).optional(),
        None => Check::new("gh", Status::Warn, "GitHub CLI not found (CI helper only)")
            .remedy("https://cli.github.com")
            .optional(),
    }
}

fn optional_checks() -> Vec<Check> {
    vec![container_engine_check(), spc_check(), gh_check()]
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stable_rustc_version() {
        assert_eq!(parse_rustc_version("rustc 1.94.0 (4a4ef493e 2026-03-02)"), Some((1, 94)));
    }

    #[test]
    fn parses_nightly_rustc_version() {
        assert_eq!(
            parse_rustc_version("rustc 1.95.0-nightly (abcdef012 2026-06-01)"),
            Some((1, 95))
        );
    }

    #[test]
    fn rejects_garbage_rustc_version() {
        assert_eq!(parse_rustc_version("clang version 17.0.6"), None);
        assert_eq!(parse_rustc_version(""), None);
        assert_eq!(parse_rustc_version("rustc x.y.z"), None);
    }

    #[test]
    fn msrv_comparison() {
        assert!(meets_msrv((1, 85)));
        assert!(meets_msrv((1, 94)));
        assert!(meets_msrv((2, 0)));
        assert!(!meets_msrv((1, 84)));
        assert!(!meets_msrv((0, 99)));
    }

    #[test]
    fn required_miss_fails_optional_miss_does_not() {
        let required_miss = Check::new("a", Status::Miss, "gone");
        let optional_miss = Check::new("b", Status::Miss, "gone").optional();
        let required_warn = Check::new("c", Status::Warn, "meh");

        assert!(has_required_failure([&required_miss].into_iter()));
        assert!(!has_required_failure([&optional_miss, &required_warn].into_iter()));
        assert!(!has_required_failure(std::iter::empty()));
    }

    #[test]
    fn render_aligns_names_and_prints_remedy_for_miss_only() {
        let sections = vec![(
            "Core",
            vec![
                Check::new("git", Status::Ok, "git version 2.50.0"),
                Check::new("longer-name", Status::Miss, "not found").remedy("install it"),
            ],
        )];
        let out = render(&sections);

        assert!(out.starts_with("Core\n"));
        assert!(out.contains("  [ OK ] git          git version 2.50.0\n"));
        assert!(out.contains("  [MISS] longer-name  not found\n"));
        // remedy line aligned to the detail column: 11 + len("longer-name")
        assert!(out.contains(&format!("{:indent$}remedy: install it\n", "", indent = 22)));
        // OK rows never print a remedy line
        assert_eq!(out.matches("remedy:").count(), 1);
    }

    #[test]
    fn render_is_ascii_only() {
        let sections = vec![(
            "Rust toolchain",
            vec![
                Check::new("rustc", Status::Ok, "rustc 1.94.0"),
                Check::new("wsl", Status::Skip, "only needed on Windows hosts"),
                Check::new("nightly rustfmt", Status::Warn, "unavailable").remedy("rustup"),
            ],
        )];
        assert!(render(&sections).is_ascii());
    }

    #[test]
    fn status_tags_are_fixed_width() {
        for status in [Status::Ok, Status::Miss, Status::Skip, Status::Warn] {
            assert_eq!(tag(status).len(), 6);
        }
    }
}
