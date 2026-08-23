//! `ephpm middleware` — fetch prebuilt native-middleware modules from the
//! `ephpm/middleware` GitHub releases into the loader's search path.
//!
//! # Security
//!
//! A middleware module is a shared library that ePHPm `dlopen`s and runs with
//! its own privileges — a tampered or wrong asset is arbitrary code execution
//! in the server process. This CLI therefore treats the download as untrusted:
//!
//! * Everything is fetched over **HTTPS** (GitHub is the trust root), on the
//!   same aws-lc-rs rustls provider the server uses.
//! * A downloaded module is verified against the release's **`SHA256SUMS`**
//!   before it is written anywhere. A mismatch, or a missing `SHA256SUMS`,
//!   aborts with nothing written — **fail closed**.
//! * The release's `manifest.json` records the ABI major it targets; a module
//!   built for a different ABI major than this binary is refused at download
//!   time (and would refuse to load anyway).
//!
//! The platform → asset-name mapping is derived from
//! [`ephpm_server::middleware::platform_tag`] — the exact function the loader
//! resolves against — so a fetched file lands under the name a bare
//! `library = "<name>"` mount then finds.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::MiddlewareSubcommand;

/// GitHub REST API base.
const API: &str = "https://api.github.com";
/// User-Agent (GitHub requires one).
const UA: &str = concat!("ephpm-middleware-cli/", env!("EPHPM_VERSION"));

/// A GitHub release (only the fields we use).
#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

/// One release asset. `url` is the API URL (works for public repos
/// anonymously and for private repos with a token); `Accept:
/// application/octet-stream` returns the bytes.
#[derive(Deserialize, Clone)]
struct Asset {
    name: String,
    url: String,
}

/// The release's `manifest.json` (the CLI-facing metadata contract).
#[derive(Deserialize)]
struct Manifest {
    abi_major: u32,
    #[serde(default)]
    modules: Vec<ManifestModule>,
}

/// One module entry in the manifest.
#[derive(Deserialize)]
struct ManifestModule {
    name: String,
    #[serde(default)]
    assets: Vec<ManifestAsset>,
}

/// One platform asset of a module in the manifest.
#[derive(Deserialize)]
struct ManifestAsset {
    platform: String,
    #[serde(default)]
    libc: String,
    file: String,
    #[serde(default)]
    sha256: String,
}

/// Entry point for the `middleware` subcommand.
pub async fn run(repo: &str, cmd: MiddlewareSubcommand) -> anyhow::Result<ExitCode> {
    match cmd {
        MiddlewareSubcommand::SearchPath => {
            print_search_path();
            Ok(ExitCode::SUCCESS)
        }
        MiddlewareSubcommand::List => list(repo).await,
        MiddlewareSubcommand::Get { name, dest, musl, allow_abi_mismatch } => {
            get(repo, &name, dest, musl, allow_abi_mismatch).await
        }
    }
}

/// The ABI major this binary speaks (the module's `declare!` gate compares
/// against the same value at load time).
fn host_abi_major() -> u32 {
    ephpm_middleware::abi::ABI_V1 >> 24
}

/// Build an HTTPS client on the server's crypto provider.
fn http_client() -> anyhow::Result<reqwest::Client> {
    // reqwest (built `-no-provider`) constructs its ClientConfig from the
    // process default provider, so it must be installed first — otherwise the
    // first request panics with "No provider set".
    ephpm_server::tls::install_default_crypto_provider();
    reqwest::Client::builder().user_agent(UA).build().context("failed to build HTTPS client")
}

/// `GH_TOKEN` / `GITHUB_TOKEN`, if set — used for private repos and to lift the
/// anonymous API rate limit. Not required for the public download path.
fn token() -> Option<String> {
    for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(v) = std::env::var(var)
            && !v.is_empty()
        {
            return Some(v);
        }
    }
    None
}

/// GET a GitHub API JSON resource.
async fn get_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<T> {
    let mut req = client.get(url).header("Accept", "application/vnd.github+json");
    if let Some(t) = token() {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("GET {url} returned HTTP {status}");
    }
    resp.json::<T>().await.with_context(|| format!("parsing JSON from {url}"))
}

/// Download an asset's raw bytes. reqwest follows the API → signed-URL redirect
/// and strips the `Authorization` header on the cross-host hop, so this works
/// for both public (anonymous) and private (token) repos.
async fn download_asset(client: &reqwest::Client, asset: &Asset) -> anyhow::Result<Vec<u8>> {
    let mut req = client.get(&asset.url).header("Accept", "application/octet-stream");
    if let Some(t) = token() {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.with_context(|| format!("downloading asset {}", asset.name))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("downloading asset {} returned HTTP {status}", asset.name);
    }
    let bytes = resp.bytes().await.with_context(|| format!("reading asset {}", asset.name))?;
    Ok(bytes.to_vec())
}

/// List the repo's releases, newest first.
async fn list_releases(client: &reqwest::Client, repo: &str) -> anyhow::Result<Vec<Release>> {
    let url = format!("{API}/repos/{repo}/releases?per_page=30");
    let releases: Vec<Release> = get_json(client, &url).await?;
    Ok(releases)
}

/// Resolve the release to use: an exact tag when `version` is given (a leading
/// `v` is tried too), otherwise the newest non-prerelease (falling back to the
/// newest release of any kind, with a note).
async fn resolve_release(
    client: &reqwest::Client,
    repo: &str,
    version: Option<&str>,
) -> anyhow::Result<Release> {
    let mut releases = list_releases(client, repo).await?;
    if releases.is_empty() {
        bail!("no releases found in {repo}");
    }
    if let Some(want) = version {
        let alt = format!("v{want}");
        if let Some(idx) = releases.iter().position(|r| r.tag_name == want || r.tag_name == alt) {
            return Ok(releases.swap_remove(idx));
        }
        let tags: Vec<&str> = releases.iter().map(|r| r.tag_name.as_str()).collect();
        bail!("no release tagged \"{want}\" in {repo}; available: {}", tags.join(", "));
    }
    // Newest stable, else newest of any kind.
    if let Some(idx) = releases.iter().position(|r| !r.prerelease) {
        return Ok(releases.swap_remove(idx));
    }
    eprintln!("note: no stable release; using the newest prerelease");
    Ok(releases.swap_remove(0))
}

/// Fetch and parse an asset named `name` from `release` as UTF-8 text.
async fn fetch_text_asset(
    client: &reqwest::Client,
    release: &Release,
    name: &str,
) -> anyhow::Result<String> {
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == name)
        .with_context(|| format!("release {} has no {name} asset", release.tag_name))?;
    let bytes = download_asset(client, asset).await?;
    String::from_utf8(bytes).with_context(|| format!("{name} is not valid UTF-8"))
}

/// Parse `SHA256SUMS` (sha256sum format) into filename → lowercase hex digest.
fn parse_sha256sums(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let mut it = line.split_whitespace();
            let hex = it.next()?;
            // The filename is the remainder (a leading `*` marks binary mode).
            let name = it.next()?.trim_start_matches('*');
            Some((name.to_owned(), hex.to_ascii_lowercase()))
        })
        .collect()
}

/// Lowercase hex SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Default install directory: `$EPHPM_MIDDLEWARE_DIR` when set, else the
/// system module directory — the same places the loader searches.
fn default_dest() -> PathBuf {
    if let Ok(dir) = std::env::var("EPHPM_MIDDLEWARE_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    PathBuf::from("/usr/local/lib/ephpm/middleware")
}

/// Write `bytes` to `final_path` atomically (temp file in the same directory,
/// then rename) so a partial download never leaves a truncated module behind.
fn write_atomic(final_path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let dir = final_path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating a temp file in {}", dir.display()))?;
    std::io::Write::write_all(&mut tmp, bytes).context("writing the module bytes")?;
    tmp.as_file().sync_all().context("flushing the module to disk")?;
    tmp.persist(final_path)
        .map_err(|e| e.error)
        .with_context(|| format!("installing {}", final_path.display()))?;
    Ok(())
}

/// `ephpm middleware search-path`.
fn print_search_path() {
    let platform = ephpm_server::middleware::platform_tag();
    let ext = std::env::consts::DLL_EXTENSION;
    println!("Platform: {platform}");
    println!("A mount `library = \"<name>\"` resolves, in each directory below, by trying:");
    println!("  <name>.{platform}.{ext}   (the release asset name; `get` writes here)");
    println!("  lib<name>.{ext}");
    println!("  <name>.{ext}");
    println!("Search directories, in order:");
    for dir in ephpm_server::middleware::search_dirs() {
        println!("  {}", dir.display());
    }
}

/// `ephpm middleware list`.
async fn list(repo: &str) -> anyhow::Result<ExitCode> {
    let client = http_client()?;
    let release = resolve_release(&client, repo, None).await?;
    let manifest_text = fetch_text_asset(&client, &release, "manifest.json").await?;
    let manifest: Manifest =
        serde_json::from_str(&manifest_text).context("parsing manifest.json")?;

    println!("Repository: {repo}");
    println!("Latest release: {} (ABI major {})", release.tag_name, manifest.abi_major);
    let host = host_abi_major();
    if manifest.abi_major != host {
        println!("  warning: this binary speaks ABI major {host} — these modules will not load");
    }
    println!("Modules:");
    for m in &manifest.modules {
        let mut plats: Vec<String> = m
            .assets
            .iter()
            .map(|a| {
                if a.libc.is_empty() || a.libc == "gnu" {
                    a.platform.clone()
                } else {
                    format!("{}-{}", a.platform, a.libc)
                }
            })
            .collect();
        plats.sort();
        println!("  {:<18} {}", m.name, plats.join(", "));
    }

    let releases = list_releases(&client, repo).await?;
    let tags: Vec<String> = releases
        .iter()
        .map(|r| if r.prerelease { format!("{} (pre)", r.tag_name) } else { r.tag_name.clone() })
        .collect();
    println!("Available versions: {}", tags.join(", "));
    Ok(ExitCode::SUCCESS)
}

/// `ephpm middleware get <name>[@version]`.
async fn get(
    repo: &str,
    name_arg: &str,
    dest: Option<PathBuf>,
    musl: bool,
    allow_abi_mismatch: bool,
) -> anyhow::Result<ExitCode> {
    let (name, version) = match name_arg.split_once('@') {
        Some((n, v)) => (n, Some(v)),
        None => (name_arg, None),
    };

    let client = http_client()?;
    let release = resolve_release(&client, repo, version).await?;

    // ABI gate (download-time; the module's own `declare!` gate is the real
    // enforcement at load).
    let manifest_text = fetch_text_asset(&client, &release, "manifest.json").await?;
    let manifest: Manifest =
        serde_json::from_str(&manifest_text).context("parsing manifest.json")?;
    let host = host_abi_major();
    if manifest.abi_major != host {
        if allow_abi_mismatch {
            eprintln!(
                "warning: release ABI major {} != this binary's {host}; \
                 the module will refuse to load unless it actually matches",
                manifest.abi_major
            );
        } else {
            bail!(
                "release {} targets ABI major {} but this binary speaks {host}; \
                 refusing (pass --allow-abi-mismatch to override)",
                release.tag_name,
                manifest.abi_major
            );
        }
    }

    // Compute the asset name for this platform, and the loader file name it is
    // installed as (identical except a musl asset drops its `-musl` marker,
    // since the loader has no libc distinction).
    let platform = ephpm_server::middleware::platform_tag();
    let ext = std::env::consts::DLL_EXTENSION;
    let asset_name = if musl {
        format!("{name}.{platform}-musl.{ext}")
    } else {
        format!("{name}.{platform}.{ext}")
    };
    let loader_name = format!("{name}.{platform}.{ext}");

    // The integrity floor: verify against SHA256SUMS, not the manifest.
    let sums = parse_sha256sums(&fetch_text_asset(&client, &release, "SHA256SUMS").await?);
    let Some((_, expected)) = sums.iter().find(|(n, _)| n == &asset_name) else {
        let have: Vec<&str> = manifest
            .modules
            .iter()
            .find(|m| m.name == name)
            .map(|m| m.assets.iter().map(|a| a.file.as_str()).collect())
            .unwrap_or_default();
        bail!(
            "release {} has no asset \"{asset_name}\" for module \"{name}\" on this platform. \
             Module assets in this release: {}",
            release.tag_name,
            if have.is_empty() { "(module not found)".to_owned() } else { have.join(", ") }
        );
    };

    let asset = release.assets.iter().find(|a| a.name == asset_name).with_context(|| {
        format!(
            "release {} lists {asset_name} in SHA256SUMS but has no such asset",
            release.tag_name
        )
    })?;

    let bytes = download_asset(&client, asset).await?;
    let got = sha256_hex(&bytes);
    if &got != expected {
        bail!(
            "checksum mismatch for {asset_name}: expected {expected}, got {got} — \
             refusing to write (possible tampering or a corrupt download)"
        );
    }
    // Defense in depth: the manifest should agree with SHA256SUMS.
    if let Some(m) = manifest.modules.iter().find(|m| m.name == name)
        && let Some(a) = m.assets.iter().find(|a| a.file == asset_name)
        && !a.sha256.is_empty()
        && a.sha256.to_ascii_lowercase() != got
    {
        bail!("manifest.json and SHA256SUMS disagree on {asset_name}'s digest — refusing");
    }

    let dest_dir = dest.unwrap_or_else(default_dest);
    std::fs::create_dir_all(&dest_dir).with_context(|| {
        format!(
            "creating install directory {} (use --dest for a writable path, or run with elevated privileges)",
            dest_dir.display()
        )
    })?;
    let final_path = dest_dir.join(&loader_name);
    write_atomic(&final_path, &bytes)?;

    println!("Installed {name} {} → {}", release.tag_name, final_path.display());
    println!("Verified sha256 {got}");
    if musl {
        println!(
            "note: installed the musl build under its gnu-style loader name; mount it by \
             explicit path (`library = \"{}\"`) if a gnu module of the same name is also present",
            final_path.display()
        );
    } else {
        println!("Mount it with:  [[middleware]] library = \"{name}\"");
    }
    if !ephpm_server::middleware::search_dirs().iter().any(|d| d == &dest_dir) {
        println!(
            "note: {} is not on the loader's default search path; set EPHPM_MIDDLEWARE_DIR or \
             mount by explicit path",
            dest_dir.display()
        );
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256sums_parses_both_modes() {
        let text = "abc123  jwt.linux-x86_64.so\ndef456 *cors.darwin-aarch64.dylib\n\n";
        let parsed = parse_sha256sums(text);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0], ("jwt.linux-x86_64.so".to_owned(), "abc123".to_owned()));
        assert_eq!(parsed[1], ("cors.darwin-aarch64.dylib".to_owned(), "def456".to_owned()));
    }

    #[test]
    fn sha256_hex_is_lowercase_and_64_chars() {
        let h = sha256_hex(b"hello");
        assert_eq!(h.len(), 64);
        assert_eq!(h, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
    }

    #[test]
    fn host_abi_major_is_one() {
        // Pinned: the ABI crate is at major 1. If this ever changes, the
        // download-time gate and the release manifest must move together.
        assert_eq!(host_abi_major(), 1);
    }
}
