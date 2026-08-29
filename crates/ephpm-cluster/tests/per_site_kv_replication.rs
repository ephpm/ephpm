//! Two-node, in-process end-to-end test for **per-vhost KV replication**.
//!
//! # Why this exists
//!
//! Two rounds of per-site KV replication shipped green unit tests and failed on
//! the live cluster, because every existing test stopped short of the real
//! path:
//!
//! * the data-plane tests drove `handle_connection` directly, so they proved
//!   *routing* but never that a write reached the wire at all;
//! * the multi-tenant tests used trivial replicator closures, so they proved
//!   *installation* but never replication;
//! * nothing drove the **top-level `Store` API** — which is where the actual
//!   bug lived: `Store::incr_by` mutated the local map and never called the
//!   replicator, so a counter advanced on the writing node and stayed frozen on
//!   every peer, silently, forever.
//!
//! This harness closes that gap. It stands up two real gossip nodes in one
//! process with the **production wiring** (`MultiTenantStore` +
//! `SiteKvReplicator` factory + gossip applier + routed data plane) and drives
//! `Store::set` / `Store::incr_by` on node A's site store, asserting the value
//! becomes visible through node B's site store — across **both** tiers:
//!
//! * **small values** (≤ `small_key_threshold`) ride chitchat gossip;
//! * **large values** (> threshold) ride the TCP data plane's replica set.
//!
//! # Node addressing
//!
//! The two nodes use distinct loopback IPs (`127.0.0.1` / `127.0.0.2`) rather
//! than distinct ports. `ClusteredStore` derives a peer's data-plane address as
//! `peer_gossip_ip : data_port` — one port for the whole cluster — so two nodes
//! sharing an IP would collide on that port. Distinct loopback IPs are the same
//! trick `kv_data_plane::serve_on` documents for in-process multi-node tests.
//!
//! Gossip needs seconds to converge, so every assertion polls with a generous
//! timeout instead of sleeping a fixed amount.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ephpm_cluster::clustered_store::{new_applied_write_map, start_gossip_applier_multi_tenant};
use ephpm_cluster::{ClusteredStore, SiteKvReplicator};
use ephpm_kv::multi_tenant::{MultiTenantStore, SiteReplicatorFactory};
use ephpm_kv::store::{Store, StoreConfig};

/// The vhost under test — named after the live deployment that exposed the bug.
const SITE: &str = "switchboard";

/// Values at or below this ride gossip; above it, the data plane.
const SMALL_KEY_THRESHOLD: usize = 1024;

/// How long to wait for a value to replicate. Gossip converges in ~1-3s; this
/// is deliberately far above that so a slow CI box cannot flake the test, while
/// a genuine "never replicates" failure still terminates.
const CONVERGE_TIMEOUT: Duration = Duration::from_secs(45);

/// One node's full KV stack, wired exactly as `ephpm-server::serve` wires it.
struct Node {
    sites: MultiTenantStore,
}

impl Node {
    /// This node's store for [`SITE`] — the same handle a request would get.
    fn site_store(&self) -> Arc<Store> {
        self.sites.get_site_store(SITE)
    }
}

/// Reserve a port that is free on **both** loopback IPs, so the two nodes can
/// share one `data_port` value while binding different addresses.
fn free_shared_port() -> u16 {
    for _ in 0..64 {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe bind");
        let port = probe.local_addr().expect("probe addr").port();
        drop(probe);
        // The same port must also be bindable on the second IP.
        if let Ok(second) = std::net::TcpListener::bind(("127.0.0.2", port)) {
            drop(second);
            return port;
        }
    }
    panic!("could not find a port free on both 127.0.0.1 and 127.0.0.2");
}

/// Whether this host routes `127.0.0.2` to loopback so a second in-process node
/// can bind it. Linux treats the whole `127.0.0.0/8` as loopback, so this is
/// always true there; macOS only has `127.0.0.1` unless an alias is added
/// (`sudo ifconfig lo0 alias 127.0.0.2`), so these two-IP tests cannot run on an
/// unaliased macOS host. Callers skip rather than fail when this is false — the
/// coverage is environmental, not a property of the code under test.
fn second_loopback_bindable() -> bool {
    std::net::TcpListener::bind("127.0.0.2:0").is_ok()
        && std::net::UdpSocket::bind("127.0.0.2:0").is_ok()
}

/// A free UDP port on `ip`, for gossip.
fn free_gossip_addr(ip: &str) -> String {
    let sock = std::net::UdpSocket::bind((ip, 0)).expect("gossip probe bind");
    let addr = sock.local_addr().expect("gossip probe addr");
    drop(sock);
    format!("{ip}:{}", addr.port())
}

fn kv_config(data_port: u16) -> ephpm_config::ClusterKvConfig {
    ephpm_config::ClusterKvConfig {
        small_key_threshold: SMALL_KEY_THRESHOLD,
        // Two copies on a two-node cluster ⇒ a large value lands on BOTH nodes,
        // which is what makes the large-tier assertion deterministic.
        replication_factor: 2,
        // Await replica writes so a large-value assertion is not racing a
        // fire-and-forget task.
        replication_mode: "sync".to_string(),
        data_port,
        // Hot-key caching is a read-path optimisation and irrelevant here;
        // leaving it on would only add gossip noise.
        hot_key_cache: false,
        ..ephpm_config::ClusterKvConfig::default()
    }
}

/// Build one node with the production wiring and return it.
async fn start_node(
    bind: &str,
    join: Vec<String>,
    secret: &str,
    data_port: u16,
    data_bind_ip: &str,
) -> Node {
    let cfg = ephpm_config::ClusterConfig {
        enabled: true,
        bind: bind.to_string(),
        join,
        secret: secret.to_string(),
        kv: kv_config(data_port),
        ..ephpm_config::ClusterConfig::default()
    };

    let cluster = Arc::new(ephpm_cluster::start_gossip(&cfg).await.expect("gossip start"));

    let store_config = StoreConfig::default();
    let global = Store::new(store_config.clone());
    let sites = MultiTenantStore::new(Arc::clone(&global), store_config);

    // Routed data plane on this node's own IP (see the module docs on
    // addressing). Serves per-site keys into their own vhost stores.
    let dp_router =
        ephpm_cluster::data_plane::KvRouter::multi_tenant(Arc::clone(&global), sites.clone());
    let dp_addr: std::net::SocketAddr =
        format!("{data_bind_ip}:{data_port}").parse().expect("data plane addr");
    tokio::spawn(async move {
        let _ = ephpm_cluster::data_plane::serve_router(dp_router, dp_addr, None).await;
    });

    let clustered =
        ClusteredStore::new(Arc::clone(&global), Arc::clone(&cluster), cfg.kv.clone(), None);

    let applied = new_applied_write_map();

    // The production factory: pure construction over the store handed in.
    let factory_clustered = Arc::clone(&clustered);
    let factory_applied = Arc::clone(&applied);
    let factory_handle = tokio::runtime::Handle::current();
    let factory: SiteReplicatorFactory = Arc::new(move |site: &str, store: &Arc<Store>| {
        SiteKvReplicator::new(
            Arc::clone(&factory_clustered),
            Arc::clone(store),
            site,
            factory_handle.clone(),
            Arc::clone(&factory_applied),
        ) as Arc<dyn ephpm_kv::store::Replicator>
    });
    sites.set_replicator_factory(Some(factory));

    start_gossip_applier_multi_tenant(&cluster, global, Some(sites.clone()), applied).await;

    Node { sites }
}

/// Poll `f` until it returns `Some`, or fail with `what` after the timeout.
async fn eventually<T, F>(what: &str, mut f: F) -> T
where
    F: FnMut() -> Option<T>,
{
    let deadline = Instant::now() + CONVERGE_TIMEOUT;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The end-to-end proof: a vhost's writes made through the **top-level `Store`
/// API** on node A become visible in the same vhost's store on node B, for both
/// replication tiers and for a counter.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_site_writes_replicate_between_two_nodes() {
    if !second_loopback_bindable() {
        eprintln!(
            "skipping per_site_writes_replicate_between_two_nodes: 127.0.0.2 is not \
             bindable on this host (macOS needs `sudo ifconfig lo0 alias 127.0.0.2`)"
        );
        return;
    }
    let secret = "per-site-kv-replication-test-secret";
    let data_port = free_shared_port();
    let addr_a = free_gossip_addr("127.0.0.1");
    let addr_b = free_gossip_addr("127.0.0.2");

    let node_a = start_node(&addr_a, vec![], secret, data_port, "127.0.0.1").await;
    let node_b = start_node(&addr_b, vec![addr_a.clone()], secret, data_port, "127.0.0.2").await;

    let a = node_a.site_store();
    let b = node_b.site_store();

    // ── Small tier: a plain SET rides gossip ──────────────────────────────
    assert!(a.set("switchboard:index".into(), b"page-1".to_vec(), None));
    let got = eventually("the small-tier SET to replicate to node B", || {
        b.get("switchboard:index").map(|v| v.to_vec())
    })
    .await;
    assert_eq!(got, b"page-1".to_vec());

    // ── The regression: INCR must replicate ───────────────────────────────
    // `Store::incr_by` used to mutate locally and publish nothing, so node B
    // read 0 forever while node A read 1.
    assert_eq!(a.incr_by("switchboard:gen", 1).expect("incr"), 1);
    let counter = eventually("the INCR to replicate to node B", || {
        b.get("switchboard:gen").map(|v| v.to_vec())
    })
    .await;
    assert_eq!(counter, b"1".to_vec(), "node B must observe the incremented counter");

    // A second increment must propagate the running total, not just the first.
    assert_eq!(a.incr_by("switchboard:gen", 1).expect("incr"), 2);
    let counter = eventually("the second INCR to replicate", || {
        b.get("switchboard:gen").filter(|v| v.as_ref() == b"2").map(|v| v.to_vec())
    })
    .await;
    assert_eq!(counter, b"2".to_vec());

    // ── Large tier: above the threshold, rides the data plane ─────────────
    let big = vec![b'x'; SMALL_KEY_THRESHOLD * 4];
    assert!(a.set("switchboard:blob".into(), big.clone(), None));
    let got_big = eventually("the large-tier SET to replicate to node B", || {
        b.get("switchboard:blob").map(|v| v.to_vec())
    })
    .await;
    assert_eq!(got_big.len(), big.len(), "node B must receive the full large value");
    assert_eq!(got_big, big);

    // ── Isolation still holds across nodes ────────────────────────────────
    // Another vhost on node B must not see this vhost's keys, and neither may
    // node B's GLOBAL keyspace (the envelope must not leak upward).
    let other_b = node_b.sites.get_site_store("other-tenant");
    assert_eq!(other_b.get("switchboard:index"), None, "a sibling vhost must not see these keys");
    assert_eq!(
        node_b.sites.default_store().get("switchboard:index"),
        None,
        "a per-site key must never land in the global keyspace"
    );
    // ...and the raw transport spelling must not be readable as a global key.
    let enveloped = ephpm_cluster::site_namespace::encode(SITE, "switchboard:index");
    assert_eq!(node_b.sites.default_store().get(&enveloped), None);
}

/// A per-site DELETE must reach peers too — the tombstone path, which shares
/// the envelope with SET.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_site_delete_replicates_between_two_nodes() {
    if !second_loopback_bindable() {
        eprintln!(
            "skipping per_site_delete_replicates_between_two_nodes: 127.0.0.2 is not \
             bindable on this host (macOS needs `sudo ifconfig lo0 alias 127.0.0.2`)"
        );
        return;
    }
    let secret = "per-site-kv-delete-test-secret";
    let data_port = free_shared_port();
    let addr_a = free_gossip_addr("127.0.0.1");
    let addr_b = free_gossip_addr("127.0.0.2");

    let node_a = start_node(&addr_a, vec![], secret, data_port, "127.0.0.1").await;
    let node_b = start_node(&addr_b, vec![addr_a.clone()], secret, data_port, "127.0.0.2").await;

    let a = node_a.site_store();
    let b = node_b.site_store();

    assert!(a.set("switchboard:doomed".into(), b"here".to_vec(), None));
    eventually("the key to replicate before deleting", || b.get("switchboard:doomed")).await;

    a.remove("switchboard:doomed");
    eventually("the DELETE to replicate to node B", || {
        b.get("switchboard:doomed").is_none().then_some(())
    })
    .await;
}
