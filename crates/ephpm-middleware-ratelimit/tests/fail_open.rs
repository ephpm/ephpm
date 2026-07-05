//! Fail-open behaviour when the KV store is unavailable.
//!
//! This lives in its own integration-test binary (= its own process) because
//! the host KV store is process-global: the unit tests wire one in, and once
//! set it cannot be unset. Here `set_kv_store` is never called, so every
//! `kv_incr` fails — the limiter must let requests through.
#![allow(unsafe_code)] // builds the FFI Request view by hand, like the unit tests.

use ephpm_middleware::Middleware;
use ephpm_middleware::abi::ACTION_CONTINUE;
use ephpm_middleware::host::{RequestCtx, host_table};
use ephpm_middleware_ratelimit::RateLimit;

#[test]
fn kv_unavailable_fails_open() {
    let mw = RateLimit::init(&serde_json::json!({ "per_ip_rps": 1, "burst": 0 })).expect("init");
    let ctx = RequestCtx::new("GET", "/api/x", "", "198.51.100.9", "vhost-open", &[]);
    // SAFETY: `ctx` outlives the view; host_table() is 'static.
    let req = unsafe { ephpm_middleware::Request::from_raw(ctx.as_abi(), host_table()) };
    // Way past the allowance — every single one must still continue.
    for _ in 0..50 {
        assert_eq!(mw.invoke(&req).__action(), ACTION_CONTINUE);
    }
}
