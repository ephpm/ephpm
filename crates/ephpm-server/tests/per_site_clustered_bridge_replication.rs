//! Two-node, in-process end-to-end test for the **bridge-only replication
//! gap** in per-site clustered Turso mode.
//!
//! # The failure this pins
//!
//! In per-site clustered mode a node joins a site's replication set only when
//! that site is *announced* to the per-site driver (`ensure_site_driver` in
//! `turso_cdc`), which is what subscribes it to `cdc/<site>` and materializes a
//! local replica. There are exactly two announcers, and they must stay in sync:
//!
//! 1. `SiteBackends::get_or_open`'s `on_open` hook — a **local** database open
//!    (the site's HRW owner, and any stock-`pdo_mysql` connection); and
//! 2. `ClusteredSiteResolver`'s forwarding branch (`plan_serve`) — the
//!    `ephpm_db_*` bridge route on a node that does **not** own the site.
//!
//! Only the first existed. A node whose entire exposure to a tenant is bridge
//! traffic for a site it does not own therefore never opened that site locally,
//! never announced it, and never replicated it — while behaving perfectly:
//! reads and writes both worked (they were forwarded to the owner), every
//! health check stayed green, and `/_ephpm/primary` returned 200. The tenant's
//! data simply lived on exactly **one** node. The bill came due at failover:
//! HRW re-homes a site to a node that is supposed to already hold a warm
//! replica, and that node held nothing.
//!
//! Stock `pdo_mysql` hides the bug completely — it opens the database locally
//! as a side effect, so announcer 1 fires. It bites precisely the *recommended*
//! deployment: the `db-*` drop-ins, which call `ephpm_db_*` and nothing else.
//!
//! # Why an integration test and not just the unit tests
//!
//! `sql_forward`'s unit tests already assert that `plan_serve` calls the
//! announcement hook on the forwarding branch. That pins the call site, not the
//! consequence — it would still pass if the hook the resolver was handed were a
//! *different* hook from the registry's (they are `Arc`-cloned from one channel
//! sender in `wire_per_site_clustered_db`), or if announcing failed to actually
//! start a driver. This test wires two real gossip nodes with the production
//! wiring and asserts the observable end state instead: a `.db` file for the
//! site materializes on the bridge-only node and fills with the owner's data.
//!
//! # What a regression looks like here
//!
//! Revert `plan_serve`'s `note_active(site)` and this test fails at the
//! "node B materializes a local replica" assertion: `<b_dir>/<site>.db` never
//! appears, because no driver was ever started for the site on B. Every other
//! step still passes — which is exactly what made the bug invisible in
//! production.
//!
//! # Node addressing
//!
//! The two nodes use distinct loopback IPs (`127.0.0.1` / `127.0.0.2`) sharing
//! one port, the same trick `per_site_kv_replication.rs` documents. It is not
//! cosmetic here: a non-owner derives its peer's cluster-channel address as
//! `peer_gossip_ip : gossip_port + 2` (`sql_forward::member_channel_addr`), so
//! two nodes distinguished only by port would compute each other's channel
//! address wrongly. Binding the channel at the derived port (by leaving
//! `[cluster.channel] listen` unset, as a default deployment does) means the
//! forwarding and snapshot dials in this test resolve exactly as they do in
//! production.
//!
//! Gossip convergence and the election's startup grace take tens of seconds, so
//! every wait polls to a generous deadline rather than sleeping a fixed amount.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use ephpm_cluster::{
    ChannelFeatureFlags, ChannelHandle, ClusterHandle, hrw_owner, maybe_start_cluster_channel,
    start_gossip,
};
use ephpm_config::{ClusterChannelConfig, ClusterConfig, SqliteConfig};
use ephpm_php::db_bridge::SiteBackendResolver;
use ephpm_server::site_backends::{SiteBackends, SiteOpenHook};
use ephpm_server::sql_forward::ClusteredSiteResolver;
use litewire::backend::{SharedBackend, Value};

/// Shared cluster secret for the two nodes.
const SECRET: &str = "per-site-clustered-bridge-replication-test-secret";

/// How long to wait for gossip to report both nodes. Convergence is normally
/// 1-3s; this is far above that so a loaded box cannot flake it.
const CONVERGE_TIMEOUT: Duration = Duration::from_secs(45);

/// How long to wait for the bridge-only node to materialize the site's local
/// database file. This is the bug-sensitive assertion: on the fixed code the
/// driver's mgmt factory creates the file within milliseconds of the
/// announcement, so a generous deadline costs nothing when passing and still
/// terminates promptly-enough when the announcement never happens.
const REPLICA_FILE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long to wait for the owner's row to arrive on the replica. This one is
/// genuinely slow by design: the per-site election applies a ~15s startup grace
/// before the owner may publish its claim (issue #314), the replica then needs
/// a heartbeat tick to observe it, and only then does it bootstrap.
const REPLICATE_TIMEOUT: Duration = Duration::from_secs(120);

/// One node's per-site clustered stack, wired exactly as
/// `ephpm_server::wire_per_site_clustered_db` + `start_clustered_per_site_turso`
/// wire it in `serve()`.
struct Node {
    /// Configured gossip node id — the identity HRW ownership is computed over.
    id: String,
    /// `[db.sqlite] dir` for this node. Held so it outlives the test.
    dir: tempfile::TempDir,
    cluster: Arc<ClusterHandle>,
    /// The `ephpm_db_*` bridge's resolver: local when this node owns the site,
    /// a forwarding proxy to the owner otherwise.
    resolver: Arc<dyn SiteBackendResolver>,
    /// The per-site serving registry. Only ever touched *after* the
    /// bug-sensitive assertion — resolving through it opens the database
    /// locally, which announces the site and would mask the defect.
    registry: SiteBackends,
    /// Kept alive for the test's duration.
    _channel: ChannelHandle,
    _handles: Vec<tokio::task::JoinHandle<()>>,
    _primary_view: Arc<AtomicBool>,
}

impl Node {
    /// The path this node would hold `site`'s database at.
    fn db_path(&self, site: &str) -> std::path::PathBuf {
        self.dir.path().join(format!("{site}.db"))
    }
}

/// Whether this host routes `127.0.0.2` to loopback so a second in-process node
/// can bind it. Linux treats all of `127.0.0.0/8` as loopback and Windows
/// accepts the whole range too; macOS only has `127.0.0.1` unless an alias is
/// added (`sudo ifconfig lo0 alias 127.0.0.2`). Callers skip rather than fail —
/// the gap is environmental, not a property of the code under test.
fn second_loopback_bindable() -> bool {
    std::net::TcpListener::bind("127.0.0.2:0").is_ok()
        && std::net::UdpSocket::bind("127.0.0.2:0").is_ok()
}

/// Reserve a gossip port usable by **both** nodes: free for UDP on each
/// loopback IP, with `port + 2` (the derived cluster-channel port) free for TCP
/// on each as well. Both nodes then share one port number across two IPs, which
/// is what makes `gossip_ip : gossip_port + 2` a correct channel address for
/// each node from the other's point of view.
fn pick_cluster_port() -> u16 {
    for _ in 0..128 {
        let Ok(probe) = std::net::UdpSocket::bind("127.0.0.1:0") else { continue };
        let Ok(addr) = probe.local_addr() else { continue };
        let port = addr.port();
        drop(probe);
        let Some(channel_port) = port.checked_add(2) else { continue };
        if std::net::UdpSocket::bind(("127.0.0.1", port)).is_ok()
            && std::net::UdpSocket::bind(("127.0.0.2", port)).is_ok()
            && std::net::TcpListener::bind(("127.0.0.1", channel_port)).is_ok()
            && std::net::TcpListener::bind(("127.0.0.2", channel_port)).is_ok()
        {
            return port;
        }
    }
    panic!("could not find a gossip port free on both loopback IPs with its channel port free too");
}

/// Query-stats collector for the per-site registries — disabled, since this
/// test is about replication rather than observability.
fn stats() -> ephpm_query_stats::QueryStats {
    ephpm_query_stats::QueryStats::new(ephpm_query_stats::StatsConfig {
        enabled: false,
        slow_query_threshold: Duration::from_secs(1),
        max_digests: 16,
        metric_label_series_max: 16,
    })
}

/// The `[db.sqlite]` config the replication plane reads: per-site clustered,
/// rooted at `dir`.
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

/// Bring up one node: gossip + cluster channel + the per-site clustered
/// registry, forwarding resolver, and replication plane.
///
/// The `note_active` hook is built once and shared between the registry and the
/// resolver, exactly as `wire_per_site_clustered_db` does. That sharing is the
/// property under test: it is what makes a forwarded site announce itself into
/// the same replication plane a locally-opened one does.
async fn bring_up_node(node_id: &str, ip: &str, port: u16, join: Vec<String>) -> Node {
    let cluster_cfg = ClusterConfig {
        enabled: true,
        bind: format!("{ip}:{port}"),
        join,
        secret: SECRET.to_string(),
        node_id: node_id.to_string(),
        cluster_id: "per-site-bridge-replication".to_string(),
        ..ClusterConfig::default()
    };
    let cluster = Arc::new(start_gossip(&cluster_cfg).await.expect("gossip start"));

    // `listen: None` derives `gossip_port + 2` — the same rule
    // `sql_forward::member_channel_addr` uses to find a peer's channel, so
    // peers can dial each other cold, before any election claim is published.
    let channel = maybe_start_cluster_channel(
        &ClusterChannelConfig { listen: None, secret: None },
        &cluster_cfg.secret,
        &cluster,
        ChannelFeatureFlags { cdc: true },
    )
    .await
    .expect("cluster channel start")
    .expect("cluster channel bound (the cdc feature is enabled)");

    let dir = tempfile::tempdir().expect("per-site database dir");
    let sqlite = sqlite_config(dir.path());

    // ONE hook, two announcers — see the function docs.
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
        registry.clone(),
        &mut handles,
        &primary_view,
    )
    .await
    .expect("per-site clustered replication plane");

    Node {
        id: node_id.to_string(),
        dir,
        cluster,
        resolver,
        registry,
        _channel: channel,
        _handles: handles,
        _primary_view: primary_view,
    }
}

/// Poll `check` until it returns `true`, or panic naming `what`.
async fn eventually<F, Fut>(what: &str, timeout: Duration, interval: Duration, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if check().await {
            return;
        }
        assert!(Instant::now() < deadline, "timed out after {timeout:?} waiting for {what}");
        tokio::time::sleep(interval).await;
    }
}

/// Wait until every node's membership view holds `expected` alive nodes.
///
/// Load-bearing for correctness, not just tidiness: before gossip converges a
/// node's view holds only itself, so `hrw_owner` names *it* as every site's
/// owner and a resolve would take the LOCAL branch — opening the database,
/// announcing the site through the registry hook, and masking the very defect
/// this test exists to catch.
async fn await_membership(nodes: &[&Node], expected: usize) {
    eventually(
        "both nodes to see a two-node cluster",
        CONVERGE_TIMEOUT,
        Duration::from_millis(100),
        || async {
            for n in nodes {
                let alive = n
                    .cluster
                    .nodes()
                    .await
                    .iter()
                    .filter(|m| m.state == ephpm_cluster::NodeState::Alive)
                    .count();
                if alive < expected {
                    return false;
                }
            }
            true
        },
    )
    .await;
}

/// Resolve `site` the way the PHP bridge does.
///
/// `SiteBackendResolver::resolve` is synchronous and `block_on`s internally, so
/// it must run on a blocking thread — calling it from a runtime worker panics
/// ("Cannot start a runtime from within a runtime"). In production that
/// invariant holds because the bridge only ever runs on `spawn_blocking` PHP
/// threads; here it is reproduced explicitly.
async fn resolve_as_bridge(
    resolver: &Arc<dyn SiteBackendResolver>,
    site: &str,
) -> Result<SharedBackend, String> {
    let resolver = Arc::clone(resolver);
    let site = site.to_string();
    tokio::task::spawn_blocking(move || resolver.resolve(&site)).await.expect("resolve task")
}

/// The first `tenant-N.test` key whose HRW owner is `owner_id`.
///
/// Searched rather than hardcoded: HRW is a hash over `(node id, site)`, so
/// which node owns a given name is not predictable from reading the test, and a
/// guessed key that happened to land on node B would silently invert the whole
/// scenario.
fn site_owned_by(nodes: &[ephpm_cluster::NodeInfo], owner_id: &str) -> String {
    (0..4096)
        .map(|i| format!("tenant-{i}.test"))
        .find(|site| hrw_owner(nodes, site).is_some_and(|n| n.id == owner_id))
        .expect("some tenant key must hash to the requested owner")
}

/// Count rows in `bridge_marker`, or `None` if the table is not there yet /
/// the database is momentarily locked. Never panics on a transient error: the
/// replica's driver and this read hold two handles on one file (as they do in
/// production), so `database is locked` is an expected, retryable outcome.
async fn marker_rows(backend: &SharedBackend) -> Option<i64> {
    let conn = backend.connect().await.ok()?;
    let rs = conn.query("SELECT COUNT(*) FROM bridge_marker", &[]).await.ok()?;
    match rs.rows.first()?.first()? {
        Value::Integer(n) => Some(*n),
        _ => None,
    }
}

/// **The end-to-end proof.** A site owned by node A, whose only exposure on
/// node B is `ephpm_db_*` bridge traffic, ends up with a live local replica on
/// B that receives A's writes over CDC.
///
/// The steps, and what each one is guarding:
///
/// 1. Two nodes, production wiring, converged membership — without convergence
///    a node owns every site by default and nothing forwards.
/// 2. A site key chosen so node **A** is its HRW owner.
/// 3. A resolves it (local branch) and writes a row. This is the only copy of
///    the tenant's data in the cluster.
/// 4. B resolves it through the **bridge resolver** — the forwarding branch,
///    and the only traffic B ever sees for this site. B's registry is
///    deliberately untouched until after step 5.
/// 5. B materializes `<b_dir>/<site>.db`. **This is the assertion the bug
///    fails**: no announcement means no driver means no file.
/// 6. B's local replica contains A's row — proving a real CDC/bootstrap path,
///    not merely that a driver was spawned.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_bridge_only_node_replicates_the_site_it_forwards() {
    if !second_loopback_bindable() {
        eprintln!(
            "skipping a_bridge_only_node_replicates_the_site_it_forwards: 127.0.0.2 is not \
             bindable on this host (macOS needs `sudo ifconfig lo0 alias 127.0.0.2`)"
        );
        return;
    }

    // ── 1. Two nodes, production wiring ──────────────────────────────────
    let port = pick_cluster_port();
    let node_a = bring_up_node("ephpm-a", "127.0.0.1", port, vec![]).await;
    let node_b =
        bring_up_node("ephpm-b", "127.0.0.2", port, vec![format!("127.0.0.1:{port}")]).await;
    await_membership(&[&node_a, &node_b], 2).await;

    // ── 2. A site node A owns ────────────────────────────────────────────
    let members = node_b.cluster.nodes().await;
    let site = site_owned_by(&members, &node_a.id);
    assert_eq!(
        hrw_owner(&members, &site).map(|n| n.id.as_str()),
        Some(node_a.id.as_str()),
        "the chosen site must be owned by node A from node B's point of view, \
         or B would serve it locally instead of forwarding"
    );

    // ── 3. The owner writes the tenant's only copy of its data ───────────
    let owner_backend =
        resolve_as_bridge(&node_a.resolver, &site).await.expect("node A resolves its own site");
    let owner_conn = owner_backend.connect().await.expect("owner session");
    owner_conn
        .execute("CREATE TABLE bridge_marker (id INTEGER PRIMARY KEY, v TEXT NOT NULL)", &[])
        .await
        .expect("create marker table on the owner");
    owner_conn
        .execute("INSERT INTO bridge_marker (id, v) VALUES (1, 'from-owner')", &[])
        .await
        .expect("insert marker row on the owner");
    assert!(node_a.db_path(&site).exists(), "the owner must hold the site's database on disk");

    // ── 4. Node B's ONLY exposure to the site: the bridge resolver ───────
    // Nothing has touched B's copy yet — assert that, so a later file check
    // cannot be satisfied by something this test did itself.
    assert!(
        !node_b.db_path(&site).exists(),
        "node B must not hold the site's database before it has seen any traffic for it"
    );
    let forwarding_backend = resolve_as_bridge(&node_b.resolver, &site)
        .await
        .expect("node B resolves the site through the forwarding bridge resolver");

    // A read through the proxy exercises the forwarding path itself (dial the
    // owner over `sql/<site>`, run there, stream the rowset back). Polled
    // rather than asserted once: it is not what this test is about, and a dial
    // can lose a race with the owner's stream handler coming up.
    eventually(
        "a forwarded read on node B to reach the site's owner",
        CONVERGE_TIMEOUT,
        Duration::from_millis(250),
        || async { marker_rows(&forwarding_backend).await == Some(1) },
    )
    .await;

    // ── 5. THE BUG-SENSITIVE ASSERTION ───────────────────────────────────
    // Forwarding announced the site to B's replication plane, which started a
    // per-site driver, which opened B's mgmt factory — creating the file. With
    // the announcement removed from `plan_serve`, no driver ever starts and
    // this file never appears, even though step 4 above keeps working forever.
    eventually(
        "node B to materialize a local replica of the site it only ever forwarded \
         (regression: the forwarding branch must announce the site to the replication plane)",
        REPLICA_FILE_TIMEOUT,
        Duration::from_millis(200),
        || {
            let path = node_b.db_path(&site);
            async move { path.exists() }
        },
    )
    .await;

    // ── 6. And the replica actually holds the owner's data ───────────────
    // Now that a driver is running, resolving through B's registry is safe: the
    // site is already announced, so this cannot be what created the replica.
    let registry_resolver = node_b.registry.as_resolver();
    let replica_backend =
        resolve_as_bridge(&registry_resolver, &site).await.expect("node B opens its local replica");
    eventually(
        "the owner's row to replicate to node B's local database over CDC",
        REPLICATE_TIMEOUT,
        Duration::from_millis(250),
        || async { marker_rows(&replica_backend).await == Some(1) },
    )
    .await;

    let conn = replica_backend.connect().await.expect("replica session");
    let rs = conn.query("SELECT id, v FROM bridge_marker", &[]).await.expect("read the replica");
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(rs.rows[0][0], Value::Integer(1));
    assert_eq!(rs.rows[0][1], Value::Text("from-owner".into()));

    // Hold both nodes' gossip handles to the end — chitchat heartbeats only
    // while the handle lives.
    drop((node_a, node_b));
}
