//! Large values (above `small_key_threshold`) across a 3-node cluster with
//! `replication_factor = 2` — stress findings F-8, F-9, F-10, F-11.
//!
//! Every node is wired as `ephpm-server::serve()` wires it: a local `Store`
//! hooked through `KvReplicator`, the gossip applier, and a data-plane
//! listener sharing one applied-stamp map. Reads go through
//! `Store::get_through`, the path RESP `GET` and PHP `ephpm_kv_get` use.
//!
//! Loopback addressing as in `kv_replication.rs`: each node binds a distinct
//! `127.0.0.x`, which Linux routes and Windows/macOS usually do not, so the
//! tests skip there with a notice.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ephpm_cluster::data_plane::KvRouter;
use ephpm_cluster::node::{ClusterHandle, NodeState, start_gossip};
use ephpm_cluster::{ClusteredStore, KvReplicator};
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

/// A large value: well above [`THRESHOLD`], distinct per call.
fn large(tag: &str) -> Vec<u8> {
    format!("{tag}:").into_bytes().into_iter().chain(std::iter::repeat_n(b'L', 2000)).collect()
}

struct Node {
    name: &'static str,
    handle: Arc<ClusterHandle>,
    local: Arc<Store>,
    clustered: Arc<ClusteredStore>,
    data_plane: tokio::task::JoinHandle<()>,
}

impl Node {
    async fn shutdown(self) {
        self.data_plane.abort();
        self.local.set_replicator(None);
        drop(self.clustered);
        drop(self.local);
        Arc::try_unwrap(self.handle).expect("sole owner").shutdown().await;
    }
}

async fn start_node(ip: &str, data_port: u16, seeds: Vec<String>, name: &'static str) -> Node {
    let gossip_sock = tokio::net::UdpSocket::bind(format!("{ip}:0")).await.expect("bind gossip");
    let gossip_port = gossip_sock.local_addr().expect("gossip addr").port();
    drop(gossip_sock);

    let kv = ClusterKvConfig {
        small_key_threshold: THRESHOLD,
        replication_factor: 2,
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
        node_id: name.to_string(),
        cluster_id: "large-values-test".to_string(),
        kv: kv.clone(),
        ..ClusterConfig::default()
    };
    let handle = Arc::new(start_gossip(&cluster_config).await.expect("gossip start"));

    let local = Store::new(StoreConfig::default());
    let applied = ephpm_cluster::clustered_store::new_applied_write_map();
    let data_addr: SocketAddr = format!("{ip}:{data_port}").parse().expect("data addr");
    let router = KvRouter::global(Arc::clone(&local)).with_applied(Arc::clone(&applied));
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
    Node { name, handle, local, clustered, data_plane }
}

async fn three_nodes() -> Option<[Node; 3]> {
    if !loopback_aliases_available() {
        eprintln!("skipping: platform lacks 127.0.0.x loopback aliases (Linux-only harness)");
        return None;
    }
    let port = free_data_port();
    let n1 = start_node("127.0.0.1", port, vec![], "lv-a").await;
    let seed = n1.handle.self_node().gossip_addr.clone();
    let n2 = start_node("127.0.0.2", port, vec![seed.clone()], "lv-b").await;
    let n3 = start_node("127.0.0.3", port, vec![seed], "lv-c").await;
    let nodes = [n1, n2, n3];
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let mut all = true;
        for n in &nodes {
            let alive =
                n.handle.nodes().await.iter().filter(|m| m.state == NodeState::Alive).count();
            if alive != 3 {
                all = false;
            }
        }
        if all {
            break;
        }
        assert!(Instant::now() < deadline, "membership did not converge");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Some(nodes)
}

/// Poll until `key` reads as `expected` through `Store::get_through` on every
/// node (`None` = absent everywhere).
async fn wait_everywhere(nodes: &[Node], key: &str, expected: Option<&[u8]>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let mut mismatch = Vec::new();
        for n in nodes {
            let got = n.local.get_through(key);
            if got.as_deref() != expected {
                mismatch.push(format!("{}={:?}", n.name, got.as_deref().map(<[u8]>::len)));
            }
        }
        if mismatch.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{key}: expected {:?} everywhere, still differs on {mismatch:?}",
            expected.map(<[u8]>::len)
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

async fn shutdown_all(nodes: [Node; 3]) {
    for n in nodes {
        n.shutdown().await;
    }
}

/// F-8: a large value is readable through the normal read path on every
/// node, including the one outside its replica set.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_value_is_readable_on_every_node() {
    let Some(nodes) = three_nodes().await else { return };
    for i in 0..6 {
        let key = format!("big:{i}");
        let value = large(&key);
        assert!(nodes[i % 3].local.set(key.clone(), value.clone(), None));
        // Read-your-write on the origin, whatever its replica role.
        assert_eq!(nodes[i % 3].local.get(&key).as_deref(), Some(value.as_slice()));
        wait_everywhere(&nodes, &key, Some(&value), Duration::from_secs(10)).await;
    }
    // At least one of the six keys has a non-replica node that served it by
    // fetching through (RF 2 of 3 nodes): that node's local-only read misses.
    let mut fetched_through = 0;
    for i in 0..6 {
        let key = format!("big:{i}");
        for n in &nodes {
            if n.local.get(&key).is_none() {
                fetched_through += 1;
                assert!(n.local.get_through(&key).is_some());
            }
        }
    }
    assert!(fetched_through > 0, "with RF=2 of 3 some node must be outside a replica set");
    shutdown_all(nodes).await;
}

/// F-9: a large value's TTL travels with it, so replica copies expire.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_value_ttl_expires_on_replicas() {
    let Some(nodes) = three_nodes().await else { return };
    let value = large("ttl");
    assert!(nodes[0].local.set("big:ttl".into(), value.clone(), Some(Duration::from_secs(2))));
    wait_everywhere(&nodes, "big:ttl", Some(&value), Duration::from_secs(10)).await;
    for n in &nodes {
        if let Some(pttl) = n.local.pttl("big:ttl") {
            assert!(pttl <= 2_000, "{}: replica copy must carry the TTL, pttl={pttl}", n.name);
        }
    }
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    wait_everywhere(&nodes, "big:ttl", None, Duration::from_secs(5)).await;
    shutdown_all(nodes).await;
}

/// F-10: overwriting a small value with a large one drops the small copy on
/// nodes outside the new replica set instead of serving it forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn small_then_large_overwrite_drops_the_stale_small_copy() {
    let Some(nodes) = three_nodes().await else { return };
    for i in 0..4 {
        let key = format!("sl:{i}");
        assert!(nodes[0].local.set(key.clone(), b"small-v1".to_vec(), None));
        wait_everywhere(&nodes, &key, Some(b"small-v1"), Duration::from_secs(10)).await;
        let value = large(&key);
        assert!(nodes[0].local.set(key.clone(), value.clone(), None));
        wait_everywhere(&nodes, &key, Some(&value), Duration::from_secs(10)).await;
        // And no node still holds the small value locally.
        for n in &nodes {
            assert_ne!(n.local.get(&key).as_deref(), Some(b"small-v1".as_slice()), "{}", n.name);
        }
    }
    shutdown_all(nodes).await;
}

/// F-11: `EXPIRE` on a large value reaches its replicas.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expire_on_a_large_value_reaches_replicas() {
    let Some(nodes) = three_nodes().await else { return };
    let value = large("expire");
    assert!(nodes[0].local.set("big:expire".into(), value.clone(), None));
    wait_everywhere(&nodes, "big:expire", Some(&value), Duration::from_secs(10)).await;
    assert!(nodes[0].local.expire("big:expire", Duration::from_secs(2)));
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    wait_everywhere(&nodes, "big:expire", None, Duration::from_secs(5)).await;
    shutdown_all(nodes).await;
}

/// A delete of a large value from a node that does not own it still removes
/// it everywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_of_a_large_value_reaches_everyone() {
    let Some(nodes) = three_nodes().await else { return };
    let value = large("del");
    assert!(nodes[0].local.set("big:del".into(), value.clone(), None));
    wait_everywhere(&nodes, "big:del", Some(&value), Duration::from_secs(10)).await;
    nodes[1].local.remove("big:del");
    wait_everywhere(&nodes, "big:del", None, Duration::from_secs(10)).await;
    // The clustered reader agrees.
    for n in &nodes {
        assert!(n.clustered.get("big:del").await.is_none(), "{}", n.name);
    }
    shutdown_all(nodes).await;
}

/// APPEND on a node with no copy of a large key must not create the key from
/// the appended bytes alone — that write would replicate and replace the
/// real value cluster-wide. It is refused instead, and the value survives.
/// On a node that holds a copy, APPEND works and replicates as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn append_to_a_large_value_held_elsewhere_is_refused_not_destructive() {
    use ephpm_kv::store::AppendError;
    let Some(nodes) = three_nodes().await else { return };
    // Find a key with a node outside its replica set holding no copy (RF 2
    // of 3, written from node 0, so some key leaves one node empty-handed).
    let mut found = None;
    for i in 0..12 {
        let key = format!("big:append:{i}");
        let value = large("append");
        assert!(nodes[0].local.set(key.clone(), value.clone(), None));
        wait_everywhere(&nodes, &key, Some(&value), Duration::from_secs(10)).await;
        if let Some(idx) = nodes.iter().position(|n| n.local.get(&key).is_none()) {
            found = Some((key, value, idx));
            break;
        }
    }
    let (key, value, outsider) = found.expect("with RF=2 of 3 some node holds no copy");

    let refused = nodes[outsider].local.try_append(&key, b"-tail");
    assert_eq!(refused, Err(AppendError::HeldElsewhere));
    // Nothing changed anywhere, and the outsider still has no local copy.
    tokio::time::sleep(Duration::from_millis(500)).await;
    wait_everywhere(&nodes, &key, Some(&value), Duration::from_secs(5)).await;
    assert!(nodes[outsider].local.get(&key).is_none());

    // A node that holds the key appends normally, and everyone sees it.
    let holder = nodes.iter().position(|n| n.local.get(&key).is_some()).expect("a holder");
    let new_len = nodes[holder].local.try_append(&key, b"-tail").expect("append on a holder");
    let mut want = value.clone();
    want.extend_from_slice(b"-tail");
    assert_eq!(new_len, want.len());
    wait_everywhere(&nodes, &key, Some(&want), Duration::from_secs(10)).await;

    // A key that exists nowhere is still created by APPEND, from any node.
    assert_eq!(nodes[outsider].local.try_append("big:append:new", b"abc"), Ok(3));
    wait_everywhere(&nodes, "big:append:new", Some(b"abc"), Duration::from_secs(10)).await;
    shutdown_all(nodes).await;
}
