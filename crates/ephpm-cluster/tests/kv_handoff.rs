//! Peer handoff of large KV values across a 3-node cluster with
//! `replication_factor = 2`: push on drain, pull on start, stamp ordering.
//!
//! Each node is wired as `ephpm-server::serve()` wires it (local `Store`,
//! `KvReplicator`, gossip applier, data-plane listener with the handoff
//! source, one applied-stamp map), so what the test proves is what the
//! server does. Loopback addressing as in `kv_large_values.rs`: distinct
//! `127.0.0.x` per node, so the suite skips on platforms without the
//! aliases.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ephpm_cluster::data_plane::KvRouter;
use ephpm_cluster::node::{ClusterHandle, NodeState, start_gossip};
use ephpm_cluster::{ClusteredStore, HandoffSource, KvReplicator, PeerHandoff};
use ephpm_config::{ClusterConfig, ClusterKvConfig};
use ephpm_kv::store::{Replicator, Store, StoreConfig};

fn free_data_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").expect("bind").local_addr().expect("addr").port()
}

fn loopback_aliases_available() -> bool {
    std::net::TcpListener::bind("127.0.0.2:0").is_ok()
}

/// Values above this ride the data plane.
const THRESHOLD: usize = 64;

fn large(tag: &str) -> Vec<u8> {
    format!("{tag}:").into_bytes().into_iter().chain(std::iter::repeat_n(b'L', 1500)).collect()
}

struct Node {
    name: &'static str,
    ip: &'static str,
    handle: Arc<ClusterHandle>,
    local: Arc<Store>,
    clustered: Arc<ClusteredStore>,
    handoff: Arc<PeerHandoff>,
    data_plane: tokio::task::JoinHandle<()>,
}

impl Node {
    async fn shutdown(self) {
        // The data-plane task's router holds the handoff source, which holds
        // the cluster handle: abort it and *wait* for it to be dropped, or
        // `try_unwrap` below sees a second owner.
        let data_plane = self.data_plane;
        data_plane.abort();
        let _ = data_plane.await;
        self.local.set_replicator(None);
        drop(self.handoff);
        drop(self.clustered);
        drop(self.local);
        Arc::try_unwrap(self.handle).expect("sole owner").shutdown().await;
    }
}

fn kv_config(data_port: u16) -> ClusterKvConfig {
    ClusterKvConfig {
        small_key_threshold: THRESHOLD,
        replication_factor: 2,
        hot_key_cache: false,
        data_port,
        ..ClusterKvConfig::default()
    }
}

async fn start_node(
    ip: &'static str,
    data_port: u16,
    seeds: Vec<String>,
    name: &'static str,
) -> Node {
    let gossip_sock = tokio::net::UdpSocket::bind(format!("{ip}:0")).await.expect("bind gossip");
    let gossip_port = gossip_sock.local_addr().expect("gossip addr").port();
    drop(gossip_sock);

    let kv = kv_config(data_port);
    let cluster_config = ClusterConfig {
        enabled: true,
        bind: format!("{ip}:{gossip_port}"),
        join: seeds,
        secret: String::new(),
        allow_insecure_no_auth: false,
        node_id: name.to_string(),
        cluster_id: "handoff-test".to_string(),
        kv: kv.clone(),
        ..ClusterConfig::default()
    };
    let handle = Arc::new(start_gossip(&cluster_config).await.expect("gossip start"));

    let local = Store::new(StoreConfig::default());
    let applied = ephpm_cluster::clustered_store::new_applied_write_map();
    let data_addr: SocketAddr = format!("{ip}:{data_port}").parse().expect("data addr");
    let source = HandoffSource::new(Arc::clone(&handle), &kv);
    let router = KvRouter::global(Arc::clone(&local))
        .with_applied(Arc::clone(&applied))
        .with_handoff(Arc::clone(&source));
    let handoff = PeerHandoff::new(Arc::clone(&handle), None, router.clone(), source, &kv);
    let data_plane = tokio::spawn(async move {
        if let Err(e) = ephpm_cluster::data_plane::serve_router(router, data_addr, None).await {
            eprintln!("data plane failed on {data_addr}: {e}");
        }
    });

    let clustered = ClusteredStore::new(Arc::clone(&local), Arc::clone(&handle), kv, None);
    let replicator = KvReplicator::new(
        Arc::clone(&clustered),
        tokio::runtime::Handle::current(),
        Arc::clone(&applied),
    );
    local.set_replicator(Some(replicator as Arc<dyn Replicator>));
    ephpm_cluster::clustered_store::start_gossip_applier(&handle, Arc::clone(&local), applied)
        .await;
    Node { name, ip, handle, local, clustered, handoff, data_plane }
}

async fn wait_alive(nodes: &[&Node], want: usize) {
    wait_alive_for(nodes, want, Duration::from_secs(20)).await;
}

/// Failure detection (a node that left) takes phi-accrual time, well past the
/// join-convergence budget, so callers waiting for a *shrink* pass a longer one.
async fn wait_alive_for(nodes: &[&Node], want: usize, budget: Duration) {
    let deadline = Instant::now() + budget;
    loop {
        let all = {
            let mut ok = true;
            for n in nodes {
                let alive =
                    n.handle.nodes().await.iter().filter(|m| m.state == NodeState::Alive).count();
                if alive != want {
                    ok = false;
                }
            }
            ok
        };
        if all {
            return;
        }
        assert!(Instant::now() < deadline, "membership did not converge to {want}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn three_nodes() -> Option<[Node; 3]> {
    if !loopback_aliases_available() {
        eprintln!("skipping: platform lacks 127.0.0.x loopback aliases (Linux-only harness)");
        return None;
    }
    let port = free_data_port();
    let n1 = start_node("127.0.0.1", port, vec![], "ho-a").await;
    let seed = n1.handle.self_node().gossip_addr.clone();
    let n2 = start_node("127.0.0.2", port, vec![seed.clone()], "ho-b").await;
    let n3 = start_node("127.0.0.3", port, vec![seed], "ho-c").await;
    wait_alive(&[&n1, &n2, &n3], 3).await;
    Some([n1, n2, n3])
}

/// Which of the three holds a local copy of `key` (by name).
fn holders(nodes: &[Node], key: &str) -> Vec<&'static str> {
    nodes.iter().filter(|n| n.local.get(key).is_some()).map(|n| n.name).collect()
}

/// A graceful restart of one node: the leaving node pushes its large values
/// to the nodes that take over, and the returning node (empty store) pulls
/// its share back — and after both, every key is readable on every node and
/// the restarted node holds every key it is a replica for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn push_on_drain_and_pull_on_start_keep_large_values_available() {
    let Some(nodes) = three_nodes().await else { return };
    let [a, b, c] = nodes;
    let nodes = [a, b, c];
    let keys: Vec<String> = (0..30).map(|i| format!("big:{i}")).collect();
    for (i, key) in keys.iter().enumerate() {
        assert!(nodes[i % 3].local.set(key.clone(), large(key), None));
    }
    // Replication is async: wait until every key has two holders.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if keys.iter().all(|k| holders(&nodes, k).len() >= 2) {
            break;
        }
        assert!(Instant::now() < deadline, "replication did not settle");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let before: Vec<Vec<&str>> = keys.iter().map(|k| holders(&nodes, k)).collect();
    let on_b: Vec<&String> =
        keys.iter().zip(&before).filter(|(_, h)| h.contains(&"ho-b")).map(|(k, _)| k).collect();
    assert!(!on_b.is_empty(), "the test needs node b to hold something");

    // b drains: push.
    let [a, b, c] = nodes;
    let report = b.handoff.push(tokio::time::Instant::now() + Duration::from_secs(10)).await;
    assert!(!report.timed_out, "{report:?}");
    assert_eq!(report.failed_writes, 0, "{report:?}");
    // b pushes only the keys it holds as a current replica (its origin-only
    // copies are held by their real replicas already), so the count is not
    // every key b held — but it is never nothing.
    assert!(report.keys > 0, "{report:?}");
    // Every key b held now has two holders among a and c.
    let ac = [a, c];
    for key in &on_b {
        let h = holders(&ac, key);
        assert_eq!(h.len(), 2, "{key}: after b's push, a and c both hold it: {h:?}");
    }
    let b_ip = b.ip;
    let port = b.handle.kv_data_plane_addr().port();
    b.shutdown().await;
    let [a, c] = ac;
    wait_alive_for(&[&a, &c], 2, Duration::from_secs(120)).await;

    // A write while b is away: must win over whatever b hands back later.
    let changed = on_b[0].clone();
    let newer = large("newer");
    assert!(a.local.set(changed.clone(), newer.clone(), None));
    tokio::time::sleep(Duration::from_millis(300)).await;

    // b comes back with an EMPTY store and pulls its share.
    let seed = a.handle.self_node().gossip_addr.clone();
    let b2 = start_node(b_ip, port, vec![seed], "ho-b").await;
    wait_alive(&[&a, &c, &b2], 3).await;
    assert_eq!(b2.local.len(), 0, "cold");
    let report = b2.handoff.pull(tokio::time::Instant::now() + Duration::from_secs(10), true).await;
    assert!(!report.timed_out && report.failed_peers == 0, "{report:?}");
    assert!(report.keys > 0, "b pulled nothing: {report:?}");
    let nodes = [a, b2, c];
    // b holds exactly its share: every key whose replica set (over the
    // full ring) includes b, and the newer value where a rewrote it.
    let b = &nodes[1];
    let source = HandoffSource::new(Arc::clone(&b.handle), &kv_config(port));
    let membership = source.membership_with("ho-b").await;
    let mut expected = 0;
    for key in &keys {
        if source.replica_includes(&membership, "ho-b", key) {
            expected += 1;
            let got = b.local.get(key);
            assert!(got.is_some(), "{key}: b is a replica but holds nothing after the pull");
            if key == &changed {
                assert_eq!(
                    got.as_deref(),
                    Some(newer.as_slice()),
                    "the write during b's absence wins"
                );
            }
        }
    }
    assert!(expected > 0);
    assert_eq!(b.local.len(), expected, "b holds exactly its share, nothing else");
    // And every key is readable everywhere through the normal path.
    for key in &keys {
        for n in &nodes {
            assert!(n.local.get_through(key).is_some(), "{key} unreadable on {}", n.name);
        }
    }
    for n in nodes {
        n.shutdown().await;
    }
}

/// A handed-off value carries a stamp that loses to every real write: a
/// write that lands on the receiver during the handoff is not overwritten.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_concurrent_write_is_never_overwritten_by_a_handed_off_value() {
    let Some(nodes) = three_nodes().await else { return };
    let [a, b, c] = nodes;
    let key = "race:key".to_string();
    assert!(b.local.set(key.clone(), large("old"), None));
    tokio::time::sleep(Duration::from_millis(300)).await;
    // The receiver writes a newer value before b pushes.
    let newer = large("new");
    assert!(a.local.set(key.clone(), newer.clone(), None));
    assert!(c.local.set(key.clone(), newer.clone(), None));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let report = b.handoff.push(tokio::time::Instant::now() + Duration::from_secs(10)).await;
    assert_eq!(report.failed_writes, 0, "{report:?}");
    assert_eq!(a.local.get(&key).as_deref(), Some(newer.as_slice()));
    assert_eq!(c.local.get(&key).as_deref(), Some(newer.as_slice()));
    for n in [a, b, c] {
        n.shutdown().await;
    }
}

/// A pull with no peers, and a push with no peers, are no-ops that report
/// zero and never block.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alone_in_the_cluster_is_a_fast_no_op() {
    if !loopback_aliases_available() {
        return;
    }
    let port = free_data_port();
    let a = start_node("127.0.0.1", port, vec![], "solo").await;
    let started = Instant::now();
    let pulled = a.handoff.pull(tokio::time::Instant::now() + Duration::from_secs(5), false).await;
    let pushed = a.handoff.push(tokio::time::Instant::now() + Duration::from_secs(5)).await;
    assert_eq!((pulled.keys, pulled.peers), (0, 0));
    assert_eq!((pushed.keys, pushed.peers), (0, 0));
    assert!(started.elapsed() < Duration::from_secs(2));
    a.shutdown().await;
}
