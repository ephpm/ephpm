//! Process-wide privilege drop for `[server] run_as_user` / `run_as_group`.
//!
//! ePHPm is a single process that runs every tenant's PHP on tokio
//! `spawn_blocking` threads. There is no per-thread uid on Linux worth relying
//! on (glibc broadcasts credential changes to every thread via the `setxid`
//! mechanism), and per-tenant uids would require per-tenant *processes*, which
//! this model does not have. What this module provides is the one thing that
//! *is* achievable in-process and still valuable: after binding privileged
//! ports and opening everything root is needed for, drop the **whole process**
//! from root to a single unprivileged uid/gid before serving any request.
//!
//! This removes the root-escalation blast radius — a PHP/FFI compromise no
//! longer executes as uid 0 — but it does **not** add a cross-tenant boundary:
//! all tenants still share this one uid, so cross-tenant confidentiality still
//! rests on `open_basedir` + the `disable_functions` denylist, not on kernel
//! permissions. See `site/content/guides/virtual-hosts.md`.
//!
//! # Ordering
//!
//! The caller invokes [`drop_privileges`] at the very end of startup — after
//! `bind_listeners` (privileged ports) and `start_db_proxies`, immediately
//! before the accept loop. At that point no PHP request has run, so the only
//! threads alive are tokio's own; the `setxid` broadcast is cheap and cannot
//! race a live request. Directories ePHPm keeps *writing* after the drop
//! (per-site database files, per-vhost temp/session dirs, ACME cert cache) are
//! `chown`ed to the target uid/gid first.

// Privilege drop is inherently a set of libc credential syscalls; every unsafe
// block below carries a SAFETY note explaining the FFI invariant it upholds.
#![allow(unsafe_code)]

#[cfg(unix)]
use anyhow::Context as _;
use ephpm_config::Config;

/// Drop the process to `[server] run_as_user` / `run_as_group` if configured.
///
/// A no-op when `run_as_user` is unset. On Unix, when set and the process is
/// running as root, this binds nothing new — it `chown`s the runtime-writable
/// directories, then permanently drops supplementary groups, gid, and uid, and
/// verifies the drop actually took (fails closed if euid is still 0).
///
/// # Errors
///
/// Returns an error if the target user/group cannot be resolved, if any
/// `set*id` syscall fails, or if the post-drop identity is not the requested
/// one (a failed drop must never be mistaken for a successful one).
#[cfg(unix)]
// The real/effective uid/gid names are intentionally parallel here.
#[allow(clippy::similar_names)]
pub fn drop_privileges(config: &Config) -> anyhow::Result<()> {
    let Some(user_spec) = config.server.run_as_user.as_deref() else {
        // Group without user is meaningless — warn so it is never silent.
        if config.server.run_as_group.is_some() {
            tracing::warn!(
                "[server] run_as_group is set without run_as_user — ignored; a \
                 privilege drop needs a target user"
            );
        }
        return Ok(());
    };

    let (uid, primary_gid) = resolve_user(user_spec)
        .with_context(|| format!("failed to resolve [server] run_as_user = \"{user_spec}\""))?;
    let gid = match config.server.run_as_group.as_deref() {
        Some(group_spec) => resolve_group(group_spec).with_context(|| {
            format!("failed to resolve [server] run_as_group = \"{group_spec}\"")
        })?,
        // No explicit group: the user's primary group (named user) or the same
        // numeric id as the uid (numeric user with no passwd entry).
        None => primary_gid.unwrap_or(uid),
    };

    // SAFETY: geteuid/getuid never fail and take no arguments.
    let current_euid = unsafe { libc::geteuid() };
    if current_euid != 0 {
        // A drop can only happen from root. If we are already the target uid,
        // the operator's intent is satisfied; otherwise we cannot comply and
        // say so loudly rather than pretend we hardened the process.
        // SAFETY: getuid never fails.
        if unsafe { libc::getuid() } == uid {
            tracing::info!(uid, gid, "already running as the target uid; no privilege drop needed");
        } else {
            tracing::warn!(
                uid,
                gid,
                current_euid,
                "[server] run_as_user is set but the process is not running as root — \
                 cannot drop privileges; continuing as the current uid. Start ePHPm as \
                 root for the drop to take effect."
            );
        }
        return Ok(());
    }

    // chown everything ePHPm keeps writing to after the drop, BEFORE dropping,
    // while we still have the privilege to do so. Best-effort per directory —
    // a missing optional directory is not fatal, but a chown failure on a
    // directory that exists is surfaced.
    chown_runtime_dirs(config, uid, gid)?;

    // Order is load-bearing: drop supplementary groups and the gid while still
    // uid 0, then the uid last. Doing setuid first would forfeit the privilege
    // needed for setgroups/setgid.
    //
    // SAFETY: setgroups/setgid/setuid are the documented POSIX credential
    // syscalls. We are single-purpose here (startup), pass a valid pointer/len
    // for the one-element group list, and check every return value below. glibc
    // applies these process-wide across all threads.
    let groups = [gid as libc::gid_t];
    let rc = unsafe { libc::setgroups(groups.len() as _, groups.as_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .context("setgroups() failed while dropping privileges");
    }
    // SAFETY: see above.
    if unsafe { libc::setgid(gid as libc::gid_t) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("setgid() failed while dropping privileges");
    }
    // SAFETY: see above. setuid from euid 0 sets real, effective, AND saved
    // uids to `uid`, so the privilege cannot be regained afterwards.
    if unsafe { libc::setuid(uid as libc::uid_t) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("setuid() failed while dropping privileges");
    }

    // Verify the drop actually took — fail closed. A silent failure here would
    // leave the process running as root while the config claims otherwise.
    // SAFETY: these getters never fail and take no arguments.
    let real_uid = unsafe { libc::getuid() };
    let eff_uid = unsafe { libc::geteuid() };
    let real_gid = unsafe { libc::getgid() };
    let eff_gid = unsafe { libc::getegid() };
    if real_uid != uid || eff_uid != uid || real_gid != gid || eff_gid != gid {
        anyhow::bail!(
            "privilege drop did not take: wanted uid={uid} gid={gid}, got \
             uid={real_uid} euid={eff_uid} gid={real_gid} egid={eff_gid}"
        );
    }
    // Belt and suspenders: prove root cannot be regained.
    // SAFETY: seteuid with no argument-side effects; a success here would mean
    // the saved-set-uid still held 0, which must never happen post-drop.
    if unsafe { libc::seteuid(0) } == 0 {
        anyhow::bail!("privilege drop is reversible (seteuid(0) succeeded) — refusing to serve");
    }

    tracing::info!(
        uid,
        gid,
        "dropped privileges: process now runs unprivileged (single non-root uid — \
         NOT per-tenant; cross-tenant isolation still rests on open_basedir + \
         disable_functions)"
    );
    Ok(())
}

/// Windows / non-Unix stub: privilege drop is not supported.
///
/// # Errors
///
/// Never fails on non-Unix — the `run_as_*` knobs are ignored with a WARN.
/// (The `Result` shape matches the Unix implementation.)
#[cfg(not(unix))]
pub fn drop_privileges(config: &Config) -> anyhow::Result<()> {
    if config.server.run_as_user.is_some() || config.server.run_as_group.is_some() {
        tracing::warn!(
            "[server] run_as_user / run_as_group are set but privilege dropping is \
             only supported on Unix — ignored on this platform"
        );
    }
    Ok(())
}

/// `chown` the directories ePHPm keeps writing to after the drop, so the
/// unprivileged uid can still create per-site databases, per-vhost temp/session
/// files, and ACME certificates.
#[cfg(unix)]
fn chown_runtime_dirs(config: &Config, uid: u32, gid: u32) -> anyhow::Result<()> {
    // Per-site SQLite database files ([db.sqlite] dir). Created lazily on first
    // request for each tenant, so the directory must be writable by the target.
    if let Some(dir) = config.db.sqlite.as_ref().and_then(|s| s.dir.as_ref()) {
        let dir = std::path::Path::new(dir);
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create [db.sqlite] dir {}", dir.display()))?;
        chown_tree(dir, uid, gid)
            .with_context(|| format!("failed to chown [db.sqlite] dir {}", dir.display()))?;
    }

    // Per-vhost temp/session base (<tmpdir>/ephpm-vhosts). The router creates a
    // private 0700 subdir per tenant under here on first request; the base must
    // be owned by the target uid so those mkdirs succeed post-drop.
    if config.server.sites_dir.is_some() {
        let base = std::env::temp_dir().join("ephpm-vhosts");
        std::fs::create_dir_all(&base)
            .with_context(|| format!("failed to create per-vhost temp base {}", base.display()))?;
        // 0700: only the target uid may traverse/list tenants' state roots.
        set_mode(&base, 0o700);
        chown_tree(&base, uid, gid)
            .with_context(|| format!("failed to chown per-vhost temp base {}", base.display()))?;
    }

    // ACME certificate cache: rustls-acme writes renewed certs here at runtime.
    if let Some(tls) = config.server.tls.as_ref()
        && tls.is_acme()
    {
        let dir = &tls.cache_dir;
        // start_acme already created it during bind; create_dir_all is a
        // harmless idempotent guard.
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create ACME cache dir {}", dir.display()))?;
        chown_tree(dir, uid, gid)
            .with_context(|| format!("failed to chown ACME cache dir {}", dir.display()))?;
    }

    Ok(())
}

/// Recursively `lchown` `path` and its descendants to `uid`/`gid`.
///
/// Uses `lchown` (never follows symlinks) so a symlink planted inside one of
/// these directories cannot redirect the ownership change onto an unrelated
/// file. Bounded in practice — these trees hold a handful of database, temp,
/// or certificate files.
#[cfg(unix)]
fn chown_tree(path: &std::path::Path, uid: u32, gid: u32) -> std::io::Result<()> {
    lchown(path, uid, gid)?;
    // Only descend into real directories, never through a symlink.
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        for entry in std::fs::read_dir(path)? {
            chown_tree(&entry?.path(), uid, gid)?;
        }
    }
    Ok(())
}

/// `lchown(2)` wrapper.
#[cfg(unix)]
fn lchown(path: &std::path::Path, uid: u32, gid: u32) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: `c` is a valid NUL-terminated C string that outlives the call.
    if unsafe { libc::lchown(c.as_ptr(), uid as libc::uid_t, gid as libc::gid_t) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Best-effort `chmod`; a failure here only weakens/omits a defence-in-depth
/// permission tightening, so it is logged rather than fatal.
#[cfg(unix)]
fn set_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
        tracing::warn!(path = %path.display(), error = %e, "failed to set directory mode");
    }
}

/// Resolve a user spec (numeric uid or name) to `(uid, primary_gid)`.
/// `primary_gid` is `Some` only for a named user resolved via `getpwnam`.
#[cfg(unix)]
fn resolve_user(spec: &str) -> anyhow::Result<(u32, Option<u32>)> {
    if let Ok(uid) = spec.parse::<u32>() {
        return Ok((uid, None));
    }
    let c = std::ffi::CString::new(spec)
        .map_err(|_| anyhow::anyhow!("user name contains an interior NUL byte"))?;
    // SAFETY: getpwnam is called once at startup before any other thread would
    // touch the passwd database; `c` is a valid NUL-terminated string. The
    // returned pointer (if non-null) points at libc's static passwd buffer,
    // which we read immediately and do not retain.
    let pw = unsafe { libc::getpwnam(c.as_ptr()) };
    if pw.is_null() {
        anyhow::bail!("no such user (getpwnam returned NULL)");
    }
    // SAFETY: pw is non-null and points at a valid `passwd`.
    let (uid, gid) = unsafe { ((*pw).pw_uid, (*pw).pw_gid) };
    Ok((uid, Some(gid)))
}

/// Resolve a group spec (numeric gid or name) to a gid.
#[cfg(unix)]
fn resolve_group(spec: &str) -> anyhow::Result<u32> {
    if let Ok(gid) = spec.parse::<u32>() {
        return Ok(gid);
    }
    let c = std::ffi::CString::new(spec)
        .map_err(|_| anyhow::anyhow!("group name contains an interior NUL byte"))?;
    // SAFETY: same contract as getpwnam above — startup-only, valid string,
    // static buffer read immediately.
    let gr = unsafe { libc::getgrnam(c.as_ptr()) };
    if gr.is_null() {
        anyhow::bail!("no such group (getgrnam returned NULL)");
    }
    // SAFETY: gr is non-null and points at a valid `group`.
    Ok(unsafe { (*gr).gr_gid })
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::Mutex;

    use ephpm_config::Config;

    use super::*;

    /// Serializes the tests that perform a *name* lookup.
    ///
    /// `getpwnam`/`getgrnam` return a pointer into libc's shared static buffer.
    /// Production upholds the invariant in [`resolve_user`]'s SAFETY note — the
    /// drop happens once, at startup, before other threads exist — but the test
    /// harness runs these concurrently, where one lookup overwrites another's
    /// buffer. That surfaced as `resolve_user("root")` returning the *current*
    /// user's uid, and it is timing-dependent: it appears and disappears as
    /// unrelated tests change the scheduling.
    ///
    /// Locking here makes the tests honour the same single-lookup-at-a-time
    /// contract the production caller does. Removing the invariant entirely
    /// (switching to the reentrant `getpwnam_r`/`getgrnam_r`) is the more
    /// thorough fix and is left as follow-up work — it changes unsafe FFI and
    /// is unrelated to whatever change happens to be in flight.
    static NAME_LOOKUP: Mutex<()> = Mutex::new(());

    #[test]
    fn numeric_user_and_group_parse_without_lookup() {
        // Numeric specs short-circuit before any libc call, so no lock needed.
        assert_eq!(resolve_user("1000").unwrap(), (1000, None));
        assert_eq!(resolve_group("2000").unwrap(), 2000);
    }

    #[test]
    fn unknown_name_is_an_error_not_a_panic() {
        let _guard = NAME_LOOKUP.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(resolve_user("definitely-no-such-user-zzz").is_err());
        assert!(resolve_group("definitely-no-such-group-zzz").is_err());
    }

    #[test]
    fn root_user_and_group_resolve() {
        let _guard = NAME_LOOKUP.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // root exists on every Unix; getpwnam/getgrnam must find it.
        let (uid, gid) = resolve_user("root").unwrap();
        assert_eq!(uid, 0);
        assert_eq!(gid, Some(0));
    }

    #[test]
    fn no_run_as_user_is_a_noop() {
        // Default config sets neither field; the drop must return Ok without
        // touching credentials, regardless of whether the test runs as root.
        let config = Config::default_config().expect("default config");
        assert!(config.server.run_as_user.is_none());
        drop_privileges(&config).expect("no-op drop must succeed");
    }
}
