//! `preview-gate` — the per-preview access gate: it verifies a GitHub-OAuth
//! session cookie **and** accepts a time-limited, revocable shareable-URL
//! capability token, at one enforcement point, fail-closed.
//!
//! This is the request-phase enforcement half of the preview access gate
//! (issue #487). It runs on **every** request to a gated preview — both the
//! PHP path and, through [`crate`]'s host, the static-file path — and
//! short-circuits an unauthenticated request with a redirect to the login
//! service **before** any content (a script *or* a file) is served. That
//! static-path coverage is the property the whole design turns on: an
//! unauthenticated `GET /wp-content/uploads/secret.png` is redirected exactly
//! as `GET /index.php` is, so the file's bytes never leave disk.
//!
//! ## Two grant paths, one verifier
//!
//! A request is admitted by **either** credential, and both are checked with
//! the *same* [`Hs256Policy`] as the hot-path `session-cookie` verifier — there
//! is deliberately **no second verifier** (issue #396's one-verifier design):
//!
//! 1. **An OAuth session cookie** minted by the `github-auth` issuer, bound to
//!    this preview by a `site` claim (`Request::vhost_id`, the canonical site
//!    key). A session for preview A never opens preview B.
//! 2. **A shareable-URL capability token** (`via: "share"`), for a stakeholder
//!    who lacks GitHub repo access. Same HS256 secret, same `site` binding, but
//!    additionally short-lived and **revocable** (see [`Self::share_admitted`]).
//!    Presented either as `?<share_param>=<token>` (exchanged for the cookie on
//!    first use and stripped from the redirect) or as the session cookie
//!    itself.
//!
//! ## Threat model of the share link — stated plainly
//!
//! A share URL is a **bearer capability**: anyone who holds the link is in,
//! until it expires or is revoked. That is the point — it lets someone who
//! cannot authenticate to GitHub see one preview — and it is a strictly
//! *weaker* property than the OAuth gate. Its blast radius is bounded by
//! design, not by an issuance ACL:
//!
//! * **Per-preview** — the `site` claim is checked by the same binding the
//!   OAuth session uses, so a link opens exactly one preview, never the fleet.
//! * **Short-lived** — a small `exp` (the minter's responsibility; the ePHPm
//!   verifier only enforces that `exp` exists and is in the future) means a
//!   leaked link self-heals.
//! * **Revocable** — a per-`jti` KV deny-list kills one link, and a per-site
//!   epoch (`preview:share:epoch`, or the static `share_epoch` config floor)
//!   kills every outstanding link at once, which is what a preview teardown
//!   writes.
//!
//! What it explicitly does **not** defend: someone the link was shared with
//! forwarding it within its lifetime, or a compromised holder. Those are
//! accepted for a preview-privacy feature — a preview is not a secrets vault.
//!
//! ## Configuration (`config = { ... }`)
//!
//! | key | default | meaning |
//! |-----|---------|---------|
//! | `secret` (string) | **required** | HS256 shared secret — the same key the `github-auth` issuer signs sessions with |
//! | `login_url` (string) | **required** | absolute `https://`/`http://` URL, or same-origin absolute path, unauthenticated browsers are redirected to |
//! | `cookie` (string) | `"ephpm_session"` | session cookie name (must match the issuer's `cookie_name`) |
//! | `exempt_paths` (array of string) | `[]` | request paths that bypass the gate entirely — the issuer's `login_path`/`callback_path` go here so the OAuth round trip can complete without a redirect loop |
//! | `share_param` (string) | `"ephpm_share"` | query parameter carrying a share capability token |
//! | `share_epoch` (integer) | `0` | static floor: a share token whose `iat` is below this is refused (a config-time revoke-all) |
//! | `share_revocation` (bool) | `true` | consult the per-`jti` KV deny-list and per-site KV epoch for `via:"share"` tokens (a plain session pays nothing) |
//! | `return_to_param` (string) | unset | query parameter on `login_url` carrying the validated same-origin return path |
//! | `site_param` (string) | unset | query parameter carrying this request's canonical site key |
//! | `issuer` (string) | unset | required `iss` claim |
//! | `audience` (string) | unset | required `aud` claim |
//! | `require_https` (bool) | `true` | refuse a credential presented over cleartext (loopback exempt) |
//! | `require_site` (bool) | `true` | require the token's `site` claim to equal the request's canonical site key (issue #396) |

use base64ct::{Base64UrlUnpadded, Encoding as _};
use ephpm_middleware::{Middleware, Request, Response};
use hmac::{Hmac, Mac as _};
use sha2::Sha256;

use crate::hs256::{Hs256Policy, now_unix, opt_bool, opt_str};
use crate::session_cookie::{
    DEFAULT_COOKIE, cookie_values, is_loopback, percent_encode, same_origin_return_to,
};

/// Default query parameter carrying a share capability token.
const DEFAULT_SHARE_PARAM: &str = "ephpm_share";

/// KV key (per-vhost keyspace) whose presence revokes a specific share token.
/// The `jti` is appended: `preview:share:revoked:<jti>`.
const REVOKED_KEY_PREFIX: &str = "preview:share:revoked:";

/// KV key (per-vhost keyspace) holding the site's share epoch: a share token
/// whose `iat` is below the stored value is refused. Teardown writes `now`
/// here to kill every outstanding link for the preview at once.
const EPOCH_KEY: &str = "preview:share:epoch";

/// The per-preview access gate, built once at `init`.
pub struct PreviewGate {
    /// Signature + registered-claim policy, shared with `session-cookie`/`jwt`.
    policy: Hs256Policy,
    cookie: String,
    login_url: String,
    exempt_paths: Vec<String>,
    share_param: String,
    share_epoch: u64,
    share_revocation: bool,
    return_to_param: Option<String>,
    site_param: Option<String>,
    require_https: bool,
    require_site: bool,
}

impl PreviewGate {
    /// Build the `Location` for an unauthenticated request — identical shape to
    /// `session-cookie`'s, reusing the same percent-encoding and same-origin
    /// return-to validation.
    fn login_location(&self, return_to: Option<&str>, site: Option<&str>) -> String {
        let mut url = self.login_url.clone();
        let mut sep = if url.contains('?') { '&' } else { '?' };
        if let (Some(param), Some(target)) = (self.return_to_param.as_deref(), return_to) {
            url.push(sep);
            url.push_str(&percent_encode(param));
            url.push('=');
            url.push_str(&percent_encode(target));
            sep = '&';
        }
        if let (Some(param), Some(site)) = (self.site_param.as_deref(), site)
            && !site.is_empty()
        {
            url.push(sep);
            url.push_str(&percent_encode(param));
            url.push('=');
            url.push_str(&percent_encode(site));
        }
        url
    }

    /// The redirect verdict for an unauthenticated request (`302` for GET/HEAD,
    /// `303` otherwise, so a POST does not re-submit its body to the login
    /// service).
    fn redirect(&self, req: &Request<'_>) -> Response {
        let status = if matches!(req.method(), "GET" | "HEAD") { 302 } else { 303 };
        let return_to = same_origin_return_to(req.path(), req.query());
        Response::respond(status, "redirecting to login")
            .header("Location", self.login_location(return_to.as_deref(), req.vhost_id()))
            .header("Cache-Control", "no-store")
            .header("Vary", "Cookie")
    }

    /// Whether `path` is one the gate must not touch — the issuer's own login
    /// and callback endpoints, so the OAuth round trip can complete without the
    /// gate redirecting it back to login (a loop). Exact match: these are fixed
    /// reserved paths, never globs.
    fn is_exempt(&self, path: &str) -> bool {
        self.exempt_paths.iter().any(|p| p == path)
    }

    /// Decide whether a verified token's claims admit the request.
    ///
    /// A plain session (`via != "share"`) is admitted on signature/expiry/site
    /// alone — [`Hs256Policy::verify`] already checked those, so it pays nothing
    /// more here. A share token additionally passes revocation and epoch — see
    /// [`Self::share_admitted`]. `claims_json` is the verified payload returned
    /// by the policy.
    fn admitted(&self, req: &Request<'_>, claims_json: &str, now: u64) -> bool {
        let Ok(claims) = serde_json::from_str::<serde_json::Value>(claims_json) else {
            // The policy already parsed this exact payload, so a failure here is
            // impossible — but if it somehow happened, fail closed.
            return false;
        };
        match claims.get("via").and_then(serde_json::Value::as_str) {
            Some("share") => self.share_admitted(req, &claims, now),
            _ => true,
        }
    }

    /// Whether a `via:"share"` token is still live: not on the per-`jti` deny
    /// list, and issued at or after the effective epoch. Both checks read the
    /// request's **own** (per-vhost) KV keyspace, so a share link cannot be
    /// revoked from, or leak into, another tenant's keyspace.
    ///
    /// A share token with no `jti` cannot be individually revoked; that is a
    /// minting choice, not a verification failure, so it is admitted (subject to
    /// the epoch, which still kills it on teardown). A token with no `iat` is
    /// treated as `iat = 0`, so any non-zero epoch refuses it — the fail-closed
    /// direction.
    fn share_admitted(&self, req: &Request<'_>, claims: &serde_json::Value, _now: u64) -> bool {
        if !self.share_revocation {
            return true;
        }
        if let Some(jti) = claims.get("jti").and_then(serde_json::Value::as_str) {
            let key = format!("{REVOKED_KEY_PREFIX}{jti}");
            if req.host().kv_get(&key).is_some() {
                return false;
            }
        }
        let effective_epoch = self.effective_epoch(req);
        if effective_epoch == 0 {
            return true;
        }
        let iat = claims.get("iat").and_then(serde_json::Value::as_u64).unwrap_or(0);
        iat >= effective_epoch
    }

    /// The greater of the static `share_epoch` config floor and the dynamic
    /// per-site epoch stored in KV (`preview:share:epoch`), so either channel
    /// can revoke every outstanding link. A malformed KV value is ignored (the
    /// static floor still applies).
    fn effective_epoch(&self, req: &Request<'_>) -> u64 {
        let kv_epoch = req
            .host()
            .kv_get(EPOCH_KEY)
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        self.share_epoch.max(kv_epoch)
    }

    /// Handle a share token presented in the query string: on success, redirect
    /// to the same path with the parameter stripped and the token planted as
    /// the session cookie, so every subsequent request carries it as a cookie.
    /// Returns `None` when there is no valid share token in the query, so the
    /// caller falls through to the cookie check (a bad `?share=` must not
    /// hard-fail a browser that also has a good cookie).
    fn share_query_exchange(
        &self,
        req: &Request<'_>,
        now: u64,
        expected: Option<&str>,
    ) -> Option<Response> {
        let token = query_param(req.query(), &self.share_param)?;
        let claims_json = self.policy.verify(&token, now, expected)?;
        // The query path admits share tokens only — a full OAuth session does
        // not travel in a URL — and it must clear revocation/epoch.
        let claims = serde_json::from_str::<serde_json::Value>(&claims_json).ok()?;
        if claims.get("via").and_then(serde_json::Value::as_str) != Some("share") {
            return None;
        }
        if !self.share_admitted(req, &claims, now) {
            return None;
        }
        let exp = claims.get("exp").and_then(serde_json::Value::as_u64).unwrap_or(now);
        let max_age = exp.saturating_sub(now);

        // Redirect to the same path minus the share parameter — a same-origin
        // absolute path validated by the shared checker (falls back to "/").
        let stripped = strip_query_param(req.query(), &self.share_param);
        let target = same_origin_return_to(req.path(), &stripped).unwrap_or_else(|| "/".to_owned());

        Some(
            Response::respond(303, "share link accepted")
                .header("Location", target)
                .header("Set-Cookie", self.set_cookie(&token, max_age, req.is_secure()))
                .header("Cache-Control", "no-store")
                .header("Vary", "Cookie"),
        )
    }

    /// The `Set-Cookie` value planting a share token as the session cookie.
    /// `Secure` is set whenever the request is secure; `HttpOnly` and
    /// `SameSite=Lax` always. `Max-Age` bounds the cookie to the token's own
    /// remaining life so the browser drops it when the capability expires.
    fn set_cookie(&self, token: &str, max_age: u64, secure: bool) -> String {
        let mut c =
            format!("{}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}", self.cookie);
        if secure {
            c.push_str("; Secure");
        }
        c
    }
}

impl Middleware for PreviewGate {
    const CONFIG_KEYS: Option<&'static [&'static str]> = Some(&[
        "secret",
        "login_url",
        "cookie",
        "exempt_paths",
        "share_param",
        "share_epoch",
        "share_revocation",
        "return_to_param",
        "site_param",
        "issuer",
        "audience",
        "require_https",
        "require_site",
    ]);

    fn init(config: &serde_json::Value) -> Result<Self, String> {
        let login_url = opt_str(config, "login_url")?
            .ok_or("`login_url` is required (where to send unauthenticated browsers)")?;
        // Same redirect-target constraint as `session-cookie`: only real HTTP
        // origins and same-origin absolute paths, never `javascript:`/`data:`
        // (an XSS primitive on every unauthenticated request) or `//host`
        // (protocol-relative, off-origin).
        let absolute = login_url.starts_with("https://") || login_url.starts_with("http://");
        let same_origin_path = login_url.starts_with('/') && !login_url.starts_with("//");
        if !absolute && !same_origin_path {
            return Err(format!(
                "`login_url` must be an https:// or http:// URL, or a same-origin absolute path \
                 starting with `/` — got {login_url:?}"
            ));
        }

        let exempt_paths = match config.get("exempt_paths") {
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .map(|v| {
                    let p = v.as_str().ok_or_else(|| {
                        format!("`exempt_paths` entries must be strings, got {v}")
                    })?;
                    if !p.starts_with('/') {
                        return Err(format!("`exempt_paths` entry {p:?} must be an absolute path"));
                    }
                    Ok(p.to_owned())
                })
                .collect::<Result<Vec<_>, String>>()?,
            Some(other) => return Err(format!("`exempt_paths` must be an array, got {other}")),
        };

        let share_epoch = match config.get("share_epoch") {
            None | Some(serde_json::Value::Null) => 0,
            Some(v) => v
                .as_u64()
                .ok_or_else(|| format!("`share_epoch` must be a positive integer, got {v}"))?,
        };

        Ok(Self {
            policy: Hs256Policy::from_config(config)?,
            cookie: opt_str(config, "cookie")?.unwrap_or_else(|| DEFAULT_COOKIE.to_owned()),
            login_url,
            exempt_paths,
            share_param: opt_str(config, "share_param")?
                .unwrap_or_else(|| DEFAULT_SHARE_PARAM.to_owned()),
            share_epoch,
            share_revocation: opt_bool(config, "share_revocation", true)?,
            return_to_param: opt_str(config, "return_to_param")?,
            site_param: opt_str(config, "site_param")?,
            require_https: opt_bool(config, "require_https", true)?,
            require_site: opt_bool(config, "require_site", true)?,
        })
    }

    fn invoke(&self, req: &Request<'_>) -> Response {
        // The issuer's own login/callback endpoints are never gated, or the
        // OAuth round trip could never complete (the login redirect would be
        // redirected back to login). Checked first, before any transport or
        // tenant rule, because those endpoints answer to the issuer's rules,
        // not the gate's.
        if self.is_exempt(req.path()) {
            return Response::cont();
        }

        // Transport check: a credential presented over cleartext is one an
        // on-path observer has already read. Loopback is a secure context (so
        // local development works). A hard stop, not a redirect — bouncing to
        // an HTTPS login that sets a cookie the browser returns over HTTP loops.
        if self.require_https && !req.is_secure() && !is_loopback(req.remote_ip()) {
            return Response::respond(403, "preview access requires HTTPS (see `require_https`)")
                .header("Cache-Control", "no-store");
        }

        // Per-tenant binding (issue #396): a credential is only valid for the
        // canonical site key the router resolved for this request.
        let expected_site = if self.require_site {
            let Some(site) = req.vhost_id() else {
                return Response::respond(
                    403,
                    "this preview gate requires a resolved site (see `require_site`)",
                )
                .header("Cache-Control", "no-store");
            };
            Some(site)
        } else {
            None
        };

        let now = now_unix();

        // Grant path 1: a share token in the query string — verify it, plant it
        // as the cookie, and redirect to the clean URL. A missing/invalid share
        // param yields None and falls through to the cookie check.
        if let Some(resp) = self.share_query_exchange(req, now, expected_site) {
            return resp;
        }

        // Grant path 2: a session (or share) token in the cookie. Try EVERY
        // cookie of this name — a parent-domain cookie shadows this host's, and
        // accepting only the first would let anyone who can set a cookie on the
        // parent domain lock users out. Each candidate still has to pass the
        // same HMAC, expiry, site and (for share tokens) revocation checks.
        if let Some(raw) = req.header("Cookie") {
            let admitted = cookie_values(raw, &self.cookie).any(|token| {
                self.policy
                    .verify(token, now, expected_site)
                    .is_some_and(|claims_json| self.admitted(req, &claims_json, now))
            });
            if admitted {
                return Response::cont();
            }
        }

        self.redirect(req)
    }

    fn describe() -> &'static str {
        "preview-gate/1.0"
    }
}

/// Read the first value of query parameter `name` from a raw query string
/// (`a=1&b=2`), percent-decoding nothing — a capability token is
/// base64url and carries no reserved characters. Values are compared on the
/// first `=` only, and the name is matched whole.
fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| v.to_owned())
    })
}

/// Rebuild `query` with every occurrence of parameter `name` removed, returning
/// the remaining `k=v&...` string (no leading `?`). Used to produce the clean
/// URL a share link redirects to after the token is planted as a cookie.
fn strip_query_param(query: &str, name: &str) -> String {
    query
        .split('&')
        .filter(|pair| {
            let key = pair.split_once('=').map_or(*pair, |(k, _)| k);
            key != name
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Mint a share capability token — the **reference** minter, used by the tests
/// here and mirrored by whatever control plane (switchboard) issues links in
/// production.
///
/// This is deliberately an ordinary function: issuance is *not* privileged —
/// anything holding the HS256 secret can mint, which is inherent to a
/// self-contained token and is why the blast radius is bounded by the short
/// `exp` and by revocation, not by an issuance ACL (see the module threat
/// model). `jti` should be a unique random string so an individual link can be
/// revoked; `iat`/`exp` are unix seconds.
///
/// # Panics
///
/// Panics only if the fixed, in-process claims object fails to serialise to
/// JSON, which cannot happen for the object built here — the `expect` is a
/// type artefact, not a reachable path.
#[must_use]
pub fn mint_share_token(secret: &[u8], site: &str, jti: &str, iat: u64, exp: u64) -> String {
    let claims = serde_json::json!({
        "site": site,
        "via": "share",
        "jti": jti,
        "iat": iat,
        "exp": exp,
    });
    let header_b64 = Base64UrlUnpadded::encode_string(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload_b64 = Base64UrlUnpadded::encode_string(
        &serde_json::to_vec(&claims).expect("share claims serialise"),
    );
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(header_b64.as_bytes());
    mac.update(b".");
    mac.update(payload_b64.as_bytes());
    let sig = Base64UrlUnpadded::encode_string(&mac.finalize().into_bytes());
    format!("{header_b64}.{payload_b64}.{sig}")
}

#[cfg(test)]
mod tests {
    #![allow(unsafe_code)] // tests build the FFI Request view by hand.

    use ephpm_middleware::abi::{ACTION_CONTINUE, ACTION_RESPOND};
    use ephpm_middleware::host::{RequestCtx, host_table};

    use super::*;

    const SECRET: &str = "test-secret-please-rotate-0123456789";
    const LOGIN: &str = "/auth/github/login";
    const SITE: &str = "pr-42.preview.example";

    fn gate(mut config: serde_json::Value) -> PreviewGate {
        let map = config.as_object_mut().expect("object config");
        map.entry("secret").or_insert_with(|| SECRET.into());
        map.entry("login_url").or_insert_with(|| LOGIN.into());
        PreviewGate::init(&config).expect("init")
    }

    fn future_exp() -> u64 {
        now_unix() + 3600
    }

    /// A github-auth-style session token bound to `site` (no `via`).
    fn session_token(site: &str) -> String {
        let secret = SECRET.as_bytes();
        let claims = serde_json::json!({ "sub": "octocat", "site": site, "exp": future_exp() });
        let header_b64 = Base64UrlUnpadded::encode_string(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload_b64 = Base64UrlUnpadded::encode_string(&serde_json::to_vec(&claims).unwrap());
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac.update(header_b64.as_bytes());
        mac.update(b".");
        mac.update(payload_b64.as_bytes());
        let sig = Base64UrlUnpadded::encode_string(&mac.finalize().into_bytes());
        format!("{header_b64}.{payload_b64}.{sig}")
    }

    fn share_token(site: &str, jti: &str) -> String {
        mint_share_token(SECRET.as_bytes(), site, jti, now_unix(), future_exp())
    }

    #[allow(clippy::too_many_arguments, reason = "test harness; every axis varies")]
    fn invoke_on(
        mw: &PreviewGate,
        method: &str,
        path: &str,
        query: &str,
        headers: &[(String, String)],
        https: bool,
        ip: &str,
        site: &str,
    ) -> Response {
        let ctx = RequestCtx::new(method, path, query, ip, site, headers).with_scheme(https);
        // SAFETY: `ctx` outlives the view; `host_table()` is 'static.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        mw.invoke(&req)
    }

    /// Invoke over HTTPS from a remote client for the default [`SITE`].
    fn invoke(mw: &PreviewGate, path: &str, query: &str, headers: &[(String, String)]) -> Response {
        invoke_on(mw, "GET", path, query, headers, true, "203.0.113.9", SITE)
    }

    fn cookie(value: &str) -> Vec<(String, String)> {
        vec![("Cookie".to_owned(), value.to_owned())]
    }

    fn header_of<'a>(resp: &'a Response, name: &str) -> Option<&'a str> {
        resp.__headers().iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    fn assert_redirect(resp: &Response) {
        assert_eq!(resp.__action(), ACTION_RESPOND, "unauthenticated must short-circuit");
        assert!(matches!(resp.__status(), 302 | 303), "got {}", resp.__status());
        assert!(header_of(resp, "Location").is_some());
    }

    // ── config ─────────────────────────────────────────────────────────────

    #[test]
    fn init_requires_secret_and_login_url() {
        assert!(PreviewGate::init(&serde_json::json!({ "secret": SECRET })).is_err());
        assert!(PreviewGate::init(&serde_json::json!({ "login_url": LOGIN })).is_err());
    }

    #[test]
    fn init_rejects_a_dangerous_login_url() {
        for bad in ["javascript:alert(1)", "data:text/html,x", "//evil.example/login"] {
            let cfg = serde_json::json!({ "secret": SECRET, "login_url": bad });
            assert!(PreviewGate::init(&cfg).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn a_mistyped_bool_knob_fails_startup() {
        for knob in ["require_https", "require_site", "share_revocation"] {
            let cfg = serde_json::json!({ "secret": SECRET, "login_url": LOGIN, knob: "false" });
            assert!(PreviewGate::init(&cfg).is_err(), "{knob} = \"false\" must fail");
        }
    }

    // ── Deliverable 1: session enforcement, fail-closed ──────────────────────

    #[test]
    fn an_unauthenticated_request_is_redirected() {
        let mw = gate(serde_json::json!({}));
        assert_redirect(&invoke(&mw, "/index.php", "", &[]));
        // A static asset path is redirected exactly the same way — the gate is
        // path-agnostic, which is what makes the static-file path fail closed.
        assert_redirect(&invoke(&mw, "/wp-content/uploads/secret.png", "", &[]));
    }

    #[test]
    fn a_valid_session_for_this_site_is_admitted() {
        let mw = gate(serde_json::json!({}));
        let resp = invoke(
            &mw,
            "/index.php",
            "",
            &cookie(&format!("ephpm_session={}", session_token(SITE))),
        );
        assert_eq!(resp.__action(), ACTION_CONTINUE);
    }

    /// The #396 property, end to end through the gate: a session for `alpha` is
    /// admitted on `alpha` and REJECTED on `beta`.
    #[test]
    fn a_session_for_another_site_is_rejected() {
        let mw = gate(serde_json::json!({}));
        let alpha = session_token("alpha");
        assert_eq!(
            invoke_on(
                &mw,
                "GET",
                "/i.php",
                "",
                &cookie(&format!("ephpm_session={alpha}")),
                true,
                "203.0.113.9",
                "alpha"
            )
            .__action(),
            ACTION_CONTINUE
        );
        assert_redirect(&invoke_on(
            &mw,
            "GET",
            "/i.php",
            "",
            &cookie(&format!("ephpm_session={alpha}")),
            true,
            "203.0.113.9",
            "beta",
        ));
    }

    #[test]
    fn cleartext_from_a_remote_client_is_refused() {
        let mw = gate(serde_json::json!({}));
        let resp = invoke_on(
            &mw,
            "GET",
            "/i.php",
            "",
            &cookie(&format!("ephpm_session={}", session_token(SITE))),
            false,
            "203.0.113.9",
            SITE,
        );
        assert_eq!(resp.__status(), 403);
    }

    #[test]
    fn exempt_paths_are_never_gated() {
        let mw = gate(
            serde_json::json!({ "exempt_paths": ["/auth/github/login", "/auth/github/callback"] }),
        );
        // No cookie, but the login/callback endpoints must pass through so the
        // OAuth round trip can run (no redirect loop).
        assert_eq!(invoke(&mw, "/auth/github/login", "", &[]).__action(), ACTION_CONTINUE);
        assert_eq!(invoke(&mw, "/auth/github/callback", "code=x", &[]).__action(), ACTION_CONTINUE);
        // A non-exempt path is still gated.
        assert_redirect(&invoke(&mw, "/index.php", "", &[]));
    }

    // ── Deliverable 2: shareable-URL capability tokens ───────────────────────

    #[test]
    fn a_valid_share_query_plants_the_cookie_and_redirects_clean() {
        let mw = gate(serde_json::json!({}));
        let tok = share_token(SITE, "jti-1");
        let resp = invoke(&mw, "/dashboard.php", &format!("ephpm_share={tok}&tab=1"), &[]);
        assert_eq!(resp.__action(), ACTION_RESPOND);
        assert_eq!(resp.__status(), 303);
        // Redirect drops the share param, keeps the rest.
        assert_eq!(header_of(&resp, "Location"), Some("/dashboard.php?tab=1"));
        let set_cookie = header_of(&resp, "Set-Cookie").expect("Set-Cookie");
        assert!(set_cookie.starts_with(&format!("ephpm_session={tok}")));
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("Secure"));
    }

    #[test]
    fn a_share_token_presented_as_a_cookie_is_admitted() {
        let mw = gate(serde_json::json!({}));
        let tok = share_token(SITE, "jti-1");
        let resp = invoke(&mw, "/index.php", "", &cookie(&format!("ephpm_session={tok}")));
        assert_eq!(resp.__action(), ACTION_CONTINUE);
    }

    #[test]
    fn an_expired_share_token_is_rejected() {
        let mw = gate(serde_json::json!({}));
        let past = now_unix().saturating_sub(10);
        let tok =
            mint_share_token(SECRET.as_bytes(), SITE, "jti-exp", past.saturating_sub(3600), past);
        assert_redirect(&invoke(&mw, "/index.php", "", &cookie(&format!("ephpm_session={tok}"))));
        // ...and in the query string it does not exchange either.
        assert_redirect(&invoke(&mw, "/index.php", &format!("ephpm_share={tok}"), &[]));
    }

    #[test]
    fn a_share_token_for_another_site_is_rejected() {
        let mw = gate(serde_json::json!({}));
        let tok = share_token("some-other-preview", "jti-x");
        assert_redirect(&invoke(&mw, "/index.php", &format!("ephpm_share={tok}"), &[]));
        assert_redirect(&invoke(&mw, "/index.php", "", &cookie(&format!("ephpm_session={tok}"))));
    }

    #[test]
    fn a_share_token_below_the_static_epoch_is_rejected() {
        // `share_epoch` in config is a revoke-all floor: a token issued before
        // it is refused even though its signature and exp are fine.
        let future_epoch = now_unix() + 100;
        let mw = gate(serde_json::json!({ "share_epoch": future_epoch }));
        let tok = share_token(SITE, "jti-old"); // iat = now < future_epoch
        assert_redirect(&invoke(&mw, "/index.php", "", &cookie(&format!("ephpm_session={tok}"))));

        // A token issued at/after the epoch is fine.
        let fresh =
            mint_share_token(SECRET.as_bytes(), SITE, "jti-new", future_epoch, future_epoch + 3600);
        assert_eq!(
            invoke(&mw, "/index.php", "", &cookie(&format!("ephpm_session={fresh}"))).__action(),
            ACTION_CONTINUE
        );
    }

    /// Wire the process-global KV store so `req.kv_get`/`kv_set` reach a real
    /// map (first call in the test binary wins; every test then shares it).
    fn wire_kv() {
        let store = std::sync::Arc::new(ephpm_kv::store::Store::new(
            ephpm_kv::store::StoreConfig::default(),
        ));
        ephpm_middleware::host::set_kv_store(&store);
    }

    /// Write a KV key through the same path the gate reads it, independent of
    /// which store won the process-global `set_kv_store` race.
    fn kv_put(key: &str, value: &[u8]) {
        let ctx = RequestCtx::new("GET", "/", "", "203.0.113.9", SITE, &[]);
        // SAFETY: `ctx` outlives the view; `host_table()` is 'static.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        assert!(req.host().kv_set(key, value, 0), "kv_set must succeed");
    }

    /// A revoked share token (its `jti` on the KV deny-list) is rejected, both
    /// as a cookie and in the query string — the individual-revoke path.
    #[test]
    fn a_revoked_share_token_is_rejected() {
        wire_kv();
        let mw = gate(serde_json::json!({}));
        let tok = share_token(SITE, "jti-revoked");

        // Control: before revocation it is admitted.
        assert_eq!(
            invoke(&mw, "/index.php", "", &cookie(&format!("ephpm_session={tok}"))).__action(),
            ACTION_CONTINUE,
            "control: an un-revoked share token is admitted"
        );

        kv_put("preview:share:revoked:jti-revoked", b"1");

        assert_redirect(&invoke(&mw, "/index.php", "", &cookie(&format!("ephpm_session={tok}"))));
        assert_redirect(&invoke(&mw, "/index.php", &format!("ephpm_share={tok}"), &[]));
    }

    /// The per-site KV epoch (`preview:share:epoch`) kills every outstanding
    /// share link at once — what a preview teardown writes. A token issued
    /// before the epoch is refused; a plain session is untouched.
    #[test]
    fn the_kv_epoch_revokes_all_outstanding_share_links() {
        wire_kv();
        let mw = gate(serde_json::json!({}));
        let tok = share_token(SITE, "jti-epoch"); // iat = now

        assert_eq!(
            invoke(&mw, "/index.php", "", &cookie(&format!("ephpm_session={tok}"))).__action(),
            ACTION_CONTINUE,
            "control: admitted before the epoch is set"
        );

        // Teardown bumps the epoch past the token's iat.
        kv_put("preview:share:epoch", (now_unix() + 100).to_string().as_bytes());

        assert_redirect(&invoke(&mw, "/index.php", "", &cookie(&format!("ephpm_session={tok}"))));
        // A normal OAuth session is unaffected by the epoch.
        assert_eq!(
            invoke(
                &mw,
                "/index.php",
                "",
                &cookie(&format!("ephpm_session={}", session_token(SITE)))
            )
            .__action(),
            ACTION_CONTINUE
        );
    }

    #[test]
    fn share_revocation_false_skips_the_kv_lookups() {
        wire_kv();
        let mw = gate(serde_json::json!({ "share_revocation": false }));
        let tok = share_token(SITE, "jti-norev");
        kv_put("preview:share:revoked:jti-norev", b"1");
        // With revocation disabled the deny-list is not consulted at all.
        assert_eq!(
            invoke(&mw, "/index.php", "", &cookie(&format!("ephpm_session={tok}"))).__action(),
            ACTION_CONTINUE
        );
    }

    #[test]
    fn a_plain_session_is_unaffected_by_the_share_epoch() {
        // The epoch/revocation checks apply ONLY to `via:"share"` tokens; a
        // normal OAuth session pays nothing and is admitted regardless.
        let future_epoch = now_unix() + 100;
        let mw = gate(serde_json::json!({ "share_epoch": future_epoch }));
        let resp = invoke(
            &mw,
            "/index.php",
            "",
            &cookie(&format!("ephpm_session={}", session_token(SITE))),
        );
        assert_eq!(resp.__action(), ACTION_CONTINUE);
    }
}
