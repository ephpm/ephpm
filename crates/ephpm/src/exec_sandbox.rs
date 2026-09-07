//! `ephpm exec` — run a command inside a virtual host's tenant sandbox.
//!
//! This is the sandboxed execution primitive described in
//! `site/content/roadmap/ephpm-exec-sandboxed-vhost.md`. It reconstructs a
//! tenant's on-disk boundary (the same derivation a request goes through, via
//! [`ephpm_server::router::resolve_sandbox_site`]), then, in order:
//!
//! 1. resolves `--site <key>` → the vhost's container + private state root;
//! 2. sets `PR_SET_NO_NEW_PRIVS` so a setuid binary in the command cannot
//!    re-escalate;
//! 3. applies a **Landlock** ruleset scoping filesystem access to the
//!    container + state root (+ the minimum read/exec set a program needs to
//!    run) and *nothing else* — `/etc/ephpm`, `/root`, `/etc/shadow` are
//!    unreachable;
//! 4. irreversibly drops to the tenant uid/gid (reusing the server's audited
//!    [`ephpm_server::privdrop::drop_to_user`] sequence), which is what arms
//!    the host's uid-keyed `ephpm_egress` firewall; then
//! 5. `execvp`s the command.
//!
//! Everything from step 2 on is `cfg(target_os = "linux")`; on any other
//! platform the subcommand refuses (the sandbox mechanisms are Linux-only). The
//! module carries **no PHP linkage**, so it compiles and is testable in stub
//! mode with no PHP SDK.
//!
//! # TODO — deliberately out of scope for this proof-of-concept
//!
//! - `TODO(#471)`: the per-site DB session bind (`db_bridge::set_resolver` +
//!   `set_current_site`) so `ephpm exec --site … -- wp …` can reach the
//!   tenant's Turso database. The containment property this PoC proves does not
//!   depend on it.
//! - `TODO`: per-site-clustered owner-refusal (`hrw_owner`) — an exec on a
//!   non-owner must refuse rather than write to a replica whose writes never
//!   replicate. Not needed to prove uid/Landlock containment.
//! - `TODO`: `setrlimit` (`RLIMIT_AS`/`RLIMIT_CPU`) resource caps and a
//!   transient cgroup. The wall-clock `--timeout` is implemented via `alarm(2)`;
//!   memory/CPU ceilings are follow-up.
//! - `TODO`: the `--no-sandbox` DB-bind-only path for Windows/dev. On Linux
//!   `--no-sandbox` currently just runs the command uncontained (loudly warned);
//!   on non-Linux the whole subcommand refuses.
//!
//! # Intended switchboard call site (follow-up, not wired here)
//!
//! Switchboard's deployer runs manifest `build:` / `seed:` steps as root today.
//! The one-line change is to run each step as, instead of `bash -c "$step"`:
//!
//! ```text
//! ephpm exec --config /etc/ephpm/ephpm.toml --site <key> -- bash -c "$step"
//! ```
//!
//! which inherits the uid drop, Landlock scope, and egress lock. Rewriting
//! switchboard's deployer is a separate change in that repo.

// The sandbox is a sequence of libc credential/prctl/exec syscalls and Landlock
// FFI; every unsafe block below carries a SAFETY note explaining the invariant
// it upholds.
#![allow(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

/// The tenant user `ephpm exec` drops to when `[server] run_as_user` is unset.
///
/// On the preview nodes this resolves to uid 997, which is exactly the uid the
/// host `inet ephpm_egress` nftables table filters (`meta skuid 997`), so the
/// drop arms the existing egress jail with no extra configuration.
#[cfg(target_os = "linux")]
const DEFAULT_TENANT_USER: &str = "ephpm-web";

/// Entry point for `ephpm exec`. See the module docs for the layer sequence.
///
/// # Errors
///
/// Returns an error if the config cannot be loaded, the site cannot be
/// resolved, the sandbox cannot be applied, the privilege drop fails, or the
/// command cannot be `exec`ed. On success on Linux this never returns — the
/// process image is replaced by the command.
#[cfg(target_os = "linux")]
pub fn run(
    config_path: &PathBuf,
    site_key: &str,
    command: &[String],
    timeout: u64,
    no_sandbox: bool,
) -> anyhow::Result<ExitCode> {
    use anyhow::Context as _;
    use ephpm_server::{privdrop, router};

    let program = command.first().context("no command given after `--`")?;

    let config = ephpm_config::Config::load(config_path).context("failed to load configuration")?;

    let site = router::resolve_sandbox_site(&config, site_key)
        .context("failed to resolve --site to a tenant sandbox")?;

    // Resolve the tenant uid/gid the same way the server drop does: an explicit
    // [server] run_as_user / run_as_group wins, else the ephpm-web default.
    let user_spec = config.server.run_as_user.as_deref().unwrap_or(DEFAULT_TENANT_USER);
    let (uid, primary_gid) = privdrop::resolve_user(user_spec)
        .with_context(|| format!("failed to resolve tenant user {user_spec:?}"))?;
    let gid = match config.server.run_as_group.as_deref() {
        Some(group_spec) => privdrop::resolve_group(group_spec)
            .with_context(|| format!("failed to resolve tenant group {group_spec:?}"))?,
        None => primary_gid.unwrap_or(uid),
    };

    // SAFETY: geteuid never fails and takes no arguments.
    let euid = unsafe { libc::geteuid() };

    tracing::info!(
        site = %site.key,
        uid,
        gid,
        timeout,
        no_sandbox,
        container = %site.container.display(),
        document_root = %site.document_root.display(),
        command = ?command,
        "ephpm exec: entering tenant sandbox"
    );

    if no_sandbox {
        tracing::warn!(
            "ephpm exec --no-sandbox: NO Landlock scope and NO uid drop — the \
             command runs with the caller's full privileges. This defeats every \
             containment layer; never use it for untrusted commands."
        );
    } else {
        if euid != 0 {
            anyhow::bail!(
                "ephpm exec must start as root to enter a tenant sandbox (it needs \
                 to setuid to {uid} and prepare the private state root); current \
                 euid={euid}. Re-run under sudo, or pass --no-sandbox to run \
                 uncontained."
            );
        }
        // Create + chown the tenant's private temp/session root while still root,
        // so the dropped command can write there. Mirrors the router's
        // `ensure_vhost_private_dirs`.
        prepare_state_root(&site, uid, gid)?;
        // Block execve-based privilege regain (setuid binaries, file caps) in the
        // command we are about to run. Must precede Landlock and the uid drop.
        set_no_new_privs()?;
        // Kernel filesystem boundary — the only fs scope for a non-PHP command.
        apply_landlock(&site).context("failed to apply the Landlock sandbox")?;
    }

    // Wall-clock deadline. alarm(2) survives execve and the default SIGALRM
    // disposition terminates the process, so this fires even inside the command.
    if timeout > 0 {
        let secs = u32::try_from(timeout).unwrap_or(u32::MAX);
        // SAFETY: alarm takes a scalar, has no failure mode, and only schedules
        // a signal; it cancels any previous alarm (there is none here).
        unsafe {
            libc::alarm(secs);
        }
    }

    // Irreversible drop to the tenant identity, reusing the server's audited
    // sequence (setgroups → setgid → setuid + fail-closed verification).
    if !no_sandbox {
        privdrop::drop_to_user(uid, gid).context("failed to drop to the tenant uid/gid")?;
    }

    // Run inside the web root so relative paths resolve within the sandbox.
    if let Err(e) = std::env::set_current_dir(&site.document_root) {
        tracing::warn!(
            path = %site.document_root.display(),
            error = %e,
            "could not chdir into the document root; continuing from the current directory"
        );
    }

    exec(program, command)
}

/// Create and chown the tenant's private state root (`state_root`, `tmp`,
/// `sessions`) so the dropped command can write session/temp files there.
///
/// Best-effort ownership: the directories are created while still root, tightened
/// to `0700`, and `chown`ed to the tenant. Mirrors the router's
/// `ensure_vhost_private_dirs` so an exec'd tenant CLI shares the same private
/// temp/session tree its HTTP requests use.
#[cfg(target_os = "linux")]
fn prepare_state_root(
    site: &ephpm_server::router::SandboxSite,
    uid: u32,
    gid: u32,
) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    use anyhow::Context as _;

    let tmp = site.state_root.join("tmp");
    let sessions = site.state_root.join("sessions");
    for dir in [&site.state_root, &tmp, &sessions] {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    // 0700 on the root so only the tenant uid may traverse it.
    let _ = std::fs::set_permissions(&site.state_root, std::fs::Permissions::from_mode(0o700));
    for dir in [&site.state_root, &tmp, &sessions] {
        chown(dir, uid, gid).with_context(|| format!("failed to chown {}", dir.display()))?;
    }
    Ok(())
}

/// `chown(2)` a path to `uid`/`gid`.
#[cfg(target_os = "linux")]
fn chown(path: &std::path::Path, uid: u32, gid: u32) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;

    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: `c` is a valid NUL-terminated C string that outlives the call; the
    // return value is checked immediately.
    if unsafe { libc::chown(c.as_ptr(), uid as libc::uid_t, gid as libc::gid_t) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Set `PR_SET_NO_NEW_PRIVS` so no descendant `execve` can gain privileges via
/// a setuid/setgid bit or file capabilities. Required for a defence-in-depth
/// sandbox and a precondition for Landlock without `CAP_SYS_ADMIN`.
#[cfg(target_os = "linux")]
fn set_no_new_privs() -> anyhow::Result<()> {
    use anyhow::Context as _;

    // SAFETY: prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) is the documented form with
    // scalar arguments and no pointer operands; the return value is checked.
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("prctl(PR_SET_NO_NEW_PRIVS) failed");
    }
    Ok(())
}

/// Apply the Landlock filesystem ruleset for this tenant.
///
/// Grants read/write beneath the site container and its private state root,
/// read+execute beneath the standard system directories a program needs to run,
/// and read on a minimal, non-secret slice of `/etc` (name resolution + the
/// dynamic loader cache) plus entropy sources. Everything else — crucially
/// `/etc/ephpm` (the cluster secret), `/root`, and `/etc/shadow` — is denied by
/// Landlock's default-deny once `restrict_self` runs.
///
/// Uses ABI v1 (the widest kernel support) in best-effort mode, and refuses to
/// run if the kernel does not enforce Landlock at all — a fail-closed posture,
/// overridable only with the loudly-warned `--no-sandbox`.
#[cfg(target_os = "linux")]
fn apply_landlock(site: &ephpm_server::router::SandboxSite) -> anyhow::Result<()> {
    use std::path::{Path, PathBuf};

    use anyhow::Context as _;
    use landlock::{
        ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus, path_beneath_rules,
    };

    let abi = ABI::V1;
    let read_exec = AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir;
    let read_only = AccessFs::from_read(abi);
    let read_write = AccessFs::from_all(abi);

    // Read/write: the tenant's own code container and its private temp/session
    // root. These are the two entries the request-path `open_basedir` grants.
    let rw: Vec<PathBuf> = existing(&[
        site.container.as_path(),
        site.state_root.as_path(),
        Path::new("/dev/null"),
        Path::new("/dev/zero"),
        Path::new("/dev/full"),
    ]);

    // Read/execute: where interpreters, the loader, and shared libraries live.
    let rx: Vec<PathBuf> = existing(&[
        Path::new("/usr"),
        Path::new("/lib"),
        Path::new("/lib64"),
        Path::new("/bin"),
        Path::new("/sbin"),
        Path::new("/opt"),
    ]);

    // Read-only: the minimum non-secret /etc a program needs (name resolution,
    // the loader cache, timezone) plus entropy and /proc for self-inspection.
    // Deliberately NOT /etc wholesale — that would expose /etc/ephpm and
    // /etc/shadow. Each is an individual path, so only these leaves are granted.
    let ro: Vec<PathBuf> = existing(&[
        Path::new("/etc/passwd"),
        Path::new("/etc/group"),
        Path::new("/etc/nsswitch.conf"),
        Path::new("/etc/ld.so.cache"),
        Path::new("/etc/ld.so.preload"),
        Path::new("/etc/localtime"),
        Path::new("/etc/resolv.conf"),
        Path::new("/etc/hosts"),
        Path::new("/dev/urandom"),
        Path::new("/dev/random"),
        Path::new("/proc"),
    ]);

    let status = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))
        .context("landlock: handle_access")?
        .create()
        .context("landlock: create ruleset")?
        .add_rules(path_beneath_rules(&rw, read_write))
        .context("landlock: add read/write rules")?
        .add_rules(path_beneath_rules(&rx, read_exec))
        .context("landlock: add read/exec rules")?
        .add_rules(path_beneath_rules(&ro, read_only))
        .context("landlock: add read-only rules")?
        .restrict_self()
        .context("landlock: restrict_self")?;

    match status.ruleset {
        RulesetStatus::FullyEnforced => {
            tracing::info!(
                container = %site.container.display(),
                state_root = %site.state_root.display(),
                "Landlock: fully enforced (filesystem scoped to the tenant sandbox)"
            );
        }
        RulesetStatus::PartiallyEnforced => {
            tracing::warn!(
                "Landlock: partially enforced — this kernel supports only a subset \
                 of the requested access rights. The sandbox is active but weaker \
                 than intended."
            );
        }
        RulesetStatus::NotEnforced => {
            anyhow::bail!(
                "Landlock is not enforced by this kernel — refusing to run without a \
                 filesystem sandbox. Pass --no-sandbox to override (removes all \
                 containment)."
            );
        }
    }
    Ok(())
}

/// Filter a list of candidate paths down to those that currently exist, as
/// owned `PathBuf`s. Landlock rule creation opens each path, so a non-existent
/// entry (e.g. `/etc/ld.so.preload` on a system without one) would otherwise
/// error; skipping it keeps the ruleset fail-closed on the paths that matter.
#[cfg(target_os = "linux")]
fn existing(paths: &[&std::path::Path]) -> Vec<std::path::PathBuf> {
    paths.iter().filter(|p| p.exists()).map(|p| p.to_path_buf()).collect()
}

/// `execvp` the command, replacing this process image. Returns only on failure.
#[cfg(target_os = "linux")]
fn exec(program: &str, command: &[String]) -> anyhow::Result<ExitCode> {
    use std::ffi::CString;

    use anyhow::Context as _;

    let prog_c =
        CString::new(program.as_bytes()).context("command name contains an interior NUL byte")?;
    let mut argv_owned: Vec<CString> = Vec::with_capacity(command.len());
    for arg in command {
        argv_owned.push(
            CString::new(arg.as_bytes())
                .context("command argument contains an interior NUL byte")?,
        );
    }
    let mut argv: Vec<*const libc::c_char> = argv_owned.iter().map(|c| c.as_ptr()).collect();
    argv.push(std::ptr::null());

    // SAFETY: `prog_c` and every `CString` in `argv_owned` outlive the call, and
    // `argv` is NULL-terminated. On success execvp replaces the process image
    // and never returns; on failure it returns -1 and we read errno immediately.
    unsafe {
        libc::execvp(prog_c.as_ptr(), argv.as_ptr());
    }
    let err = std::io::Error::last_os_error();
    Err(err).with_context(|| format!("failed to exec {program:?}"))
}

/// Non-Linux stub: sandboxed exec is unavailable, so the subcommand refuses.
///
/// The sandbox layers (Landlock, `setuid`, the host egress firewall) are all
/// Linux mechanisms; pretending to isolate when we cannot would be worse than
/// refusing. The `--no-sandbox` DB-bind-only path a design doc sketches for
/// Windows/dev is not implemented in this proof-of-concept.
///
/// # Errors
///
/// Always returns an error on non-Linux platforms.
#[cfg(not(target_os = "linux"))]
pub fn run(
    config_path: &PathBuf,
    site_key: &str,
    command: &[String],
    timeout: u64,
    no_sandbox: bool,
) -> anyhow::Result<ExitCode> {
    let _ = (config_path, site_key, command, timeout, no_sandbox);
    anyhow::bail!(
        "`ephpm exec` sandboxed execution is Linux-only — it relies on Landlock, \
         setuid, and the host uid-keyed egress firewall, none of which exist on \
         this platform. (The --no-sandbox DB-bind-only path is not implemented in \
         this build.)"
    )
}
