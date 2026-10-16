//! `ephpm dev --xdebug` — step debugging with an ABI-matched Xdebug.
//!
//! A shared PHP extension loads only into the PHP it was built for: same
//! `ZEND_MODULE_API_NO`, same thread-safety mode (ePHPm is ZTS everywhere),
//! same libc. Distro Xdebug packages are NTS and are rejected, so this module
//! gets a ZTS build from the ePHPm extension catalog that `ephpm/php-sdk`
//! publishes per PHP release:
//!
//! ```text
//! https://github.com/ephpm/php-sdk/releases/download/ext-<php>/ephpm-ext-<php>-linux-<arch>-gnu.tar.gz
//!                                                            (macOS: ephpm-ext-<php>-macos-<arch>.tar.gz)
//!   ├── manifest.json   php_api_no / zts / os / libc / arch + per-file sha256
//!   ├── xdebug.so
//!   └── redis.so, apcu.so, …
//! ```
//!
//! Resolution, in order:
//!
//! 1. **Cache** — `<user cache dir>/ephpm/ext/<api>-zts-<arch>/xdebug.json`
//!    names a cached `xdebug-<version>.so` and its sha256. A hit whose bytes
//!    still hash to the recorded value is used with **no network at all**, so
//!    `--xdebug` works offline after the first run.
//! 2. **The binary's own release** — `ext-<PHP_VERSION>`, by direct download.
//! 3. **Same-minor fallback** — patch releases of one PHP minor share
//!    `ZEND_MODULE_API_NO` (that is the ABI promise), so when the exact release
//!    is missing or does not ship Xdebug, the newest `ext-<major>.<minor>.*`
//!    whose manifest declares the *same* API number is used instead, and the
//!    banner says so. Releases are listed through the GitHub REST API,
//!    unauthenticated; a rate-limit response becomes a clear error.
//!
//! Every archive is verified before anything is written: the manifest's
//! `php_api_no`, `zts`, `os`, `libc`, and `arch` must match this binary, every
//! listed file must be present and hash to its manifest sha256, and a mismatch
//! is a hard refusal (never a silent skip to another release). The hashes are
//! an **integrity** check against corrupted or inconsistent archives, not an
//! authenticity one: the manifest travels in the same tarball, so whoever can
//! replace a release asset can rewrite both. Authenticity rests on HTTPS to
//! the catalog host (plain http is refused except to a loopback mirror) and
//! on who can publish php-sdk releases. Only the verified `xdebug.so` is
//! written to disk; nothing in the tarball is extracted by path.
//!
//! `--xdebug-path <file>` skips the catalog entirely (no network).
//!
//! The library is then loaded through the generated php.ini as a **Zend
//! extension with an absolute path** (`zend_extension=` resolves a relative
//! value against `extension_dir`), with `xdebug.mode=debug,develop`,
//! `xdebug.start_with_request=trigger`, and the IDE address. The request
//! deadline and `max_execution_time` are lifted so a request paused on a
//! breakpoint is not killed. After PHP starts, [`verify_after_init`] proves
//! Xdebug registered as a Zend extension and prints the banner.
//!
//! Platforms: Linux (glibc, x86_64/aarch64) and macOS (arm64) use the catalog.
//! A macOS bundle resolves its PHP imports against the ephpm executable at
//! load time, so it needs a binary linked with `-export_dynamic`
//! (`crates/ephpm/build.rs`); older macOS binaries fail with "symbol not found
//! in flat namespace". On Windows it depends
//! on the build: the default binary links PHP statically (`php8embed.lib`),
//! which cannot load extension DLLs at all, so `--xdebug` is refused there; a
//! **shared-engine** build (`cargo xtask release --target windows --variant
//! shared`, `php8ts.dll` beside `ephpm.exe` — see `ephpm_php::shared`) loads
//! `php_xdebug.dll` from the catalog's `ephpm-ext-<php>-windows-x86_64.tar.gz`
//! (manifest `libc` = `msvc`), the official xdebug.org ZTS build for that PHP
//! minor.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, bail};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

/// Default `xdebug.client_host`: an IDE on the same machine (or in the same
/// WSL distro).
pub(crate) const DEFAULT_CLIENT_HOST: &str = "127.0.0.1";

/// Default `xdebug.client_port` — Xdebug 3's default, and what VS Code's
/// "Listen for Xdebug" and PhpStorm listen on.
pub(crate) const DEFAULT_CLIENT_PORT: u16 = 9003;

/// Where catalog tarballs are downloaded from: `<base>/<tag>/<asset>`.
const DEFAULT_DOWNLOAD_BASE: &str = "https://github.com/ephpm/php-sdk/releases/download";

/// GitHub REST endpoint listing php-sdk releases (for the same-minor fallback).
const DEFAULT_RELEASES_API: &str = "https://api.github.com/repos/ephpm/php-sdk/releases";

/// Overrides the download base, e.g. an internal mirror of the catalog
/// release assets laid out as `<base>/<tag>/<asset>`.
pub(crate) const CATALOG_URL_ENV: &str = "EPHPM_EXT_CATALOG_URL";

/// Overrides the releases-listing endpoint (GitHub REST API shape).
pub(crate) const CATALOG_API_ENV: &str = "EPHPM_EXT_CATALOG_API";

/// Largest catalog tarball accepted (compressed). Today's is ~5 MB.
const MAX_TARBALL_BYTES: u64 = 256 * 1024 * 1024;

/// Largest total unpacked size accepted (decompression-bomb guard).
const MAX_UNPACKED_BYTES: u64 = 512 * 1024 * 1024;

/// Largest releases-listing JSON accepted.
const MAX_LISTING_BYTES: u64 = 16 * 1024 * 1024;

/// How many same-minor fallback releases are downloaded at most before
/// giving up (each is a full catalog tarball).
const MAX_FALLBACK_ATTEMPTS: usize = 4;

/// The extension's catalog / registry name.
const XDEBUG: &str = "xdebug";

// ─────────────────────────────────────────────────────────────────────────────
// CLI
// ─────────────────────────────────────────────────────────────────────────────

/// `ephpm dev` flags for Xdebug step debugging.
#[derive(clap::Args, Debug, Default, Clone)]
#[command(next_help_heading = "Xdebug (step debugging, dev only)")]
pub(crate) struct XdebugArgs {
    /// Load Xdebug for step debugging. On Linux and macOS an ABI-matched ZTS
    /// build is downloaded from the ePHPm extension catalog
    /// (github.com/ephpm/php-sdk) on first use and cached; later runs work
    /// offline. On Windows this needs the shared-engine build (php8ts.dll
    /// beside ephpm.exe).
    #[arg(long)]
    pub xdebug: bool,

    /// Load this local Xdebug shared object instead of the catalog build
    /// (implies --xdebug; no network). Must be a ZTS build for this binary's
    /// PHP minor.
    #[arg(long, value_name = "FILE")]
    pub xdebug_path: Option<PathBuf>,

    /// Host the IDE listens on (xdebug.client_host) [default: 127.0.0.1]. From
    /// WSL with the IDE on Windows, use the Windows host's IP.
    #[arg(long, value_name = "HOST")]
    pub xdebug_client_host: Option<String>,

    /// Port the IDE listens on (xdebug.client_port) [default: 9003].
    #[arg(long, value_name = "PORT")]
    pub xdebug_client_port: Option<u16>,
}

impl XdebugArgs {
    /// Whether Xdebug was requested (`--xdebug` or `--xdebug-path`).
    pub(crate) fn enabled(&self) -> bool {
        self.xdebug || self.xdebug_path.is_some()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry points
// ─────────────────────────────────────────────────────────────────────────────

/// What [`verify_after_init`] must find, and what the banner reports.
#[derive(Debug)]
struct Expected {
    /// Absolute path of the library PHP was told to load.
    so_path: PathBuf,
    /// Where it came from, for the banner.
    origin: Origin,
    /// `xdebug.client_host`.
    client_host: String,
    /// `xdebug.client_port`.
    client_port: u16,
    /// The dev server's base URL, for the trigger example.
    url: String,
    /// The embedded PHP version, for error messages.
    php_version: String,
}

static EXPECTED: OnceLock<Expected> = OnceLock::new();

/// Resolve the Xdebug library for `ephpm dev` and wire it into `config`.
///
/// No-op when neither `--xdebug` nor `--xdebug-path` was given (but the
/// client host/port flags are refused on their own, so they cannot look like
/// they did something). Must run before PHP is initialized and before any
/// tokio runtime exists — the catalog client's background thread is joined
/// before this returns.
///
/// # Errors
///
/// Refuses on unsupported platforms, in stub builds, on an ABI or checksum
/// mismatch, and when no usable build can be found (with a message naming
/// `--xdebug-path` as the way out).
pub(crate) fn prepare(args: &XdebugArgs, config: &mut ephpm_config::Config) -> anyhow::Result<()> {
    if !args.enabled() {
        if args.xdebug_client_host.is_some() || args.xdebug_client_port.is_some() {
            bail!(
                "--xdebug-client-host / --xdebug-client-port only apply together with --xdebug \
                 (or --xdebug-path)"
            );
        }
        return Ok(());
    }

    if cfg!(windows) && !ephpm_php::shared::SharedRuntime::is_shared_build() {
        bail!(
            "--xdebug is not available on Windows with this build: it links PHP statically \
             (php8embed.lib), and a static PHP cannot load extension DLLs such as \
             php_xdebug.dll. Use the shared-engine Windows build (php8ts.dll beside ephpm.exe; \
             `cargo xtask release --target windows --variant shared`), or ePHPm under WSL."
        );
    }

    let Some(abi) = ephpm_php::PhpRuntime::abi() else {
        bail!(
            "--xdebug needs a PHP-linked ephpm, but this binary was built without PHP (stub \
             mode: no PHP_SDK_PATH at build time). Build with `cargo xtask release`."
        );
    };

    let client_host =
        validate_client_host(args.xdebug_client_host.as_deref().unwrap_or(DEFAULT_CLIENT_HOST))?;
    let client_port = args.xdebug_client_port.unwrap_or(DEFAULT_CLIENT_PORT);
    if client_port == 0 {
        bail!("--xdebug-client-port must be between 1 and 65535");
    }

    let resolved = if let Some(path) = &args.xdebug_path {
        resolve_local(path)?
    } else {
        let Some(os) = CatalogOs::current() else {
            bail!(
                "--xdebug: the ePHPm extension catalog only publishes glibc Linux, macOS and \
                 Windows builds. Pass --xdebug-path <xdebug.so> with a ZTS build for PHP {}.",
                abi.php_version
            );
        };
        if !abi.zts {
            bail!(
                "--xdebug: the extension catalog publishes ZTS builds only, but this binary's \
                 PHP is NTS. Pass --xdebug-path <xdebug.so> with an NTS build."
            );
        }
        let target = Target {
            php_version: abi.php_version.clone(),
            api_no: abi.api_no,
            zts: abi.zts,
            arch: std::env::consts::ARCH.to_string(),
            os,
        };
        let cache_root = user_cache_root().context(
            "--xdebug: cannot determine a cache directory (set XDG_CACHE_HOME or HOME; \
             LOCALAPPDATA on Windows), or pass --xdebug-path",
        )?;
        let cache = ExtCache::new(&cache_root, &target);
        // Scope the client so its runtime thread is joined before PHP starts.
        let resolved = {
            let source = HttpCatalog::from_env()?;
            resolve_from_catalog(&source, &target, &cache, &mut |line| {
                eprintln!("  xdebug: {line}");
            })?
        };
        if !resolved.zend_extension && ext_is_not_zend(&resolved.path) {
            bail!(
                "--xdebug: the catalog's {} does not export zend_extension_entry, so it cannot \
                 be loaded as a Zend extension — refusing to use it",
                resolved.path.display()
            );
        }
        resolved
    };

    apply_to_config(config, &resolved.path, &client_host, client_port)?;

    let _ = EXPECTED.set(Expected {
        so_path: resolved.path,
        origin: resolved.origin,
        client_host,
        client_port,
        url: format!("http://{}", config.server.listen),
        php_version: abi.php_version,
    });
    Ok(())
}

/// After PHP startup: prove Xdebug registered as a Zend extension, then print
/// the banner. No-op unless [`prepare`] armed it.
///
/// # Errors
///
/// When Xdebug is not loaded (PHP rejected it — the reason is on stderr from
/// PHP's own startup message) or was loaded as a plain module only.
pub(crate) fn verify_after_init() -> anyhow::Result<()> {
    let Some(exp) = EXPECTED.get() else {
        return Ok(());
    };
    if let Some(version) = ephpm_php::PhpRuntime::zend_extension_version(XDEBUG) {
        print_banner(exp, &version);
        return Ok(());
    }
    if ephpm_php::PhpRuntime::loaded_module_version(XDEBUG).is_some() {
        bail!(
            "Xdebug from {} loaded as a plain PHP module, not as a Zend extension, so step \
             debugging is unavailable",
            exp.so_path.display()
        );
    }
    bail!(
        "Xdebug did not load from {} — PHP rejected it at startup (see the \"Failed loading\" / \
         \"Unable to load\" message above). The usual cause is an ABI mismatch: the build must be \
         ZTS for PHP {} and the same libc as this binary.",
        exp.so_path.display(),
        exp.php_version
    );
}

/// The post-startup banner (stdout, like the dev banner).
fn print_banner(exp: &Expected, version: &str) {
    let version = if version.is_empty() { "(unknown version)" } else { version };
    let origin = match &exp.origin {
        Origin::LocalPath => "--xdebug-path".to_string(),
        Origin::Cache { release } => format!("cached from catalog {release}"),
        Origin::Catalog { release, fallback_from: None } => format!("catalog {release}"),
        Origin::Catalog { release, fallback_from: Some(wanted) } => {
            format!("catalog {release}, same PHP ABI as {wanted}")
        }
    };
    println!("  Xdebug {version} loaded (dev only: never load Xdebug in production)");
    println!("    library:  {} ({origin})", exp.so_path.display());
    println!(
        "    ide:      DBGp to {}:{} (start the IDE listening first)",
        exp.client_host, exp.client_port
    );
    println!("    trigger:  {}/?XDEBUG_TRIGGER=1", exp.url);
    println!("              or cookie XDEBUG_SESSION=1, or a browser \"Xdebug helper\" extension");
    println!("    timeouts: request deadline and max_execution_time lifted while debugging");
    println!(
        "    vscode:   .vscode/launch.json → {{ \"name\": \"Listen for Xdebug\", \"type\": \
         \"php\", \"request\": \"launch\", \"port\": {} }}",
        exp.client_port
    );
    println!("              (no pathMappings needed when the IDE and ephpm share the filesystem)");
    println!();
}

/// Wire the resolved library and the debug settings into the dev config.
///
/// The `zend_extension` line is **absolute** and quoted; the `xdebug.*`
/// settings follow it in the same `ini_overrides` block, so they apply to an
/// already-declared extension. The request deadline is disabled
/// (`[server.timeouts] request = 0`) and, on builds that enforce it natively,
/// `max_execution_time` is set to `0`: a request sitting on a breakpoint must
/// not be cut off.
///
/// # Errors
///
/// When the path cannot be written into the generated php.ini safely.
pub(crate) fn apply_to_config(
    config: &mut ephpm_config::Config,
    so_path: &Path,
    client_host: &str,
    client_port: u16,
) -> anyhow::Result<()> {
    let mut path = so_path.to_string_lossy().into_owned();
    // php.ini unescapes `\\` (and `\"`, `\$`) inside double quotes, which
    // would mangle a UNC path; Windows accepts `/`, so use it there. (On Unix
    // a backslash is an ordinary file-name byte and is left alone.)
    if cfg!(windows) {
        path = path.replace('\\', "/");
    }
    // Inside a double-quoted ini value `${...}` is expanded and `"` ends it;
    // a line break would start a second directive.
    if !so_path.is_absolute() || path.contains(['"', '\n', '\r', '\0']) || path.contains("${") {
        bail!(
            "cannot load Xdebug from {path}: the path must be absolute and must not contain \
             quotes, line breaks, or \"${{\""
        );
    }
    let overrides = &mut config.php.ini_overrides;
    let quoted = crate::ext_ini::ini_quote(&path).context("Xdebug path is not ini-safe")?;
    overrides.push(["zend_extension".into(), quoted]);
    overrides.push(["xdebug.mode".into(), "debug,develop".into()]);
    overrides.push(["xdebug.start_with_request".into(), "trigger".into()]);
    overrides.push(["xdebug.client_host".into(), client_host.to_string()]);
    overrides.push(["xdebug.client_port".into(), client_port.to_string()]);
    config.server.timeouts.request = 0;
    lift_max_execution_time(config);
    Ok(())
}

/// Disable PHP's own execution timer where it is natively enforced. Elsewhere
/// it is not enforced at all, and changing it would only trip the startup
/// warning that says so.
#[cfg(php_max_exec_timers)]
fn lift_max_execution_time(config: &mut ephpm_config::Config) {
    config.php.max_execution_time = 0;
}

/// See the `php_max_exec_timers` variant: nothing to lift on this build.
#[cfg(not(php_max_exec_timers))]
fn lift_max_execution_time(_config: &mut ephpm_config::Config) {}

/// `xdebug.client_host` lands in the generated php.ini, so only hostname /
/// IP characters are accepted.
fn validate_client_host(host: &str) -> anyhow::Result<String> {
    let host = host.trim();
    let ok = !host.is_empty()
        && host.len() <= 253
        && host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'));
    if !ok {
        bail!("--xdebug-client-host {host:?} is not a valid hostname or IP address");
    }
    Ok(host.to_string())
}

/// `--xdebug-path`: an existing file, made absolute, that is a Zend extension.
fn resolve_local(path: &Path) -> anyhow::Result<Resolved> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().context("failed to read current directory")?.join(path)
    };
    if !absolute.is_file() {
        bail!("--xdebug-path {}: no such file", absolute.display());
    }
    match crate::ext_ini::is_zend_extension(&absolute) {
        Some(true) => {}
        Some(false) => bail!(
            "--xdebug-path {}: not a Zend extension (it exports no zend_extension_entry) — is \
             this really xdebug.so?",
            absolute.display()
        ),
        None => bail!(
            "--xdebug-path {}: not a shared library this platform can load",
            absolute.display()
        ),
    }
    Ok(Resolved { path: absolute, version: None, origin: Origin::LocalPath, zend_extension: true })
}

/// `true` only when the library is parseable and definitely lacks the Zend
/// extension entry point.
fn ext_is_not_zend(path: &Path) -> bool {
    crate::ext_ini::is_zend_extension(path) == Some(false)
}

// ─────────────────────────────────────────────────────────────────────────────
// Target identity
// ─────────────────────────────────────────────────────────────────────────────

/// The ABI a catalog build must match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    /// Embedded `PHP_VERSION`, e.g. `8.5.7`.
    pub php_version: String,
    /// `ZEND_MODULE_API_NO`, e.g. `20250925`.
    pub api_no: u64,
    /// ZTS build.
    pub zts: bool,
    /// `std::env::consts::ARCH` — `x86_64` / `aarch64`, the catalog's spelling.
    pub arch: String,
    /// Which catalog flavor (asset name, manifest `os` / `libc`, library
    /// suffix).
    pub os: CatalogOs,
}

/// The platforms the extension catalog publishes for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatalogOs {
    /// glibc Linux: `ephpm-ext-<php>-linux-<arch>-gnu.tar.gz`, `.so` files,
    /// manifest `libc` = `gnu` and no `os` key (php-sdk `extensions.yml`).
    LinuxGnu,
    /// macOS: `ephpm-ext-<php>-macos-<arch>.tar.gz`, `.so` files, manifest
    /// `os` = `macos` and no `libc` key (php-sdk `extensions.yml`,
    /// `build-extensions-macos`).
    Macos,
    /// Windows (official windows.php.net ABI):
    /// `ephpm-ext-<php>-windows-<arch>.tar.gz`, `.dll` files, manifest `libc`
    /// `msvc` (php-sdk `windows-shared.yml`). Loadable only by a shared-engine
    /// build.
    Windows,
}

impl CatalogOs {
    /// This binary's catalog flavor, if the catalog publishes one for it.
    pub(crate) fn current() -> Option<Self> {
        if cfg!(all(target_os = "linux", target_env = "gnu")) {
            Some(Self::LinuxGnu)
        } else if cfg!(target_os = "macos") {
            Some(Self::Macos)
        } else if cfg!(windows) {
            Some(Self::Windows)
        } else {
            None
        }
    }

    /// The manifest `os` value this flavor matches.
    fn manifest_os(self) -> &'static str {
        match self {
            Self::LinuxGnu => "linux",
            Self::Macos => "macos",
            Self::Windows => "windows",
        }
    }

    /// Check a manifest's `os` / `libc` against this flavor.
    ///
    /// Linux manifests predate the `os` key, so on Linux it may be absent but
    /// must say `linux` if present, and `libc` must be `gnu` if present. A
    /// macOS manifest must say `os: macos` — which is also what rejects a
    /// Linux catalog on macOS — and must not name a libc. A Windows manifest
    /// must say `libc: msvc` (a .dll's C runtime is not something to assume),
    /// and `os` must be `windows` if present.
    fn check(self, manifest: &Manifest) -> Result<(), CatalogError> {
        let os = manifest.os.as_deref().map(str::trim);
        let os_ok = match self {
            Self::LinuxGnu => os.is_none_or(|o| o == "linux"),
            Self::Macos => os == Some("macos"),
            Self::Windows => os.is_none_or(|o| o == "windows"),
        };
        if !os_ok {
            return Err(CatalogError::OsMismatch {
                expected: self.manifest_os(),
                found: os.unwrap_or("(unset)").to_string(),
            });
        }
        let libc = manifest.libc.as_deref().map(str::trim);
        let (libc_ok, expected) = match self {
            Self::LinuxGnu => (libc.is_none_or(|l| l == "gnu"), "gnu"),
            Self::Macos => (libc.is_none(), "(none)"),
            Self::Windows => (libc == Some("msvc"), "msvc"),
        };
        if !libc_ok {
            return Err(CatalogError::LibcMismatch {
                found: libc.unwrap_or("(unset)").to_string(),
                expected,
            });
        }
        Ok(())
    }

    /// Shared-library file extension, for the cached file's name.
    fn lib_suffix(self) -> &'static str {
        match self {
            Self::LinuxGnu | Self::Macos => "so",
            Self::Windows => "dll",
        }
    }
}

impl Target {
    /// The catalog asset name for release `version` (`8.5.11`).
    pub(crate) fn asset_name(&self, version: &str) -> String {
        match self.os {
            CatalogOs::LinuxGnu => format!("ephpm-ext-{version}-linux-{}-gnu.tar.gz", self.arch),
            CatalogOs::Macos => format!("ephpm-ext-{version}-macos-{}.tar.gz", self.arch),
            CatalogOs::Windows => format!("ephpm-ext-{version}-windows-{}.tar.gz", self.arch),
        }
    }

    /// A human name for this platform's catalog flavor (`macOS aarch64`).
    fn platform_label(&self) -> String {
        match self.os {
            CatalogOs::LinuxGnu => format!("Linux {} (glibc)", self.arch),
            CatalogOs::Macos => format!("macOS {}", self.arch),
            CatalogOs::Windows => format!("Windows {}", self.arch),
        }
    }

    /// The cache subdirectory: everything that determines ABI compatibility.
    fn cache_key(&self) -> String {
        format!("{}-{}-{}", self.api_no, if self.zts { "zts" } else { "nts" }, self.arch)
    }
}

/// The user's cache directory: `$XDG_CACHE_HOME` or `~/.cache` on Linux,
/// `~/Library/Caches` on macOS, `%LOCALAPPDATA%` on Windows.
fn user_cache_root() -> Option<PathBuf> {
    let env_dir = |key: &str| std::env::var_os(key).map(PathBuf::from).filter(|p| p.is_absolute());
    if cfg!(windows) {
        return env_dir("LOCALAPPDATA");
    }
    if cfg!(target_os = "macos") {
        return env_dir("HOME").map(|home| home.join("Library").join("Caches"));
    }
    env_dir("XDG_CACHE_HOME").or_else(|| env_dir("HOME").map(|home| home.join(".cache")))
}

// ─────────────────────────────────────────────────────────────────────────────
// Manifest + archive verification
// ─────────────────────────────────────────────────────────────────────────────

/// `manifest.json` at the root of a catalog tarball.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Manifest {
    /// The PHP release the catalog was built against.
    pub php_version: String,
    /// `ZEND_MODULE_API_NO` of that PHP.
    #[serde(deserialize_with = "de_api_no")]
    pub php_api_no: String,
    /// ZTS build (`1`/`true`).
    #[serde(deserialize_with = "de_flag")]
    pub zts: bool,
    /// `macos` / `windows` on those catalogs; absent on Linux ones.
    #[serde(default)]
    pub os: Option<String>,
    /// `gnu` (glibc) on Linux catalogs, `msvc` on Windows; absent on macOS.
    #[serde(default)]
    pub libc: Option<String>,
    /// `x86_64` / `aarch64`.
    pub arch: String,
    /// One entry per shared object in the tarball.
    pub extensions: Vec<ManifestExt>,
}

/// One extension in a [`Manifest`].
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ManifestExt {
    /// Registry name (`xdebug`).
    pub name: String,
    /// File name inside the tarball (`xdebug.so`).
    pub file: String,
    /// Extension version, `"unknown"` in older manifests.
    #[serde(default)]
    pub version: Option<String>,
    /// `ZEND_MODULE_API_NO` this file was built for.
    #[serde(deserialize_with = "de_api_no")]
    pub php_api_no: String,
    /// ZTS build.
    #[serde(deserialize_with = "de_flag")]
    pub zts: bool,
    /// Hex sha256 of the file.
    pub sha256: String,
    /// Load with `zend_extension=`. Missing means `false`; the library is also
    /// inspected independently.
    #[serde(default, deserialize_with = "de_flag")]
    pub zend_extension: bool,
}

impl ManifestExt {
    /// The version, unless the manifest says it does not know it.
    fn known_version(&self) -> Option<&str> {
        self.version.as_deref().map(str::trim).filter(|v| !v.is_empty() && *v != "unknown")
    }
}

/// Accept `1`, `true`, `"1"`, `"true"` as true; `0`/`false`/`null` as false.
fn de_flag<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    use serde_json::Value;
    Ok(match Value::deserialize(d)? {
        Value::Bool(b) => b,
        Value::Number(n) => n.as_u64() == Some(1),
        Value::String(s) => matches!(s.trim(), "1" | "true"),
        _ => false,
    })
}

/// Accept the API number as a JSON string or number.
fn de_api_no<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    use serde_json::Value;
    match Value::deserialize(d)? {
        Value::String(s) => Ok(s.trim().to_string()),
        Value::Number(n) => Ok(n.to_string()),
        other => Err(serde::de::Error::custom(format!("php_api_no must be a string, got {other}"))),
    }
}

/// Why a catalog archive was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum CatalogError {
    /// The manifest is for another PHP API.
    #[error(
        "catalog is built for PHP API {found}, but this ephpm embeds PHP {php_version} (API \
         {expected})"
    )]
    ApiMismatch {
        /// This binary's API number.
        expected: u64,
        /// The manifest's.
        found: String,
        /// This binary's PHP version.
        php_version: String,
    },
    /// ZTS/NTS disagreement.
    #[error("catalog is {found}, but this ephpm embeds a {expected} PHP")]
    ZtsMismatch {
        /// This binary's mode.
        expected: &'static str,
        /// The manifest's.
        found: &'static str,
    },
    /// Built for another CPU.
    #[error("catalog is built for {found}, but this ephpm is {expected}")]
    ArchMismatch {
        /// This binary's arch.
        expected: String,
        /// The manifest's.
        found: String,
    },
    /// Built for another operating system.
    #[error("catalog is built for os {found:?}, but this ephpm needs {expected:?}")]
    OsMismatch {
        /// What this binary needs (`linux`, `macos`, `windows`).
        expected: &'static str,
        /// The manifest's `os`.
        found: String,
    },
    /// Built against another libc / C runtime.
    #[error("catalog is built for libc {found:?}, but this ephpm needs {expected:?}")]
    LibcMismatch {
        /// The manifest's libc.
        found: String,
        /// What this binary needs (`gnu`, `msvc`, or none on macOS).
        expected: &'static str,
    },
    /// One file's manifest entry disagrees with the binary.
    #[error("catalog entry {file} is built for PHP API {found} / zts={zts}, not this binary's API")]
    EntryAbiMismatch {
        /// The file.
        file: String,
        /// Its API number.
        found: String,
        /// Its ZTS flag.
        zts: bool,
    },
    /// A file's bytes do not hash to its manifest sha256.
    #[error(
        "{file}: sha256 mismatch (manifest {expected}, archive {actual}) — refusing a corrupted \
         or inconsistent catalog download"
    )]
    ShaMismatch {
        /// The file.
        file: String,
        /// Manifest sha256.
        expected: String,
        /// Actual sha256.
        actual: String,
    },
    /// A listed file is not in the archive.
    #[error("manifest lists {file}, but the archive does not contain it")]
    MissingFile {
        /// The file.
        file: String,
    },
    /// Not a readable catalog tarball.
    #[error("malformed catalog archive: {0}")]
    Malformed(String),
}

/// A catalog tarball, unpacked into memory.
#[derive(Debug)]
pub(crate) struct CatalogArchive {
    /// The parsed `manifest.json`.
    pub manifest: Manifest,
    /// Top-level regular files by name (`./` stripped). Anything nested is
    /// ignored — names come from the manifest, never from tar paths.
    pub files: HashMap<String, Vec<u8>>,
}

impl CatalogArchive {
    /// The Xdebug entry, if this release ships it.
    pub(crate) fn xdebug(&self) -> Option<&ManifestExt> {
        self.manifest.extensions.iter().find(|e| e.name.eq_ignore_ascii_case(XDEBUG))
    }
}

/// Unpack a `.tar.gz` catalog into memory (bounded).
pub(crate) fn unpack_catalog(tarball: &[u8]) -> Result<CatalogArchive, CatalogError> {
    use std::path::Component;

    let gz = flate2::read::GzDecoder::new(tarball).take(MAX_UNPACKED_BYTES);
    let mut archive = tar::Archive::new(gz);
    let mut files = HashMap::new();
    let entries = archive.entries().map_err(|e| CatalogError::Malformed(e.to_string()))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| CatalogError::Malformed(e.to_string()))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().map_err(|e| CatalogError::Malformed(e.to_string()))?;
        let mut parts = path.components().filter(|c| !matches!(c, Component::CurDir));
        let (Some(Component::Normal(name)), None) = (parts.next(), parts.next()) else {
            continue;
        };
        let name = name.to_string_lossy().into_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).map_err(|e| CatalogError::Malformed(e.to_string()))?;
        files.insert(name, bytes);
    }
    let manifest_bytes = files
        .remove("manifest.json")
        .ok_or_else(|| CatalogError::Malformed("no manifest.json".into()))?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| CatalogError::Malformed(format!("manifest.json: {e}")))?;
    Ok(CatalogArchive { manifest, files })
}

/// Hex sha256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Check the manifest's ABI against the binary — the release-level gate that
/// the same-minor fallback uses to skip incompatible releases.
fn check_manifest_abi(manifest: &Manifest, target: &Target) -> Result<(), CatalogError> {
    if manifest.php_api_no != target.api_no.to_string() {
        return Err(CatalogError::ApiMismatch {
            expected: target.api_no,
            found: manifest.php_api_no.clone(),
            php_version: target.php_version.clone(),
        });
    }
    let mode = |zts: bool| if zts { "ZTS" } else { "NTS" };
    if manifest.zts != target.zts {
        return Err(CatalogError::ZtsMismatch {
            expected: mode(target.zts),
            found: mode(manifest.zts),
        });
    }
    if manifest.arch != target.arch {
        return Err(CatalogError::ArchMismatch {
            expected: target.arch.clone(),
            found: manifest.arch.clone(),
        });
    }
    target.os.check(manifest)
}

/// Full verification: ABI identity, every entry's ABI, every file's presence
/// and sha256.
pub(crate) fn validate_catalog(
    archive: &CatalogArchive,
    target: &Target,
) -> Result<(), CatalogError> {
    check_manifest_abi(&archive.manifest, target)?;
    for ext in &archive.manifest.extensions {
        if ext.file.is_empty() || ext.file.contains(['/', '\\']) || ext.file == ".." {
            return Err(CatalogError::Malformed(format!("bad file name {:?}", ext.file)));
        }
        if ext.php_api_no != target.api_no.to_string() || ext.zts != target.zts {
            return Err(CatalogError::EntryAbiMismatch {
                file: ext.file.clone(),
                found: ext.php_api_no.clone(),
                zts: ext.zts,
            });
        }
        let bytes = archive
            .files
            .get(&ext.file)
            .ok_or_else(|| CatalogError::MissingFile { file: ext.file.clone() })?;
        let actual = sha256_hex(bytes);
        if !actual.eq_ignore_ascii_case(ext.sha256.trim()) {
            return Err(CatalogError::ShaMismatch {
                file: ext.file.clone(),
                expected: ext.sha256.trim().to_ascii_lowercase(),
                actual,
            });
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Release selection
// ─────────────────────────────────────────────────────────────────────────────

/// One release from the listing endpoint.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ReleaseInfo {
    /// `ext-8.5.11`.
    #[serde(rename = "tag_name")]
    pub tag: String,
    /// Drafts are never used.
    #[serde(default)]
    pub draft: bool,
    /// Pre-releases are never used.
    #[serde(default)]
    pub prerelease: bool,
    /// Asset names.
    #[serde(default, deserialize_with = "de_asset_names")]
    pub assets: Vec<String>,
}

/// GitHub's `assets: [{ "name": ... }]` → names.
fn de_asset_names<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    struct Asset {
        name: String,
    }
    Ok(Vec::<Asset>::deserialize(d)?.into_iter().map(|a| a.name).collect())
}

/// `8.5.11` → `(8, 5, 11)`; anything else (`8.5.10-ghreltest`, `8.6.0beta1`)
/// → `None`.
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.split('.');
    let mut num = || -> Option<u64> {
        let part = it.next()?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let parsed = (num()?, num()?, num()?);
    it.next().is_none().then_some(parsed)
}

/// The binary's own `major.minor`, tolerating a suffixed patch (`8.5.8-dev`,
/// `8.6.0beta1`): the embedded `PHP_VERSION` is not always a release number
/// (an SDK built from a php-src branch tip reports e.g. `8.5.8-dev`; the
/// v0.5.1-v0.11.2 macOS binaries labelled php8.5.7 do), and the fallback only
/// needs the minor. Catalog tags stay strict — see [`parse_version`].
fn parse_minor(v: &str) -> Option<(u64, u64)> {
    let mut it = v.split('.');
    let mut num = || -> Option<u64> {
        let part = it.next()?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let parsed = (num()?, num()?);
    // A third component must at least start with the numeric patch.
    it.next().is_some_and(|p| p.starts_with(|c: char| c.is_ascii_digit())).then_some(parsed)
}

/// Same-minor `ext-X.Y.Z` releases other than `php_version` itself that carry
/// this arch's asset, newest patch first. Drafts, pre-releases, and suffixed
/// tags are excluded. ABI identity is confirmed later from each manifest.
pub(crate) fn select_fallback_releases(releases: &[ReleaseInfo], target: &Target) -> Vec<String> {
    let Some((major, minor)) = parse_minor(&target.php_version) else {
        return Vec::new();
    };
    let mut picked: Vec<(u64, String)> = releases
        .iter()
        .filter(|r| !r.draft && !r.prerelease)
        .filter_map(|r| {
            let version = r.tag.strip_prefix("ext-")?;
            let (ma, mi, patch) = parse_version(version)?;
            let asset = target.asset_name(version);
            (ma == major
                && mi == minor
                && version != target.php_version
                && r.assets.contains(&asset))
            .then(|| (patch, version.to_string()))
        })
        .collect();
    picked.sort_by_key(|p| std::cmp::Reverse(p.0));
    picked.dedup_by(|a, b| a.0 == b.0);
    picked.into_iter().map(|(_, v)| v).collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// Cache
// ─────────────────────────────────────────────────────────────────────────────

/// `xdebug.json` beside the cached library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CacheRecord {
    /// File name in the cache directory.
    pub file: String,
    /// Its sha256 (from the verified manifest).
    pub sha256: String,
    /// Xdebug version, when the manifest knew it.
    pub version: Option<String>,
    /// The catalog release it came from (`ext-8.5.11`).
    pub release: String,
    /// That release's PHP version.
    pub php_version: String,
    /// Whether the manifest flagged it `zend_extension`.
    #[serde(default)]
    pub zend_extension: bool,
}

/// Per-ABI cache directory for catalog builds.
#[derive(Debug, Clone)]
pub(crate) struct ExtCache {
    dir: PathBuf,
}

impl ExtCache {
    /// `<root>/ephpm/ext/<api>-<zts|nts>-<arch>`.
    pub(crate) fn new(root: &Path, target: &Target) -> Self {
        Self { dir: root.join("ephpm").join("ext").join(target.cache_key()) }
    }

    /// The cache directory.
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    fn record_path(&self) -> PathBuf {
        self.dir.join(format!("{XDEBUG}.json"))
    }

    /// A cached build whose bytes still match the recorded sha256. A cache
    /// directory another user could write to is never trusted (see
    /// [`dir_is_private`]): the record and the library sit side by side, so
    /// whoever can write there could plant a matching pair.
    pub(crate) fn lookup(&self) -> Option<(PathBuf, CacheRecord)> {
        if !dir_is_private(&self.dir) {
            return None;
        }
        let record: CacheRecord =
            serde_json::from_slice(&std::fs::read(self.record_path()).ok()?).ok()?;
        if record.file.contains(['/', '\\']) {
            return None;
        }
        let path = self.dir.join(&record.file);
        let bytes = std::fs::read(&path).ok()?;
        (sha256_hex(&bytes) == record.sha256).then_some((path, record))
    }

    /// Write `bytes` + its record atomically (temp file + rename).
    pub(crate) fn store(&self, bytes: &[u8], record: &CacheRecord) -> anyhow::Result<PathBuf> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("failed to create cache dir {}", self.dir.display()))?;
        if !dir_is_private(&self.dir) {
            bail!(
                "refusing to cache Xdebug in {}: the directory is not owned by the current user \
                 or is world-writable (set XDG_CACHE_HOME to a private directory)",
                self.dir.display()
            );
        }
        let path = self.dir.join(&record.file);
        write_atomic(&path, bytes)?;
        write_atomic(&self.record_path(), &serde_json::to_vec_pretty(record)?)?;
        Ok(path)
    }
}

/// Whether `dir` is a directory another user cannot plant files in: owned by
/// the effective uid and not world-writable (group-writable is allowed:
/// a umask of 002 with per-user groups is common). On non-Unix platforms
/// (per-user `%LOCALAPPDATA%`) this is always true.
#[cfg(unix)]
#[allow(unsafe_code)] // one libc::geteuid() call, no invariants
pub(crate) fn dir_is_private(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    std::fs::metadata(dir).is_ok_and(|m| m.is_dir() && m.uid() == euid && m.mode() & 0o002 == 0)
}

/// See the Unix variant.
#[cfg(not(unix))]
pub(crate) fn dir_is_private(_dir: &Path) -> bool {
    true
}

/// Write via a sibling temp file and rename, so a concurrent reader never
/// sees a half-written library.
fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes).with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed to move {} into place", path.display()))
}

/// A file name segment from an untrusted version string.
fn file_safe(v: &str) -> String {
    let s: String =
        v.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')).collect();
    if s.is_empty() || s.starts_with('.') { "unknown".into() } else { s }
}

// ─────────────────────────────────────────────────────────────────────────────
// Fetching
// ─────────────────────────────────────────────────────────────────────────────

/// Where catalog releases come from (HTTP in production, a mock in tests).
pub(crate) trait CatalogSource {
    /// Download `asset` of release `tag`; `Ok(None)` when it does not exist.
    fn fetch_asset(&self, tag: &str, asset: &str) -> anyhow::Result<Option<Vec<u8>>>;
    /// List releases (newest first, as GitHub returns them).
    fn list_releases(&self) -> anyhow::Result<Vec<ReleaseInfo>>;
}

/// The catalog over HTTPS.
pub(crate) struct HttpCatalog {
    download_base: String,
    releases_api: String,
    client: reqwest::blocking::Client,
}

impl HttpCatalog {
    /// The public catalog, unless [`CATALOG_URL_ENV`] / [`CATALOG_API_ENV`]
    /// point elsewhere (a mirror).
    pub(crate) fn from_env() -> anyhow::Result<Self> {
        let env = |key: &str, default: &str| {
            std::env::var(key)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| default.into())
        };
        Self::new(
            &env(CATALOG_URL_ENV, DEFAULT_DOWNLOAD_BASE),
            &env(CATALOG_API_ENV, DEFAULT_RELEASES_API),
        )
    }

    /// A client for the given endpoints.
    pub(crate) fn new(download_base: &str, releases_api: &str) -> anyhow::Result<Self> {
        // The downloaded library is loaded as native code, and the manifest
        // that vouches for it travels in the same tarball — so the transport
        // is the trust boundary. Plain HTTP is accepted only for a loopback
        // mirror (local testing); everything else, redirects included, must
        // be HTTPS.
        let all_https = catalog_url_is_https(download_base, CATALOG_URL_ENV)?
            & catalog_url_is_https(releases_api, CATALOG_API_ENV)?;
        // reqwest is built with no crypto provider and no roots of its own
        // (#241); hand it the process provider + bundled webpki roots.
        ephpm_server::tls::install_default_crypto_provider();
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls =
            rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
        let client = reqwest::blocking::Client::builder()
            .use_preconfigured_tls(tls)
            .user_agent(concat!("ephpm/", env!("EPHPM_VERSION"), " (dev --xdebug)"))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(300))
            .https_only(all_https)
            .build()
            .context("failed to build the extension-catalog HTTP client")?;
        Ok(Self {
            download_base: download_base.trim_end_matches('/').to_string(),
            releases_api: releases_api.to_string(),
            client,
        })
    }
}

/// `Ok(true)` for an `https://` catalog endpoint, `Ok(false)` for `http://`
/// to a loopback host (a local mirror), an error for anything else.
pub(crate) fn catalog_url_is_https(url: &str, env: &str) -> anyhow::Result<bool> {
    let parsed = reqwest::Url::parse(url)
        .with_context(|| format!("extension catalog URL {url:?} ({env}) is not a valid URL"))?;
    match parsed.scheme() {
        "https" => Ok(true),
        "http" => {
            let host =
                parsed.host_str().unwrap_or_default().trim_start_matches('[').trim_end_matches(']');
            let loopback = host.eq_ignore_ascii_case("localhost")
                || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
            if loopback {
                Ok(false)
            } else {
                bail!(
                    "extension catalog URL {url} ({env}) uses plain http: the downloaded library \
                     runs as native code, so only https (or http to a loopback mirror) is accepted"
                )
            }
        }
        other => bail!("extension catalog URL {url} ({env}): unsupported scheme {other:?}"),
    }
}

/// Read a response body, refusing more than `max` bytes.
fn read_capped(resp: reqwest::blocking::Response, max: u64, what: &str) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    resp.take(max + 1).read_to_end(&mut buf).with_context(|| format!("reading {what}"))?;
    if buf.len() as u64 > max {
        bail!("{what} is larger than {max} bytes — refusing it");
    }
    Ok(buf)
}

/// A friendly message for a GitHub rate-limit response, or `None` when the
/// response is not one. `now` is seconds since the epoch.
pub(crate) fn rate_limit_message(
    status: u16,
    remaining: Option<&str>,
    reset: Option<&str>,
    now: u64,
) -> Option<String> {
    let limited = status == 429 || (status == 403 && remaining.map(str::trim) == Some("0"));
    if !limited {
        return None;
    }
    let wait = reset
        .and_then(|r| r.trim().parse::<u64>().ok())
        .map(|reset| reset.saturating_sub(now))
        .map_or_else(String::new, |secs| format!(" (resets in ~{} min)", secs.div_ceil(60)));
    Some(format!(
        "GitHub API rate limit reached while listing ephpm/php-sdk releases{wait}. Unauthenticated \
         clients get 60 requests per hour. Retry later, or pass --xdebug-path <xdebug.so>."
    ))
}

impl CatalogSource for HttpCatalog {
    fn fetch_asset(&self, tag: &str, asset: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let url = format!("{}/{tag}/{asset}", self.download_base);
        let resp = self.client.get(&url).send().with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            bail!("GET {url}: HTTP {status}");
        }
        read_capped(resp, MAX_TARBALL_BYTES, &url).map(Some)
    }

    fn list_releases(&self) -> anyhow::Result<Vec<ReleaseInfo>> {
        let url = format!("{}?per_page=100", self.releases_api);
        let resp = self
            .client
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .send()
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        let header =
            |name: &str| resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        if let Some(msg) = rate_limit_message(
            status.as_u16(),
            header("x-ratelimit-remaining").as_deref(),
            header("x-ratelimit-reset").as_deref(),
            now,
        ) {
            bail!(msg);
        }
        if !status.is_success() {
            bail!("GET {url}: HTTP {status}");
        }
        let body = read_capped(resp, MAX_LISTING_BYTES, &url)?;
        serde_json::from_slice(&body)
            .with_context(|| format!("parsing the release list from {url}"))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Resolution
// ─────────────────────────────────────────────────────────────────────────────

/// Where the loaded library came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Origin {
    /// `--xdebug-path`.
    LocalPath,
    /// A verified cache hit (no network).
    Cache {
        /// The release it was originally downloaded from.
        release: String,
    },
    /// Downloaded on this run.
    Catalog {
        /// The release used.
        release: String,
        /// Set when it is a same-ABI fallback for this release.
        fallback_from: Option<String>,
    },
}

/// A usable Xdebug library.
#[derive(Debug, Clone)]
pub(crate) struct Resolved {
    /// Absolute path.
    pub path: PathBuf,
    /// Xdebug version, when known before loading.
    #[allow(dead_code)]
    pub version: Option<String>,
    /// Provenance.
    pub origin: Origin,
    /// The manifest flagged it as a Zend extension.
    pub zend_extension: bool,
}

/// Verify `archive` and, if it ships Xdebug, store it in `cache`.
fn install_from(
    archive: &CatalogArchive,
    release_version: &str,
    target: &Target,
    cache: &ExtCache,
    fallback_from: Option<String>,
) -> anyhow::Result<Option<Resolved>> {
    let tag = format!("ext-{release_version}");
    validate_catalog(archive, target).with_context(|| format!("catalog release {tag}"))?;
    let Some(entry) = archive.xdebug() else {
        return Ok(None);
    };
    let bytes = &archive.files[&entry.file];
    let version = entry.known_version().map(str::to_string);
    let record = CacheRecord {
        file: format!(
            "{XDEBUG}-{}.{}",
            file_safe(version.as_deref().unwrap_or(release_version)),
            target.os.lib_suffix()
        ),
        sha256: sha256_hex(bytes),
        version: version.clone(),
        release: tag.clone(),
        php_version: archive.manifest.php_version.clone(),
        zend_extension: entry.zend_extension,
    };
    let path = cache.store(bytes, &record)?;
    Ok(Some(Resolved {
        path,
        version,
        origin: Origin::Catalog { release: tag, fallback_from },
        zend_extension: entry.zend_extension,
    }))
}

/// Find a verified Xdebug for `target`: cache, then the exact release, then
/// the newest same-ABI release of the same PHP minor. `notice` receives one
/// line per user-visible step.
///
/// # Errors
///
/// Hard refusal on any ABI/sha mismatch in a release that would otherwise be
/// used; otherwise an error listing what was tried, naming `--xdebug-path`.
pub(crate) fn resolve_from_catalog(
    source: &dyn CatalogSource,
    target: &Target,
    cache: &ExtCache,
    notice: &mut dyn FnMut(&str),
) -> anyhow::Result<Resolved> {
    if let Some((path, record)) = cache.lookup() {
        return Ok(Resolved {
            path,
            version: record.version,
            origin: Origin::Cache { release: record.release },
            zend_extension: record.zend_extension,
        });
    }

    let exact_tag = format!("ext-{}", target.php_version);
    let exact_asset = target.asset_name(&target.php_version);
    notice(&format!("downloading {exact_asset} (catalog release {exact_tag})"));
    let mut tried: Vec<String> = Vec::new();
    let mut network_error: Option<anyhow::Error> = None;
    // Whether any release had a tarball for this platform at all; when none
    // did, the catalog simply does not publish this platform yet.
    let mut platform_asset_seen = false;
    match source.fetch_asset(&exact_tag, &exact_asset) {
        Ok(Some(bytes)) => {
            platform_asset_seen = true;
            let archive =
                unpack_catalog(&bytes).with_context(|| format!("catalog release {exact_tag}"))?;
            if let Some(resolved) =
                install_from(&archive, &target.php_version, target, cache, None)?
            {
                return Ok(resolved);
            }
            notice(&format!("{exact_tag} does not include Xdebug yet"));
            tried.push(format!("{exact_tag} (no xdebug)"));
        }
        Ok(None) => {
            notice(&format!("{exact_tag} has no {exact_asset}"));
            tried.push(format!("{exact_tag} (missing)"));
        }
        Err(e) => {
            notice(&format!("download failed: {e:#}"));
            tried.push(format!("{exact_tag} (download failed)"));
            network_error = Some(e);
        }
    }

    let releases = match source.list_releases() {
        Ok(releases) => releases,
        Err(e) => {
            let cause = network_error.map_or_else(|| format!("{e:#}"), |n| format!("{n:#}; {e:#}"));
            bail!(
                "--xdebug: no usable Xdebug build ({cause}). Nothing is cached in {} yet — run \
                 once with network access, or pass --xdebug-path <xdebug.so>.",
                cache.dir().display()
            );
        }
    };

    for version in
        select_fallback_releases(&releases, target).into_iter().take(MAX_FALLBACK_ATTEMPTS)
    {
        let tag = format!("ext-{version}");
        let asset = target.asset_name(&version);
        notice(&format!("trying same-minor release {tag}"));
        let Some(bytes) = source.fetch_asset(&tag, &asset)? else {
            tried.push(format!("{tag} (missing)"));
            continue;
        };
        platform_asset_seen = true;
        let archive = unpack_catalog(&bytes).with_context(|| format!("catalog release {tag}"))?;
        if let Err(e) = check_manifest_abi(&archive.manifest, target) {
            notice(&format!("{tag} skipped: {e}"));
            tried.push(format!("{tag} (other ABI)"));
            continue;
        }
        if let Some(resolved) =
            install_from(&archive, &version, target, cache, Some(exact_tag.clone()))?
        {
            notice(&format!(
                "using {tag}: same PHP API ({}) as this binary's PHP {}, so it is ABI-compatible",
                target.api_no, target.php_version
            ));
            return Ok(resolved);
        }
        tried.push(format!("{tag} (no xdebug)"));
    }

    if !platform_asset_seen && network_error.is_none() {
        let minor = parse_minor(&target.php_version)
            .map_or_else(|| target.php_version.clone(), |(ma, mi)| format!("{ma}.{mi}"));
        bail!(
            "--xdebug: the ePHPm extension catalog does not publish {} builds for PHP {minor} \
             yet — no release has an `{}` asset (tried: {}). Pass --xdebug-path <xdebug.so> with \
             a ZTS Xdebug build for PHP {minor}.",
            target.platform_label(),
            target.asset_name("<version>"),
            if tried.is_empty() { "nothing".to_string() } else { tried.join(", ") }
        );
    }

    bail!(
        "--xdebug: no ePHPm extension catalog release ships Xdebug for PHP {} (API {}, {}) yet \
         (tried: {}). Pass --xdebug-path <xdebug.so / php_xdebug.dll> with a ZTS build for this PHP minor.",
        target.php_version,
        target.api_no,
        target.arch,
        if tried.is_empty() { "nothing".to_string() } else { tried.join(", ") }
    )
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::Write as _;

    use super::*;
    use crate::ext_ini::tests::fake_elf_so;

    fn target() -> Target {
        Target {
            php_version: "8.5.7".into(),
            api_no: 20_250_925,
            zts: true,
            arch: "x86_64".into(),
            os: CatalogOs::LinuxGnu,
        }
    }

    /// The v0.11.2 macOS binary: arm64, and a branch-tip `PHP_VERSION`.
    fn macos_target() -> Target {
        Target {
            php_version: "8.5.8-dev".into(),
            arch: "aarch64".into(),
            os: CatalogOs::Macos,
            ..target()
        }
    }

    fn windows_target() -> Target {
        Target { os: CatalogOs::Windows, ..target() }
    }

    fn xdebug_so() -> Vec<u8> {
        fake_elf_so(&["get_module", "zend_extension_entry", "extension_version_info"], &[])
    }

    /// One file for [`tarball`]: (name, bytes, manifest flags).
    struct Entry {
        name: &'static str,
        bytes: Vec<u8>,
        zend: Option<bool>,
        version: &'static str,
    }

    fn entry(name: &'static str, bytes: Vec<u8>) -> Entry {
        Entry { name, bytes, zend: None, version: "unknown" }
    }

    fn manifest_json(php_version: &str, api: &str, arch: &str, entries: &[Entry]) -> String {
        let exts: Vec<String> = entries
            .iter()
            .map(|e| {
                let zend = e.zend.map_or_else(String::new, |z| format!(", \"zend_extension\": {z}"));
                format!(
                    "{{ \"name\": \"{}\", \"file\": \"{}.so\", \"version\": \"{}\", \"php_api_no\": \
                     \"{api}\", \"zts\": 1, \"sha256\": \"{}\"{zend} }}",
                    e.name,
                    e.name,
                    e.version,
                    sha256_hex(&e.bytes)
                )
            })
            .collect();
        format!(
            "{{ \"php_version\": \"{php_version}\", \"php_api_no\": \"{api}\", \"zts\": 1, \
             \"libc\": \"gnu\", \"arch\": \"{arch}\", \"extensions\": [ {} ] }}",
            exts.join(",\n")
        )
    }

    /// A catalog `.tar.gz` laid out like the real one (`./manifest.json`,
    /// `./<name>.so`).
    fn tarball_with_manifest(manifest: &str, entries: &[Entry]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(gz);
        let mut add = |name: String, bytes: &[u8]| {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, name, bytes).unwrap();
        };
        add("./manifest.json".into(), manifest.as_bytes());
        for e in entries {
            add(format!("./{}.so", e.name), &e.bytes);
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    fn tarball(php_version: &str, api: &str, entries: &[Entry]) -> Vec<u8> {
        tarball_with_manifest(&manifest_json(php_version, api, "x86_64", entries), entries)
    }

    fn release(tag: &str, arch_assets: &[&str]) -> ReleaseInfo {
        let version = tag.trim_start_matches("ext-");
        ReleaseInfo {
            tag: tag.into(),
            draft: false,
            prerelease: false,
            assets: arch_assets
                .iter()
                .map(|a| format!("ephpm-ext-{version}-linux-{a}-gnu.tar.gz"))
                .collect(),
        }
    }

    /// In-memory catalog. `offline` makes every call fail like a dead network.
    #[derive(Default)]
    struct MockSource {
        assets: HashMap<String, Vec<u8>>,
        releases: Vec<ReleaseInfo>,
        offline: bool,
        fetched: RefCell<Vec<String>>,
    }

    impl MockSource {
        fn with(mut self, version: &str, tarball: Vec<u8>) -> Self {
            self.assets.insert(format!("ext-{version}"), tarball);
            self.releases.push(release(&format!("ext-{version}"), &["x86_64"]));
            self
        }
    }

    impl CatalogSource for MockSource {
        fn fetch_asset(&self, tag: &str, asset: &str) -> anyhow::Result<Option<Vec<u8>>> {
            if self.offline {
                bail!("network unreachable");
            }
            self.fetched.borrow_mut().push(tag.to_string());
            assert!(
                asset.ends_with("-linux-x86_64-gnu.tar.gz")
                    || asset.ends_with("-macos-aarch64.tar.gz")
                    || asset.ends_with("-windows-x86_64.tar.gz"),
                "{asset}"
            );
            Ok(self.assets.get(tag).cloned())
        }

        fn list_releases(&self) -> anyhow::Result<Vec<ReleaseInfo>> {
            if self.offline {
                bail!("network unreachable");
            }
            Ok(self.releases.clone())
        }
    }

    fn resolve(source: &MockSource, cache: &ExtCache) -> (anyhow::Result<Resolved>, Vec<String>) {
        let mut notes = Vec::new();
        let r = resolve_from_catalog(source, &target(), cache, &mut |l| notes.push(l.to_string()));
        (r, notes)
    }

    // ── manifest parsing ────────────────────────────────────────────────

    #[test]
    fn parses_the_published_manifest_shape() {
        // Verbatim shape of ext-8.5.7's manifest (php-sdk extensions.yml).
        let json = r#"{
          "php_version": "8.5.11", "php_api_no": "20250925", "zts": 1, "libc": "gnu",
          "arch": "x86_64",
          "extensions": [
            { "name": "redis", "file": "redis.so", "version": "6.x",
              "php_api_no": "20250925", "zts": 1, "sha256": "ab" }
            ,
            { "name": "xdebug", "file": "xdebug.so", "version": "3.5.3",
              "php_api_no": 20250925, "zts": true, "sha256": "cd", "zend_extension": true }
          ]
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.php_api_no, "20250925");
        assert!(m.zts);
        assert_eq!(m.libc.as_deref(), Some("gnu"));
        // Missing zend_extension → false; number api_no accepted.
        assert!(!m.extensions[0].zend_extension);
        assert!(m.extensions[1].zend_extension);
        assert_eq!(m.extensions[1].php_api_no, "20250925");
        assert_eq!(m.extensions[1].known_version(), Some("3.5.3"));
        let unknown = ManifestExt { version: Some("unknown".into()), ..m.extensions[0].clone() };
        assert_eq!(unknown.known_version(), None);
    }

    // ── archive validation ─────────────────────────────────────────────

    #[test]
    fn valid_catalog_unpacks_and_verifies() {
        let tgz = tarball(
            "8.5.7",
            "20250925",
            &[entry("redis", b"redis".to_vec()), entry("xdebug", xdebug_so())],
        );
        let archive = unpack_catalog(&tgz).unwrap();
        assert_eq!(archive.files.len(), 2, "manifest is taken out of the file map");
        validate_catalog(&archive, &target()).unwrap();
        assert_eq!(archive.xdebug().unwrap().file, "xdebug.so");
    }

    #[test]
    fn sha_mismatch_is_refused() {
        let entries = [entry("xdebug", xdebug_so())];
        let manifest = manifest_json("8.5.7", "20250925", "x86_64", &entries);
        // Same manifest, tampered payload.
        let tampered = [entry("xdebug", b"evil".to_vec())];
        let archive = unpack_catalog(&tarball_with_manifest(&manifest, &tampered)).unwrap();
        let err = validate_catalog(&archive, &target()).unwrap_err();
        assert!(
            matches!(err, CatalogError::ShaMismatch { ref file, .. } if file == "xdebug.so"),
            "{err}"
        );
        assert!(err.to_string().contains("refusing"));
    }

    #[test]
    fn abi_mismatches_are_refused() {
        let entries = [entry("xdebug", xdebug_so())];
        let api = unpack_catalog(&tarball("8.4.23", "20240924", &entries)).unwrap();
        assert!(matches!(
            validate_catalog(&api, &target()).unwrap_err(),
            CatalogError::ApiMismatch { expected: 20_250_925, .. }
        ));

        let arch = unpack_catalog(&tarball_with_manifest(
            &manifest_json("8.5.7", "20250925", "aarch64", &entries),
            &entries,
        ))
        .unwrap();
        assert!(matches!(
            validate_catalog(&arch, &target()).unwrap_err(),
            CatalogError::ArchMismatch { .. }
        ));

        let nts = manifest_json("8.5.7", "20250925", "x86_64", &entries).replacen(
            "\"zts\": 1",
            "\"zts\": 0",
            1,
        );
        let nts = unpack_catalog(&tarball_with_manifest(&nts, &entries)).unwrap();
        assert!(matches!(
            validate_catalog(&nts, &target()).unwrap_err(),
            CatalogError::ZtsMismatch { .. }
        ));

        let musl =
            manifest_json("8.5.7", "20250925", "x86_64", &entries).replace("\"gnu\"", "\"musl\"");
        let musl = unpack_catalog(&tarball_with_manifest(&musl, &entries)).unwrap();
        assert!(matches!(
            validate_catalog(&musl, &target()).unwrap_err(),
            CatalogError::LibcMismatch { .. }
        ));

        // A per-entry API that disagrees with the binary.
        let entry_api = manifest_json("8.5.7", "20250925", "x86_64", &entries).replace(
            "\"php_api_no\": \"20250925\", \"zts\": 1, \"sha256\"",
            "\"php_api_no\": \"1\", \"zts\": 1, \"sha256\"",
        );
        let entry_api = unpack_catalog(&tarball_with_manifest(&entry_api, &entries)).unwrap();
        assert!(matches!(
            validate_catalog(&entry_api, &target()).unwrap_err(),
            CatalogError::EntryAbiMismatch { .. }
        ));
    }

    /// A Windows catalog tarball laid out like php-sdk `windows-shared.yml`
    /// publishes it: `php_xdebug.dll` + `manifest.json` (PowerShell's
    /// `ConvertTo-Json` spacing, `libc` = `msvc`, extra `os`/`crt`/`source`
    /// keys) + `LICENSE-xdebug.txt`, which is not in the manifest.
    fn windows_tarball(php_version: &str, libc: Option<&str>, dll: &[u8]) -> Vec<u8> {
        let libc = libc.map_or_else(String::new, |l| format!("\"libc\":  \"{l}\",\n"));
        let manifest = format!(
            "{{\n    \"php_version\":  \"{php_version}\",\n    \"php_api_no\":  \"20250925\",\n    \
             \"zts\":  1,\n    {libc}    \"os\":  \"windows\",\n    \"arch\":  \"x86_64\",\n    \
             \"crt\":  \"vs17\",\n    \"extensions\":  [\n        {{\n            \"name\":  \
             \"xdebug\",\n            \"file\":  \"php_xdebug.dll\",\n            \"version\":  \
             \"3.5.3\",\n            \"php_api_no\":  \"20250925\",\n            \"zts\":  1,\n            \
             \"zend_extension\":  true,\n            \"sha256\":  \"{}\",\n            \"source\":  \
             \"https://xdebug.org/files/php_xdebug-3.5.3-8.5-ts-vs17-x86_64.dll\"\n        }}\n    \
             ]\n}}",
            sha256_hex(dll)
        );
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(gz);
        for (name, bytes) in [
            ("./manifest.json", manifest.as_bytes()),
            ("./php_xdebug.dll", dll),
            ("./LICENSE-xdebug.txt", b"The Xdebug License".as_slice()),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, name, bytes).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn windows_catalog_names_and_libc() {
        let t = windows_target();
        assert_eq!(t.asset_name("8.5.7"), "ephpm-ext-8.5.7-windows-x86_64.tar.gz");
        assert_eq!(target().asset_name("8.5.7"), "ephpm-ext-8.5.7-linux-x86_64-gnu.tar.gz");

        let dll = b"MZ fake xdebug".to_vec();
        let good = unpack_catalog(&windows_tarball("8.5.7", Some("msvc"), &dll)).unwrap();
        validate_catalog(&good, &t).unwrap();
        assert_eq!(good.xdebug().unwrap().file, "php_xdebug.dll");
        assert!(good.xdebug().unwrap().zend_extension);

        // A Linux catalog can never satisfy a Windows binary, nor vice versa.
        let gnu = unpack_catalog(&windows_tarball("8.5.7", Some("gnu"), &dll)).unwrap();
        assert!(matches!(
            validate_catalog(&gnu, &t).unwrap_err(),
            CatalogError::LibcMismatch { expected: "msvc", .. }
        ));
        assert!(matches!(
            validate_catalog(&good, &target()).unwrap_err(),
            CatalogError::OsMismatch { expected: "linux", .. }
        ));
        // A Windows manifest must say which C runtime it is built for.
        let unset = unpack_catalog(&windows_tarball("8.5.7", None, &dll)).unwrap();
        assert!(matches!(
            validate_catalog(&unset, &t).unwrap_err(),
            CatalogError::LibcMismatch { .. }
        ));
    }

    #[test]
    fn windows_catalog_resolves_to_a_cached_dll() {
        let root = tempfile::tempdir().unwrap();
        let t = windows_target();
        let cache = ExtCache::new(root.path(), &t);
        let dll = b"MZ fake xdebug".to_vec();
        let source =
            MockSource::default().with("8.5.7", windows_tarball("8.5.7", Some("msvc"), &dll));
        let resolved = resolve_from_catalog(&source, &t, &cache, &mut |_| {}).unwrap();
        assert_eq!(resolved.path, cache.dir().join("xdebug-3.5.3.dll"));
        assert_eq!(std::fs::read(&resolved.path).unwrap(), dll);
        assert!(resolved.zend_extension);
    }

    #[test]
    fn missing_file_and_garbage_are_refused() {
        let entries = [entry("xdebug", xdebug_so())];
        let manifest = manifest_json("8.5.7", "20250925", "x86_64", &entries);
        let archive = unpack_catalog(&tarball_with_manifest(&manifest, &[])).unwrap();
        assert!(matches!(
            validate_catalog(&archive, &target()).unwrap_err(),
            CatalogError::MissingFile { .. }
        ));
        assert!(matches!(
            unpack_catalog(b"not a tarball").unwrap_err(),
            CatalogError::Malformed(_)
        ));
        assert!(matches!(
            unpack_catalog(&tarball_with_manifest("{}", &[])).unwrap_err(),
            CatalogError::Malformed(_)
        ));
    }

    // ── macOS catalog ──────────────────────────────────────────────────

    /// A macOS catalog tarball laid out as php-sdk `build-extensions-macos`
    /// writes it (php-sdk#74): `"os": "macos"`, no `libc`, the shell's
    /// one-key-per-line formatting with a bare `,` between entries.
    fn macos_tarball(php_version: &str, os: &str, libc: Option<&str>, so: &[u8]) -> Vec<u8> {
        let libc = libc.map_or_else(String::new, |l| format!("  \"libc\": \"{l}\",\n"));
        let manifest = format!(
            "{{\n  \"php_version\": \"{php_version}\",\n  \"php_api_no\": \"20250925\",\n  \
             \"zts\": 1,\n  \"os\": \"{os}\",\n{libc}  \"arch\": \"aarch64\",\n  \"extensions\": \
             [\n    {{\n      \"name\": \"apcu\",\n      \"file\": \"apcu.so\",\n      \
             \"version\": \"unknown\",\n      \"php_api_no\": \"20250925\",\n      \"zts\": 1,\n      \
             \"zend_extension\": false,\n      \"sha256\": \"{}\"\n    }}\n    ,\n    {{\n      \
             \"name\": \"xdebug\",\n      \"file\": \"xdebug.so\",\n      \"version\": \"3.5.3\",\n      \
             \"php_api_no\": \"20250925\",\n      \"zts\": 1,\n      \"zend_extension\": true,\n      \
             \"sha256\": \"{}\"\n    }}\n  ]\n}}\n",
            sha256_hex(b"apcu"),
            sha256_hex(so)
        );
        tarball_with_manifest(
            &manifest,
            &[entry("apcu", b"apcu".to_vec()), entry("xdebug", so.to_vec())],
        )
    }

    #[test]
    fn macos_catalog_names_os_and_libc() {
        let mac = macos_target();
        assert_eq!(mac.asset_name("8.5.11"), "ephpm-ext-8.5.11-macos-aarch64.tar.gz");
        assert_eq!(target().asset_name("8.5.11"), "ephpm-ext-8.5.11-linux-x86_64-gnu.tar.gz");

        let so = xdebug_so();
        let good = unpack_catalog(&macos_tarball("8.5.11", "macos", None, &so)).unwrap();
        validate_catalog(&good, &mac).unwrap();
        assert_eq!(good.manifest.os.as_deref(), Some("macos"));
        assert!(good.xdebug().unwrap().zend_extension);
        assert!(!good.manifest.extensions[0].zend_extension);

        // Same arch, so only the os gate can tell a macOS catalog from Linux.
        let linux_arm = Target { arch: "aarch64".into(), ..target() };
        assert!(matches!(
            validate_catalog(&good, &linux_arm).unwrap_err(),
            CatalogError::OsMismatch { expected: "linux", .. }
        ));
        // A Linux manifest (libc gnu, no os key) never satisfies macOS.
        let entries = [entry("xdebug", so.clone())];
        let linux = unpack_catalog(&tarball_with_manifest(
            &manifest_json("8.5.11", "20250925", "aarch64", &entries),
            &entries,
        ))
        .unwrap();
        assert!(matches!(
            validate_catalog(&linux, &mac).unwrap_err(),
            CatalogError::OsMismatch { expected: "macos", .. }
        ));
        validate_catalog(&linux, &linux_arm).unwrap();
        // A macOS manifest naming a libc is not one this binary understands.
        let odd = unpack_catalog(&macos_tarball("8.5.11", "macos", Some("gnu"), &so)).unwrap();
        assert!(matches!(
            validate_catalog(&odd, &mac).unwrap_err(),
            CatalogError::LibcMismatch { .. }
        ));
        let wrong_os = unpack_catalog(&macos_tarball("8.5.11", "windows", None, &so)).unwrap();
        assert!(matches!(
            validate_catalog(&wrong_os, &mac).unwrap_err(),
            CatalogError::OsMismatch { .. }
        ));
    }

    #[test]
    fn macos_dev_php_version_falls_back_to_the_same_minor() {
        let root = tempfile::tempdir().unwrap();
        let mac = macos_target();
        let cache = ExtCache::new(root.path(), &mac);
        let so = xdebug_so();
        let mut source = MockSource::default();
        source.assets.insert("ext-8.5.11".into(), macos_tarball("8.5.11", "macos", None, &so));
        source.releases = vec![
            ReleaseInfo {
                tag: "ext-8.5.11".into(),
                draft: false,
                prerelease: false,
                assets: vec![
                    mac.asset_name("8.5.11"),
                    "ephpm-ext-8.5.11-linux-aarch64-gnu.tar.gz".into(),
                ],
            },
            // Linux-only release: no macOS asset, never fetched.
            release("ext-8.5.10", &["aarch64"]),
        ];
        let mut notes = Vec::new();
        let resolved =
            resolve_from_catalog(&source, &mac, &cache, &mut |l| notes.push(l.to_string()))
                .unwrap();
        assert_eq!(
            resolved.origin,
            Origin::Catalog {
                release: "ext-8.5.11".into(),
                fallback_from: Some("ext-8.5.8-dev".into())
            }
        );
        assert!(resolved.zend_extension);
        assert_eq!(resolved.path, cache.dir().join("xdebug-3.5.3.so"));
        assert_eq!(std::fs::read(&resolved.path).unwrap(), so);
        assert_eq!(*source.fetched.borrow(), vec!["ext-8.5.8-dev", "ext-8.5.11"]);
    }

    #[test]
    fn minor_parsing_tolerates_suffixed_patches() {
        assert_eq!(parse_minor("8.5.7"), Some((8, 5)));
        assert_eq!(parse_minor("8.5.8-dev"), Some((8, 5)));
        assert_eq!(parse_minor("8.6.0beta1"), Some((8, 6)));
        assert_eq!(parse_minor("8.5"), None);
        assert_eq!(parse_minor("8.5.dev"), None);
        assert_eq!(parse_minor("8.x.1"), None);
        // Catalog tags stay strict.
        assert_eq!(parse_version("8.5.8-dev"), None);
    }

    // ── release selection ──────────────────────────────────────────────

    #[test]
    fn fallback_picks_same_minor_newest_first() {
        let mut draft = release("ext-8.5.12", &["x86_64"]);
        draft.draft = true;
        let mut pre = release("ext-8.5.13", &["x86_64"]);
        pre.prerelease = true;
        let releases = vec![
            release("ext-8.4.26", &["x86_64"]),
            release("ext-8.5.7", &["x86_64"]),
            release("ext-8.5.9", &["x86_64"]),
            release("ext-8.5.11", &["x86_64", "aarch64"]),
            release("ext-8.5.10", &["aarch64"]), // no x86_64 asset
            release("ext-8.5.10-ghreltest", &["x86_64"]),
            release("v8.5.11", &["x86_64"]),
            release("ext-8.6.0beta1", &["x86_64"]),
            release("ext-8.5.8", &["x86_64"]),
            draft,
            pre,
        ];
        assert_eq!(
            select_fallback_releases(&releases, &target()),
            vec!["8.5.11", "8.5.9", "8.5.8"]
        );
        assert_eq!(parse_version("8.5.11"), Some((8, 5, 11)));
        assert_eq!(parse_version("8.5"), None);
        assert_eq!(parse_version("8.5.1.2"), None);
        assert_eq!(parse_version("8.5.x"), None);
    }

    // ── end-to-end resolution (mock source) ─────────────────────────────

    #[test]
    fn exact_release_is_downloaded_then_served_from_cache_offline() {
        let root = tempfile::tempdir().unwrap();
        let cache = ExtCache::new(root.path(), &target());
        let mut x = entry("xdebug", xdebug_so());
        x.version = "3.5.3";
        x.zend = Some(true);
        let source = MockSource::default()
            .with("8.5.7", tarball("8.5.7", "20250925", &[entry("redis", b"r".to_vec()), x]));

        let (resolved, _) = resolve(&source, &cache);
        let resolved = resolved.unwrap();
        assert_eq!(
            resolved.origin,
            Origin::Catalog { release: "ext-8.5.7".into(), fallback_from: None }
        );
        assert!(resolved.zend_extension);
        assert_eq!(resolved.version.as_deref(), Some("3.5.3"));
        assert_eq!(resolved.path, cache.dir().join("xdebug-3.5.3.so"));
        assert!(cache.dir().ends_with("20250925-zts-x86_64"));
        assert_eq!(std::fs::read(&resolved.path).unwrap(), xdebug_so());

        // Second run: no network at all.
        let offline = MockSource { offline: true, ..MockSource::default() };
        let (again, notes) = resolve(&offline, &cache);
        let again = again.unwrap();
        assert_eq!(again.origin, Origin::Cache { release: "ext-8.5.7".into() });
        assert_eq!(again.path, resolved.path);
        assert!(notes.is_empty(), "a cache hit prints nothing: {notes:?}");
    }

    #[test]
    fn corrupted_cache_is_refetched() {
        let root = tempfile::tempdir().unwrap();
        let cache = ExtCache::new(root.path(), &target());
        let source = MockSource::default()
            .with("8.5.7", tarball("8.5.7", "20250925", &[entry("xdebug", xdebug_so())]));
        let first = resolve(&source, &cache).0.unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&first.path)
            .unwrap()
            .write_all(b"x")
            .unwrap();
        assert!(cache.lookup().is_none(), "sha mismatch must not be a hit");
        let second = resolve(&source, &cache).0.unwrap();
        assert!(matches!(second.origin, Origin::Catalog { .. }));
        assert_eq!(std::fs::read(&second.path).unwrap(), xdebug_so());
    }

    #[test]
    fn missing_exact_release_falls_back_to_newest_same_abi() {
        let root = tempfile::tempdir().unwrap();
        let cache = ExtCache::new(root.path(), &target());
        let source = MockSource::default()
            .with("8.5.9", tarball("8.5.9", "20250925", &[entry("xdebug", xdebug_so())]))
            .with("8.5.11", tarball("8.5.11", "20250925", &[entry("xdebug", xdebug_so())]));
        let (resolved, notes) = resolve(&source, &cache);
        let resolved = resolved.unwrap();
        assert_eq!(
            resolved.origin,
            Origin::Catalog {
                release: "ext-8.5.11".into(),
                fallback_from: Some("ext-8.5.7".into())
            }
        );
        // Unknown xdebug version → named after the release.
        assert_eq!(resolved.path, cache.dir().join("xdebug-8.5.11.so"));
        assert!(notes.iter().any(|n| n.contains("ext-8.5.7 has no")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("ABI-compatible")), "{notes:?}");
        assert_eq!(*source.fetched.borrow(), vec!["ext-8.5.7", "ext-8.5.11"]);
    }

    #[test]
    fn exact_release_without_xdebug_falls_back_skipping_other_abis() {
        let root = tempfile::tempdir().unwrap();
        let cache = ExtCache::new(root.path(), &target());
        let source = MockSource::default()
            // The binary's own release exists but predates xdebug in the catalog.
            .with("8.5.7", tarball("8.5.7", "20250925", &[entry("redis", b"r".to_vec())]))
            // Newest same-minor release claims another API: skipped, not used.
            .with("8.5.12", tarball("8.5.12", "20991231", &[entry("xdebug", xdebug_so())]))
            .with("8.5.11", tarball("8.5.11", "20250925", &[entry("xdebug", xdebug_so())]));
        let (resolved, notes) = resolve(&source, &cache);
        let resolved = resolved.unwrap();
        assert_eq!(
            resolved.origin,
            Origin::Catalog {
                release: "ext-8.5.11".into(),
                fallback_from: Some("ext-8.5.7".into())
            }
        );
        assert!(notes.iter().any(|n| n.contains("does not include Xdebug")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("ext-8.5.12 skipped")), "{notes:?}");
    }

    #[test]
    fn tampered_release_is_a_hard_error_not_a_skip() {
        let root = tempfile::tempdir().unwrap();
        let cache = ExtCache::new(root.path(), &target());
        let entries = [entry("xdebug", xdebug_so())];
        let manifest = manifest_json("8.5.7", "20250925", "x86_64", &entries);
        let tampered = tarball_with_manifest(&manifest, &[entry("xdebug", b"evil".to_vec())]);
        let source = MockSource::default()
            .with("8.5.7", tampered)
            .with("8.5.11", tarball("8.5.11", "20250925", &entries));
        let err = resolve(&source, &cache).0.unwrap_err();
        assert!(format!("{err:#}").contains("sha256 mismatch"), "{err:#}");
        assert!(cache.lookup().is_none(), "nothing may be cached from a tampered archive");
    }

    #[test]
    fn exact_release_with_wrong_api_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let cache = ExtCache::new(root.path(), &target());
        let source = MockSource::default()
            .with("8.5.7", tarball("8.5.7", "20240924", &[entry("xdebug", xdebug_so())]));
        let err = resolve(&source, &cache).0.unwrap_err();
        assert!(format!("{err:#}").contains("API 20240924"), "{err:#}");
    }

    #[test]
    fn offline_without_cache_names_the_escape_hatch() {
        let root = tempfile::tempdir().unwrap();
        let cache = ExtCache::new(root.path(), &target());
        let source = MockSource { offline: true, ..MockSource::default() };
        let err = format!("{:#}", resolve(&source, &cache).0.unwrap_err());
        assert!(err.contains("network unreachable"), "{err}");
        assert!(err.contains("--xdebug-path"), "{err}");
        assert!(err.contains("20250925-zts-x86_64"), "{err}");
    }

    #[test]
    fn unpublished_platform_says_so_plainly() {
        // Today's macOS situation: every catalog release carries Linux
        // tarballs only, so a macOS binary finds nothing for its platform.
        let root = tempfile::tempdir().unwrap();
        let mac = macos_target();
        let cache = ExtCache::new(root.path(), &mac);
        let entries = [entry("xdebug", xdebug_so())];
        let source = MockSource::default()
            .with("8.5.7", tarball("8.5.7", "20250925", &entries))
            .with("8.5.11", tarball("8.5.11", "20250925", &entries));
        let err = resolve_from_catalog(&source, &mac, &cache, &mut |_| {}).unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("does not publish macOS aarch64 builds for PHP 8.5"), "{err}");
        assert!(err.contains("-macos-aarch64.tar.gz"), "{err}");
        assert!(err.contains("--xdebug-path"), "{err}");
    }

    #[test]
    fn nothing_ships_xdebug_lists_what_was_tried() {
        let root = tempfile::tempdir().unwrap();
        let cache = ExtCache::new(root.path(), &target());
        let source = MockSource::default()
            .with("8.5.7", tarball("8.5.7", "20250925", &[entry("redis", b"r".to_vec())]))
            .with("8.5.11", tarball("8.5.11", "20250925", &[entry("redis", b"r".to_vec())]));
        let err = format!("{:#}", resolve(&source, &cache).0.unwrap_err());
        assert!(
            err.contains("ext-8.5.7 (no xdebug)") && err.contains("ext-8.5.11 (no xdebug)"),
            "{err}"
        );
        assert!(err.contains("--xdebug-path"), "{err}");
    }

    #[test]
    fn rate_limit_responses_get_a_clear_message() {
        let msg = rate_limit_message(403, Some("0"), Some("1600"), 1000).unwrap();
        assert!(
            msg.contains("rate limit") && msg.contains("~10 min") && msg.contains("--xdebug-path"),
            "{msg}"
        );
        assert!(rate_limit_message(429, None, None, 0).is_some());
        // A 403 that is not a rate limit (e.g. blocked) is a normal error.
        assert!(rate_limit_message(403, Some("42"), None, 0).is_none());
        assert!(rate_limit_message(200, Some("0"), None, 0).is_none());
    }

    // ── config wiring ──────────────────────────────────────────────────

    #[test]
    fn apply_emits_absolute_zend_extension_and_lifts_timeouts() {
        let mut config = ephpm_config::Config::default_config().unwrap();
        let so = std::env::temp_dir().join("xdebug.so");
        apply_to_config(&mut config, &so, "172.20.0.1", 9100).unwrap();
        let o = &config.php.ini_overrides;
        let mut expected = so.to_string_lossy().into_owned();
        if cfg!(windows) {
            expected = expected.replace('\\', "/");
        }
        let quoted = crate::ext_ini::ini_quote(&expected).unwrap();
        assert_eq!(o[0], ["zend_extension".to_string(), quoted]);
        assert!(o.contains(&["xdebug.mode".into(), "debug,develop".into()]));
        assert!(o.contains(&["xdebug.start_with_request".into(), "trigger".into()]));
        assert!(o.contains(&["xdebug.client_host".into(), "172.20.0.1".into()]));
        assert!(o.contains(&["xdebug.client_port".into(), "9100".into()]));
        assert_eq!(config.server.timeouts.request, 0);
        // The xdebug.* settings must follow the extension that declares them.
        let pos = |k: &str| o.iter().position(|[key, _]| key == k).unwrap();
        assert!(pos("zend_extension") < pos("xdebug.mode"));
    }

    #[test]
    fn apply_refuses_unsafe_paths() {
        let mut config = ephpm_config::Config::default_config().unwrap();
        assert!(apply_to_config(&mut config, Path::new("relative/xdebug.so"), "h", 1).is_err());
        let quoted = std::env::temp_dir().join("x\"y.so");
        assert!(apply_to_config(&mut config, &quoted, "h", 1).is_err());
        let interp = std::env::temp_dir().join("${HOME}.so");
        assert!(apply_to_config(&mut config, &interp, "h", 1).is_err());
        assert_eq!(config.php.ini_overrides, Vec::<[String; 2]>::new());
    }

    #[test]
    fn client_host_is_validated() {
        assert_eq!(validate_client_host(" 127.0.0.1 ").unwrap(), "127.0.0.1");
        assert!(validate_client_host("host.docker.internal").is_ok());
        assert!(validate_client_host("::1").is_ok());
        assert!(validate_client_host("").is_err());
        assert!(validate_client_host("evil\nmemory_limit=-1").is_err());
        assert!(validate_client_host("a b").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn world_writable_cache_dir_is_never_trusted() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        assert!(dir_is_private(dir.path()));
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(!dir_is_private(dir.path()));
        assert!(!dir_is_private(&dir.path().join("missing")));
    }

    #[test]
    fn catalog_urls_must_be_https_or_loopback() {
        let ok = |u: &str| catalog_url_is_https(u, "TEST").unwrap();
        assert!(ok(DEFAULT_DOWNLOAD_BASE));
        assert!(ok(DEFAULT_RELEASES_API));
        // Loopback mirrors (the fixture E2E) may use plain http.
        assert!(!ok("http://127.0.0.1:8000/dl"));
        assert!(!ok("http://localhost:8000/dl"));
        assert!(!ok("http://[::1]:8000/dl"));
        // Anything else over http would let the network supply native code.
        for bad in ["http://mirror.example.com/dl", "http://10.0.0.5/dl", "ftp://x/y", "nope"] {
            assert!(catalog_url_is_https(bad, "TEST").is_err(), "{bad}");
        }
        assert!(HttpCatalog::new("http://mirror.example.com/dl", DEFAULT_RELEASES_API).is_err());
    }

    #[test]
    fn host_flags_without_xdebug_are_refused() {
        let mut config = ephpm_config::Config::default_config().unwrap();
        prepare(&XdebugArgs::default(), &mut config).unwrap();
        let args = XdebugArgs { xdebug_client_port: Some(9100), ..XdebugArgs::default() };
        assert!(prepare(&args, &mut config).is_err());
        assert_eq!(config.php.ini_overrides, Vec::<[String; 2]>::new());
    }

    #[test]
    fn dev_cli_shape() {
        use clap::Parser;

        let parse = |args: &[&str]| match crate::Cli::try_parse_from(args).unwrap().command {
            Some(crate::Commands::Dev { xdebug, .. }) => xdebug,
            other => panic!("not dev: {other:?}"),
        };
        let x = parse(&["ephpm", "dev"]);
        assert!(!x.enabled());
        let x = parse(&["ephpm", "dev", "--xdebug", "--xdebug-client-host", "172.20.0.1"]);
        assert!(x.enabled());
        assert_eq!(x.xdebug_client_host.as_deref(), Some("172.20.0.1"));
        let x = parse(&[
            "ephpm",
            "dev",
            "--xdebug-path",
            "/opt/xdebug.so",
            "--xdebug-client-port",
            "9100",
        ]);
        assert!(x.enabled() && !x.xdebug);
        assert_eq!(x.xdebug_path.as_deref(), Some(Path::new("/opt/xdebug.so")));
        assert_eq!(x.xdebug_client_port, Some(9100));
        assert!(crate::Cli::try_parse_from(["ephpm", "dev", "--xdebug-client-port", "x"]).is_err());
    }

    #[test]
    fn xdebug_refuses_clearly_where_it_cannot_work() {
        if ephpm_php::PhpRuntime::abi().is_some()
            && (!cfg!(windows) || ephpm_php::shared::SharedRuntime::is_shared_build())
        {
            return; // PHP-linked where it works: would really resolve; covered by the E2E.
        }
        let mut config = ephpm_config::Config::default_config().unwrap();
        let args = XdebugArgs { xdebug: true, ..XdebugArgs::default() };
        let err = prepare(&args, &mut config).unwrap_err().to_string();
        if cfg!(windows) {
            assert!(err.contains("not available on Windows"), "{err}");
        } else {
            assert!(err.contains("stub mode"), "{err}");
        }
        assert_eq!(config.php.ini_overrides, Vec::<[String; 2]>::new());
        assert_eq!(
            config.server.timeouts.request,
            ephpm_config::Config::default_config().unwrap().server.timeouts.request
        );
    }

    // ── the real HTTP client against a local fixture server ─────────────

    /// A minimal HTTP/1.1 server answering from `routes` (path → (status,
    /// body)); everything else is 404. One request per connection.
    fn fixture_server(routes: HashMap<String, (u16, Vec<u8>)>) -> String {
        use std::io::{BufRead, BufReader};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (status, body) = routes.get(&path).cloned().unwrap_or((404, b"nope".to_vec()));
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn http_catalog_against_a_fixture_server() {
        let t = target();
        let mut x = entry("xdebug", xdebug_so());
        x.version = "3.5.3";
        let good = tarball("8.5.11", "20250925", &[entry("redis", b"r".to_vec()), x]);
        let listing = serde_json::json!([
            { "tag_name": "ext-8.5.11", "draft": false, "prerelease": false,
              "assets": [ { "name": t.asset_name("8.5.11") } ] },
            { "tag_name": "ext-8.5.7", "draft": false, "prerelease": false,
              "assets": [ { "name": t.asset_name("8.5.7") } ] }
        ]);
        let mut routes = HashMap::new();
        // ext-8.5.7's asset is deliberately absent (404) → fallback.
        routes.insert(format!("/dl/ext-8.5.11/{}", t.asset_name("8.5.11")), (200, good));
        routes.insert(
            "/api/releases?per_page=100".to_string(),
            (200, listing.to_string().into_bytes()),
        );
        let base = fixture_server(routes);

        let source =
            HttpCatalog::new(&format!("{base}/dl/"), &format!("{base}/api/releases")).unwrap();
        let root = tempfile::tempdir().unwrap();
        let cache = ExtCache::new(root.path(), &t);
        let mut notes = Vec::new();
        let resolved =
            resolve_from_catalog(&source, &t, &cache, &mut |l| notes.push(l.to_string())).unwrap();
        assert_eq!(
            resolved.origin,
            Origin::Catalog {
                release: "ext-8.5.11".into(),
                fallback_from: Some("ext-8.5.7".into())
            }
        );
        assert_eq!(std::fs::read(&resolved.path).unwrap(), xdebug_so());
        assert!(crate::ext_ini::is_zend_extension(&resolved.path).unwrap());
    }

    #[test]
    fn http_catalog_reports_rate_limits() {
        // The fixture can't set headers, so a bare 429 is the rate-limit case.
        let mut routes = HashMap::new();
        routes.insert("/api?per_page=100".to_string(), (429, b"slow down".to_vec()));
        let base = fixture_server(routes);
        let source = HttpCatalog::new(&format!("{base}/dl"), &format!("{base}/api")).unwrap();
        let err = format!("{:#}", source.list_releases().unwrap_err());
        assert!(err.contains("rate limit"), "{err}");
    }
}
