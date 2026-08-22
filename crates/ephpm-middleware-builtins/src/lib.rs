//! The five in-tree ePHPm middleware modules as plain Rust library code.
//!
//! Each module here is an ordinary [`ephpm_middleware::Middleware`]
//! implementation with **no C ABI exports** — that is what lets
//! `ephpm-server` link all of them into one binary and run them in-process
//! through the static builtin registry (`library = "jwt"` works even in a
//! custom fully static build, where `dlopen` does not exist).
//!
//! The sibling `ephpm-middleware-{jwt,cors,ratelimit,security-headers,basicauth}`
//! crates are thin cdylib shells: they re-export these types and add the
//! `declare!` C ABI glue, producing the loadable `.so`/`.dylib`/`.dll`
//! artifacts for the dynamic (dlopen) lane. The shells cannot be merged into
//! one binary — five copies of the same `ephpm_middleware_*` export symbols
//! collide at link time — which is exactly why the implementations live
//! here instead.

pub mod basicauth;
pub mod cors;
pub mod jwt;
pub mod password_hash;
pub mod ratelimit;
pub mod security_headers;

/// Shared KV wiring for this crate's unit tests.
///
/// [`ephpm_middleware::host::set_kv_store`] is a `OnceLock`: the first caller
/// in a test binary wins and every later call is a silent no-op. Since
/// `basicauth` and `ratelimit` both need a live store, they must agree on one
/// instance — otherwise whichever module's test ran first would own the store
/// and the other would silently exercise the "KV unavailable" path. Tests keep
/// their keys apart by vhost/prefix rather than by owning separate stores.
#[cfg(test)]
mod test_kv {
    use std::sync::{Arc, OnceLock};

    use ephpm_kv::store::{Store, StoreConfig};

    /// Wire the shared in-memory store into the host table and hand it back,
    /// so a test can also reach it directly (e.g. to delete a key — the v1
    /// middleware ABI exposes get/set/incr but no delete).
    pub fn wire() -> &'static Arc<Store> {
        static SHARED: OnceLock<Arc<Store>> = OnceLock::new();
        let store = SHARED.get_or_init(|| Store::new(StoreConfig::default()));
        ephpm_middleware::host::set_kv_store(store);
        store
    }
}
