//! Process-wide privilege drop for `[server] run_as_user` / `run_as_group`.
//!
//! ePHPm is a single process that runs every tenant's PHP on its own
//! execution-pool threads. There is no per-thread uid on Linux worth relying
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
/// directories (plus each of `extra_paths`, non-recursively — the KV
/// snapshot directory and file), then permanently drops supplementary groups,
/// gid, and uid, and verifies the drop actually took (fails closed if euid is
/// still 0).
///
/// # Errors
///
/// Returns an error if the target user/group cannot be resolved, if a chown
/// or any `set*id` syscall fails, or if the post-drop identity is not the
/// requested one (a failed drop must never be mistaken for a successful one).
#[cfg(unix)]
// The real/effective uid/gid names are intentionally parallel here.
#[allow(clippy::similar_names)]
pub fn drop_privileges(config: &Config, extra_paths: &[std::path::PathBuf]) -> anyhow::Result<()> {
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
    // Caller-supplied paths (the KV snapshot directory and file): each one
    // itself only, never recursively, and never through a symlink.
    for path in extra_paths {
        lchown(path, uid, gid).with_context(|| format!("failed to chown {}", path.display()))?;
    }

    drop_to_user(uid, gid)?;

    tracing::info!(
        uid,
        gid,
        "dropped privileges: process now runs unprivileged (single non-root uid — \
         NOT per-tenant; cross-tenant isolation still rests on open_basedir + \
         disable_functions)"
    );
    Ok(())
}

/// Fail closed on a dangerous privilege posture (multi-tenant breakout audit
/// **F3**): a root process with no `[server] run_as_user` to drop to runs every
/// tenant's PHP — and anything that breaks out of the in-process engine — as
/// **host root**, because [`drop_privileges`] is a no-op when `run_as_user` is
/// unset.
///
/// Called once, early in `serve`, before any port is bound. The behaviour:
///
/// * `euid != 0`, or `run_as_user` is set: nothing to warn about, return `Ok`.
///   (Whether the drop then succeeds is [`drop_privileges`]'s concern.)
/// * `euid == 0` and `run_as_user` is unset: emit a loud `WARN` (mirroring the
///   exposed-DB-listener warnings), then **fail closed** with an explanatory
///   error when `ServerConfig::require_privilege_drop_enforced` resolves `true`
///   (the multi-tenant default). `dev_mode` downgrades the failure to a warning
///   so a developer iterating locally is never blocked.
///
/// # Errors
///
/// Returns an error when the process is root, no `run_as_user` is configured,
/// `require_privilege_drop` resolves to `true`, and `dev_mode` is `false`.
#[cfg(unix)]
pub fn enforce_posture(config: &Config, dev_mode: bool) -> anyhow::Result<()> {
    // SAFETY: geteuid never fails and takes no arguments.
    let is_root = unsafe { libc::geteuid() } == 0;
    let inputs = PostureInputs {
        is_root,
        run_as_user_set: config.server.run_as_user.is_some(),
        require_drop: config.server.require_privilege_drop_enforced(),
        dev_mode,
    };
    apply_posture_decision(posture_decision(inputs), config.server.sites_dir.is_some())
}

/// The posture inputs [`posture_decision`] reasons over. A struct rather than a
/// row of `bool` parameters (both for readability and to keep the decision
/// exhaustively testable without tripping `clippy::fn_params_excessive_bools`).
#[cfg(unix)]
#[derive(Debug, Clone, Copy)]
struct PostureInputs {
    /// `euid == 0`.
    is_root: bool,
    /// `[server] run_as_user` is set (a drop target is configured).
    run_as_user_set: bool,
    /// `require_privilege_drop` resolved to `true` (the multi-tenant default).
    require_drop: bool,
    /// Running under `ephpm dev` (never fatal).
    dev_mode: bool,
}

/// What [`enforce_posture`] should do about the current privilege posture.
///
/// Pure (no syscalls, no logging) so the F3 fail-fast logic is exhaustively
/// unit-testable without needing to actually be root.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PostureDecision {
    /// Posture is fine (not root, or a drop is configured) — stay quiet.
    Ok,
    /// Root with no drop, but not fatal — warn and continue.
    WarnOnly,
    /// Root with no drop in `ephpm dev` while enforcement is on — warn that it
    /// would be fatal under `ephpm serve`, then continue.
    WarnDevExempt,
    /// Root with no drop and enforcement is on — fail closed.
    Fail,
}

/// Decide the posture outcome. See [`PostureDecision`].
#[cfg(unix)]
fn posture_decision(inputs: PostureInputs) -> PostureDecision {
    if !inputs.is_root || inputs.run_as_user_set {
        return PostureDecision::Ok;
    }
    if !inputs.require_drop {
        return PostureDecision::WarnOnly;
    }
    if inputs.dev_mode { PostureDecision::WarnDevExempt } else { PostureDecision::Fail }
}

/// Carry out a [`PostureDecision`]: emit the matching log line and, for
/// [`PostureDecision::Fail`], return the fail-closed error.
#[cfg(unix)]
fn apply_posture_decision(decision: PostureDecision, multi_tenant: bool) -> anyhow::Result<()> {
    if decision == PostureDecision::Ok {
        return Ok(());
    }

    let detail = if multi_tenant {
        "This is a MULTI-TENANT deployment: every tenant's PHP — and anything \
         that breaks out of the in-process engine — executes as host root."
    } else {
        "Every PHP request — and anything that breaks out of the in-process \
         engine — executes as host root."
    };
    tracing::warn!(
        "ePHPm is running as root (euid=0) and [server] run_as_user is not set, so \
         it will NOT drop privileges. {detail} Set [server] run_as_user to an \
         unprivileged account so the process drops to it after binding privileged \
         ports (see the multi-tenant hardening guide)."
    );

    match decision {
        PostureDecision::Ok => unreachable!("handled above"),
        PostureDecision::WarnOnly => Ok(()),
        PostureDecision::WarnDevExempt => {
            tracing::warn!(
                "[server] require_privilege_drop is in effect but `ephpm dev` does not \
                 enforce it — continuing as root for local iteration only. This posture \
                 would FAIL startup under `ephpm serve`."
            );
            Ok(())
        }
        PostureDecision::Fail => anyhow::bail!(
            "refusing to serve as root without a privilege drop (multi-tenant breakout \
             audit finding F3). Set [server] run_as_user to an unprivileged account, or \
             set [server] require_privilege_drop = false to acknowledge running as root \
             with no drop (only safe when an external sandbox already confines the \
             process)."
        ),
    }
}

/// Non-Unix stub for [`enforce_posture`]: there is no uid to drop, so the
/// dangerous posture does not exist. Always `Ok`.
///
/// # Errors
///
/// Never fails on non-Unix.
#[cfg(not(unix))]
pub fn enforce_posture(config: &Config, _dev_mode: bool) -> anyhow::Result<()> {
    let _ = config;
    Ok(())
}

/// The irreversible credential drop itself: `setgroups` → `setgid` → `setuid`,
/// then a fail-closed verification that the drop took and cannot be undone.
///
/// Factored out of [`drop_privileges`] so the one-shot sandboxed exec path
/// (`ephpm exec`) and the long-running server share a single audited
/// implementation of the sequence. This function performs **only** the
/// credential syscalls — it does not `chown` anything, resolve names, or log a
/// summary; the caller owns that context. The process must be `euid == 0` when
/// this is called (checked by [`drop_privileges`]; the exec path checks it too).
///
/// # Errors
///
/// Returns an error if any `set*id` syscall fails, if the post-drop identity is
/// not exactly `(uid, gid)`, or if root can still be regained afterwards
/// (`seteuid(0)` succeeds) — a reversible or partial drop must never be mistaken
/// for a real one.
#[cfg(unix)]
// The real/effective uid/gid names are intentionally parallel here.
#[allow(clippy::similar_names)]
pub fn drop_to_user(uid: u32, gid: u32) -> anyhow::Result<()> {
    // Order is load-bearing: drop supplementary groups and the gid while still
    // uid 0, then the uid last. Doing setuid first would forfeit the privilege
    // needed for setgroups/setgid.
    //
    // SAFETY: setgroups/setgid/setuid are the documented POSIX credential
    // syscalls. We pass a valid pointer/len for the one-element group list and
    // check every return value below. glibc applies these process-wide across
    // all threads.
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
    Ok(())
}

/// Windows / non-Unix stub: privilege drop is not supported.
///
/// # Errors
///
/// Never fails on non-Unix — the `run_as_*` knobs are ignored with a WARN.
/// (The `Result` shape matches the Unix implementation.)
#[cfg(not(unix))]
pub fn drop_privileges(config: &Config, _extra_paths: &[std::path::PathBuf]) -> anyhow::Result<()> {
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
///
/// Public so the `ephpm exec` sandbox can resolve its target tenant user
/// through the same audited lookup the server drop uses (e.g. `ephpm-web` →
/// `(997, Some(989))`).
///
/// # Errors
///
/// Returns an error if `spec` is a name containing an interior NUL byte or if
/// no such user exists (`getpwnam` returned NULL).
#[cfg(unix)]
pub fn resolve_user(spec: &str) -> anyhow::Result<(u32, Option<u32>)> {
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
///
/// Public for the same reason as [`resolve_user`] — the `ephpm exec` sandbox
/// resolves an explicit tenant group through it.
///
/// # Errors
///
/// Returns an error if `spec` is a name containing an interior NUL byte or if
/// no such group exists (`getgrnam` returned NULL).
#[cfg(unix)]
pub fn resolve_group(spec: &str) -> anyhow::Result<u32> {
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
        drop_privileges(&config, &[]).expect("no-op drop must succeed");
    }

    // --- F3 posture fail-fast logic (pure, independent of the real euid) ---

    use super::{PostureDecision, PostureInputs, posture_decision};

    // A terse positional constructor for the posture-decision table tests; the
    // bool row is the point of the exhaustive matrix, so the lint is noise here.
    #[allow(clippy::fn_params_excessive_bools)]
    fn inputs(
        is_root: bool,
        run_as_user_set: bool,
        require_drop: bool,
        dev_mode: bool,
    ) -> PostureInputs {
        PostureInputs { is_root, run_as_user_set, require_drop, dev_mode }
    }

    #[test]
    fn posture_ok_when_not_root() {
        // Not root: nothing to enforce, whatever the other inputs are.
        for &run_as in &[false, true] {
            for &req in &[false, true] {
                for &dev in &[false, true] {
                    assert_eq!(
                        posture_decision(inputs(false, run_as, req, dev)),
                        PostureDecision::Ok,
                    );
                }
            }
        }
    }

    #[test]
    fn posture_ok_when_run_as_user_is_set() {
        // Root but a drop target is configured: drop_privileges will do the work.
        assert_eq!(posture_decision(inputs(true, true, true, false)), PostureDecision::Ok);
    }

    #[test]
    fn posture_fails_closed_for_root_multi_tenant_default() {
        // The dangerous posture: root, no run_as_user, enforcement on (the
        // multi-tenant default), not dev. Must be fatal.
        assert_eq!(posture_decision(inputs(true, false, true, false)), PostureDecision::Fail);
    }

    #[test]
    fn posture_warns_only_when_enforcement_disabled() {
        // Root, no run_as_user, but the operator set require_privilege_drop =
        // false (or single-tenant default): warn, do not fail.
        assert_eq!(posture_decision(inputs(true, false, false, false)), PostureDecision::WarnOnly);
    }

    #[test]
    fn posture_dev_mode_never_fails() {
        // `ephpm dev` as root with enforcement on: downgraded to a warning.
        assert_eq!(
            posture_decision(inputs(true, false, true, true)),
            PostureDecision::WarnDevExempt
        );
    }

    #[test]
    fn enforce_posture_default_config_is_ok_in_tests() {
        // The default config sets no run_as_user; the test harness is not root,
        // so enforce_posture must return Ok regardless of dev_mode.
        let config = Config::default_config().expect("default config");
        enforce_posture(&config, false).expect("non-root posture must be Ok");
        enforce_posture(&config, true).expect("non-root posture must be Ok in dev too");
    }
}
