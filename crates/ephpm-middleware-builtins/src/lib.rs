//! The in-tree official ePHPm middleware modules as plain Rust library
//! code.
//!
//! Each module here is an ordinary [`ephpm_middleware::Middleware`]
//! implementation with **no C ABI exports** — that is what lets
//! `ephpm-server` link all of them into one binary and run them in-process
//! through the static builtin registry (`library = "jwt"` works even in a
//! custom fully static build, where `dlopen` does not exist).
//!
//! The modules, by phase:
//!
//! - **Request phase** ([`ephpm_middleware::Middleware`]): [`jwt`],
//!   [`session_cookie`], [`cors`], [`ratelimit`], [`security_headers`],
//!   [`api_key`], [`ip_allowlist`], [`maintenance_mode`], [`redirect`].
//! - **Request + response phase** (also
//!   [`ephpm_middleware::ResponseMiddleware`], registered in the server via
//!   [`ephpm_middleware::builtin::BuiltinModule::init_response`]):
//!   [`request_id`], [`header_transform`].
//!
//! [`hs256`] is not a middleware: it is the token-verification core that
//! [`jwt`] (API bearer tokens) and [`session_cookie`] (browser sessions) both
//! call, so the two gates can differ in policy and failure behaviour without
//! ever differing in crypto. [`session_cookie`] additionally enforces a
//! per-tenant `site` binding (issue #396) so a session minted for one preview
//! cannot open another.
//!
//! The sibling `ephpm-middleware-<name>` crates in the `ephpm/middleware`
//! (examples) repository are thin cdylib shells: they re-export these types
//! and add the `declare!` C ABI glue, producing the loadable
//! `.so`/`.dylib`/`.dll` artifacts for the dynamic (dlopen) lane. The shells
//! cannot be merged into one binary — many copies of the same
//! `ephpm_middleware_*` export symbols collide at link time — which is exactly
//! why the implementations live here instead.

//! ## Config strictness
//!
//! Every module here declares its accepted `config` keys via
//! [`ephpm_middleware::Middleware::CONFIG_KEYS`], so a mount carrying a key
//! none of them reads is a **startup error** naming the mount, the key and the
//! nearest accepted spelling — not a silent no-op running the default (issue
//! #473). These ten ship inside the binary that enforces the check, so there
//! is no cross-version skew to tolerate; a third-party module keeps the
//! lenient default until it declares its own keys. See
//! [`ephpm_middleware::config`].

pub mod api_key;
pub mod cors;
pub mod header_transform;
pub mod hs256;
pub mod ip_allowlist;
pub mod jwt;
pub mod maintenance_mode;
pub mod ratelimit;
pub mod redirect;
pub mod request_id;
pub mod security_headers;
pub mod session_cookie;

#[cfg(test)]
mod config_strictness_tests {
    use ephpm_middleware::{Middleware, init_checked};
    use serde_json::json;

    /// Assert the whole contract for one module: the control config is
    /// accepted, the same config with one extra key is refused, the message
    /// names that key, and the key set is declared at all.
    ///
    /// The control matters — without it a "rejects unknown keys" assertion
    /// passes just as well for a module that refuses every config it is given.
    fn strict<T: Middleware>(control: &serde_json::Value, typo: &str) {
        let name = std::any::type_name::<T>();
        assert!(T::CONFIG_KEYS.is_some(), "{name} declares no config keys");
        if let Err(e) = init_checked::<T>(control).map(|_| ()) {
            panic!("control config for {name} was rejected: {e}");
        }

        let mut bad = control.clone();
        bad.as_object_mut().expect("object control").insert(typo.to_owned(), json!("x"));
        let err = init_checked::<T>(&bad).map(|_| ()).expect_err("unknown key must be refused");
        assert!(err.contains(&format!("`{typo}`")), "message must name the key: {err}");
    }

    /// All ten in-tree builtins, each with a minimally valid config and a
    /// realistic misspelling of one of its own keys. A new builtin added
    /// without a `CONFIG_KEYS` declaration fails here as soon as it is listed,
    /// and the list is checked against the server's registry by
    /// `ephpm_server::middleware::tests::every_builtin_rejects_an_unknown_config_key`.
    #[test]
    fn every_builtin_rejects_an_unknown_config_key() {
        strict::<crate::api_key::ApiKey>(&json!({ "keys": { "k": "consumer" } }), "kv_key_tmplate");
        strict::<crate::cors::Cors>(&json!({ "allow_origins": ["*"] }), "allow_credential");
        strict::<crate::header_transform::HeaderTransform>(
            &json!({ "response": { "set": { "X-A": "b" } } }),
            "responses",
        );
        strict::<crate::ip_allowlist::IpAllowlist>(&json!({ "allow": ["10.0.0.0/8"] }), "alow");
        strict::<crate::jwt::Jwt>(&json!({ "secret": "s3cret" }), "issue");
        strict::<crate::maintenance_mode::MaintenanceMode>(&json!({}), "bypass_ip");
        strict::<crate::ratelimit::RateLimit>(&json!({ "per_ip_rps": 50 }), "per_ip_rp");
        strict::<crate::redirect::Redirect>(&json!({ "force_https": true }), "cannonical_host");
        strict::<crate::request_id::RequestId>(&json!({ "header": "X-Rid" }), "trust_inbound_");
        strict::<crate::security_headers::SecurityHeaders>(
            &json!({ "hsts": true }),
            "referer_policy",
        );
        strict::<crate::session_cookie::SessionCookie>(
            &json!({ "secret": "s3cret", "login_url": "https://login.example/start" }),
            "require_sites",
        );
    }

    /// The check runs *before* the module's own `init`, so a typo is reported
    /// even when the rest of the payload is unusable. Without this ordering the
    /// operator's first fix ("add the required key") would then surface a
    /// second, unrelated error.
    #[test]
    fn the_unknown_key_is_reported_before_a_missing_required_key() {
        let err = init_checked::<crate::ratelimit::RateLimit>(&json!({ "per_ip_rp": 50 }))
            .map(|_| ())
            .expect_err("must fail");
        assert!(err.contains("unknown config key `per_ip_rp`"), "{err}");
        assert!(err.contains("did you mean `per_ip_rps`"), "{err}");
        assert!(!err.contains("is required"), "the typo, not the consequence: {err}");
    }
}
