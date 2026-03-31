//! Cluster node management and gossip lifecycle.
//!
//! Wraps the [`chitchat`] crate to provide a clean, chitchat-agnostic API
//! surface for the rest of ePHPm.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use chitchat::transport::UdpTransport;
use chitchat::{
    ChitchatConfig, ChitchatHandle, ChitchatId, FailureDetectorConfig, spawn_chitchat,
};
use ephpm_config::ClusterConfig;

/// Information about a single cluster node.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NodeInfo {
    /// Unique node identifier.
    pub id: String,
    /// Gossip UDP address (`host:port`).
    pub gossip_addr: String,
    /// Whether the node is alive or dead (per failure detector).
    pub state: NodeState,
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
            });
        }

        for id in &dead_ids {
            nodes.push(NodeInfo {
                id: id.node_id.clone(),
                gossip_addr: id.gossip_advertise_addr.to_string(),
                state: NodeState::Dead,
            });
        }

        // Include self if not already in live/dead sets (always alive).
        let self_in_live = live_ids
            .iter()
            .any(|id| id.node_id == self.self_id);
        let self_in_dead = dead_ids
            .iter()
            .any(|id| id.node_id == self.self_id);
        if !self_in_live && !self_in_dead {
            nodes.push(NodeInfo {
                id: self.self_id.clone(),
                gossip_addr: self.gossip_addr.to_string(),
                state: NodeState::Alive,
            });
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
        }
    }

    /// Return the cluster identifier.
    #[must_use]
    pub fn cluster_id(&self) -> &str {
        &self.cluster_id
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
/// # Errors
///
/// Returns an error if the bind address is invalid or the UDP socket
/// fails to bind.
///
/// # Panics
///
/// Panics if the system clock is before the UNIX epoch.
pub async fn start_gossip(config: &ClusterConfig) -> anyhow::Result<ClusterHandle> {
    let node_id = if config.node_id.is_empty() {
        generate_node_id()
    } else {
        config.node_id.clone()
    };

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

    let handle = spawn_chitchat(chitchat_config, vec![], &UdpTransport)
        .await
        .context("failed to spawn chitchat gossip")?;

    tracing::info!(
        %node_id,
        %listen_addr,
        seeds = ?config.join,
        cluster_id = %config.cluster_id,
        "gossip started"
    );

    Ok(ClusterHandle {
        handle,
        self_id: node_id,
        cluster_id: config.cluster_id.clone(),
        gossip_addr: listen_addr,
    })
}

/// Generate a unique node ID from the hostname and a random suffix.
fn generate_node_id() -> String {
    let hostname = std::env::var("HOSTNAME")
        .ok()
        .unwrap_or_else(|| "unknown".to_string());

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
