//! The outbound half: exchanging an OAuth `code` for a token and asking
//! GitHub whether this user may see this preview.
//!
//! **Everything in this module runs exactly once per login.** It is the cold
//! path by construction — nothing here is reachable from a request that
//! already carries a session cookie, and the crate's request routing is
//! written so that it cannot become reachable by accident. See the crate
//! docs, and `lib.rs`'s `route` table.
//!
//! # The calls this module makes
//!
//! In order, per login, against `github_api_base` unless noted:
//!
//! | # | Call | Purpose | Minimum GitHub App permission / OAuth scope |
//! |---|---|---|---|
//! | 1 | `POST {github_base}/login/oauth/access_token` | code → user access token | none (this *is* the grant) |
//! | 2 | `GET /user` | the login name that becomes the token's `sub` | GitHub App: none beyond the grant. OAuth App: `read:user` |
//! | 3a | `GET /repos/{owner}/{name}` | `check = "repo"` | GitHub App: `metadata: read` on the installation. OAuth App: `repo` (private repos) |
//! | 3b | `GET /user/memberships/orgs/{org}` | `check = "org"` | `read:org` |
//! | 3c | `GET /orgs/{org}/teams/{team}/memberships/{login}` | `check = "team"` | `read:org` |
//!
//! Three requests, once, at login. A session then lasts `session_ttl_secs`
//! and costs GitHub nothing.
//!
//! # Why a GitHub App is the recommended shape
//!
//! With a classic OAuth App, seeing a private repository at all requires the
//! `repo` scope — which is read **and write** on **every** repository the
//! user can touch, granted to a preview-hosting service. With a GitHub App,
//! the user-to-server token is bounded by the app's installation: `GET
//! /repos/{owner}/{name}` returns 200 only when the user has access *and* the
//! app is installed there, and no `repo` scope is involved. That is why
//! `scopes` defaults to empty rather than to something that "just works".
//!
//! # Redirects are not followed
//!
//! The client is built with [`reqwest::redirect::Policy::none()`]. A 3xx on
//! the token exchange is an attempt to move a request carrying the client
//! secret somewhere else; a 3xx on an API call is treated as "no access".
//! The visible consequence is that a **renamed** repository stops matching
//! until the config is updated, which is the safe direction.

use std::io::Read as _;
use std::time::Duration;

use crate::config::{Check, Config};

/// Largest response body read from GitHub. Every response this module parses
/// is a small JSON object; the cap turns a hostile or broken endpoint into an
/// error instead of unbounded memory on a request thread.
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;

/// User agent sent on every call. GitHub requires one.
const USER_AGENT: &str = concat!("ephpm-middleware-github-auth/", env!("CARGO_PKG_VERSION"));

/// Failures from the GitHub round trip.
///
/// `Display` is written to be safe to log and safe to show a user: no
/// variant carries a code, a token or a secret. That is enforced by
/// construction — the values simply are not in the type.
#[derive(Debug)]
pub enum GhError {
    /// The client could not be built (TLS or configuration).
    Client(String),
    /// The request never completed (DNS, connect, TLS, timeout).
    Transport {
        /// Which call failed, e.g. `"token exchange"`.
        what: &'static str,
    },
    /// GitHub answered, but not with something usable.
    Status {
        /// Which call.
        what: &'static str,
        /// HTTP status returned.
        status: u16,
    },
    /// The body was not the JSON this module expects.
    Body {
        /// Which call.
        what: &'static str,
    },
    /// GitHub reported an OAuth-level error (its `error` field). The value is
    /// one of GitHub's fixed identifiers, never user data.
    OAuth {
        /// GitHub's `error` identifier, e.g. `bad_verification_code`.
        code: String,
    },
}

impl std::fmt::Display for GhError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(m) => write!(f, "GitHub client could not be built: {m}"),
            Self::Transport { what } => write!(f, "{what} could not reach GitHub"),
            Self::Status { what, status } => write!(f, "{what} returned HTTP {status}"),
            Self::Body { what } => write!(f, "{what} returned an unexpected body"),
            Self::OAuth { code } => write!(f, "GitHub rejected the authorization: {code}"),
        }
    }
}

impl std::error::Error for GhError {}

/// The identity behind an access token.
#[derive(Debug, Clone)]
pub struct User {
    /// GitHub login (the `sub` claim).
    pub login: String,
    /// Numeric account id — stable across renames, which `login` is not.
    pub id: u64,
}

/// Blocking GitHub client, built once at `init`.
pub struct GitHub {
    http: reqwest::blocking::Client,
    github_base: String,
    api_base: String,
    client_id: String,
    client_secret: Vec<u8>,
    trust_anchors: String,
}

/// Build the TLS configuration for the outbound client.
///
/// Two things here are deliberate and neither is stylistic:
///
/// 1. **The crypto provider is named.** This workspace compiles both `ring`
///    and `aws-lc-rs` into the graph, so rustls' `from_crate_features()`
///    returns `None` and a bare `ClientConfig::builder()` panics at runtime
///    (ephpm#241, and the long comment on `ephpm-server`'s
///    `tls::crypto_provider`). `aws_lc_rs` is named rather than `ring`
///    because it is the provider ePHPm is converging on — this module keeps
///    working when `ring` leaves the workspace pin.
/// 2. **Roots are the union of the OS store and the bundled Mozilla set** —
///    the same choice the OTLP exporter makes. The OS store is what lets a
///    GitHub Enterprise Server behind a private CA work at all (and it
///    honours `SSL_CERT_FILE`/`SSL_CERT_DIR`, so no ePHPm-specific trust
///    knob is needed); the bundled set is what keeps `api.github.com`
///    reachable from a distroless image that ships no CA bundle.
///
/// Returns the config and a non-secret description of the trust store, which
/// the caller logs through the host once. A GHES certificate problem is
/// otherwise invisible until the first login fails.
fn tls_config() -> Result<(rustls::ClientConfig, String), GhError> {
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    let (native_added, _ignored) = roots.add_parsable_certificates(native.certs);
    let bundled = webpki_roots::TLS_SERVER_ROOTS.len();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let description = format!("{native_added} OS trust anchors + {bundled} bundled");

    rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| GhError::Client(format!("crypto provider rejected the default TLS versions: {e}")))
    .map(|b| (b.with_root_certificates(roots).with_no_client_auth(), description))
}

impl GitHub {
    /// Build the client.
    ///
    /// # Errors
    ///
    /// [`GhError::Client`] when TLS or the HTTP client cannot be constructed.
    pub fn new(config: &Config) -> Result<Self, GhError> {
        let (tls, trust_anchors) = tls_config()?;
        let timeout = Duration::from_secs(config.http_timeout_secs);

        // `reqwest::blocking::Client::builder().build()` starts a background
        // runtime, and reqwest documents that as a panic when it happens
        // inside one. `init` runs before ePHPm's runtime exists, so this is
        // already safe; building on a plain thread makes it a local
        // guarantee rather than an obligation on whoever calls `init` next.
        // (Per-request *use* is fine from any thread — the blocking client
        // hands work to that background runtime over a channel.)
        let http = std::thread::spawn(move || {
            reqwest::blocking::Client::builder()
                .timeout(timeout)
                .connect_timeout(timeout)
                // See the module docs: a 3xx is never followed.
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(USER_AGENT)
                .use_preconfigured_tls(tls)
                .build()
        })
        .join()
        .map_err(|_| GhError::Client("client builder thread panicked".into()))?
        .map_err(|e| GhError::Client(e.to_string()))?;

        Ok(Self {
            http,
            github_base: config.github_base.clone(),
            api_base: config.github_api_base.clone(),
            client_id: config.client_id.clone(),
            client_secret: config.client_secret.expose().to_vec(),
            trust_anchors,
        })
    }

    /// Non-secret description of the TLS trust store, for the startup log.
    #[must_use]
    pub fn trust_anchors(&self) -> &str {
        &self.trust_anchors
    }

    /// Exchange an authorization `code` for a user access token.
    ///
    /// The returned token is a live credential: it is used for the access
    /// check and then dropped. It is never written to a cookie, a log, or
    /// the issued session — the session carries the *verdict*, not the token.
    ///
    /// # Errors
    ///
    /// See [`GhError`]. GitHub answers OAuth failures with HTTP 200 and an
    /// `error` field, which becomes [`GhError::OAuth`].
    pub fn exchange_code(&self, code: &str, redirect_uri: &str) -> Result<String, GhError> {
        let what = "token exchange";
        let secret = String::from_utf8_lossy(&self.client_secret).into_owned();
        let resp = self
            .http
            .post(format!("{}/login/oauth/access_token", self.github_base))
            .header("Accept", "application/json")
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", secret.as_str()),
                ("code", code),
                ("redirect_uri", redirect_uri),
            ])
            .send()
            .map_err(|_| GhError::Transport { what })?;

        let status = resp.status().as_u16();
        if status != 200 {
            return Err(GhError::Status { what, status });
        }
        let body = read_json(resp, what)?;
        // GitHub reports `bad_verification_code`, `redirect_uri_mismatch`,
        // ... with HTTP 200. Checking `error` first is mandatory.
        if let Some(code) = body.get("error").and_then(serde_json::Value::as_str) {
            return Err(GhError::OAuth { code: sanitize_identifier(code) });
        }
        body.get("access_token")
            .and_then(serde_json::Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
            .ok_or(GhError::Body { what })
    }

    /// `GET /user` — the identity the session will name.
    ///
    /// # Errors
    ///
    /// See [`GhError`].
    pub fn current_user(&self, token: &str) -> Result<User, GhError> {
        let what = "user lookup";
        let body =
            self.api_get(token, "/user", what)?.ok_or(GhError::Status { what, status: 404 })?;
        let login = body
            .get("login")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or(GhError::Body { what })?;
        let id =
            body.get("id").and_then(serde_json::Value::as_u64).ok_or(GhError::Body { what })?;
        Ok(User { login: login.to_owned(), id })
    }

    /// Does `user` satisfy `check`?
    ///
    /// Returns `Ok(false)` for a clean "no" (GitHub answered, the answer was
    /// negative) and `Err` only when the question could not be asked. The
    /// caller must treat `Err` as a denial too — see `lib.rs` — but the two
    /// produce different messages, because "you are not on this repo" and
    /// "GitHub is down" are different problems for the person reading the
    /// page.
    ///
    /// # Errors
    ///
    /// See [`GhError`].
    pub fn check_access(&self, token: &str, check: &Check, user: &User) -> Result<bool, GhError> {
        match check {
            Check::Repo { owner, name } => {
                let what = "repository access check";
                // A user access token only ever sees repositories the user
                // can see, so a 200 here *is* the authorization answer. 404
                // is GitHub's deliberate "exists but not for you" — it does
                // not distinguish absence from denial, and neither do we.
                let Some(body) = self.api_get(token, &format!("/repos/{owner}/{name}"), what)?
                else {
                    return Ok(false);
                };
                // When the field is present, require read. When it is absent
                // (some token types omit it), the 200 stands on its own.
                Ok(body
                    .get("permissions")
                    .and_then(|p| p.get("pull"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true))
            }
            Check::Org { org } => {
                let what = "organisation membership check";
                let Some(body) =
                    self.api_get(token, &format!("/user/memberships/orgs/{org}"), what)?
                else {
                    return Ok(false);
                };
                // `pending` means invited but not joined — not a member.
                Ok(body.get("state").and_then(serde_json::Value::as_str) == Some("active"))
            }
            Check::Team { org, team } => {
                let what = "team membership check";
                // The login is URL-safe: it came from GitHub's own `/user`,
                // and `encode_segment` is applied regardless.
                let path =
                    format!("/orgs/{org}/teams/{team}/memberships/{}", encode_segment(&user.login));
                let Some(body) = self.api_get(token, &path, what)? else {
                    return Ok(false);
                };
                Ok(body.get("state").and_then(serde_json::Value::as_str) == Some("active"))
            }
        }
    }

    /// One authenticated `GET`. `Ok(None)` means 403/404 — a clean "no".
    fn api_get(
        &self,
        token: &str,
        path: &str,
        what: &'static str,
    ) -> Result<Option<serde_json::Value>, GhError> {
        let resp = self
            .http
            .get(format!("{}{path}", self.api_base))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .bearer_auth(token)
            .send()
            .map_err(|_| GhError::Transport { what })?;

        match resp.status().as_u16() {
            200 => Ok(Some(read_json(resp, what)?)),
            // 403 = forbidden, 404 = GitHub's "not visible to you", 302/301 =
            // a redirect we refuse to follow. All of them are "no".
            301 | 302 | 403 | 404 => Ok(None),
            status => Err(GhError::Status { what, status }),
        }
    }
}

/// Read a bounded JSON body.
fn read_json(
    resp: reqwest::blocking::Response,
    what: &'static str,
) -> Result<serde_json::Value, GhError> {
    let mut buf = Vec::new();
    resp.take(MAX_RESPONSE_BYTES).read_to_end(&mut buf).map_err(|_| GhError::Transport { what })?;
    serde_json::from_slice(&buf).map_err(|_| GhError::Body { what })
}

/// Keep only characters GitHub uses in its OAuth error identifiers, so a
/// hostile endpoint cannot inject markup or newlines into a page or a log
/// line through the `error` field.
fn sanitize_identifier(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(64).collect()
}

/// Percent-encode one path segment.
#[must_use]
pub fn encode_segment(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Percent-encode a query-string value (`application/x-www-form-urlencoded`
/// component rules, but with space as `%20` since these go in a URL path
/// query rather than a form body).
#[must_use]
pub fn encode_query(s: &str) -> String {
    encode_segment(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_never_carries_a_credential() {
        // The point of the check: no variant of GhError has a field that
        // could hold a code, a token or a secret, so no Display can leak one.
        let cases = [
            GhError::Client("boom".into()),
            GhError::Transport { what: "token exchange" },
            GhError::Status { what: "token exchange", status: 500 },
            GhError::Body { what: "user lookup" },
            GhError::OAuth { code: "bad_verification_code".into() },
        ];
        for e in cases {
            let rendered = format!("{e} {e:?}");
            assert!(!rendered.contains("secret"));
            assert!(!rendered.contains("access_token"));
        }
    }

    #[test]
    fn oauth_error_identifiers_are_sanitized() {
        assert_eq!(sanitize_identifier("bad_verification_code"), "bad_verification_code");
        assert_eq!(sanitize_identifier("<script>alert(1)</script>"), "scriptalert1script");
        assert_eq!(sanitize_identifier("a\r\nSet-Cookie: x"), "aSetCookiex");
        assert_eq!(sanitize_identifier(&"x".repeat(200)).len(), 64);
    }

    #[test]
    fn tls_names_its_provider_and_never_installs_a_process_default() {
        // A dlopen'd module shares a process with the host. `install_default`
        // is process-global and single-shot, so a module that called it would
        // either lose a race with ePHPm's own TLS setup or win one and change
        // the server's provider out from under it. This module must build a
        // config from a NAMED provider and leave the process default alone.
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_none(),
            "nothing in this crate may install a process-default provider"
        );
        let (_config, description) = tls_config().expect("aws-lc-rs supports the default versions");
        assert!(description.contains("bundled"), "trust store is described for the log");
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_none(),
            "building a ClientConfig must not have installed a process default"
        );
    }

    #[test]
    fn segment_encoding_blocks_traversal_and_query_smuggling() {
        assert_eq!(encode_segment("octocat"), "octocat");
        assert_eq!(encode_segment("../../admin"), "..%2F..%2Fadmin");
        assert_eq!(encode_segment("a?b=c"), "a%3Fb%3Dc");
        assert_eq!(encode_segment("a b"), "a%20b");
        assert_eq!(encode_segment("a#b"), "a%23b");
    }
}
