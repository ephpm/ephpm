//! Three-node, in-process test of a per-site owner **leaving the cluster
//! gracefully under forwarded load** (v0.13.0 release-candidate stress finding
//! R1).
//!
//! # The failure this pins
//!
//! A node given SIGTERM kept gossiping, and so stayed the HRW owner of its
//! sites, for its whole shutdown: the HTTP drain, then the final WAL
//! checkpoint (128 s under load). But `serve()` closes the cluster channel
//! *before* that checkpoint, so for two minutes every peer forwarded the
//! node's sites to a closed channel. The RC run counted 42,676 HTTP 500s. And
//! chitchat has no leave message, so even after the process exited the peers
//! kept the stale owner for another 24–40 s.
//!
//! The fix announces the leave in gossip right after the HTTP drain
//! (`ephpm_server::drain`). HRW then skips the node on every node as the
//! announcement arrives, the node refuses forwarded statements so their clients
//! re-resolve, and it keeps its channel open long enough for the new owners to
//! pull snapshots before it closes.
//!
//! # The scenario
//!
//! The node ids are fixed, so a site ranked A > B > C is computable up front: A
//! owns it and B takes it over. B and C both write to it continuously through
//! the bridge resolver (B and C forward to A). Then A runs the shutdown
//! sequence: announce the leave, wait for the handover, close the cluster
//! channel. The test asserts:
//!
//! 1. Ownership moves to B promptly, as both B and C see it.
//! 2. Errors stay inside a bounded window after the announcement. Once A has
//!    closed its channel no write fails at all: nothing forwards to A any more.
//!    Before the fix every write forwarded to A failed from the channel close
//!    until A exited.
//! 3. No acknowledged write is lost: every row a writer was told succeeded is in
//!    the site's database, read through the new owner.
//!
//! Node addressing follows `per_site_handoff_any_peer.rs`: one port across three
//! loopback IPs. B, the node that takes the site over, is the one on
//! `127.0.0.1`. A node serves a snapshot of a site it does not own only to the
//! site's current owner, checked by the connecting peer's IP, and an in-process
//! dial between loopback addresses leaves from `127.0.0.1`. So only a new owner
//! on that address can pull from A here. In a real cluster each node dials from
//! its own address.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ephpm_cluster::{
    ChannelFeatureFlags, ChannelHandle, ClusterHandle, NodeInfo, NodeState, hrw_owner,
    maybe_start_cluster_channel, start_gossip,
};
use ephpm_config::{ClusterChannelConfig, ClusterConfig, SqliteConfig};
use ephpm_php::db_bridge::SiteBackendResolver;
use ephpm_server::site_backends::{SiteBackends, SiteOpenHook};
use ephpm_server::site_wire_auth::SiteWireRoute;
use ephpm_server::sql_forward::ClusteredSiteResolver;
use litewire::backend::{SharedBackend, Value};

const SECRET: &str = "per-site-graceful-leave-test-secret";
/// Index order is bring-up order: B (the heir) is node 0 so that it gets
/// `127.0.0.1`, see the module docs.
const IDS: [&str; 3] = ["ephpm-b", "ephpm-a", "ephpm-c"];
const IPS: [&str; 3] = ["127.0.0.1", "127.0.0.2", "127.0.0.3"];

/// Gossip convergence is normally 1-3 s.
const CONVERGE_TIMEOUT: Duration = Duration::from_secs(45);
/// The first owner settles a brand-new site after the 15 s post-start grace.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(90);
/// How long after the announcement both peers must see B as the owner.
const OWNERSHIP_MOVE_BOUND: Duration = Duration::from_secs(10);
/// How long after the announcement writes may fail: a gossip round for the
/// peers to see it, plus B's serving gate re-checking ownership. Generous for
/// a loaded CI host; before the fix the failures lasted the whole shutdown.
const ERROR_WINDOW: Duration = Duration::from_secs(15);
/// Writes keep going this long after A closes its channel.
const AFTER_CLOSE: Duration = Duration::from_secs(8);

struct Node {
    _dir: tempfile::TempDir,
    cluster: Arc<ClusterHandle>,
    resolver: Arc<dyn SiteBackendResolver>,
    channel: ChannelHandle,
    _handles: Vec<tokio::task::JoinHandle<()>>,
    _primary_view: Arc<AtomicBool>,
}

fn loopbacks_bindable() -> bool {
    IPS.iter().all(|ip| {
        std::net::TcpListener::bind((*ip, 0)).is_ok() && std::net::UdpSocket::bind((*ip, 0)).is_ok()
    })
}

/// A gossip port free (UDP) on all three IPs with `port + 2` free (TCP) too.
fn pick_cluster_port() -> u16 {
    for _ in 0..128 {
        let Ok(probe) = std::net::UdpSocket::bind("127.0.0.1:0") else { continue };
        let Ok(addr) = probe.local_addr() else { continue };
        let port = addr.port();
        drop(probe);
        let Some(channel_port) = port.checked_add(2) else { continue };
        if IPS.iter().all(|ip| {
            std::net::UdpSocket::bind((*ip, port)).is_ok()
                && std::net::TcpListener::bind((*ip, channel_port)).is_ok()
        }) {
            return port;
        }
    }
    panic!("could not find a gossip port free on all loopback IPs with its channel port free too");
}

fn stats() -> ephpm_query_stats::QueryStats {
    ephpm_query_stats::QueryStats::new(ephpm_query_stats::StatsConfig {
        enabled: false,
        slow_query_threshold: Duration::from_secs(1),
        max_digests: 16,
        metric_label_series_max: 16,
    })
}

fn sqlite_config(dir: &Path) -> SqliteConfig {
    SqliteConfig {
        path: "unused-in-per-site-mode.db".into(),
        dir: Some(dir.to_string_lossy().into_owned()),
        max_open_dbs: 64,
        engine: "turso".into(),
        proxy: ephpm_config::SqliteProxyConfig::default(),
        sqld: None,
        replication: ephpm_config::ReplicationConfig {
            role: "auto".into(),
            per_site: true,
            ..ephpm_config::ReplicationConfig::default()
        },
    }
}

/// HRW rank of the three nodes for `site`, best first.
fn rank(site: &str) -> Vec<String> {
    let mut left: Vec<NodeInfo> = IDS
        .iter()
        .zip(IPS)
        .map(|(id, ip)| NodeInfo::new(*id, format!("{ip}:7946"), NodeState::Alive))
        .collect();
    let mut ranked = Vec::new();
    while let Some(id) = hrw_owner(&left, site).map(|n| n.id.clone()) {
        left.retain(|n| n.id != id);
        ranked.push(id);
    }
    ranked
}

/// Bring up one node with production wiring.
async fn bring_up_node(idx: usize, port: u16, dir: tempfile::TempDir) -> Node {
    let join = if idx == 0 { vec![] } else { vec![format!("{}:{port}", IPS[0])] };
    let cluster_cfg = ClusterConfig {
        enabled: true,
        bind: format!("{}:{port}", IPS[idx]),
        join,
        secret: SECRET.to_string(),
        node_id: IDS[idx].to_string(),
        cluster_id: "per-site-graceful-leave".to_string(),
        ..ClusterConfig::default()
    };
    let cluster = Arc::new(start_gossip(&cluster_cfg).await.expect("gossip start"));
    let channel = maybe_start_cluster_channel(
        &ClusterChannelConfig { listen: None, secret: None },
        &cluster_cfg.secret,
        &cluster,
        ChannelFeatureFlags { cdc: true },
    )
    .await
    .expect("cluster channel start")
    .expect("cluster channel bound");

    let sqlite = sqlite_config(dir.path());
    let (site_tx, site_events) = tokio::sync::mpsc::unbounded_channel::<String>();
    let note_active: SiteOpenHook = Arc::new(move |site: &str| {
        let _ = site_tx.send(site.to_string());
    });
    let registry = SiteBackends::new_clustered(
        dir.path().to_path_buf(),
        sqlite.max_open_dbs,
        stats(),
        tokio::runtime::Handle::current(),
        Arc::clone(&note_active),
    )
    .expect("clustered per-site registry");
    let resolver: Arc<dyn SiteBackendResolver> = Arc::new(ClusteredSiteResolver::new(
        registry.clone(),
        Arc::clone(&cluster),
        channel.clone(),
        cluster.self_node().id,
        tokio::runtime::Handle::current(),
        note_active,
    ));

    let mut handles = Vec::new();
    let primary_view = Arc::new(AtomicBool::new(false));
    ephpm_server::turso_cdc::start_clustered_per_site_turso(
        &sqlite,
        dir.path().to_path_buf(),
        Some(&cluster),
        Some(&channel),
        site_events,
        registry,
        &mut handles,
        &primary_view,
    )
    .await
    .expect("per-site clustered replication plane");

    Node { _dir: dir, cluster, resolver, channel, _handles: handles, _primary_view: primary_view }
}

/// Create `site`'s database in `dir` with the `marker` table, the way the node
/// holding the site has it on disk.
async fn seed_site(dir: &Path, site: &str) {
    let registry =
        SiteBackends::new(dir.to_path_buf(), 4, stats(), tokio::runtime::Handle::current())
            .expect("seeding registry");
    let backend = registry.backend_for(site).await.expect("open the seeded site");
    let conn = backend.connect().await.expect("seed session");
    conn.execute("CREATE TABLE marker (id INTEGER PRIMARY KEY, v TEXT NOT NULL)", &[])
        .await
        .expect("create marker table");
}

async fn await_membership(nodes: &[&Node]) {
    let deadline = Instant::now() + CONVERGE_TIMEOUT;
    loop {
        let mut converged = true;
        for n in nodes {
            let alive =
                n.cluster.nodes().await.iter().filter(|m| m.state == NodeState::Alive).count();
            converged &= alive == nodes.len();
        }
        if converged {
            return;
        }
        assert!(Instant::now() < deadline, "membership did not converge to {} nodes", nodes.len());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Resolve `site` as the PHP bridge does (synchronously, on a blocking thread).
async fn resolve(node: &Node, site: &str) -> Result<SharedBackend, String> {
    let resolver = Arc::clone(&node.resolver);
    let key = site.to_string();
    tokio::task::spawn_blocking(move || resolver.resolve(&key)).await.expect("resolve task")
}

/// One bridge-style write: resolve, open a session, insert row `id`.
async fn write_once(node: &Node, site: &str, id: i64) -> Result<(), String> {
    let backend = resolve(node, site).await?;
    let conn = backend.connect().await.map_err(|e| format!("connect: {e}"))?;
    conn.execute("INSERT INTO marker (id, v) VALUES (?, 'w')", &[Value::Integer(id)])
        .await
        .map(drop)
        .map_err(|e| format!("insert: {e}"))
}

/// The outcome of one write: when it finished, and its error if it failed.
struct Outcome {
    at: Instant,
    id: i64,
    error: Option<String>,
}

/// Write rows `first..` to `site` through `node` until `stop` is set.
async fn writer(node: Arc<Node>, site: String, first: i64, stop: Arc<AtomicBool>) -> Vec<Outcome> {
    let mut out = Vec::new();
    let mut id = first;
    while !stop.load(Ordering::SeqCst) {
        let error = write_once(&node, &site, id).await.err();
        out.push(Outcome { at: Instant::now(), id, error });
        id += 1;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn an_owner_leaving_under_load_hands_its_sites_over_without_a_failure_tail() {
    if !loopbacks_bindable() {
        eprintln!(
            "skipping an_owner_leaving_under_load_hands_its_sites_over_without_a_failure_tail: \
             127.0.0.2/127.0.0.3 are not bindable on this host (macOS needs `sudo ifconfig lo0 \
             alias 127.0.0.N`)"
        );
        return;
    }

    let site = (0..8192)
        .map(|i| format!("leave-{i}.test"))
        .find(|s| rank(s) == [IDS[1], IDS[0], IDS[2]])
        .expect("some tenant key must rank A > B > C");

    // A already holds the site, so it serves it without probing its peers.
    // (A probe from A would leave from 127.0.0.1, not A's address, and be
    // refused: see the module docs.)
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    seed_site(dirs[1].path(), &site).await;

    let port = pick_cluster_port();
    let mut dirs = dirs.into_iter();
    // B (the heir) is node 0 on 127.0.0.1: see the module docs.
    let b = Arc::new(bring_up_node(0, port, dirs.next().unwrap()).await);
    let a = Arc::new(bring_up_node(1, port, dirs.next().unwrap()).await);
    let c = Arc::new(bring_up_node(2, port, dirs.next().unwrap()).await);
    await_membership(&[&a, &b, &c]).await;

    // Wait until a write forwarded through C (to the owner A) succeeds.
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    while let Err(e) = write_once(&c, &site, 0).await {
        assert!(Instant::now() < deadline, "site {site} never became writable: {e}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    // ── Forwarded load from B and C ─────────────────────────────────────
    let stop = Arc::new(AtomicBool::new(false));
    let load_b = tokio::spawn(writer(Arc::clone(&b), site.clone(), 1, Arc::clone(&stop)));
    let load_c = tokio::spawn(writer(Arc::clone(&c), site.clone(), 1_000_000, Arc::clone(&stop)));
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ── A shuts down: the order `serve()` uses ──────────────────────────
    let announced = Instant::now();
    let leave = ephpm_server::drain::announce_leave(&a.cluster).await;

    // 1. Ownership moves promptly, on both remaining nodes.
    let deadline = announced + OWNERSHIP_MOVE_BOUND;
    for (name, node) in [("B", &b), ("C", &c)] {
        loop {
            let view = node.cluster.nodes().await;
            if hrw_owner(&view, &site).is_some_and(|n| n.id == IDS[0]) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{name} still does not see B as {site}'s owner {OWNERSHIP_MOVE_BOUND:?} after A \
                 announced its leave"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let moved_after = announced.elapsed();

    leave.await_handover().await;
    a.channel.shutdown();
    let closed = Instant::now();

    tokio::time::sleep(AFTER_CLOSE).await;
    stop.store(true, Ordering::SeqCst);
    let mut outcomes = load_b.await.unwrap();
    outcomes.extend(load_c.await.unwrap());

    // 2. Failures only inside the window, none after A closed its channel.
    let failures: Vec<&Outcome> = outcomes.iter().filter(|o| o.error.is_some()).collect();
    let late: Vec<String> = failures
        .iter()
        .filter(|o| o.at > closed || o.at > announced + ERROR_WINDOW)
        .map(|o| {
            format!(
                "+{:?} id {}: {}",
                o.at.saturating_duration_since(announced),
                o.id,
                o.error.as_deref().unwrap_or_default()
            )
        })
        .collect();
    let before = failures.iter().filter(|o| o.at < announced).count();
    let last_failure = failures.iter().map(|o| o.at.saturating_duration_since(announced)).max();
    eprintln!(
        "graceful leave: {} writes, {} failed ({} before the announcement, last at \
         +{last_failure:?}), ownership moved after {moved_after:?}, channel closed after {:?}",
        outcomes.len(),
        failures.len(),
        before,
        closed.saturating_duration_since(announced),
    );
    assert_eq!(before, 0, "writes failed before A announced anything");
    assert!(
        late.is_empty(),
        "writes failed after A closed its channel or outside the error window:\n{}",
        late.join("\n")
    );
    let after_close = outcomes.iter().filter(|o| o.at > closed && o.error.is_none()).count();
    assert!(after_close > 0, "no write ran after A closed its channel; the test proves nothing");

    // 3. No acknowledged write lost, read through the new owner.
    let acked: Vec<i64> = outcomes.iter().filter(|o| o.error.is_none()).map(|o| o.id).collect();
    let backend = resolve(&c, &site).await.expect("resolve through C after the leave");
    let conn = backend.connect().await.expect("session through C");
    let rs = conn.query("SELECT id FROM marker", &[]).await.expect("read rows");
    let stored: std::collections::HashSet<i64> = rs
        .rows
        .into_iter()
        .filter_map(|mut r| match r.remove(0) {
            Value::Integer(i) => Some(i),
            _ => None,
        })
        .collect();
    let lost: Vec<i64> = acked.iter().copied().filter(|id| !stored.contains(id)).collect();
    assert!(lost.is_empty(), "acknowledged writes lost across the leave: {lost:?}");

    drop((a, b, c));
}
