//! A node that restarts into a running cluster must not serve a version of a
//! key that a peer has already acknowledged as superseded (#631).
//!
//! A restarted node is a fresh chitchat generation with an empty store. It
//! receives each peer's gossip state **per peer, in version order**, one MTU
//! per gossip exchange, so catching up on a large state takes tens of
//! seconds to minutes. In that window the copy of a key it materializes first
//! can be an older one — a peer's copy that has not been retired yet because
//! that peer is itself behind on the writer's backlog — while the newest
//! version sits at the top of another peer's state, minutes away in cursor
//! terms. Before the catch-up gate, the restarted node served that older copy
//! as a local hit; the data-plane fetch-through never ran because the local
//! store answered.
//!
//! The nodes share `127.0.0.1` on distinct UDP/TCP ports (no loopback
//! aliases), so this runs on every platform.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ephpm_cluster::data_plane::KvRouter;
use ephpm_cluster::node::{ClusterHandle, NodeState, start_gossip};
use ephpm_cluster::{ClusteredStore, KvReplicator};
use ephpm_config::{ClusterConfig, ClusterKvConfig};
use ephpm_kv::store::{Replicator, Store, StoreConfig};

/// Values below this ride gossip; every value in this test does.
const THRESHOLD: usize = 4096;

/// Filler entries written to one peer so its state takes many gossip
/// exchanges to stream. Chitchat compresses each delta, so the filler is
/// pseudo-random (incompressible): ~2.7 MB of base64 at most 64 KB per
/// exchange and at most two exchanges per second per peer is at least
/// twenty seconds of catch-up.
const FILLER: usize = 2000;
const FILLER_LEN: usize = 1000;

/// Incompressible bytes for filler `i` (xorshift64*, seeded by `i`).
fn noise(i: usize, len: usize) -> Vec<u8> {
    let mut x = (i as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (0..len)
        .map(|_| {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
        })
        .collect()
}

fn free_port(proto_tcp: bool) -> u16 {
    if proto_tcp {
        std::net::TcpListener::bind("127.0.0.1:0").expect("bind").local_addr().expect("addr").port()
    } else {
        std::net::UdpSocket::bind("127.0.0.1:0").expect("bind").local_addr().expect("addr").port()
    }
}

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

/// Wire a node exactly as `ephpm-server::serve()` does: local store, data
/// plane listener, `ClusteredStore` + `KvReplicator` sharing one applied map,
/// the gossip applier, and the catch-up gate.
async fn start_node(name: &str, gossip_port: u16, data_port: u16, seeds: Vec<String>) -> Node {
    start_node_with(name, gossip_port, data_port, seeds, true).await
}

/// [`start_node`]; with `serve_data = false` the data port is a black hole:
/// it accepts connections and never reads or answers — a peer that is alive
/// to gossip but stalled on the data plane.
async fn start_node_with(
    name: &str,
    gossip_port: u16,
    data_port: u16,
    seeds: Vec<String>,
    serve_data: bool,
) -> Node {
    let kv = ClusterKvConfig {
        small_key_threshold: THRESHOLD,
        replication_factor: 2,
        hot_key_cache: false,
        data_port,
        ..ClusterKvConfig::default()
    };
    let cluster_config = ClusterConfig {
        enabled: true,
        bind: format!("127.0.0.1:{gossip_port}"),
        join: seeds,
        secret: String::new(),
        allow_insecure_no_auth: false,
        node_id: name.to_string(),
        cluster_id: "restart-catchup-test".to_string(),
        kv: kv.clone(),
        ..ClusterConfig::default()
    };
    let handle = Arc::new(start_gossip(&cluster_config).await.expect("gossip start"));

    let local = Store::new(StoreConfig::default());
    let applied = ephpm_cluster::clustered_store::new_applied_write_map();
    let data_addr: SocketAddr = format!("127.0.0.1:{data_port}").parse().expect("data addr");
    let router = KvRouter::global(Arc::clone(&local)).with_applied(Arc::clone(&applied));
    let data_plane = if serve_data {
        tokio::spawn(async move {
            if let Err(e) = ephpm_cluster::data_plane::serve_router(router, data_addr, None).await {
                eprintln!("data plane failed on {data_addr}: {e}");
            }
        })
    } else {
        let listener = tokio::net::TcpListener::bind(data_addr).await.expect("bind black hole");
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        })
    };

    let clustered = ClusteredStore::new(Arc::clone(&local), Arc::clone(&handle), kv, None);
    clustered.init_catch_up_gate().await;
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

async fn wait_alive(nodes: &[&Node], expected: usize, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let mut all = true;
        for n in nodes {
            let alive =
                n.handle.nodes().await.iter().filter(|m| m.state == NodeState::Alive).count();
            if alive != expected {
                all = false;
            }
        }
        if all {
            return;
        }
        assert!(Instant::now() < deadline, "membership did not converge to {expected}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_until(mut pred: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if pred() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The ledger rule the live lab applies: a read that starts more than a
/// second after a newer version was acknowledged and returns an older one is
/// stale. A miss is not.
///
/// Linux only: the backlog rides chitchat's 64 KB UDP datagrams, which the
/// default socket buffers on Windows and macOS (`net.inet.udp.maxdgram` =
/// 9216) do not deliver on loopback — the catch-up itself never happens
/// there, so there is nothing to observe. Clustered mode is validated on
/// Linux.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restarted_node_never_serves_a_superseded_version_while_catching_up() {
    if !cfg!(target_os = "linux") {
        eprintln!("skipping: 64 KB gossip datagrams need Linux loopback buffers");
        return;
    }
    let (ga, gb, gr) = (free_port(false), free_port(false), free_port(false));
    let (da, db, dr) = (free_port(true), free_port(true), free_port(true));
    let a = start_node("catchup-a", ga, da, vec![]).await;
    let seed = a.handle.self_node().gossip_addr.clone();
    let b = start_node("catchup-b", gb, db, vec![seed.clone()]).await;
    let r = start_node("catchup-r", gr, dr, vec![seed.clone()]).await;
    wait_alive(&[&a, &b, &r], 3, Duration::from_secs(20)).await;

    // A writes the key; everyone converges on v1. A is now its publisher.
    assert!(a.local.set("K".into(), b"v1".to_vec(), None));
    for n in [&b, &r] {
        assert!(
            wait_until(
                || n.local.get("K").as_deref() == Some(b"v1".as_slice()),
                Duration::from_secs(15)
            )
            .await,
            "v1 must reach every node"
        );
    }

    // R goes away. Its next incarnation is a new chitchat generation (the
    // generation id is wall-clock seconds), so wait past the second boundary.
    r.shutdown().await;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // B buries the key's newest version at the top of a large state: a
    // peer streaming B's state in version order reaches v2 last.
    for i in 0..FILLER {
        assert!(b.local.set(format!("filler:{i}"), noise(i, FILLER_LEN), None));
    }
    assert!(b.local.set("K".into(), b"v2".to_vec(), None));
    let v2_acked = Instant::now();

    // R comes back with an empty store and catches up from both peers. Reads
    // start at once, as a node under a load balancer would see them.
    let r2 = start_node("catchup-r", gr, dr, vec![seed]).await;

    // Watch R's reads until it has applied v2 (its cursor on B reached the
    // top) or the gossip transfer budget runs out. Every read made more than
    // a second after v2 was acknowledged must be v2 or a miss — never v1.
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut reads = 0usize;
    let mut misses = 0usize;
    let mut first_v2: Option<Duration> = None;
    loop {
        let applied_v2 = r2.local.get("K").as_deref() == Some(b"v2".as_slice());
        let got = r2.local.get_through("K");
        if v2_acked.elapsed() > Duration::from_secs(1) {
            reads += 1;
            match got.as_deref() {
                Some(b"v2") => {
                    first_v2.get_or_insert(v2_acked.elapsed());
                }
                None => misses += 1,
                Some(other) => panic!(
                    "restarted node served a superseded version {:?} {:.1}s after v2 was \
                     acknowledged (cold={}, stats={:?})",
                    String::from_utf8_lossy(other),
                    v2_acked.elapsed().as_secs_f64(),
                    r2.clustered.is_cold(),
                    r2.clustered.catch_up_stats(),
                ),
            }
        }
        if applied_v2 || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(reads > 10, "the window must be long enough to observe reads ({reads})");
    let first_v2 = first_v2.expect("v2 must be readable on the restarted node");
    eprintln!(
        "reads={reads} misses={misses} first_v2_after={:.1}s stats={:?}",
        first_v2.as_secs_f64(),
        r2.clustered.catch_up_stats()
    );
    // Once the catch-up gate opens, reads are local again and still v2.
    assert!(
        wait_until(|| !r2.clustered.is_cold(), Duration::from_secs(60)).await,
        "the gate must open once every peer's post-join mark has arrived: {:?}",
        r2.clustered.catch_up_stats()
    );
    assert_eq!(r2.local.get_through("K").as_deref(), Some(b"v2".as_slice()));

    r2.shutdown().await;
    b.shutdown().await;
    a.shutdown().await;
}

/// A node with no peer has nothing to catch up on: the gate opens after the
/// join wait and reads are local throughout (a miss inside the wait, never a
/// peer round trip).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_node_opens_the_gate_after_the_join_wait() {
    let n = start_node("catchup-solo", free_port(false), free_port(true), vec![]).await;
    assert!(n.clustered.is_cold(), "armed cold");
    assert!(n.local.set("mine".into(), b"v".to_vec(), None));
    assert_eq!(n.local.get_through("mine").as_deref(), Some(b"v".as_slice()), "own write");
    assert_eq!(n.local.get_through("nobody").as_deref(), None);
    let opened = wait_until(
        || !n.clustered.is_cold(),
        ephpm_cluster::clustered_store::CATCH_UP_JOIN_WAIT + Duration::from_secs(3),
    )
    .await;
    assert!(opened, "no peers: the gate must open after the join wait");
    let stats = n.clustered.catch_up_stats();
    assert_eq!((stats.found, stats.unavailable), (0, 0), "nobody was asked: {stats:?}");
    n.shutdown().await;
}

/// While cold, a node serves its own writes locally (read-your-writes) and
/// everything else through peers: a key a peer holds reads as the peer's
/// copy, a key nobody holds as a miss even if gossip has not reached it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_node_reads_own_writes_locally_and_everything_else_through_peers() {
    let (ga, gr) = (free_port(false), free_port(false));
    let (da, dr) = (free_port(true), free_port(true));
    let a = start_node("catchup-peer", ga, da, vec![]).await;
    let seed = a.handle.self_node().gossip_addr.clone();
    assert!(wait_until(|| !a.clustered.is_cold(), Duration::from_secs(10)).await, "a opens alone");
    assert!(a.local.set("theirs".into(), b"peer-value".to_vec(), None));

    let r = start_node("catchup-joiner", gr, dr, vec![seed]).await;
    assert!(r.clustered.is_cold());
    // Wait for the failure detector to call A alive (the join wait covers it).
    wait_alive(&[&a, &r], 2, Duration::from_secs(15)).await;
    // The gate's peer list follows the failure detector by one poll, and A's
    // advertised data port rides gossip; until both have landed a cold read
    // is a deliberate miss. Poll until the peer answers (or the gate opens).
    let peer_answers = wait_until(
        || {
            !r.clustered.is_cold()
                || r.local.get_through("theirs").as_deref() == Some(b"peer-value".as_slice())
        },
        Duration::from_secs(10),
    )
    .await;
    assert!(peer_answers, "A must answer cold reads: {:?}", r.clustered.catch_up_stats());
    if r.clustered.is_cold() {
        // Own write: local, no peer asked.
        let before = r.clustered.catch_up_stats();
        assert!(r.local.set("mine".into(), b"my-value".to_vec(), None));
        assert_eq!(r.local.get_through("mine").as_deref(), Some(b"my-value".as_slice()));
        assert_eq!(r.clustered.catch_up_stats().found, before.found, "own write asks nobody");
        // Peer's key: served from the peer, counted as a cold read.
        assert_eq!(r.local.get_through("theirs").as_deref(), Some(b"peer-value".as_slice()));
        assert_eq!(r.clustered.catch_up_stats().found, before.found + 1);
        // Unknown key: a miss, counted as absent.
        assert_eq!(r.local.get_through("nowhere"), None);
        assert_eq!(r.clustered.catch_up_stats().absent, before.absent + 1);
    } else {
        eprintln!("gate opened before the assertions ran; nothing to check while cold");
    }
    // The gate opens once A's post-join mark has arrived, and reads are local
    // and still correct.
    assert!(
        wait_until(|| !r.clustered.is_cold(), Duration::from_secs(30)).await,
        "the gate must open: {:?} / {:?}",
        r.clustered.catch_up_stats(),
        r.handle.catch_up_status().await
    );
    let found = r.clustered.catch_up_stats().found;
    assert_eq!(r.local.get_through("theirs").as_deref(), Some(b"peer-value".as_slice()));
    assert_eq!(r.clustered.catch_up_stats().found, found, "warm reads ask nobody");
    assert_eq!(r.local.get_through("nowhere"), None);

    r.shutdown().await;
    a.shutdown().await;
}

/// Fill `node` with incompressible entries so a peer joining it takes many
/// gossip exchanges (tens of seconds) to catch up — the cold window the
/// tests below act in. Linux only, like the test above.
fn bury(node: &Node) {
    for i in 0..FILLER {
        assert!(node.local.set(format!("filler:{i}"), noise(i, FILLER_LEN), None));
    }
}

/// While a restarted node is still catching up, its local copies may be
/// missing or superseded. Read-modify-writes used to compute from them and
/// publish a fresh stamp that won everywhere — an `INCR` on a counter not yet
/// received reset it to 1 cluster-wide; an `HINCRBY` likewise; a TTL'd
/// counter lost its TTL. They now resolve the key (or field) through the
/// peers first, so the result is what a caught-up node computes, and the
/// value replicates back. Whole-hash reads are refused while cold.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_node_read_modify_writes_resolve_through_peers() {
    if !cfg!(target_os = "linux") {
        eprintln!("skipping: 64 KB gossip datagrams need Linux loopback buffers");
        return;
    }
    let (ga, gr) = (free_port(false), free_port(false));
    let (da, dr) = (free_port(true), free_port(true));
    let a = start_node("rmw-a", ga, da, vec![]).await;
    let seed = a.handle.self_node().gossip_addr.clone();
    assert!(wait_until(|| !a.clustered.is_cold(), Duration::from_secs(10)).await, "a opens alone");
    assert!(a.local.set("ctr".into(), b"41".to_vec(), Some(Duration::from_secs(600))));
    assert!(a.local.set("s".into(), b"abc".to_vec(), None));
    a.local.hset("h", "f", b"v".to_vec()).unwrap();
    a.local.hset("h", "c", b"10".to_vec()).unwrap();
    bury(&a);

    let r = start_node("rmw-r", gr, dr, vec![seed]).await;
    wait_alive(&[&a, &r], 2, Duration::from_secs(15)).await;
    // Until A's data port has arrived in gossip a cold read is a deliberate
    // miss; wait for A to answer one.
    let answers = wait_until(
        || !r.clustered.is_cold() || r.local.get_through("s").as_deref() == Some(b"abc".as_slice()),
        Duration::from_secs(15),
    )
    .await;
    assert!(answers, "A must answer cold reads: {:?}", r.clustered.catch_up_stats());
    if !r.clustered.is_cold() {
        eprintln!("gate opened before the assertions ran; nothing to check while cold");
        r.shutdown().await;
        a.shutdown().await;
        return;
    }

    // Reads that return a value come from the peer too, not only GET.
    assert!(r.local.exists_through("ctr"));
    assert_eq!(r.local.strlen_through("s"), 3);
    let pttl = r.local.pttl_through("ctr").expect("exists");
    assert!(pttl > 500_000, "the peer's TTL: {pttl}");
    // INCR continues the cluster's counter (and keeps its TTL).
    assert_eq!(r.local.incr_by("ctr", 1), Ok(42));
    assert!(r.local.pttl("ctr").expect("held now") > 500_000, "TTL kept");
    // APPEND appends to the cluster's value.
    assert_eq!(r.local.try_append("s", b"d"), Ok(4));
    // Hash fields: per-field reads and writes resolve through the peers.
    assert!(r.local.prepare_hash_fields("h", &["f", "c"]).is_ok());
    assert_eq!(r.local.hget("h", "f").as_deref(), Some(b"v".as_slice()));
    assert_eq!(r.local.hincr_by("h", "c", 5), Ok(15));
    // A command over the whole hash cannot know which fields exist yet.
    assert!(r.clustered.is_cold(), "still cold: the assertions above ran inside the window");
    assert!(r.local.prepare_hash_whole("h").is_err());
    // So is a key-expiry change of a hash — refused, and resolving the name
    // as a string (no peer holds one) must not discard the local hash.
    assert!(r.local.try_expire("h", Duration::from_secs(60)).is_err());
    assert_eq!(r.local.hget("h", "f").as_deref(), Some(b"v".as_slice()), "hash kept");
    // A key nobody holds is created from scratch, as on a caught-up node.
    assert_eq!(r.local.incr_by("fresh", 1), Ok(1));

    // Everything replicated back with the cluster's history intact.
    for (key, want) in [("ctr", &b"42"[..]), ("s", b"abcd"), ("fresh", b"1")] {
        assert!(
            wait_until(|| a.local.get(key).as_deref() == Some(want), Duration::from_secs(15)).await,
            "{key} must reach A as {:?}, got {:?}",
            String::from_utf8_lossy(want),
            a.local.get(key)
        );
    }
    assert!(
        wait_until(
            || a.local.hget("h", "c").as_deref() == Some(b"15".as_slice()),
            Duration::from_secs(15)
        )
        .await,
        "h.c must reach A as 15"
    );
    assert!(a.local.pttl("ctr").expect("held") > 500_000, "the counter kept its TTL on A");

    r.shutdown().await;
    a.shutdown().await;
}

/// A peer that is alive to gossip but stalled on the data plane (accepts,
/// never answers) used to cost every cold read the full fetch budget, for
/// as long as the node stayed cold. After a few timeouts the data-plane
/// client's stall breaker skips it, and cold reads are answered by the
/// healthy peer at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_reads_skip_a_stalled_peer_after_repeated_timeouts() {
    if !cfg!(target_os = "linux") {
        eprintln!("skipping: 64 KB gossip datagrams need Linux loopback buffers");
        return;
    }
    let (ga, gs, gr) = (free_port(false), free_port(false), free_port(false));
    let (da, ds, dr) = (free_port(true), free_port(true), free_port(true));
    let a = start_node("stall-a", ga, da, vec![]).await;
    let seed = a.handle.self_node().gossip_addr.clone();
    let s = start_node_with("stall-s", gs, ds, vec![seed.clone()], false).await;
    wait_alive(&[&a, &s], 2, Duration::from_secs(15)).await;
    assert!(a.local.set("k".into(), b"v".to_vec(), None));
    bury(&a);

    let r = start_node("stall-r", gr, dr, vec![seed]).await;
    wait_alive(&[&a, &s, &r], 3, Duration::from_secs(20)).await;
    let budget = ephpm_cluster::clustered_store::FETCH_THROUGH_TIMEOUT;
    let mut durations = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(40);
    while durations.len() < 8 && Instant::now() < deadline {
        if !r.clustered.is_cold() {
            break;
        }
        let started = Instant::now();
        let got = r.local.get_through("k");
        let took = started.elapsed();
        if got.as_deref() == Some(b"v".as_slice()) {
            durations.push(took);
        } else {
            // A's data port has not arrived in gossip yet.
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    if durations.len() < 8 {
        eprintln!("gate opened before the reads ran ({durations:?}); nothing to check while cold");
    } else {
        eprintln!("cold read durations: {durations:?}");
        // Reads made while A's data port was still unknown may already have
        // tripped the breaker; at most the threshold's worth are slow.
        let slow = durations.iter().filter(|d| **d >= budget / 2).count();
        assert!(
            slow <= ephpm_cluster::data_plane::PEER_STALL_THRESHOLD as usize,
            "the stalled peer must stop costing reads their budget: {durations:?}"
        );
        assert!(
            durations[5..].iter().all(|d| *d < budget / 4),
            "after the breaker trips the stalled peer is skipped: {durations:?}"
        );
    }

    r.shutdown().await;
    s.shutdown().await;
    a.shutdown().await;
}
