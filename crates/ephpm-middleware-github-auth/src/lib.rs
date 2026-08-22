//! `github-auth` — a GitHub OAuth login gate for ePHPm, shipped as a
//! `dlopen`'d native middleware module.
//!
//! It answers one question for a private preview site: *does the person at
//! this browser have access, on GitHub, to the thing this preview is for?*
//! It answers it **once**, at login, and then issues a stateless signed
//! session that every later request is judged by locally.
//!
//! # This module is the cold path. It is half of a pair.
//!
//! There are two jobs here and they are deliberately in two modules:
//!
//! | | Cold path — **this module** | Hot path — the session verifier |
//! |---|---|---|
//! | Runs on | login, callback, and the first request of a session | every request |
//! | Talks to GitHub | yes, ~3 calls | **never** |
//! | Owns | issuing tokens | the token format and its verification |
//! | Verdict when unhappy | `RESPOND` 302 → GitHub | `RESPOND` 302 → `login_path` |
//!
//! Mount them in that order and they compose into a complete gate. The
//! verifier this is designed to pair with is the **`session-cookie` builtin**
//! (ephpm#388), which performs exactly the HS256 verification this module's
//! tokens are minted for:
//!
//! ```toml
//! [[middleware]]
//! library = "/opt/ephpm/modules/libgithub_auth.so"
//! order   = 10                       # cold path: owns the reserved paths
//! config  = { client_id = "Iv1.…", client_secret = "env:GH_CLIENT_SECRET",
//!             session_secret = "env:EPHPM_SESSION_SECRET", repo = "acme/web" }
//!
//! [[middleware]]
//! library = "session-cookie"          # hot path: verifies, redirects to login
//! order   = 20
//! config  = { secret = "env:EPHPM_SESSION_SECRET", cookie = "ephpm_session",
//!             login_url = "/_ephpm/auth/github/login",
//!             return_to_param = "next" }
//! ```
//!
//! Exactly three things have to line up: the HMAC key
//! (`session_secret` ↔ `secret`), the cookie name (`cookie_name` ↔ `cookie`,
//! whose defaults already match), and the login path (`login_path` ↔
//! `login_url`, with `return_to_param = "next"` because this module reads the
//! return-to from `?next=`).
//!
//! **This module performs no signature verification of a session cookie, by
//! design.** On a request that already carries the session cookie it returns
//! `CONTINUE` immediately and lets the verifier decide. Which leads to the
//! one thing an operator must not get wrong:
//!
//! > **Mounted alone, this module is not an authenticator.** It stops a
//! > request that has *no* cookie, but it does not — and must not — judge one
//! > that has a cookie of any kind. It logs that fact at every startup. Mount
//! > the verifier.
//!
//! Splitting it this way is not ceremony. It is what keeps a network call off
//! the hot path structurally rather than by discipline: the code that can
//! reach GitHub is only reachable from two fixed paths and from a request
//! with no session cookie at all, and [`GithubAuth::route`] is a flat,
//! testable statement of exactly that.
//!
//! # The flow
//!
//! | Request | Verdict | Cost |
//! |---|---|---|
//! | no session cookie | `RESPOND` 302 → GitHub `authorize`, `Set-Cookie` state | no network |
//! | `GET {callback_path}?code=…&state=…` | `RESPOND` 302 → return-to, `Set-Cookie` session | 3 GitHub calls |
//! | `GET {login_path}` | `RESPOND` 302 → GitHub `authorize` | no network |
//! | valid bypass token, no cookie | `RESPOND` 302 → same path, `Set-Cookie` session | no network |
//! | session cookie present | `CONTINUE` | **no network, no PHP-side work** |
//!
//! PHP does not run on any of the `RESPOND` rows — the middleware chain is
//! evaluated before PHP dispatch and before the request body is read.
//!
//! # Sessions are self-contained
//!
//! The session is an HS256 JWT (see [`token`]) carrying its subject, the
//! vhost it was issued for, how it was obtained, and an expiry. There is no
//! server-side session record: restarts do not log anyone out, a second node
//! needs no replication, and the module never touches the middleware KV
//! surface — which is process-global rather than per-site (ephpm#376) and
//! therefore the wrong place for anything tenant-scoped.
//!
//! The cost of that, stated plainly: **an issued session cannot be revoked
//! before it expires.** Rotating `session_secret` invalidates every session
//! at once and is the only revocation available. `session_ttl_secs` is
//! therefore the blast radius of a stolen cookie, and it defaults to eight
//! hours.
//!
//! # Share links, for free
//!
//! Issuance and verification are separate on purpose, and issuance is not
//! privileged: anything holding `session_secret` can mint an acceptable
//! token. A hosting control plane that wants to give a designer with no
//! GitHub account a two-hour link needs **no change to this module** — it
//! mints the same HS256 JWT with `via: "share"` and a short `exp`, and hands
//! out `https://<preview>/?…` with the cookie set by its own redirect. The
//! claim set is documented under [`GithubAuth::session_claims`]; a share
//! token differs from a login token only in `sub` and `via`.
//!
//! # Automation bypass
//!
//! Set `bypass_token` and CI can reach a protected preview without a browser.
//! Presenting it (header `bypass_header`, default `x-ephpm-bypass`) yields
//! *the same kind of session* — a 302 and a `Set-Cookie` — rather than a
//! special-cased pass-through, so Playwright and Lighthouse follow one extra
//! redirect on their first request and behave normally afterwards. Designing
//! it now rather than retrofitting is the whole point: a bypass bolted on
//! later tends to become a header that skips the gate entirely on every
//! request, which is a second, weaker authenticator.
//!
//! # Operational cost: the client secret lives on the node
//!
//! Doing OAuth in-process means the OAuth **client secret** is on every node
//! that serves previews, readable by the ePHPm process. That is the real
//! price of not running a separate identity service, and it should be
//! supplied as `client_secret = "env:GH_CLIENT_SECRET"` from a secret store,
//! never as a literal in a templated `ephpm.toml`.
//!
//! Blast radius if a node is compromised: the attacker can impersonate the
//! OAuth app — i.e. complete an authorization flow for any user who can be
//! induced to click an authorize link — and, holding `session_secret`, can
//! mint sessions for any user and any site this gate protects. They cannot
//! read a user's GitHub data without that user authorizing, and no long-lived
//! GitHub token is stored: the access token is used during the login and
//! dropped. Rotate `client_secret` and `session_secret` together after any
//! node compromise.
//!
//! # Why a `dlopen`'d module and not a builtin
//!
//! The gate needs an HTTP client and a TLS stack. Compiling those into every
//! ePHPm binary is real weight for a feature most deployments do not use, and
//! it would put a second TLS surface next to the one ephpm#241/#368 is busy
//! converging onto a single crypto provider. As a separate `cdylib` none of
//! it is in the `ephpm` binary — `cargo build -p ephpm` does not compile a
//! line of this crate.
//!
//! The trade-off is explicit: **a fully static (musl) ePHPm cannot `dlopen`,
//! so it cannot use this module.** The stock Linux release is glibc-dynamic
//! and can; a custom `-musl` build cannot, and would need this compiled in as
//! a builtin instead.
//!
//! # Requirement: the reserved paths must reach PHP routing
//!
//! ePHPm evaluates the middleware chain on the **PHP-bound** path, so
//! `login_path` and `callback_path` must resolve to a PHP script for this
//! module to see them. With ePHPm's default
//! `fallback = ["$uri", "$uri/", "/index.php?$query_string"]` and a front
//! controller at `index.php` — WordPress, Laravel, Symfony, and every preview
//! shape this was written for — that is automatic, and PHP still never runs
//! because the chain short-circuits first. A site with **no** `index.php` and
//! a `=404` fallback would 404 the callback before the module is consulted.
//! [`GithubAuth::describe`] repeats this at startup.

pub mod config;
pub mod cookie;
pub mod github;
pub mod redirect;
pub mod token;

use std::sync::atomic::{AtomicBool, Ordering};

use ephpm_middleware::abi::{LOG_ERROR, LOG_INFO, LOG_WARN};
use ephpm_middleware::{Middleware, Request, Response};

use crate::config::{Check, Config, STATE_TTL_SECS};
use crate::cookie::{read_cookie, set_cookie};
use crate::github::{GhError, GitHub, encode_query};

/// What the gate decided to do with a request, before any work is done.
///
/// Extracted from [`GithubAuth::invoke`] so the routing rule — *which
/// requests can reach the network* — is a value a test can assert on rather
/// than a shape inferred from control flow.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    /// Start a login. Never touches the network.
    StartLogin {
        /// Where to send the user after a successful login.
        return_to: String,
    },
    /// Handle GitHub's redirect. **The only variant that reaches GitHub.**
    Callback,
    /// A valid automation bypass token was presented. No network.
    Bypass,
    /// A session cookie is present: hand off to the verifier. No network.
    HasSession,
}

/// The gate.
pub struct GithubAuth {
    config: Config,
    github: GitHub,
    /// Startup banner is emitted on the first request, because `init` has no
    /// usable host logger of its own.
    banner_done: AtomicBool,
}

impl GithubAuth {
    /// Decide what to do with a request from its path, query and cookies
    /// alone.
    ///
    /// Pure, and pure on purpose: this is the function that makes
    /// "no network call on the hot path" checkable. Only [`Route::Callback`]
    /// leads to `github.rs`, and it is reachable only from the exact
    /// configured `callback_path`.
    #[must_use]
    pub fn route(&self, path: &str, query: &str, cookies: &str, bypass: Option<&str>) -> Route {
        let reserved = self.config.reserved_paths();

        if path == self.config.callback_path {
            return Route::Callback;
        }
        if path == self.config.login_path {
            // `?next=` lets the verifier hand the original destination over
            // when it redirects an expired session here.
            let next = query_param(query, "next").unwrap_or_default();
            return Route::StartLogin { return_to: redirect::sanitize_return_to(&next, &reserved) };
        }
        // A request that already carries a session belongs to the verifier —
        // checked before the bypass so an automation client mints once and
        // then behaves like any other client.
        if read_cookie(cookies, &self.config.cookie_name).is_some_and(|v| !v.is_empty()) {
            return Route::HasSession;
        }
        if let (Some(expected), Some(presented)) = (self.config.bypass_token.as_ref(), bypass)
            && token::constant_time_eq(
                self.config.state_secret.expose(),
                presented.as_bytes(),
                expected.expose(),
            )
        {
            return Route::Bypass;
        }
        Route::StartLogin { return_to: redirect::sanitize_return_to(&here(path, query), &reserved) }
    }

    /// The claim set of an issued session.
    ///
    /// | claim | meaning |
    /// |---|---|
    /// | `iss` | `issuer` (default `ephpm-github-auth`) |
    /// | `sub` | GitHub login, or the bypass subject |
    /// | `aud` | `audience`, when configured |
    /// | `exp` / `iat` | expiry / issue time, seconds since the epoch |
    /// | `site` | the vhost this session is for — a session for one preview must not open another |
    /// | `via` | `"github"` or `"bypass"`; an external minter uses `"share"` |
    /// | `gh_id` | numeric GitHub account id (stable across renames), for `via = "github"` |
    /// | `check` | the rule that was satisfied, e.g. `repo acme/web` — for audit |
    #[must_use]
    pub fn session_claims(
        &self,
        subject: &str,
        vhost: &str,
        via: &str,
        gh_id: Option<u64>,
        check: Option<&Check>,
        now: u64,
        ttl: u64,
    ) -> serde_json::Value {
        let mut claims = serde_json::json!({
            "iss": self.config.issuer,
            "sub": subject,
            "iat": now,
            "exp": now.saturating_add(ttl),
            "site": vhost,
            "via": via,
        });
        if let Some(aud) = &self.config.audience {
            claims["aud"] = serde_json::Value::String(aud.clone());
        }
        if let Some(id) = gh_id {
            claims["gh_id"] = serde_json::Value::from(id);
        }
        if let Some(c) = check {
            claims["check"] = serde_json::Value::String(c.describe());
        }
        claims
    }

    /// Build the `redirect_uri` for this request.
    fn redirect_uri(&self, vhost: &str) -> String {
        self.config
            .redirect_uri
            .clone()
            .unwrap_or_else(|| format!("https://{vhost}{}", self.config.callback_path))
    }

    /// 302 to GitHub's authorize endpoint, with a fresh signed `state`.
    fn start_login(&self, req: &Request<'_>, vhost: &str, return_to: &str, now: u64) -> Response {
        let Some(check) = self.config.check_for(vhost) else {
            log(
                req,
                LOG_WARN,
                &format!(
                    "github-auth: refusing login for vhost {vhost:?} — it has no `sites` entry and \
                 no default target, so there is nothing to check access against"
                ),
            );
            return deny(403, "This preview is not configured for GitHub access control.");
        };

        let nonce = token::random_nonce();
        let state_claims = serde_json::json!({
            "n": nonce,
            "rt": return_to,
            "v": vhost,
            "exp": now.saturating_add(STATE_TTL_SECS),
        });
        let Some(state_cookie) = token::mint(self.config.state_secret.expose(), &state_claims)
        else {
            log(req, LOG_ERROR, "github-auth: could not mint the OAuth state token");
            return deny(500, "Login could not be started.");
        };

        let mut url = format!(
            "{}/login/oauth/authorize?client_id={}&redirect_uri={}&state={}&response_type=code",
            self.config.github_base,
            encode_query(&self.config.client_id),
            encode_query(&self.redirect_uri(vhost)),
            encode_query(&nonce),
        );
        if !self.config.scopes.is_empty() {
            url.push_str("&scope=");
            url.push_str(&encode_query(&self.config.scopes.join(" ")));
        }

        log(
            req,
            LOG_INFO,
            &format!(
                "github-auth: starting login for vhost {vhost:?} against {}",
                check.describe()
            ),
        );
        redirect_to(&url).header(
            "Set-Cookie",
            set_cookie(
                &self.config.state_cookie_name,
                &state_cookie,
                i64::try_from(STATE_TTL_SECS).unwrap_or(600),
                &self.config.cookie_attrs,
            ),
        )
    }

    /// Handle GitHub's redirect: verify `state`, exchange the code, check
    /// access, issue the session.
    #[allow(clippy::too_many_lines, reason = "one linear flow, each step guarded")]
    fn handle_callback(&self, req: &Request<'_>, vhost: &str, now: u64) -> Response {
        let query = req.query();
        let clear_state =
            set_cookie(&self.config.state_cookie_name, "", 0, &self.config.cookie_attrs);

        // GitHub reports a user-side refusal (`access_denied`) here.
        if let Some(err) = query_param(query, "error") {
            let id: String = err.chars().filter(char::is_ascii_alphanumeric).take(64).collect();
            log(req, LOG_INFO, &format!("github-auth: GitHub declined the authorization ({id})"));
            return deny(403, "GitHub did not authorize this login.")
                .header("Set-Cookie", clear_state);
        }

        // ── CSRF: the state parameter must match the signed state cookie ──
        //
        // Both halves are required. The cookie is HttpOnly and was set by a
        // login this browser started, so an attacker who plants their own
        // `code`+`state` in a victim's browser cannot also supply the
        // matching cookie — which is exactly the login-CSRF an unvalidated
        // callback allows.
        let Some(cookie_value) =
            req.header("cookie").and_then(|c| read_cookie(c, &self.config.state_cookie_name))
        else {
            log(req, LOG_WARN, "github-auth: callback with no OAuth state cookie — rejected");
            return deny(400, "This login link is not valid here. Start again.");
        };
        let Some(state_claims) =
            token::verify(self.config.state_secret.expose(), cookie_value, now)
        else {
            log(req, LOG_WARN, "github-auth: OAuth state cookie failed verification — rejected");
            return deny(400, "This login has expired. Start again.")
                .header("Set-Cookie", clear_state);
        };
        let presented_state = query_param(query, "state").unwrap_or_default();
        let expected_nonce =
            state_claims.get("n").and_then(serde_json::Value::as_str).unwrap_or_default();
        if expected_nonce.is_empty()
            || !token::constant_time_eq(
                self.config.state_secret.expose(),
                presented_state.as_bytes(),
                expected_nonce.as_bytes(),
            )
        {
            log(req, LOG_WARN, "github-auth: OAuth state mismatch — rejected");
            return deny(400, "This login link is not valid here. Start again.")
                .header("Set-Cookie", clear_state);
        }
        // The state is bound to the vhost it was issued on, so a state minted
        // for one tenant cannot complete a login on another.
        if state_claims.get("v").and_then(serde_json::Value::as_str) != Some(vhost) {
            log(req, LOG_WARN, "github-auth: OAuth state was issued for a different host");
            return deny(400, "This login link is not valid here. Start again.")
                .header("Set-Cookie", clear_state);
        }

        // Re-sanitize even though we signed it: the value was attacker-chosen
        // before it was signed, and a signature attests origin, not safety.
        let return_to = redirect::sanitize_return_to(
            state_claims.get("rt").and_then(serde_json::Value::as_str).unwrap_or("/"),
            &self.config.reserved_paths(),
        );

        let Some(check) = self.config.check_for(vhost) else {
            return deny(403, "This preview is not configured for GitHub access control.")
                .header("Set-Cookie", clear_state);
        };

        let Some(code) = query_param(query, "code").filter(|c| is_plausible_code(c)) else {
            log(req, LOG_WARN, "github-auth: callback with no usable `code`");
            return deny(400, "This login link is not valid here. Start again.")
                .header("Set-Cookie", clear_state);
        };

        // ── The only network calls in the module ─────────────────────────
        let outcome =
            self.github.exchange_code(&code, &self.redirect_uri(vhost)).and_then(|access| {
                let user = self.github.current_user(&access)?;
                let allowed = self.github.check_access(&access, check, &user)?;
                Ok((user, allowed))
            });

        let (user, allowed) = match outcome {
            Ok(v) => v,
            Err(e) => {
                // `GhError`'s Display carries no credential — see github.rs.
                log(req, LOG_ERROR, &format!("github-auth: login failed: {e}"));
                let status = if matches!(e, GhError::OAuth { .. }) { 400 } else { 502 };
                return deny(status, "Could not complete the GitHub login.")
                    .header("Set-Cookie", clear_state);
            }
        };

        if !allowed {
            log(
                req,
                LOG_INFO,
                &format!(
                    "github-auth: denied {} on vhost {vhost:?} — no access to {}",
                    user.login,
                    check.describe()
                ),
            );
            return deny(403, "Your GitHub account does not have access to this preview.")
                .header("Set-Cookie", clear_state);
        }

        self.issue(
            req,
            vhost,
            &user.login,
            "github",
            Some(user.id),
            Some(check),
            now,
            self.config.session_ttl_secs,
            &return_to,
        )
        .header("Set-Cookie", clear_state)
    }

    /// Mint a session and 302 to `return_to` carrying it.
    #[allow(clippy::too_many_arguments, reason = "one call site each; all values are required")]
    fn issue(
        &self,
        req: &Request<'_>,
        vhost: &str,
        subject: &str,
        via: &str,
        gh_id: Option<u64>,
        check: Option<&Check>,
        now: u64,
        ttl: u64,
        return_to: &str,
    ) -> Response {
        let claims = self.session_claims(subject, vhost, via, gh_id, check, now, ttl);
        let Some(session) = token::mint(self.config.session_secret.expose(), &claims) else {
            log(req, LOG_ERROR, "github-auth: could not mint the session token");
            return deny(500, "Login could not be completed.");
        };
        log(
            req,
            LOG_INFO,
            &format!("github-auth: session issued for {subject} on vhost {vhost:?} (via {via})"),
        );
        redirect_to(return_to).header(
            "Set-Cookie",
            set_cookie(
                &self.config.cookie_name,
                &session,
                i64::try_from(ttl).unwrap_or(i64::MAX),
                &self.config.cookie_attrs,
            ),
        )
    }

    /// One-shot startup banner, emitted on the first request because `init`
    /// has no host logger available to it.
    fn banner(&self, req: &Request<'_>) {
        if self.banner_done.swap(true, Ordering::Relaxed) {
            return;
        }
        let target = self.config.default_check.as_ref().map_or_else(
            || format!("{} vhost(s) mapped", self.config.sites.len()),
            Check::describe,
        );
        log(
            req,
            LOG_INFO,
            &format!(
                "github-auth: active — login {}, callback {}, cookie {} ({}s), target {}, \
                 bypass {}, TLS {}. This module ISSUES sessions only; a session-verifier \
                 middleware must be mounted at a higher `order` or a request carrying any \
                 cookie value will pass through unchecked.",
                self.config.login_path,
                self.config.callback_path,
                self.config.cookie_name,
                self.config.session_ttl_secs,
                target,
                if self.config.bypass_token.is_some() { "enabled" } else { "off" },
                self.github.trust_anchors(),
            ),
        );
    }
}

impl Middleware for GithubAuth {
    fn init(config: &serde_json::Value) -> Result<Self, String> {
        let config = Config::parse(config)?;
        let github = GitHub::new(&config).map_err(|e| e.to_string())?;
        Ok(Self { config, github, banner_done: AtomicBool::new(false) })
    }

    fn invoke(&self, req: &Request<'_>) -> Response {
        self.banner(req);

        // Canonicalise ONCE, here, and use only this value below — as the
        // `sites` key, in the `site` claim, in the state binding and in the
        // derived `redirect_uri`. `request_vhost_id` is the raw `Host`
        // header; see `config::normalize_vhost` for why that matters.
        let vhost = config::normalize_vhost(req.vhost_id());
        let vhost = vhost.as_str();
        if config::validate_vhost(vhost).is_err() {
            log(req, LOG_WARN, "github-auth: request with an unusable Host header — rejected");
            return deny(400, "Bad request.");
        }

        let cookies = req.header("cookie").unwrap_or_default();
        let bypass = self.presented_bypass(req);
        let now = unix_now();

        match self.route(req.path(), req.query(), cookies, bypass.as_deref()) {
            Route::HasSession => Response::cont(),
            Route::StartLogin { return_to } => self.start_login(req, vhost, &return_to, now),
            Route::Callback => self.handle_callback(req, vhost, now),
            Route::Bypass => {
                let return_to = redirect::sanitize_return_to(
                    &here(req.path(), req.query()),
                    &self.config.reserved_paths(),
                );
                self.issue(
                    req,
                    vhost,
                    "automation",
                    "bypass",
                    None,
                    None,
                    now,
                    self.config.bypass_ttl_secs,
                    &return_to,
                )
            }
        }
    }

    fn describe() -> &'static str {
        "github-auth (GitHub OAuth login gate — issues sessions; pair with a session verifier)"
    }
}

impl GithubAuth {
    /// The bypass token presented on this request, if any.
    ///
    /// The header form is the default. A query-parameter form exists because
    /// some tools (and every "just paste a link into a browser" case) cannot
    /// set headers, but it is **off unless `bypass_query` is configured**:
    /// a query string lands in access logs, browser history and `Referer`,
    /// which a header does not.
    fn presented_bypass(&self, req: &Request<'_>) -> Option<String> {
        self.config.bypass_token.as_ref()?;
        if let Some(v) = req.header(&self.config.bypass_header).filter(|v| !v.is_empty()) {
            return Some(v.to_owned());
        }
        let name = self.config.bypass_query.as_deref()?;
        query_param(req.query(), name)
    }
}

// ── helpers ───────────────────────────────────────────────────────────────

/// Log through the host's `tracing` subscriber.
///
/// The module's own `tracing` crate instance has a different global
/// dispatcher than ePHPm's, so this callback is the only thing that reaches
/// the server's logs. Nothing passed here may contain a code, a token or a
/// secret; every call site in this crate is written to that rule.
fn log(req: &Request<'_>, level: i32, msg: &str) {
    req.host().log(level, msg);
}

/// Seconds since the Unix epoch.
fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The current request as a return-to (`path` plus `?query`).
fn here(path: &str, query: &str) -> String {
    if query.is_empty() { path.to_owned() } else { format!("{path}?{query}") }
}

/// A 302 with caching disabled.
///
/// `no-store` matters: without it a shared cache (or the browser's back/
/// forward cache) can serve one user the redirect — and the `Set-Cookie` —
/// minted for another.
fn redirect_to(location: &str) -> Response {
    Response::respond(302, Vec::new())
        .header("Location", location)
        .header("Cache-Control", "no-store, private")
}

/// A refusal page. Plain text on purpose: no template, no interpolation of
/// anything GitHub or the client said, and therefore no way to turn the gate
/// into a reflected-XSS surface on the preview's own origin.
fn deny(status: u16, message: &str) -> Response {
    Response::respond(status, message.as_bytes().to_vec())
        .header("Content-Type", "text/plain; charset=utf-8")
        .header("Cache-Control", "no-store, private")
}

/// An OAuth `code` is an opaque token; anything else is not one.
fn is_plausible_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 512
        && code.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~'))
}

/// Read one parameter out of a raw query string, percent-decoded.
#[must_use]
pub fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (percent_decode(k) == name).then(|| percent_decode(v))
    })
}

/// `application/x-www-form-urlencoded` decoding. Invalid escapes are left
/// literal rather than dropped, so a malformed value stays visibly malformed
/// instead of silently becoming a different valid one.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    out.push(b);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

ephpm_middleware::declare!(GithubAuth);

#[cfg(test)]
mod tests {
    #![allow(unsafe_code, reason = "tests build the FFI Request view by hand")]

    use ephpm_middleware::abi::{ACTION_CONTINUE, ACTION_RESPOND};
    use ephpm_middleware::host::{RequestCtx, host_table};

    use super::*;

    const SESSION_SECRET: &str = "0123456789abcdef0123456789abcdef";
    const BYPASS: &str = "bypass-token-that-is-long-enough-32";

    fn gate(extra: serde_json::Value) -> GithubAuth {
        let mut cfg = serde_json::json!({
            "client_id": "Iv1.test",
            "client_secret": "client-secret",
            "session_secret": SESSION_SECRET,
            "repo": "acme/web",
            // Loopback so `init` does not need to be pointed at real GitHub.
            "github_base": "http://127.0.0.1:1",
            "github_api_base": "http://127.0.0.1:1",
        });
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            cfg[k] = v;
        }
        GithubAuth::init(&cfg).expect("init")
    }

    fn invoke(mw: &GithubAuth, path: &str, query: &str, headers: &[(String, String)]) -> Response {
        let ctx = RequestCtx::new("GET", path, query, "203.0.113.9", "pr-1.preview.test", headers);
        // SAFETY: `ctx` outlives the borrow; `host_table()` is 'static.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        mw.invoke(&req)
    }

    fn header(resp: &Response, name: &str) -> Option<String> {
        resp.__headers().iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone())
    }

    fn all_headers(resp: &Response, name: &str) -> Vec<String> {
        resp.__headers()
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
            .collect()
    }

    // ── routing: what can reach the network ──────────────────────────────

    #[test]
    fn only_the_callback_path_can_reach_github() {
        let mw = gate(serde_json::json!({ "bypass_token": BYPASS }));
        let cases = [
            ("/", "", "", None),
            ("/index.php", "a=b", "", None),
            ("/_ephpm/auth/github/login", "", "", None),
            ("/wp-admin/", "", "ephpm_session=tok", None),
            ("/", "", "", Some(BYPASS)),
            // Near misses on the callback path must NOT route to Callback.
            ("/_ephpm/auth/github/callback/", "code=x", "", None),
            ("/_ephpm/auth/github/CALLBACK", "code=x", "", None),
            ("/_ephpm/auth/github/callbackx", "code=x", "", None),
        ];
        for (path, query, cookies, bypass) in cases {
            assert_ne!(
                mw.route(path, query, cookies, bypass),
                Route::Callback,
                "{path} must not reach the GitHub round trip"
            );
        }
        assert_eq!(
            mw.route("/_ephpm/auth/github/callback", "code=x&state=y", "", None),
            Route::Callback
        );
    }

    #[test]
    fn a_request_with_a_session_cookie_is_handed_straight_to_the_verifier() {
        let mw = gate(serde_json::json!({ "bypass_token": BYPASS }));
        // Present, valid-looking: CONTINUE.
        assert_eq!(mw.route("/", "", "ephpm_session=anything", None), Route::HasSession);
        // Present but garbage: still CONTINUE — judging it is the verifier's
        // job, and re-deciding it here would be a second, divergent verifier.
        assert_eq!(mw.route("/", "", "ephpm_session=not.a.token", None), Route::HasSession);
        // Present alongside a bypass token: the cookie wins, so an automation
        // client mints once instead of on every request.
        assert_eq!(mw.route("/", "", "ephpm_session=x", Some(BYPASS)), Route::HasSession);
        // Empty value is not a session.
        assert_ne!(mw.route("/", "", "ephpm_session=", None), Route::HasSession);
    }

    #[test]
    fn no_cookie_starts_a_login_that_remembers_where_you_were() {
        let mw = gate(serde_json::json!({}));
        assert_eq!(
            mw.route("/wp-admin/edit.php", "post=7", "", None),
            Route::StartLogin { return_to: "/wp-admin/edit.php?post=7".into() }
        );
    }

    #[test]
    fn login_path_honours_next_but_sanitizes_it() {
        let mw = gate(serde_json::json!({}));
        assert_eq!(
            mw.route("/_ephpm/auth/github/login", "next=%2Fdashboard", "", None),
            Route::StartLogin { return_to: "/dashboard".into() }
        );
        // The verifier hands `next` over verbatim from a client-controlled
        // URL, so it gets the same treatment as any other return-to.
        for hostile in ["next=https%3A%2F%2Fevil.test", "next=%2F%2Fevil.test", "next=%2F%5Cevil"] {
            assert_eq!(
                mw.route("/_ephpm/auth/github/login", hostile, "", None),
                Route::StartLogin { return_to: "/".into() },
                "{hostile} must not survive"
            );
        }
    }

    // ── the 302 to GitHub ───────────────────────────────────────────────

    #[test]
    fn unauthenticated_request_redirects_to_github_with_a_state() {
        let mw = gate(serde_json::json!({ "github_base": "http://127.0.0.1:1" }));
        let resp = invoke(&mw, "/secret.php", "x=1", &[]);
        assert_eq!(resp.__action(), ACTION_RESPOND);
        assert_eq!(resp.__status(), 302);
        assert!(resp.__body().is_empty(), "no body — PHP never ran and neither did a template");

        let location = header(&resp, "Location").expect("Location");
        assert!(location.starts_with("http://127.0.0.1:1/login/oauth/authorize?"));
        assert!(location.contains("client_id=Iv1.test"));
        assert!(location.contains("&state="), "state is mandatory (CSRF)");
        assert!(
            location.contains(
                "redirect_uri=https%3A%2F%2Fpr-1.preview.test%2F_ephpm%2Fauth%2Fgithub%2Fcallback"
            ),
            "redirect_uri derives from the request's vhost: {location}"
        );

        let set = header(&resp, "Set-Cookie").expect("state cookie");
        assert!(set.starts_with("ephpm_session_oauth="));
        assert!(set.contains("HttpOnly") && set.contains("Secure") && set.contains("SameSite=Lax"));
        assert!(set.contains("Max-Age=600"));
        assert_eq!(header(&resp, "Cache-Control").as_deref(), Some("no-store, private"));
    }

    #[test]
    fn scopes_are_omitted_unless_configured() {
        assert!(
            !header(&invoke(&gate(serde_json::json!({})), "/", "", &[]), "Location")
                .expect("Location")
                .contains("scope="),
            "an empty scope list is the GitHub App shape and must not send `scope=`"
        );
        let mw = gate(serde_json::json!({ "scopes": ["read:user", "repo"] }));
        assert!(
            header(&invoke(&mw, "/", "", &[]), "Location")
                .expect("Location")
                .contains("scope=read%3Auser%20repo")
        );
    }

    #[test]
    fn an_unmapped_vhost_is_denied_rather_than_defaulted() {
        let mw = gate(serde_json::json!({
            "sites": { "pr-2.preview.test": { "repo": "acme/other" } },
        }));
        let resp = invoke(&mw, "/", "", &[]);
        assert_eq!(resp.__status(), 403, "an unmapped host must not inherit another tenant's rule");
    }

    // ── callback: CSRF ──────────────────────────────────────────────────

    /// Drive a login, then replay its state cookie into a callback.
    fn login_state_cookie(mw: &GithubAuth) -> (String, String) {
        let resp = invoke(mw, "/dashboard", "", &[]);
        let location = header(&resp, "Location").expect("Location");
        let state = query_param(location.split_once('?').expect("query").1, "state")
            .expect("state parameter");
        let cookie = header(&resp, "Set-Cookie").expect("Set-Cookie");
        let value = cookie.split(';').next().expect("cookie pair").to_owned();
        (state, value)
    }

    #[test]
    fn callback_without_a_state_cookie_is_rejected() {
        let mw = gate(serde_json::json!({}));
        let resp = invoke(&mw, "/_ephpm/auth/github/callback", "code=abc&state=whatever", &[]);
        assert_eq!(resp.__status(), 400);
        assert!(
            all_headers(&resp, "Set-Cookie").iter().all(|c| !c.starts_with("ephpm_session=")),
            "a rejected callback must not issue a session"
        );
    }

    #[test]
    fn callback_with_a_forged_state_is_rejected() {
        let mw = gate(serde_json::json!({}));
        let (_state, cookie) = login_state_cookie(&mw);
        let headers = vec![("Cookie".to_owned(), cookie)];
        for forged in
            ["", "state=", "state=guessed", "state=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"]
        {
            let q = if forged.is_empty() {
                "code=abc".to_owned()
            } else {
                format!("code=abc&{forged}")
            };
            let resp = invoke(&mw, "/_ephpm/auth/github/callback", &q, &headers);
            assert_eq!(resp.__status(), 400, "state {forged:?} must be rejected");
        }
    }

    #[test]
    fn callback_with_a_state_cookie_signed_by_someone_else_is_rejected() {
        let mw = gate(serde_json::json!({}));
        // Mint a state token under a different key — the shape is right, the
        // signature is not.
        let other = token::derive_key(b"a-completely-different-secret-key", token::STATE_KEY_LABEL);
        let forged = token::mint(
            &other,
            &serde_json::json!({ "n": "abc", "rt": "/", "v": "pr-1.preview.test", "exp": unix_now() + 300 }),
        )
        .expect("mint");
        let headers = vec![("Cookie".to_owned(), format!("ephpm_session_oauth={forged}"))];
        let resp = invoke(&mw, "/_ephpm/auth/github/callback", "code=abc&state=abc", &headers);
        assert_eq!(resp.__status(), 400);
    }

    #[test]
    fn callback_state_is_bound_to_the_host_it_was_issued_on() {
        let mw = gate(serde_json::json!({}));
        let state_token = token::mint(
            mw.config.state_secret.expose(),
            &serde_json::json!({
                "n": "nonce-value", "rt": "/", "v": "other.preview.test",
                "exp": unix_now() + 300,
            }),
        )
        .expect("mint");
        let headers = vec![("Cookie".to_owned(), format!("ephpm_session_oauth={state_token}"))];
        let resp =
            invoke(&mw, "/_ephpm/auth/github/callback", "code=abc&state=nonce-value", &headers);
        assert_eq!(
            resp.__status(),
            400,
            "a state minted for another tenant must not complete here"
        );
    }

    #[test]
    fn expired_state_is_rejected() {
        let mw = gate(serde_json::json!({}));
        let state_token = token::mint(
            mw.config.state_secret.expose(),
            &serde_json::json!({ "n": "n", "rt": "/", "v": "pr-1.preview.test", "exp": 1_000 }),
        )
        .expect("mint");
        let headers = vec![("Cookie".to_owned(), format!("ephpm_session_oauth={state_token}"))];
        let resp = invoke(&mw, "/_ephpm/auth/github/callback", "code=abc&state=n", &headers);
        assert_eq!(resp.__status(), 400);
    }

    #[test]
    fn callback_rejects_an_implausible_code_before_any_network_call() {
        let mw = gate(serde_json::json!({}));
        let (state, cookie) = login_state_cookie(&mw);
        let headers = vec![("Cookie".to_owned(), cookie)];
        // `github_base` points at 127.0.0.1:1 (closed), so a network attempt
        // would surface as 502. A 400 proves the code was refused first.
        for bad in ["", "a b", "a/b", "<script>", &"x".repeat(513)] {
            let q = format!("code={}&state={state}", github::encode_query(bad));
            let resp = invoke(&mw, "/_ephpm/auth/github/callback", &q, &headers);
            assert_eq!(resp.__status(), 400, "code {bad:?} must be refused locally");
        }
    }

    // ── bypass ──────────────────────────────────────────────────────────

    #[test]
    fn bypass_token_issues_a_normal_session() {
        let mw = gate(serde_json::json!({ "bypass_token": BYPASS, "bypass_ttl_secs": 900 }));
        let headers = vec![("x-ephpm-bypass".to_owned(), BYPASS.to_owned())];
        let resp = invoke(&mw, "/report", "a=1", &headers);
        assert_eq!(resp.__status(), 302);
        assert_eq!(header(&resp, "Location").as_deref(), Some("/report?a=1"));
        let set = header(&resp, "Set-Cookie").expect("session cookie");
        assert!(set.starts_with("ephpm_session="));
        assert!(set.contains("Max-Age=900"));
    }

    #[test]
    fn a_wrong_bypass_token_falls_through_to_github_and_never_pretends_otherwise() {
        let mw = gate(serde_json::json!({ "bypass_token": BYPASS }));
        for wrong in
            ["", "x", "bypass-token-that-is-long-enough-3", "bypass-token-that-is-long-enough-320"]
        {
            let headers = vec![("x-ephpm-bypass".to_owned(), wrong.to_owned())];
            let resp = invoke(&mw, "/", "", &headers);
            assert_eq!(resp.__status(), 302);
            assert!(
                header(&resp, "Location").expect("Location").contains("/login/oauth/authorize"),
                "a wrong bypass token must behave exactly like no token at all"
            );
        }
    }

    #[test]
    fn the_bypass_query_parameter_is_off_unless_asked_for() {
        let mw = gate(serde_json::json!({ "bypass_token": BYPASS }));
        let resp = invoke(&mw, "/", &format!("_bypass={BYPASS}"), &[]);
        assert!(header(&resp, "Location").expect("Location").contains("/login/oauth/authorize"));

        let mw = gate(serde_json::json!({ "bypass_token": BYPASS, "bypass_query": "_bypass" }));
        let resp = invoke(&mw, "/", &format!("_bypass={BYPASS}"), &[]);
        assert!(header(&resp, "Set-Cookie").expect("Set-Cookie").starts_with("ephpm_session="));
    }

    // ── issued token ────────────────────────────────────────────────────

    #[test]
    fn issued_sessions_carry_the_documented_claims() {
        let mw = gate(serde_json::json!({ "audience": "preview", "issuer": "switchboard" }));
        let claims = mw.session_claims(
            "octocat",
            "pr-1.preview.test",
            "github",
            Some(583_231),
            Some(&Check::Repo { owner: "acme".into(), name: "web".into() }),
            1_000,
            3_600,
        );
        assert_eq!(claims["iss"], "switchboard");
        assert_eq!(claims["sub"], "octocat");
        assert_eq!(claims["aud"], "preview");
        assert_eq!(claims["iat"], 1_000);
        assert_eq!(claims["exp"], 4_600);
        assert_eq!(claims["site"], "pr-1.preview.test");
        assert_eq!(claims["via"], "github");
        assert_eq!(claims["gh_id"], 583_231);
        assert_eq!(claims["check"], "repo acme/web");
    }

    #[test]
    fn an_externally_minted_share_token_is_the_same_object() {
        // The share-link claim: a control plane holding `session_secret` can
        // mint an acceptable session with no help from this module.
        let mw = gate(serde_json::json!({}));
        let external = token::mint(
            mw.config.session_secret.expose(),
            &serde_json::json!({
                "iss": "ephpm-github-auth", "sub": "share:designer@example.com",
                "site": "pr-1.preview.test", "via": "share",
                "iat": unix_now(), "exp": unix_now() + 7_200,
            }),
        )
        .expect("mint");
        // Same three-part HS256 shape as one this module issues.
        assert_eq!(external.split('.').count(), 3);
        // And it is what the gate treats as a session.
        assert_eq!(
            mw.route("/", "", &format!("ephpm_session={external}"), None),
            Route::HasSession
        );
    }

    // ── misc ────────────────────────────────────────────────────────────

    #[test]
    fn a_session_cookie_costs_nothing_and_adds_no_headers() {
        let mw = gate(serde_json::json!({}));
        let headers = vec![("Cookie".to_owned(), "ephpm_session=abc.def.ghi".to_owned())];
        let resp = invoke(&mw, "/index.php", "", &headers);
        assert_eq!(resp.__action(), ACTION_CONTINUE);
        assert!(resp.__headers().is_empty());
        assert!(resp.__response_headers().is_empty());
    }

    #[test]
    fn the_host_header_is_normalised_before_it_becomes_an_identity() {
        // `request_vhost_id` hands over the RAW `Host` header. Two spellings
        // of one host must not produce two identities, or a session minted
        // under one would not match the other's `site` claim — and a `sites`
        // lookup would be bypassable by changing a letter's case.
        let mw = gate(serde_json::json!({}));
        let mut seen = std::collections::BTreeSet::new();
        for host in ["pr-1.preview.test", "PR-1.Preview.TEST", "pr-1.preview.test."] {
            let ctx = RequestCtx::new("GET", "/", "", "203.0.113.9", host, &[]);
            // SAFETY: `ctx` outlives the borrow; `host_table()` is 'static.
            let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
            let resp = mw.invoke(&req);
            assert_eq!(resp.__status(), 302);
            let loc = header(&resp, "Location").expect("Location");
            let redirect_uri = loc
                .split("redirect_uri=")
                .nth(1)
                .and_then(|s| s.split('&').next())
                .expect("redirect_uri")
                .to_owned();
            seen.insert(redirect_uri);
        }
        assert_eq!(seen.len(), 1, "every spelling must derive one redirect_uri: {seen:?}");
        assert!(seen.iter().next().expect("one").contains("pr-1.preview.test"));
    }

    #[test]
    fn a_hostile_host_header_is_rejected() {
        let mw = gate(serde_json::json!({}));
        let ctx = RequestCtx::new("GET", "/", "", "203.0.113.9", "evil.test/../x", &[]);
        // SAFETY: `ctx` outlives the borrow; `host_table()` is 'static.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        assert_eq!(mw.invoke(&req).__status(), 400);
    }

    #[test]
    fn query_parsing_decodes_and_does_not_confuse_prefixes() {
        assert_eq!(query_param("a=1&next=%2Fx%20y", "next").as_deref(), Some("/x y"));
        assert_eq!(query_param("nextish=1&next=2", "next").as_deref(), Some("2"));
        assert_eq!(query_param("a=1", "b"), None);
        assert_eq!(query_param("", "a"), None);
        assert_eq!(query_param("a=b+c", "a").as_deref(), Some("b c"));
        // A truncated escape stays literal rather than silently changing.
        assert_eq!(query_param("a=%2", "a").as_deref(), Some("%2"));
        assert_eq!(query_param("a=%zz", "a").as_deref(), Some("%zz"));
    }

    #[test]
    fn describe_names_the_pairing_requirement() {
        assert!(GithubAuth::describe().contains("session verifier"));
    }
}
