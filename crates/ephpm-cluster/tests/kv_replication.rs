//! Integration tests for large-value KV replication.
//!
//! These spin up several in-process cluster nodes, each with its own
//! local [`Store`], gossip [`ClusterHandle`], KV data plane listener, and
//! [`ClusteredStore`]. A large value is written with
//! `replication_factor = 2` and read back from a non-owner node; then the
//! owner node is dropped and the value is read again via a replica.
//!
//! ## Loopback addressing
//!
//! The production replica addressing maps a peer's gossip IP to the
//! *local* `data_port` (every real node shares one data port on a
//! different host). To reproduce that in-process, each node binds its
//! gossip UDP and TCP data plane on a distinct `127.0.0.x` address while
//! sharing the same `data_port`. Linux routes all of `127.0.0.0/8`, so
//! CI exercises the full path. Some platforms (Windows, stock macOS)
//! only bind `127.0.0.1`; there, [`loopback_aliases_available`] returns
//! `false` and the multi-node tests skip with a logged notice rather
//! than failing on an environment limitation.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ephpm_cluster::ClusteredStore;
use ephpm_cluster::node::{ClusterHandle, NodeState, start_gossip};
use ephpm_cluster::secure_transport::ClusterCipher;
use ephpm_config::{ClusterConfig, ClusterKvConfig};
use ephpm_kv::store::{Store, StoreConfig};

/// Pick a currently-free TCP port on loopback. Each test uses its own
/// data port (shared by that test's nodes on distinct `127.0.0.x`) so
/// concurrently-running tests never collide on a fixed port.
fn free_data_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// A single in-process cluster node under test.
struct TestNode {
    handle: Arc<ClusterHandle>,
    store: Arc<ClusteredStore>,
    /// Aborts the data plane listener task on drop.
    data_plane: tokio::task::JoinHandle<()>,
}

impl TestNode {
    async fn shutdown(self) {
        self.data_plane.abort();
        drop(self.store);
        Arc::try_unwrap(self.handle)
            .expect("all other handle Arcs dropped before shutdown")
            .shutdown()
            .await;
    }
}

/// Whether this platform can bind `127.0.0.2` (loopback aliases). Linux
/// routes all of `127.0.0.0/8`; Windows/macOS usually do not.
fn loopback_aliases_available() -> bool {
    std::net::TcpListener::bind("127.0.0.2:0").is_ok()
}

/// Build the KV config used by every test node.
fn kv_config(data_port: u16) -> ClusterKvConfig {
    ClusterKvConfig {
        // Force everything above a tiny threshold onto the large-value
        // (data plane) tier so we actually exercise replication.
        small_key_threshold: 8,
        replication_factor: 2,
        replication_mode: "async".to_string(),
        // Disable the hot-key cache so reads always hit the data plane
        // and we test the true replica fallback, not a local cache.
        hot_key_cache: false,
        data_port,
        ..ClusterKvConfig::default()
    }
}

/// Start one node on `ip` (a `127.0.0.x` loopback), seeded at `seeds`.
async fn start_test_node(
    ip: &str,
    data_port: u16,
    seeds: Vec<String>,
    node_id: &str,
    secret: &str,
    replication_mode: &str,
) -> TestNode {
    // Bind gossip on a random UDP port on this IP.
    let gossip_sock =
        tokio::net::UdpSocket::bind(format!("{ip}:0")).await.expect("bind gossip udp");
    let gossip_port = gossip_sock.local_addr().expect("gossip local_addr").port();
    drop(gossip_sock); // Release so chitchat can bind it.

    let mut config = kv_config(data_port);
    config.replication_mode = replication_mode.to_string();

    let cluster_config = ClusterConfig {
        enabled: true,
        bind: format!("{ip}:{gossip_port}"),
        join: seeds,
        secret: secret.to_string(),
        allow_insecure_no_auth: false,
        node_id: node_id.to_string(),
        cluster_id: "kv-repl-test".to_string(),
        kv: config.clone(),
    };
    let handle = Arc::new(
        start_gossip(&cluster_config)
            .await
            .unwrap_or_else(|e| panic!("gossip start failed for {node_id}: {e}")),
    );

    let store = Store::new(StoreConfig::default());
    let cipher: Option<Arc<ClusterCipher>> = if secret.is_empty() {
        None
    } else {
        Some(Arc::new(ClusterCipher::for_kv_data_plane(secret)))
    };

    // Data plane listens on this node's IP + this test's data port.
    // `serve_on` binds this exact address (not 0.0.0.0), so several
    // nodes can share one host on distinct 127.0.0.x IPs.
    let data_addr: SocketAddr = format!("{ip}:{data_port}").parse().expect("data addr");
    let data_store = Arc::clone(&store);
    let data_cipher = cipher.clone();
    let data_plane = tokio::spawn(async move {
        if let Err(e) =
            ephpm_cluster::data_plane::serve_on(data_store, data_addr, data_cipher).await
        {
            eprintln!("data plane serve_on failed on {data_addr}: {e}");
        }
    });

    let clustered = ClusteredStore::new(store, Arc::clone(&handle), config, cipher);
    TestNode { handle, store: clustered, data_plane }
}

/// Wait until every handle sees `expected` alive nodes.
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

/// Read `key` from `store`, retrying briefly to absorb gossip/replica
/// propagation. Returns the value or `None` if not readable in time.
async fn read_with_retry(store: &ClusteredStore, key: &str, timeout: Duration) -> Option<Vec<u8>> {
    let start = Instant::now();
    loop {
        if let Some(v) = store.get(key).await {
            return Some(v);
        }
        if start.elapsed() > timeout {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Core scenario shared by the async and sync tests.
async fn replication_survives_owner_loss(mode: &str, secret: &str) {
    if !loopback_aliases_available() {
        eprintln!(
            "skipping replication_survives_owner_loss[{mode}]: platform lacks 127.0.0.x \
             loopback aliases (Linux-only in-process multi-node harness)"
        );
        return;
    }

    // Three nodes on distinct loopback IPs, all sharing this test's own
    // data port (a fresh ephemeral port so parallel tests don't collide).
    let data_port = free_data_port();
    let n1 = start_test_node("127.0.0.1", data_port, vec![], "repl-a", secret, mode).await;
    let seed = n1.handle.self_node().gossip_addr.clone();
    let n2 =
        start_test_node("127.0.0.2", data_port, vec![seed.clone()], "repl-b", secret, mode).await;
    let n3 = start_test_node("127.0.0.3", data_port, vec![seed], "repl-c", secret, mode).await;

    wait_for_convergence(
        &[n1.handle.as_ref(), n2.handle.as_ref(), n3.handle.as_ref()],
        3,
        Duration::from_secs(15),
    )
    .await;

    // Write a large value from node 1. With replication_factor = 2 it
    // lands on its primary owner plus one secondary.
    let key = "big:payload";
    let value = vec![0xABu8; 4096];
    assert!(n1.store.set(key.to_string(), value.clone(), None).await, "primary set must succeed");

    // Give async replication a moment to fan out.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Readable from every node (each either owns a copy or fetches it
    // from a replica over the data plane).
    for (name, node) in [("n1", &n1), ("n2", &n2), ("n3", &n3)] {
        let got = read_with_retry(node.store.as_ref(), key, Duration::from_secs(10)).await;
        assert_eq!(got.as_deref(), Some(value.as_slice()), "value must be readable from {name}");
    }

    // Determine the primary owner of the key and drop that node, then
    // assert a surviving node can still read via the replica.
    let owner_id = {
        let mut alive = n1.handle.nodes().await;
        alive.retain(|n| n.state == NodeState::Alive);
        alive.sort_by(|a, b| a.id.cmp(&b.id));
        // Mirror the production owner selection: hash(key) % alive.len().
        // Reduce mod len in u64 first, so the cast to usize is lossless.
        let len = alive.len() as u64;
        let idx = usize::try_from(fnv_like(key) % len).expect("index fits usize");
        alive[idx].id.clone()
    };

    // Pick a surviving reader that is NOT the owner.
    let nodes = [n1, n2, n3];
    let (owner_idx, _) = nodes
        .iter()
        .enumerate()
        .find(|(_, n)| n.handle.self_node().id == owner_id)
        .expect("owner must be one of the three nodes");

    // Shut the owner down (simulates node loss).
    let mut survivors = Vec::new();
    for (i, node) in nodes.into_iter().enumerate() {
        if i == owner_idx {
            node.shutdown().await;
        } else {
            survivors.push(node);
        }
    }

    // Give gossip a chance to mark the owner dead so replica_nodes()
    // drops it. This is best-effort: even if the owner is still listed
    // as alive, the read fallback tolerates it — the fetch to the dead
    // owner's (aborted) data plane fails and the loop moves on to the
    // live replica. So we wait but do not hard-fail on non-convergence.
    let converge_deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < converge_deadline {
        let mut all_two = true;
        for n in &survivors {
            let alive =
                n.handle.nodes().await.iter().filter(|m| m.state == NodeState::Alive).count();
            if alive != 2 {
                all_two = false;
                break;
            }
        }
        if all_two {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // The value must still be readable from a surviving node via the
    // replica copy — this is the whole point of replication.
    let reader = &survivors[0];
    let got = read_with_retry(reader.store.as_ref(), key, Duration::from_secs(15)).await;
    assert_eq!(
        got.as_deref(),
        Some(value.as_slice()),
        "value must survive owner loss via a replica ({mode} mode)"
    );

    for node in survivors {
        node.shutdown().await;
    }
}

/// Hash matching `clustered_store::hash_key` (Rust `DefaultHasher` over
/// the key string) so the test computes the same primary owner.
fn fnv_like(key: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

#[tokio::test]
async fn async_replication_survives_owner_loss() {
    replication_survives_owner_loss("async", "").await;
}

#[tokio::test]
async fn sync_replication_survives_owner_loss() {
    replication_survives_owner_loss("sync", "").await;
}

#[tokio::test]
async fn encrypted_replication_survives_owner_loss() {
    replication_survives_owner_loss("sync", "kv-repl-secret").await;
}
