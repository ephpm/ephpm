//! Integration proof for the `ephpm php --site` KV wire transport (#700),
//! exercising the two halves that live in different crates together: the
//! server's internal loopback KV endpoint ([`ephpm_server::internal_kv`]) and
//! the CLI's blocking RESP client ([`ephpm_php::kv_remote`]).
//!
//! This is the runnable, binary-free core of the feature's test matrix — it
//! proves the mechanism the e2e CLI tests then exercise end to end:
//!
//! - **Headline / redis_compat OFF:** a client write over the internal endpoint
//!   lands in the *serving process's* per-site store, with no public
//!   `[kv.redis_compat]` listener configured (the config leaves it off).
//! - **Per-site isolation:** a client authenticated as site A cannot see a key
//!   another client wrote for site B, and each write lands only in its own
//!   tenant's store.
//! - **Fail closed:** `internal_kv::start` returns `None` (no endpoint) for a
//!   single-site config and for a multi-site config with no `[kv] secret`.
//!
//! The RESP client is blocking, so every use of it runs under
//! `spawn_blocking` — blocking the test's own task would starve the endpoint's
//! accept loop on the runtime.

use std::sync::Arc;

use ephpm_config::Config;
use ephpm_kv::multi_tenant::MultiTenantStore;
use ephpm_kv::store::{Store, StoreConfig};
use ephpm_php::kv_remote::{RemoteKvClient, RemoteKvEndpoint};

const SECRET: &str = "internal-kv-wire-test-secret";

/// A multi-tenant config with vhosts `alpha` and `beta` and `[kv] secret` set,
/// but **no** `[kv.redis_compat]` listener (it stays at its disabled default).
fn multi_site_config(secret: Option<&str>) -> (Config, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let sites = tmp.path().join("sites");
    std::fs::create_dir_all(sites.join("alpha")).expect("alpha dir");
    std::fs::create_dir_all(sites.join("beta")).expect("beta dir");
    let mut config = Config::default();
    config.server.sites_dir = Some(sites);
    config.kv.secret = secret.map(ToString::to_string);
    // redis_compat.enabled stays false (the default) — this is the whole point.
    assert!(!config.kv.redis_compat.enabled, "the public listener must be off for this proof");
    (config, tmp)
}

/// Build the store + per-vhost store the server would build.
fn stores() -> (Arc<Store>, MultiTenantStore) {
    let store = Store::new(StoreConfig::default());
    let mt = MultiTenantStore::new(Arc::clone(&store), StoreConfig::default());
    (store, mt)
}

/// Connect a blocking client to `addr` as `site` and run `f`; all on the
/// blocking pool so the endpoint's accept loop keeps running.
async fn with_client<T, F>(addr: std::net::SocketAddr, site: &str, f: F) -> T
where
    F: FnOnce(&RemoteKvClient) -> T + Send + 'static,
    T: Send + 'static,
{
    let site = site.to_string();
    tokio::task::spawn_blocking(move || {
        let endpoint = RemoteKvEndpoint {
            addr,
            user: Some(site.clone()),
            password: Some(ephpm_kv::auth::derive_site_password(SECRET, &site)),
        };
        let client = RemoteKvClient::connect(endpoint).expect("connect + AUTH");
        f(&client)
    })
    .await
    .expect("blocking task")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_write_lands_in_the_serving_per_site_store_without_redis_compat() {
    let (config, _tmp) = multi_site_config(Some(SECRET));
    let (store, mt) = stores();
    let endpoint = ephpm_server::internal_kv::start(&config, &store, Some(&mt))
        .expect("internal endpoint must bind for a multi-site + secret config");
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], endpoint.port()));

    // The CLI (as site `alpha`) writes a key over the wire.
    let set_ok = with_client(addr, "alpha", |client| {
        matches!(
            client.command(&[b"SET", b"warm:cache", b"hello"]),
            Ok(ephpm_kv::resp::Frame::Simple(_))
        )
    })
    .await;
    assert!(set_ok, "the CLI SET must succeed over the internal endpoint");

    // The value is visible to the serving process — the exact `Arc<Store>` an
    // HTTP request's `ephpm_kv_get('warm:cache')` would read for `alpha`.
    assert_eq!(
        mt.get_site_store("alpha").get("warm:cache").as_deref(),
        Some(&b"hello"[..]),
        "the CLI write must be visible in the serving process's per-site store"
    );

    // And the CLI can read it back over the wire.
    let read_back =
        with_client(addr, "alpha", |client| match client.command(&[b"GET", b"warm:cache"]) {
            Ok(ephpm_kv::resp::Frame::Bulk(b)) => Some(b.to_vec()),
            _ => None,
        })
        .await;
    assert_eq!(read_back.as_deref(), Some(&b"hello"[..]), "the CLI must read its own write back");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_site_isolation_a_cannot_see_bs_keys() {
    let (config, _tmp) = multi_site_config(Some(SECRET));
    let (store, mt) = stores();
    let endpoint = ephpm_server::internal_kv::start(&config, &store, Some(&mt)).expect("endpoint");
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], endpoint.port()));

    // `beta` writes a key only `beta` should see.
    let beta_set = with_client(addr, "beta", |client| {
        matches!(
            client.command(&[b"SET", b"secret", b"beta-only"]),
            Ok(ephpm_kv::resp::Frame::Simple(_))
        )
    })
    .await;
    assert!(beta_set);

    // `alpha`, authenticating with its own derived password, reads a miss.
    let alpha_sees =
        with_client(addr, "alpha", |client| match client.command(&[b"GET", b"secret"]) {
            Ok(ephpm_kv::resp::Frame::Bulk(b)) => Some(b.to_vec()),
            _ => None,
        })
        .await;
    assert_eq!(alpha_sees, None, "site A must not read a key written for site B");

    // The stores confirm it landed only in beta's keyspace.
    assert_eq!(mt.get_site_store("beta").get("secret").as_deref(), Some(&b"beta-only"[..]));
    assert!(mt.get_site_store("alpha").get("secret").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_password_is_rejected() {
    let (config, _tmp) = multi_site_config(Some(SECRET));
    let (store, mt) = stores();
    let endpoint = ephpm_server::internal_kv::start(&config, &store, Some(&mt)).expect("endpoint");
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], endpoint.port()));

    let result = tokio::task::spawn_blocking(move || {
        let endpoint = RemoteKvEndpoint {
            addr,
            user: Some("alpha".into()),
            password: Some("not-the-derived-password".into()),
        };
        RemoteKvClient::connect(endpoint).is_ok()
    })
    .await
    .expect("blocking task");
    assert!(!result, "a connection with a bad per-site password must be refused");
}

#[test]
fn start_returns_none_without_a_secret() {
    let (config, _tmp) = multi_site_config(None);
    let (store, mt) = stores();
    // No runtime needed: it returns before binding anything.
    let endpoint = ephpm_server::internal_kv::start(&config, &store, Some(&mt));
    assert!(endpoint.is_none(), "no [kv] secret ⇒ no internal endpoint (fail closed)");
}

#[test]
fn start_returns_none_for_single_site() {
    let mut config = Config::default();
    config.kv.secret = Some(SECRET.to_string());
    // No sites_dir → single-site; the CLI wire is not offered.
    let (store, mt) = stores();
    let endpoint = ephpm_server::internal_kv::start(&config, &store, Some(&mt));
    assert!(endpoint.is_none(), "single-site ⇒ no internal endpoint");
}
