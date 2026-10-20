//! Cluster node management and gossip lifecycle.
//!
//! Wraps the [`chitchat`] crate to provide a clean, chitchat-agnostic API
//! surface for the rest of ePHPm.

use std::collections::HashSet;
use std::net::SocketAddr;
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
}

impl NodeInfo {
    /// A node known only by id and gossip address — no advertised data port.
    #[must_use]
    pub fn new(id: impl Into<String>, gossip_addr: impl Into<String>, state: NodeState) -> Self {
        Self { id: id.into(), gossip_addr: gossip_addr.into(), state, data_port: None }
    }
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

        for id in &live_ids {
            nodes.push(NodeInfo {
                id: id.node_id.clone(),
                gossip_addr: id.gossip_advertise_addr.to_string(),
                state: NodeState::Alive,
                data_port: guard.node_state(id).and_then(advertised_data_port),
            });
        }

        for id in &dead_ids {
            nodes.push(NodeInfo {
                id: id.node_id.clone(),
                gossip_addr: id.gossip_advertise_addr.to_string(),
                state: NodeState::Dead,
                data_port: guard.node_state(id).and_then(advertised_data_port),
            });
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
        }
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

    /// Gracefully shut down the gossip protocol.
    pub async fn shutdown(self) {
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
    handle.chitchat().lock().await.self_node_state().set(KV_DATA_PORT_KEY, data_port.to_string());

    tracing::info!(
        %node_id,
        %listen_addr,
        seeds = ?config.join,
        cluster_id = %config.cluster_id,
        encrypted,
        kv_data_port = data_port,
        "gossip started"
    );

    Ok(ClusterHandle {
        handle,
        self_id: node_id,
        cluster_id: config.cluster_id.clone(),
        gossip_addr: listen_addr,
        data_port,
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
