//! Configuration parsing for the gate.
//!
//! Everything arrives as the `[[middleware]] config` table, serialised to
//! JSON by ePHPm. Parsing is strict and fail-closed: an unusable value is an
//! `Err` out of `init`, which aborts server startup with the message rather
//! than booting a gate that silently lets everyone through.
//!
//! # Where the secrets come from
//!
//! Three values are secrets: `client_secret`, `session_secret` and the
//! optional `bypass_token`. Each accepts either a literal or an
//! **`env:NAME`** indirection that reads the named environment variable at
//! startup. `env:` is the recommended form — `ephpm.toml` is frequently
//! templated, checked in, or rendered into a ConfigMap, and a literal in it
//! is a secret in git.
//!
//! Read [`Secret`] and then the crate-level "Operational cost" section: the
//! OAuth client secret genuinely lives on the node, and that is the price of
//! doing this in-process instead of in an external identity service.

use std::collections::BTreeMap;

use crate::cookie::{CookieAttrs, SameSite, validate_cookie_name};

/// Default reserved path that receives GitHub's OAuth redirect.
pub const DEFAULT_CALLBACK_PATH: &str = "/_ephpm/auth/github/callback";
/// Default reserved path that starts a login.
pub const DEFAULT_LOGIN_PATH: &str = "/_ephpm/auth/github/login";
/// Default session cookie name.
pub const DEFAULT_COOKIE_NAME: &str = "ephpm_session";
/// Default session lifetime: one working day.
pub const DEFAULT_SESSION_TTL: u64 = 8 * 60 * 60;
/// Lifetime of the OAuth `state` cookie — long enough to type a password and
/// answer an MFA prompt, short enough that a stolen one is worthless.
pub const STATE_TTL_SECS: u64 = 600;
/// Default automation-bypass session lifetime.
pub const DEFAULT_BYPASS_TTL: u64 = 60 * 60;
/// Default header carrying the automation bypass token.
pub const DEFAULT_BYPASS_HEADER: &str = "x-ephpm-bypass";
/// Default `iss` claim.
pub const DEFAULT_ISSUER: &str = "ephpm-github-auth";

/// A configured secret.
///
/// The `Debug` impl redacts, so a secret cannot reach a log through a
/// derived `Debug` on some struct that happens to contain one. That is not
/// hypothetical tidiness: `tracing`'s `?value` sigil is one keystroke away
/// from `%value`.
#[derive(Clone)]
pub struct Secret(Vec<u8>);

impl Secret {
    /// The raw bytes, for signing and comparison only.
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Which GitHub relationship grants access to a preview.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    /// The user can read `owner/name`. The default, and the tightest answer
    /// to "does this person have access to the repo this preview is for?".
    Repo {
        /// `owner` segment.
        owner: String,
        /// Repository name.
        name: String,
    },
    /// The user is an *active* member of the organisation.
    Org {
        /// Organisation login.
        org: String,
    },
    /// The user is an *active* member of a specific team.
    Team {
        /// Organisation login.
        org: String,
        /// Team slug.
        team: String,
    },
}

impl Check {
    /// A short, non-secret description for logs and error bodies.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Repo { owner, name } => format!("repo {owner}/{name}"),
            Self::Org { org } => format!("org {org}"),
            Self::Team { org, team } => format!("team {org}/{team}"),
        }
    }
}

/// The gate's fully-resolved configuration.
#[derive(Debug)]
pub struct Config {
    /// OAuth client id (not a secret).
    pub client_id: String,
    /// OAuth client secret.
    pub client_secret: Secret,
    /// HMAC key for the session token. Shared with the hot-path verifier.
    pub session_secret: Secret,
    /// Key for the OAuth `state` cookie, derived from `session_secret`.
    pub state_secret: Secret,

    /// Access check applied when the request's vhost has no `sites` entry.
    pub default_check: Option<Check>,
    /// Per-vhost access checks (`request_vhost_id` → check).
    pub sites: BTreeMap<String, Check>,

    /// Reserved path that starts a login.
    pub login_path: String,
    /// Reserved path that receives GitHub's redirect.
    pub callback_path: String,

    /// Session cookie name. Must match the hot-path verifier's.
    pub cookie_name: String,
    /// OAuth `state` cookie name.
    pub state_cookie_name: String,
    /// Attributes applied to both cookies.
    pub cookie_attrs: CookieAttrs,
    /// Session lifetime, seconds.
    pub session_ttl_secs: u64,

    /// `iss` claim written into issued tokens.
    pub issuer: String,
    /// `aud` claim written into issued tokens, when configured.
    pub audience: Option<String>,

    /// Pre-shared automation bypass token.
    pub bypass_token: Option<Secret>,
    /// Header carrying the bypass token.
    pub bypass_header: String,
    /// Query parameter carrying the bypass token; off unless configured.
    pub bypass_query: Option<String>,
    /// Lifetime of a bypass-issued session, seconds.
    pub bypass_ttl_secs: u64,

    /// OAuth authorize/token host (`https://github.com`, or a GHES host).
    pub github_base: String,
    /// REST API base (`https://api.github.com`, or `https://ghes/api/v3`).
    pub github_api_base: String,
    /// Explicit `redirect_uri`. When unset it is derived per request as
    /// `https://<vhost><callback_path>`.
    pub redirect_uri: Option<String>,
    /// OAuth scopes requested. Empty is correct for a GitHub App.
    pub scopes: Vec<String>,
    /// Timeout for each outbound GitHub call, seconds.
    pub http_timeout_secs: u64,
}

/// Read a string field.
fn opt_str(config: &serde_json::Value, key: &str) -> Result<Option<String>, String> {
    match config.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) if s.is_empty() => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(format!("`{key}` must be a string, got {other}")),
    }
}

fn req_str(config: &serde_json::Value, key: &str) -> Result<String, String> {
    opt_str(config, key)?.ok_or_else(|| format!("`{key}` is required and must be non-empty"))
}

fn opt_bool(config: &serde_json::Value, key: &str, default: bool) -> Result<bool, String> {
    match config.get(key) {
        None | Some(serde_json::Value::Null) => Ok(default),
        Some(serde_json::Value::Bool(b)) => Ok(*b),
        Some(other) => Err(format!("`{key}` must be a boolean, got {other}")),
    }
}

fn opt_u64(config: &serde_json::Value, key: &str, default: u64) -> Result<u64, String> {
    match config.get(key) {
        None | Some(serde_json::Value::Null) => Ok(default),
        Some(v) => v.as_u64().ok_or_else(|| format!("`{key}` must be a positive integer, got {v}")),
    }
}

/// How a secret's `env:NAME` indirection is resolved.
///
/// Injected rather than calling [`std::env::var`] inline so the tests can
/// exercise the indirection without `set_var`, which is `unsafe` in the 2024
/// edition and a genuine data race in a multi-threaded test binary.
pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Resolve a secret, following an `env:NAME` indirection.
fn secret(
    config: &serde_json::Value,
    key: &str,
    env: EnvLookup<'_>,
) -> Result<Option<Secret>, String> {
    let Some(raw) = opt_str(config, key)? else {
        return Ok(None);
    };
    let value = if let Some(var) = raw.strip_prefix("env:") {
        if var.is_empty() {
            return Err(format!("`{key}` is `env:` with no variable name"));
        }
        // The variable NAME is safe to name in an error; the value never is.
        env(var).ok_or_else(|| {
            format!("`{key}` points at environment variable `{var}`, which is not set")
        })?
    } else {
        raw
    };
    if value.is_empty() {
        return Err(format!("`{key}` resolved to an empty value"));
    }
    Ok(Some(Secret(value.into_bytes())))
}

/// Shortest accepted `session_secret`.
///
/// 32 bytes is the HMAC-SHA256 block-security level. Shorter keys are what
/// make an offline forgery attempt on a self-contained token worth
/// attempting, and the whole security of the session rests on this one value.
const MIN_SESSION_SECRET: usize = 32;

/// Parse a `check` selection out of a JSON object (the top-level config or
/// one `sites` entry). Returns `None` when the object names no target.
fn parse_check(v: &serde_json::Value, ctx: &str) -> Result<Option<Check>, String> {
    let kind = opt_str(v, "check")?;
    let repo = opt_str(v, "repo")?;
    let org = opt_str(v, "org")?;
    let team = opt_str(v, "team")?;

    // Infer the kind when it is unambiguous, so the common single-key form
    // (`{ repo = "acme/web" }`) needs no ceremony.
    let kind = match kind.as_deref() {
        Some(k) => k.to_ascii_lowercase(),
        None if team.is_some() => "team".into(),
        None if repo.is_some() => "repo".into(),
        None if org.is_some() => "org".into(),
        None => return Ok(None),
    };

    match kind.as_str() {
        "repo" => {
            let spec = repo.ok_or_else(|| format!("{ctx}: `check = \"repo\"` needs `repo` set"))?;
            let (owner, name) = spec
                .split_once('/')
                .ok_or_else(|| format!("{ctx}: `repo` must be `owner/name`, got {spec:?}"))?;
            if owner.is_empty() || name.is_empty() || name.contains('/') {
                return Err(format!("{ctx}: `repo` must be exactly `owner/name`, got {spec:?}"));
            }
            validate_path_segment(owner, &format!("{ctx}: repo owner"))?;
            validate_path_segment(name, &format!("{ctx}: repo name"))?;
            Ok(Some(Check::Repo { owner: owner.to_owned(), name: name.to_owned() }))
        }
        "org" => {
            let org = org.ok_or_else(|| format!("{ctx}: `check = \"org\"` needs `org` set"))?;
            validate_path_segment(&org, &format!("{ctx}: org"))?;
            Ok(Some(Check::Org { org }))
        }
        "team" => {
            let org = org.ok_or_else(|| format!("{ctx}: `check = \"team\"` needs `org` set"))?;
            let team = team.ok_or_else(|| format!("{ctx}: `check = \"team\"` needs `team` set"))?;
            validate_path_segment(&org, &format!("{ctx}: org"))?;
            validate_path_segment(&team, &format!("{ctx}: team"))?;
            Ok(Some(Check::Team { org, team }))
        }
        other => Err(format!("{ctx}: `check` must be repo, org or team, got {other:?}")),
    }
}

/// Reject anything that would not survive being interpolated into a GitHub
/// API path. This is the one place configured strings become part of an
/// outbound URL, so it is the one place path traversal or a smuggled query
/// could be introduced.
fn validate_path_segment(s: &str, what: &str) -> Result<(), String> {
    let ok = !s.is_empty()
        && s.len() <= 100
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && s != "."
        && s != "..";
    if ok { Ok(()) } else { Err(format!("{what}: {s:?} is not a valid GitHub name")) }
}

/// Validate a reserved path: absolute, no query, no traversal.
fn validate_reserved_path(p: &str, key: &str) -> Result<(), String> {
    if !p.starts_with('/') || p.contains(['?', '#', '\\', ' ']) || !p.is_ascii() {
        return Err(format!("`{key}` must be an absolute ASCII path with no query, got {p:?}"));
    }
    Ok(())
}

/// Validate an `https://…` (or loopback `http://…`) base URL.
///
/// Plaintext is permitted **only** to a loopback literal. That single
/// exception is what lets the integration tests point the module at a stub
/// OAuth server on `127.0.0.1`; it cannot be turned into "plaintext to
/// github.com", which would put the client secret and the access token on
/// the wire in clear.
fn validate_base_url(url: &str, key: &str) -> Result<(), String> {
    let rest = if let Some(r) = url.strip_prefix("https://") {
        r
    } else if let Some(r) = url.strip_prefix("http://") {
        let host = r.split(['/', ':']).next().unwrap_or("");
        if !matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1") {
            return Err(format!(
                "`{key}` may only use http:// for a loopback host (got {url:?}); \
                 plaintext to a real GitHub host would expose the client secret \
                 and the user's access token"
            ));
        }
        r
    } else {
        return Err(format!("`{key}` must start with https:// (got {url:?})"));
    };
    if rest.is_empty() || url.ends_with('/') {
        return Err(format!("`{key}` must have a host and no trailing slash (got {url:?})"));
    }
    if !url.is_ascii() || url.chars().any(|c| c.is_ascii_control()) {
        return Err(format!("`{key}` must be printable ASCII (got {url:?})"));
    }
    Ok(())
}

impl Config {
    /// Parse and validate the mount's config table.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message for any missing, malformed or unsafe
    /// value. Every one of these aborts server startup.
    pub fn parse(config: &serde_json::Value) -> Result<Self, String> {
        Self::parse_with_env(config, &|var| std::env::var(var).ok())
    }

    /// [`Config::parse`] with the `env:` resolver injected.
    ///
    /// # Errors
    ///
    /// As [`Config::parse`].
    #[allow(clippy::too_many_lines, reason = "one flat, auditable validation pass")]
    pub fn parse_with_env(config: &serde_json::Value, env: EnvLookup<'_>) -> Result<Self, String> {
        if !config.is_object() {
            return Err("config must be a table (`config = { client_id = ... }`)".into());
        }

        let client_id = req_str(config, "client_id")?;
        let client_secret =
            secret(config, "client_secret", env)?.ok_or("`client_secret` is required")?;
        let session_secret =
            secret(config, "session_secret", env)?.ok_or("`session_secret` is required")?;
        if session_secret.expose().len() < MIN_SESSION_SECRET {
            return Err(format!(
                "`session_secret` must be at least {MIN_SESSION_SECRET} bytes; the whole \
                 security of an unrevocable, self-contained session token rests on it"
            ));
        }
        let state_secret = Secret(crate::token::derive_key(
            session_secret.expose(),
            crate::token::STATE_KEY_LABEL,
        ));

        // Access targets. `sites` maps a vhost id to its own check, which is
        // what lets one mount serve a fleet of per-PR preview hostnames.
        let mut sites = BTreeMap::new();
        match config.get("sites") {
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::Object(map)) => {
                for (host, entry) in map {
                    let host_key = normalize_vhost(host);
                    validate_vhost(&host_key)?;
                    let check = parse_check(entry, &format!("sites.{host}"))?
                        .ok_or_else(|| format!("sites.{host}: no repo/org/team configured"))?;
                    sites.insert(host_key, check);
                }
            }
            Some(other) => return Err(format!("`sites` must be a table, got {other}")),
        }
        let default_check = parse_check(config, "config")?;
        if default_check.is_none() && sites.is_empty() {
            return Err(
                "no access target configured: set `repo` (or `org`/`team`), or a `sites` table \
                 mapping each vhost to one. A gate with nothing to check would authenticate \
                 every GitHub user in the world."
                    .into(),
            );
        }

        let login_path =
            opt_str(config, "login_path")?.unwrap_or_else(|| DEFAULT_LOGIN_PATH.to_owned());
        let callback_path =
            opt_str(config, "callback_path")?.unwrap_or_else(|| DEFAULT_CALLBACK_PATH.to_owned());
        validate_reserved_path(&login_path, "login_path")?;
        validate_reserved_path(&callback_path, "callback_path")?;
        if login_path == callback_path {
            return Err("`login_path` and `callback_path` must differ".into());
        }

        let cookie_name =
            opt_str(config, "cookie_name")?.unwrap_or_else(|| DEFAULT_COOKIE_NAME.to_owned());
        validate_cookie_name(&cookie_name)?;
        let state_cookie_name =
            opt_str(config, "state_cookie_name")?.unwrap_or_else(|| format!("{cookie_name}_oauth"));
        validate_cookie_name(&state_cookie_name)?;
        if state_cookie_name == cookie_name {
            return Err("`state_cookie_name` must differ from `cookie_name`".into());
        }

        let cookie_path = opt_str(config, "cookie_path")?.unwrap_or_else(|| "/".to_owned());
        validate_reserved_path(&cookie_path, "cookie_path")?;
        let same_site =
            SameSite::parse(opt_str(config, "cookie_samesite")?.as_deref().unwrap_or("Lax"))?;
        let secure = opt_bool(config, "cookie_secure", true)?;
        if same_site == SameSite::None && !secure {
            return Err("`cookie_samesite = \"None\"` requires `cookie_secure = true`".into());
        }

        let session_ttl_secs = opt_u64(config, "session_ttl_secs", DEFAULT_SESSION_TTL)?;
        if !(60..=7 * 24 * 60 * 60).contains(&session_ttl_secs) {
            return Err(
                "`session_ttl_secs` must be between 60 and 604800: the token cannot be revoked \
                 before it expires, so its lifetime is the blast radius of a stolen cookie"
                    .into(),
            );
        }

        let bypass_token = secret(config, "bypass_token", env)?;
        if let Some(t) = &bypass_token
            && t.expose().len() < MIN_SESSION_SECRET
        {
            return Err(format!(
                "`bypass_token` must be at least {MIN_SESSION_SECRET} bytes — it is a password \
                 that skips GitHub entirely"
            ));
        }
        let bypass_ttl_secs = opt_u64(config, "bypass_ttl_secs", DEFAULT_BYPASS_TTL)?;
        if !(60..=7 * 24 * 60 * 60).contains(&bypass_ttl_secs) {
            return Err("`bypass_ttl_secs` must be between 60 and 604800".into());
        }
        let bypass_header = opt_str(config, "bypass_header")?
            .unwrap_or_else(|| DEFAULT_BYPASS_HEADER.to_owned())
            .to_ascii_lowercase();
        let bypass_query = opt_str(config, "bypass_query")?;

        let github_base =
            opt_str(config, "github_base")?.unwrap_or_else(|| "https://github.com".to_owned());
        let github_api_base = opt_str(config, "github_api_base")?
            .unwrap_or_else(|| "https://api.github.com".to_owned());
        validate_base_url(&github_base, "github_base")?;
        validate_base_url(&github_api_base, "github_api_base")?;

        let redirect_uri = opt_str(config, "redirect_uri")?;
        if let Some(uri) = &redirect_uri {
            let base = uri.split(['?', '#']).next().unwrap_or(uri);
            validate_base_url(base, "redirect_uri")?;
            if !base.ends_with(&callback_path) {
                return Err(format!(
                    "`redirect_uri` must end with `callback_path` ({callback_path}); GitHub \
                     redirects there and only that path is handled"
                ));
            }
        }

        let scopes = match config.get("scopes") {
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| format!("`scopes` entries must be strings, got {v}"))
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(other) => return Err(format!("`scopes` must be an array, got {other}")),
        };
        for s in &scopes {
            if !s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-')) {
                return Err(format!("`scopes` entry {s:?} contains unexpected characters"));
            }
        }

        let http_timeout_secs = opt_u64(config, "http_timeout_secs", 10)?;
        if !(1..=60).contains(&http_timeout_secs) {
            return Err(
                "`http_timeout_secs` must be between 1 and 60 — `invoke` is synchronous, so this \
                 is how long one request thread can be parked on GitHub"
                    .into(),
            );
        }

        Ok(Self {
            client_id,
            client_secret,
            session_secret,
            state_secret,
            default_check,
            sites,
            login_path,
            callback_path,
            cookie_name,
            state_cookie_name,
            cookie_attrs: CookieAttrs { path: cookie_path, secure, same_site },
            session_ttl_secs,
            issuer: opt_str(config, "issuer")?.unwrap_or_else(|| DEFAULT_ISSUER.to_owned()),
            audience: opt_str(config, "audience")?,
            bypass_token,
            bypass_header,
            bypass_query,
            bypass_ttl_secs,
            github_base,
            github_api_base,
            redirect_uri,
            scopes,
            http_timeout_secs,
        })
    }

    /// The access check that applies to `vhost`.
    ///
    /// `vhost` must already have been through [`normalize_vhost`]; the keys
    /// of `sites` were normalised at parse time, so anything else silently
    /// misses.
    ///
    /// When a `sites` table is configured it is authoritative: a vhost with
    /// no entry gets **no** check and therefore no access, even if a
    /// top-level `repo` is also set. Falling back would mean a hostname
    /// nobody mapped quietly inherits some other tenant's rule.
    #[must_use]
    pub fn check_for(&self, vhost: &str) -> Option<&Check> {
        if self.sites.is_empty() {
            return self.default_check.as_ref();
        }
        self.sites.get(vhost)
    }

    /// Paths the gate owns, for the return-to loop check.
    #[must_use]
    pub fn reserved_paths(&self) -> [&str; 2] {
        [self.login_path.as_str(), self.callback_path.as_str()]
    }
}

/// Canonicalise the middleware ABI's `request_vhost_id` before it is used as
/// a key or written into a token.
///
/// **This is not cosmetic.** `request_vhost_id` is the router's `SERVER_NAME`
/// — the raw `Host` header with the port split off and *nothing else*. It is
/// **not** lowercased, the FQDN-root dot is not stripped, and it is not the
/// canonical site key that `Router::resolve_site` produces. `Host` is
/// case-insensitive per RFC 9110 and entirely client-controlled, so without
/// this a `sites` lookup misses on `Host: PR-1.Preview.Test`, and — worse —
/// two spellings of the same host would mint sessions with two different
/// `site` claims and two different derived `redirect_uri`s.
///
/// The port is deliberately **not** split off here: the server already
/// removed it, and splitting on `:` would corrupt an IPv6 literal
/// (`[::1]` → `[`). If a future ABI change reintroduced the port, the key
/// would simply stop matching, which fails closed.
///
/// `ephpm-server`'s own `normalize_host_key` is `pub(crate)`, so a module
/// cannot call it; the duplication is forced.
#[must_use]
pub fn normalize_vhost(raw: &str) -> String {
    raw.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Reject a vhost id that could not appear in a URL authority.
///
/// The vhost is interpolated into the derived `redirect_uri`, so it is
/// attacker-influenced input (it comes from the `Host` header) heading for an
/// outbound URL.
///
/// # Errors
///
/// Returns a message when the name is empty, over-long, non-ASCII, or
/// contains anything outside the host/port character set.
pub fn validate_vhost(host: &str) -> Result<(), String> {
    let ok = !host.is_empty()
        && host.len() <= 253
        && host.is_ascii()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | ':' | '[' | ']'));
    if ok { Ok(()) } else { Err(format!("{host:?} is not a usable virtual-host name")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> serde_json::Value {
        serde_json::json!({
            "client_id": "Iv1.abc",
            "client_secret": "shhh",
            "session_secret": "0123456789abcdef0123456789abcdef",
            "repo": "acme/web",
        })
    }

    fn parse(v: serde_json::Value) -> Result<Config, String> {
        Config::parse(&v)
    }

    #[test]
    fn defaults_are_the_documented_ones() {
        let c = parse(base()).expect("parse");
        assert_eq!(c.login_path, DEFAULT_LOGIN_PATH);
        assert_eq!(c.callback_path, DEFAULT_CALLBACK_PATH);
        assert_eq!(c.cookie_name, DEFAULT_COOKIE_NAME);
        assert_eq!(c.state_cookie_name, "ephpm_session_oauth");
        assert_eq!(c.session_ttl_secs, DEFAULT_SESSION_TTL);
        assert!(c.cookie_attrs.secure);
        assert_eq!(c.cookie_attrs.same_site, SameSite::Lax);
        assert_eq!(c.cookie_attrs.path, "/");
        assert_eq!(c.github_base, "https://github.com");
        assert_eq!(c.github_api_base, "https://api.github.com");
        assert_eq!(c.issuer, DEFAULT_ISSUER);
        assert!(c.scopes.is_empty(), "empty scopes is correct for a GitHub App");
        assert_eq!(c.http_timeout_secs, 10);
        assert_eq!(c.default_check, Some(Check::Repo { owner: "acme".into(), name: "web".into() }));
    }

    #[test]
    fn required_fields_are_required() {
        for missing in ["client_id", "client_secret", "session_secret"] {
            let mut v = base();
            v.as_object_mut().expect("object").remove(missing);
            assert!(parse(v).is_err(), "{missing} must be required");
        }
        // No target at all.
        let mut v = base();
        v.as_object_mut().expect("object").remove("repo");
        let err = parse(v).expect_err("a gate with no target must not start");
        assert!(err.contains("every GitHub user in the world"));
    }

    #[test]
    fn weak_session_secret_is_refused() {
        let mut v = base();
        v["session_secret"] = serde_json::json!("too-short");
        assert!(parse(v).expect_err("weak secret").contains("at least 32 bytes"));
    }

    #[test]
    fn env_indirection_reads_the_variable() {
        let env = |var: &str| {
            (var == "GH_SESSION_SECRET").then(|| "0123456789abcdef0123456789abcdef".to_owned())
        };
        let mut v = base();
        v["session_secret"] = serde_json::json!("env:GH_SESSION_SECRET");
        let c = Config::parse_with_env(&v, &env).expect("parse");
        assert_eq!(c.session_secret.expose(), b"0123456789abcdef0123456789abcdef");

        let mut v = base();
        v["session_secret"] = serde_json::json!("env:NOT_SET_ANYWHERE");
        let err = Config::parse_with_env(&v, &env).expect_err("unset var");
        assert!(err.contains("is not set"));
        assert!(err.contains("NOT_SET_ANYWHERE"), "the variable NAME is safe to report");

        // `env:` with no name is a config error, not a lookup of "".
        let mut v = base();
        v["client_secret"] = serde_json::json!("env:");
        assert!(Config::parse_with_env(&v, &env).is_err());
    }

    #[test]
    fn secret_debug_is_redacted() {
        let c = parse(base()).expect("parse");
        let rendered = format!("{:?}", c.client_secret);
        assert_eq!(rendered, "Secret(<redacted>)");
        // And through the whole struct, which derives Debug.
        let whole = format!("{c:?}");
        assert!(!whole.contains("shhh"), "a secret must not reach a Debug rendering");
        assert!(!whole.contains("0123456789abcdef"));
    }

    #[test]
    fn plaintext_github_base_is_refused_except_on_loopback() {
        for bad in ["http://github.com", "http://evil.test", "ftp://x", "github.com", "https://x/"]
        {
            let mut v = base();
            v["github_base"] = serde_json::json!(bad);
            assert!(parse(v).is_err(), "{bad:?} must be refused");
        }
        for ok in ["http://127.0.0.1:9000", "http://localhost:1", "https://ghe.corp.example"] {
            let mut v = base();
            v["github_base"] = serde_json::json!(ok);
            assert!(parse(v).is_ok(), "{ok:?} must be accepted");
        }
    }

    #[test]
    fn repo_spec_must_be_owner_slash_name() {
        for bad in ["acme", "acme/web/extra", "/web", "acme/", "../../etc", "acme/we b"] {
            let mut v = base();
            v["repo"] = serde_json::json!(bad);
            assert!(parse(v).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn org_and_team_checks_parse() {
        let mut v = base();
        v.as_object_mut().expect("object").remove("repo");
        v["org"] = serde_json::json!("acme");
        assert_eq!(
            parse(v.clone()).expect("org").default_check,
            Some(Check::Org { org: "acme".into() })
        );
        v["team"] = serde_json::json!("platform");
        assert_eq!(
            parse(v).expect("team").default_check,
            Some(Check::Team { org: "acme".into(), team: "platform".into() })
        );
    }

    #[test]
    fn sites_table_is_authoritative_when_present() {
        let mut v = base();
        v["sites"] = serde_json::json!({
            "pr-1.preview.example.com": { "repo": "acme/web" },
            "PR-2.preview.example.com": { "org": "acme" },
        });
        let c = parse(v).expect("parse");
        assert_eq!(
            c.check_for("pr-1.preview.example.com"),
            Some(&Check::Repo { owner: "acme".into(), name: "web".into() })
        );
        // A `sites` KEY written in mixed case is normalised at parse time.
        assert_eq!(
            c.check_for("pr-2.preview.example.com"),
            Some(&Check::Org { org: "acme".into() })
        );
        // An unmapped vhost gets nothing — it does NOT inherit the top-level
        // `repo`, which is also set in this config.
        assert_eq!(c.check_for("pr-99.preview.example.com"), None);
    }

    #[test]
    fn vhost_normalisation_closes_the_case_bypass() {
        // `request_vhost_id` is the RAW `Host` header. Without normalisation
        // a client picks its own key by changing a letter's case, and mints
        // sessions whose `site` claim disagrees with everyone else's.
        for raw in [
            "PR-1.Preview.Example.COM",
            "pr-1.preview.example.com.",
            "  pr-1.preview.example.com  ",
            "PR-1.PREVIEW.EXAMPLE.COM.",
        ] {
            assert_eq!(normalize_vhost(raw), "pr-1.preview.example.com", "{raw:?}");
        }
        // An IPv6 literal survives: the port is already gone, and splitting
        // on ':' here would truncate it to "[".
        assert_eq!(normalize_vhost("[::1]"), "[::1]");
        assert!(validate_vhost(&normalize_vhost("[::1]")).is_ok());

        let mut v = base();
        v["sites"] = serde_json::json!({ "PR-1.Preview.Example.COM": { "repo": "acme/web" } });
        let c = parse(v).expect("parse");
        assert!(c.check_for(&normalize_vhost("pr-1.PREVIEW.example.com")).is_some());
        assert!(c.check_for(&normalize_vhost("PR-1.preview.example.com.")).is_some());
    }

    #[test]
    fn session_ttl_is_bounded() {
        for bad in [0u64, 59, 7 * 24 * 60 * 60 + 1] {
            let mut v = base();
            v["session_ttl_secs"] = serde_json::json!(bad);
            assert!(parse(v).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn reserved_paths_must_be_absolute_and_distinct() {
        for (k, bad) in [
            ("login_path", "no-slash"),
            ("callback_path", "/cb?x=1"),
            ("login_path", "/a b"),
            ("callback_path", "/a\\b"),
        ] {
            let mut v = base();
            v[k] = serde_json::json!(bad);
            assert!(parse(v).is_err(), "{k}={bad:?} must be refused");
        }
        let mut v = base();
        v["login_path"] = serde_json::json!("/same");
        v["callback_path"] = serde_json::json!("/same");
        assert!(parse(v).is_err());
    }

    #[test]
    fn redirect_uri_must_end_at_the_callback_path() {
        let mut v = base();
        v["redirect_uri"] = serde_json::json!("https://preview.example.com/somewhere-else");
        assert!(parse(v.clone()).is_err());
        v["redirect_uri"] =
            serde_json::json!(format!("https://preview.example.com{DEFAULT_CALLBACK_PATH}"));
        assert!(parse(v).is_ok());
    }

    #[test]
    fn samesite_none_requires_secure() {
        let mut v = base();
        v["cookie_samesite"] = serde_json::json!("None");
        v["cookie_secure"] = serde_json::json!(false);
        assert!(parse(v).is_err());
    }

    #[test]
    fn weak_bypass_token_is_refused() {
        let mut v = base();
        v["bypass_token"] = serde_json::json!("hunter2");
        assert!(parse(v).expect_err("weak bypass").contains("at least 32 bytes"));
    }

    #[test]
    fn vhost_validation() {
        assert!(validate_vhost("pr-1.preview.example.com").is_ok());
        assert!(validate_vhost("localhost:8080").is_ok());
        for bad in ["", "has space", "evil.test/path", "a@b", "caf\u{e9}.test", "x\r\ny"] {
            assert!(validate_vhost(bad).is_err(), "{bad:?}");
        }
    }
}
