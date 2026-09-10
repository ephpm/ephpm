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
const KNOWN_KEYS: &[&str] = &["document_root", "auto_prepend_file", "preview_auth"];

/// Shortest accepted preview-gate `session_secret`, in bytes.
///
/// 32 bytes is the HMAC-SHA256 block-security level and the same floor the
/// `github-auth` issuer enforces on the key it signs sessions with — the two
/// must agree, and a short key is what makes offline forgery of a self-contained
/// token worth attempting.
const MIN_SESSION_SECRET: usize = 32;

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
///
/// Not `Eq`: [`preview_gate`](Self::preview_gate) is a `serde_json::Value`,
/// which is only `PartialEq` (it can hold a float). `PartialEq` is all the
/// tests need.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct SiteOverride {
    /// The document root the operator declared, already validated as a
    /// contained, existing directory under the site container.
    ///
    /// `None` means **the container is the web root**, and it is only ever set
    /// that way when nothing was declared: no file, no `document_root` key, or
    /// the explicit `"."`. A declaration that could not be honoured sets
    /// [`unusable`](Self::unusable) instead and leaves this `None` — a caller
    /// that resolved to the container from a *failed* narrowing instruction
    /// would be serving more than the operator asked for.
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
    /// A fully-resolved [`crate::preview-gate`](ephpm_middleware_builtins::preview_gate)
    /// config for this site, as the JSON the builtin's `init` accepts — present
    /// only when the operator declared a valid `[preview_auth]` section. `None`
    /// means the site is **not** access-gated (the common case).
    ///
    /// The `session_secret` is already resolved to a literal here (following any
    /// `env:`/`file:` indirection), so the caller can build the gate without
    /// re-touching the environment or filesystem. It is the operator-owned
    /// secret, never anything a tenant's PHP can write.
    ///
    /// A declared-but-broken `[preview_auth]` never lands here: it sets
    /// [`unusable`](Self::unusable) instead, because a gate that cannot be built
    /// must fail **closed** (503) rather than serve the preview ungated — the
    /// same narrowing-instruction rule `document_root` follows.
    pub(crate) preview_gate: Option<serde_json::Value>,
    /// The preview access gate's target repository (`[preview_auth] repo`),
    /// validated as `owner/name`. Present only when the operator declared it.
    ///
    /// Kept **separate** from [`preview_gate`](Self::preview_gate) on purpose:
    /// the per-site verifier (the `preview-gate` builtin) must never see it — it
    /// verifies the session, and repo authorization is the OAuth *issuer*'s job,
    /// done once at login. The router carries this on the trusted
    /// `request_gate_repo` ABI channel so the issuer can seal it into the signed
    /// OAuth state; it never travels a request header.
    ///
    /// A present-but-invalid `repo` does **not** land here: it sets
    /// [`unusable`](Self::unusable), failing the one site closed (503) rather
    /// than gating it against a malformed target.
    pub(crate) preview_gate_repo: Option<String>,
    /// Set when the override file **exists but cannot be honoured**: it is
    /// unreadable, it is not valid TOML, or a key this binary implements
    /// carries a value it had to reject. The value is a short, stable reason
    /// for the log and the operator.
    ///
    /// The caller must refuse to serve this site rather than fall back to the
    /// container. `document_root` is a *narrowing* instruction, so the old
    /// fallback was wider than what the operator asked for: a daemon
    /// interrupted mid-write published the `vendor/`, `.git` and
    /// `storage/logs/` that the override existed to hide, with a warning and a
    /// green health check. Refusing one site is contained, loud, and the only
    /// outcome a health check catches.
    ///
    /// `None` for an absent file (the documented "container is the web root"
    /// default) and for a file that declares nothing, or only things this
    /// binary does not implement.
    pub(crate) unusable: Option<&'static str>,
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
    #[serde(default)]
    preview_auth: Option<RawPreviewAuth>,
    #[serde(flatten)]
    unknown: toml::Table,
}

/// The `[preview_auth]` section: it turns the GitHub-OAuth **preview access
/// gate** on for this one vhost and carries what the request-phase enforcer
/// needs. Written by the operator/switchboard, never by the tenant.
///
/// Only the enforcement half lives here. The OAuth *issuer* (`github-auth`, the
/// cold-path login/callback round trip that holds the GitHub App's
/// `client_id`/`client_secret`) stays a normal operator-owned `[[middleware]]`
/// mount — deliberately **not** in this file, because this file is derived from
/// a manifest inside the tenant's own repository, and an OAuth client secret is
/// exactly the kind of value that must not travel a tenant-influenced channel.
/// The two are coupled by the shared `session_secret` (typically an
/// `env:NAME` reference both read) and the matching `cookie` name; see the
/// preview-access-gate roadmap for the switchboard contract.
#[derive(serde::Deserialize)]
struct RawPreviewAuth {
    /// HS256 session secret, or an indirection: `env:NAME` reads that
    /// environment variable, `file:/abs/path` reads the file (trimmed). The
    /// indirections keep the literal out of a file switchboard derives from
    /// tenant input, and let this and the issuer name one source of truth.
    session_secret: Option<String>,
    /// Where unauthenticated browsers are redirected — the issuer's login path.
    /// Required. The issuer's default `/_ephpm/auth/github/login` **is**
    /// reachable: since #487 the router carves `/_ephpm/auth/` out of the
    /// reserved-namespace 404 and dispatches it to the global middleware chain
    /// (after site resolution), so the issuer's login/callback endpoints work at
    /// their defaults. A path outside `/_ephpm/` is still fine if the operator
    /// prefers one; it just must match the issuer's `login_path`.
    login_url: Option<String>,
    /// The preview's target repository, `owner/name` (issue #487 per-preview
    /// gate). Written by switchboard, validated here; surfaced separately on
    /// [`SiteOverride::preview_gate_repo`] and carried to the OAuth issuer on
    /// the trusted `request_gate_repo` ABI channel — **never** folded into the
    /// gate config the verifier sees. A present-but-invalid value fails the site
    /// closed (503), like any other value this binary understood and rejected.
    repo: Option<String>,
    /// Session cookie name (must match the issuer's `cookie_name`).
    cookie: Option<String>,
    /// Required `iss` claim.
    issuer: Option<String>,
    /// Required `aud` claim.
    audience: Option<String>,
    /// Refuse a credential over cleartext (loopback exempt). Defaults on.
    require_https: Option<bool>,
    /// Require the token's `site` claim to equal this vhost (issue #396).
    /// Defaults on; a preview should never turn it off.
    require_site: Option<bool>,
    /// Query parameter carrying a shareable-URL capability token.
    share_param: Option<String>,
    /// Static revoke-all floor: a share token issued before this unix time is
    /// refused.
    share_epoch: Option<u64>,
    /// Consult the KV deny-list / per-site epoch for share tokens. Defaults on.
    share_revocation: Option<bool>,
    /// Query parameter on `login_url` carrying the validated return path.
    return_to_param: Option<String>,
    /// Query parameter on `login_url` carrying this vhost's site key.
    site_param: Option<String>,
    /// Paths that bypass the gate entirely — the issuer's login/callback
    /// endpoints, so the OAuth round trip is never redirected back to login.
    exempt_paths: Option<Vec<String>>,
    /// Unknown keys inside the section — tolerated (forward-compat with a newer
    /// switchboard) and surfaced in the file's [`SiteOverride::unknown_keys`]
    /// report, exactly as unknown top-level keys are.
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
/// # Failing "safe" used to mean failing WIDER, which is not safe
///
/// This module used to say "failure is always the safe direction: every way this
/// can go wrong produces the behaviour the site had before an override existed."
/// That conflated two different kinds of safe. `document_root` is a
/// **narrowing** instruction — the operator wrote it to stop serving `vendor/`,
/// `.git` and `storage/logs/laravel.log`. Falling back to the container when it
/// cannot be applied is safe for *availability* and the wrong direction for
/// *containment*: a daemon interrupted mid-write published the very files the
/// override existed to hide, with a warning and a green health check.
///
/// So the fallback now depends on whether the operator asked for anything:
///
/// | State | Outcome |
/// |---|---|
/// | No file | Container is the web root. The default, not a failure. |
/// | `document_root = "."` / empty | Container is the web root. Explicitly requested. |
/// | File unreadable, or not valid TOML | [`SiteOverride::unusable`] — the site refuses to serve |
/// | A key we implement, with a value we cannot honour | [`SiteOverride::unusable`] — the site refuses to serve |
/// | A key we do not implement at all | Ignored, loudly. The site serves. |
///
/// Refusing to serve is deliberately scoped to **one site**: refusing to start
/// would let a single bad tenant file kill every tenant, and serving the
/// container is what this section exists to stop. It is also the only outcome a
/// health check catches, which is the half of #429 that actually mattered.
///
/// # Why an unknown key still does not fail the file
///
/// Every `[section]` in `ephpm-config` is `deny_unknown_fields` — #429's lesson,
/// where `per_site = true` on a binary predating the knob parsed fine and came up
/// in the wrong mode with every health check green. That lesson is honoured
/// above: everything this binary *understood and could not do* now fails closed.
/// What stays lenient is the strictly narrower case of a key it did not
/// understand at all, and three properties separate that from `ephpm.toml`:
///
/// * **Different author, different release cadence — by design.** `ephpm.toml`
///   is written by the operator who chose the binary. This file is written by a
///   separate program, deliberately (see the derived-artifact argument above),
///   so #429's assumption that the config author knows which binary they are
///   running does not hold.
/// * **The skew has a direction, and it is the bad one.** On the motivating
///   fleet the daemon is built from source on demand while ePHPm comes from
///   tagged releases, so the *writer leads*. Under `deny_unknown_fields` the
///   next daemon release that adds a key takes every site it manages off its web
///   root simultaneously, from a routine deploy of another repository. A missing
///   feature is better than a fleet outage.
/// * **The strict reaction is unbounded here.** For `ephpm.toml`, strict means
///   one node fails to start while the operator is watching it. Here it means
///   every site fails at once, asynchronously, on a running fleet.
///
/// A schema-version field was considered as the "make skew explicit" answer and
/// **rejected**: it renames the problem rather than solving it. A daemon adding
/// a key must bump the version or the key fails closed; bumping it makes every
/// server that does not know that version reject the file — the same fleet
/// outage with a better error message. Avoiding that needs the daemon to
/// negotiate down to the server's version, which is a capability channel this
/// architecture deliberately does not have (the daemon writes files; it never
/// reads ePHPm). An `x-` experimental-key prefix has the same two-phase trap:
/// the rename from `x-foo` to `foo` is itself the breaking deploy. Unknown key
/// names *are* the skew signal; nothing is gained by encoding it twice.
///
/// So the key is ignored — and returned in [`SiteOverride::unknown_keys`] for
/// the caller to report at `warn`, with the file, the site, the keys and a
/// [`did_you_mean`] hint. The thing #463 actually found was not the tolerance
/// but the `debug` line nobody reads.
pub(crate) fn load(
    overrides_dir: &Path,
    site_key: &str,
    container: &Path,
    prepend: PrependSupport,
) -> SiteOverride {
    let path = overrides_dir.join(format!("{site_key}.toml"));

    // A file that exists but cannot be understood is NOT "no override": the
    // operator asked for something and we do not know what. Serving the
    // container would publish whatever the file was written to hide.
    let unusable = |reason: &'static str, detail: &dyn std::fmt::Display| {
        tracing::warn!(
            path = %path.display(),
            site = site_key,
            reason,
            detail = %detail,
            "per-site override exists but cannot be honoured — this site will REFUSE TO SERVE \
             (503) rather than fall back to serving its whole container, which is what the \
             override was written to prevent"
        );
        SiteOverride { unusable: Some(reason), ..SiteOverride::default() }
    };

    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        // Absent is the normal case for a site with no override. Not an error,
        // not logged — most sites in a fleet will never have one, and "no file"
        // is the documented way to say "the container is the web root".
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return SiteOverride::default(),
        Err(e) => return unusable("override file could not be read", &e),
    };

    let raw: RawOverride = match toml::from_str(&text) {
        Ok(raw) => raw,
        Err(e) => return unusable("override file is not valid TOML", &e),
    };

    let mut unknown_keys: Vec<String> = raw.unknown.keys().cloned().collect();
    // Unknown keys *inside* `[preview_auth]` are tolerated the same way (a newer
    // switchboard adds one), reported with a `preview_auth.` prefix so the
    // operator sees which section they belong to.
    if let Some(pa) = &raw.preview_auth {
        unknown_keys.extend(pa.unknown.keys().map(|k| format!("preview_auth.{k}")));
    }
    unknown_keys.sort_unstable();

    let document_root = match raw.document_root.as_deref() {
        Some(declared) => match validate_declared_root(container, declared, site_key) {
            Ok(resolved) => resolved,
            Err(reason) => return unusable(reason, &declared),
        },
        None => None,
    };

    let auto_prepend_file = match raw.auto_prepend_file.as_deref() {
        Some(declared) => match prepend {
            PrependSupport::Yes => match validate_declared_prepend(container, declared, site_key) {
                Ok(resolved) => resolved,
                Err(reason) => return unusable(reason, &declared),
            },
            // NOT unusable, and the distinction is the whole rule: a value this
            // binary understood and could not honour fails closed, but a
            // *feature this binary does not implement here* belongs in the same
            // forward-compatibility bucket as an unknown key. Worker mode is a
            // permanent, server-wide capability gap, not a broken file — and
            // failing closed on it would take down every site on a
            // worker-mode fleet the moment the daemon started writing the key,
            // which is exactly the outage the leniency argument exists to
            // prevent.
            PrependSupport::NoWorkerMode => {
                tracing::warn!(
                    path = %path.display(),
                    site = site_key,
                    declared,
                    "per-site override declares auto_prepend_file, but `[php] mode = \"worker\"` \
                     cannot run one — the worker script owns the request loop, so there is no \
                     per-request prepend position. The key is IGNORED and the site still serves. \
                     Use the framework's own middleware (PSR-15 / Octane) or switch to \
                     `mode = \"per_request\"`"
                );
                None
            }
        },
        None => None,
    };

    // The access gate is a *narrowing* instruction, exactly like `document_root`:
    // the operator declared `[preview_auth]` to stop serving this preview to
    // anyone who guessed its hostname. So a section we understood and could not
    // turn into a working gate must fail **closed** (`unusable` → 503), never
    // fall back to serving the preview ungated.
    let (preview_gate, preview_gate_repo) = match &raw.preview_auth {
        Some(pa) => match resolve_preview_auth(pa) {
            Ok((config, repo)) => (Some(config), repo),
            Err(reason) => return unusable(reason, &"[preview_auth]"),
        },
        None => (None, None),
    };

    SiteOverride {
        document_root,
        auto_prepend_file,
        preview_gate,
        preview_gate_repo,
        unknown_keys,
        unusable: None,
    }
}

/// Turn a `[preview_auth]` section into the JSON config the
/// [`preview-gate`](ephpm_middleware_builtins::preview_gate) builtin accepts,
/// resolving the session secret indirection.
///
/// Returns the gate config **and** the validated per-preview repository (kept
/// separate — the verifier must not see the repo). Every failure is a
/// `&'static str` reason for [`SiteOverride::unusable`]: a broken gate, or a
/// malformed repo, takes the one preview out of service rather than serving it
/// open or gating it against a bad target.
fn resolve_preview_auth(
    pa: &RawPreviewAuth,
) -> Result<(serde_json::Value, Option<String>), &'static str> {
    let login_url = pa
        .login_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("[preview_auth] requires `login_url`")?;

    let raw_secret = pa
        .session_secret
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("[preview_auth] requires `session_secret`")?;
    let secret = resolve_secret(raw_secret)?;
    if secret.len() < MIN_SESSION_SECRET {
        return Err("[preview_auth] `session_secret` resolved to fewer than 32 bytes");
    }

    // The per-preview repository, if declared. Validated to `owner/name`; a
    // present-but-invalid value fails the site closed, exactly like a broken
    // `document_root`. It is deliberately NOT inserted into the gate `config`
    // below — repo authorization is the OAuth issuer's job, and the per-site
    // verifier this config feeds must never see it (issue #487).
    let repo = match pa.repo.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(spec) => Some(validate_owner_name(spec)?),
        None => None,
    };

    // Assemble the builtin's config. Only set keys the operator supplied, so the
    // builtin applies its own documented defaults for the rest.
    let mut config = serde_json::Map::new();
    config.insert("secret".into(), secret.into());
    config.insert("login_url".into(), login_url.into());
    let mut set_str = |key: &str, v: &Option<String>| {
        if let Some(s) = v.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            config.insert(key.into(), s.into());
        }
    };
    set_str("cookie", &pa.cookie);
    set_str("issuer", &pa.issuer);
    set_str("audience", &pa.audience);
    set_str("share_param", &pa.share_param);
    set_str("return_to_param", &pa.return_to_param);
    set_str("site_param", &pa.site_param);
    if let Some(b) = pa.require_https {
        config.insert("require_https".into(), b.into());
    }
    if let Some(b) = pa.require_site {
        config.insert("require_site".into(), b.into());
    }
    if let Some(b) = pa.share_revocation {
        config.insert("share_revocation".into(), b.into());
    }
    if let Some(e) = pa.share_epoch {
        config.insert("share_epoch".into(), e.into());
    }
    if let Some(paths) = &pa.exempt_paths {
        config.insert("exempt_paths".into(), serde_json::Value::from(paths.clone()));
    }
    Ok((serde_json::Value::Object(config), repo))
}

/// Validate a `[preview_auth] repo` value as `owner/name`.
///
/// Mirrors the issuer's `config::repo_from_spec` shape check (both segments
/// non-empty, no extra `/`, GitHub-name character set, not `.`/`..`) so a value
/// accepted here is one the issuer accepts when it decodes it back out of the
/// signed OAuth state. The duplication is forced: `ephpm-middleware-github-auth`
/// is a `dlopen` module this crate does not link, the same reason `normalize_vhost`
/// is duplicated there.
fn validate_owner_name(spec: &str) -> Result<String, &'static str> {
    let seg_ok = |s: &str| {
        !s.is_empty()
            && s.len() <= 100
            && s != "."
            && s != ".."
            && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    let Some((owner, name)) = spec.split_once('/') else {
        return Err("[preview_auth] `repo` must be `owner/name`");
    };
    if !seg_ok(owner) || name.contains('/') || !seg_ok(name) {
        return Err("[preview_auth] `repo` must be exactly `owner/name` (GitHub names)");
    }
    Ok(format!("{owner}/{name}"))
}

/// Resolve a `session_secret` value, following an `env:NAME` or
/// `file:/abs/path` indirection. A literal is used as-is (discouraged: the
/// override file is derived from tenant input, so a reference keeps the secret
/// out of it). Every failure is a fail-closed `&'static str` reason.
fn resolve_secret(raw: &str) -> Result<String, &'static str> {
    if let Some(var) = raw.strip_prefix("env:") {
        if var.is_empty() {
            return Err("[preview_auth] `session_secret` is `env:` with no variable name");
        }
        let value = std::env::var(var).map_err(
            |_| "[preview_auth] `session_secret` names an environment variable that is not set",
        )?;
        let value = value.trim().to_owned();
        if value.is_empty() {
            return Err("[preview_auth] `session_secret` environment variable is empty");
        }
        Ok(value)
    } else if let Some(path) = raw.strip_prefix("file:") {
        if path.is_empty() {
            return Err("[preview_auth] `session_secret` is `file:` with no path");
        }
        let text = std::fs::read_to_string(path)
            .map_err(|_| "[preview_auth] `session_secret` file could not be read")?;
        let value = text.trim().to_owned();
        if value.is_empty() {
            return Err("[preview_auth] `session_secret` file is empty");
        }
        Ok(value)
    } else {
        Ok(raw.to_owned())
    }
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

/// Validate a declared `document_root` against its site container.
///
/// * `Ok(Some(path))` — resolved, contained, and a directory.
/// * `Ok(None)` — the declaration explicitly means "the container is the web
///   root". `"."` is accepted for this because it is the natural spelling of
///   "this repository has no separate web root" and a daemon translating an
///   application manifest will emit it verbatim; the empty string is the same
///   for a daemon that rendered an empty template variable.
/// * `Err(reason)` — the operator asked to narrow the web root and we cannot.
///   The caller turns this into [`SiteOverride::unusable`] rather than serving
///   the container, because serving the container is *wider* than what was
///   asked for and is exactly what the declaration existed to prevent.
fn validate_declared_root(
    container: &Path,
    declared: &str,
    site_key: &str,
) -> Result<Option<PathBuf>, &'static str> {
    let Some(trimmed) = normalize_declared(declared) else {
        // "." / "./" / "" all mean "the container is the web root".
        return Ok(None);
    };
    resolve_contained(container, trimmed, site_key, "document_root", Expect::Directory).map(Some)
}

/// Trim a declared path and decide whether it says anything at all.
///
/// `None` means "declared nothing" — the empty string, `"."` or `"./"`. Anything
/// else is returned with trailing slashes stripped, for the containment checks
/// to judge.
///
/// The emptiness test runs on the value **before** trailing slashes are
/// stripped, which matters: `"/"` strips to `""`, and reading that as "declared
/// nothing" would have quietly turned the absolute filesystem root into "serve
/// the container" instead of the rejection it is. All-slash values are handed
/// on so [`resolve_contained`]'s absolute-path check refuses them by name.
fn normalize_declared(declared: &str) -> Option<&str> {
    let trimmed = declared.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == "./" {
        return None;
    }
    let stripped = trimmed.trim_end_matches('/');
    // `"/"`, `"//"`, … — an absolute root, not an absent declaration.
    Some(if stripped.is_empty() { trimmed } else { stripped })
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
///
/// Same `Ok(Some)` / `Ok(None)` / `Err(reason)` contract as
/// [`validate_declared_root`]. A rejected prepend does not *widen* anything the
/// way a rejected document root does, but it is still a key this binary
/// implements whose value it could not honour — and running a preview without
/// the environment it was told to inject is the silent-wrong-answer failure
/// #463 was filed about. It fails closed for consistency and for that reason.
fn validate_declared_prepend(
    container: &Path,
    declared: &str,
    site_key: &str,
) -> Result<Option<PathBuf>, &'static str> {
    // Empty is "declared nothing", matching how `document_root` spells it — a
    // daemon that renders an empty template variable gets a no-op, not a
    // site-down. `"."` is caught by the same helper and then, being a
    // directory, could never have been a prepend anyway.
    let Some(trimmed) = normalize_declared(declared) else {
        return Ok(None);
    };
    resolve_contained(container, trimmed, site_key, "auto_prepend_file", Expect::File).map(Some)
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
) -> Result<PathBuf, &'static str> {
    let reject = |reason: &'static str| {
        tracing::warn!(
            container = %container.display(),
            site = site_key,
            key,
            declared = trimmed,
            reason,
            "per-site override declared a path that was rejected"
        );
        Err::<PathBuf, &'static str>(reason)
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
    Ok(ephpm_config::strip_verbatim_prefix(target))
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
        f.write("auto_prepend_file = \"web\"\n");
        let over = f.load();
        assert_eq!(over.auto_prepend_file, None);
        assert!(over.unusable.is_some(), "a directory is not a script — fail closed");

        // `.` and `./` spell "declared nothing", not "prepend the container".
        for empty in [".", "./"] {
            f.write(&format!("auto_prepend_file = {empty:?}\n"));
            let over = f.load();
            assert_eq!(over.auto_prepend_file, None, "for {empty:?}");
            assert_eq!(over.unusable, None, "for {empty:?}");
        }
    }

    /// `"/"` trims to the empty string, and reading that as "declared nothing"
    /// would silently turn the absolute filesystem root into "serve the
    /// container". It is a rejection, on both keys.
    #[test]
    fn a_bare_slash_is_a_rejection_not_an_absent_declaration() {
        for key in ["document_root", "auto_prepend_file"] {
            for slashes in ["/", "//", " / "] {
                let f = fixture();
                f.write(&format!("{key} = {slashes:?}\n"));
                assert!(
                    f.load().unusable.is_some(),
                    "{key} = {slashes:?} is an absolute root and must be refused, not read as \
                     an absent declaration"
                );
            }
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
    fn rejected_prepend_makes_the_file_unusable() {
        let f = fixture();
        f.write("document_root = \"web\"\nauto_prepend_file = \"../escape.php\"\n");

        let over = f.load();
        assert!(over.unusable.is_some(), "a refused prepend must fail closed");
        // Deliberately NOT handed back: a caller reading `document_root` alone
        // would serve a site whose operator asked for something we could not
        // do, which is the silent wrong answer #463 exists to stop.
        assert_eq!(over.document_root, None);
        assert_eq!(over.auto_prepend_file, None);
    }

    /// **The containment-direction fix.** `document_root` is a *narrowing*
    /// instruction, so falling back to the container when it cannot be applied
    /// serves MORE than the operator asked for — the `vendor/`, `.git` and
    /// `storage/logs/laravel.log` the declaration existed to hide. Every way of
    /// failing to honour a declared root must therefore be `unusable`.
    ///
    /// The second assertion is the one that pins the direction: `document_root`
    /// must be `None`, so no caller can reach the old wider fallback by reading
    /// that field alone.
    #[test]
    fn a_document_root_that_cannot_be_honoured_never_widens_to_the_container() {
        let f = fixture();
        std::fs::write(f.container.join("index.php"), b"<?php").unwrap();
        for bad in ["nope", "..", "../../etc", "/etc", r"C:\Windows", "vendor/../..", "index.php"] {
            f.write(&format!("document_root = {bad:?}\n"));
            let over = f.load();
            assert!(
                over.unusable.is_some(),
                "declared root {bad:?} could not be honoured, so the site must fail closed \
                 rather than serve its whole container"
            );
            assert_eq!(over.document_root, None, "for {bad:?}");
        }
    }

    /// The states that are NOT failures and must keep serving: an absent file,
    /// and a file that explicitly says the container is the web root.
    #[test]
    fn no_declaration_is_not_a_failure() {
        let f = fixture();
        assert_eq!(f.load().unusable, None, "absent file is the documented default");

        for text in ["", "# just a comment\n", "document_root = \".\"\n", "document_root = \"\"\n"]
        {
            f.write(text);
            let over = f.load();
            assert_eq!(over.unusable, None, "for {text:?}");
            assert_eq!(over.document_root, None, "for {text:?}");
        }
    }

    /// A key this binary does not implement *at all* stays in the
    /// forward-compatibility bucket: ignored, reported, site keeps serving.
    /// Failing closed here is what would let a leading provisioning daemon take
    /// a whole fleet off its web roots with one routine deploy.
    #[test]
    fn an_unknown_key_does_not_make_the_file_unusable() {
        let f = fixture();
        f.write("document_root = \"web\"\na_key_from_a_newer_daemon = \"x\"\n");

        let over = f.load();
        assert_eq!(over.unusable, None, "an unimplemented key must not take the site down");
        assert_eq!(over.document_root, Some(resolved(&f.container.join("web"))));
        assert_eq!(over.unknown_keys, vec!["a_key_from_a_newer_daemon".to_string()]);
    }

    /// Worker mode is a server-wide *capability gap*, not a broken file, so it
    /// belongs in the same bucket as an unknown key. Failing closed on it would
    /// take down every site on a worker-mode fleet the moment the provisioning
    /// daemon started writing `auto_prepend_file` — the outage the leniency
    /// argument exists to prevent.
    #[test]
    fn worker_mode_is_a_capability_gap_not_an_unusable_file() {
        let f = fixture();
        f.php("preview-env.php");
        f.write("document_root = \"web\"\nauto_prepend_file = \"preview-env.php\"\n");

        let over = f.load_worker();
        assert_eq!(over.unusable, None, "worker mode must not take the site down");
        assert_eq!(over.auto_prepend_file, None);
        assert_eq!(over.document_root, Some(resolved(&f.container.join("web"))));
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
        assert_eq!(over.unusable, None, "a capability gap is not a broken file");
    }

    // ── Declarations rejected even though the writer is trusted ────────

    #[test]
    fn traversal_declarations_are_rejected() {
        for bad in ["..", "../", "../../etc", "web/../../..", "a/../../b", r"..\..\Windows"] {
            let f = fixture();
            f.write(&format!("document_root = {bad:?}\n"));
            let over = f.load();
            assert_eq!(over.document_root, None, "traversal {bad:?} must be rejected");
            assert!(over.unusable.is_some(), "traversal {bad:?} must also fail closed");
        }
    }

    #[test]
    fn absolute_declarations_are_rejected() {
        for bad in ["/", "/etc", "/etc/passwd", r"C:\Windows", r"\\server\share"] {
            let f = fixture();
            f.write(&format!("document_root = {bad:?}\n"));
            let over = f.load();
            assert_eq!(over.document_root, None, "absolute path {bad:?} must be rejected");
            assert!(over.unusable.is_some(), "absolute path {bad:?} must also fail closed");
        }
    }

    #[test]
    fn declaration_naming_a_file_is_rejected() {
        let f = fixture();
        std::fs::write(f.container.join("index.php"), b"<?php").unwrap();
        f.write("document_root = \"index.php\"\n");
        let over = f.load();
        assert_eq!(over.document_root, None);
        assert!(over.unusable.is_some());
    }

    #[test]
    fn declaration_naming_a_missing_directory_is_rejected() {
        let f = fixture();
        f.write("document_root = \"nope\"\n");
        let over = f.load();
        assert_eq!(over.document_root, None);
        assert!(over.unusable.is_some());
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

        let over = f.load();
        assert_eq!(
            over.document_root, None,
            "a symlink resolving outside the container must be rejected"
        );
        assert!(over.unusable.is_some(), "and must fail closed, not serve the container");
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
    fn malformed_override_takes_the_site_out_of_service() {
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
            let over = f.load();
            assert!(
                over.unusable.is_some(),
                "a half-written file ({text:?}) means the operator asked for something we \
                 cannot read — serving the container instead publishes exactly what the \
                 override was written to hide"
            );
            assert_eq!(over.document_root, None, "for {text:?}");
            assert_eq!(over.auto_prepend_file, None, "for {text:?}");
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
        // landing in the flattened `unknown` table. `preview_auth` is a section
        // (a TOML table), so it takes table syntax; the scalar keys take a
        // string. Either way, nothing must fall into `unknown`.
        for key in KNOWN_KEYS {
            let toml = if *key == "preview_auth" {
                format!("[{key}]\nsession_secret = \"x\"\n")
            } else {
                format!("{key} = \"x\"\n")
            };
            let raw: RawOverride = toml::from_str(&toml).unwrap();
            assert!(
                raw.unknown.is_empty(),
                "{key} is in KNOWN_KEYS but is not a typed field on RawOverride"
            );
        }
        // And nothing outside the list parses as typed.
        let raw: RawOverride = toml::from_str("some_future_key = \"x\"\n").unwrap();
        assert_eq!(raw.unknown.len(), 1);
    }

    // ── [preview_auth] — the access gate (issue #487) ─────────────────────

    /// A 32-byte literal secret + a login URL is a valid gate: the section
    /// resolves to a `preview_gate` config, the site is NOT unusable, and the
    /// config is one the `preview-gate` builtin actually accepts — the check
    /// that keeps this from being a silent no-op knob.
    #[test]
    fn valid_preview_auth_resolves_to_a_working_gate_config() {
        let f = fixture();
        f.write(
            "[preview_auth]\n\
             session_secret = \"0123456789abcdef0123456789abcdef\"\n\
             login_url = \"/auth/github/login\"\n\
             exempt_paths = [\"/auth/github/login\", \"/auth/github/callback\"]\n",
        );
        let over = f.load();
        assert_eq!(over.unusable, None);
        let config = over.preview_gate.expect("a valid section must resolve to a gate config");
        assert_eq!(config["secret"], "0123456789abcdef0123456789abcdef");
        assert_eq!(config["login_url"], "/auth/github/login");
        // The produced config must be accepted by the builtin it feeds — if this
        // fails, site_overrides and the gate have drifted and the knob is inert.
        use ephpm_middleware::Middleware as _;
        ephpm_middleware_builtins::preview_gate::PreviewGate::init(&config)
            .expect("the gate must accept the config site_overrides produced");
    }

    /// `document_root` and `preview_auth` compose: a preview both narrows its
    /// web root AND gates access from one file.
    #[test]
    fn document_root_and_preview_auth_apply_together() {
        let f = fixture();
        f.write(
            "document_root = \"web\"\n\
             [preview_auth]\n\
             session_secret = \"0123456789abcdef0123456789abcdef\"\n\
             login_url = \"/auth/github/login\"\n",
        );
        let over = f.load();
        assert_eq!(over.unusable, None);
        assert_eq!(over.document_root, Some(resolved(&f.container.join("web"))));
        assert!(over.preview_gate.is_some());
    }

    /// A `file:` secret is read and trimmed. This is the recommended form for
    /// keeping the literal out of the (tenant-derived) override file.
    #[test]
    fn preview_auth_reads_a_file_secret() {
        let f = fixture();
        let secret_file = f.container.parent().unwrap().join("gate.secret");
        std::fs::write(&secret_file, "  0123456789abcdef0123456789abcdef\n  ").unwrap();
        f.write(&format!(
            "[preview_auth]\n\
             session_secret = \"file:{}\"\n\
             login_url = \"/auth/github/login\"\n",
            secret_file.display().to_string().replace('\\', "\\\\"),
        ));
        let over = f.load();
        assert_eq!(over.unusable, None);
        assert_eq!(over.preview_gate.expect("gate")["secret"], "0123456789abcdef0123456789abcdef");
    }

    /// Every broken `[preview_auth]` fails **closed** — the site refuses to
    /// serve rather than serve the preview ungated. This is the property the
    /// whole gate turns on: a half-written or misconfigured gate never opens
    /// the preview to the internet.
    #[test]
    fn a_broken_preview_auth_fails_closed_never_ungated() {
        for (label, section) in [
            ("missing secret", "[preview_auth]\nlogin_url = \"/login\"\n"),
            (
                "missing login_url",
                "[preview_auth]\nsession_secret = \"0123456789abcdef0123456789abcdef\"\n",
            ),
            (
                "short secret",
                "[preview_auth]\nsession_secret = \"too-short\"\nlogin_url = \"/login\"\n",
            ),
            ("empty secret", "[preview_auth]\nsession_secret = \"\"\nlogin_url = \"/login\"\n"),
            (
                "env var unset",
                "[preview_auth]\nsession_secret = \"env:EPHPM_TEST_DEFINITELY_UNSET_SECRET_VAR\"\nlogin_url = \"/login\"\n",
            ),
            (
                "file unreadable",
                "[preview_auth]\nsession_secret = \"file:/nonexistent/gate.secret\"\nlogin_url = \"/login\"\n",
            ),
        ] {
            let f = fixture();
            f.write(section);
            let over = f.load();
            assert!(
                over.unusable.is_some(),
                "{label}: a broken gate must fail closed (503), not serve ungated"
            );
            assert!(over.preview_gate.is_none(), "{label}: no gate config on a broken section");
        }
    }

    /// An unknown key *inside* `[preview_auth]` is tolerated (forward-compat
    /// with a newer switchboard), reported with the section prefix, and does
    /// NOT take the gate down — the same leniency the file grants unknown
    /// top-level keys.
    #[test]
    fn an_unknown_key_inside_preview_auth_is_tolerated_and_reported() {
        let f = fixture();
        f.write(
            "[preview_auth]\n\
             session_secret = \"0123456789abcdef0123456789abcdef\"\n\
             login_url = \"/auth/github/login\"\n\
             a_key_from_a_newer_switchboard = true\n",
        );
        let over = f.load();
        assert_eq!(over.unusable, None, "an unimplemented section key must not take the site down");
        assert!(over.preview_gate.is_some());
        assert_eq!(
            over.unknown_keys,
            vec!["preview_auth.a_key_from_a_newer_switchboard".to_string()]
        );
    }

    /// The per-preview gate (issue #487): a valid `repo` is surfaced on
    /// `preview_gate_repo` and — the load-bearing property — is NOT folded into
    /// the gate config the verifier sees. Repo authorization is the issuer's
    /// job, done once at login; the verifier only checks the session.
    #[test]
    fn preview_auth_repo_is_surfaced_separately_never_in_the_gate_config() {
        let f = fixture();
        f.write(
            "[preview_auth]\n\
             session_secret = \"0123456789abcdef0123456789abcdef\"\n\
             login_url = \"/auth/github/login\"\n\
             repo = \"acme/web\"\n",
        );
        let over = f.load();
        assert_eq!(over.unusable, None);
        assert_eq!(over.preview_gate_repo.as_deref(), Some("acme/web"));
        let config = over.preview_gate.expect("a valid section resolves to a gate config");
        assert!(
            config.get("repo").is_none(),
            "the repo must never reach the verifier's config: {config}"
        );
        // And the config the verifier feeds still parses (repo omission is fine).
        use ephpm_middleware::Middleware as _;
        ephpm_middleware_builtins::preview_gate::PreviewGate::init(&config)
            .expect("the gate must accept the config even with repo carried separately");
    }

    /// A `[preview_auth]` with no `repo` still resolves a working gate — the key
    /// is optional (fixed-mode issuers do not need it).
    #[test]
    fn preview_auth_without_repo_still_resolves_a_gate() {
        let f = fixture();
        f.write(
            "[preview_auth]\n\
             session_secret = \"0123456789abcdef0123456789abcdef\"\n\
             login_url = \"/auth/github/login\"\n",
        );
        let over = f.load();
        assert_eq!(over.unusable, None);
        assert!(over.preview_gate.is_some());
        assert_eq!(over.preview_gate_repo, None);
    }

    /// A present-but-malformed `repo` fails the one site **closed** (503), like a
    /// broken `document_root` — never gated against a malformed target, never
    /// served ungated.
    #[test]
    fn a_malformed_preview_auth_repo_fails_closed() {
        for bad in [
            "acme",
            "acme/",
            "/web",
            "acme/web/x",
            "../../etc",
            "acme/we b",
            "a/b/c",
            ".",
            "..",
            "acme/..",
        ] {
            let f = fixture();
            f.write(&format!(
                "[preview_auth]\n\
                 session_secret = \"0123456789abcdef0123456789abcdef\"\n\
                 login_url = \"/auth/github/login\"\n\
                 repo = {bad:?}\n",
            ));
            let over = f.load();
            assert!(over.unusable.is_some(), "repo {bad:?} must fail the site closed");
            assert!(over.preview_gate.is_none(), "repo {bad:?}: no gate on a rejected section");
            assert_eq!(over.preview_gate_repo, None, "repo {bad:?}: no repo surfaced");
        }
    }

    /// `repo` is a **typed** field on the section, not swallowed into the
    /// section's `unknown` bucket — otherwise it would be reported as an unknown
    /// key and silently ignored, the exact silent-no-op class #429/#463 forbid.
    #[test]
    fn preview_auth_repo_is_a_typed_field_not_an_unknown_key() {
        let raw: RawPreviewAuth =
            toml::from_str("session_secret = \"x\"\nrepo = \"acme/web\"\n").unwrap();
        assert!(raw.unknown.is_empty(), "repo must parse as a typed field, not land in `unknown`");
        assert_eq!(raw.repo.as_deref(), Some("acme/web"));
    }

    /// `preview_auth` is a near-miss suggestion target like the scalar keys.
    #[test]
    fn preview_auth_typo_is_suggested() {
        assert_eq!(nearest_known_key("preview_ath"), Some("preview_auth"));
        assert!(understood_keys().contains("preview_auth"));
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
