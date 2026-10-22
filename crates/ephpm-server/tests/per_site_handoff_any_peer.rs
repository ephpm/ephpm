//! Three-node, in-process test of the per-site **ownership handoff** after the
//! ownership hash changes.
//!
//! # The gap this pins
//!
//! A node that becomes a site's HRW owner with no copy of the site's data must
//! pull a copy before it serves. It used to ask exactly one peer: the HRW
//! runner-up over the alive set excluding itself — "the previous owner". That
//! is right when membership changes under a fixed hash. It is wrong after the
//! hash itself changes (v0.13.0's scheme 2, applied with a full-cluster
//! restart): the node the *new* hash ranks second need not hold the site at all,
//! while the node the *old* hash made its owner does. The runner-up answered
//! with an empty dump, the new owner served an empty database, and the tenant's
//! first write forked its data.
//!
//! The fix asks every alive peer in HRW rank order until one answers with data,
//! answers "no copy" from a peer that has none without creating its database,
//! and serves a site empty only when every alive peer has said it holds no copy.
//!
//! # The scenario
//!
//! The three nodes' ids are fixed, so HRW over them is computable before any of
//! them starts. The test picks a site ranked A > B > C — A is the owner, B the
//! runner-up — and seeds its database **only on C**, the stand-in for "the node
//! the old hash made the owner". After the cluster converges:
//!
//! 1. A serves the site with C's data (the runner-up B had nothing).
//! 2. B never grew a database file for the site, though A asked it.
//! 3. A second site nobody holds settles as a new, empty site and accepts
//!    writes — and again neither peer grew a file for it.
//!
//! Node addressing follows `per_site_clustered_bridge_replication.rs`: one port
//! across three loopback IPs, so `gossip_ip : gossip_port + 2` is each node's
//! cluster-channel address from every other node's point of view.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
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

const SECRET: &str = "per-site-handoff-any-peer-test-secret";
const IDS: [&str; 3] = ["ephpm-a", "ephpm-b", "ephpm-c"];
const IPS: [&str; 3] = ["127.0.0.1", "127.0.0.2", "127.0.0.3"];

/// Gossip convergence is normally 1-3 s.
const CONVERGE_TIMEOUT: Duration = Duration::from_secs(45);
/// A cold owner settles a site once its probes answer — immediately for a
/// site some peer holds, after the 15 s post-start grace for a new one.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(90);

struct Node {
    dir: tempfile::TempDir,
    cluster: Arc<ClusterHandle>,
    resolver: Arc<dyn SiteBackendResolver>,
    _channel: ChannelHandle,
    _handles: Vec<tokio::task::JoinHandle<()>>,
    _primary_view: Arc<AtomicBool>,
}

impl Node {
    fn db_path(&self, site: &str) -> std::path::PathBuf {
        self.dir.path().join(format!("{site}.db"))
    }
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

/// The first `tenant-N.test` key (from `start`) ranked A > B > C.
fn site_ranked_a_b_c(start: usize) -> String {
    (start..start + 8192)
        .map(|i| format!("tenant-{i}.test"))
        .find(|s| rank(s) == IDS)
        .expect("some tenant key must rank A > B > C")
}

/// Bring up one node with production wiring, its `[db.sqlite] dir` pre-built
/// (possibly seeded) by the caller.
async fn bring_up_node(idx: usize, port: u16, dir: tempfile::TempDir) -> Node {
    let join = if idx == 0 { vec![] } else { vec![format!("{}:{port}", IPS[0])] };
    let cluster_cfg = ClusterConfig {
        enabled: true,
        bind: format!("{}:{port}", IPS[idx]),
        join,
        secret: SECRET.to_string(),
        node_id: IDS[idx].to_string(),
        cluster_id: "per-site-handoff-any-peer".to_string(),
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

    Node {
        dir,
        cluster,
        resolver,
        _channel: channel,
        _handles: handles,
        _primary_view: primary_view,
    }
}

/// Seed `site`'s database in `dir` with one marker row, the way a node that
/// held the site would have it on disk.
async fn seed_site(dir: &Path, site: &str, value: &str) {
    let registry =
        SiteBackends::new(dir.to_path_buf(), 4, stats(), tokio::runtime::Handle::current())
            .expect("seeding registry");
    let backend = registry.backend_for(site).await.expect("open the seeded site");
    let conn = backend.connect().await.expect("seed session");
    conn.execute("CREATE TABLE marker (id INTEGER PRIMARY KEY, v TEXT NOT NULL)", &[])
        .await
        .expect("create marker table");
    conn.execute("INSERT INTO marker (id, v) VALUES (1, ?)", &[Value::Text(value.into())])
        .await
        .expect("insert marker row");
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

/// Resolve `site` as the PHP bridge does (synchronously, on a blocking
/// thread), retrying while its owner is still settling the handoff.
async fn resolve_settled(node: &Node, site: &str) -> SharedBackend {
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    loop {
        let resolver = Arc::clone(&node.resolver);
        let key = site.to_string();
        let res = tokio::task::spawn_blocking(move || resolver.resolve(&key))
            .await
            .expect("resolve task");
        match res {
            Ok(backend) => return backend,
            Err(e) => {
                assert!(
                    e.contains("not being served"),
                    "only the handoff gate may refuse here, got: {e}"
                );
                assert!(Instant::now() < deadline, "site {site} never settled: {e}");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
}

async fn marker_values(backend: &SharedBackend) -> Vec<Value> {
    let conn = backend.connect().await.expect("session");
    let rs = conn.query("SELECT v FROM marker ORDER BY id", &[]).await.expect("read marker");
    rs.rows.into_iter().map(|mut r| r.remove(0)).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_cold_owner_pulls_from_whichever_peer_holds_the_site() {
    if !loopbacks_bindable() {
        eprintln!(
            "skipping a_cold_owner_pulls_from_whichever_peer_holds_the_site: 127.0.0.2/127.0.0.3 \
             are not bindable on this host (macOS needs `sudo ifconfig lo0 alias 127.0.0.N`)"
        );
        return;
    }

    let held = site_ranked_a_b_c(0);
    let fresh = site_ranked_a_b_c(10_000);
    assert_ne!(held, fresh);

    // Only C — "the old hash's owner" — holds `held`.
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    seed_site(dirs[2].path(), &held, "from-the-old-owner").await;

    let port = pick_cluster_port();
    let mut dirs = dirs.into_iter();
    let a = bring_up_node(0, port, dirs.next().unwrap()).await;
    let b = bring_up_node(1, port, dirs.next().unwrap()).await;
    let c = bring_up_node(2, port, dirs.next().unwrap()).await;
    await_membership(&[&a, &b, &c]).await;

    let view = a.cluster.nodes().await;
    assert_eq!(hrw_owner(&view, &held).map(|n| n.id.as_str()), Some(IDS[0]));
    assert!(!a.db_path(&held).exists() && !b.db_path(&held).exists());
    assert!(c.db_path(&held).exists());

    // ── 1. A serves C's data, past the runner-up B that has none ─────────
    let backend = resolve_settled(&a, &held).await;
    assert_eq!(
        marker_values(&backend).await,
        [Value::Text("from-the-old-owner".into())],
        "the new owner must serve the copy held by rank 3, not an empty database"
    );

    // ── 2. Asking B did not create B's database ──────────────────────────
    assert!(
        !b.db_path(&held).exists(),
        "a peer answering a snapshot request for a site it never held must not create its file"
    );

    // ── 3. A site nobody holds settles as new and empty ──────────────────
    let backend = resolve_settled(&a, &fresh).await;
    let conn = backend.connect().await.expect("session on the new site");
    conn.execute("CREATE TABLE marker (id INTEGER PRIMARY KEY, v TEXT NOT NULL)", &[])
        .await
        .expect("a new site starts empty and accepts writes");
    conn.execute("INSERT INTO marker (id, v) VALUES (1, 'new')", &[]).await.expect("insert");
    assert_eq!(marker_values(&backend).await, [Value::Text("new".into())]);
    assert!(
        !b.db_path(&fresh).exists() && !c.db_path(&fresh).exists(),
        "probing peers for a brand-new site must not leave empty files on them"
    );

    drop((a, b, c));
}
