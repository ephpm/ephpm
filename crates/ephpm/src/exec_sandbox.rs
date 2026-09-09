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
//! # Composing with `ephpm php --site` for database access (issue #471)
//!
//! `ephpm exec` deliberately does **not** host the `ephpm_db_*` bridge itself —
//! it `execvp`s a child, so any bridge state set up here would not survive into
//! that process. Instead the per-site DB binding lives in the child: the
//! sandboxed way to run a database-touching PHP tool on Linux is to nest the
//! two subcommands —
//!
//! ```text
//! ephpm exec --site blog -- ephpm php --site blog --config /etc/ephpm/ephpm.toml -- wp db query "..."
//! ```
//!
//! The outer `exec` establishes the Landlock + uid/gid containment; the inner
//! `php --site` binds the bridge (via [`crate::site_db`]) — to the running
//! server over the wire when one is up, or by opening the site's file directly
//! for offline work — and picks the correct strategy for per-site *clustered*
//! mode (refusing when it cannot reach the owner). See [`crate::site_db`] for
//! the strategy picker; there is nothing further to wire in this module.
//!
//! # TODO — deliberately out of scope for this proof-of-concept
//!
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
/// This runs **as root, before the privilege drop**, on a tree whose base
/// (`$TMPDIR/ephpm-vhosts`) is owned by the untrusted tenant uid this feature
/// exists to contain. Every path component below the trusted temp anchor is
/// therefore attacker-controlled, so the function never trusts a path *string*
/// below the anchor: it walks the tree one component at a time with
/// `openat(O_NOFOLLOW | O_DIRECTORY)`, refusing (`bail!`) any component that is a
/// symlink, and then tightens (`fchmod`) and hands over (`fchown`) each directory
/// **through the open file descriptor** — never a path — so a name swapped
/// mid-operation (TOCTOU) can never redirect the chmod/chown onto a target
/// outside the tree.
///
/// This closes the symlink-follow privilege escalation that a naïve
/// `chown`/`set_permissions`/`create_dir_all` on `state_root/tmp` (planted by the
/// tenant as e.g. `state_root/tmp -> /root`) would otherwise turn into local
/// root. It mirrors the intent of the sibling `privdrop::chown_tree`, which
/// already uses `lchown` for the same reason, but goes further with `openat`
/// traversal so even an active mid-operation swap is defeated rather than merely
/// the static-symlink case.
///
/// # Residual TOCTOU
///
/// The tenant uid we hand ownership to is the **same** principal that owns the
/// enclosing `ephpm-vhosts` base, so the only thing an attacker can substitute
/// for one of our freshly-`mkdir`ed components is (a) a symlink — refused by
/// `O_NOFOLLOW` — or (b) another directory *they already own*, which `fchown`ing
/// to that same uid grants them nothing they did not already have. There is no
/// cross-principal target reachable, which is exactly the escalation the HIGH
/// finding described.
#[cfg(target_os = "linux")]
fn prepare_state_root(
    site: &ephpm_server::router::SandboxSite,
    uid: u32,
    gid: u32,
) -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::Component;

    use anyhow::Context as _;

    // The trusted anchor: the system temp dir (root-owned, sticky on a stock
    // host). We allow the anchor itself to resolve through a symlink (it is
    // system config, not tenant-writable), but everything below it is opened
    // O_NOFOLLOW.
    let anchor = std::env::temp_dir();
    let rel = site.state_root.strip_prefix(&anchor).with_context(|| {
        format!(
            "tenant state root {} is not under the trusted temp anchor {} — refusing \
             to prepare it",
            site.state_root.display(),
            anchor.display()
        )
    })?;

    let anchor_fd = open_anchor_dir(&anchor)
        .with_context(|| format!("failed to open temp anchor {}", anchor.display()))?;

    // Walk each component below the anchor, creating missing ones, refusing any
    // symlink. `current` ends pointing at the state root itself.
    let mut current = anchor_fd;
    for comp in rel.components() {
        let Component::Normal(name) = comp else {
            anyhow::bail!(
                "refusing tenant state root {}: unexpected path component {comp:?} \
                 (only plain names are allowed below the temp anchor)",
                site.state_root.display()
            );
        };
        current =
            open_or_create_dir_nofollow(&current, name.as_bytes(), 0o700).with_context(|| {
                format!(
                    "failed to safely open/create state-root component {:?}",
                    name.to_string_lossy()
                )
            })?;
    }

    // Create the two children under the (now fd-pinned) state root.
    let tmp_fd = open_or_create_dir_nofollow(&current, b"tmp", 0o700)
        .context("failed to safely open/create the tenant tmp directory")?;
    let sessions_fd = open_or_create_dir_nofollow(&current, b"sessions", 0o700)
        .context("failed to safely open/create the tenant sessions directory")?;

    // Hand ownership over via the open fds — never a path — so no symlink or
    // renamed target can be substituted. Children first, then the root.
    fchown_dir(&tmp_fd, uid, gid).context("failed to chown the tenant tmp directory")?;
    fchown_dir(&sessions_fd, uid, gid).context("failed to chown the tenant sessions directory")?;
    // 0700 on the root so only the tenant uid may traverse it.
    fchmod_dir(&current, 0o700).context("failed to tighten the tenant state root to 0700")?;
    fchown_dir(&current, uid, gid).context("failed to chown the tenant state root")?;
    Ok(())
}

/// Open the trusted temp anchor as a directory fd.
///
/// The anchor (e.g. `/tmp`) is system configuration rather than tenant-writable,
/// so this deliberately *follows* a symlinked anchor (some distributions symlink
/// `/tmp`) but requires the final target to be a directory (`O_DIRECTORY`).
/// Everything traversed *below* this fd is opened `O_NOFOLLOW`.
#[cfg(target_os = "linux")]
fn open_anchor_dir(path: &std::path::Path) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)?;
    Ok(file.into())
}

/// Open `name` beneath `parent` as a directory fd, creating it if absent, and
/// **refusing any symlink** (`O_NOFOLLOW`).
///
/// Returns an [`OwnedFd`](std::os::fd::OwnedFd) pinned to the opened inode, so
/// callers can `fchown`/`fchmod` it without a path and hence without a
/// symlink/rename race. Because the parent directory can be tenant-writable, a
/// bounded retry tolerates a concurrent `mkdir`/`rmdir` race but caps the loop so
/// an active attacker forces a loud failure rather than a spin.
#[cfg(target_os = "linux")]
fn open_or_create_dir_nofollow(
    parent: &std::os::fd::OwnedFd,
    name: &[u8],
    mode: libc::mode_t,
) -> anyhow::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

    use anyhow::Context as _;

    let cname = std::ffi::CString::new(name)
        .map_err(|_| anyhow::anyhow!("path component contains an interior NUL byte"))?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let shown = String::from_utf8_lossy(name).into_owned();

    for _ in 0..16 {
        // SAFETY: `parent` is a live directory fd for the whole call, `cname` is a
        // valid NUL-terminated string that outlives it, and `flags` includes
        // O_NOFOLLOW so the final component is never dereferenced as a symlink.
        // A non-negative return is a freshly-owned descriptor wrapped in OwnedFd
        // immediately (closed on drop); the error path reads errno right away.
        let fd = unsafe { libc::openat(parent.as_raw_fd(), cname.as_ptr(), flags, 0) };
        if fd >= 0 {
            // SAFETY: `fd` is a fresh, exclusively-owned descriptor (>= 0)
            // returned by openat; wrapping it transfers ownership so it is closed
            // exactly once on drop.
            return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::ENOENT) => {
                // SAFETY: same argument validity as the openat above; mkdirat
                // takes a checked path pointer and a scalar mode, return checked.
                let rc = unsafe { libc::mkdirat(parent.as_raw_fd(), cname.as_ptr(), mode) };
                if rc != 0 {
                    let mkerr = std::io::Error::last_os_error();
                    if mkerr.raw_os_error() != Some(libc::EEXIST) {
                        return Err(mkerr).with_context(|| {
                            format!("mkdirat failed while preparing state-root component {shown:?}")
                        });
                    }
                    // EEXIST: lost the create race; loop to open it (still
                    // O_NOFOLLOW, so a symlink planted in the gap is refused).
                }
                // Loop to open the (now-existing) directory O_NOFOLLOW.
            }
            // O_NOFOLLOW on a symlink surfaces as ELOOP; O_DIRECTORY on a
            // symlink-to-directory surfaces as ENOTDIR on Linux. Either way the
            // link was NOT followed and no fd was opened, so the target is never
            // touched — we just probe (lstat) to report the precise reason.
            Some(libc::ELOOP) => anyhow::bail!(
                "refusing to operate on state-root component {shown:?}: it is a \
                 symlink (symlink attack on the tenant-owned state-root tree)"
            ),
            Some(libc::ENOTDIR) => {
                if symlink_present_at(parent, &cname) {
                    anyhow::bail!(
                        "refusing to operate on state-root component {shown:?}: it \
                         is a symlink (symlink attack on the tenant-owned \
                         state-root tree)"
                    );
                }
                anyhow::bail!(
                    "refusing to operate on state-root component {shown:?}: it \
                     exists but is not a directory"
                );
            }
            _ => {
                return Err(err)
                    .with_context(|| format!("openat failed for state-root component {shown:?}"));
            }
        }
    }
    anyhow::bail!(
        "gave up opening/creating state-root component {shown:?} after repeated \
         races — refusing (possible active symlink attack on the tenant-owned \
         state-root tree)"
    )
}

/// Return `true` if `name` beneath `parent` currently exists and is a symlink.
///
/// Used only to word the refusal precisely on the `openat` error path — the
/// actual security enforcement is `O_NOFOLLOW`, which already declined to follow
/// the link. `fstatat` with `AT_SYMLINK_NOFOLLOW` never dereferences it.
#[cfg(target_os = "linux")]
fn symlink_present_at(parent: &std::os::fd::OwnedFd, name: &std::ffi::CStr) -> bool {
    use std::os::fd::AsRawFd as _;

    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `parent` is a live directory fd for the call, `name` is a valid
    // NUL-terminated C string, `st` is a correctly-sized, writable `stat` buffer,
    // and AT_SYMLINK_NOFOLLOW means the link itself (not its target) is stat'd.
    let rc = unsafe {
        libc::fstatat(parent.as_raw_fd(), name.as_ptr(), st.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW)
    };
    if rc != 0 {
        return false;
    }
    // SAFETY: fstatat returned 0, so `st` is fully initialized.
    let st = unsafe { st.assume_init() };
    (st.st_mode & libc::S_IFMT) == libc::S_IFLNK
}

/// `fchown(2)` a directory through its open fd (never a path).
#[cfg(target_os = "linux")]
fn fchown_dir(fd: &std::os::fd::OwnedFd, uid: u32, gid: u32) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;

    // SAFETY: `fd` is a live directory descriptor for the duration of the call;
    // fchown operates on the already-opened inode (no path, no symlink race) and
    // the return value is checked.
    if unsafe { libc::fchown(fd.as_raw_fd(), uid as libc::uid_t, gid as libc::gid_t) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// `fchmod(2)` a directory through its open fd (never a path).
#[cfg(target_os = "linux")]
fn fchmod_dir(fd: &std::os::fd::OwnedFd, mode: libc::mode_t) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;

    // SAFETY: `fd` is a live directory descriptor for the duration of the call;
    // fchmod operates on the already-opened inode (no path, no symlink race) and
    // the return value is checked.
    if unsafe { libc::fchmod(fd.as_raw_fd(), mode) } != 0 {
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
/// read on a minimal, non-secret slice of `/etc` (name resolution + the dynamic
/// loader cache) plus entropy sources, and read on the **public** system CA
/// trust-store directories so an in-sandbox HTTPS client can verify certificates
/// (issue #485). Everything else — crucially `/etc/ephpm` (the cluster secret),
/// `/etc/ssl/private` (TLS private keys), `/root`, and `/etc/shadow` — is denied
/// by Landlock's default-deny once `restrict_self` runs.
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
    // the loader cache, timezone) plus entropy and /proc/self for self-inspection.
    // Deliberately NOT /etc wholesale — that would expose /etc/ephpm and
    // /etc/shadow. Each is an individual path, so only these leaves are granted.
    //
    // Deliberately `/proc/self`, NOT `/proc` wholesale: the exec'd command shares
    // the `ephpm-web` uid with the *running server*, so a broad `/proc` grant
    // would let it read the server's `/proc/<server-pid>/environ` and `maps`
    // (same-uid `0400`) and exfiltrate the server's environment (any `EPHPM_*`
    // secrets) and address layout. An interpreter/shell needs only its own
    // `/proc/self/*`; per-pid entries for *other* pids stay unreachable.
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
        Path::new("/proc/self"),
    ]);

    // Read-only: the system CA trust store, so an HTTPS client inside the sandbox
    // can verify server certificates (issue #485). Without this every outbound TLS
    // handshake fails with `curl: (77) error setting certificate file` — no
    // WordPress core download, `composer install`, wp.org fetch, or `git clone
    // https://…` can run. The tenant egress firewall deliberately *allows* public
    // TCP because build steps are expected to reach the internet, so an
    // HTTPS-incapable sandbox is not useful; the CA store is public, non-secret
    // data, so reading it is safe.
    //
    // These are the **public** cert directories only. It is deliberately NOT
    // `/etc/ssl/private` / `/etc/pki/tls/private` (TLS private keys) and NOT
    // `/etc/ssl`, `/etc/pki`, or `/etc` wholesale — widening to a parent would
    // re-expose `/etc/ephpm` (the cluster secret) and `/etc/shadow`, which the
    // paragraph above guarantees stay unreachable. Granted **read-only**: no write
    // right is added on any of them.
    //
    // Directories (not the bundle files) are granted so the distro's symlink
    // indirection resolves, because Landlock checks the *resolved* target path,
    // not the link:
    //   * Debian/Ubuntu/Alpine keep the concatenated bundle and the hashed `*.0`
    //     symlinks in `/etc/ssl/certs`; the per-cert symlinks point into
    //     `/usr/share/ca-certificates`, already covered by the `/usr` read+exec
    //     grant above.
    //   * RHEL/Alma/Fedora point `/etc/pki/tls/certs/ca-bundle.crt` (and
    //     `cert.pem`) at `/etc/pki/ca-trust/extracted/…`, so the real target tree
    //     `/etc/pki/ca-trust` is granted too — not merely the symlink entry dir
    //     `/etc/pki/tls/certs` — or the resolved read is still denied.
    // Non-present paths are dropped by `existing`, so the RHEL paths on a Debian
    // host (and vice versa) are silently skipped rather than erroring.
    let ca: Vec<PathBuf> = existing(&ca_trust_dirs());

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
        .add_rules(path_beneath_rules(&ca, read_only))
        .context("landlock: add CA trust-store read rules")?
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

/// The well-known system CA trust-store directories granted **read-only** so an
/// HTTPS client inside the sandbox can verify server certificates (issue #485).
///
/// This is a fixed, built-in list rather than a config knob: HTTPS working is a
/// baseline expectation of a build sandbox (composer, wp.org, `git clone
/// https://…`), not something an operator should have to configure, and a
/// misconfigured/empty override would silently break every outbound TLS
/// handshake. The paths are well-known, public, and read-only, so there is no
/// per-deployment variation to express.
///
/// Every entry is a **public**, non-secret directory. The list deliberately
/// excludes the private-key siblings (`/etc/ssl/private`, `/etc/pki/tls/private`)
/// and never returns `/etc/ssl`, `/etc/pki`, or `/etc` as a whole — granting a
/// parent would re-expose `/etc/ephpm` (the cluster secret) and `/etc/shadow`.
/// Both the Debian family (`/etc/ssl/certs`) and the RHEL family
/// (`/etc/pki/tls/certs` plus the `/etc/pki/ca-trust` tree the bundle symlinks
/// resolve into) are included so a general `ephpm exec` works on either; callers
/// pass the result through [`existing`], so a path absent on this host is skipped
/// rather than erroring.
#[cfg(target_os = "linux")]
fn ca_trust_dirs() -> [&'static std::path::Path; 3] {
    [
        // Debian / Ubuntu / Alpine: concatenated bundle + hashed `*.0` symlinks
        // (per-cert links resolve into /usr/share/ca-certificates, under /usr).
        std::path::Path::new("/etc/ssl/certs"),
        // RHEL / Alma / Fedora: the symlink entry dir (ca-bundle.crt, cert.pem).
        std::path::Path::new("/etc/pki/tls/certs"),
        // RHEL / Alma / Fedora: the real extracted-bundle tree those links point
        // at — Landlock checks the resolved target, so this must be granted too.
        std::path::Path::new("/etc/pki/ca-trust"),
    ]
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

/// Regression tests for the HIGH symlink-follow privilege escalation in
/// `prepare_state_root` (the root-privileged setup phase).
///
/// These reproduce the reviewed escape — a tenant-planted symlink where a
/// `state_root` component or `state_root/tmp` should be — and assert the fixed
/// code **refuses** it and never touches the link's target. They run rootless
/// (the refusal happens at `openat(O_NOFOLLOW)`, before any `fchown`), and the
/// `chmod` tell deterministically fails against the pre-fix `create_dir_all` +
/// `set_permissions` + `chown` implementation, which follows the symlink and
/// operates on the target.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::PathBuf;

    use ephpm_server::router::SandboxSite;

    /// A unique scratch base directly under the trusted temp anchor, so it is a
    /// valid `state_root` prefix for [`super::prepare_state_root`].
    fn unique_base(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let base =
            std::env::temp_dir().join(format!("ephpm-exec-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&base).expect("create scratch base");
        base
    }

    fn site_at(state_root: PathBuf) -> SandboxSite {
        SandboxSite {
            key: "symtest".to_string(),
            container: state_root.clone(),
            document_root: state_root.clone(),
            state_root,
            open_basedir: String::new(),
        }
    }

    fn current_ids() -> (u32, u32) {
        // SAFETY: getuid/getgid take no arguments and cannot fail.
        let uid = unsafe { libc::getuid() };
        // SAFETY: getuid/getgid take no arguments and cannot fail.
        let gid = unsafe { libc::getgid() };
        (uid, gid)
    }

    #[test]
    fn refuses_symlinked_state_root_component_and_leaves_target_untouched() {
        let base = unique_base("symcomp");
        let target = base.join("target");
        std::fs::create_dir_all(&target).expect("create target");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
            .expect("chmod target 0755");
        let state_root = base.join("site");
        std::os::unix::fs::symlink(&target, &state_root).expect("plant symlink");
        let (uid, gid) = current_ids();
        let result = super::prepare_state_root(&site_at(state_root), uid, gid);
        assert!(result.is_err(), "prepare_state_root must refuse a symlinked component");
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("symlink"), "error must name the symlink refusal, got: {msg}");
        let mode = std::fs::metadata(&target).expect("stat target").permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "symlink target's mode must be untouched");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn refuses_symlinked_tmp_child_and_leaves_target_untouched() {
        let base = unique_base("symtmp");
        let target = base.join("escalate-here");
        std::fs::create_dir_all(&target).expect("create target");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
            .expect("chmod target 0755");
        let state_root = base.join("site");
        std::fs::create_dir_all(&state_root).expect("create real state_root");
        std::os::unix::fs::symlink(&target, state_root.join("tmp")).expect("plant tmp symlink");
        let (uid, gid) = current_ids();
        let result = super::prepare_state_root(&site_at(state_root), uid, gid);
        assert!(result.is_err(), "prepare_state_root must refuse a symlinked tmp child");
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("symlink"), "error must name the symlink refusal, got: {msg}");
        let mode = std::fs::metadata(&target).expect("stat target").permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "symlink target's mode must be untouched");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The CA trust-store allowlist (issue #485) must grant only **public** cert
    /// directories and must never widen the read scope to a private-key sibling
    /// or to a parent that would re-expose the cluster secret (`/etc/ephpm`) or
    /// `/etc/shadow`. This guards the security bound of the fix independently of
    /// whether any given path happens to exist on the test host.
    #[test]
    fn ca_trust_dirs_are_public_cert_dirs_only() {
        use std::path::Path;

        let dirs = super::ca_trust_dirs();

        // The two distro-family public trust stores are present.
        assert!(
            dirs.contains(&Path::new("/etc/ssl/certs")),
            "Debian/Alpine trust store must be granted"
        );
        assert!(
            dirs.contains(&Path::new("/etc/pki/ca-trust")),
            "RHEL extracted-bundle target tree must be granted"
        );

        for p in dirs {
            let s = p.to_str().expect("ascii path");
            // Never a TLS private-key directory.
            assert!(!s.contains("private"), "must never grant a private-key dir: {s}");
            // Never the cluster secret.
            assert!(
                !Path::new(s).starts_with("/etc/ephpm"),
                "must never grant the cluster-secret dir: {s}"
            );
            // Never a bare parent that would pull in private-key / secret siblings.
            assert!(
                !matches!(s, "/etc" | "/etc/ssl" | "/etc/pki"),
                "must not widen to a parent of the cert dirs: {s}"
            );
        }
    }

    #[test]
    fn creates_clean_state_root_tree() {
        let base = unique_base("clean");
        let state_root = base.join("site");
        let (uid, gid) = current_ids();
        super::prepare_state_root(&site_at(state_root.clone()), uid, gid)
            .expect("clean prepare_state_root should succeed");
        assert!(state_root.is_dir(), "state root created");
        assert!(state_root.join("tmp").is_dir(), "tmp created");
        assert!(state_root.join("sessions").is_dir(), "sessions created");
        let mode = std::fs::metadata(&state_root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "state root tightened to 0700");
        let _ = std::fs::remove_dir_all(&base);
    }
}
