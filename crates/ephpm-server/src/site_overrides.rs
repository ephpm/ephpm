//! Operator-supplied per-site overrides — one file per virtual host, read from
//! a directory outside `sites_dir`.
//!
//! In `sites_dir` mode a vhost directory is the site **container**: the whole
//! checkout, including everything that must not be reachable over HTTP. ePHPm
//! historically served the container itself, which publishes a framework's
//! `vendor/`, `composer.json`, `config/` and `storage/logs/*.log` — and a Laravel
//! log routinely carries stack traces containing env values and database
//! credentials. An override file is how the operator declares a site's real web
//! root:
//!
//! ```toml
//! # <site_overrides_dir>/<site-key>.toml
//! document_root = "web"
//! ```
//!
//! A site with no override file is completely unaffected — the container stays
//! the document root, exactly as before.
//!
//! # Why the file lives outside the tenant's checkout
//!
//! The motivating deployment is a PR-preview host checking out arbitrary
//! customer repositories as vhosts, so the natural-looking design — let the
//! repository declare its own layout in a committed file — was considered and
//! **rejected**:
//!
//! * **A marker inside the container is not a channel.** A vhost's
//!   `open_basedir` *includes* its own container by construction (that is the
//!   invariant [`crate::router::SiteRoots`] exists to protect), so the tenant's
//!   own PHP can rewrite any file in it. A "trusted" file there is trusted in
//!   name only — one `file_put_contents` and the tenant is choosing its own
//!   routing.
//! * **Parsing a tenant's application manifest would put an untrusted parser on
//!   the request path.** The obvious format for that manifest is YAML, and YAML
//!   has *expansion* semantics (anchors, aliases, merge keys): a few hundred
//!   bytes can expand to gigabytes, so a byte cap is not an expansion cap. The
//!   maintained Rust YAML parsers are all young forks of an archived crate.
//! * **Two parsers for one contract drift silently.** A provisioning daemon that
//!   already reads the application manifest and a second reader inside ePHPm are
//!   two interpretations of one `document_root` semantic. When they diverge, the
//!   daemon believes the web root is X while ePHPm serves Y — a worse failure
//!   class than either component simply being wrong.
//!
//! So ePHPm reads only a **derived, operator-owned artifact**. A daemon that
//! consumes an application manifest is welcome to write these files from it;
//! ePHPm never reads anything inside a tenant checkout to decide routing.
//! `Config::validate` refuses to start if `site_overrides_dir` is inside
//! `sites_dir`, which is the one mistake that would silently reintroduce the
//! whole problem.
//!
//! # The containment check stays, despite a trusted writer
//!
//! [`validate_declared_root`] treats the declared path as if hostile: relative
//! only, `Component::Normal` segments only, canonicalized, and required to
//! resolve inside the site container. "The daemon validated it" is a claim about
//! another codebase's *current* behaviour, not an invariant this one can
//! enforce, and the check costs one `canonicalize` per site per
//! `SITE_CONFIG_TTL`.
//!
//! And structurally, nothing here can widen the PHP sandbox: this module only
//! ever produces a `document_root`. `open_basedir` is derived from the container
//! by [`crate::router`] and is unreachable from these files.

use std::path::{Component, Path, PathBuf};

/// The resolved override for one site.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct SiteOverride {
    /// The document root the operator declared, already validated as a
    /// contained, existing directory under the site container.
    ///
    /// `None` when the file is absent, unreadable, malformed, declares nothing,
    /// or declares something that failed validation — all of which collapse to
    /// the same safe outcome: the container is the document root.
    pub(crate) document_root: Option<PathBuf>,
}

/// The override file's schema.
///
/// Unknown keys are **ignored** so an override written for a newer ePHPm never
/// breaks a site on an older one — the provisioning daemon and the server are
/// upgraded independently, and a site 404ing because its override mentions a key
/// the running server has not learned yet would be a bad trade. Unrecognized
/// keys are reported at `debug`; a *typo* in `document_root` therefore shows up
/// as "site serves its container" plus a debug line, which the startup summary
/// in [`crate::router::Router::seed_site_roots`] makes visible.
#[derive(serde::Deserialize)]
struct RawOverride {
    #[serde(default)]
    document_root: Option<String>,
    #[serde(flatten)]
    unknown: toml::Table,
}

/// Read and validate the override for `site_key`, whose container is `container`.
///
/// The file is `<overrides_dir>/<site_key>.toml`. `site_key` is the **canonical
/// site key** — the traversal-safe `[a-z0-9._-]` string that also named the
/// vhost directory, selects `<dir>/<key>.db`, and derives the `pdo_mysql`
/// credential — so the join onto `overrides_dir` is safe by the same argument
/// those paths rely on. A daemon that writes files under any other name simply
/// has no effect: the site serves its container.
///
/// # Failure is always the safe direction
///
/// There is deliberately no error type. Every way this can go wrong produces the
/// behaviour the site had before an override existed, having logged why.
pub(crate) fn load(overrides_dir: &Path, site_key: &str, container: &Path) -> SiteOverride {
    let path = overrides_dir.join(format!("{site_key}.toml"));

    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        // Absent is the normal case for a site with no override. Not an error,
        // not logged — most sites in a fleet will never have one.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return SiteOverride::default(),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                site = site_key,
                error = %e,
                "per-site override could not be read — serving the site container"
            );
            return SiteOverride::default();
        }
    };

    let raw: RawOverride = match toml::from_str(&text) {
        Ok(raw) => raw,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                site = site_key,
                error = %e,
                "per-site override is not valid TOML — serving the site container"
            );
            return SiteOverride::default();
        }
    };

    if !raw.unknown.is_empty() {
        let mut keys: Vec<&str> = raw.unknown.keys().map(String::as_str).collect();
        keys.sort_unstable();
        tracing::debug!(
            path = %path.display(),
            site = site_key,
            keys = %keys.join(", "),
            "per-site override has keys this ePHPm does not understand — ignored"
        );
    }

    let document_root = raw
        .document_root
        .as_deref()
        .and_then(|declared| validate_declared_root(container, declared, site_key));

    SiteOverride { document_root }
}

/// Validate a declared `document_root` against its site container, returning the
/// resolved absolute path or `None` (with a warning) if it must be rejected.
///
/// # The two checks, and why both are needed
///
/// 1. **Lexical** — every component must be [`Component::Normal`]. That rejects
///    absolute paths (`/etc`), Windows prefixes (`C:\Windows`), root components,
///    `.` and — the one that matters — `..`. It runs *before* any filesystem
///    access, so a traversal attempt never even causes a stat, and the warning
///    can say accurately what was wrong.
/// 2. **Canonical containment** — join, canonicalize both sides, and require the
///    target to start with the canonicalized container and to be a directory.
///    The lexical check cannot see through a symlink; this one can. A declared
///    `web` where `web -> /etc` passes check 1 and is caught here.
///
/// `"."` is accepted as "the container", because that is the natural spelling of
/// "this repository has no separate web root" and a daemon translating an
/// application manifest will emit it verbatim. It returns `None` — same outcome
/// as no override at all — rather than a rejection warning.
fn validate_declared_root(container: &Path, declared: &str, site_key: &str) -> Option<PathBuf> {
    let reject = |reason: &str| {
        tracing::warn!(
            container = %container.display(),
            site = site_key,
            declared,
            reason,
            "per-site override declared a document_root that was rejected — \
             serving the site container"
        );
        None::<PathBuf>
    };

    let trimmed = declared.trim().trim_end_matches('/');
    // "." / "./" / "" all mean "the container is the web root".
    if trimmed.is_empty() || trimmed == "." {
        return None;
    }

    let relative = Path::new(trimmed);
    if relative.is_absolute() {
        return reject("absolute paths are not permitted — declare a path relative to the site");
    }
    if !relative.components().all(|c| matches!(c, Component::Normal(_))) {
        return reject(
            "must be a plain relative path with no `..`, `.`, drive prefix or root component",
        );
    }
    // Backslash is a separator on Windows but an ordinary filename character on
    // Unix, so `..\..\etc` would pass the component check on Unix as a single
    // "normal" component and then act as a separator on Windows. Reject it so an
    // override file means the same thing on every platform.
    if trimmed.contains('\\') {
        return reject("backslashes are not permitted — use `/` as the separator on all platforms");
    }

    let candidate = container.join(relative);

    // Canonicalize both sides. `canonicalize` resolves symlinks, `..` and (on
    // Windows) 8.3 short names, which is what makes the containment comparison
    // meaningful rather than textual.
    let (Ok(target), Ok(canonical_container)) =
        (candidate.canonicalize(), container.canonicalize())
    else {
        return reject("does not exist, or could not be resolved");
    };

    if !target.starts_with(&canonical_container) {
        return reject("resolves outside the site container (symlink escape or traversal)");
    }
    if !target.is_dir() {
        return reject("is not a directory");
    }

    // On Windows `canonicalize()` returns an extended-length *verbatim* path
    // (`\\?\C:\...`), and PHP's stream layer cannot open one — an override would
    // produce a `DOCUMENT_ROOT` and `SCRIPT_FILENAME` that every `require` and
    // `include` fails on. The containment check above deliberately runs on the
    // verbatim forms (both sides canonicalized, so their prefixes agree); only
    // the value handed onwards is simplified. Same hazard, same fix, same
    // helper as `[php.worker] script`.
    Some(ephpm_config::strip_verbatim_prefix(target))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fleet root with one site container and an override directory beside
    /// (not inside) the sites directory.
    struct Fixture {
        _dir: tempfile::TempDir,
        overrides: PathBuf,
        container: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let overrides = dir.path().join("overrides");
        let container = dir.path().join("sites").join("app.test");
        std::fs::create_dir_all(&overrides).unwrap();
        std::fs::create_dir_all(container.join("web")).unwrap();
        std::fs::create_dir_all(container.join("vendor")).unwrap();
        Fixture { _dir: dir, overrides, container }
    }

    impl Fixture {
        fn write(&self, text: &str) {
            std::fs::write(self.overrides.join("app.test.toml"), text).unwrap();
        }

        fn load(&self) -> SiteOverride {
            load(&self.overrides, "app.test", &self.container)
        }
    }

    #[test]
    fn absent_override_declares_nothing() {
        let f = fixture();
        assert_eq!(f.load(), SiteOverride::default());
    }

    #[test]
    fn valid_declaration_resolves_to_the_subdirectory() {
        let f = fixture();
        f.write("document_root = \"web\"\n");
        assert_eq!(
            f.load().document_root,
            Some(
                f.container
                    .join("web")
                    .canonicalize()
                    .map(ephpm_config::strip_verbatim_prefix)
                    .unwrap()
            )
        );
    }

    /// The resolved root must be a path PHP's stream layer can actually open.
    ///
    /// On Windows `canonicalize()` yields an extended-length *verbatim* path
    /// (`\\?\C:\...`), and PHP cannot open one — shipping that as
    /// `DOCUMENT_ROOT`/`SCRIPT_FILENAME` makes every `require` in the site fail
    /// with "Failed to open stream". Caught by driving real HTTP; pinned here.
    /// Same hazard `[php.worker] script` already had, hence the shared helper.
    #[test]
    fn resolved_root_is_not_a_verbatim_path() {
        let f = fixture();
        f.write("document_root = \"web\"\n");
        let resolved = f.load().document_root.expect("should resolve");
        let shown = resolved.display().to_string();
        assert!(
            !shown.starts_with(r"\\?\"),
            "resolved document_root must not be a verbatim path, got {shown}"
        );
        // Still the right directory, just spelled the way PHP can open.
        assert!(shown.ends_with("web"), "unexpected resolution: {shown}");
    }

    #[test]
    fn nested_declaration_is_allowed_while_contained() {
        let f = fixture();
        std::fs::create_dir_all(f.container.join("app").join("htdocs")).unwrap();
        f.write("document_root = \"app/htdocs\"\n");
        assert_eq!(
            f.load().document_root,
            Some(
                f.container
                    .join("app")
                    .join("htdocs")
                    .canonicalize()
                    .map(ephpm_config::strip_verbatim_prefix)
                    .unwrap()
            )
        );
    }

    /// `"."` is how "no separate web root" is naturally spelled, and a daemon
    /// translating an application manifest will emit it verbatim. It must mean
    /// "the container", not "rejected".
    #[test]
    fn dot_means_the_container_without_a_warning() {
        for text in ["document_root = \".\"\n", "document_root = \"./\"\n"] {
            let f = fixture();
            f.write(text);
            assert_eq!(f.load().document_root, None, "for {text:?}");
        }
    }

    #[test]
    fn empty_or_missing_key_declares_nothing() {
        for text in ["", "# just a comment\n", "document_root = \"\"\n", "document_root = \"  \"\n"]
        {
            let f = fixture();
            f.write(text);
            assert_eq!(f.load().document_root, None, "for {text:?}");
        }
    }

    /// A file named for a different site is simply not read — a daemon writing
    /// `preview-1234.toml` while the vhost is `app.test` fails open (site serves
    /// its container), which is safe but silent.
    #[test]
    fn override_named_for_another_site_is_not_read() {
        let f = fixture();
        std::fs::write(f.overrides.join("some-other-key.toml"), "document_root = \"web\"\n")
            .unwrap();
        assert_eq!(f.load().document_root, None);
    }

    // ── Declarations rejected even though the writer is trusted ────────

    #[test]
    fn traversal_declarations_are_rejected() {
        for bad in ["..", "../", "../../etc", "web/../../..", "a/../../b", r"..\..\Windows"] {
            let f = fixture();
            f.write(&format!("document_root = {bad:?}\n"));
            assert_eq!(f.load().document_root, None, "traversal {bad:?} must be rejected");
        }
    }

    #[test]
    fn absolute_declarations_are_rejected() {
        for bad in ["/", "/etc", "/etc/passwd", r"C:\Windows", r"\\server\share"] {
            let f = fixture();
            f.write(&format!("document_root = {bad:?}\n"));
            assert_eq!(f.load().document_root, None, "absolute path {bad:?} must be rejected");
        }
    }

    #[test]
    fn declaration_naming_a_file_is_rejected() {
        let f = fixture();
        std::fs::write(f.container.join("index.php"), b"<?php").unwrap();
        f.write("document_root = \"index.php\"\n");
        assert_eq!(f.load().document_root, None);
    }

    #[test]
    fn declaration_naming_a_missing_directory_is_rejected() {
        let f = fixture();
        f.write("document_root = \"nope\"\n");
        assert_eq!(f.load().document_root, None);
    }

    /// The lexical check cannot see through a symlink; canonical containment
    /// can. `escape -> <outside>` is a plain relative name with no `..` in it.
    #[test]
    fn symlink_escaping_the_container_is_rejected() {
        let f = fixture();
        let outside = f.container.parent().unwrap().parent().unwrap().join("secrets");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("id_rsa"), b"private").unwrap();

        if !try_symlink_dir(&outside, &f.container.join("escape")) {
            return; // platform refuses symlinks
        }
        f.write("document_root = \"escape\"\n");

        assert_eq!(
            f.load().document_root,
            None,
            "a symlink resolving outside the container must be rejected"
        );
    }

    /// A symlink that stays inside the container is fine — `web -> releases/42`
    /// is the atomic-swap deploy pattern, and its target is already inside the
    /// vhost's `open_basedir`.
    #[test]
    fn symlink_inside_the_container_is_accepted() {
        let f = fixture();
        let release = f.container.join("releases").join("42");
        std::fs::create_dir_all(&release).unwrap();

        if !try_symlink_dir(&release, &f.container.join("current")) {
            return;
        }
        f.write("document_root = \"current\"\n");

        assert_eq!(
            f.load().document_root,
            Some(release.canonicalize().map(ephpm_config::strip_verbatim_prefix).unwrap())
        );
    }

    /// Create a directory symlink, or return `false` when the platform refuses
    /// (Windows without the symlink privilege). Callers skip rather than fail:
    /// the check is exercised on every other platform, and a host that cannot
    /// create a symlink cannot deploy one either.
    fn try_symlink_dir(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(target, link).is_ok()
        }
    }

    // ── Robustness ─────────────────────────────────────────────────────

    /// A malformed override must not break the site. The writer is trusted, but
    /// a half-written file (a daemon interrupted mid-write) is a real state.
    #[test]
    fn malformed_override_serves_the_container() {
        for text in [
            "document_root =\n",
            "document_root = web\n",
            "[[[\n",
            "document_root = 42\n",
            "document_root = [\"web\"]\n",
        ] {
            let f = fixture();
            f.write(text);
            assert_eq!(f.load().document_root, None, "for {text:?}");
        }
    }

    /// Unknown keys are ignored, not fatal: the provisioning daemon and the
    /// server are upgraded independently, and an override mentioning a key this
    /// server has not learned yet must not take the site down.
    #[test]
    fn unknown_keys_are_ignored_but_the_known_one_still_applies() {
        let f = fixture();
        f.write("document_root = \"web\"\nfuture_key = true\n[future_section]\nx = 1\n");
        assert!(f.load().document_root.is_some());
    }
}
