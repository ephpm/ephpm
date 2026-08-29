//! Broadcast-tier KV replication: values every node must hold **locally**.
//!
//! The sharded large-value tier is the right default for cache and session
//! data: a multi-kilobyte value goes to `replication_factor` nodes and any
//! other node fetches it through from an owner on a miss. It is the wrong
//! default for cluster-wide state that every node reads out of its own raw
//! [`Store`] with no round trip — an ACME certificate above all, because every
//! node terminates TLS with it and `acme::get_acme_cert` reads the local map
//! directly.
//!
//! That mismatch was a live outage: on a three-node cluster the ACME leader
//! issued a 5140-byte chain against a 4096-byte `small_key_threshold`, so the
//! certificate took the sharded tier, landed on two nodes, and the third
//! answered every TLS handshake with `tlsv1 alert access denied`.
//!
//! These tests pin both halves of the fix, using the same in-process
//! multi-node harness as `kv_replication.rs` (see its module docs for the
//! `127.0.0.x` loopback-alias arrangement and why non-Linux platforms skip):
//!
//! 1. [`broadcast_reaches_a_node_outside_the_replica_set`] — a cert-sized
//!    value written with `Store::set_broadcast` is readable from the raw local
//!    store of the node that the sharded tier would have excluded.
//! 2. [`sharded_set_is_invisible_to_a_non_replica_but_get_cluster_finds_it`] —
//!    the regression control. The same value written with plain `Store::set`
//!    is *never* local on that node, which is exactly the bug; and
//!    `Store::get_cluster` still finds it, which is the read-side half of the
//!    fix that covers a node which joined after the write.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ephpm_cluster::node::{ClusterHandle, NodeState, start_gossip};
use ephpm_cluster::{ClusteredStore, KvReplicator};
use ephpm_config::{ClusterConfig, ClusterKvConfig};
use ephpm_kv::store::{Replicator, Store, StoreConfig};

/// Size of the certificate chain from the live incident, so the test payload
/// is the shape that actually broke rather than a round number.
const CERT_SIZED: usize = 5140;

/// The KV key an ACME certificate is stored under, so the test exercises the
/// real key shape (nothing routes on the key text, but it keeps the failure
/// message legible).
const CERT_KEY: &str = "acme:cert:example.com:cert";

/// Pick a currently-free TCP port on loopback, so concurrently-running tests
/// never collide on a fixed data port.
fn free_data_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// Whether this platform can bind `127.0.0.2` (loopback aliases). Linux routes
/// all of `127.0.0.0/8`; Windows and stock macOS do not.
fn loopback_aliases_available() -> bool {
    std::net::TcpListener::bind("127.0.0.2:0").is_ok()
}

/// One in-process cluster node: gossip, a local [`Store`] with the production
/// [`KvReplicator`] installed, a data plane listener, and a [`ClusteredStore`].
struct TestNode {
    handle: Arc<ClusterHandle>,
    /// The raw local store — what `acme::get_acme_cert` reads.
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
        // Order matters: the local store holds the replicator, the replicator
        // holds the `ClusteredStore`, and that holds the `ClusterHandle`. Drop
        // them front to back or the handle still has live `Arc`s.
        self.local.set_replicator(None);
        drop(self.clustered);
        drop(self.local);
        match Arc::try_unwrap(self.handle) {
            Ok(handle) => handle.shutdown().await,
            // Teardown only — a lingering `Arc` here would turn an unrelated
            // failure into a confusing panic in the wrong test.
            Err(_) => eprintln!("test node handle still shared at shutdown; skipping gossip stop"),
        }
    }
}

/// Start one node on `ip` (a `127.0.0.x` loopback), seeded at `seeds`.
async fn start_test_node(ip: &str, data_port: u16, seeds: Vec<String>, node_id: &str) -> TestNode {
    let gossip_sock =
        tokio::net::UdpSocket::bind(format!("{ip}:0")).await.expect("bind gossip udp");
    let gossip_port = gossip_sock.local_addr().expect("gossip local_addr").port();
    drop(gossip_sock); // Release so chitchat can bind it.

    let kv = ClusterKvConfig {
        // Anything above a tiny threshold takes the large-value tier, so the
        // cert-sized payload below exercises data-plane routing exactly as it
        // does in production against a 4096-byte threshold.
        small_key_threshold: 8,
        replication_factor: 2,
        replication_mode: "sync".to_string(),
        // No hot-key cache: a read must reflect where the value really is, not
        // a local cache of a previous remote fetch.
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
        cluster_id: "broadcast-kv-test".to_string(),
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

    // Install the sync bridge, exactly as `serve()` does at startup — so the
    // tests drive `Store::set` / `set_broadcast` / `get_cluster`, the same
    // public entry points the ACME code calls, rather than reaching into
    // `ClusteredStore` directly.
    let replicator = KvReplicator::new(
        Arc::clone(&clustered),
        tokio::runtime::Handle::current(),
        ephpm_cluster::clustered_store::new_applied_write_map(),
    );
    local.set_replicator(Some(replicator as Arc<dyn Replicator>));

    TestNode { handle, local, clustered, data_plane }
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

/// Hash matching `clustered_store::hash_key` so the test computes the same
/// replica set the production router does.
fn hash_key(key: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

/// The id of the one node that the sharded tier leaves out: with three alive
/// nodes and `replication_factor = 2`, the replica set is two consecutive
/// nodes starting at `hash(key) % 3`, so the excluded node is the third.
async fn non_replica_id(handle: &ClusterHandle, key: &str) -> String {
    let mut alive = handle.nodes().await;
    alive.retain(|n| n.state == NodeState::Alive);
    alive.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(alive.len(), 3, "expected a converged three-node cluster");
    let primary = usize::try_from(hash_key(key) % 3).expect("index fits usize");
    alive[(primary + 2) % 3].id.clone()
}

/// A converged three-node cluster.
async fn three_nodes() -> [TestNode; 3] {
    let data_port = free_data_port();
    let n1 = start_test_node("127.0.0.1", data_port, vec![], "bcast-a").await;
    let seed = n1.handle.self_node().gossip_addr.clone();
    let n2 = start_test_node("127.0.0.2", data_port, vec![seed.clone()], "bcast-b").await;
    let n3 = start_test_node("127.0.0.3", data_port, vec![seed], "bcast-c").await;

    wait_for_convergence(
        &[n1.handle.as_ref(), n2.handle.as_ref(), n3.handle.as_ref()],
        3,
        Duration::from_secs(15),
    )
    .await;

    [n1, n2, n3]
}

/// Poll a node's **raw local** store until the key materialises.
async fn await_local(node: &TestNode, key: &str, timeout: Duration) -> Option<Vec<u8>> {
    let start = Instant::now();
    loop {
        if let Some(v) = node.local.get(key) {
            return Some(v.to_vec());
        }
        if start.elapsed() > timeout {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The fix: `set_broadcast` puts a cert-sized value on **every** node's local
/// store, including the one the sharded tier would have excluded. That local
/// copy is what `acme::get_acme_cert` reads, and what the node needs in order
/// to answer a TLS handshake at all.
#[tokio::test]
async fn broadcast_reaches_a_node_outside_the_replica_set() {
    if !loopback_aliases_available() {
        eprintln!(
            "skipping broadcast_reaches_a_node_outside_the_replica_set: platform lacks \
             127.0.0.x loopback aliases (Linux-only in-process multi-node harness)"
        );
        return;
    }

    let [n1, n2, n3] = three_nodes().await;
    let value = vec![0xACu8; CERT_SIZED];

    // Written through the public `Store` seam, the same call `store_acme_cert`
    // makes.
    assert!(
        n1.local.set_broadcast(CERT_KEY.to_string(), value.clone(), None),
        "broadcast write must succeed on the writer"
    );

    let excluded = non_replica_id(n1.handle.as_ref(), CERT_KEY).await;
    let nodes = [&n1, &n2, &n3];

    // Every node — not just the replica set — must hold a local copy. The
    // payload is 5 KB, so report presence and length rather than letting a
    // failure dump the whole chain, then check the bytes separately.
    for node in nodes {
        let got = await_local(node, CERT_KEY, Duration::from_secs(15)).await;
        let role = if node.id() == excluded { "non-replica" } else { "replica" };
        assert_eq!(
            got.as_ref().map(Vec::len),
            Some(CERT_SIZED),
            "node {} ({role}) must hold a local copy of a broadcast value",
            node.id()
        );
        assert!(
            got.as_deref() == Some(value.as_slice()),
            "node {} ({role}) holds a local copy with the wrong contents",
            node.id()
        );
    }

    // Name the node the sharded tier would have missed, so a failure here
    // reads as the regression it is rather than a generic replication flake.
    assert!(
        nodes.iter().any(|n| n.id() == excluded),
        "the excluded node must be one of the three under test"
    );

    for node in [n1, n2, n3] {
        node.shutdown().await;
    }
}

/// The control, and the read-side half of the fix.
///
/// A plain `Store::set` of the same value is the pre-fix behaviour: the node
/// outside the replica set never receives it, so a raw local read — which is
/// all `acme::get_acme_cert` does — returns `None` forever. `get_cluster`
/// nevertheless finds it, which is what lets a node that joined *after*
/// issuance install the leader's certificate.
#[tokio::test]
async fn sharded_set_is_invisible_to_a_non_replica_but_get_cluster_finds_it() {
    if !loopback_aliases_available() {
        eprintln!(
            "skipping sharded_set_is_invisible_to_a_non_replica_but_get_cluster_finds_it: \
             platform lacks 127.0.0.x loopback aliases"
        );
        return;
    }

    let [n1, n2, n3] = three_nodes().await;
    let value = vec![0xACu8; CERT_SIZED];

    assert!(
        n1.local.set(CERT_KEY.to_string(), value.clone(), None),
        "sharded write must succeed on the writer"
    );

    let excluded_id = non_replica_id(n1.handle.as_ref(), CERT_KEY).await;
    let excluded = [&n1, &n2, &n3]
        .into_iter()
        .find(|n| n.id() == excluded_id)
        .expect("the excluded node must be one of the three under test");

    // Generous settle time: `replication_mode = "sync"` awaits every reachable
    // replica, so if this value were going to reach the non-replica node it
    // would have by now.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        excluded.local.get(CERT_KEY),
        None,
        "a sharded write must NOT be local on node {excluded_id} — if this starts passing, \
         the replica set has changed and this control no longer reproduces the bug"
    );

    // ...but the cluster-aware read still resolves it, by fetching from a
    // replica over the data plane.
    let fetched = excluded
        .local
        .get_cluster(CERT_KEY)
        .await
        .expect("get_cluster must fetch a non-local value from the key's replica set");
    assert_eq!(fetched.len(), CERT_SIZED, "get_cluster returned a truncated value");
    assert!(
        fetched.as_ref() == value.as_slice(),
        "get_cluster returned the wrong bytes for {CERT_KEY}"
    );

    for node in [n1, n2, n3] {
        node.shutdown().await;
    }
}
