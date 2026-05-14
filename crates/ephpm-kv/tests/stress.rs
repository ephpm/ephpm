//! Stress tests for the KV store.
//!
//! All tests are `#[ignore]` so they only run during nightly CI via
//! `cargo test -- --run-ignored` (or `cargo nextest --run-ignored`).
//! They exercise concurrency, TTL expiry under load, compression
//! round-trips at scale, and multi-tenant isolation.

use std::sync::Arc;
use std::time::Duration;

use ephpm_kv::auth::derive_site_password;
use ephpm_kv::multi_tenant::MultiTenantStore;
use ephpm_kv::server;
use ephpm_kv::store::{CompressionAlgo, CompressionConfig, Store, StoreConfig};
use redis::AsyncCommands;
use tokio::net::TcpListener;

const WRITERS: usize = 50;
const OPS_PER_WRITER: usize = 1_000;
const TTL_KEY_COUNT: usize = 200;
const COMPRESSION_KEY_COUNT: usize = 1_000;
const TENANTS: usize = 5;
const KEYS_PER_TENANT: usize = 200;

/// Assert helper: compare DBSIZE (i64) against an expected usize count.
fn assert_dbsize(actual: i64, expected: usize, label: &str) {
    let expected_i64 = i64::try_from(expected).expect("count exceeds i64::MAX");
    assert_eq!(actual, expected_i64, "{label}: expected {expected}, got {actual}");
}

// ── Test harness ─────────────────────────────────────────────────────────────

struct TestServer {
    addr: String,
    handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with_config(StoreConfig::default()).await
    }

    async fn start_with_config(config: StoreConfig) -> Self {
        let listener =
            TcpListener::bind("127.0.0.1:0").await.expect("failed to bind test listener");
        let addr = listener.local_addr().expect("failed to get local addr").to_string();

        let store = Store::new(config);
        let handle = tokio::spawn(async move {
            server::serve_on(store, listener, 64 * 1024 * 1024, None, None, None).await.ok();
        });

        // Give the accept loop a moment to become ready.
        tokio::time::sleep(Duration::from_millis(10)).await;

        Self { addr, handle }
    }

    async fn con(&self) -> redis::aio::MultiplexedConnection {
        let url = format!("redis://{}/", self.addr);
        redis::Client::open(url)
            .expect("invalid redis URL")
            .get_multiplexed_async_connection()
            .await
            .expect("failed to connect to test server")
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

struct HmacTestServer {
    addr: String,
    handle: tokio::task::JoinHandle<()>,
    secret: String,
}

impl HmacTestServer {
    async fn start(secret: &str) -> Self {
        let listener =
            TcpListener::bind("127.0.0.1:0").await.expect("failed to bind test listener");
        let addr = listener.local_addr().expect("failed to get local addr").to_string();

        let store = Store::new(StoreConfig::default());
        let mt = MultiTenantStore::new(Arc::clone(&store), StoreConfig::default());
        let sec = Some(secret.to_string());
        let handle = tokio::spawn(async move {
            server::serve_on(store, listener, 64 * 1024 * 1024, None, sec, Some(mt)).await.ok();
        });

        tokio::time::sleep(Duration::from_millis(10)).await;

        Self { addr, handle, secret: secret.to_string() }
    }

    /// Open a raw (unauthenticated) connection to be manually AUTH'd.
    async fn raw_con(&self) -> redis::aio::MultiplexedConnection {
        let url = format!("redis://{}/", self.addr);
        redis::Client::open(url)
            .expect("invalid redis URL")
            .get_multiplexed_async_connection()
            .await
            .expect("failed to connect to test server")
    }
}

impl Drop for HmacTestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

// ── 1. concurrent_writer_storm ──────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[ignore = "nightly-only stress test"]
async fn concurrent_writer_storm() {
    let srv = TestServer::start().await;
    let addr = srv.addr.clone();

    let mut handles = Vec::with_capacity(WRITERS);

    for writer_id in 0..WRITERS {
        let url = format!("redis://{addr}/");
        handles.push(tokio::spawn(async move {
            let client = redis::Client::open(url).expect("invalid redis URL");
            let mut con =
                client.get_multiplexed_async_connection().await.expect("failed to connect");

            for i in 0..OPS_PER_WRITER {
                let key = format!("w{writer_id}:k{i}");
                let val = format!("v{writer_id}:{i}");
                let _: () = con.set(&key, &val).await.expect("SET failed");
                let got: String = con.get(&key).await.expect("GET failed");
                assert_eq!(got, val, "mismatch for key {key}");
            }
        }));
    }

    for h in handles {
        h.await.expect("writer task panicked");
    }

    // Verify all 50,000 keys are readable from a single connection.
    let mut con = srv.con().await;
    let total: i64 = redis::cmd("DBSIZE").query_async(&mut con).await.unwrap();
    assert_dbsize(total, WRITERS * OPS_PER_WRITER, "writer storm total");

    // Spot-check a sample of keys from different writers.
    for writer_id in [0, 24, 49] {
        for i in [0, 500, 999] {
            let key = format!("w{writer_id}:k{i}");
            let expected = format!("v{writer_id}:{i}");
            let got: String = con.get(&key).await.unwrap();
            assert_eq!(got, expected, "spot-check failed for {key}");
        }
    }
}

// ── 2. ttl_expiry_under_load ────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[ignore = "nightly-only stress test"]
async fn ttl_expiry_under_load() {
    let srv = TestServer::start().await;
    let mut con = srv.con().await;

    // Set keys with TTLs between 200ms and 1000ms.
    for i in 0..TTL_KEY_COUNT {
        let key = format!("ttl:{i}");
        let ms = 200 + (i * 4); // 200ms..996ms
        let _: () = redis::cmd("SET")
            .arg(&key)
            .arg(format!("val{i}"))
            .arg("PX")
            .arg(ms)
            .query_async(&mut con)
            .await
            .unwrap();
    }

    // Verify they all exist initially.
    let initial: i64 = redis::cmd("DBSIZE").query_async(&mut con).await.unwrap();
    assert_dbsize(initial, TTL_KEY_COUNT, "TTL keys initial");

    // Poll until all keys are expired. The reaper runs every 1s with a sample
    // size of 100, so we need to give it enough cycles. Maximum TTL is ~1s,
    // plus reaper delay — 10s total timeout is very generous.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        tokio::time::sleep(Duration::from_millis(250)).await;

        let remaining: i64 = redis::cmd("DBSIZE").query_async(&mut con).await.unwrap();
        if remaining == 0 {
            break;
        }

        assert!(
            tokio::time::Instant::now() < deadline,
            "{remaining} keys still alive after 10s — expiry reaper may be stuck",
        );
    }

    // Final verification: every key should return nil.
    for i in 0..TTL_KEY_COUNT {
        let key = format!("ttl:{i}");
        let got: Option<String> = con.get(&key).await.unwrap();
        assert!(got.is_none(), "key {key} should have expired");
    }
}

// ── 3. compression_round_trip_at_scale ──────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[ignore = "nightly-only stress test"]
async fn compression_round_trip_at_scale() {
    // Enable gzip compression with a low min_size so our test values get
    // compressed.
    let config = StoreConfig {
        memory_limit: 0, // unlimited for this test
        compression: CompressionConfig {
            algo: CompressionAlgo::Gzip,
            level: 6,
            min_size: 64, // compress anything >= 64 bytes
        },
        ..StoreConfig::default()
    };
    let srv = TestServer::start_with_config(config).await;
    let mut con = srv.con().await;

    // Build compressible values: repeated patterns of varying lengths.
    let mut expected_values: Vec<(String, Vec<u8>)> = Vec::with_capacity(COMPRESSION_KEY_COUNT);
    for i in 0..COMPRESSION_KEY_COUNT {
        let key = format!("cmp:{i}");
        // Create a value with repeated patterns that compresses well.
        // Vary the length (128 to ~1152 bytes) and the pattern.
        let pattern = format!("data-{i}-payload-");
        let repeats = 8 + (i % 64);
        let value: Vec<u8> = pattern.repeat(repeats).into_bytes();
        expected_values.push((key, value));
    }

    // SET all keys.
    for (key, value) in &expected_values {
        let _: () = con.set(key.as_str(), value.as_slice()).await.unwrap();
    }

    // GET all keys and verify byte-for-byte equality.
    for (key, expected) in &expected_values {
        let got: Vec<u8> = con.get(key.as_str()).await.unwrap();
        assert_eq!(
            got.len(),
            expected.len(),
            "length mismatch for key {key}: got {} expected {}",
            got.len(),
            expected.len(),
        );
        assert_eq!(&got, expected, "data mismatch for key {key}");
    }

    let total: i64 = redis::cmd("DBSIZE").query_async(&mut con).await.unwrap();
    assert_dbsize(total, COMPRESSION_KEY_COUNT, "compression total");
}

// ── 4. multi_tenant_isolation ───────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[ignore = "nightly-only stress test"]
async fn multi_tenant_isolation() {
    let srv = HmacTestServer::start("stress-test-secret").await;

    let hostnames: Vec<String> = (0..TENANTS).map(|i| format!("tenant{i}.example.com")).collect();

    // Each tenant writes 200 keys via its own authenticated connection.
    let mut handles = Vec::with_capacity(TENANTS);
    for (tenant_idx, hostname) in hostnames.iter().enumerate() {
        let addr = srv.addr.clone();
        let secret = srv.secret.clone();
        let hostname = hostname.clone();
        handles.push(tokio::spawn(async move {
            let password = derive_site_password(&secret, &hostname);
            let client =
                redis::Client::open(format!("redis://{addr}/")).expect("invalid redis URL");
            let mut con =
                client.get_multiplexed_async_connection().await.expect("failed to connect");

            // Authenticate.
            let _: String = redis::cmd("AUTH")
                .arg(&hostname)
                .arg(&password)
                .query_async(&mut con)
                .await
                .expect("AUTH failed");

            // Write keys.
            for i in 0..KEYS_PER_TENANT {
                let key = format!("key:{i}");
                let val = format!("tenant{tenant_idx}:val{i}");
                let _: () = con.set(&key, &val).await.expect("SET failed");
            }

            // Read keys back to verify own data.
            for i in 0..KEYS_PER_TENANT {
                let key = format!("key:{i}");
                let expected = format!("tenant{tenant_idx}:val{i}");
                let got: String = con.get(&key).await.expect("GET failed");
                assert_eq!(got, expected, "tenant {hostname} read-back mismatch for {key}");
            }

            // DBSIZE should reflect only this tenant's keys.
            let size: i64 = redis::cmd("DBSIZE").query_async(&mut con).await.unwrap();
            assert_dbsize(size, KEYS_PER_TENANT, &format!("tenant {hostname}"));
        }));
    }

    for h in handles {
        h.await.expect("tenant task panicked");
    }

    // Cross-tenant isolation check: each tenant should NOT see another
    // tenant's data. Connect as each tenant and verify only its own values.
    for (checker_idx, hostname) in hostnames.iter().enumerate() {
        let password = derive_site_password(&srv.secret, hostname);
        let mut con = srv.raw_con().await;
        let _: String = redis::cmd("AUTH")
            .arg(hostname.as_str())
            .arg(&password)
            .query_async(&mut con)
            .await
            .unwrap();

        for i in 0..KEYS_PER_TENANT {
            let key = format!("key:{i}");
            let got: String = con.get(&key).await.unwrap();
            let expected = format!("tenant{checker_idx}:val{i}");
            assert_eq!(
                got, expected,
                "isolation breach: {hostname} got {got} for {key}, expected {expected}",
            );
        }
    }
}
