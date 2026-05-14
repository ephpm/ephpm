//! KV store native function tests.
//!
//! Validates the ephpm_kv_* PHP extension functions:
//! - set/get round-trip
//! - TTL expiry
//! - counter increment across requests
//! - del + exists
//! - pttl returns -2 for missing keys
//!
//! All tests use distinct keys to avoid inter-test interference.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use std::time::Duration;

use ephpm_e2e::required_env;

/// Helper: call /kv.php with the given query string, return trimmed body.
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

#[tokio::test]
async fn kv_set_get_roundtrip() {
    let base = required_env("EPHPM_URL");

    let (s, _) = kv(&base, "op=set&key=rtrip&val=hello_world&ttl=0").await;
    assert_eq!(s, 200);

    let (s, body) = kv(&base, "op=get&key=rtrip").await;
    assert_eq!(s, 200);
    assert_eq!(body, "hello_world", "get must return the value that was set");
}

#[tokio::test(flavor = "current_thread")]
async fn kv_ttl_expiry() {
    let base = required_env("EPHPM_URL");

    // ephpm_kv_set takes the TTL in seconds (Redis convention), so the
    // shortest expiry we can request is 1 s. Sleep slightly longer to
    // tolerate scheduler jitter without ever being flaky early.
    let (s, _) = kv(&base, "op=set&key=ttl_exp&val=present&ttl=1").await;
    assert_eq!(s, 200);

    tokio::time::sleep(Duration::from_millis(1200)).await;

    let (s, body) = kv(&base, "op=get&key=ttl_exp").await;
    assert_eq!(s, 200);
    assert_eq!(body, "null", "key should have expired after TTL elapsed");
}

#[tokio::test]
async fn kv_incr_atomic() {
    let base = required_env("EPHPM_URL");

    // Reset counter so the test is idempotent across runs
    kv(&base, "op=del&key=incr_ctr").await;

    for i in 1u32..=5 {
        let (s, body) = kv(&base, "op=incr&key=incr_ctr").await;
        assert_eq!(s, 200);
        assert_eq!(
            body,
            i.to_string(),
            "counter should be {i} after {i} increments"
        );
    }
}

#[tokio::test]
async fn kv_del_and_exists() {
    let base = required_env("EPHPM_URL");

    kv(&base, "op=set&key=del_ex&val=present&ttl=0").await;

    let (_, body) = kv(&base, "op=exists&key=del_ex").await;
    assert_eq!(body, "1", "key should exist immediately after set");

    kv(&base, "op=del&key=del_ex").await;

    let (_, body) = kv(&base, "op=exists&key=del_ex").await;
    assert_eq!(body, "0", "key should not exist after del");
}

#[tokio::test]
async fn kv_incr_by_delta() {
    let base = required_env("EPHPM_URL");

    kv(&base, "op=del&key=incr_by_key").await;

    for i in 1u32..=5 {
        let (s, body) = kv(&base, "op=incr_by&key=incr_by_key&val=10").await;
        assert_eq!(s, 200);
        assert_eq!(
            body,
            (i * 10).to_string(),
            "after {i} incr_by(10) calls counter should be {}",
            i * 10
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn kv_expire_extends_ttl() {
    let base = required_env("EPHPM_URL");

    // Set with a 50 ms TTL, then extend before it expires
    kv(&base, "op=set&key=exp_ext&val=alive&ttl=50").await;
    kv(&base, "op=expire&key=exp_ext&ttl=10000").await; // extend to 10 s

    // Wait past the original 50 ms
    tokio::time::sleep(Duration::from_millis(150)).await;

    let (_, body) = kv(&base, "op=get&key=exp_ext").await;
    assert_eq!(
        body, "alive",
        "key must still exist after TTL was extended via expire"
    );
}

#[tokio::test]
async fn kv_pttl_positive_for_live_key() {
    let base = required_env("EPHPM_URL");

    kv(&base, "op=set&key=live_pttl&val=x&ttl=60000").await; // 60 s TTL

    let (s, body) = kv(&base, "op=pttl&key=live_pttl").await;
    assert_eq!(s, 200);
    let pttl: i64 = body
        .parse()
        .unwrap_or_else(|_| panic!("pttl must be an integer, got: {body}"));
    assert!(
        pttl > 0,
        "pttl must be positive for a key with an active TTL, got {pttl}"
    );
}

#[tokio::test]
async fn kv_setnx_does_not_overwrite() {
    let base = required_env("EPHPM_URL");

    kv(&base, "op=del&key=setnx_key").await;

    // First setnx — key does not exist, must set and return 1
    let (_, body) = kv(&base, "op=setnx&key=setnx_key&val=original").await;
    assert_eq!(body, "1", "setnx must return 1 when key did not exist");

    // Second setnx — key exists, must not overwrite and return 0
    let (_, body) = kv(&base, "op=setnx&key=setnx_key&val=overwritten").await;
    assert_eq!(body, "0", "setnx must return 0 when key already exists");

    // Value must still be the original
    let (_, body) = kv(&base, "op=get&key=setnx_key").await;
    assert_eq!(
        body, "original",
        "setnx must not overwrite an existing key"
    );
}

#[tokio::test]
async fn kv_mset_mget_roundtrip() {
    let base = required_env("EPHPM_URL");

    // mset encodes pairs as "k1:v1,k2:v2,k3:v3"
    let (s, _) = kv(
        &base,
        "op=mset&val=mget_a:alpha,mget_b:bravo,mget_c:charlie",
    )
    .await;
    assert_eq!(s, 200);

    // mget returns newline-separated values in key order
    let (s, body) = kv(&base, "op=mget&key=mget_a,mget_b,mget_c").await;
    assert_eq!(s, 200);
    let values: Vec<&str> = body.lines().collect();
    assert_eq!(
        values,
        vec!["alpha", "bravo", "charlie"],
        "mget must return values in the same order as the requested keys"
    );
}

#[tokio::test]
async fn kv_pttl_returns_minus_two_for_missing() {
    let base = required_env("EPHPM_URL");

    let (s, body) = kv(&base, "op=pttl&key=nosuchkey_pttl_test").await;
    assert_eq!(s, 200);
    assert_eq!(
        body, "-2",
        "ephpm_kv_pttl must return -2 for a key that does not exist"
    );
}

#[tokio::test]
async fn kv_empty_string_value() {
    let base = required_env("EPHPM_URL");

    // Set an empty string value
    let (s, _) = kv(&base, "op=set&key=empty_val&val=&ttl=0").await;
    assert_eq!(s, 200);

    let (s, body) = kv(&base, "op=get&key=empty_val").await;
    assert_eq!(s, 200);
    assert_eq!(
        body, "",
        "get of a key set to empty string must return empty, got: {body:?}"
    );

    // Verify the key exists (not the same as missing)
    let (_, body) = kv(&base, "op=exists&key=empty_val").await;
    assert_eq!(
        body, "1",
        "key with empty string value must still exist"
    );
}

#[tokio::test]
async fn kv_large_value() {
    let base = required_env("EPHPM_URL");

    // ~10KB value
    let large = "A".repeat(10_000);
    let encoded = urlencoding::encode(&large);
    let (s, _) = kv(&base, &format!("op=set&key=large_val&val={encoded}&ttl=0")).await;
    assert_eq!(s, 200);

    let (s, body) = kv(&base, "op=get&key=large_val").await;
    assert_eq!(s, 200);
    assert_eq!(
        body.len(),
        10_000,
        "retrieved value must be 10000 bytes, got {}",
        body.len()
    );
    assert_eq!(
        body, large,
        "retrieved large value must match what was stored"
    );
}

#[tokio::test]
async fn kv_overwrite_returns_latest() {
    let base = required_env("EPHPM_URL");

    let (s, _) = kv(&base, "op=set&key=overwrite_key&val=first&ttl=0").await;
    assert_eq!(s, 200);

    let (s, _) = kv(&base, "op=set&key=overwrite_key&val=second&ttl=0").await;
    assert_eq!(s, 200);

    let (s, body) = kv(&base, "op=get&key=overwrite_key").await;
    assert_eq!(s, 200);
    assert_eq!(
        body, "second",
        "get after overwrite must return the latest value"
    );
}
