//! KV store advanced configuration tests.
//!
//! Validates that `[kv]` settings (`memory_limit`, `eviction_policy`,
//! `compression`) are accepted and that the KV store remains functional
//! under a non-default configuration.
//!
//! The test config sets:
//! - `memory_limit = "64MB"`
//! - `eviction_policy = "allkeys-lru"`
//!
//! These tests do NOT attempt to hit the memory ceiling — that belongs in
//! nightly stress tests. They verify the config is accepted and basic
//! operations still work.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

/// Helper: call /kv.php with the given query string, return (status, trimmed body).
async fn kv(base_url: &str, query: &str) -> (u16, String) {
    let url = format!("{base_url}/kv.php?{query}");
    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));
    let status = resp.status().as_u16();
    let body = resp
        .text()
        .await
        .expect("failed to read kv response body")
        .trim()
        .to_owned();
    (status, body)
}

/// Basic set/get with the custom memory_limit + eviction_policy config.
///
/// If the config parsing broke (e.g. unrecognised eviction policy string),
/// the server would fail to start and this test would not reach 200.
#[tokio::test]
async fn kv_set_get_with_custom_config() {
    let base = required_env("EPHPM_URL");

    let (s, _) = kv(&base, "op=set&key=adv_cfg&val=configured&ttl=0").await;
    assert_eq!(s, 200, "set must succeed with custom KV config");

    let (s, body) = kv(&base, "op=get&key=adv_cfg").await;
    assert_eq!(s, 200);
    assert_eq!(
        body, "configured",
        "get must return the stored value under custom KV config"
    );
}

/// Write several moderately-sized values and read them back.
///
/// This exercises the memory-tracking path (each set updates
/// `memory_used`) and the LRU bookkeeping (`last_accessed` update on
/// read) without coming close to the 64 MB ceiling.
#[tokio::test]
async fn kv_multiple_keys_with_memory_tracking() {
    let base = required_env("EPHPM_URL");

    // Write 20 keys, each ~1 KB (well under the 64 MB limit).
    let value = "V".repeat(1_000);
    let encoded = urlencoding::encode(&value);

    for i in 0..20 {
        let (s, _) = kv(
            &base,
            &format!("op=set&key=adv_bulk_{i}&val={encoded}&ttl=0"),
        )
        .await;
        assert_eq!(s, 200, "set adv_bulk_{i} must succeed");
    }

    // Read them all back.
    for i in 0..20 {
        let (s, body) = kv(&base, &format!("op=get&key=adv_bulk_{i}")).await;
        assert_eq!(s, 200);
        assert_eq!(
            body.len(),
            1_000,
            "adv_bulk_{i} must return 1000-byte value, got {}",
            body.len()
        );
    }
}

/// Overwrite the same key many times to exercise the memory-accounting
/// delta path (old entry size subtracted, new entry size added).
#[tokio::test]
async fn kv_overwrite_updates_memory_accounting() {
    let base = required_env("EPHPM_URL");

    // Start with a small value.
    let (s, _) = kv(&base, "op=set&key=adv_ow&val=small&ttl=0").await;
    assert_eq!(s, 200);

    // Overwrite with a larger value.
    let large = "L".repeat(5_000);
    let encoded = urlencoding::encode(&large);
    let (s, _) = kv(
        &base,
        &format!("op=set&key=adv_ow&val={encoded}&ttl=0"),
    )
    .await;
    assert_eq!(s, 200);

    // Read back — must be the large value.
    let (s, body) = kv(&base, "op=get&key=adv_ow").await;
    assert_eq!(s, 200);
    assert_eq!(
        body.len(),
        5_000,
        "overwritten key must return the latest (larger) value"
    );

    // Overwrite again with a smaller value.
    let (s, _) = kv(&base, "op=set&key=adv_ow&val=tiny&ttl=0").await;
    assert_eq!(s, 200);

    let (s, body) = kv(&base, "op=get&key=adv_ow").await;
    assert_eq!(s, 200);
    assert_eq!(
        body, "tiny",
        "overwritten key must return the latest (smaller) value"
    );
}

/// Delete keys and verify they are removed, exercising the memory
/// reclamation path.
#[tokio::test]
async fn kv_delete_reclaims_memory() {
    let base = required_env("EPHPM_URL");

    let value = "D".repeat(2_000);
    let encoded = urlencoding::encode(&value);

    // Set 5 keys.
    for i in 0..5 {
        let (s, _) = kv(
            &base,
            &format!("op=set&key=adv_del_{i}&val={encoded}&ttl=0"),
        )
        .await;
        assert_eq!(s, 200);
    }

    // Delete them.
    for i in 0..5 {
        let (s, _) = kv(&base, &format!("op=del&key=adv_del_{i}")).await;
        assert_eq!(s, 200);
    }

    // Confirm they are gone.
    for i in 0..5 {
        let (_, body) = kv(&base, &format!("op=exists&key=adv_del_{i}")).await;
        assert_eq!(
            body, "0",
            "adv_del_{i} must not exist after deletion"
        );
    }
}
