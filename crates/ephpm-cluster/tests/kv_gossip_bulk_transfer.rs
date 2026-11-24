//! Bulk transfer of the gossip (small) tier on (re)join (#631).
//!
//! A node that rejoins with a fresh chitchat generation otherwise fills its
//! small tier by streaming each peer's gossip state per peer, in version
//! order, one MTU per exchange — seconds to minutes, and under continuous
//! writes a tombstone-grace reset storm can keep it from ever finishing. The
//! gossip bulk transfer ([`OP_SHARE_GOSSIP`]) pulls the whole small tier from
//! every alive peer over the data plane and lands each entry through the
//! stamped-write path, so the local store is complete in seconds and never
//! holds a stale copy.
//!
//! The wire-level tests use only the data plane (two [`Store`]s + listeners),
//! so they are deterministic and run on every platform. The convergence test
//! brings up real gossip membership for peer discovery; it is still data-plane
//! only (no 64 KB gossip backlog), so it also runs everywhere.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ephpm_cluster::data_plane::{DataPlaneClient, KvRouter, ShareEntry};
use ephpm_cluster::node::{ClusterHandle, NodeState, start_gossip};
use ephpm_cluster::{ClusterCipher, GossipBulkTransfer, Stamp};
use ephpm_config::{ClusterConfig, ClusterKvConfig};
use ephpm_kv::store::{Store, StoreConfig};

/// Values at or below this ride the gossip tier and are bulk-transferred;
/// larger ones are the replica-set data plane's job and must not be shared.
const THRESHOLD: usize = 512;

fn free_tcp_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").expect("bind").local_addr().expect("addr").port()
}

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").expect("bind").local_addr().expect("addr").port()
}

/// A store with a data-plane listener that answers gossip bulk pulls, plus the
/// router (its applied map) used to seed stamped writes into it.
struct Sender {
    store: Arc<Store>,
    router: KvRouter,
    _listener: tokio::task::JoinHandle<()>,
    addr: SocketAddr,
}

async fn sender(name: &str, cipher: Option<Arc<ClusterCipher>>) -> Sender {
    let store = Store::new(StoreConfig::default());
    let applied = ephpm_cluster::clustered_store::new_applied_write_map();
    let router = KvRouter::global(Arc::clone(&store))
        .with_applied(applied)
        .with_gossip_share(THRESHOLD, name);
    let port = free_tcp_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().expect("addr");
    let serve_router = router.clone();
    let serve_cipher = cipher.clone();
    let listener = tokio::spawn(async move {
        if let Err(e) =
            ephpm_cluster::data_plane::serve_router(serve_router, addr, serve_cipher).await
        {
            eprintln!("sender data plane failed: {e}");
        }
    });
    // Let the listener bind before any client dials.
    tokio::time::sleep(Duration::from_millis(50)).await;
    Sender { store, router, _listener: listener, addr }
}

/// Seed a stamped string into a sender's store and applied map, so the bulk
/// share carries it with exactly `stamp`.
fn seed(s: &Sender, key: &str, value: &[u8], ttl_ms: u64, stamp: Stamp) {
    assert_eq!(s.router.apply_stamped(key, value.to_vec(), ttl_ms, stamp), Some(true));
    assert_eq!(s.store.get(key).as_deref(), Some(value));
}

/// Pull `sender`'s whole small tier into `into` (its applied map), returning
/// the entry/byte counts. Bulk pulls use their own connection per the wire.
async fn pull_into(client: &DataPlaneClient, sender: &Sender, into: &KvRouter) -> (u64, u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    client
        .share_gossip_from(sender.addr, deadline, |entry: ShareEntry| {
            let _ =
                into.apply_stamped(&entry.transport_key, entry.value, entry.ttl_ms, entry.stamp);
        })
        .await
        .expect("gossip bulk pull")
}

fn receiver() -> (Arc<Store>, KvRouter) {
    let store = Store::new(StoreConfig::default());
    let applied = ephpm_cluster::clustered_store::new_applied_write_map();
    let router = KvRouter::global(Arc::clone(&store)).with_applied(applied);
    (store, router)
}

/// The whole small tier is pulled; large-tier values are excluded; TTLs
/// survive. Plaintext and encrypted frames behave identically.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fills_small_tier_and_excludes_large() {
    for cipher in [None, Some(Arc::new(ClusterCipher::for_kv_data_plane("s3cr3t")))] {
        let a = sender("A", cipher.clone()).await;
        seed(&a, "small:1", b"one", 0, Stamp::new(10, "A"));
        seed(&a, "small:2", b"two", 0, Stamp::new(20, "A"));
        // A value with a TTL — the remaining lifetime must come across.
        seed(&a, "ttld", b"temp", 60_000, Stamp::new(30, "A"));
        // Above the gossip threshold: the replica-set data plane's job, never
        // shared here (over-replication). 600 B > 512 B.
        seed(&a, "big", &vec![b'x'; 600], 0, Stamp::new(40, "A"));

        let (r_store, r_router) = receiver();
        let client = DataPlaneClient::new(cipher.clone());
        let (keys, _bytes) = pull_into(&client, &a, &r_router).await;

        assert_eq!(keys, 3, "three small entries, not the large one");
        assert_eq!(r_store.get("small:1").as_deref(), Some(b"one".as_slice()));
        assert_eq!(r_store.get("small:2").as_deref(), Some(b"two".as_slice()));
        assert_eq!(r_store.get("ttld").as_deref(), Some(b"temp".as_slice()));
        let (_v, ttl) = r_store.get_with_ttl("ttld").expect("ttld present");
        let ttl = ttl.expect("ttl carried across");
        assert!(
            ttl > Duration::from_secs(30) && ttl <= Duration::from_secs(60),
            "ttl ~60s: {ttl:?}"
        );
        assert_eq!(r_store.get("big"), None, "large-tier value must not be bulk-shared");
    }
}

/// A newer write already on the receiver is never regressed by an older bulk
/// copy: the pull only fills slots the receiver has no newer record for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn never_regresses_a_newer_local_write() {
    let a = sender("A", None).await;
    seed(&a, "k", b"from-peer-old", 0, Stamp::new(100, "A"));

    let (r_store, r_router) = receiver();
    // The receiver already holds a strictly newer write for the key.
    assert_eq!(
        r_router.apply_stamped("k", b"local-new".to_vec(), 0, Stamp::new(200, "B")),
        Some(true)
    );

    let client = DataPlaneClient::new(None);
    pull_into(&client, &a, &r_router).await;

    assert_eq!(
        r_store.get("k").as_deref(),
        Some(b"local-new".as_slice()),
        "the older bulk copy must not overwrite the newer local write"
    );
}

/// Pulling the same key from two peers that disagree keeps the newest stamp,
/// whichever peer is pulled first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keeps_newest_across_peers() {
    let old = sender("OLD", None).await;
    let new = sender("NEW", None).await;
    seed(&old, "k", b"older", 0, Stamp::new(100, "OLD"));
    seed(&new, "k", b"newer", 0, Stamp::new(300, "NEW"));

    // Pull the OLDER peer first, then the newer: newest-stamp-wins corrects it.
    let (r_store, r_router) = receiver();
    let client = DataPlaneClient::new(None);
    pull_into(&client, &old, &r_router).await;
    assert_eq!(r_store.get("k").as_deref(), Some(b"older".as_slice()));
    pull_into(&client, &new, &r_router).await;
    assert_eq!(r_store.get("k").as_deref(), Some(b"newer".as_slice()));

    // And the other order: newer first, then older must not regress it.
    let (r_store2, r_router2) = receiver();
    pull_into(&client, &new, &r_router2).await;
    pull_into(&client, &old, &r_router2).await;
    assert_eq!(
        r_store2.get("k").as_deref(),
        Some(b"newer".as_slice()),
        "an older peer's copy pulled later must not regress the newer one"
    );
}

async fn wait_alive(handles: &[&Arc<ClusterHandle>], expected: usize, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let mut all = true;
        for h in handles {
            let alive = h.nodes().await.iter().filter(|m| m.state == NodeState::Alive).count();
            if alive != expected {
                all = false;
            }
        }
        if all {
            return;
        }
        assert!(Instant::now() < deadline, "membership did not converge to {expected}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// End-to-end: a node that joins a peer holding a large small-tier state fills
/// its whole local store from that peer **over the data plane in seconds** via
/// [`GossipBulkTransfer`], without waiting on the per-MTU gossip catch-up (this
/// node runs no gossip applier, so the bulk pull is the only source). This is
/// the convergence #631 asks for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gossip_bulk_transfer_fills_a_large_tier_fast() {
    const KEYS: usize = 4_000;

    let ga = free_udp_port();
    let da = free_tcp_port();
    let kv = ClusterKvConfig {
        small_key_threshold: THRESHOLD,
        hot_key_cache: false,
        data_port: da,
        ..ClusterKvConfig::default()
    };
    let config_a = ClusterConfig {
        enabled: true,
        bind: format!("127.0.0.1:{ga}"),
        join: vec![],
        secret: String::new(),
        node_id: "bulk-a".to_string(),
        cluster_id: "gossip-bulk-test".to_string(),
        kv: kv.clone(),
        ..ClusterConfig::default()
    };
    let handle_a = Arc::new(start_gossip(&config_a).await.expect("gossip a"));
    let seed_addr = handle_a.self_node().gossip_addr.clone();

    // A's store + a listener that answers gossip bulk pulls.
    let store_a = Store::new(StoreConfig::default());
    let applied_a = ephpm_cluster::clustered_store::new_applied_write_map();
    let router_a = KvRouter::global(Arc::clone(&store_a))
        .with_applied(applied_a)
        .with_gossip_share(THRESHOLD, "bulk-a");
    let data_addr_a: SocketAddr = format!("127.0.0.1:{da}").parse().expect("addr");
    {
        let router_a = router_a.clone();
        tokio::spawn(async move {
            let _ = ephpm_cluster::data_plane::serve_router(router_a, data_addr_a, None).await;
        });
    }
    for i in 0..KEYS {
        let _ = router_a.apply_stamped(
            &format!("k{i}"),
            format!("v{i}").into_bytes(),
            0,
            Stamp::new(i as u64 + 1, "bulk-a"),
        );
    }

    // R joins A. It runs NO gossip applier, so its only path to A's tier is the
    // bulk pull — any key it ends up holding came over the data plane.
    let gr = free_udp_port();
    let dr = free_tcp_port();
    let config_r = ClusterConfig {
        enabled: true,
        bind: format!("127.0.0.1:{gr}"),
        join: vec![seed_addr],
        secret: String::new(),
        node_id: "bulk-r".to_string(),
        cluster_id: "gossip-bulk-test".to_string(),
        kv: ClusterKvConfig { data_port: dr, ..kv.clone() },
        ..ClusterConfig::default()
    };
    let handle_r = Arc::new(start_gossip(&config_r).await.expect("gossip r"));
    let store_r = Store::new(StoreConfig::default());
    let applied_r = ephpm_cluster::clustered_store::new_applied_write_map();
    let router_r = KvRouter::global(Arc::clone(&store_r)).with_applied(applied_r);

    wait_alive(&[&handle_a, &handle_r], 2, Duration::from_secs(20)).await;

    let pull = GossipBulkTransfer::new(Arc::clone(&handle_r), None, router_r.clone(), &kv);
    let started = Instant::now();
    let report = pull.pull(tokio::time::Instant::now() + Duration::from_secs(20), true).await;
    let elapsed = started.elapsed();
    eprintln!(
        "bulk pull: keys={} bytes={} peers={} in {:?}",
        report.keys, report.bytes, report.peers, elapsed
    );

    assert_eq!(report.peers, 1, "A must be discovered");
    assert_eq!(report.failed_peers, 0, "the pull must succeed");
    assert_eq!(report.keys, KEYS as u64, "every small-tier key is pulled");
    for i in [0usize, 1, KEYS / 2, KEYS - 1] {
        assert_eq!(
            store_r.get(&format!("k{i}")).as_deref(),
            Some(format!("v{i}").as_bytes()),
            "k{i} must be present after the bulk pull"
        );
    }
    // The whole tier arrived well inside the bound — seconds, not the minutes a
    // per-MTU gossip walk of this many entries would take.
    assert!(elapsed < Duration::from_secs(15), "the bulk fill must be fast: {elapsed:?}");

    // `pull` holds a clone of handle_r; drop it before reclaiming sole ownership.
    drop(pull);
    Arc::try_unwrap(handle_r).expect("sole owner").shutdown().await;
    Arc::try_unwrap(handle_a).expect("sole owner").shutdown().await;
}
