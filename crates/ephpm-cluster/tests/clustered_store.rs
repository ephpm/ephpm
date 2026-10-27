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
        allow_insecure_no_auth: false,
        node_id: node_id.to_string(),
        cluster_id: "test-cluster".to_string(),
        kv: ClusterKvConfig::default(),
        ..ClusterConfig::default()
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
        assert!(start.elapsed() <= timeout, "convergence timeout after {timeout:?}");
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

    let cs = ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), config, None);

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

    let cs = ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), config, None);

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

    let cs1 = ClusteredStore::new(local_store(), Arc::clone(&h1), ClusterKvConfig::default(), None);
    let cs2 = ClusteredStore::new(local_store(), Arc::clone(&h2), ClusterKvConfig::default(), None);

    // Set on node1's clustered store.
    cs1.set("replicated".to_string(), b"data".to_vec(), None).await;

    // Wait for node2 to see it via gossip.
    let start = Instant::now();
    loop {
        if let Some(val) = cs2.get("replicated").await {
            assert_eq!(val, b"data");
            break;
        }
        assert!(start.elapsed() <= Duration::from_secs(10), "replication timeout");
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

    let cs = ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), config, None);

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

    let cs = ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), config, None);

    cs.set("gs".to_string(), b"tiny".to_vec(), None).await;
    cs.set("ls".to_string(), vec![0u8; 32], None).await;

    assert!(cs.remove("gs"));
    assert!(cs.remove("ls"));
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

    let cs = ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), config, None);

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

    let cs = ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), config, None);

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

    let cs = ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), config, None);

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

    let cs = ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), config, None);

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

    let cs = ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), config, None);

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

    let cs = ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), config, None);

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

    let cs = ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), config, None);

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

    let cs = ClusteredStore::new(
        Arc::clone(&store),
        Arc::clone(&handle),
        ClusterKvConfig::default(),
        None,
    );

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

    let cs = ClusteredStore::new(
        Arc::clone(&store),
        Arc::clone(&handle),
        ClusterKvConfig::default(),
        None,
    );

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

    let cs = ClusteredStore::new(
        Arc::clone(&store),
        Arc::clone(&handle),
        ClusterKvConfig::default(),
        None,
    );

    // Both small and large values should succeed.
    let small_ok = cs.set("s".to_string(), b"x".to_vec(), None).await;
    let large_ok = cs.set("l".to_string(), vec![0u8; 1024], None).await;
    assert!(small_ok, "small set should return true");
    assert!(large_ok, "large set should return true");

    drop(cs);
    shutdown(handle).await;
}

// ---------------------------------------------------------------------------
// Write stamps order after the version this node already applied
// ---------------------------------------------------------------------------

/// Every write a replicator originates is stamped above the key's applied
/// record, even when that record is a peer's stamp a few seconds ahead of this
/// node's clock (within the drift bound) — so the write applies, here and on
/// peers, instead of losing to the version it was computed from. Pins each
/// write site of both replicators: reverting one to a plain clock stamp makes
/// its write lose to the seed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_replicator_write_is_stamped_above_the_applied_record() {
    use ephpm_cluster::clustered_store::new_applied_write_map;
    use ephpm_cluster::gossip_kv::SEQ_BITS;
    use ephpm_cluster::{KvReplicator, SiteKvReplicator, Stamp, hash_field, site_namespace};
    use ephpm_kv::store::{HashFieldState, Replicator};

    let port = random_udp_port().await;
    let handle = Arc::new(start_node(port, vec![], "stamp-after").await);
    let store = local_store();
    let site_store = local_store();
    let config = ClusterKvConfig { small_key_threshold: 8, ..ClusterKvConfig::default() };
    let cs = ClusteredStore::new(Arc::clone(&store), Arc::clone(&handle), config, None);
    let applied = new_applied_write_map();
    let rt = tokio::runtime::Handle::current();
    let global = KvReplicator::new(Arc::clone(&cs), rt.clone(), Arc::clone(&applied));
    let site =
        SiteKvReplicator::new(Arc::clone(&cs), Arc::clone(&site_store), "s", rt, applied.clone());

    let now_ms = u64::try_from(
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis(),
    )
    .unwrap();
    // A peer's stamp 2 s ahead with the highest tag: a plain local stamp loses.
    // Each case gets a later one, ahead of where the previous case left this
    // node's clock, so every case tests its own write site.
    let seed_for =
        |i: u64| Stamp { write: (now_ms + 2_000 + 20 * i) << SEQ_BITS, node_tag: u64::MAX };
    let small = b"v".to_vec();
    let large = vec![b'L'; 64];

    // (key, its transport key, the write)
    type Case<'a> = (&'a str, Box<dyn Fn(&str) -> String>, Box<dyn Fn(&str) + 'a>);
    let cases: Vec<Case> = vec![
        (
            "g:set",
            Box::new(str::to_string),
            Box::new(|k| {
                global.replicate_set(k.into(), small.clone(), None);
            }),
        ),
        (
            "g:set-large",
            Box::new(str::to_string),
            Box::new(|k| {
                global.replicate_set(k.into(), large.clone(), None);
            }),
        ),
        (
            "g:broadcast",
            Box::new(str::to_string),
            Box::new(|k| {
                global.replicate_broadcast(k.into(), small.clone(), None);
            }),
        ),
        (
            "g:remove",
            Box::new(str::to_string),
            Box::new(|k| {
                global.replicate_remove(k);
            }),
        ),
        (
            "g:published",
            Box::new(str::to_string),
            Box::new(|k| {
                global.replicate_published(k.into(), small.clone(), None);
            }),
        ),
        (
            "g:published-large",
            Box::new(str::to_string),
            Box::new(|k| {
                global.replicate_published(k.into(), large.clone(), None);
            }),
        ),
        (
            "g:remove-published",
            Box::new(str::to_string),
            Box::new(|k| {
                global.replicate_remove_published(k.into());
            }),
        ),
        (
            "g:expire",
            Box::new(str::to_string),
            Box::new(|k| {
                store.set_local(k.into(), small.clone(), None);
                global.replicate_expire(k, Duration::from_secs(60));
            }),
        ),
        (
            "g:hash",
            Box::new(|k| hash_field::encode_key(k, "f")),
            Box::new(|k| {
                global.replicate_hash_field(k, "f", &mut || {
                    Some(HashFieldState {
                        value: Some(b"x".to_vec()),
                        field_ttl: None,
                        hash_ttl: None,
                    })
                });
            }),
        ),
        (
            "s:set",
            Box::new(|k| site_namespace::encode("s", k)),
            Box::new(|k| {
                site.replicate_set(k.into(), small.clone(), None);
            }),
        ),
        (
            "s:set-large",
            Box::new(|k| site_namespace::encode("s", k)),
            Box::new(|k| {
                site.replicate_set(k.into(), large.clone(), None);
            }),
        ),
        (
            "s:remove",
            Box::new(|k| site_namespace::encode("s", k)),
            Box::new(|k| {
                site.replicate_remove(k);
            }),
        ),
        (
            "s:published",
            Box::new(|k| site_namespace::encode("s", k)),
            Box::new(|k| {
                site.replicate_published(k.into(), small.clone(), None);
            }),
        ),
        (
            "s:published-large",
            Box::new(|k| site_namespace::encode("s", k)),
            Box::new(|k| {
                site.replicate_published(k.into(), large.clone(), None);
            }),
        ),
        (
            "s:remove-published",
            Box::new(|k| site_namespace::encode("s", k)),
            Box::new(|k| {
                site.replicate_remove_published(k.into());
            }),
        ),
        (
            "s:expire",
            Box::new(|k| site_namespace::encode("s", k)),
            Box::new(|k| {
                site_store.set_local(k.into(), small.clone(), None);
                site.replicate_expire(k, Duration::from_secs(60));
            }),
        ),
        (
            "s:hash",
            Box::new(|k| site_namespace::encode("s", &hash_field::encode_key(k, "f"))),
            Box::new(|k| {
                site.replicate_hash_field(k, "f", &mut || {
                    Some(HashFieldState {
                        value: Some(b"x".to_vec()),
                        field_ttl: None,
                        hash_ttl: None,
                    })
                });
            }),
        ),
    ];
    for (i, (key, transport, write)) in (0u64..).zip(&cases) {
        let seed = seed_for(i);
        let wire = transport(key);
        applied.insert(wire.clone(), seed);
        write(key);
        let recorded = applied.get(&wire).map(|s| *s);
        assert!(
            recorded.is_some_and(|s| s > seed),
            "{key}: the write must be stamped above the applied {seed:?}, recorded {recorded:?}"
        );
    }
    // The local effects landed (not skipped as stale).
    assert_eq!(store.get("g:set").as_deref(), Some(small.as_slice()));
    assert_eq!(store.get("g:set-large").as_deref(), Some(large.as_slice()));
    assert!(store.pttl("g:expire").is_some_and(|ms| ms > 0));
    assert_eq!(site_store.get("s:set").as_deref(), Some(small.as_slice()));
    assert!(site_store.pttl("s:expire").is_some_and(|ms| ms > 0));

    drop(cases);
    drop((global, site, cs));
    // The queued large writes' workers hold the store while they run.
    tokio::time::sleep(Duration::from_millis(500)).await;
    shutdown(handle).await;
}
