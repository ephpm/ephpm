//! `session-cookie` — ePHPm native middleware gating a whole site on a
//! signed session cookie minted by an external identity service, redirecting
//! unauthenticated browsers to that service's login URL.
//!
//! The split this exists for: an external service ("the login service") runs
//! whatever identity dance it likes — OAuth, SSO, SAML, a magic link — and,
//! once it is satisfied, mints an HS256 token and sets it as a cookie on the
//! application's domain. ePHPm then does the *only* part that has to happen
//! on every request: verify the signature and the expiry, locally, and send
//! the browser to the login URL when that fails.
//!
//! **ePHPm never talks to the identity provider.** Verification is a single
//! HMAC over bytes already in the request: no network call, no rate limits,
//! no OAuth client secret on the serving nodes. The nodes hold one shared
//! HMAC secret, and a compromise of a node yields the ability to mint
//! sessions for that one deployment — not access to the identity provider.
//!
//! Relationship to [`crate::jwt`]: identical verification (both call
//! [`crate::hs256`]) and deliberately *nothing* else. `jwt` is an API gate —
//! token in a header, `401` on failure, a machine reads the status code.
//! This is a browser gate — token in a cookie, redirect on failure, a human
//! reads the login page. Keeping them apart means neither module's config
//! can accidentally change the other's failure behaviour, and an operator
//! mounting `jwt` in front of an API is never one key away from turning
//! their `401`s into redirects.
//!
//! Configuration (`[[middleware]] config = { ... }`):
//!
//! | key | default | meaning |
//! |-----|---------|---------|
//! | `secret` (string) | **required** | HS256 shared secret, same key the login service signs with |
//! | `login_url` (string) | **required** | absolute `https://`/`http://` URL, or a same-origin absolute path, to redirect unauthenticated browsers to |
//! | `cookie` (string) | `"ephpm_session"` | name of the cookie carrying the token |
//! | `return_to_param` (string) | unset (no return-to is sent) | query parameter on `login_url` carrying the validated, same-origin return path |
//! | `site_param` (string) | unset | query parameter carrying this request's vhost id, so one login service can serve many sites |
//! | `issuer` (string) | unset | required `iss` claim value |
//! | `audience` (string) | unset | required `aud` claim value (string or array member) |
//! | `claims_header` (string) | unset | when set, REWRITE with this request header = the verified claims JSON |
//! | `require_https` (bool) | `true` | refuse to accept a session cookie on a cleartext request (loopback clients exempt) |

use std::net::IpAddr;

use ephpm_middleware::{Middleware, Request, Response};

use crate::hs256::{Hs256Policy, now_unix, opt_bool, opt_str};

/// Default cookie name when the mount does not set one.
const DEFAULT_COOKIE: &str = "ephpm_session";

/// Longest return-to target (path + query) we are willing to hand to the
/// login service. Anything longer is dropped — the user still reaches the
/// login page, just without a return-to. Well under the ~8 KB request-line
/// limit typical proxies enforce, so the redirect can never build a URL the
/// next hop refuses.
const MAX_RETURN_TO: usize = 2048;

/// Session-cookie gate policy, built once at `init`.
pub struct SessionCookie {
    /// Signature + registered-claim policy, shared with `jwt`.
    policy: Hs256Policy,
    cookie: String,
    login_url: String,
    return_to_param: Option<String>,
    site_param: Option<String>,
    claims_header: Option<String>,
    require_https: bool,
}

impl SessionCookie {
    /// Build the `Location` for an unauthenticated request.
    ///
    /// `return_to` is already sanitized by [`same_origin_return_to`]; both it
    /// and the vhost id are percent-encoded into the query, so nothing the
    /// client controls can add a parameter, a fragment, or a second URL.
    fn login_location(&self, return_to: Option<&str>, vhost: &str) -> String {
        let mut url = self.login_url.clone();
        // A login_url may already carry query parameters of its own.
        let mut sep = if url.contains('?') { '&' } else { '?' };
        if let (Some(param), Some(target)) = (self.return_to_param.as_deref(), return_to) {
            url.push(sep);
            url.push_str(&percent_encode(param));
            url.push('=');
            url.push_str(&percent_encode(target));
            sep = '&';
        }
        if let Some(param) = self.site_param.as_deref() {
            let site = normalize_vhost(vhost);
            if !site.is_empty() {
                url.push(sep);
                url.push_str(&percent_encode(param));
                url.push('=');
                url.push_str(&percent_encode(&site));
            }
        }
        url
    }

    /// The redirect verdict for an unauthenticated request.
    ///
    /// `302` for GET/HEAD, `303` for everything else: after an unauthenticated
    /// POST, `303` is what tells the browser to *GET* the login page rather
    /// than re-submit the form body to the identity service.
    ///
    /// `Cache-Control: no-store` keeps a shared cache from serving one user's
    /// redirect to another, and `Vary: Cookie` keeps any cache from treating
    /// the authenticated and unauthenticated responses for a URL as the same
    /// entry.
    fn redirect(&self, req: &Request<'_>) -> Response {
        let status = if matches!(req.method(), "GET" | "HEAD") { 302 } else { 303 };
        let return_to = same_origin_return_to(req.path(), req.query());
        Response::respond(status, "redirecting to login")
            .header("Location", self.login_location(return_to.as_deref(), req.vhost_id()))
            .header("Cache-Control", "no-store")
            .header("Vary", "Cookie")
    }
}

impl Middleware for SessionCookie {
    fn init(config: &serde_json::Value) -> Result<Self, String> {
        let login_url = opt_str(config, "login_url")?
            .ok_or("`login_url` is required (where to send unauthenticated browsers)")?;
        // Constrain the redirect target we will emit. A `javascript:` or
        // `data:` login_url would turn this module into an XSS primitive on
        // every unauthenticated request, so only real HTTP origins and
        // same-origin absolute paths are accepted. `//host` is rejected for
        // the same reason it is rejected in a return-to: it is a
        // protocol-relative URL, not a path.
        let absolute = login_url.starts_with("https://") || login_url.starts_with("http://");
        let same_origin_path = login_url.starts_with('/') && !login_url.starts_with("//");
        if !absolute && !same_origin_path {
            return Err(format!(
                "`login_url` must be an https:// or http:// URL, or a same-origin absolute path \
                 starting with `/` — got {login_url:?}"
            ));
        }
        let require_https = opt_bool(config, "require_https", true)?;
        Ok(Self {
            policy: Hs256Policy::from_config(config)?,
            cookie: opt_str(config, "cookie")?.unwrap_or_else(|| DEFAULT_COOKIE.to_owned()),
            login_url,
            return_to_param: opt_str(config, "return_to_param")?,
            site_param: opt_str(config, "site_param")?,
            claims_header: opt_str(config, "claims_header")?,
            require_https,
        })
    }

    fn invoke(&self, req: &Request<'_>) -> Response {
        // Transport check first: a session cookie presented over cleartext is
        // one an on-path observer has already read, so it is not evidence of
        // anything. Loopback is exempt because the web platform itself treats
        // `http://127.0.0.1` as a secure context — that is what makes local
        // development possible without an opt-out that then ships to prod.
        //
        // This is a hard stop, not a redirect: bouncing to an HTTPS login
        // service that sets a cookie the browser then returns over HTTP would
        // loop forever, and a loop is a worse diagnostic than a message.
        if self.require_https && !req.is_https() && !is_loopback(req.remote_ip()) {
            return Response::respond(
                403,
                "session cookie authentication requires HTTPS (see `require_https`)",
            )
            .header("Cache-Control", "no-store");
        }

        let Some(raw) = req.header("Cookie") else {
            return self.redirect(req);
        };

        // Try EVERY cookie of this name, not just the first. A cookie set on
        // a parent domain shadows one set on this host, so accepting only the
        // first match would let anyone who can write a cookie on the parent
        // domain lock every user out of the site. Each candidate still has to
        // pass the same HMAC and expiry check, so trying them all costs an
        // HMAC and grants nothing.
        let now = now_unix();
        let verified =
            cookie_values(raw, &self.cookie).find_map(|token| self.policy.verify(token, now));

        match verified {
            Some(claims_json) => match &self.claims_header {
                Some(name) => Response::rewrite().header(name.as_str(), claims_json),
                None => Response::cont(),
            },
            None => self.redirect(req),
        }
    }

    fn describe() -> &'static str {
        "session-cookie/1.0"
    }
}

/// Iterate the values of every cookie named `name` in a `Cookie` header.
///
/// RFC 6265 §4.2.1: the header is `name=value` pairs separated by `;`, with
/// optional surrounding whitespace. Two details a naive `split('=')` or a
/// `contains("name=")` check get wrong, and both are exploitable:
///
/// * a value may itself contain `=` (base64 padding, for one), so the split
///   is on the **first** `=` only;
/// * substring matching would let `evil_ephpm_session=...` satisfy a lookup
///   for `ephpm_session`, so names are compared **whole and exactly** —
///   cookie names are case-sensitive.
///
/// A matched pair of surrounding double quotes is stripped (RFC 6265's
/// `cookie-value` production permits them).
fn cookie_values<'a>(header: &'a str, name: &'a str) -> impl Iterator<Item = &'a str> {
    header.split(';').filter_map(move |pair| {
        let (raw_name, raw_value) = pair.split_once('=')?;
        if raw_name.trim_matches([' ', '\t']) != name {
            return None;
        }
        let value = raw_value.trim_matches([' ', '\t']);
        Some(match value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
            Some(unquoted) => unquoted,
            None => value,
        })
    })
}

/// Validate this request's own path+query for use as a return-to target,
/// returning `None` when it cannot be proven same-origin.
///
/// The target is never taken from a client-supplied parameter — it is the
/// path the browser actually asked for. That removes most of the open-redirect
/// surface by construction, but not all of it: the request line is still
/// client-controlled, and a login service that does `Location: <return_to>`
/// would happily follow a *protocol-relative* URL. `GET //evil.example/ HTTP/1.1`
/// is a legal request whose path is `//evil.example/`.
///
/// So the target must be an absolute path and nothing else:
///
/// * it starts with a single `/` — never `//` (protocol-relative) and never
///   `/\` or `\`, which browsers normalise to `/` and therefore treat exactly
///   like `//`;
/// * it contains no ASCII control character, space, or `DEL`, which could
///   split a header or truncate a URL, and no non-ASCII byte (paths reach us
///   percent-encoded);
/// * it fits in [`MAX_RETURN_TO`].
///
/// Anything else yields `None` and the user is simply sent to the login page
/// without a return-to — failing safe, never failing open. Percent-encoded
/// slashes (`/%2F%2Fevil.example`) survive validation and are harmless: they
/// stay a single-segment path when resolved, and a browser does not decode
/// `%2F` before deciding a URL's origin.
fn same_origin_return_to(path: &str, query: &str) -> Option<String> {
    if !path.starts_with('/') || path.starts_with("//") || path.starts_with("/\\") {
        return None;
    }
    if path.len() + query.len() + 1 > MAX_RETURN_TO {
        return None;
    }
    let unsafe_byte =
        |s: &str| s.bytes().any(|b| !(0x21..=0x7E).contains(&b) || b == b'\\' || b == b'#');
    if unsafe_byte(path) || unsafe_byte(query) {
        return None;
    }
    if query.is_empty() { Some(path.to_owned()) } else { Some(format!("{path}?{query}")) }
}

/// Percent-encode a string for use as a URL query component.
///
/// Everything outside the RFC 3986 unreserved set is encoded, `/` and `&` and
/// `=` and `#` included. Over-encoding is the point: whatever the login
/// service's URL parser does, a value produced here cannot introduce a
/// parameter, a fragment, or a second URL.
fn percent_encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    out
}

/// Normalise the ABI's vhost id into a stable site identity.
///
/// `request_vhost_id` is the router's `SERVER_NAME`: the `Host` header with
/// the port removed and **nothing else** — not lowercased, trailing
/// FQDN-root dot not stripped. `Host` is case-insensitive per RFC 9110 and
/// entirely client-controlled, so `Site.Example`, `site.example.` and
/// `site.example` are one site sending three different strings. Emitting
/// them raw would hand the login service three identities for one site, and
/// any exact-match lookup it does would miss for two of them.
///
/// This duplicates `ephpm-server`'s `normalize_host_key`, which is
/// `pub(crate)` and so unreachable from here; the duplication is deliberate
/// and must stay in step with it.
///
/// Normalising does **not** make the value trustworthy: an unrecognised
/// `Host` still reaches the middleware chain, so the login service must
/// validate the identity against its own registry rather than trusting it.
fn normalize_vhost(vhost: &str) -> String {
    vhost.split(':').next().unwrap_or("").trim_end_matches('.').to_ascii_lowercase()
}

/// Whether a client-IP string is a loopback address.
///
/// The host passes the client IP **after** trusted-proxy resolution, so this
/// is loopback only for a client genuinely on this machine (or one a trusted
/// proxy reported as such). An unparseable value is treated as not loopback —
/// the fail-closed direction.
fn is_loopback(remote_ip: &str) -> bool {
    remote_ip.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    #![allow(unsafe_code)] // tests build the FFI Request view by hand.

    use base64ct::{Base64UrlUnpadded, Encoding};
    use ephpm_middleware::abi::{ACTION_CONTINUE, ACTION_RESPOND, ACTION_REWRITE};
    use ephpm_middleware::host::{RequestCtx, host_table};
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    use super::*;

    const SECRET: &str = "test-secret-please-rotate";
    const LOGIN: &str = "https://login.example/start";

    /// Forge a token through the same HMAC code path the module verifies with.
    fn sign(secret: &str, header_json: &str, claims_json: &str) -> String {
        let h = Base64UrlUnpadded::encode_string(header_json.as_bytes());
        let p = Base64UrlUnpadded::encode_string(claims_json.as_bytes());
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
        mac.update(format!("{h}.{p}").as_bytes());
        let sig = Base64UrlUnpadded::encode_string(&mac.finalize().into_bytes());
        format!("{h}.{p}.{sig}")
    }

    fn token(secret: &str, claims_json: &str) -> String {
        sign(secret, r#"{"alg":"HS256","typ":"JWT"}"#, claims_json)
    }

    fn future_exp() -> u64 {
        now_unix() + 3600
    }

    fn valid_token() -> String {
        token(SECRET, &format!(r#"{{"sub":"octocat","exp":{}}}"#, future_exp()))
    }

    fn gate(mut config: serde_json::Value) -> SessionCookie {
        // Every test mount shares the secret and login URL unless it set them.
        let map = config.as_object_mut().expect("object config");
        map.entry("secret").or_insert_with(|| SECRET.into());
        map.entry("login_url").or_insert_with(|| LOGIN.into());
        SessionCookie::init(&config).expect("init")
    }

    /// Invoke over HTTPS from a non-loopback client — the production shape.
    fn invoke(mw: &SessionCookie, headers: &[(String, String)]) -> Response {
        invoke_full(mw, "GET", "/index.php", "", headers, true, "203.0.113.9")
    }

    fn invoke_full(
        mw: &SessionCookie,
        method: &str,
        path: &str,
        query: &str,
        headers: &[(String, String)],
        https: bool,
        ip: &str,
    ) -> Response {
        let ctx = RequestCtx::new(method, path, query, ip, "pr-42.preview.example", headers)
            .with_https(https);
        // SAFETY: `ctx` outlives the view; host_table() is 'static.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        mw.invoke(&req)
    }

    fn cookie(value: &str) -> Vec<(String, String)> {
        vec![("Cookie".to_owned(), value.to_owned())]
    }

    fn header_of<'a>(resp: &'a Response, name: &str) -> Option<&'a str> {
        resp.__headers().iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    /// Assert the verdict is a redirect to the login service, and return the
    /// `Location` so the caller can inspect the query it carries.
    fn assert_redirect(resp: &Response) -> String {
        assert_eq!(resp.__action(), ACTION_RESPOND, "unauthenticated must short-circuit");
        assert_eq!(resp.__status(), 302);
        assert_eq!(header_of(resp, "Cache-Control"), Some("no-store"));
        assert_eq!(header_of(resp, "Vary"), Some("Cookie"));
        header_of(resp, "Location").expect("Location header").to_owned()
    }

    // ── config ───────────────────────────────────────────────────────────

    #[test]
    fn init_requires_secret_and_login_url() {
        assert!(SessionCookie::init(&serde_json::json!({ "secret": SECRET })).is_err());
        assert!(SessionCookie::init(&serde_json::json!({ "login_url": LOGIN })).is_err());
        assert!(
            SessionCookie::init(&serde_json::json!({ "secret": "", "login_url": LOGIN })).is_err()
        );
    }

    #[test]
    fn init_rejects_a_dangerous_login_url() {
        // A `javascript:` login_url would be an XSS primitive fired on every
        // unauthenticated request; `//host` is protocol-relative, not a path.
        for bad in
            ["javascript:alert(1)", "data:text/html,x", "//evil.example/login", "evil.example"]
        {
            let cfg = serde_json::json!({ "secret": SECRET, "login_url": bad });
            assert!(SessionCookie::init(&cfg).is_err(), "{bad} must be rejected");
        }
        for good in ["https://login.example/start", "http://login.local/start", "/login.php"] {
            let cfg = serde_json::json!({ "secret": SECRET, "login_url": good });
            assert!(SessionCookie::init(&cfg).is_ok(), "{good} must be accepted");
        }
    }

    #[test]
    fn config_absent_keys_take_their_documented_defaults() {
        // "section absent": only the two required keys are set. The cookie
        // name defaults, require_https defaults ON, and no return-to or site
        // parameter is emitted unless asked for.
        let mw = gate(serde_json::json!({}));
        assert_eq!(mw.cookie, DEFAULT_COOKIE);
        assert!(mw.require_https, "require_https must default to ON");
        assert_eq!(mw.login_location(Some("/a"), "site"), LOGIN, "no params unless configured");
        let resp = invoke(&mw, &cookie(&format!("{DEFAULT_COOKIE}={}", valid_token())));
        assert_eq!(resp.__action(), ACTION_CONTINUE, "the default cookie name is honoured");
    }

    #[test]
    fn config_present_keys_are_all_honoured() {
        let mw = gate(serde_json::json!({
            "cookie": "sb_session",
            "return_to_param": "next",
            "site_param": "site",
            "claims_header": "X-Session-Claims",
            "require_https": false,
        }));
        assert_eq!(mw.cookie, "sb_session");
        assert!(!mw.require_https);
        let resp = invoke_full(&mw, "GET", "/a.php", "b=1", &[], true, "203.0.113.9");
        let location = assert_redirect(&resp);
        assert_eq!(location, format!("{LOGIN}?next=%2Fa.php%3Fb%3D1&site=pr-42.preview.example"));
    }

    #[test]
    fn a_mistyped_require_https_fails_startup() {
        // `require_https = "false"` (a string) must not be quietly read as
        // "set, therefore truthy" nor as false — it fails the mount.
        let cfg =
            serde_json::json!({ "secret": SECRET, "login_url": LOGIN, "require_https": "false" });
        assert!(SessionCookie::init(&cfg).is_err());
    }

    // ── the happy path ───────────────────────────────────────────────────

    #[test]
    fn valid_cookie_continues_to_php() {
        let mw = gate(serde_json::json!({}));
        let resp = invoke(&mw, &cookie(&format!("ephpm_session={}", valid_token())));
        assert_eq!(resp.__action(), ACTION_CONTINUE);
    }

    #[test]
    fn valid_cookie_forwards_claims_when_configured() {
        let mw = gate(serde_json::json!({ "claims_header": "X-Session-Claims" }));
        let claims = format!(r#"{{"sub":"octocat","exp":{}}}"#, future_exp());
        let resp = invoke(&mw, &cookie(&format!("ephpm_session={}", token(SECRET, &claims))));
        assert_eq!(resp.__action(), ACTION_REWRITE);
        assert_eq!(header_of(&resp, "X-Session-Claims"), Some(claims.as_str()));
    }

    // ── the rejection paths ──────────────────────────────────────────────

    #[test]
    fn missing_cookie_header_redirects() {
        assert_redirect(&invoke(&gate(serde_json::json!({})), &[]));
    }

    #[test]
    fn a_differently_named_cookie_redirects() {
        let mw = gate(serde_json::json!({}));
        let t = valid_token();
        assert_redirect(&invoke(&mw, &cookie(&format!("other={t}"))));
        // Substring matching would accept this; whole-name matching must not.
        assert_redirect(&invoke(&mw, &cookie(&format!("evil_ephpm_session={t}"))));
        assert_redirect(&invoke(&mw, &cookie(&format!("ephpm_session_x={t}"))));
    }

    #[test]
    fn expired_cookie_redirects() {
        let mw = gate(serde_json::json!({}));
        let expired = token(SECRET, r#"{"sub":"octocat","exp":1000}"#);
        assert_redirect(&invoke(&mw, &cookie(&format!("ephpm_session={expired}"))));
    }

    #[test]
    fn a_cookie_without_exp_redirects() {
        // A session token that cannot expire is a config bug, not a session.
        let mw = gate(serde_json::json!({}));
        let no_exp = token(SECRET, r#"{"sub":"octocat"}"#);
        assert_redirect(&invoke(&mw, &cookie(&format!("ephpm_session={no_exp}"))));
    }

    #[test]
    fn tampered_signature_redirects_and_the_tamper_is_actually_detected() {
        let mw = gate(serde_json::json!({}));
        let good = valid_token();

        // Control: the untampered token IS accepted, so a redirect below
        // proves detection rather than the token never having been valid.
        assert_eq!(
            invoke(&mw, &cookie(&format!("ephpm_session={good}"))).__action(),
            ACTION_CONTINUE
        );

        let mut parts = good.split('.');
        let (h, p, s) =
            (parts.next().expect("h"), parts.next().expect("p"), parts.next().expect("s"));

        // 1. Payload swapped for a privilege-escalating one, signature kept.
        let forged = Base64UrlUnpadded::encode_string(
            format!(r#"{{"sub":"root","exp":{}}}"#, future_exp()).as_bytes(),
        );
        assert_redirect(&invoke(&mw, &cookie(&format!("ephpm_session={h}.{forged}.{s}"))));

        // 2. One flipped character in the signature.
        let flipped = if s.ends_with('A') { 'B' } else { 'A' };
        let tampered = format!("{}{flipped}", &s[..s.len() - 1]);
        assert_redirect(&invoke(&mw, &cookie(&format!("ephpm_session={h}.{p}.{tampered}"))));

        // 3. Signed with a different key.
        let other = token("attacker-key", &format!(r#"{{"sub":"root","exp":{}}}"#, future_exp()));
        assert_redirect(&invoke(&mw, &cookie(&format!("ephpm_session={other}"))));

        // 4. `alg: none` with a signature that verifies under our key.
        let alg_none = sign(SECRET, r#"{"alg":"none"}"#, &format!(r#"{{"exp":{}}}"#, future_exp()));
        assert_redirect(&invoke(&mw, &cookie(&format!("ephpm_session={alg_none}"))));

        // 5. Structurally broken tokens.
        for junk in ["", "not-a-jwt", "a.b.c.d", "..", "e30.e30."] {
            assert_redirect(&invoke(&mw, &cookie(&format!("ephpm_session={junk}"))));
        }
    }

    #[test]
    fn a_non_get_request_redirects_with_303() {
        // 303 makes the browser GET the login page instead of re-POSTing the
        // form body to the identity service.
        let mw = gate(serde_json::json!({}));
        let resp = invoke_full(&mw, "POST", "/submit.php", "", &[], true, "203.0.113.9");
        assert_eq!(resp.__action(), ACTION_RESPOND);
        assert_eq!(resp.__status(), 303);
        assert!(header_of(&resp, "Location").is_some());
    }

    // ── cookie parsing ───────────────────────────────────────────────────

    #[test]
    fn cookie_parsing_handles_real_browser_headers() {
        let mw = gate(serde_json::json!({}));
        let t = valid_token();
        for header in [
            // Surrounded by other cookies, with the usual "; " separator.
            format!("_ga=GA1.1.9; ephpm_session={t}; wordpress_test_cookie=WP"),
            // No space after the separator.
            format!("a=1;ephpm_session={t};b=2"),
            // Leading/trailing whitespace and tabs around the pair.
            format!("a=1; \t ephpm_session={t} \t ; b=2"),
            // RFC 6265 permits a quoted cookie-value.
            format!("ephpm_session=\"{t}\""),
            // Last cookie in the header, no trailing separator.
            format!("a=1; ephpm_session={t}"),
        ] {
            assert_eq!(
                invoke(&mw, &cookie(&header)).__action(),
                ACTION_CONTINUE,
                "must find the cookie in {header:?}"
            );
        }
    }

    #[test]
    fn a_cookie_value_containing_equals_survives_parsing() {
        // Splitting on every `=` instead of the first would truncate this.
        let padded = format!("{}==", valid_token());
        assert_eq!(
            cookie_values(&format!("ephpm_session={padded}"), "ephpm_session").next(),
            Some(padded.as_str())
        );
        // And a value that is only `=` signs still parses as a value.
        assert_eq!(cookie_values("a==b=c", "a").next(), Some("=b=c"));
    }

    #[test]
    fn a_shadowing_cookie_cannot_lock_a_user_out() {
        // An attacker who can set `ephpm_session` on a PARENT domain gets it
        // sent first, shadowing the real one. Taking only the first match
        // would be a trivial denial of service; every candidate is tried.
        let mw = gate(serde_json::json!({}));
        let t = valid_token();
        let shadowed = format!("ephpm_session=garbage-from-parent-domain; ephpm_session={t}");
        assert_eq!(invoke(&mw, &cookie(&shadowed)).__action(), ACTION_CONTINUE);
        // ...and a valid token first, junk second, also works.
        let reversed = format!("ephpm_session={t}; ephpm_session=junk");
        assert_eq!(invoke(&mw, &cookie(&reversed)).__action(), ACTION_CONTINUE);
        // Two invalid ones still redirect — trying all candidates must not
        // become "accept anything".
        assert_redirect(&invoke(&mw, &cookie("ephpm_session=junk; ephpm_session=more-junk")));
    }

    #[test]
    fn malformed_cookie_headers_do_not_panic_or_admit() {
        let mw = gate(serde_json::json!({}));
        for header in ["", ";", ";;;", "=", "=value", "ephpm_session", "ephpm_session;", "   "] {
            assert_redirect(&invoke(&mw, &cookie(header)));
        }
    }

    // ── return-to (open redirect) ────────────────────────────────────────

    #[test]
    fn return_to_carries_the_requested_path_and_query() {
        let mw = gate(serde_json::json!({ "return_to_param": "next" }));
        let resp =
            invoke_full(&mw, "GET", "/wp-admin/edit.php", "post=7&x=a b", &[], true, "203.0.113.9");
        // Space is not a legal request-target byte; such a query is dropped
        // rather than encoded, so this one yields no `next` at all.
        assert_eq!(assert_redirect(&resp), LOGIN);

        let resp =
            invoke_full(&mw, "GET", "/wp-admin/edit.php", "post=7", &[], true, "203.0.113.9");
        assert_eq!(
            assert_redirect(&resp),
            format!("{LOGIN}?next=%2Fwp-admin%2Fedit.php%3Fpost%3D7")
        );
    }

    #[test]
    fn hostile_return_to_targets_are_rejected() {
        // Every one of these is a legal request target that a login service
        // doing `Location: <next>` would follow off-origin, or that could
        // break out of the query parameter. None may ever appear in `next`.
        let hostile = [
            "//evil.example/",              // protocol-relative
            "///evil.example/",             // three slashes, still off-origin
            "//evil.example/path?a=b",      //
            "/\\evil.example/",             // browsers normalise \ to /
            "/\\/evil.example",             //
            "\\\\evil.example",             // does not start with / at all
            "https://evil.example/",        // absolute URL
            "http:/evil.example",           //
            "javascript:alert(1)",          // XSS payload
            "/x\nLocation:%20https://evil", // header injection attempt
            "/x\r\nSet-Cookie:%20a=b",      //
            "/x y",                         // raw space truncates a URL
            "/x#@evil.example",             // fragment/userinfo confusion
            "/\u{7f}",                      // DEL
            "/\u{e9}vil",                   // raw non-ASCII
            "",                             // empty
            "relative/path",                // not absolute
        ];
        let mw = gate(serde_json::json!({ "return_to_param": "next" }));
        for target in hostile {
            assert_eq!(
                same_origin_return_to(target, ""),
                None,
                "{target:?} must not be usable as a return-to"
            );
            // ...and end to end: the emitted Location carries no `next`.
            let resp = invoke_full(&mw, "GET", target, "", &[], true, "203.0.113.9");
            let location = assert_redirect(&resp);
            assert_eq!(location, LOGIN, "{target:?} leaked into {location}");
        }
        // A hostile QUERY is caught too, even with an innocuous path.
        assert_eq!(same_origin_return_to("/ok.php", "a=b\r\nX: y"), None);
        assert_eq!(same_origin_return_to("/ok.php", "a=#b"), None);

        // A NUL is rejected here...
        assert_eq!(same_origin_return_to("/x\u{0}y", ""), None);
        // ...but never reaches a module as one: the host strips interior NULs
        // when it builds the request context, so end to end this arrives as
        // the perfectly ordinary path `/xy` and is returned to as such. Both
        // layers are checked because either alone would be load-bearing.
        let resp = invoke_full(&mw, "GET", "/x\u{0}y", "", &[], true, "203.0.113.9");
        assert_eq!(assert_redirect(&resp), format!("{LOGIN}?next=%2Fxy"));
    }

    #[test]
    fn a_percent_encoded_slash_is_safe_to_return_to() {
        // `/%2F%2Fevil.example` is a single-segment path, not a
        // protocol-relative URL: a browser does not decode %2F before
        // deciding origin, so this one is allowed through — and it comes back
        // double-encoded, so it cannot decode into `//` at the login service.
        let mw = gate(serde_json::json!({ "return_to_param": "next" }));
        let resp = invoke_full(&mw, "GET", "/%2F%2Fevil.example", "", &[], true, "203.0.113.9");
        assert_eq!(assert_redirect(&resp), format!("{LOGIN}?next=%2F%252F%252Fevil.example"));
    }

    #[test]
    fn an_overlong_return_to_is_dropped_not_truncated() {
        let long = format!("/{}", "a".repeat(MAX_RETURN_TO));
        assert_eq!(same_origin_return_to(&long, ""), None);
    }

    #[test]
    fn return_to_appends_to_a_login_url_that_already_has_a_query() {
        let mw = gate(serde_json::json!({
            "login_url": "https://login.example/start?tenant=acme",
            "return_to_param": "next",
            "site_param": "site",
        }));
        let resp = invoke_full(&mw, "GET", "/a.php", "", &[], true, "203.0.113.9");
        assert_eq!(
            assert_redirect(&resp),
            "https://login.example/start?tenant=acme&next=%2Fa.php&site=pr-42.preview.example"
        );
    }

    #[test]
    fn the_site_identity_lets_one_login_service_serve_many_sites() {
        let mw = gate(serde_json::json!({ "site_param": "site" }));
        let resp = invoke(&mw, &[]);
        assert_eq!(assert_redirect(&resp), format!("{LOGIN}?site=pr-42.preview.example"));
    }

    #[test]
    fn the_site_identity_is_normalized_before_it_is_emitted() {
        // `request_vhost_id` is the raw `Host` minus the port: not
        // lowercased, trailing FQDN-root dot not stripped. `Host` is
        // case-insensitive and client-controlled, so without normalising,
        // one site would present the login service with several identities
        // and any exact-match lookup there would miss.
        for raw in [
            "PR-42.Preview.Example",
            "pr-42.preview.example.",
            "PR-42.PREVIEW.EXAMPLE.",
            "pr-42.preview.example",
        ] {
            assert_eq!(normalize_vhost(raw), "pr-42.preview.example", "{raw}");
        }
        // A vhost that normalises away emits no parameter at all rather than
        // an empty one.
        let mw = gate(serde_json::json!({ "site_param": "site" }));
        assert_eq!(mw.login_location(None, ""), LOGIN);
        assert_eq!(mw.login_location(None, "."), LOGIN);
    }

    // ── transport ────────────────────────────────────────────────────────

    #[test]
    fn cleartext_from_a_remote_client_is_refused_by_default() {
        let mw = gate(serde_json::json!({}));
        let resp = invoke_full(
            &mw,
            "GET",
            "/index.php",
            "",
            &cookie(&format!("ephpm_session={}", valid_token())),
            false,
            "203.0.113.9",
        );
        assert_eq!(resp.__action(), ACTION_RESPOND);
        assert_eq!(resp.__status(), 403, "a valid cookie over cleartext is still refused");
    }

    #[test]
    fn cleartext_from_loopback_is_allowed_so_local_development_works() {
        let mw = gate(serde_json::json!({}));
        for ip in ["127.0.0.1", "::1"] {
            let resp = invoke_full(
                &mw,
                "GET",
                "/index.php",
                "",
                &cookie(&format!("ephpm_session={}", valid_token())),
                false,
                ip,
            );
            assert_eq!(resp.__action(), ACTION_CONTINUE, "loopback {ip} is a secure context");
        }
        // And a loopback client with no cookie still gets the redirect, not
        // the 403 — the transport check must not swallow the normal path.
        let resp = invoke_full(&mw, "GET", "/index.php", "", &[], false, "127.0.0.1");
        assert_redirect(&resp);
    }

    #[test]
    fn require_https_false_allows_cleartext_from_anywhere() {
        let mw = gate(serde_json::json!({ "require_https": false }));
        let resp = invoke_full(
            &mw,
            "GET",
            "/index.php",
            "",
            &cookie(&format!("ephpm_session={}", valid_token())),
            false,
            "203.0.113.9",
        );
        assert_eq!(resp.__action(), ACTION_CONTINUE);
    }

    #[test]
    fn an_unparseable_client_ip_is_not_treated_as_loopback() {
        assert!(!is_loopback(""));
        assert!(!is_loopback("localhost"));
        assert!(!is_loopback("127.0.0.1, 203.0.113.9"));
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("127.99.1.2"));
        assert!(is_loopback("::1"));
        assert!(!is_loopback("203.0.113.9"));
    }

    #[test]
    fn a_forged_x_forwarded_proto_does_not_satisfy_require_https() {
        // The header reaches modules exactly as the client sent it; only the
        // host's own verdict counts.
        let mw = gate(serde_json::json!({}));
        let headers = vec![
            ("X-Forwarded-Proto".to_owned(), "https".to_owned()),
            ("Cookie".to_owned(), format!("ephpm_session={}", valid_token())),
        ];
        let resp = invoke_full(&mw, "GET", "/i.php", "", &headers, false, "203.0.113.9");
        assert_eq!(resp.__status(), 403, "a client-set X-Forwarded-Proto must not unlock this");
    }
}
