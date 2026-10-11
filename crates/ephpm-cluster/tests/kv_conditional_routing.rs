//! Cluster-wide `SETNX` / compare-and-delete: one arbiter per key.
//!
//! Every node's `Store` is atomic under its own shard lock, but a cluster has
//! one map per node. When `SETNX` was decided on each node's own copy, two
//! nodes racing for the same lock both saw "absent" and both acquired it —
//! the replicated value then converged last-arrival-wins, long after both
//! callers had entered the critical section. The release side had the mirror
//! bug: a holder whose lock had been re-granted elsewhere still held its old
//! token in its local copy, so a local compare-and-delete "matched" and
//! broadcast a tombstone that deleted the **new** holder's lock.
//!
//! The fix routes both operations to the key's primary owner over the KV data
//! plane, so every node's attempt on a key is serialised by the same shard lock
//! on the same node. These tests drive the production wiring — `KvReplicator`
//! installed on each node's `Store`, a data-plane listener, and the gossip
//! applier — through the same public `Store::set_nx` / `del_if_eq` entry points
//! the PHP bridge and RESP dispatcher use.
//!
//! Uses the `127.0.0.x` loopback-alias harness from `kv_replication.rs`, so the
//! tests skip on platforms without loopback aliases (Windows, stock macOS).
//! They run on the multi-thread runtime because routing blocks the calling
//! thread for a network round trip, exactly as it does in production.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ephpm_cluster::node::{ClusterHandle, NodeState, start_gossip};
use ephpm_cluster::{ClusteredStore, KvReplicator};
use ephpm_config::{ClusterConfig, ClusterKvConfig};
use ephpm_kv::store::{Replicator, SetNxOutcome, Store, StoreConfig};

fn free_data_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

fn loopback_aliases_available() -> bool {
    std::net::TcpListener::bind("127.0.0.2:0").is_ok()
}

struct TestNode {
    handle: Arc<ClusterHandle>,
    local: Arc<Store>,
    clustered: Arc<ClusteredStore>,
    data_plane: tokio::task::JoinHandle<()>,
}

impl TestNode {
    fn id(&self) -> String {
        self.handle.self_node().id
    }

    async fn shutdown(self) {
        self.data_plane.abort();
        self.local.set_replicator(None);
        drop(self.clustered);
        drop(self.local);
        match Arc::try_unwrap(self.handle) {
            Ok(handle) => handle.shutdown().await,
            Err(_) => eprintln!("test node handle still shared at shutdown; skipping gossip stop"),
        }
    }
}

async fn start_test_node(ip: &str, data_port: u16, seeds: Vec<String>, node_id: &str) -> TestNode {
    let gossip_sock =
        tokio::net::UdpSocket::bind(format!("{ip}:0")).await.expect("bind gossip udp");
    let gossip_port = gossip_sock.local_addr().expect("gossip local_addr").port();
    drop(gossip_sock);

    // Lock tokens are small, so they ride the gossip tier — the production
    // shape of a session lock or a `Cache::lock()`.
    let kv = ClusterKvConfig {
        small_key_threshold: 4096,
        hot_key_cache: false,
        data_port,
        ..ClusterKvConfig::default()
    };
    let cluster_config = ClusterConfig {
        enabled: true,
        bind: format!("{ip}:{gossip_port}"),
        join: seeds,
        secret: String::new(),
        allow_insecure_no_auth: false,
        node_id: node_id.to_string(),
        cluster_id: "kv-conditional-test".to_string(),
        kv: kv.clone(),
        ..ClusterConfig::default()
    };
    let handle = Arc::new(
        start_gossip(&cluster_config)
            .await
            .unwrap_or_else(|e| panic!("gossip start failed for {node_id}: {e}")),
    );

    let local = Store::new(StoreConfig::default());
    let data_addr: SocketAddr = format!("{ip}:{data_port}").parse().expect("data addr");
    let data_store = Arc::clone(&local);
    let data_plane = tokio::spawn(async move {
        if let Err(e) = ephpm_cluster::data_plane::serve_on(data_store, data_addr, None).await {
            eprintln!("data plane serve_on failed on {data_addr}: {e}");
        }
    });

    let clustered = ClusteredStore::new(Arc::clone(&local), Arc::clone(&handle), kv, None);
    let applied = ephpm_cluster::clustered_store::new_applied_write_map();
    let replicator = KvReplicator::new(
        Arc::clone(&clustered),
        tokio::runtime::Handle::current(),
        applied.clone(),
    );
    local.set_replicator(Some(replicator as Arc<dyn Replicator>));
    ephpm_cluster::clustered_store::start_gossip_applier(&handle, Arc::clone(&local), applied)
        .await;

    TestNode { handle, local, clustered, data_plane }
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

async fn three_nodes(prefix: &str) -> [TestNode; 3] {
    let data_port = free_data_port();
    let n1 = start_test_node("127.0.0.1", data_port, vec![], &format!("{prefix}-a")).await;
    let seed = n1.handle.self_node().gossip_addr.clone();
    let n2 =
        start_test_node("127.0.0.2", data_port, vec![seed.clone()], &format!("{prefix}-b")).await;
    let n3 = start_test_node("127.0.0.3", data_port, vec![seed], &format!("{prefix}-c")).await;
    wait_for_convergence(
        &[n1.handle.as_ref(), n2.handle.as_ref(), n3.handle.as_ref()],
        3,
        Duration::from_secs(15),
    )
    .await;
    [n1, n2, n3]
}

/// Index (into `nodes`) of the node that arbitrates `key`, as the router
/// computes it from this node's membership view.
async fn owner_index(nodes: &[TestNode; 3], key: &str) -> usize {
    let owner = nodes[0].clustered.conditional_owner(key).await;
    let owner_id = owner.map_or_else(|| nodes[0].id(), |n| n.id);
    nodes.iter().position(|n| n.id() == owner_id).expect("owner is one of the three nodes")
}

async fn wait_local(node: &TestNode, key: &str, want: &[u8], timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if node.local.get(key).as_deref() == Some(want) {
            return true;
        }
        if start.elapsed() > timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The bug: concurrent `SETNX` on the same key from every node. Decided per
/// node, each node had its own winner. Routed to the owner, there is one
/// winner in the whole cluster — for every key, whichever node owns it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn setnx_has_exactly_one_winner_across_the_cluster() {
    if !loopback_aliases_available() {
        eprintln!("skipping: platform lacks 127.0.0.x loopback aliases");
        return;
    }
    let nodes = three_nodes("nx").await;
    let locals: Vec<Arc<Store>> = nodes.iter().map(|n| Arc::clone(&n.local)).collect();

    for k in 0..6 {
        let key = format!("lock:job-{k}");
        let winners = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for (n, local) in locals.iter().enumerate() {
            for t in 0..4 {
                let local = Arc::clone(local);
                let key = key.clone();
                let winners = Arc::clone(&winners);
                // `spawn_blocking`: the caller shape of a PHP worker thread.
                tasks.push(tokio::task::spawn_blocking(move || {
                    let token = format!("node{n}-t{t}").into_bytes();
                    match local.set_nx_outcome(key, token, Some(Duration::from_secs(30))) {
                        SetNxOutcome::Inserted => {
                            winners.fetch_add(1, Ordering::SeqCst);
                        }
                        SetNxOutcome::Exists => {}
                        other => panic!("unexpected SETNX outcome {other:?}"),
                    }
                }));
            }
        }
        for task in tasks {
            task.await.expect("setnx task");
        }
        assert_eq!(
            winners.load(Ordering::SeqCst),
            1,
            "{key}: exactly one caller in the cluster may acquire (owner = node {})",
            owner_index(&nodes, &key).await
        );
    }

    for node in nodes {
        node.shutdown().await;
    }
}

/// The release bug: a holder whose lock was re-granted to someone else still
/// holds its old token locally. A local compare "matched" and broadcast a
/// tombstone that deleted the new holder's lock. Decided on the owner, the
/// stale release is a no-op and the new holder keeps its lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_holder_cannot_release_a_regranted_lock() {
    if !loopback_aliases_available() {
        eprintln!("skipping: platform lacks 127.0.0.x loopback aliases");
        return;
    }
    let nodes = three_nodes("cad").await;
    let key = "lock:report";
    let owner = owner_index(&nodes, key).await;
    let holder = (owner + 1) % 3;

    // The holder acquires; the owner's grant replicates back to it.
    let holder_local = Arc::clone(&nodes[holder].local);
    let acquired = tokio::task::spawn_blocking(move || {
        holder_local.set_nx("lock:report".into(), b"old-holder".to_vec(), None)
    })
    .await
    .unwrap();
    assert!(acquired, "the first acquire must win");
    assert!(
        wait_local(&nodes[holder], key, b"old-holder", Duration::from_secs(10)).await,
        "the grant must replicate to the holder's node"
    );

    // On the owner, the lock moves to a new holder without that change having
    // reached the old holder's node yet (`set_local` does not replicate) — the
    // state a TTL expiry + re-grant leaves during the replication lag.
    nodes[owner].local.set_local(key.into(), b"new-holder".to_vec(), None);

    let holder_local = Arc::clone(&nodes[holder].local);
    let released = tokio::task::spawn_blocking(move || {
        holder_local.try_del_if_eq("lock:report", b"old-holder")
    })
    .await
    .unwrap();
    assert_eq!(released, Ok(false), "a stale token must not release anything");

    // Give any (erroneous) tombstone time to propagate, then check the owner.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        nodes[owner].local.get(key).as_deref(),
        Some(&b"new-holder"[..]),
        "the new holder's lock must survive a stale release"
    );

    for node in nodes {
        node.shutdown().await;
    }
}

/// An unreachable owner fails closed: nobody acquires, nothing is written on
/// the caller as a fallback.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unreachable_owner_is_not_acquired() {
    if !loopback_aliases_available() {
        eprintln!("skipping: platform lacks 127.0.0.x loopback aliases");
        return;
    }
    let nodes = three_nodes("down").await;
    let key = "lock:cron";
    let owner = owner_index(&nodes, key).await;
    let caller = (owner + 1) % 3;

    // Take the owner's data plane away while it still looks alive to gossip —
    // the window before failure detection declares it dead.
    nodes[owner].data_plane.abort();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let caller_local = Arc::clone(&nodes[caller].local);
    let outcome = tokio::task::spawn_blocking(move || {
        caller_local.set_nx_outcome("lock:cron".into(), b"tok".to_vec(), None)
    })
    .await
    .unwrap();
    assert_eq!(outcome, SetNxOutcome::OwnerUnreachable);
    assert_eq!(nodes[caller].local.get(key), None, "no local fallback acquisition");

    for node in nodes {
        node.shutdown().await;
    }
}
