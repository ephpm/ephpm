//! Integration tests for the sync `KvReplicator` -> `ClusteredStore` seam.
//!
//! `Store::set` / `remove` / `expire` are called from **sync** contexts
//! (RESP command dispatcher, PHP FFI callbacks). `KvReplicator` bridges
//! those sync callers to the async `ClusteredStore` by pinning a tokio
//! [`Handle`] and spawning the routed write. These tests verify:
//!
//! 1. A sync `Store::set` on a hooked local store causes the cluster
//!    tier to observe the write (issue #143 root cause: without a
//!    replicator installed, SETs never left the local map).
//! 2. Small values (≤ `small_key_threshold`) land in the gossip tier.
//! 3. Large values land in the local store via the `ClusteredStore`
//!    routing (proving `set_local` is what runs, not a re-entrant
//!    `set`).
//! 4. Two-node smoke: a hooked SET on node A becomes visible via
//!    `ClusteredStore::get` on node B for a small (gossip-replicated)
//!    value.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ephpm_cluster::node::{ClusterHandle, NodeState, start_gossip};
use ephpm_cluster::{ClusteredStore, KvReplicator};
use ephpm_config::{ClusterConfig, ClusterKvConfig};
use ephpm_kv::store::{Replicator, Store, StoreConfig};

async fn random_udp_port() -> u16 {
    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.expect("bind failed");
    sock.local_addr().expect("local_addr failed").port()
}

async fn start_node(port: u16, seeds: Vec<String>, node_id: &str) -> ClusterHandle {
    let config = ClusterConfig {
        enabled: true,
        bind: format!("127.0.0.1:{port}"),
        join: seeds,
        secret: String::new(),
        node_id: node_id.to_string(),
        cluster_id: "seam-test".to_string(),
        kv: ClusterKvConfig::default(),
    };
    start_gossip(&config).await.unwrap_or_else(|e| panic!("gossip start failed for {node_id}: {e}"))
}

async fn shutdown(handle: Arc<ClusterHandle>) {
    Arc::try_unwrap(handle)
        .expect("other Arc references must be dropped before shutdown")
        .shutdown()
        .await;
}

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

/// The core seam guarantee for issue #143: `Store::set` from a **sync
/// context** on a hooked store DOES cause the clustered write to happen —
/// specifically, the value ends up routed through `ClusteredStore` and
/// visible on the gossip tier (for a small value).
#[tokio::test]
async fn sync_store_set_routes_through_clustered_store_gossip_tier() {
    let port = random_udp_port().await;
    let handle = Arc::new(start_node(port, vec![], "seam-a").await);

    let local = Store::new(StoreConfig::default());
    let clustered = ClusteredStore::new(
        Arc::clone(&local),
        Arc::clone(&handle),
        // Force everything into the gossip tier so we can inspect it
        // deterministically.
        ClusterKvConfig { small_key_threshold: 4096, ..ClusterKvConfig::default() },
        None,
    );

    // Install the sync bridge on the local store — this is what
    // `serve()` does at startup.
    let replicator = KvReplicator::new(
        Arc::clone(&clustered),
        tokio::runtime::Handle::current(),
        ephpm_cluster::clustered_store::new_applied_write_map(),
    );
    local.set_replicator(Some(replicator as Arc<dyn Replicator>));

    // Sync API — mirrors what the RESP dispatcher and PHP kv_bridge do.
    // Without the fix these would only touch `local` and gossip would
    // never see the write.
    assert!(local.set("issue-143".into(), b"v".to_vec(), None));

    // The bridge spawns the routed write on the runtime; wait briefly
    // for the async gossip-set to land.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(v) = handle.gossip_get("issue-143").await {
            assert_eq!(v, b"v".to_vec(), "gossip must hold the value written via sync Store::set");
            break;
        }
        assert!(Instant::now() < deadline, "sync SET never reached gossip within 5s");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The ORIGIN node must hold a materialized local copy: raw-store
    // readers (RESP GET, PHP native functions, the OPcache watcher) never
    // consult the gossip tier, so without local materialization the
    // origin could not read back its own write (found live on the
    // two-node kind demo). Gossip is the transport; each node's Store is
    // the materialized view.
    assert_eq!(
        local.get("issue-143").as_deref(),
        Some(b"v".as_slice()),
        "origin node must materialize its own small-value write locally"
    );

    // Clear the replicator so its Arc<ClusteredStore> is released; then
    // drop the clustered handle and the local store — leaving `handle`
    // as the sole owner for shutdown.
    local.set_replicator(None);
    drop(clustered);
    drop(local);
    shutdown(handle).await;
}

/// Large values (above `small_key_threshold`) route to the local store
/// tier via `ClusteredStore` — that path uses `set_local` internally so
/// this must NOT recurse into the replicator.
#[tokio::test]
async fn sync_store_set_routes_large_value_to_local_via_set_local() {
    let port = random_udp_port().await;
    let handle = Arc::new(start_node(port, vec![], "seam-large").await);

    let local = Store::new(StoreConfig::default());
    let clustered = ClusteredStore::new(
        Arc::clone(&local),
        Arc::clone(&handle),
        ClusterKvConfig { small_key_threshold: 8, ..ClusterKvConfig::default() },
        None,
    );
    let replicator = KvReplicator::new(
        Arc::clone(&clustered),
        tokio::runtime::Handle::current(),
        ephpm_cluster::clustered_store::new_applied_write_map(),
    );
    local.set_replicator(Some(replicator as Arc<dyn Replicator>));

    // Large value — goes to the local store tier via ClusteredStore.
    assert!(local.set("large".into(), vec![0xAB; 64], None));

    // Wait for the spawned routed write to land locally.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if local.get("large").is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "large-value routed write never landed locally");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(local.get("large").as_deref(), Some(vec![0xAB; 64].as_slice()));
    // Gossip must not hold the large value.
    assert!(handle.gossip_get("large").await.is_none());

    // Clear the replicator so its Arc<ClusteredStore> is released; then
    // drop the clustered handle and the local store — leaving `handle`
    // as the sole owner for shutdown.
    local.set_replicator(None);
    drop(clustered);
    drop(local);
    shutdown(handle).await;
}

/// Two-node smoke: a hooked sync SET on node A becomes visible via the
/// ClusteredStore reader on node B (gossip-replicated tier). This is the
/// end-to-end shape of what the OPcache invalidation feature needs from
/// the KV layer.
#[tokio::test]
async fn hooked_sync_set_replicates_to_peer_via_gossip() {
    let port1 = random_udp_port().await;
    let port2 = random_udp_port().await;
    let seed = format!("127.0.0.1:{port1}");

    let h1 = Arc::new(start_node(port1, vec![], "peer-a").await);
    let h2 = Arc::new(start_node(port2, vec![seed], "peer-b").await);

    wait_for_convergence(&[&h1, &h2], 2, Duration::from_secs(10)).await;

    let local1 = Store::new(StoreConfig::default());
    let cs1 = ClusteredStore::new(
        Arc::clone(&local1),
        Arc::clone(&h1),
        // Force gossip tier so the write is small enough to fan out via
        // chitchat.
        ClusterKvConfig { small_key_threshold: 4096, ..ClusterKvConfig::default() },
        None,
    );
    let rep1 = KvReplicator::new(
        Arc::clone(&cs1),
        tokio::runtime::Handle::current(),
        ephpm_cluster::clustered_store::new_applied_write_map(),
    );
    local1.set_replicator(Some(rep1 as Arc<dyn Replicator>));

    let local2 = Store::new(StoreConfig::default());
    let cs2 = ClusteredStore::new(
        Arc::clone(&local2),
        Arc::clone(&h2),
        ClusterKvConfig { small_key_threshold: 4096, ..ClusterKvConfig::default() },
        None,
    );

    // The bug reproduction: sync SET on node A. Before the fix this
    // never left A. After the fix, gossip propagates it and B's
    // ClusteredStore::get returns the value.
    assert!(local1.set("opcache:version:demo".into(), b"1".to_vec(), None));

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(v) = cs2.get("opcache:version:demo").await {
            assert_eq!(v, b"1", "peer B must see the value written via sync SET on peer A");
            break;
        }
        assert!(Instant::now() < deadline, "cross-node gossip propagation timeout");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    local1.set_replicator(None);
    drop(cs1);
    drop(cs2);
    drop(local1);
    drop(local2);
    shutdown(h1).await;
    shutdown(h2).await;
}
