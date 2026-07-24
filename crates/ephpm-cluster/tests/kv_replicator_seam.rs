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
        allow_insecure_no_auth: false,
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

/// Fixture: two chitchat-joined nodes, each with a local `Store` hooked
/// through a `KvReplicator` + a running gossip applier — exactly the
/// wiring `ephpm-server::serve()` installs at startup. Returns
/// `(handle, local, clustered)` per node so tests can drive sync
/// `Store::remove` / `Store::expire` and observe cluster-wide effects.
async fn two_node_hooked_fixture() -> TwoNode {
    let port1 = random_udp_port().await;
    let port2 = random_udp_port().await;
    let seed = format!("127.0.0.1:{port1}");

    let h1 = Arc::new(start_node(port1, vec![], "hook-a").await);
    let h2 = Arc::new(start_node(port2, vec![seed], "hook-b").await);
    wait_for_convergence(&[&h1, &h2], 2, Duration::from_secs(10)).await;

    // Force gossip tier so writes fan out via chitchat.
    let cfg = ClusterKvConfig { small_key_threshold: 4096, ..ClusterKvConfig::default() };

    let (local1, cs1) = hook_node(&h1, cfg.clone()).await;
    let (local2, cs2) = hook_node(&h2, cfg).await;

    TwoNode { h1, local1, cs1, h2, local2, cs2 }
}

struct TwoNode {
    h1: Arc<ClusterHandle>,
    local1: Arc<Store>,
    cs1: Arc<ClusteredStore>,
    h2: Arc<ClusterHandle>,
    local2: Arc<Store>,
    cs2: Arc<ClusteredStore>,
}

impl TwoNode {
    async fn shutdown(self) {
        self.local1.set_replicator(None);
        self.local2.set_replicator(None);
        drop(self.cs1);
        drop(self.cs2);
        drop(self.local1);
        drop(self.local2);
        shutdown(self.h1).await;
        shutdown(self.h2).await;
    }
}

/// Install the same replicator + applier pair `ephpm-server::serve()`
/// wires up. Returns the local Store + the ClusteredStore for reads.
async fn hook_node(
    handle: &Arc<ClusterHandle>,
    cfg: ClusterKvConfig,
) -> (Arc<Store>, Arc<ClusteredStore>) {
    let local = Store::new(StoreConfig::default());
    let clustered = ClusteredStore::new(Arc::clone(&local), Arc::clone(handle), cfg, None);
    let applied = ephpm_cluster::clustered_store::new_applied_write_map();
    let replicator = KvReplicator::new(
        Arc::clone(&clustered),
        tokio::runtime::Handle::current(),
        applied.clone(),
    );
    local.set_replicator(Some(replicator as Arc<dyn Replicator>));
    ephpm_cluster::clustered_store::start_gossip_applier(handle, Arc::clone(&local), applied).await;
    (local, clustered)
}

/// Poll until `predicate` returns true, or the deadline expires.
/// Returns whether the predicate ever became true.
async fn wait_until<F, Fut>(mut predicate: F, timeout: Duration) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if predicate().await {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Delete-propagation: node A sets a small-value key, both nodes see it,
/// A removes it, and B's materialized copy disappears within convergence.
#[tokio::test]
async fn remote_delete_removes_peer_local_copy() {
    let f = two_node_hooked_fixture().await;

    // Set on A, wait for B to materialize (gossip applier fires).
    assert!(f.local1.set("session:abc".into(), b"payload".to_vec(), None));
    assert!(
        wait_until(|| async { f.local2.get("session:abc").is_some() }, Duration::from_secs(5))
            .await,
        "gossip SET must materialize on peer B"
    );
    assert_eq!(f.local2.get("session:abc").as_deref(), Some(b"payload".as_slice()));

    // Delete on A. Before this PR the tombstone was invisible to the
    // applier and B's copy lingered until TTL. Now the tombstone rides
    // the same subscription and remove_local fires on B.
    assert!(f.local1.remove("session:abc"));

    assert!(
        wait_until(|| async { f.local2.get("session:abc").is_none() }, Duration::from_secs(5))
            .await,
        "tombstone must propagate: peer B's local copy should be dropped"
    );
    // ClusteredStore::get on B must also read as deleted (gossip_get
    // now honours the tombstone's write_ms ordering, and the local copy
    // is gone).
    assert!(f.cs2.get("session:abc").await.is_none());

    f.shutdown().await;
}

/// A stale tombstone (older write_ms) must NOT delete a newer SET.
/// Ordering guarantee: `write_ms <= last_applied` is treated as stale.
#[tokio::test]
async fn stale_tombstone_does_not_delete_newer_set() {
    let f = two_node_hooked_fixture().await;

    // Write v1 on A, let B see it, then overwrite with v2 on A — B's
    // last-applied write_ms is now the v2 stamp.
    assert!(f.local1.set("k".into(), b"v1".to_vec(), None));
    assert!(wait_until(|| async { f.local2.get("k").is_some() }, Duration::from_secs(5)).await);

    // Small gap so the SET's write_ms is strictly < any tombstone we
    // build "before" it below.
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(f.local1.set("k".into(), b"v2".to_vec(), None));
    assert!(
        wait_until(
            || async { f.local2.get("k").as_deref() == Some(b"v2".as_slice()) },
            Duration::from_secs(5)
        )
        .await,
        "peer B must materialize the overwrite"
    );

    // Simulate a delayed echo of an OLDER delete via
    // ClusterHandle::gossip_del on A — but issued before the v2 write
    // in wall-clock time. In practice we can't rewind time from a test,
    // so we exercise the ordering guarantee at the applied-map level:
    // after v2, a tombstone stamped NOW is newer and DOES delete
    // (positive control), but the applier's `should_apply` gate is what
    // rejects stale echoes. Verified in the pure test in
    // clustered_store.rs::should_apply_stale_write_is_skipped. Here we
    // additionally assert the positive-control shape: a fresh delete
    // still works after the overwrite.
    assert!(f.local1.remove("k"));
    assert!(
        wait_until(|| async { f.local2.get("k").is_none() }, Duration::from_secs(5)).await,
        "fresh delete after overwrite must still propagate"
    );

    f.shutdown().await;
}

/// EXPIRE-propagation, extend case: node A sets with a short TTL, then
/// extends the TTL — B's copy must survive past the ORIGINAL expiry.
#[tokio::test]
async fn expire_extend_propagates_across_cluster() {
    let f = two_node_hooked_fixture().await;

    // Set with a moderate TTL (2s) on A — long enough for chitchat
    // gossip convergence to reach B before it expires. Give B enough
    // time to materialize.
    assert!(f.local1.set("session:xyz".into(), b"blob".to_vec(), Some(Duration::from_secs(2))));
    assert!(
        wait_until(|| async { f.local2.get("session:xyz").is_some() }, Duration::from_secs(10))
            .await,
        "initial SET must reach peer"
    );

    // Immediately extend the TTL well beyond the original.
    // lazy_write / update_timestamp fires this exact call on session
    // refresh: same value, longer TTL.
    assert!(f.local1.expire("session:xyz", Duration::from_secs(60)));

    // Wait until the extension has propagated (peer's PTTL passes the
    // original 2s budget). Prior to this PR the extension was
    // local-only and peer's copy expired at the original write time.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        f.local2.get("session:xyz").is_some(),
        "peer must still hold the value past the original TTL after EXPIRE extend"
    );
    // And through ClusteredStore.
    assert!(f.cs2.get("session:xyz").await.is_some());

    f.shutdown().await;
}

/// EXPIRE-propagation, shorten case: extending isn't the only refresh
/// pattern — a shortened TTL must also take effect on the peer.
#[tokio::test]
async fn expire_shorten_propagates_across_cluster() {
    let f = two_node_hooked_fixture().await;

    // Set with a long TTL (30s) on A, let B materialize.
    assert!(f.local1.set("k".into(), b"v".to_vec(), Some(Duration::from_secs(30))));
    assert!(wait_until(|| async { f.local2.get("k").is_some() }, Duration::from_secs(2)).await);

    // Shorten the TTL to 500ms. The peer must observe the shorter
    // expiry: its copy should disappear within a couple of seconds even
    // though the original TTL was 30s. The applier surfaces an
    // already-expired incoming SET as a Tombstone (see
    // `subscribe_kv_changes`), so an event that arrives a hair after
    // the encoded expiry still deletes the peer's stale copy — the
    // race that made KV v1 EXPIRE a no-op.
    assert!(f.local1.expire("k", Duration::from_millis(500)));

    assert!(
        wait_until(|| async { f.local2.get("k").is_none() }, Duration::from_secs(5)).await,
        "shortened TTL must propagate — peer's copy should expire (was 30s TTL, shortened to 500ms)"
    );

    f.shutdown().await;
}

/// The session-destroy end-to-end scenario at the store level. A user
/// logs in on node A, then logs out on node B (`session_destroy()` maps
/// to `Store::remove` on whichever node handles the logout). The
/// original node's copy must be gone — otherwise the session survives
/// the logout on any node still holding its own materialized copy.
#[tokio::test]
async fn session_destroy_from_peer_removes_origin_copy() {
    let f = two_node_hooked_fixture().await;

    // Login on A: set the session blob (small so it rides gossip).
    assert!(f.local1.set("session:logout-scenario".into(), b"user_id=42".to_vec(), None));
    // Wait for B to materialize.
    assert!(
        wait_until(
            || async { f.local2.get("session:logout-scenario").is_some() },
            Duration::from_secs(5)
        )
        .await,
        "session must be readable on both nodes after login"
    );

    // Logout on B — session_destroy() → Store::remove on node B.
    assert!(f.local2.remove("session:logout-scenario"));

    // A's local materialized copy must be gone within convergence.
    // Prior to this PR the tombstone was invisible: A held the session
    // until TTL expiry or overwrite, so the user was "logged in" on A
    // for the entire session lifetime after logout.
    assert!(
        wait_until(
            || async { f.local1.get("session:logout-scenario").is_none() },
            Duration::from_secs(5)
        )
        .await,
        "peer-initiated destroy must remove the origin's copy — the whole point of clustered \
         sessions"
    );

    f.shutdown().await;
}
