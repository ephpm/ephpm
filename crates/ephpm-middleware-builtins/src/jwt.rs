//! `jwt` — ePHPm native middleware validating HS256 JWT bearer tokens
//! before PHP runs.
//!
//! v1 supports **HS256 only** (HMAC-SHA256 via the `hmac`/`sha2` crates — no
//! heavyweight JWT dependency). A missing or malformed token short-circuits
//! with `401`; a valid token continues to PHP, optionally forwarding the raw
//! claims JSON in a request header (`claims_header`) so PHP can read them
//! without re-verifying.
//!
//! Verification: constant-time HMAC check (`hmac::Mac::verify_slice`), the
//! token's `alg` must be `HS256`, `exp` is **required** and must be in the
//! future, `nbf` is honoured when present, and `iss`/`aud` are enforced when
//! configured.
//!
//! Configuration (`[[middleware]] config = { ... }`):
//!
//! | key | default | meaning |
//! |-----|---------|---------|
//! | `secret` (string) | **required** | HS256 shared secret |
//! | `issuer` (string) | unset | required `iss` claim value |
//! | `audience` (string) | unset | required `aud` claim value (string or array member) |
//! | `header` (string) | `"Authorization"` | request header carrying the token; a `Bearer ` prefix is stripped |
//! | `claims_header` (string) | unset | when set, REWRITE with this request header = the raw claims JSON |

use std::time::{SystemTime, UNIX_EPOCH};

use base64ct::{Base64UrlUnpadded, Encoding};
use ephpm_middleware::{Middleware, Request, Response};
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// JWT validation policy, built once at `init`.
pub struct Jwt {
    secret: Vec<u8>,
    issuer: Option<String>,
    audience: Option<String>,
    header: String,
    claims_header: Option<String>,
}

/// Strip an optional (case-insensitive) `Bearer` prefix. A bare `Bearer`
/// with nothing after it yields the empty string (= missing token).
fn strip_bearer(value: &str) -> &str {
    let trimmed = value.trim();
    if let (Some(scheme), Some(rest)) = (trimmed.get(..6), trimmed.get(6..)) {
        if scheme.eq_ignore_ascii_case("bearer") && (rest.is_empty() || rest.starts_with(' ')) {
            return rest.trim_start();
        }
    }
    trimmed
}

impl Jwt {
    /// Verify `token` against this policy at time `now` (unix seconds).
    /// Returns the raw claims JSON on success, `None` on any failure.
    fn verify(&self, token: &str, now: u64) -> Option<String> {
        let mut parts = token.split('.');
        let (header_b64, payload_b64, sig_b64) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() {
            return None;
        }

        // Signature first — never parse unauthenticated JSON.
        let sig = Base64UrlUnpadded::decode_vec(sig_b64).ok()?;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.secret).ok()?;
        mac.update(header_b64.as_bytes());
        mac.update(b".");
        mac.update(payload_b64.as_bytes());
        // Constant-time comparison via the hmac crate.
        mac.verify_slice(&sig).ok()?;

        // The signature is ours, but still pin the algorithm: HS256 only.
        let header: serde_json::Value =
            serde_json::from_slice(&Base64UrlUnpadded::decode_vec(header_b64).ok()?).ok()?;
        if header.get("alg").and_then(serde_json::Value::as_str) != Some("HS256") {
            return None;
        }

        let payload = Base64UrlUnpadded::decode_vec(payload_b64).ok()?;
        let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;

        // `exp` is required — a token that cannot expire is a config bug.
        let exp = claims.get("exp").and_then(serde_json::Value::as_u64)?;
        if exp <= now {
            return None;
        }
        if let Some(nbf) = claims.get("nbf") {
            if nbf.as_u64()? > now {
                return None;
            }
        }
        if let Some(expected) = &self.issuer {
            if claims.get("iss").and_then(serde_json::Value::as_str) != Some(expected.as_str()) {
                return None;
            }
        }
        if let Some(expected) = &self.audience {
            let ok = match claims.get("aud") {
                Some(serde_json::Value::String(aud)) => aud == expected,
                Some(serde_json::Value::Array(auds)) => {
                    auds.iter().any(|a| a.as_str() == Some(expected.as_str()))
                }
                _ => false,
            };
            if !ok {
                return None;
            }
        }

        String::from_utf8(payload).ok()
    }
}

impl Middleware for Jwt {
    fn init(config: &serde_json::Value) -> Result<Self, String> {
        let secret = config
            .get("secret")
            .ok_or("`secret` is required (HS256 shared secret)")?
            .as_str()
            .ok_or("`secret` must be a string")?;
        if secret.is_empty() {
            return Err("`secret` must not be empty".into());
        }
        let opt_str = |key: &str| -> Result<Option<String>, String> {
            match config.get(key) {
                Some(serde_json::Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
                None | Some(serde_json::Value::Null | serde_json::Value::String(_)) => Ok(None),
                Some(other) => Err(format!("`{key}` must be a string, got {other}")),
            }
        };
        Ok(Self {
            secret: secret.as_bytes().to_vec(),
            issuer: opt_str("issuer")?,
            audience: opt_str("audience")?,
            header: opt_str("header")?.unwrap_or_else(|| "Authorization".to_owned()),
            claims_header: opt_str("claims_header")?,
        })
    }

    fn invoke(&self, req: &Request<'_>) -> Response {
        let Some(raw) = req.header(&self.header) else {
            return Response::respond(401, "missing bearer token");
        };
        let token = strip_bearer(raw);
        if token.is_empty() {
            return Response::respond(401, "missing bearer token");
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        match self.verify(token, now) {
            Some(claims_json) => match &self.claims_header {
                Some(name) => Response::rewrite().header(name.as_str(), claims_json),
                None => Response::cont(),
            },
            None => Response::respond(401, "invalid token"),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(unsafe_code)] // tests build the FFI Request view by hand.

    use ephpm_middleware::abi::{ACTION_CONTINUE, ACTION_RESPOND, ACTION_REWRITE};
    use ephpm_middleware::host::{RequestCtx, host_table};

    use super::*;

    const SECRET: &str = "test-secret-please-rotate";

    /// Forge a token through the same HMAC code path the module verifies
    /// with (independent of `Jwt::verify`'s parsing).
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
        SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_secs() + 3600
    }

    fn jwt(config: serde_json::Value) -> Jwt {
        Jwt::init(&config).expect("init")
    }

    fn invoke(mw: &Jwt, headers: &[(String, String)]) -> Response {
        let ctx = RequestCtx::new("GET", "/api/x", "", "203.0.113.9", "example.test", headers);
        // SAFETY: `ctx` outlives the view; host_table() is 'static.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        mw.invoke(&req)
    }

    fn bearer(token: &str) -> Vec<(String, String)> {
        vec![("Authorization".to_owned(), format!("Bearer {token}"))]
    }

    fn assert_401(resp: &Response, body: &str) {
        assert_eq!(resp.__action(), ACTION_RESPOND);
        assert_eq!(resp.__status(), 401);
        assert_eq!(resp.__body(), body.as_bytes());
    }

    #[test]
    fn init_requires_secret() {
        assert!(Jwt::init(&serde_json::Value::Null).is_err());
        assert!(Jwt::init(&serde_json::json!({ "secret": "" })).is_err());
        assert!(Jwt::init(&serde_json::json!({ "secret": 42 })).is_err());
    }

    #[test]
    fn valid_token_continues() {
        let mw = jwt(serde_json::json!({ "secret": SECRET }));
        let t = token(SECRET, &format!(r#"{{"sub":"u1","exp":{}}}"#, future_exp()));
        let resp = invoke(&mw, &bearer(&t));
        assert_eq!(resp.__action(), ACTION_CONTINUE);
    }

    #[test]
    fn valid_token_with_claims_header_rewrites() {
        let mw = jwt(serde_json::json!({ "secret": SECRET, "claims_header": "X-Jwt-Claims" }));
        let claims = format!(r#"{{"sub":"u1","exp":{}}}"#, future_exp());
        let resp = invoke(&mw, &bearer(&token(SECRET, &claims)));
        assert_eq!(resp.__action(), ACTION_REWRITE);
        let forwarded =
            resp.__headers().iter().find(|(n, _)| n == "X-Jwt-Claims").map(|(_, v)| v.as_str());
        assert_eq!(forwarded, Some(claims.as_str()));
    }

    #[test]
    fn missing_header_is_401() {
        let mw = jwt(serde_json::json!({ "secret": SECRET }));
        assert_401(&invoke(&mw, &[]), "missing bearer token");
        assert_401(&invoke(&mw, &bearer("")), "missing bearer token");
    }

    #[test]
    fn bad_signature_is_401() {
        let mw = jwt(serde_json::json!({ "secret": SECRET }));
        let t = token("wrong-secret", &format!(r#"{{"exp":{}}}"#, future_exp()));
        assert_401(&invoke(&mw, &bearer(&t)), "invalid token");
    }

    #[test]
    fn malformed_token_is_401() {
        let mw = jwt(serde_json::json!({ "secret": SECRET }));
        assert_401(&invoke(&mw, &bearer("not-a-jwt")), "invalid token");
        assert_401(&invoke(&mw, &bearer("a.b.c.d")), "invalid token");
    }

    #[test]
    fn expired_or_missing_exp_is_401() {
        let mw = jwt(serde_json::json!({ "secret": SECRET }));
        assert_401(&invoke(&mw, &bearer(&token(SECRET, r#"{"exp":1000}"#))), "invalid token");
        assert_401(&invoke(&mw, &bearer(&token(SECRET, r#"{"sub":"u1"}"#))), "invalid token");
    }

    #[test]
    fn nbf_is_honoured() {
        let mw = jwt(serde_json::json!({ "secret": SECRET }));
        let exp = future_exp(); // now + 3600
        // nbf in the past: valid.
        let past = token(SECRET, &format!(r#"{{"exp":{exp},"nbf":{}}}"#, exp - 3700));
        assert_eq!(invoke(&mw, &bearer(&past)).__action(), ACTION_CONTINUE);
        // nbf still in the future: rejected.
        let future = token(SECRET, &format!(r#"{{"exp":{exp},"nbf":{exp}}}"#));
        assert_401(&invoke(&mw, &bearer(&future)), "invalid token");
    }

    #[test]
    fn wrong_issuer_is_401() {
        let mw = jwt(serde_json::json!({ "secret": SECRET, "issuer": "auth.example" }));
        let exp = future_exp();
        let good = token(SECRET, &format!(r#"{{"exp":{exp},"iss":"auth.example"}}"#));
        assert_eq!(invoke(&mw, &bearer(&good)).__action(), ACTION_CONTINUE);
        let bad = token(SECRET, &format!(r#"{{"exp":{exp},"iss":"evil.example"}}"#));
        assert_401(&invoke(&mw, &bearer(&bad)), "invalid token");
        let none = token(SECRET, &format!(r#"{{"exp":{exp}}}"#));
        assert_401(&invoke(&mw, &bearer(&none)), "invalid token");
    }

    #[test]
    fn audience_string_or_array_is_enforced() {
        let mw = jwt(serde_json::json!({ "secret": SECRET, "audience": "api" }));
        let exp = future_exp();
        let s = token(SECRET, &format!(r#"{{"exp":{exp},"aud":"api"}}"#));
        assert_eq!(invoke(&mw, &bearer(&s)).__action(), ACTION_CONTINUE);
        let arr = token(SECRET, &format!(r#"{{"exp":{exp},"aud":["web","api"]}}"#));
        assert_eq!(invoke(&mw, &bearer(&arr)).__action(), ACTION_CONTINUE);
        let bad = token(SECRET, &format!(r#"{{"exp":{exp},"aud":"web"}}"#));
        assert_401(&invoke(&mw, &bearer(&bad)), "invalid token");
    }

    #[test]
    fn alg_none_is_rejected_even_with_valid_hmac() {
        let mw = jwt(serde_json::json!({ "secret": SECRET }));
        let t = sign(SECRET, r#"{"alg":"none"}"#, &format!(r#"{{"exp":{}}}"#, future_exp()));
        assert_401(&invoke(&mw, &bearer(&t)), "invalid token");
    }

    #[test]
    fn custom_header_without_bearer_prefix_works() {
        let mw = jwt(serde_json::json!({ "secret": SECRET, "header": "X-Auth-Token" }));
        let t = token(SECRET, &format!(r#"{{"exp":{}}}"#, future_exp()));
        let resp = invoke(&mw, &[("X-Auth-Token".to_owned(), t)]);
        assert_eq!(resp.__action(), ACTION_CONTINUE);
    }
}
