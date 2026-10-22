//! Hashes across a 3-node cluster: per-field replication, field TTLs, whole-key
//! TTL and delete, and owner-routed compare-and-set (`SET ... IFEQ`).
//!
//! Before v0.13 hash writes were node-local in clustered mode (one WARN per
//! process said so). Every field is now its own replicated gossip item, so
//! concurrent writes to different fields of one hash on different nodes all
//! survive everywhere.
//!
//! Wired exactly as `ephpm-server::serve()` wires a node: a local `Store`
//! hooked through `KvReplicator`, the gossip applier, and a data-plane listener
//! sharing one applied-stamp map. Uses the `127.0.0.x` loopback-alias harness
//! (Linux); the tests skip with a notice where aliases are unavailable.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ephpm_cluster::data_plane::KvRouter;
use ephpm_cluster::node::{ClusterHandle, NodeState, start_gossip};
use ephpm_cluster::{ClusteredStore, KvReplicator};
use ephpm_config::{ClusterConfig, ClusterKvConfig};
use ephpm_kv::store::{
    ExpireCondition, HashWriteError, Replicator, SetIfEqOutcome, SetTtl, Store, StoreConfig,
};

fn free_data_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").expect("bind").local_addr().expect("addr").port()
}

fn loopback_aliases_available() -> bool {
    std::net::TcpListener::bind("127.0.0.2:0").is_ok()
}

/// Gossip payload budget; hash fields above `THRESHOLD - 17` are refused.
const THRESHOLD: usize = 128;

struct Node {
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

async fn start_node(ip: &str, data_port: u16, seeds: Vec<String>, name: &str) -> Node {
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
        cluster_id: "hash-replication-test".to_string(),
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
    Node { handle, local, clustered, data_plane }
}

async fn three_nodes(prefix: &str) -> Option<[Node; 3]> {
    if !loopback_aliases_available() {
        eprintln!("skipping: platform lacks 127.0.0.x loopback aliases (Linux-only harness)");
        return None;
    }
    let port = free_data_port();
    let n1 = start_node("127.0.0.1", port, vec![], &format!("{prefix}-a")).await;
    let seed = n1.handle.self_node().gossip_addr.clone();
    let n2 = start_node("127.0.0.2", port, vec![seed.clone()], &format!("{prefix}-b")).await;
    let n3 = start_node("127.0.0.3", port, vec![seed], &format!("{prefix}-c")).await;
    let nodes = [n1, n2, n3];
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let mut all = true;
        for n in &nodes {
            let alive =
                n.handle.nodes().await.iter().filter(|m| m.state == NodeState::Alive).count();
            all &= alive == 3;
        }
        if all {
            return Some(nodes);
        }
        assert!(Instant::now() < deadline, "membership did not converge");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn shutdown_all(nodes: [Node; 3]) {
    for n in nodes {
        n.shutdown().await;
    }
}

/// Poll until `check` holds on every node.
async fn wait_all(nodes: &[Node], what: &str, timeout: Duration, check: impl Fn(&Store) -> bool) {
    let deadline = Instant::now() + timeout;
    loop {
        if nodes.iter().all(|n| check(&n.local)) {
            return;
        }
        assert!(Instant::now() < deadline, "{what}: did not converge on every node");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn sorted_fields(s: &Store, key: &str) -> Vec<(String, Vec<u8>)> {
    let mut pairs: Vec<(String, Vec<u8>)> =
        s.hgetall(key).into_iter().map(|(f, v)| (f, v.to_vec())).collect();
    pairs.sort();
    pairs
}

const CONVERGE: Duration = Duration::from_secs(15);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writes_to_different_fields_all_survive() {
    let Some(nodes) = three_nodes("hf").await else { return };
    // Three nodes write three different fields of one hash at once. Under
    // whole-hash replication the last arrival would erase the others.
    std::thread::scope(|scope| {
        for (i, n) in nodes.iter().enumerate() {
            let store = Arc::clone(&n.local);
            scope.spawn(move || {
                store.hset("profile", &format!("f{i}"), format!("v{i}").into_bytes()).unwrap();
            });
        }
    });
    let want = vec![
        ("f0".to_string(), b"v0".to_vec()),
        ("f1".to_string(), b"v1".to_vec()),
        ("f2".to_string(), b"v2".to_vec()),
    ];
    wait_all(&nodes, "three fields everywhere", CONVERGE, |s| sorted_fields(s, "profile") == want)
        .await;

    // A field overwrite and an HINCRBY replicate as the resulting value.
    nodes[1].local.hset("profile", "f0", b"changed".to_vec()).unwrap();
    nodes[2].local.hincr_by("profile", "visits", 5).unwrap();
    wait_all(&nodes, "overwrite + incr", CONVERGE, |s| {
        s.hget("profile", "f0").as_deref() == Some(&b"changed"[..])
            && s.hget("profile", "visits").as_deref() == Some(&b"5"[..])
    })
    .await;
    shutdown_all(nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hdel_and_whole_hash_delete_reach_everyone_and_stay_deleted() {
    let Some(nodes) = three_nodes("hd").await else { return };
    nodes[0].local.hset_many("h", &[("a", b"1"), ("b", b"2"), ("c", b"3")]).unwrap();
    wait_all(&nodes, "seed", CONVERGE, |s| s.hlen("h") == 3).await;

    assert_eq!(nodes[1].local.hdel_many("h", &["a"]), 1);
    wait_all(&nodes, "hdel", CONVERGE, |s| s.hlen("h") == 2 && !s.hexists("h", "a")).await;

    assert!(nodes[2].local.remove("h"));
    wait_all(&nodes, "del", CONVERGE, |s| !s.exists("h")).await;
    // The per-field tombstones keep the gossip state from re-delivering the
    // fields (the verify pass runs every second): still gone a while later.
    tokio::time::sleep(Duration::from_secs(3)).await;
    for n in &nodes {
        assert!(!n.local.exists("h"), "a deleted hash came back");
    }
    shutdown_all(nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn field_and_key_expiry_replicate() {
    let Some(nodes) = three_nodes("ht").await else { return };
    nodes[0].local.hset_many("sess", &[("short", b"x"), ("long", b"y")]).unwrap();
    wait_all(&nodes, "seed", CONVERGE, |s| s.hlen("sess") == 2).await;

    let codes = nodes[1].local.hexpire(
        "sess",
        &["short"],
        Duration::from_millis(1_500),
        ExpireCondition::Always,
    );
    assert_eq!(codes, [1]);
    wait_all(&nodes, "field TTL visible", CONVERGE, |s| s.hpttl("sess", &["short"])[0] > 0).await;
    wait_all(&nodes, "field expired", CONVERGE, |s| {
        !s.hexists("sess", "short") && s.hexists("sess", "long")
    })
    .await;

    // Whole-key EXPIRE rides every field's state.
    assert!(nodes[2].local.expire("sess", Duration::from_secs(60)));
    wait_all(&nodes, "key TTL visible", CONVERGE, |s| s.pttl("sess").is_some_and(|ms| ms > 0))
        .await;
    assert!(nodes[0].local.persist("sess"));
    wait_all(&nodes, "key TTL cleared", CONVERGE, |s| s.pttl("sess") == Some(-1)).await;
    shutdown_all(nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_field_is_refused_not_stored_locally() {
    let Some(nodes) = three_nodes("hl").await else { return };
    let limit = THRESHOLD - ephpm_cluster::hash_field::HEADER_LEN;
    let err = nodes[0].local.hset("big", "f", vec![b'x'; limit + 1]).unwrap_err();
    assert_eq!(err, HashWriteError::TooLargeForCluster { len: limit + 1, max: limit });
    assert!(!nodes[0].local.exists("big"), "nothing may be stored node-locally");
    nodes[0].local.hset("big", "f", vec![b'x'; limit]).unwrap();
    wait_all(&nodes, "a field at the limit replicates", CONVERGE, |s| {
        s.hstrlen("big", "f") == limit
    })
    .await;
    shutdown_all(nodes).await;
}

/// `SET ... IFEQ` is decided by the key's owner, like `SETNX`/`DELIFEQ`: a
/// caller on any node extends the lock it holds and cannot extend one whose
/// token has changed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_if_eq_is_owner_arbitrated() {
    let Some(nodes) = three_nodes("ce").await else { return };
    assert!(nodes[0].local.set_nx("lock".into(), b"tok".to_vec(), Some(Duration::from_secs(30))));
    wait_all(&nodes, "lock visible", CONVERGE, |s| s.get("lock").as_deref() == Some(&b"tok"[..]))
        .await;
    for (i, n) in nodes.iter().enumerate() {
        let ttl = SetTtl::After(Duration::from_secs(60 + i as u64));
        assert_eq!(
            n.local.set_if_eq("lock".into(), b"tok", b"tok".to_vec(), ttl),
            SetIfEqOutcome::Set,
            "node {i} extends its own lock"
        );
        assert_eq!(
            n.local.set_if_eq("lock".into(), b"stolen", b"x".to_vec(), SetTtl::Keep),
            SetIfEqOutcome::NotSet,
            "node {i} cannot overwrite a lock with a stale token"
        );
    }
    assert_eq!(
        nodes[1].local.set_if_eq("lock".into(), b"tok", b"next".to_vec(), SetTtl::Keep),
        SetIfEqOutcome::Set
    );
    wait_all(&nodes, "swap replicated", CONVERGE, |s| {
        s.get("lock").as_deref() == Some(&b"next"[..])
    })
    .await;
    shutdown_all(nodes).await;
}
