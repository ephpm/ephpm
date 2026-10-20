//! Convergence of concurrently written gossip-tier keys.
//!
//! Stress finding F-7: with twelve writers on three nodes hammering twenty
//! keys, 6 of 20 keys were still different on different nodes 30 s after the
//! writers stopped — permanently. Two causes, both in the write path: the
//! origin stamped its local bookkeeping and the gossip payload at two
//! different instants, so two nodes could each beat the other; and equal
//! stamps were treated as stale, so a same-millisecond tie was never
//! resolved. This test is that scenario in-process: every node must end up
//! holding the same value for every key once the writers stop.
//!
//! Gossip-only (small values), so the nodes share `127.0.0.1` on distinct
//! UDP ports and the test runs on every platform.

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
        cluster_id: "convergence-test".to_string(),
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

/// One node wired as `ephpm-server::serve()` wires it: local store, sync
/// replicator hook, gossip applier, one shared applied map.
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

/// Twelve sync writers (four per node) overwrite twenty keys for a few
/// seconds; afterwards every node must hold identical values for all of
/// them, and keep holding them (no late echo may flip a key back).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_hot_key_writes_converge_on_every_node() {
    const KEYS: usize = 20;
    const WRITERS_PER_NODE: usize = 4;
    const WRITE_FOR: Duration = Duration::from_secs(3);

    let ports = [random_udp_port().await, random_udp_port().await, random_udp_port().await];
    let seed = format!("127.0.0.1:{}", ports[0]);
    let h1 = Arc::new(start_node(ports[0], vec![], "conv-a").await);
    let h2 = Arc::new(start_node(ports[1], vec![seed.clone()], "conv-b").await);
    let h3 = Arc::new(start_node(ports[2], vec![seed], "conv-c").await);
    wait_for_convergence(&[&h1, &h2, &h3], 3, Duration::from_secs(15)).await;

    let nodes = [hook_node(h1).await, hook_node(h2).await, hook_node(h3).await];

    // Writers: each spins on its node's sync `Store::set` (the RESP / PHP
    // path), round-robin over the keys, with a value naming the writer.
    let mut writers = Vec::new();
    for (n, node) in nodes.iter().enumerate() {
        for w in 0..WRITERS_PER_NODE {
            let store = Arc::clone(&node.local);
            writers.push(tokio::task::spawn_blocking(move || {
                let end = Instant::now() + WRITE_FOR;
                let mut i = 0usize;
                while Instant::now() < end {
                    let key = format!("hot:{}", i % KEYS);
                    assert!(store.set(key, format!("n{n}:w{w}:{i}").into_bytes(), None));
                    i += 1;
                    if i % 50 == 0 {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
                i
            }));
        }
    }
    let mut total = 0;
    for w in writers {
        total += w.await.expect("writer task");
    }
    assert!(total > 1000, "writers must have produced real contention, got {total} writes");

    // Converge: every key equal on every node, and stable for a further
    // settling window so a late echo that flips a key back is caught.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut stable_since: Option<Instant> = None;
    loop {
        let mut diverged = Vec::new();
        for k in 0..KEYS {
            let key = format!("hot:{k}");
            let values: Vec<Option<Vec<u8>>> =
                nodes.iter().map(|n| n.local.get(&key).map(|b| b.to_vec())).collect();
            if values.iter().any(|v| v != &values[0]) {
                diverged.push(key);
            }
        }
        if diverged.is_empty() {
            let since = *stable_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_secs(3) {
                break;
            }
        } else {
            stable_since = None;
            assert!(
                Instant::now() < deadline,
                "{} of {KEYS} keys still differ across nodes after the writers stopped: {diverged:?}",
                diverged.len()
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Every key was written by someone, so none may be missing.
    for k in 0..KEYS {
        assert!(nodes[0].local.get(&format!("hot:{k}")).is_some(), "hot:{k} must exist");
    }

    for node in nodes {
        node.shutdown().await;
    }
}
