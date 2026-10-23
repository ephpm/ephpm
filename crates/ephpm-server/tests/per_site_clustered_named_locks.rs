//! Three-node, in-process test of MySQL named locks (`GET_LOCK()` & co.) in
//! per-site **clustered** mode.
//!
//! # What this pins
//!
//! In per-site clustered mode a site's statements run on its HRW owner: the
//! owner serves them locally and every other node forwards over
//! `sql/<site>`. Named locks are session state, so they must be coordinated
//! **on the owner** — two connections for one site must exclude each other
//! whichever node they landed on — and a lock must be released when its
//! holder's connection ends, however it ends: the connection closed, or the
//! holder's whole node went away.
//!
//! The scenarios, each the shape of a real failure:
//!
//! 1. A site owned by node A. Node B's connection (forwarded) takes the lock;
//!    node C's connection (forwarded) and A's own local connection both get
//!    `0`. Lock table = one per site, on the owner.
//! 2. The same name under a *different* site owned by A is free: the
//!    namespace is the site, not the node.
//! 3. B's connection is dropped without releasing (a killed worker): C's
//!    timed `GET_LOCK` wakes and acquires well before its timeout.
//! 4. C now holds. C's node dies (its cluster channel is shut down, closing
//!    every connection it had — what a crashed process does at the TCP
//!    level): A's local timed `GET_LOCK` acquires.
//!
//! # Node addressing
//!
//! Three loopback IPs on one port, as `per_site_handoff_any_peer.rs`
//! documents: a non-owner derives a peer's channel address as
//! `gossip_ip : gossip_port + 2`, so the nodes must differ by IP. Skipped
//! where `127.0.0.2`/`127.0.0.3` are not bindable (macOS without aliases).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use ephpm_cluster::{
    ChannelFeatureFlags, ChannelHandle, ClusterHandle, NodeState, hrw_owner,
    maybe_start_cluster_channel, start_gossip,
};
use ephpm_config::{ClusterChannelConfig, ClusterConfig, SqliteConfig};
use ephpm_php::db_bridge::SiteBackendResolver;
use ephpm_server::site_backends::{SiteBackends, SiteOpenHook};
use ephpm_server::sql_forward::ClusteredSiteResolver;
use litewire::backend::{SharedBackend, Value};
use litewire::translate::Dialect;
use litewire::{Session, SessionResult};

const SECRET: &str = "per-site-clustered-named-locks-test-secret";
const IDS: [&str; 3] = ["ephpm-a", "ephpm-b", "ephpm-c"];
const IPS: [&str; 3] = ["127.0.0.1", "127.0.0.2", "127.0.0.3"];

/// Gossip convergence is normally 1-3 s.
const CONVERGE_TIMEOUT: Duration = Duration::from_secs(45);
/// A new site settles on its owner once the post-start grace has passed.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(90);

struct Node {
    id: String,
    cluster: Arc<ClusterHandle>,
    resolver: Arc<dyn SiteBackendResolver>,
    channel: ChannelHandle,
    _dir: tempfile::TempDir,
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

/// Bring up one node exactly as `wire_per_site_clustered_db` +
/// `start_clustered_per_site_turso` wire it in `serve()`.
async fn bring_up_node(idx: usize, port: u16) -> Node {
    let dir = tempfile::tempdir().expect("per-site database dir");
    let join = if idx == 0 { vec![] } else { vec![format!("{}:{port}", IPS[0])] };
    let cluster_cfg = ClusterConfig {
        enabled: true,
        bind: format!("{}:{port}", IPS[idx]),
        join,
        secret: SECRET.to_string(),
        node_id: IDS[idx].to_string(),
        cluster_id: "per-site-clustered-named-locks".to_string(),
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
        id: IDS[idx].to_string(),
        cluster,
        resolver,
        channel,
        _dir: dir,
        _handles: handles,
        _primary_view: primary_view,
    }
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

/// A MySQL-dialect session over one connection to `backend` — what a
/// `pdo_mysql` client or the bridge drives.
async fn session(backend: &SharedBackend) -> Session {
    let conn = backend.connect().await.expect("connect");
    Session::new(conn, Dialect::MySQL)
}

/// `SELECT <lock expr>` → the single cell.
async fn lock_value(session: &mut Session, sql: &str) -> Value {
    match session.query(sql, &[]).await.expect("lock statement") {
        SessionResult::Rows(rs) => rs.rows[0][0].clone(),
        SessionResult::Ok(ok) => panic!("expected rows, got {ok:?}"),
    }
}

/// The first `tenant-N.test` key (from `start`) whose HRW owner is `owner_id`.
fn site_owned_by(nodes: &[ephpm_cluster::NodeInfo], owner_id: &str, start: usize) -> String {
    (start..start + 4096)
        .map(|i| format!("tenant-{i}.test"))
        .find(|site| hrw_owner(nodes, site).is_some_and(|n| n.id == owner_id))
        .expect("some tenant key must hash to the requested owner")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn named_locks_are_coordinated_on_the_owner_and_die_with_their_holder() {
    if !loopbacks_bindable() {
        eprintln!(
            "skipping named_locks_are_coordinated_on_the_owner_and_die_with_their_holder: \
             127.0.0.2/127.0.0.3 are not bindable on this host"
        );
        return;
    }

    let port = pick_cluster_port();
    let node_a = bring_up_node(0, port).await;
    let node_b = bring_up_node(1, port).await;
    let node_c = bring_up_node(2, port).await;
    await_membership(&[&node_a, &node_b, &node_c]).await;

    // Two sites, both owned by A (from every node's point of view).
    let members = node_b.cluster.nodes().await;
    let site = site_owned_by(&members, &node_a.id, 0);
    let other_site = site_owned_by(&members, &node_a.id, 4096);
    for n in [&node_b, &node_c] {
        assert_eq!(
            hrw_owner(&n.cluster.nodes().await, &site).map(|o| o.id.clone()),
            Some(node_a.id.clone()),
            "the site must be owned by A from {}'s point of view",
            n.id
        );
    }

    // A serves locally (once the new site settles); B and C forward.
    let a_backend = resolve_settled(&node_a, &site).await;
    let b_backend = resolve_settled(&node_b, &site).await;
    let c_backend = resolve_settled(&node_c, &site).await;
    let mut a_local = session(&a_backend).await;
    let mut b_fwd = session(&b_backend).await;
    let mut c_fwd = session(&c_backend).await;

    // ── 1. One lock table per site, on the owner ─────────────────────────
    let acquire = r#"SELECT GET_LOCK("sendy_campaign_send_1", 0) AS lock_status"#;
    assert_eq!(lock_value(&mut b_fwd, acquire).await, Value::Integer(1), "B (forwarded) takes it");
    assert_eq!(
        lock_value(&mut c_fwd, acquire).await,
        Value::Integer(0),
        "C (forwarded) is excluded"
    );
    assert_eq!(lock_value(&mut a_local, acquire).await, Value::Integer(0), "A (local) is excluded");
    assert!(
        matches!(
            lock_value(&mut a_local, "SELECT IS_USED_LOCK('sendy_campaign_send_1')").await,
            Value::Integer(_)
        ),
        "the owner sees the forwarded holder's session"
    );
    // The holder may re-enter; nobody else may release.
    assert_eq!(lock_value(&mut b_fwd, acquire).await, Value::Integer(1));
    assert_eq!(
        lock_value(&mut c_fwd, "SELECT RELEASE_LOCK('sendy_campaign_send_1')").await,
        Value::Integer(0)
    );

    // ── 2. The namespace is the site ─────────────────────────────────────
    let other_backend = resolve_settled(&node_a, &other_site).await;
    let mut other_local = session(&other_backend).await;
    assert_eq!(
        lock_value(&mut other_local, acquire).await,
        Value::Integer(1),
        "the same name under another site is another lock"
    );
    drop(other_local);

    // ── 3. The holder's connection vanishes without releasing ────────────
    // C waits (forwarded, with the owner heartbeating); dropping B's session
    // closes B's forwarding stream, the owner drops the stream's session, and
    // C is woken.
    let pending = tokio::spawn(async move {
        let started = Instant::now();
        let got = lock_value(&mut c_fwd, r#"SELECT GET_LOCK("sendy_campaign_send_1", 60)"#).await;
        (got, started.elapsed(), c_fwd)
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(b_fwd);
    let (got, waited, mut c_fwd) = pending.await.expect("waiter task");
    assert_eq!(got, Value::Integer(1), "C acquires once B's connection is gone");
    assert!(waited < Duration::from_secs(30), "woken by the release, not the timeout: {waited:?}");
    assert_eq!(lock_value(&mut a_local, acquire).await, Value::Integer(0), "C holds it now");

    // ── 4. The holder's NODE dies ────────────────────────────────────────
    // Shutting C's channel closes every connection it had, which is what a
    // crashed process does to its sockets. The owner's stream reads EOF.
    let pending = tokio::spawn(async move {
        let started = Instant::now();
        let got = lock_value(&mut a_local, r#"SELECT GET_LOCK("sendy_campaign_send_1", 60)"#).await;
        (got, started.elapsed(), a_local)
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    node_c.channel.shutdown();
    let (got, waited, _a_local) = pending.await.expect("waiter task");
    assert_eq!(got, Value::Integer(1), "A acquires once C's node is gone");
    assert!(
        waited < Duration::from_secs(30),
        "woken by the node's death, not the timeout: {waited:?}"
    );
    // C's session is unusable now, as its owner's refusal/EOF says.
    assert!(c_fwd.query("SELECT 1", &[]).await.is_err());
}
