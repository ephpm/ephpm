//! Integration tests for the `ClusteredStore` routing and hot key cache.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ephpm_cluster::ClusteredStore;
use ephpm_cluster::node::{ClusterHandle, NodeState, start_gossip};
use ephpm_config::{ClusterConfig, ClusterKvConfig};
use ephpm_kv::store::{Store, StoreConfig};

/// Allocate a random UDP port by binding to :0 and immediately closing.
async fn random_udp_port() -> u16 {
    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.expect("bind failed");
    sock.local_addr().expect("local_addr failed").port()
}

/// Start a gossip node.
async fn start_node(port: u16, seeds: Vec<String>, node_id: &str) -> ClusterHandle {
    let config = ClusterConfig {
        enabled: true,
        bind: format!("127.0.0.1:{port}"),
        join: seeds,
        secret: String::new(),
        node_id: node_id.to_string(),
        cluster_id: "test-cluster".to_string(),
        kv: ClusterKvConfig::default(),
    };
    start_gossip(&config).await.unwrap_or_else(|e| panic!("gossip start failed for {node_id}: {e}"))
}

/// Create a local KV store with defaults.
fn local_store() -> Arc<Store> {
    Store::new(StoreConfig::default())
}

/// Shutdown a handle wrapped in Arc (drops all other Arcs first).
async fn shutdown(handle: Arc<ClusterHandle>) {
    Arc::try_unwrap(handle)
        .expect("other Arc references must be dropped before shutdown")
        .shutdown()
        .await;
}

/// Wait for all handles to see `expected` alive nodes.
async fn wait_for_convergence(handles: &[&ClusterHandle], expected: usize, timeout: Duration) {
    let start = Instant::now();
    loop {
        let mut all_ok = true;
        for h in handles {
            let alive = h.nodes().await.iter().filter(|n| n.state == NodeState::Alive).count();
            if alive != expected {
                all_ok = false;
                break;
            }
        }
        if all_ok {
            return;
        }
        assert!(start.elapsed() <= timeout, "convergence timeout after {timeout:?}",);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// ---------------------------------------------------------------------------
// Routing tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn small_value_routes_via_gossip() {
    let port = random_udp_port().await;
    let handle = Arc::new(start_node(port, vec![], "route-small").await);
    let store = local_store();
    let config = ClusterKvConfig { small_key_threshold: 64, ..ClusterKvConfig::default() };

    let cs = ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), config);

    // A small value should go to gossip, not local store.
    cs.set("tiny".to_string(), b"abc".to_vec(), None).await;

    // Readable via clustered store.
    let val = cs.get("tiny").await;
    assert_eq!(val.as_deref(), Some(b"abc".as_slice()));

    // Should NOT be in the local store (it went to gossip).
    assert!(store.get("tiny").is_none());

    // Should be in gossip.
    let gossip_val = handle.gossip_get("tiny").await;
    assert_eq!(gossip_val.as_deref(), Some(b"abc".as_slice()));

    drop(cs);
    shutdown(handle).await;
}

#[tokio::test]
async fn large_value_routes_to_local_store() {
    let port = random_udp_port().await;
    let handle = Arc::new(start_node(port, vec![], "route-large").await);
    let store = local_store();
    let config = ClusterKvConfig { small_key_threshold: 8, ..ClusterKvConfig::default() };

    let cs = ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), config);

    // A large value should go to local store.
    let big = vec![0u8; 100];
    cs.set("big".to_string(), big.clone(), None).await;

    // Readable via clustered store.
    assert_eq!(cs.get("big").await.as_deref(), Some(big.as_slice()));

    // Should be in local store.
    assert!(store.get("big").is_some());

    // Should NOT be in gossip.
    assert!(handle.gossip_get("big").await.is_none());

    drop(cs);
    shutdown(handle).await;
}

#[tokio::test]
async fn small_value_replicates_via_clustered_store() {
    let port1 = random_udp_port().await;
    let port2 = random_udp_port().await;
    let seed = format!("127.0.0.1:{port1}");

    let h1 = Arc::new(start_node(port1, vec![], "cs-a").await);
    let h2 = Arc::new(start_node(port2, vec![seed], "cs-b").await);

    wait_for_convergence(&[&h1, &h2], 2, Duration::from_secs(10)).await;

    let cs1 = ClusteredStore::new(local_store(), Arc::clone(&h1), ClusterKvConfig::default());
    let cs2 = ClusteredStore::new(local_store(), Arc::clone(&h2), ClusterKvConfig::default());

    // Set on node1's clustered store.
    cs1.set("replicated".to_string(), b"data".to_vec(), None).await;

    // Wait for node2 to see it via gossip.
    let start = Instant::now();
    loop {
        if let Some(val) = cs2.get("replicated").await {
            assert_eq!(val, b"data");
            break;
        }
        assert!(start.elapsed() <= Duration::from_secs(10), "replication timeout",);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    drop(cs1);
    drop(cs2);
    shutdown(h1).await;
    shutdown(h2).await;
}

#[tokio::test]
async fn exists_checks_both_tiers() {
    let port = random_udp_port().await;
    let handle = Arc::new(start_node(port, vec![], "exists-test").await);
    let store = local_store();
    let config = ClusterKvConfig { small_key_threshold: 16, ..ClusterKvConfig::default() };

    let cs = ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), config);

    // Small key → gossip.
    cs.set("s".to_string(), b"x".to_vec(), None).await;
    // Large key → local store.
    cs.set("l".to_string(), vec![0u8; 32], None).await;

    assert!(cs.exists("s").await);
    assert!(cs.exists("l").await);
    assert!(!cs.exists("missing").await);

    drop(cs);
    shutdown(handle).await;
}

#[tokio::test]
async fn remove_from_both_tiers() {
    let port = random_udp_port().await;
    let handle = Arc::new(start_node(port, vec![], "rm-test").await);
    let store = local_store();
    let config = ClusterKvConfig { small_key_threshold: 16, ..ClusterKvConfig::default() };

    let cs = ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), config);

    cs.set("gs".to_string(), b"tiny".to_vec(), None).await;
    cs.set("ls".to_string(), vec![0u8; 32], None).await;

    assert!(cs.remove("gs").await);
    assert!(cs.remove("ls").await);
    assert!(!cs.exists("gs").await);
    assert!(!cs.exists("ls").await);

    drop(cs);
    shutdown(handle).await;
}

// ---------------------------------------------------------------------------
// Hot key cache tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hot_key_promotion_after_threshold() {
    let port = random_udp_port().await;
    let handle = Arc::new(start_node(port, vec![], "hot-test").await);
    let store = local_store();
    let config = ClusterKvConfig {
        small_key_threshold: 8,
        hot_key_cache: true,
        hot_key_threshold: 3,
        hot_key_window_secs: 10,
        hot_key_local_ttl_secs: 30,
        hot_key_max_memory: "1MB".to_string(),
        ..ClusterKvConfig::default()
    };

    let cs = ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), config);

    let value = b"large-value-data-here";

    // Simulate 3 remote fetches (threshold = 3).
    cs.track_remote_fetch("product:99", value);
    cs.track_remote_fetch("product:99", value);
    assert_eq!(cs.hot_cache_len(), 0, "should not promote yet");

    cs.track_remote_fetch("product:99", value);
    assert_eq!(cs.hot_cache_len(), 1, "should promote after 3rd fetch");
    assert!(cs.hot_cache_mem_used() > 0);

    // The hot cache should serve the value.
    let cached = cs.get("product:99").await;
    assert_eq!(cached.as_deref(), Some(value.as_slice()));

    drop(cs);
    shutdown(handle).await;
}

#[tokio::test]
async fn hot_key_cache_respects_memory_limit() {
    let port = random_udp_port().await;
    let handle = Arc::new(start_node(port, vec![], "hot-mem").await);
    let store = local_store();
    let config = ClusterKvConfig {
        small_key_threshold: 8,
        hot_key_cache: true,
        hot_key_threshold: 1, // promote immediately
        hot_key_window_secs: 10,
        hot_key_local_ttl_secs: 30,
        hot_key_max_memory: "256B".to_string(), // very small
        ..ClusterKvConfig::default()
    };

    let cs = ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), config);

    // First key: should fit.
    let small = vec![0u8; 50];
    cs.track_remote_fetch("key1", &small);
    assert_eq!(cs.hot_cache_len(), 1);

    // Second key: pushes past the 256B limit — should be rejected.
    let big = vec![0u8; 300];
    cs.track_remote_fetch("key2", &big);
    // key2 should NOT be in the cache (exceeds budget).
    assert_eq!(cs.hot_cache_len(), 1);

    drop(cs);
    shutdown(handle).await;
}

#[tokio::test]
async fn hot_key_cache_ttl_expiry() {
    let port = random_udp_port().await;
    let handle = Arc::new(start_node(port, vec![], "hot-ttl").await);
    let store = local_store();
    let config = ClusterKvConfig {
        small_key_threshold: 8,
        hot_key_cache: true,
        hot_key_threshold: 1,
        hot_key_window_secs: 10,
        hot_key_local_ttl_secs: 0, // expire immediately
        hot_key_max_memory: "1MB".to_string(),
        ..ClusterKvConfig::default()
    };

    let cs = ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), config);

    cs.track_remote_fetch("expires", b"data");
    assert_eq!(cs.hot_cache_len(), 1);

    // Reading should evict the expired entry (TTL = 0s).
    tokio::time::sleep(Duration::from_millis(10)).await;
    let val = cs.get("expires").await;
    assert!(val.is_none(), "expired hot key should not be served");
    assert_eq!(cs.hot_cache_len(), 0);

    drop(cs);
    shutdown(handle).await;
}

#[tokio::test]
async fn hot_cache_cleanup_evicts_stale() {
    let port = random_udp_port().await;
    let handle = Arc::new(start_node(port, vec![], "hot-cleanup").await);
    let store = local_store();
    let config = ClusterKvConfig {
        small_key_threshold: 8,
        hot_key_cache: true,
        hot_key_threshold: 1,
        hot_key_window_secs: 0, // window expires immediately
        hot_key_local_ttl_secs: 0,
        hot_key_max_memory: "1MB".to_string(),
        ..ClusterKvConfig::default()
    };

    let cs = ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), config);

    cs.track_remote_fetch("stale", b"data");
    assert_eq!(cs.hot_cache_len(), 1);

    tokio::time::sleep(Duration::from_millis(10)).await;
    cs.hot_cache_cleanup();

    assert_eq!(cs.hot_cache_len(), 0, "cleanup should evict expired entries");
    assert_eq!(cs.hot_cache_mem_used(), 0);

    drop(cs);
    shutdown(handle).await;
}

#[tokio::test]
async fn hot_key_disabled_skips_tracking() {
    let port = random_udp_port().await;
    let handle = Arc::new(start_node(port, vec![], "hot-off").await);
    let store = local_store();
    let config = ClusterKvConfig {
        hot_key_cache: false,
        hot_key_threshold: 1,
        ..ClusterKvConfig::default()
    };

    let cs = ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), config);

    cs.track_remote_fetch("ignored", b"data");
    assert_eq!(cs.hot_cache_len(), 0, "should not track when disabled");

    drop(cs);
    shutdown(handle).await;
}

// ---------------------------------------------------------------------------
// pttl tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pttl_gossip_tier_key() {
    let port = random_udp_port().await;
    let handle = Arc::new(start_node(port, vec![], "pttl-gossip").await);
    let store = local_store();
    let config = ClusterKvConfig { small_key_threshold: 64, ..ClusterKvConfig::default() };

    let cs = ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), config);

    // Small key with TTL → gossip tier
    cs.set("tiny-ttl".to_string(), b"val".to_vec(), Some(Duration::from_secs(60))).await;

    let pttl = cs.pttl("tiny-ttl").await;
    assert!(pttl.is_some(), "gossip key with TTL should have pttl");
    let ms = pttl.unwrap();
    assert!(ms > 58_000, "pttl should be ~60s, got {ms}");

    drop(cs);
    shutdown(handle).await;
}

#[tokio::test]
async fn pttl_local_store_key() {
    let port = random_udp_port().await;
    let handle = Arc::new(start_node(port, vec![], "pttl-local").await);
    let store = local_store();
    let config = ClusterKvConfig { small_key_threshold: 8, ..ClusterKvConfig::default() };

    let cs = ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), config);

    // Large key with TTL → local store
    let big = vec![0u8; 100];
    cs.set("big-ttl".to_string(), big, Some(Duration::from_secs(120))).await;

    let pttl = cs.pttl("big-ttl").await;
    assert!(pttl.is_some(), "local key with TTL should have pttl");
    let ms = pttl.unwrap();
    assert!(ms > 118_000, "pttl should be ~120s, got {ms}");

    drop(cs);
    shutdown(handle).await;
}

#[tokio::test]
async fn pttl_missing_key_returns_none() {
    let port = random_udp_port().await;
    let handle = Arc::new(start_node(port, vec![], "pttl-miss").await);
    let store = local_store();

    let cs =
        ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), ClusterKvConfig::default());

    // pttl for non-existent key should return None (or -2 via local store).
    // The ClusteredStore checks gossip first (returns None), then local store.
    let result = cs.pttl("no-such-key").await;
    // Local store returns Some(-2) for missing keys.
    assert!(
        result.is_none() || result == Some(-2),
        "missing key pttl should be None or -2, got {result:?}"
    );

    drop(cs);
    shutdown(handle).await;
}

// ---------------------------------------------------------------------------
// Accessor tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_store_accessor_returns_correct_store() {
    let port = random_udp_port().await;
    let handle = Arc::new(start_node(port, vec![], "accessor").await);
    let store = local_store();

    let cs =
        ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), ClusterKvConfig::default());

    // Write directly to local store, verify via accessor.
    store.set("direct".to_string(), b"value".to_vec(), None);
    assert!(cs.local_store().get("direct").is_some());

    drop(cs);
    shutdown(handle).await;
}

#[tokio::test]
async fn set_returns_true_on_success() {
    let port = random_udp_port().await;
    let handle = Arc::new(start_node(port, vec![], "set-ok").await);
    let store = local_store();

    let cs =
        ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), ClusterKvConfig::default());

    // Both small and large values should succeed.
    let small_ok = cs.set("s".to_string(), b"x".to_vec(), None).await;
    let large_ok = cs.set("l".to_string(), vec![0u8; 1024], None).await;
    assert!(small_ok, "small set should return true");
    assert!(large_ok, "large set should return true");

    drop(cs);
    shutdown(handle).await;
}
