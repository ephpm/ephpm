//! Operator-supplied per-site overrides — one file per virtual host, read from
//! a directory outside `sites_dir`.
//!
//! In `sites_dir` mode a vhost directory is the site **container**: the whole
//! checkout, including everything that must not be reachable over HTTP. ePHPm
//! historically served the container itself, which publishes a framework's
//! `vendor/`, `composer.json`, `config/` and `storage/logs/*.log` — and a Laravel
//! log routinely carries stack traces containing env values and database
//! credentials. An override file is how the operator declares a site's real web
//! root, and how it hands that one site a PHP file to run before every request:
//!
//! ```toml
//! # <site_overrides_dir>/<site-key>.toml
//! document_root     = "web"
//! auto_prepend_file = ".ephpm-preview-env.php"
//! ```
//!
//! A site with no override file is completely unaffected — the container stays
//! the document root and no prepend runs, exactly as before.
//!
//! # The schema is deliberately two keys, not a `[php]` section
//!
//! The obvious generalisation — let the file carry an arbitrary `ini` table —
//! is **rejected**, and the reason is the deployment this mechanism exists for.
//! In a PR-preview host the operator's daemon derives this file from a manifest
//! committed *inside the tenant's repository*, so every value here is
//! transitively tenant-influenced. An arbitrary INI channel from that source is
//! a sandbox-escape primitive: `open_basedir`, `include_path`, `sys_temp_dir`,
//! `upload_tmp_dir`, `session.save_path` and `error_log` are precisely the
//! directives [`crate::router`] *derives per vhost* to keep tenants apart
//! (issue #276), and a file that could set them would hand that boundary to the
//! thing it is defending against.
//!
//! [`auto_prepend_file`](SiteOverride::auto_prepend_file) is safe under exactly
//! the same argument that makes the `php:` middleware lane safe in multi-tenant
//! mode: the named script is **inside the tenant's own container**, so it runs
//! with precisely the reach that tenant's `index.php` already has and not one
//! byte more. It cannot widen `open_basedir`; it lives inside it.
//!
//! If a bounded set of resource knobs (`memory_limit`, `max_execution_time`) is
//! ever wanted, it should arrive as named, typed, individually-clamped fields —
//! not as a table. Nothing here forecloses that.
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
//! [`validate_declared_root`] and [`validate_declared_prepend`] treat the
//! declared path as if hostile: relative only, `Component::Normal` segments
//! only, canonicalized, and required to resolve inside the site container.
//! "The daemon validated it" is a claim about another codebase's *current*
//! behaviour, not an invariant this one can enforce, and the check costs one
//! `canonicalize` per site per `SITE_CONFIG_TTL`.
//!
//! Both paths resolve against the **container**, not the web root, and that is
//! deliberate for the prepend: the useful placement for a secret-bearing
//! bootstrap file is *above* the web root, where no URL can reach it. Forcing
//! it to be document-root-relative would force it onto the HTTP surface.
//!
//! Containment here is the first of two independent boundaries. The second is
//! PHP's own: `auto_prepend_file` is opened through the stream layer, which
//! checks the **realpath** against `open_basedir` — and `open_basedir` is the
//! container, derived by [`crate::router`] and never override-controlled. So a
//! tenant that replaces a validated prepend file with a symlink out of its
//! container after the check has run gets a fatal at include time, not a read.
//! (Contrast the static-file path, which has no such backstop — hence #394's
//! per-resolve re-check for `document_root`.)

use std::path::{Component, Path, PathBuf};

/// Every key this ePHPm understands in an override file.
///
/// Drives the "did you mean" hint on an unrecognized key, so it must stay in
/// step with [`RawOverride`]'s fields — pinned by
/// [`tests::known_keys_matches_the_parsed_schema`].
const KNOWN_KEYS: &[&str] = &["document_root", "auto_prepend_file"];

/// Whether this server is able to honour a per-site `auto_prepend_file` at all.
///
/// Carried into [`load`] rather than checked there so the warning can name the
/// *reason* the declaration is inert, which is the only actionable part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrependSupport {
    /// Per-request mode: the prepend runs inside each request, per vhost.
    Yes,
    /// Worker mode (`[php] mode = "worker"`). The worker script owns the
    /// request loop and is executed **once**, at boot, for whichever site
    /// happened to be first — so a per-site prepend would either never run or
    /// run for the wrong tenant. Same reasoning (and same alternative) as the
    /// `php:` middleware lane's startup rejection.
    NoWorkerMode,
}

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
    /// A PHP file to execute immediately before this vhost's script, on every
    /// request — PHP's own `auto_prepend_file`, scoped to one site. Already
    /// validated as a contained, existing regular file under the site
    /// container, and absolute so it never resolves against `include_path`.
    ///
    /// The demanded use is injecting per-deployment environment into an app
    /// whose document root *is* its repository root, where there is nowhere
    /// else to put a bootstrap hook (`switchboard#4`).
    ///
    /// `None` under all the same failure modes as `document_root`, plus worker
    /// mode ([`PrependSupport::NoWorkerMode`]).
    pub(crate) auto_prepend_file: Option<PathBuf>,
    /// The keys in the file that this ePHPm does not understand, sorted.
    ///
    /// Diagnostic only — nothing routes on it. Returned rather than logged here
    /// because the file is re-read every `SITE_CONFIG_TTL`, so warning from
    /// inside `load` would emit the same line every two seconds forever. The
    /// caller ([`crate::router::Router::site_roots`]) owns the previous value
    /// and reports a *transition*, exactly as it does for `document_root`.
    pub(crate) unknown_keys: Vec<String>,
}

/// The override file's schema.
///
/// Unknown keys are **tolerated** — but no longer silently. See
/// [`load`] for the version-skew argument that keeps this file lenient while
/// every `[section]` in `ephpm-config` is `deny_unknown_fields`.
#[derive(serde::Deserialize)]
struct RawOverride {
    #[serde(default)]
    document_root: Option<String>,
    #[serde(default)]
    auto_prepend_file: Option<String>,
    #[serde(flatten)]
    unknown: toml::Table,
}

/// What a declared path has to *be* once it resolves.
#[derive(Debug, Clone, Copy)]
enum Expect {
    /// A directory — `document_root`.
    Directory,
    /// A regular file — `auto_prepend_file`.
    File,
}

impl Expect {
    /// How a failure of this expectation reads in the rejection warning.
    fn mismatch(self) -> &'static str {
        match self {
            Self::Directory => "is not a directory",
            Self::File => "is not a regular file",
        }
    }
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
///
/// # Why an unknown key does not fail the file
///
/// Every `[section]` in `ephpm-config` is `deny_unknown_fields`, because there a
/// misspelled key is an operator instruction that silently became a no-op and
/// the cost of catching it is a startup error the operator sees immediately.
/// Neither half of that holds here, and the asymmetry is what decides it:
///
/// * **There is no startup to fail.** This file is read lazily, per site, with a
///   short TTL. "Fail closed" can only mean *discarding the whole file*, which
///   throws away `document_root` too — and a site that loses its declared web
///   root serves its entire container, publishing `vendor/`, `.git` and
///   `storage/logs/`. Refusing the file is a **worse** outcome than tolerating
///   the key, not a safer one.
/// * **The writer ships on its own schedule.** The provisioning daemon and the
///   server are upgraded independently and in either order. A daemon that
///   learns a key one release before the fleet does would, under
///   `deny_unknown_fields`, take every site it manages off its web root at once.
///
/// So the key is ignored — and returned in
/// [`SiteOverride::unknown_keys`] for the caller to report at `warn`, with the
/// file, the site, the keys and a [`did_you_mean`] hint. The thing #463
/// actually found was not the tolerance but the `debug` line nobody reads: a
/// misspelled `documnet_root` now names itself in the log next to the site it
/// broke.
pub(crate) fn load(
    overrides_dir: &Path,
    site_key: &str,
    container: &Path,
    prepend: PrependSupport,
) -> SiteOverride {
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

    let mut unknown_keys: Vec<String> = raw.unknown.keys().cloned().collect();
    unknown_keys.sort_unstable();

    let document_root = raw
        .document_root
        .as_deref()
        .and_then(|declared| validate_declared_root(container, declared, site_key));

    let auto_prepend_file = raw.auto_prepend_file.as_deref().and_then(|declared| {
        match prepend {
            PrependSupport::Yes => validate_declared_prepend(container, declared, site_key),
            // Not a silent drop: worker mode is a deliberate, permanent
            // limitation of the mechanism and the operator has to hear about it
            // once per site, at startup (`seed_site_roots` reads every site).
            PrependSupport::NoWorkerMode => {
                tracing::warn!(
                    path = %path.display(),
                    site = site_key,
                    declared,
                    "per-site override declares auto_prepend_file, but `[php] mode = \"worker\"` \
                     cannot run one — the worker script owns the request loop, so there is no \
                     per-request prepend position. Use the framework's own middleware (PSR-15 / \
                     Octane) or switch the site to `mode = \"per_request\"`"
                );
                None
            }
        }
    });

    SiteOverride { document_root, auto_prepend_file, unknown_keys }
}

/// The "did you mean" clause for a set of unrecognized keys, or `None` when no
/// key is close enough to a known one to guess at.
///
/// Formatted here rather than at the log site so [`KNOWN_KEYS`] and the matching
/// rule stay in one module.
pub(crate) fn did_you_mean(unknown_keys: &[String]) -> Option<String> {
    let hints: Vec<String> = unknown_keys
        .iter()
        .filter_map(|key| nearest_known_key(key).map(|known| format!("{key} -> {known}")))
        .collect();
    (!hints.is_empty()).then(|| hints.join(", "))
}

/// The keys an override file may legally declare, for an operator-facing
/// message that has to say what *was* understood.
pub(crate) fn understood_keys() -> String {
    KNOWN_KEYS.join(", ")
}

/// Validate a declared `document_root` against its site container, returning the
/// resolved absolute path or `None` (with a warning) if it must be rejected.
///
/// `"."` is accepted as "the container", because that is the natural spelling of
/// "this repository has no separate web root" and a daemon translating an
/// application manifest will emit it verbatim. It returns `None` — same outcome
/// as no override at all — rather than a rejection warning.
fn validate_declared_root(container: &Path, declared: &str, site_key: &str) -> Option<PathBuf> {
    let trimmed = declared.trim().trim_end_matches('/');
    // "." / "./" / "" all mean "the container is the web root".
    if trimmed.is_empty() || trimmed == "." {
        return None;
    }
    resolve_contained(container, trimmed, site_key, "document_root", Expect::Directory)
}

/// Validate a declared `auto_prepend_file` against its site container.
///
/// Same containment rules as [`validate_declared_root`] — and against the same
/// base, the container rather than the web root, so the file may (and should)
/// live *above* the document root where no URL reaches it.
///
/// Two differences from the root case:
///
/// 1. It must resolve to a **regular file**. `"."` therefore has no meaning here
///    and is rejected by that check rather than silently accepted; only the
///    empty string spells "no prepend".
/// 2. The value handed onward is **absolute**. PHP resolves a relative
///    `auto_prepend_file` against the current working directory and then
///    `include_path`, and `php_execute_script` has already `chdir`-ed into the
///    *primary script's* directory by then — so a relative value would name a
///    different file depending on which entrypoint the request hit.
fn validate_declared_prepend(container: &Path, declared: &str, site_key: &str) -> Option<PathBuf> {
    let trimmed = declared.trim().trim_end_matches('/');
    // Empty is "declared nothing", matching how `document_root` spells it. A
    // daemon that renders an empty template variable gets a no-op, not a
    // warning it cannot act on.
    if trimmed.is_empty() {
        return None;
    }
    resolve_contained(container, trimmed, site_key, "auto_prepend_file", Expect::File)
}

/// The containment core shared by both declared paths.
///
/// # The two checks, and why both are needed
///
/// 1. **Lexical** — every component must be [`Component::Normal`]. That rejects
///    absolute paths (`/etc`), Windows prefixes (`C:\Windows`), root components,
///    `.` and — the one that matters — `..`. It runs *before* any filesystem
///    access, so a traversal attempt never even causes a stat, and the warning
///    can say accurately what was wrong.
/// 2. **Canonical containment** — join, canonicalize both sides, and require the
///    target to start with the canonicalized container and to match `expect`.
///    The lexical check cannot see through a symlink; this one can. A declared
///    `web` where `web -> /etc` passes check 1 and is caught here.
fn resolve_contained(
    container: &Path,
    trimmed: &str,
    site_key: &str,
    key: &'static str,
    expect: Expect,
) -> Option<PathBuf> {
    let reject = |reason: &str| {
        tracing::warn!(
            container = %container.display(),
            site = site_key,
            key,
            declared = trimmed,
            reason,
            "per-site override declared a path that was rejected — the site behaves as if the \
             key were absent"
        );
        None::<PathBuf>
    };

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
    let shape_ok = match expect {
        Expect::Directory => target.is_dir(),
        Expect::File => target.is_file(),
    };
    if !shape_ok {
        return reject(expect.mismatch());
    }

    // On Windows `canonicalize()` returns an extended-length *verbatim* path
    // (`\\?\C:\...`), and PHP's stream layer cannot open one — an override would
    // produce a `DOCUMENT_ROOT` and `SCRIPT_FILENAME` that every `require` and
    // `include` fails on, and an `auto_prepend_file` that fatals the request.
    // The containment check above deliberately runs on the verbatim forms (both
    // sides canonicalized, so their prefixes agree); only the value handed
    // onwards is simplified. Same hazard, same fix, same helper as
    // `[php.worker] script`.
    Some(ephpm_config::strip_verbatim_prefix(target))
}

/// The known key an unrecognized one was most plausibly meant to be, if any.
///
/// Deliberately narrow: an edit distance of at most 2 over a key of at least
/// four characters. That catches the failure this exists for — a transposition
/// or dropped letter in `document_root` — without inventing a suggestion for a
/// genuine forward-compatibility key from a newer daemon, where a wrong hint
/// would be worse than none.
fn nearest_known_key(unknown: &str) -> Option<&'static str> {
    if unknown.len() < 4 {
        return None;
    }
    KNOWN_KEYS
        .iter()
        .map(|known| (edit_distance(unknown, known), *known))
        .filter(|(distance, _)| *distance > 0 && *distance <= 2)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, known)| known)
}

/// Levenshtein distance between two ASCII-ish keys, over `char`s.
///
/// One row of the DP table; the inputs are TOML key names, so both are short and
/// the allocation is off any hot path (an override file is read at most once per
/// site per `SITE_CONFIG_TTL`, and only when it declares an unknown key).
fn edit_distance(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b_chars.len()).collect();
    let mut current = vec![0usize; b_chars.len() + 1];

    for (i, a_char) in a.chars().enumerate() {
        current[0] = i + 1;
        for (j, &b_char) in b_chars.iter().enumerate() {
            let substitution = previous[j] + usize::from(a_char != b_char);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b_chars.len()]
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

        /// Drop a PHP file into the container at `relative`, creating parents.
        fn php(&self, relative: &str) -> PathBuf {
            let path = self.container.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"<?php $_SERVER['X'] = 1;").unwrap();
            path
        }

        fn load(&self) -> SiteOverride {
            load(&self.overrides, "app.test", &self.container, PrependSupport::Yes)
        }

        fn load_worker(&self) -> SiteOverride {
            load(&self.overrides, "app.test", &self.container, PrependSupport::NoWorkerMode)
        }
    }

    /// The canonical, PHP-openable spelling of a path inside the fixture.
    fn resolved(path: &Path) -> PathBuf {
        path.canonicalize().map(ephpm_config::strip_verbatim_prefix).unwrap()
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
        assert_eq!(f.load().document_root, Some(resolved(&f.container.join("web"))));
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
        assert_eq!(f.load().document_root, Some(resolved(&f.container.join("app").join("htdocs"))));
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

    // ── auto_prepend_file (#463) ───────────────────────────────────────

    /// The whole point of #463: this key used to be swallowed by the flattened
    /// `unknown` table, logged at `debug`, and dropped. A file declaring only
    /// `auto_prepend_file` must now resolve it — if this fails, the key is back
    /// to being a no-op dressed as a feature.
    #[test]
    fn auto_prepend_file_is_typed_not_swallowed_as_an_unknown_key() {
        let f = fixture();
        let script = f.php("preview-env.php");
        f.write("auto_prepend_file = \"preview-env.php\"\n");

        let over = f.load();
        assert_eq!(over.auto_prepend_file, Some(resolved(&script)));
        // And it did not accidentally become a document root.
        assert_eq!(over.document_root, None);
    }

    /// The demanded shape from `switchboard#4`: docroot is the repository root
    /// AND a prepend is injected. Both keys must apply from one file.
    #[test]
    fn document_root_and_prepend_apply_together() {
        let f = fixture();
        let script = f.php("web/.preview-env.php");
        f.write("document_root = \"web\"\nauto_prepend_file = \"web/.preview-env.php\"\n");

        let over = f.load();
        assert_eq!(over.document_root, Some(resolved(&f.container.join("web"))));
        assert_eq!(over.auto_prepend_file, Some(resolved(&script)));
    }

    /// Containment is against the **container**, not the web root — so the
    /// recommended placement (above the document root, unreachable by URL) is
    /// accepted rather than rejected as an escape.
    #[test]
    fn prepend_above_the_document_root_is_accepted() {
        let f = fixture();
        let script = f.php("bootstrap/env.php");
        f.write("document_root = \"web\"\nauto_prepend_file = \"bootstrap/env.php\"\n");
        assert_eq!(f.load().auto_prepend_file, Some(resolved(&script)));
    }

    /// A dotfile is the recommended spelling when the prepend has to live
    /// inside the web root (`docroot: "."`), because `[server] hidden_files`
    /// keeps it off the HTTP surface. `hidden_files` gates *serving*, never
    /// PHP's own `include`, so the file still runs.
    #[test]
    fn prepend_may_be_a_hidden_dotfile() {
        let f = fixture();
        let script = f.php(".ephpm-preview-env.php");
        f.write("auto_prepend_file = \".ephpm-preview-env.php\"\n");
        assert_eq!(f.load().auto_prepend_file, Some(resolved(&script)));
    }

    #[test]
    fn resolved_prepend_is_absolute_and_not_a_verbatim_path() {
        let f = fixture();
        f.php("preview-env.php");
        f.write("auto_prepend_file = \"preview-env.php\"\n");

        let resolved = f.load().auto_prepend_file.expect("should resolve");
        assert!(resolved.is_absolute(), "PHP resolves a relative prepend against the cwd");
        let shown = resolved.display().to_string();
        assert!(!shown.starts_with(r"\\?\"), "PHP cannot open a verbatim path: {shown}");
    }

    /// An empty value is "no prepend", the same way it is for `document_root` —
    /// a daemon rendering an empty template variable gets a no-op.
    #[test]
    fn empty_prepend_declares_nothing() {
        for text in ["auto_prepend_file = \"\"\n", "auto_prepend_file = \"   \"\n"] {
            let f = fixture();
            f.write(text);
            assert_eq!(f.load().auto_prepend_file, None, "for {text:?}");
        }
    }

    /// **The containment test.** `auto_prepend_file` names a file PHP executes
    /// on every request for this vhost, so a value that escapes the site
    /// container is a tenant-escape primitive. Delete the check in
    /// `resolve_contained` and this fails.
    #[test]
    fn prepend_escaping_the_container_is_rejected() {
        for bad in [
            "..",
            "../",
            "../evil.php",
            "../../etc/passwd",
            "web/../../evil.php",
            "a/../../b.php",
            r"..\..\Windows\win.ini",
            "/etc/passwd",
            "/",
            r"C:\Windows\win.ini",
            r"\\server\share\evil.php",
        ] {
            let f = fixture();
            // Make the traversal target real, so the only thing that can
            // reject it is the containment logic rather than a missing file.
            let outside = f.container.parent().unwrap().join("evil.php");
            std::fs::write(&outside, b"<?php").unwrap();
            let up = f.container.parent().unwrap().parent().unwrap().join("evil.php");
            std::fs::write(&up, b"<?php").unwrap();

            f.write(&format!("auto_prepend_file = {bad:?}\n"));
            assert_eq!(
                f.load().auto_prepend_file,
                None,
                "auto_prepend_file {bad:?} escapes the site container and must be refused"
            );
        }
    }

    /// The lexical check cannot see through a symlink; canonical containment
    /// can. A link with a perfectly ordinary name whose target is outside the
    /// container must not become executable prepend code.
    #[test]
    fn prepend_symlinked_out_of_the_container_is_rejected() {
        let f = fixture();
        let outside = f.container.parent().unwrap().parent().unwrap().join("secrets");
        std::fs::create_dir_all(&outside).unwrap();
        let secret = outside.join("credentials.php");
        std::fs::write(&secret, b"<?php const DB_PASS = 'hunter2';").unwrap();

        if !try_symlink_file(&secret, &f.container.join("env.php")) {
            return; // platform refuses symlinks
        }
        f.write("auto_prepend_file = \"env.php\"\n");

        assert_eq!(
            f.load().auto_prepend_file,
            None,
            "a prepend symlinked outside the container must be rejected"
        );
    }

    #[test]
    fn prepend_naming_a_directory_is_rejected() {
        let f = fixture();
        for bad in ["web", ".", "./"] {
            f.write(&format!("auto_prepend_file = {bad:?}\n"));
            assert_eq!(f.load().auto_prepend_file, None, "for {bad:?}");
        }
    }

    #[test]
    fn prepend_naming_a_missing_file_is_rejected() {
        let f = fixture();
        f.write("auto_prepend_file = \"nope.php\"\n");
        assert_eq!(f.load().auto_prepend_file, None);
    }

    /// A rejected prepend must not take the `document_root` with it. The two
    /// keys are validated independently so a daemon bug in one does not put a
    /// site's `vendor/` on the web.
    #[test]
    fn rejected_prepend_leaves_the_document_root_applied() {
        let f = fixture();
        f.write("document_root = \"web\"\nauto_prepend_file = \"../escape.php\"\n");

        let over = f.load();
        assert_eq!(over.document_root, Some(resolved(&f.container.join("web"))));
        assert_eq!(over.auto_prepend_file, None);
    }

    /// Worker mode has no per-request prepend position, so the key is dropped
    /// (loudly) rather than run once at boot for whichever tenant booted first.
    /// `document_root` still applies — it is a routing decision, not a PHP one.
    #[test]
    fn prepend_is_dropped_in_worker_mode_but_the_document_root_survives() {
        let f = fixture();
        f.php("preview-env.php");
        f.write("document_root = \"web\"\nauto_prepend_file = \"preview-env.php\"\n");

        let over = f.load_worker();
        assert_eq!(over.auto_prepend_file, None, "worker mode cannot run a per-site prepend");
        assert_eq!(over.document_root, Some(resolved(&f.container.join("web"))));
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

        assert_eq!(f.load().document_root, Some(resolved(&release)));
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

    /// Same, for a file symlink.
    fn try_symlink_file(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link).is_ok()
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
            "auto_prepend_file = 42\n",
            "auto_prepend_file = [\"a.php\"]\n",
        ] {
            let f = fixture();
            f.write(text);
            assert_eq!(f.load(), SiteOverride::default(), "for {text:?}");
        }
    }

    /// Unknown keys are tolerated, not fatal: the provisioning daemon and the
    /// server are upgraded independently, and refusing the file would discard
    /// `document_root` too — putting the site's whole container on the web,
    /// which is strictly worse than ignoring one key. See [`load`].
    #[test]
    fn unknown_keys_are_ignored_but_the_known_ones_still_apply() {
        let f = fixture();
        let script = f.php("env.php");
        f.write(
            "document_root = \"web\"\nauto_prepend_file = \"env.php\"\n\
             future_key = true\n[future_section]\nx = 1\n",
        );
        let over = f.load();
        assert!(over.document_root.is_some());
        assert_eq!(over.auto_prepend_file, Some(resolved(&script)));
        // Reported, sorted, for the caller to warn about on transition.
        assert_eq!(over.unknown_keys, vec!["future_key".to_string(), "future_section".to_string()]);
    }

    /// The keys are *reported*, not logged here: an override file is re-read
    /// every `SITE_CONFIG_TTL`, so warning inside `load` would emit the same
    /// line every two seconds forever. The router owns the previous value and
    /// reports the transition.
    #[test]
    fn a_file_with_no_unknown_keys_reports_none() {
        let f = fixture();
        f.write("document_root = \"web\"\n");
        assert!(f.load().unknown_keys.is_empty());
    }

    #[test]
    fn did_you_mean_names_only_plausible_typos() {
        let keys = vec!["documnet_root".to_string(), "some_future_key".to_string()];
        assert_eq!(did_you_mean(&keys).as_deref(), Some("documnet_root -> document_root"));
        assert_eq!(did_you_mean(&["some_future_key".to_string()]), None);
        assert!(understood_keys().contains("auto_prepend_file"));
    }

    // ── The "did you mean" hint on a misspelled key ────────────────────

    /// The typo case #463 called out: the log line existed but nothing failed,
    /// so a misspelled `document_root` presented only as "site serves its
    /// container". The hint has to fire for realistic typos.
    #[test]
    fn near_misses_of_a_known_key_are_suggested() {
        for typo in [
            "documnet_root",
            "document_roots",
            "document_root2",
            "ocument_root",
            "auto_prepend_files",
            "auto_prepend_fil",
        ] {
            assert!(
                nearest_known_key(typo).is_some(),
                "{typo:?} should be recognized as a near-miss of a known key"
            );
        }
        assert_eq!(nearest_known_key("documnet_root"), Some("document_root"));
        assert_eq!(nearest_known_key("auto_prepend_fil"), Some("auto_prepend_file"));
    }

    /// A genuine forward-compatibility key from a newer daemon must NOT be
    /// given a bogus suggestion — a wrong hint is worse than none, because it
    /// sends the reader looking for a typo that is not there.
    #[test]
    fn unrelated_keys_get_no_suggestion() {
        for future in ["memory_limit", "index_files", "fallback", "ini", "tls", "a"] {
            assert_eq!(nearest_known_key(future), None, "{future:?} should get no suggestion");
        }
    }

    /// An exact known key is never "suggested" — it is not unknown.
    #[test]
    fn known_keys_are_not_their_own_suggestion() {
        for known in KNOWN_KEYS {
            assert_eq!(nearest_known_key(known), None);
        }
    }

    /// [`KNOWN_KEYS`] drives an operator-facing message; if a field is added to
    /// [`RawOverride`] without listing it here, the log tells operators the key
    /// they just set is not understood.
    #[test]
    fn known_keys_matches_the_parsed_schema() {
        // Each listed key must actually parse into a typed field rather than
        // landing in the flattened `unknown` table.
        for key in KNOWN_KEYS {
            let raw: RawOverride = toml::from_str(&format!("{key} = \"x\"\n")).unwrap();
            assert!(
                raw.unknown.is_empty(),
                "{key} is in KNOWN_KEYS but is not a typed field on RawOverride"
            );
        }
        // And nothing outside the list parses as typed.
        let raw: RawOverride = toml::from_str("some_future_key = \"x\"\n").unwrap();
        assert_eq!(raw.unknown.len(), 1);
    }

    #[test]
    fn edit_distance_is_symmetric_and_zero_on_equality() {
        assert_eq!(edit_distance("document_root", "document_root"), 0);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abc", ""), 3);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("sitting", "kitten"), 3);
    }
}
