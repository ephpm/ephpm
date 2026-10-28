//! Cluster node management and gossip lifecycle.
//!
//! Wraps the [`chitchat`] crate to provide a clean, chitchat-agnostic API
//! surface for the rest of ePHPm.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use chitchat::transport::UdpTransport;
use chitchat::{ChitchatConfig, ChitchatHandle, ChitchatId, FailureDetectorConfig, spawn_chitchat};
use ephpm_config::ClusterConfig;

use crate::secure_transport::EncryptedUdpTransport;

/// Information about a single cluster node.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NodeInfo {
    /// Unique node identifier.
    pub id: String,
    /// Gossip UDP address (`host:port`).
    pub gossip_addr: String,
    /// Whether the node is alive or dead (per failure detector).
    pub state: NodeState,
    /// The TCP port the node's KV data plane listens on, as advertised in
    /// its gossip state ([`KV_DATA_PORT_KEY`]). `None` for a node that
    /// predates the advertisement; callers then assume their own
    /// `[cluster.kv] data_port`, which is what every node did before.
    pub data_port: Option<u16>,
    /// The per-site ownership hash scheme the node advertises in its gossip
    /// state ([`HRW_SCHEME_KEY`]). `None` for a node that predates the
    /// advertisement (≤ v0.12.x, which used scheme `1`). See
    /// [`crate::sqlite_election::HRW_SCHEME`].
    pub hrw_scheme: Option<u32>,
    /// Whether the node has announced that it is shutting down
    /// ([`DRAINING_KEY`], set by [`ClusterHandle::mark_draining`]). A draining
    /// node is still alive and still answers on the cluster channel, but it
    /// is no longer a candidate to own a per-site database
    /// ([`crate::hrw_owner`]).
    pub draining: bool,
}

impl NodeInfo {
    /// A node known only by id and gossip address — no advertised data port,
    /// no advertised ownership-hash scheme, not draining.
    #[must_use]
    pub fn new(id: impl Into<String>, gossip_addr: impl Into<String>, state: NodeState) -> Self {
        Self {
            id: id.into(),
            gossip_addr: gossip_addr.into(),
            state,
            data_port: None,
            hrw_scheme: None,
            draining: false,
        }
    }
}

/// Chitchat key a node sets when it begins a graceful shutdown
/// ([`ClusterHandle::mark_draining`]). It is the leave message chitchat does
/// not have: without it peers only notice a stopped node through the failure
/// detector, 24–40 s after its heartbeat stops, and a node that takes minutes
/// to stop (its final WAL checkpoint) is counted alive — and kept as the owner
/// of its per-site databases — the whole time. Outside the `kv:` prefix so the
/// KV applier never sees it.
pub const DRAINING_KEY: &str = "ephpm:draining";

/// Read whether a node has announced it is draining.
fn advertised_draining(state: &chitchat::NodeState) -> bool {
    state.get(DRAINING_KEY).is_some()
}

/// Chitchat key under which a node advertises the per-site ownership hash
/// scheme it computes ([`crate::sqlite_election::HRW_SCHEME`]), so a node
/// can detect peers that would disagree with it on every site's owner.
/// Outside the `kv:` prefix so the KV applier never sees it.
pub const HRW_SCHEME_KEY: &str = "ephpm:hrw_scheme";

/// Read a node's advertised ownership-hash scheme from its gossip state.
fn advertised_hrw_scheme(state: &chitchat::NodeState) -> Option<u32> {
    state.get(HRW_SCHEME_KEY).and_then(|s| s.parse().ok())
}

/// Chitchat key under which a node advertises its KV data plane port.
///
/// Peers dial `<gossip IP>:<this port>` for large-value replication and
/// read-through fetches. Before this key existed every node dialled its
/// *own* `data_port` on every peer, so a cluster with heterogeneous ports
/// silently sent "remote" writes to the wrong listener (stress finding
/// F-15). Deliberately outside the `kv:` prefix so the KV applier never sees
/// it.
pub const KV_DATA_PORT_KEY: &str = "ephpm:kv_data_port";

/// Read a node's advertised KV data port from its gossip state.
fn advertised_data_port(state: &chitchat::NodeState) -> Option<u16> {
    state.get(KV_DATA_PORT_KEY).and_then(|s| s.parse().ok())
}

/// Chitchat key a node sets **once at start** to say that it publishes
/// [`KV_MARK_KEY`]. It sits at one of the lowest versions of the node's state,
/// so a peer receives it in its first delta from the node — which is how a
/// catching-up node tells "this peer's mark has not arrived yet" from "this
/// peer runs a build that never publishes one" (see
/// [`ClusterHandle::catch_up_status`]). Outside the `kv:` prefix so the KV
/// applier never sees it.
pub const KV_CATCH_UP_KEY: &str = "ephpm:kv_catchup";

/// Chitchat key under which a node publishes a wall-clock catch-up mark
/// (epoch milliseconds), refreshed every [`KV_MARK_INTERVAL`] by its gossip
/// writer.
///
/// Chitchat ships a peer's state **in version order**, one MTU per exchange,
/// and a refreshed mark always sits at (or near) the top of the publishing
/// node's state. So when a node that joined at time `T` sees a peer's mark
/// with a value past `T`, it has received every entry the peer held when that
/// mark was set — the catch-up on that peer is complete, and anything newer
/// arrives in order from here on. Outside the `kv:` prefix so the KV applier
/// never sees it.
pub const KV_MARK_KEY: &str = "ephpm:kv_mark";

/// How often a node refreshes its [`KV_MARK_KEY`]. One small entry per
/// interval per node — a rounding error next to KV traffic — and the upper
/// bound on how long after the last entry a catch-up is declared complete.
pub const KV_MARK_INTERVAL: Duration = Duration::from_secs(2);

/// A peer's mark must be this far past the joiner's start time to count, so
/// that clock skew between the two nodes (the cluster is assumed to run NTP;
/// this covers the usual few hundred milliseconds) cannot make a mark set
/// *before* the join look like one set after it.
pub const KV_MARK_SKEW_MARGIN: Duration = Duration::from_secs(2);

/// How far one alive peer's gossip state has reached this node since this
/// node started, as seen by [`ClusterHandle::catch_up_status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerCatchUp {
    /// The peer's node id.
    pub id: String,
    /// Whether this node holds everything the peer published up to a mark
    /// set after this node started — or the peer runs a build without marks,
    /// which is treated as caught up because it can never be proven.
    pub caught_up: bool,
    /// Whether the peer's state has started arriving at all.
    pub seen: bool,
    /// Whether the peer declares that it publishes marks ([`KV_CATCH_UP_KEY`]).
    pub publishes_marks: bool,
    /// The newest mark received from the peer, if any (epoch milliseconds).
    pub mark_ms: Option<u64>,
}

/// The catch-up state of this node against every alive peer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatchUpStatus {
    /// One entry per alive peer (every generation of this node's own id is
    /// excluded — a previous incarnation is not a peer).
    pub peers: Vec<PeerCatchUp>,
}

impl CatchUpStatus {
    /// Whether every alive peer is caught up (vacuously true with no peers).
    #[must_use]
    pub fn complete(&self) -> bool {
        self.peers.iter().all(|p| p.caught_up)
    }

    /// Ids of the peers still being caught up on.
    #[must_use]
    pub fn pending(&self) -> Vec<&str> {
        self.peers.iter().filter(|p| !p.caught_up).map(|p| p.id.as_str()).collect()
    }
}

/// Decide one peer's catch-up from what its gossip state shows: `seen` is
/// whether any of it has arrived, `publishes_marks` whether its first delta
/// declared marks, `mark_ms` the newest mark received, and `started_ms` when
/// this node started.
///
/// A peer that has not been seen at all is pending. A peer that has been seen
/// and does not declare marks (an older build) can never be proven caught up,
/// so it is not waited for. Otherwise the peer is caught up once its mark is
/// at least [`KV_MARK_SKEW_MARGIN`] past the start.
#[must_use]
pub fn peer_caught_up(
    seen: bool,
    publishes_marks: bool,
    mark_ms: Option<u64>,
    started_ms: u64,
) -> bool {
    if !seen {
        return false;
    }
    if !publishes_marks {
        return true;
    }
    let margin = u64::try_from(KV_MARK_SKEW_MARGIN.as_millis()).unwrap_or(u64::MAX);
    mark_ms.is_some_and(|mark| mark >= started_ms.saturating_add(margin))
}

/// Read a node's catch-up mark from its gossip state.
fn advertised_mark(state: &chitchat::NodeState) -> Option<u64> {
    state.get(KV_MARK_KEY).and_then(|s| s.parse().ok())
}

/// A node this node has heard of but whose heartbeat has not advanced for
/// this long since it was first seen is treated as absent by the catch-up
/// gate: the failure detector never classifies a node it has no heartbeat
/// samples for, and a peer that is really there gossips several times a
/// second.
pub const SILENT_PEER_GRACE: Duration = Duration::from_secs(30);

/// Which known nodes the catch-up gate treats as **present** peers.
///
/// The failure detector calls a peer alive only after two heartbeat samples,
/// which under load (a restarted node drowning in 64 KB catch-up datagrams)
/// can take tens of seconds — the live lab opened the gate after the 5 s
/// join wait with *no* peer alive yet, on a node whose peers were plainly
/// there. So the gate also counts a node whose relayed heartbeat has
/// advanced since the tracker first saw it (a dead node's heartbeat is
/// frozen in every digest), and keeps a node it has only just heard of as
/// present for [`SILENT_PEER_GRACE`] so a silent-but-known node holds the
/// gate rather than letting it open vacuously.
///
/// Generic over the heartbeat type only so the rules can be unit-tested with
/// plain integers (chitchat's `Heartbeat` cannot be constructed outside its
/// crate); production uses [`chitchat::Heartbeat`].
#[derive(Debug)]
pub(crate) struct PeerTracker<H = chitchat::Heartbeat> {
    /// Heartbeat and time at which each node was first seen.
    first_seen: std::collections::HashMap<ChitchatId, (H, std::time::Instant)>,
}

impl<H> Default for PeerTracker<H> {
    fn default() -> Self {
        Self { first_seen: std::collections::HashMap::new() }
    }
}

impl<H: Ord + Copy> PeerTracker<H> {
    /// Whether `id` counts as a present peer right now.
    fn present(&mut self, id: &ChitchatId, heartbeat: H, fd_live: bool, fd_dead: bool) -> bool {
        if fd_dead {
            return false;
        }
        let now = std::time::Instant::now();
        let (first_heartbeat, first_at) =
            *self.first_seen.entry(id.clone()).or_insert((heartbeat, now));
        fd_live || heartbeat > first_heartbeat || now.duration_since(first_at) < SILENT_PEER_GRACE
    }
}

/// Every known node other than this one (any generation) that the tracker
/// counts as present, under a chitchat lock the caller holds.
fn present_peers<'a>(
    guard: &'a chitchat::Chitchat,
    self_id: &str,
    tracker: &mut PeerTracker,
) -> Vec<(&'a ChitchatId, &'a chitchat::NodeState)> {
    let live: HashSet<&ChitchatId> = guard.live_nodes().collect();
    let dead: HashSet<&ChitchatId> = guard.dead_nodes().collect();
    guard
        .node_states()
        .iter()
        .filter(|(id, _)| id.node_id != self_id)
        .filter(|(id, state)| {
            tracker.present(id, state.heartbeat(), live.contains(id), dead.contains(id))
        })
        .collect()
}

/// [`ClusterHandle::catch_up_status`] under a chitchat lock the caller holds,
/// with `tracker` deciding which known nodes are present (see
/// [`PeerTracker`]).
pub(crate) fn catch_up_status_locked(
    guard: &chitchat::Chitchat,
    self_id: &str,
    started_ms: u64,
    tracker: &mut PeerTracker,
) -> CatchUpStatus {
    let peers = present_peers(guard, self_id, tracker)
        .into_iter()
        .map(|(id, state)| {
            let seen = state.max_version() > 0;
            let publishes_marks = state.get(KV_CATCH_UP_KEY).is_some();
            let mark_ms = advertised_mark(state);
            PeerCatchUp {
                id: id.node_id.clone(),
                caught_up: peer_caught_up(seen, publishes_marks, mark_ms, started_ms),
                seen,
                publishes_marks,
                mark_ms,
            }
        })
        .collect();
    CatchUpStatus { peers }
}

/// The present peers (see [`PeerTracker`]; every one but this node, any
/// generation), with their advertised ports, under a chitchat lock the
/// caller holds. Sorted by id like [`ClusterHandle::nodes`].
pub(crate) fn present_peers_locked(
    guard: &chitchat::Chitchat,
    self_id: &str,
    tracker: &mut PeerTracker,
) -> Vec<NodeInfo> {
    let mut peers: Vec<NodeInfo> = present_peers(guard, self_id, tracker)
        .into_iter()
        .map(|(id, state)| NodeInfo {
            id: id.node_id.clone(),
            gossip_addr: id.gossip_advertise_addr.to_string(),
            state: NodeState::Alive,
            data_port: advertised_data_port(state),
            hrw_scheme: advertised_hrw_scheme(state),
            draining: advertised_draining(state),
        })
        .collect();
    peers.sort_by(|a, b| a.id.cmp(&b.id));
    peers
}

/// Liveness state of a cluster node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeState {
    /// Node is responding to gossip heartbeats.
    Alive,
    /// Node has missed enough heartbeats to be considered dead.
    Dead,
}

/// Handle for querying cluster membership.
///
/// Wraps a [`ChitchatHandle`] and exposes a clean API that does not
/// leak chitchat types into other crates.
pub struct ClusterHandle {
    pub(crate) handle: ChitchatHandle,
    self_id: String,
    cluster_id: String,
    gossip_addr: SocketAddr,
    /// This node's own KV data plane port (`[cluster.kv] data_port`).
    data_port: u16,
    /// Source of this node's KV write stamps (see
    /// [`crate::gossip_kv::Stamp`]).
    pub(crate) clock: crate::gossip_kv::StampClock,
    /// Bounded queue + sweeper for this node's gossip KV state.
    pub(crate) writer: Arc<crate::gossip_writer::GossipWriter>,
    /// The drain/sweep task; aborted on shutdown.
    writer_task: tokio::task::JoinHandle<()>,
    /// Wall-clock epoch milliseconds at which this node started gossip —
    /// the time a peer's [`KV_MARK_KEY`] must be past to prove catch-up.
    started_ms: u64,
    /// Set by [`ClusterHandle::mark_draining`]; mirrors [`DRAINING_KEY`] in
    /// this node's own gossip state.
    draining: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for ClusterHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterHandle")
            .field("self_id", &self.self_id)
            .field("cluster_id", &self.cluster_id)
            .field("gossip_addr", &self.gossip_addr)
            .finish_non_exhaustive()
    }
}

impl ClusterHandle {
    /// Return information about all known nodes (alive and dead).
    pub async fn nodes(&self) -> Vec<NodeInfo> {
        let chitchat = self.handle.chitchat();
        let guard = chitchat.lock().await;

        let live_ids: HashSet<ChitchatId> = guard.live_nodes().cloned().collect();
        let dead_ids: HashSet<ChitchatId> = guard.dead_nodes().cloned().collect();

        let mut nodes = Vec::with_capacity(live_ids.len() + dead_ids.len());

        for (ids, state) in [(&live_ids, NodeState::Alive), (&dead_ids, NodeState::Dead)] {
            for id in ids {
                let node_state = guard.node_state(id);
                nodes.push(NodeInfo {
                    id: id.node_id.clone(),
                    gossip_addr: id.gossip_advertise_addr.to_string(),
                    state,
                    data_port: node_state.and_then(advertised_data_port),
                    hrw_scheme: node_state.and_then(advertised_hrw_scheme),
                    draining: node_state.is_some_and(advertised_draining),
                });
            }
        }

        // Include self if not already in live/dead sets (always alive).
        let self_in_live = live_ids.iter().any(|id| id.node_id == self.self_id);
        let self_in_dead = dead_ids.iter().any(|id| id.node_id == self.self_id);
        if !self_in_live && !self_in_dead {
            nodes.push(self.self_node());
        }

        nodes.sort_by(|a, b| a.id.cmp(&b.id));
        nodes
    }

    /// Return this node's identity.
    #[must_use]
    pub fn self_node(&self) -> NodeInfo {
        NodeInfo {
            id: self.self_id.clone(),
            gossip_addr: self.gossip_addr.to_string(),
            state: NodeState::Alive,
            data_port: Some(self.data_port),
            hrw_scheme: Some(crate::sqlite_election::HRW_SCHEME),
            draining: self.is_draining(),
        }
    }

    /// Announce that this node is shutting down: set [`DRAINING_KEY`] in its
    /// gossip state, so every node (this one included, immediately; peers on
    /// their next gossip exchange, about a second) stops counting it as a
    /// candidate owner of per-site databases ([`crate::hrw_owner`]) and
    /// re-homes its sites to the next node in each site's HRW order. The node
    /// stays alive and keeps gossiping, so it is still listed, still asked for
    /// snapshots of the sites it held, and still reachable by the KV data
    /// plane. Permanent for this process; idempotent.
    pub async fn mark_draining(&self) {
        if self.draining.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let chitchat = self.handle.chitchat();
        let mut guard = chitchat.lock().await;
        guard.self_node_state().set(DRAINING_KEY, "1");
        drop(guard);
        tracing::info!(node_id = %self.self_id, "announced drain to the cluster");
    }

    /// Whether [`mark_draining`](Self::mark_draining) has run on this node.
    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.draining.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The address this node's KV data plane listens on: the gossip bind IP
    /// with `[cluster.kv] data_port`.
    #[must_use]
    pub fn kv_data_plane_addr(&self) -> SocketAddr {
        SocketAddr::new(self.gossip_addr.ip(), self.data_port)
    }

    /// Return the cluster identifier.
    #[must_use]
    pub fn cluster_id(&self) -> &str {
        &self.cluster_id
    }

    /// Return this node's gossip UDP bind address.
    ///
    /// Used by dependents that need to derive their own listen address
    /// from the gossip address, so operators only have to reason about
    /// one cluster port range: the KV data plane defaults to
    /// `gossip_port + 1` and the cluster channel to `gossip_port + 2`.
    #[must_use]
    pub fn gossip_socket_addr(&self) -> SocketAddr {
        self.gossip_addr
    }

    /// Return the number of currently alive nodes (including self).
    pub async fn live_node_count(&self) -> usize {
        let chitchat = self.handle.chitchat();
        let guard = chitchat.lock().await;
        guard.live_nodes().count()
    }

    /// Wall-clock epoch milliseconds at which this node started gossip.
    #[must_use]
    pub fn started_ms(&self) -> u64 {
        self.started_ms
    }

    /// How far this node has caught up on each alive peer's gossip state
    /// since it started (see [`KV_MARK_KEY`] for the mechanism).
    ///
    /// Peers are the alive nodes whose id differs from this node's: a
    /// previous generation of this very node — which peers keep as a dead
    /// state and still hand over — is not one. The answer is a snapshot under
    /// the chitchat lock; the catch-up gate polls it.
    pub async fn catch_up_status(&self) -> CatchUpStatus {
        let chitchat = self.handle.chitchat();
        let guard = chitchat.lock().await;
        // A fresh tracker: only the failure detector's live set counts as
        // peers here. The gate task keeps a tracker across polls and also
        // counts nodes whose heartbeat it has seen advance.
        catch_up_status_locked(&guard, &self.self_id, self.started_ms, &mut PeerTracker::default())
    }

    /// Gracefully shut down the gossip protocol.
    pub async fn shutdown(self) {
        self.writer_task.abort();
        let _ = self.handle.shutdown().await;
    }
}

/// Start the gossip protocol and return a handle.
///
/// Binds a UDP listener on `config.bind`, joins seed nodes from
/// `config.join`, and spawns the gossip background task.
///
/// When `config.secret` is non-empty, gossip runs over the
/// [`EncryptedUdpTransport`] — every datagram is sealed with
/// ChaCha20-Poly1305 and peers without the matching secret are
/// invisible. When empty, gossip is plaintext UDP and a warning is
/// logged recommending a secret on untrusted networks.
///
/// # Errors
///
/// Returns an error if the bind address is invalid or the UDP socket
/// fails to bind.
///
/// # Panics
///
/// Panics if the system clock is before the UNIX epoch.
pub async fn start_gossip(config: &ClusterConfig) -> anyhow::Result<ClusterHandle> {
    let node_id =
        if config.node_id.is_empty() { generate_node_id() } else { config.node_id.clone() };

    let listen_addr: SocketAddr = config
        .bind
        .parse()
        .with_context(|| format!("invalid cluster.bind address: {}", config.bind))?;

    let generation_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_secs();

    let chitchat_id = ChitchatId::new(node_id.clone(), generation_id, listen_addr);

    let chitchat_config = ChitchatConfig {
        chitchat_id,
        cluster_id: config.cluster_id.clone(),
        gossip_interval: Duration::from_secs(1),
        listen_addr,
        seed_nodes: config.join.clone(),
        failure_detector_config: FailureDetectorConfig::default(),
        marked_for_deletion_grace_period: Duration::from_secs(60),
        catchup_callback: None,
        extra_liveness_predicate: None,
    };

    // With a secret, every gossip datagram is sealed with a key derived
    // from it — nodes without the matching secret cannot join, read, or
    // inject. Without one, gossip is plaintext UDP.
    let encrypted = !config.secret.is_empty();
    let handle = if encrypted {
        let transport = EncryptedUdpTransport::new(&config.secret);
        spawn_chitchat(chitchat_config, vec![], &transport).await
    } else {
        tracing::warn!(
            "cluster.secret is not set (running with allow_insecure_no_auth) -- gossip and KV \
             data plane traffic is unauthenticated plaintext; set [cluster] secret when running \
             on untrusted networks"
        );
        spawn_chitchat(chitchat_config, vec![], &UdpTransport).await
    }
    .context("failed to spawn chitchat gossip")?;

    // Advertise where this node's KV data plane can be reached, so peers
    // dial the port this node actually listens on rather than their own.
    let data_port = config.kv.data_port;
    // Also advertise the per-site ownership hash scheme, so a peer running a
    // different one is detected (`spawn_hrw_scheme_watch`) instead of
    // silently disagreeing on every site's owner — and the catch-up keys:
    // the static declaration first (lowest version, so a peer's first delta
    // carries it) and an initial mark, which the gossip writer refreshes.
    let started_ms = crate::gossip_kv::now_epoch_ms();
    {
        let chitchat = handle.chitchat();
        let mut guard = chitchat.lock().await;
        let state = guard.self_node_state();
        state.set(KV_DATA_PORT_KEY, data_port.to_string());
        state.set(HRW_SCHEME_KEY, crate::sqlite_election::HRW_SCHEME.to_string());
        state.set(KV_CATCH_UP_KEY, "1");
        state.set(KV_MARK_KEY, started_ms.to_string());
    }

    tracing::info!(
        %node_id,
        %listen_addr,
        seeds = ?config.join,
        cluster_id = %config.cluster_id,
        encrypted,
        kv_data_port = data_port,
        "gossip started"
    );

    // The gossip KV writer: one task drains queued writes into chitchat
    // state in batches and sweeps this node's state for garbage.
    let writer = Arc::new(crate::gossip_writer::GossipWriter::new());
    let writer_task = tokio::spawn(Arc::clone(&writer).run(handle.chitchat(), node_id.clone()));

    Ok(ClusterHandle {
        handle,
        clock: crate::gossip_kv::StampClock::new(&node_id),
        self_id: node_id,
        cluster_id: config.cluster_id.clone(),
        gossip_addr: listen_addr,
        data_port,
        writer,
        writer_task,
        started_ms,
        draining: std::sync::atomic::AtomicBool::new(false),
    })
}

/// Generate a unique node ID from the hostname and a random suffix.
fn generate_node_id() -> String {
    let hostname = std::env::var("HOSTNAME").ok().unwrap_or_else(|| "unknown".to_string());

    let suffix: u32 = rand_suffix();
    format!("{hostname}-{suffix:08x}")
}

/// Simple random suffix using system time nanoseconds.
fn rand_suffix() -> u32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .subsec_nanos();
    // Mix in the process ID for uniqueness across simultaneous starts.
    nanos ^ (std::process::id() << 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: u64 = 1_700_000_000_000;

    #[test]
    fn peer_caught_up_rules() {
        let margin = u64::try_from(KV_MARK_SKEW_MARGIN.as_millis()).unwrap();
        // Nothing received yet: pending, whatever else is claimed.
        assert!(!peer_caught_up(false, false, None, START));
        assert!(!peer_caught_up(false, true, Some(START + margin), START));
        // Seen, and the peer never publishes marks: cannot be proven, not waited for.
        assert!(peer_caught_up(true, false, None, START));
        // Seen and marks declared: only a mark past start + margin proves it.
        assert!(!peer_caught_up(true, true, None, START));
        assert!(!peer_caught_up(true, true, Some(START - 1), START));
        assert!(!peer_caught_up(true, true, Some(START + margin - 1), START));
        assert!(peer_caught_up(true, true, Some(START + margin), START));
        assert!(peer_caught_up(true, true, Some(START + 60_000), START));
    }

    #[test]
    fn peer_tracker_presence_rules() {
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let id = ChitchatId::new("p".into(), 0, addr);
        let mut t = PeerTracker::<u64>::default();
        // Dead per the failure detector: never present.
        assert!(!t.present(&id, 5, false, true));
        // First sighting, unclassified: present for the silent grace.
        assert!(t.present(&id, 5, false, false));
        // Same heartbeat, still inside the grace: present.
        assert!(t.present(&id, 5, false, false));
        // Heartbeat advanced: present regardless of the detector.
        assert!(t.present(&id, 6, false, false));
        // Live per the detector: present.
        assert!(t.present(&id, 6, true, false));
        // A node first seen long ago whose heartbeat never moved: absent.
        let stale = ChitchatId::new("ghost".into(), 0, addr);
        t.first_seen.insert(stale.clone(), (9, std::time::Instant::now() - SILENT_PEER_GRACE * 2));
        assert!(!t.present(&stale, 9, false, false));
        assert!(t.present(&stale, 10, false, false), "a heartbeat update revives it");
    }

    #[test]
    fn catch_up_status_is_complete_only_when_every_peer_is() {
        let peer = |id: &str, caught_up| PeerCatchUp {
            id: id.to_string(),
            caught_up,
            seen: true,
            publishes_marks: true,
            mark_ms: None,
        };
        assert!(CatchUpStatus::default().complete(), "no peers: vacuously complete");
        let status = CatchUpStatus { peers: vec![peer("a", true), peer("b", false)] };
        assert!(!status.complete());
        assert_eq!(status.pending(), vec!["b"]);
        let status = CatchUpStatus { peers: vec![peer("a", true), peer("b", true)] };
        assert!(status.complete());
        assert_eq!(status.pending(), Vec::<&str>::new());
    }
}
