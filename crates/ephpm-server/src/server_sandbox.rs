//! Self-applied Landlock filesystem sandbox on the **PHP execution engine**
//! (multi-tenant breakout audit finding **F3**).
//!
//! # The gap this closes
//!
//! Even after a privilege drop ([`crate::privdrop`]), the in-process PHP engine
//! can `open(2)` any file the process uid can reach. Cross-tenant isolation
//! rests entirely on `open_basedir` + the `disable_functions` denylist, with
//! **no kernel backstop** — so a tenant that breaks out of the PHP engine
//! inherits the server's full filesystem reach (host root, when run as root).
//! F3: "the in-process engine has no self-applied sandbox."
//!
//! This module arms a **Landlock** ruleset on every thread that executes tenant
//! PHP, restricting that thread to the paths ePHPm legitimately needs and making
//! everything else — `/etc/ephpm` (the cluster secret), `/etc/ssl/private` (TLS
//! private keys), `/etc/shadow`, `/root`, other tenants' home directories —
//! unreachable, enforced by the kernel even against root. It reuses the same
//! mechanism as the out-of-process `ephpm exec` sandbox
//! (`crates/ephpm/src/exec_sandbox.rs`), applied to the long-running server.
//!
//! # Why per-thread, not "once for the whole process"
//!
//! `landlock_restrict_self(2)` restricts **only the calling thread**; sibling
//! threads that already exist are untouched, and only threads created *after*
//! the call inherit the domain. By the time the server could make a single
//! "lock down now" call, the tokio runtime and the PHP pools are already
//! running, so one call would sandbox nothing that matters.
//!
//! The threat is a tenant breakout, and tenant PHP runs **only** on threads
//! ePHPm owns for that purpose: the per-request pool ([`crate::fpm_pool`]), the
//! worker pool ([`crate::worker_pool`]), and the Xdebug debug lane
//! ([`crate::debug_lane`]). tokio worker threads never run tenant PHP (static
//! serving is Rust, path-confined to the document root). So the precise,
//! complete containment for F3 is to arm the sandbox on each PHP thread as it
//! starts — which each pool does by calling [`restrict_current_php_thread`]
//! right after its TSRM registration, before it serves a single request.
//!
//! A welcome side effect of the per-thread scope: the sandboxed domain is
//! **never** granted the TLS private key, the loaded config, or the KV/backup
//! staging dirs — those are opened on tokio/main threads, which stay outside the
//! sandbox. A naive whole-process lockdown applied after TLS keys were opened
//! would have had to grant read on them, re-exposing exactly what F3 is about.
//!
//! # The path set
//!
//! Derived from config, mirroring the request-path `open_basedir` plus the
//! read-only system directories any program needs to run:
//!
//! * **read-write**: the site trees (`sites_dir`, `document_root`), the
//!   per-vhost temp/session root, the embedded-database directory, and
//!   `/dev/{null,zero,full}`;
//! * **read-only**: the operator-owned per-site overrides, the public CA trust
//!   store (so an app's HTTPS client can verify certificates), a minimal
//!   non-secret slice of `/etc` (name resolution + the loader cache + timezone),
//!   `/dev/{u,}random`, and `/proc/self`;
//! * **read + execute**: `/usr`, `/lib`, `/lib64`, `/bin`, `/sbin`, `/opt` —
//!   where the loader and shared libraries live.
//!
//! Landlock governs the filesystem only: it does **not** restrict the network,
//! and already-open file descriptors keep working.
//!
//! # Degradation and enforcement
//!
//! Controlled by `[server] self_sandbox` ([`SelfSandboxMode`]):
//!
//! * `Auto` (default) — on in multi-tenant mode when the kernel enforces
//!   Landlock (≥ 5.13, ABI 1); if the kernel lacks it, `WARN` and serve
//!   unsandboxed. Off in single-tenant mode.
//! * `Enforce` — require it: [`init`] **fails startup** if the kernel does not
//!   enforce Landlock, and [`restrict_current_php_thread`] returns an error (so
//!   the pool retires the thread) if a thread cannot arm it.
//! * `Off` — disabled.
//!
//! Ignored on non-Linux (Windows/macOS compile the inert stub) and in
//! `ephpm dev` (a developer iterating locally is never sandboxed).

// The Linux impl calls `prctl(PR_SET_NO_NEW_PRIVS)` and the Landlock FFI; the
// one unsafe block carries a SAFETY note explaining the invariant it upholds.
#![allow(unsafe_code)]

#[cfg(target_os = "linux")]
use std::sync::OnceLock;

use ephpm_config::{Config, SelfSandboxMode};

/// The resolved sandbox for this process: `Some` with the path set once [`init`]
/// decides the engine should be sandboxed, `None` when it should not (disabled,
/// dev mode, single-tenant auto, non-Linux, or a kernel without Landlock in
/// `Auto` mode). Written exactly once by [`init`].
#[cfg(target_os = "linux")]
static SANDBOX: OnceLock<Option<imp::SandboxSpec>> = OnceLock::new();

/// Decide whether the PHP engine should be sandboxed and, if so, validate the
/// Landlock ruleset against the running kernel. Call **once**, early in `serve`,
/// before the PHP pools spawn any thread.
///
/// On Linux, when the sandbox is enabled, this computes the path set from
/// `config`, then preflights it on a throwaway thread (so the check never
/// restricts a real server thread) to learn whether the kernel actually enforces
/// Landlock. The resolved decision is stored for [`restrict_current_php_thread`].
///
/// # Errors
///
/// Returns an error only in `Enforce` mode when the kernel does not enforce
/// Landlock — the one case where continuing would serve without the sandbox the
/// operator demanded. Every softer outcome (`Auto` on an old kernel, a disabled
/// sandbox, non-Linux) logs and returns `Ok`.
pub fn init(config: &Config, dev_mode: bool) -> anyhow::Result<()> {
    let mode = config.server.self_sandbox_mode();
    let multi_tenant = config.server.sites_dir.is_some();

    if dev_mode {
        if mode != SelfSandboxMode::Off {
            tracing::info!(
                "[server] self_sandbox is ignored in `ephpm dev` — the PHP engine is not \
                 Landlock-sandboxed while iterating locally"
            );
        }
        mark_disabled();
        return Ok(());
    }

    let enabled = match mode {
        SelfSandboxMode::Off => false,
        SelfSandboxMode::Auto => multi_tenant,
        SelfSandboxMode::Enforce => true,
    };
    if !enabled {
        if mode == SelfSandboxMode::Auto && !multi_tenant {
            tracing::debug!(
                "[server] self_sandbox = auto: single-tenant, engine Landlock sandbox off \
                 (set self_sandbox = \"enforce\" to opt in)"
            );
        }
        mark_disabled();
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        init_enabled(config, mode)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Landlock is Linux-only; never break Windows/macOS. `enforce` cannot be
        // honoured here, so say so loudly (an operator asking for a hard sandbox
        // on a platform that cannot provide one deserves to know), but do not
        // fail startup — the platform simply has no self-sandbox.
        if mode == SelfSandboxMode::Enforce {
            tracing::warn!(
                "[server] self_sandbox = \"enforce\" is ignored on this platform — the \
                 Landlock engine sandbox is Linux-only"
            );
        }
        mark_disabled();
        Ok(())
    }
}

/// Arm the engine sandbox on the **current** thread. Called by each PHP pool
/// thread ([`crate::fpm_pool`], [`crate::worker_pool`], [`crate::debug_lane`])
/// right after TSRM registration and before it serves any request.
///
/// A no-op (returns `Ok`) when [`init`] decided the engine should not be
/// sandboxed, including when [`init`] was never called (unit tests that spawn
/// pool threads directly).
///
/// # Errors
///
/// Returns an error when a sandbox is configured but cannot be armed on this
/// thread. The caller treats that as a boot failure and retires the thread
/// rather than serve tenant PHP unsandboxed — the fail-closed posture the
/// `Enforce` mode (and the `Auto` mode, once [`init`] confirmed the kernel
/// enforces Landlock) require.
pub fn restrict_current_php_thread() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        match SANDBOX.get() {
            Some(Some(spec)) => imp::restrict_current_thread(spec),
            // `None` stored (disabled) or never initialised (tests): no-op.
            _ => Ok(()),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(())
    }
}

/// Record that the engine is not sandboxed (store `None`), idempotently.
fn mark_disabled() {
    #[cfg(target_os = "linux")]
    {
        let _ = SANDBOX.set(None);
    }
}

#[cfg(target_os = "linux")]
fn init_enabled(config: &Config, mode: SelfSandboxMode) -> anyhow::Result<()> {
    let spec = imp::SandboxSpec::from_config(config);
    tracing::info!(
        mode = ?mode,
        read_write = ?spec.read_write,
        read_exec = ?spec.read_exec,
        read_only = ?spec.read_only,
        "self-sandbox: arming the PHP engine Landlock ruleset on each PHP thread"
    );

    match imp::preflight(&spec) {
        imp::Enforcement::Full => {
            tracing::info!("self-sandbox: kernel fully enforces Landlock (ABI 1+)");
            let _ = SANDBOX.set(Some(spec));
            Ok(())
        }
        imp::Enforcement::Partial => {
            tracing::warn!(
                "self-sandbox: this kernel enforces only a subset of the requested Landlock \
                 access rights — the engine sandbox is active but weaker than intended"
            );
            let _ = SANDBOX.set(Some(spec));
            Ok(())
        }
        imp::Enforcement::None => {
            if mode == SelfSandboxMode::Enforce {
                anyhow::bail!(
                    "[server] self_sandbox = \"enforce\" but this kernel does not enforce \
                     Landlock (needs Linux >= 5.13). Upgrade the kernel, or set self_sandbox \
                     = \"auto\" (warn and continue) or \"off\" to run without the engine \
                     filesystem sandbox."
                );
            }
            tracing::warn!(
                "self-sandbox: this kernel does not enforce Landlock (needs Linux >= 5.13) — \
                 the PHP engine is NOT filesystem-sandboxed. Cross-tenant and host isolation \
                 rest on open_basedir + disable_functions alone. Set self_sandbox = \
                 \"enforce\" to make this fatal."
            );
            let _ = SANDBOX.set(None);
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::path::{Path, PathBuf};

    use anyhow::Context as _;
    use ephpm_config::Config;
    use landlock::{
        ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus, path_beneath_rules,
    };

    /// The paths the PHP engine is allowed to reach, by access class. Built once
    /// from config in [`SandboxSpec::from_config`]; the same instance both
    /// preflights the kernel and arms every PHP thread.
    #[derive(Debug, Clone)]
    pub(super) struct SandboxSpec {
        /// Read + write + the directory-content rights (site trees, state root,
        /// embedded DB, `/dev/{null,zero,full}`).
        pub(super) read_write: Vec<PathBuf>,
        /// Read + execute (loader + shared libraries).
        pub(super) read_exec: Vec<PathBuf>,
        /// Read only (overrides, CA store, minimal `/etc`, entropy, `/proc/self`).
        pub(super) read_only: Vec<PathBuf>,
    }

    impl SandboxSpec {
        /// Derive the allowed paths from config. Only paths that currently exist
        /// are kept (Landlock opens each rule path, so a non-existent entry would
        /// error) — see [`existing`].
        pub(super) fn from_config(config: &Config) -> Self {
            let mut rw: Vec<&Path> = Vec::new();

            // The tenant code + data trees the request path grants via
            // open_basedir. In multi-tenant mode every vhost lives under
            // `sites_dir`; `document_root` is the single-site root / fallback.
            if let Some(sites) = config.server.sites_dir.as_deref() {
                rw.push(sites);
            }
            rw.push(config.server.document_root.as_path());

            // Per-vhost temp/session root. The router creates a private 0700
            // subdir per tenant under here; PHP writes sessions/uploads there.
            let vhost_base = std::env::temp_dir().join("ephpm-vhosts");
            // Single-tenant has no per-vhost base — PHP falls back to the system
            // temp dir for sessions/uploads, so grant that instead.
            let temp_base =
                if config.server.sites_dir.is_some() { vhost_base } else { std::env::temp_dir() };

            // Embedded-database directory ([db.sqlite] dir): the per-site `.db`
            // files plus their `-wal`/`-shm`, opened on the PHP thread via the
            // in-process bridge.
            let sqlite_dir =
                config.db.sqlite.as_ref().and_then(|s| s.dir.as_ref()).map(PathBuf::from);

            let mut rw_owned: Vec<PathBuf> = rw.into_iter().map(Path::to_path_buf).collect();
            rw_owned.push(temp_base);
            if let Some(dir) = sqlite_dir {
                rw_owned.push(dir);
            }

            // A Landlock rule must open each path, so a directory that does not
            // exist yet would be silently dropped by `existing_owned` below and
            // then blocked by the kernel the moment PHP tried to use it. ePHPm
            // creates these two at privilege-drop time (`privdrop`), but the
            // sandbox is armed earlier (before `bind_listeners` spawns the PHP
            // threads), so create them here too — best-effort and idempotent, the
            // same `create_dir_all` privdrop performs. The site trees and
            // `document_root` are operator-provided and must already exist; a
            // missing one is dropped rather than created (there is no tenant to
            // serve from it).
            for dir in &rw_owned {
                // Only the data dirs we own, never the operator's site trees.
                if dir == config.server.document_root.as_path()
                    || config.server.sites_dir.as_deref() == Some(dir.as_path())
                {
                    continue;
                }
                let _ = std::fs::create_dir_all(dir);
            }
            rw_owned.extend(
                [Path::new("/dev/null"), Path::new("/dev/zero"), Path::new("/dev/full")]
                    .into_iter()
                    .map(Path::to_path_buf),
            );

            let read_exec = existing(&[
                Path::new("/usr"),
                Path::new("/lib"),
                Path::new("/lib64"),
                Path::new("/bin"),
                Path::new("/sbin"),
                Path::new("/opt"),
            ]);

            // Minimal non-secret /etc (name resolution, loader cache, timezone),
            // entropy, /proc/self — deliberately NOT /etc, /proc, or /dev
            // wholesale, which would re-expose /etc/ephpm, /etc/shadow, and other
            // pids' /proc/<pid>/environ.
            let mut ro: Vec<PathBuf> = existing(&[
                Path::new("/etc/passwd"),
                Path::new("/etc/group"),
                Path::new("/etc/nsswitch.conf"),
                Path::new("/etc/gai.conf"),
                Path::new("/etc/ld.so.cache"),
                Path::new("/etc/ld.so.preload"),
                Path::new("/etc/localtime"),
                Path::new("/etc/resolv.conf"),
                Path::new("/etc/hosts"),
                Path::new("/dev/urandom"),
                Path::new("/dev/random"),
                Path::new("/proc/self"),
            ]);
            // Operator-owned per-site overrides (read per request). Lives outside
            // `sites_dir` by a startup invariant, so granting read here never
            // widens a tenant's reach into another's container.
            if let Some(dir) = config.server.effective_site_overrides_dir() {
                ro.extend(existing(&[dir]));
            }
            // Public CA trust store, so an app's HTTPS client can verify certs
            // (same rationale and public-only path set as the exec sandbox).
            ro.extend(existing(&ca_trust_dirs()));

            Self { read_write: existing_owned(rw_owned), read_exec, read_only: ro }
        }
    }

    /// Whether the kernel enforces the ruleset, learned by a preflight that does
    /// not touch a real server thread. Mirrors [`RulesetStatus`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Enforcement {
        Full,
        Partial,
        None,
    }

    /// Preflight the ruleset on a **throwaway** thread.
    ///
    /// Landlock is per-thread, so restricting a short-lived helper thread reveals
    /// whether the kernel enforces the ruleset without restricting any thread the
    /// server actually uses. The helper exits immediately after reporting.
    pub(super) fn preflight(spec: &SandboxSpec) -> Enforcement {
        let spec = spec.clone();
        std::thread::Builder::new()
            .name("ephpm-landlock-probe".to_string())
            .spawn(move || match apply(&spec) {
                Ok(RulesetStatus::FullyEnforced) => Enforcement::Full,
                Ok(RulesetStatus::PartiallyEnforced) => Enforcement::Partial,
                Ok(RulesetStatus::NotEnforced) => Enforcement::None,
                Err(e) => {
                    tracing::warn!(error = %e, "self-sandbox: Landlock preflight failed");
                    Enforcement::None
                }
            })
            .ok()
            .and_then(|h| h.join().ok())
            .unwrap_or(Enforcement::None)
    }

    /// Arm the ruleset on the **current** thread (the PHP pool thread).
    pub(super) fn restrict_current_thread(spec: &SandboxSpec) -> anyhow::Result<()> {
        match apply(spec).context("self-sandbox: failed to arm Landlock on this PHP thread")? {
            RulesetStatus::NotEnforced => {
                // init() already confirmed enforcement on a probe thread, so
                // reaching here means the kernel changed under us or a resource
                // limit was hit. Fail closed rather than serve unsandboxed.
                anyhow::bail!(
                    "self-sandbox: Landlock reported NotEnforced on this PHP thread after \
                     startup confirmed enforcement — refusing to serve this thread unsandboxed"
                );
            }
            RulesetStatus::FullyEnforced | RulesetStatus::PartiallyEnforced => Ok(()),
        }
    }

    /// Build and apply the ruleset on the current thread, returning its status.
    ///
    /// ABI v1 (widest kernel support) in best-effort mode, so a newer kernel
    /// still enforces the v1 rights and an older one degrades rather than erroring.
    ///
    /// Sets `PR_SET_NO_NEW_PRIVS` first. Landlock without `CAP_SYS_ADMIN`
    /// requires it, and the PHP pools respawn threads *after* the process has
    /// dropped privileges, so a replacement thread arms the sandbox as a non-root
    /// uid — which only succeeds with no-new-privs set. It is per-thread and the
    /// thread sets it on itself, so the arming works whether this thread is still
    /// root (initial spawn) or already dropped (a respawn). It is harmless for a
    /// PHP thread, which never `execve`s a setuid binary.
    fn apply(spec: &SandboxSpec) -> anyhow::Result<RulesetStatus> {
        // SAFETY: prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) is the documented form
        // with scalar arguments and no pointer operands; the return is checked.
        let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error())
                .context("landlock: prctl(PR_SET_NO_NEW_PRIVS) failed");
        }

        let abi = ABI::V1;
        let read_exec = AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir;
        let read_only = AccessFs::from_read(abi);
        let read_write = AccessFs::from_all(abi);

        let status = Ruleset::default()
            .set_compatibility(CompatLevel::BestEffort)
            .handle_access(AccessFs::from_all(abi))
            .context("landlock: handle_access")?
            .create()
            .context("landlock: create ruleset")?
            .add_rules(path_beneath_rules(&spec.read_write, read_write))
            .context("landlock: add read/write rules")?
            .add_rules(path_beneath_rules(&spec.read_exec, read_exec))
            .context("landlock: add read/exec rules")?
            .add_rules(path_beneath_rules(&spec.read_only, read_only))
            .context("landlock: add read-only rules")?
            .restrict_self()
            .context("landlock: restrict_self")?;
        Ok(status.ruleset)
    }

    /// The public CA trust-store directories, granted read-only so an in-sandbox
    /// HTTPS client can verify certificates. Public, non-secret dirs only — never
    /// a private-key sibling (`/etc/ssl/private`) or a parent (`/etc`, `/etc/ssl`,
    /// `/etc/pki`) that would re-expose the cluster secret or `/etc/shadow`.
    /// Identical set to the exec sandbox's `ca_trust_dirs`.
    fn ca_trust_dirs() -> [&'static Path; 3] {
        [
            // Debian / Ubuntu / Alpine.
            Path::new("/etc/ssl/certs"),
            // RHEL / Alma / Fedora: the symlink entry dir.
            Path::new("/etc/pki/tls/certs"),
            // RHEL / Alma / Fedora: the real extracted-bundle tree the symlinks
            // resolve into (Landlock checks the resolved target).
            Path::new("/etc/pki/ca-trust"),
        ]
    }

    /// Keep only the paths that currently exist, as owned `PathBuf`s.
    fn existing(paths: &[&Path]) -> Vec<PathBuf> {
        paths.iter().filter(|p| p.exists()).map(|p| p.to_path_buf()).collect()
    }

    /// `existing` over already-owned paths (used for the config-derived set).
    fn existing_owned(paths: Vec<PathBuf>) -> Vec<PathBuf> {
        paths.into_iter().filter(|p| p.exists()).collect()
    }

    #[cfg(test)]
    mod tests {
        use ephpm_config::Config;

        use super::*;

        /// Load a multi-tenant config with a `[db.sqlite] dir` from TOML —
        /// `SqliteConfig` has no `Default`, so building it by hand is brittle;
        /// going through the real loader also exercises the config path.
        fn multi_tenant_config(sites: &Path, sqlite_dir: &Path) -> Config {
            let dir = tempfile::tempdir().expect("tempdir");
            let file = dir.path().join("ephpm.toml");
            let body = format!(
                "[server]\nsites_dir = {sites:?}\n\n[db.sqlite]\ndir = {sqlite_dir:?}\n",
                sites = sites.to_string_lossy(),
                sqlite_dir = sqlite_dir.to_string_lossy(),
            );
            std::fs::write(&file, body).expect("write toml");
            Config::load(&file).expect("load config")
        }

        #[test]
        fn spec_includes_the_site_and_db_trees_and_excludes_secrets() {
            let tmp = std::env::temp_dir().join(format!("ephpm-sbx-{}", std::process::id()));
            let sites = tmp.join("sites");
            let sqlite = tmp.join("db");
            std::fs::create_dir_all(&sites).unwrap();
            std::fs::create_dir_all(&sqlite).unwrap();

            let config = multi_tenant_config(&sites, &sqlite);
            let spec = SandboxSpec::from_config(&config);

            assert!(spec.read_write.contains(&sites), "site tree must be writable");
            assert!(spec.read_write.contains(&sqlite), "embedded-db dir must be writable");
            assert!(
                spec.read_write.iter().any(|p| p.ends_with("ephpm-vhosts")),
                "per-vhost state root must be writable"
            );

            // The security bound: the sandbox never grants a secret path or a
            // broad /etc/ /proc/ parent, in any access class.
            for p in spec.read_write.iter().chain(&spec.read_exec).chain(&spec.read_only) {
                let s = p.to_string_lossy();
                assert!(!s.contains("private"), "must never grant a private-key dir: {s}");
                assert_ne!(s, "/etc/shadow", "must never grant /etc/shadow");
                assert!(!p.starts_with("/etc/ephpm"), "must never grant the cluster secret: {s}");
                assert!(!p.starts_with("/root"), "must never grant /root: {s}");
                assert!(
                    !matches!(&*s, "/etc" | "/etc/ssl" | "/etc/pki" | "/proc" | "/dev"),
                    "must not grant a broad parent dir: {s}"
                );
            }
            let _ = std::fs::remove_dir_all(&tmp);
        }

        #[test]
        fn ca_trust_dirs_are_public_cert_dirs_only() {
            for p in ca_trust_dirs() {
                let s = p.to_string_lossy();
                assert!(!s.contains("private"), "must never grant a private-key dir: {s}");
                assert!(!matches!(&*s, "/etc" | "/etc/ssl" | "/etc/pki"), "no broad parent: {s}");
            }
        }

        /// After arming the sandbox on a throwaway thread, a path outside the
        /// ruleset must be denied — the kernel-enforced lockdown F3 asks for.
        /// Gated on the kernel actually enforcing Landlock (the preflight tells
        /// us), so it is a no-op on CI kernels without it rather than a false
        /// failure.
        #[test]
        fn lockdown_denies_paths_outside_the_ruleset() {
            let tmp = std::env::temp_dir().join(format!("ephpm-sbx-deny-{}", std::process::id()));
            let sites = tmp.join("sites");
            std::fs::create_dir_all(&sites).unwrap();
            // A secret-shaped file OUTSIDE the ruleset (not under any granted path).
            let outside = tmp.join("outside-secret");
            std::fs::write(&outside, b"top secret").unwrap();

            let mut config = Config::default_config().expect("default config");
            config.server.sites_dir = Some(sites.clone());
            // document_root points at the granted site tree so in-ruleset reads work.
            config.server.document_root = sites.clone();
            let spec = SandboxSpec::from_config(&config);

            if preflight(&spec) != Enforcement::Full {
                eprintln!("kernel does not fully enforce Landlock; skipping deny assertion");
                let _ = std::fs::remove_dir_all(&tmp);
                return;
            }

            // Run the assertions on their own thread, which we sandbox and then
            // discard — never the test harness thread.
            let in_ruleset = sites.join("ok.txt");
            std::fs::write(&in_ruleset, b"fine").unwrap();
            let outside2 = outside.clone();
            let handle = std::thread::spawn(move || {
                restrict_current_thread(&spec).expect("arm sandbox on probe thread");
                // The granted path is still readable.
                assert!(std::fs::read(&in_ruleset).is_ok(), "in-ruleset read must succeed");
                // The path outside the ruleset is denied by the kernel.
                let denied = std::fs::read(&outside2);
                assert!(denied.is_err(), "read outside the ruleset must be denied after lockdown");
            });
            handle.join().expect("probe thread");
            let _ = std::fs::remove_dir_all(&tmp);
        }
    }
}
