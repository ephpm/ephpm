//! Garbage collection of the gossip KV tier (stress finding F-6).
//!
//! Before this, nothing was ever removed from a node's chitchat state:
//! ~4M expired short-TTL keys pinned a node at 5.8 GB and 100% of a core
//! with no load. These tests pin the two mechanisms that bound the state:
//! the sweep (expired values leave the publishing node's state) and
//! retirement (a copy superseded by a peer's newer write leaves its
//! publisher's state, so exactly one node publishes each key).
//!
//! Gossip-only, so the nodes share `127.0.0.1` on distinct UDP ports.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ephpm_cluster::gossip_writer::SWEEP_INTERVAL;
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
        cluster_id: "gc-test".to_string(),
        kv: ClusterKvConfig::default(),
        ..ClusterConfig::default()
    };
    start_gossip(&config).await.unwrap_or_else(|e| panic!("gossip start failed for {node_id}: {e}"))
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
        assert!(start.elapsed() <= timeout, "membership convergence timeout after {timeout:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

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
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

struct Node {
    handle: Arc<ClusterHandle>,
    local: Arc<Store>,
    clustered: Arc<ClusteredStore>,
}

async fn hook_node(handle: Arc<ClusterHandle>) -> Node {
    let local = Store::new(StoreConfig::default());
    let clustered = ClusteredStore::new(
        Arc::clone(&local),
        Arc::clone(&handle),
        ClusterKvConfig { small_key_threshold: 4096, ..ClusterKvConfig::default() },
        None,
    );
    let applied = ephpm_cluster::clustered_store::new_applied_write_map();
    let replicator = KvReplicator::new(
        Arc::clone(&clustered),
        tokio::runtime::Handle::current(),
        applied.clone(),
    );
    local.set_replicator(Some(replicator as Arc<dyn Replicator>));
    ephpm_cluster::clustered_store::start_gossip_applier(&handle, Arc::clone(&local), applied)
        .await;
    Node { handle, local, clustered }
}

impl Node {
    async fn shutdown(self) {
        self.local.set_replicator(None);
        drop(self.clustered);
        drop(self.local);
        Arc::try_unwrap(self.handle).expect("sole owner").shutdown().await;
    }
}

/// Short-TTL keys leave the publishing node's gossip state once they
/// expire, instead of accumulating forever.
#[tokio::test]
async fn expired_keys_are_swept_from_the_publishers_gossip_state() {
    let port = random_udp_port().await;
    let node = hook_node(Arc::new(start_node(port, vec![], "gc-solo").await)).await;

    const N: usize = 300;
    for i in 0..N {
        assert!(node.local.set(format!("ttl:{i}"), b"v".to_vec(), Some(Duration::from_secs(1))));
    }
    // The drain has put them all in state.
    assert!(
        wait_until(
            || async { node.handle.gossip_state_size().await.live >= N },
            Duration::from_secs(5)
        )
        .await,
        "all writes must reach gossip state: {:?}",
        node.handle.gossip_state_size().await
    );

    // After expiry + slack + a sweep, none is still published.
    let swept = wait_until(
        || async { node.handle.gossip_state_size().await.live == 0 },
        Duration::from_secs(3) + SWEEP_INTERVAL * 2,
    )
    .await;
    let size = node.handle.gossip_state_size().await;
    assert!(swept, "expired keys must be swept: {size:?}");
    assert!(size.awaiting_gc >= N, "swept entries wait for chitchat's GC: {size:?}");
    assert!(node.handle.gossip_writer_stats().swept >= N as u64);
    // Readers never saw them after expiry either.
    assert!(node.handle.gossip_get("ttl:0").await.is_none());
    assert!(node.handle.gossip_keys().await.iter().all(|k| !k.starts_with("ttl:")));

    node.shutdown().await;
}

/// When a peer overwrites a key, the original publisher retires its own
/// copy: afterwards exactly one node publishes the key, and the value is
/// the peer's everywhere.
#[tokio::test]
async fn a_superseded_copy_is_retired_from_its_publisher() {
    let port1 = random_udp_port().await;
    let port2 = random_udp_port().await;
    let seed = format!("127.0.0.1:{port1}");
    let h1 = Arc::new(start_node(port1, vec![], "retire-a").await);
    let h2 = Arc::new(start_node(port2, vec![seed], "retire-b").await);
    wait_for_convergence(&[&h1, &h2], 2, Duration::from_secs(15)).await;
    let a = hook_node(h1).await;
    let b = hook_node(h2).await;

    // A writes; B materializes it; both publish nothing but A's copy.
    assert!(a.local.set("shared".into(), b"from-a".to_vec(), None));
    assert!(
        wait_until(
            || async { b.local.get("shared").as_deref() == Some(b"from-a".as_slice()) },
            Duration::from_secs(10)
        )
        .await,
        "B must materialize A's write"
    );
    assert!(a.handle.gossip_self_publishes("shared").await);
    assert!(!b.handle.gossip_self_publishes("shared").await);

    // B overwrites. A applies it, then retires its own (older) copy.
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(b.local.set("shared".into(), b"from-b".to_vec(), None));
    assert!(
        wait_until(
            || async { a.local.get("shared").as_deref() == Some(b"from-b".as_slice()) },
            Duration::from_secs(10)
        )
        .await,
        "A must apply B's newer write"
    );
    assert!(
        wait_until(
            || async { !a.handle.gossip_self_publishes("shared").await },
            Duration::from_secs(5)
        )
        .await,
        "A must retire its superseded copy"
    );
    assert!(b.handle.gossip_self_publishes("shared").await, "B is now the sole publisher");
    // Both still read B's value (A from B's state, B from its own).
    assert_eq!(a.handle.gossip_get("shared").await.as_deref(), Some(b"from-b".as_slice()));
    assert_eq!(b.handle.gossip_get("shared").await.as_deref(), Some(b"from-b".as_slice()));

    // A delete from A now leaves a marker on A only, and B drops its copy.
    assert!(a.local.remove("shared"));
    assert!(
        wait_until(|| async { b.local.get("shared").is_none() }, Duration::from_secs(10)).await,
        "B must apply A's tombstone"
    );
    assert!(
        wait_until(
            || async { !b.handle.gossip_self_publishes("shared").await },
            Duration::from_secs(5)
        )
        .await,
        "B must retire its copy under A's newer tombstone"
    );

    a.shutdown().await;
    b.shutdown().await;
}

/// A write is readable on its origin before the drain has run, and the
/// writer never blocks the caller.
#[tokio::test]
async fn queued_writes_are_read_your_write_on_the_origin() {
    let port = random_udp_port().await;
    let node = hook_node(Arc::new(start_node(port, vec![], "ryw").await)).await;
    let started = Instant::now();
    for i in 0..2_000 {
        assert!(node.local.set(format!("ryw:{i}"), i.to_string().into_bytes(), None));
    }
    assert!(started.elapsed() < Duration::from_secs(2), "2000 hooked writes must not block");
    // Immediately visible through every read path on the origin.
    assert_eq!(node.local.get("ryw:1999").as_deref(), Some(b"1999".as_slice()));
    assert_eq!(node.handle.gossip_get("ryw:1999").await.as_deref(), Some(b"1999".as_slice()));
    assert!(node.handle.gossip_keys().await.contains(&"ryw:1999".to_string()));
    assert!(
        wait_until(
            || async { node.handle.gossip_writer_stats().pending == 0 },
            Duration::from_secs(5)
        )
        .await,
        "the drain must empty the queue"
    );
    assert_eq!(node.handle.gossip_writer_stats().shed, 0);
    node.shutdown().await;
}
