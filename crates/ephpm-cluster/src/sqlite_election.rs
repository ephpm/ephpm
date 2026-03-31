//! Primary election for clustered SQLite (sqld).
//!
//! Uses the gossip KV tier to elect a primary node for sqld replication.
//! The lowest-ordinal alive node wins. The primary heartbeats its claim
//! every 5 seconds with a 10-second TTL. On primary failure, the next
//! lowest-ordinal node promotes itself.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::{ClusterHandle, NodeState};

/// Gossip KV key for the primary node identity.
const PRIMARY_KEY: &str = "sqlite:primary";

/// How often the primary refreshes its claim.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// TTL for the primary claim. If not refreshed, the key expires and
/// triggers re-election.
const PRIMARY_TTL: Duration = Duration::from_secs(10);

/// The elected role for this node's sqld instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElectedRole {
    /// This node is the primary — sqld accepts writes and serves WAL frames.
    Primary,
    /// This node is a replica — sqld syncs from the primary via gRPC.
    Replica {
        /// gRPC URL of the primary node.
        primary_grpc_url: String,
    },
}

/// Value stored in gossip KV for the primary claim.
///
/// Format: `"{node_id}|{grpc_addr}"`.
#[derive(Debug, Clone)]
struct PrimaryClaim {
    node_id: String,
    grpc_addr: String,
}

impl PrimaryClaim {
    fn encode(&self) -> Vec<u8> {
        format!("{}|{}", self.node_id, self.grpc_addr).into_bytes()
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let s = std::str::from_utf8(bytes).ok()?;
        let (node_id, grpc_addr) = s.split_once('|')?;
        Some(Self {
            node_id: node_id.to_string(),
            grpc_addr: grpc_addr.to_string(),
        })
    }
}

/// Manages primary election for sqld via gossip KV.
///
/// Spawn the [`run`](Self::run) method as a tokio task. Watch for role
/// changes via [`watch_role`](Self::watch_role).
pub struct SqliteElection {
    cluster: Arc<ClusterHandle>,
    grpc_listen: String,
    role_tx: watch::Sender<ElectedRole>,
    role_rx: watch::Receiver<ElectedRole>,
}

impl SqliteElection {
    /// Create a new election manager.
    ///
    /// `grpc_listen` is this node's sqld gRPC address that replicas will
    /// connect to if this node becomes primary.
    pub fn new(cluster: Arc<ClusterHandle>, grpc_listen: String) -> Self {
        // Start as replica with empty URL — will be resolved on first tick.
        let (role_tx, role_rx) = watch::channel(ElectedRole::Replica {
            primary_grpc_url: String::new(),
        });

        Self {
            cluster,
            grpc_listen,
            role_tx,
            role_rx,
        }
    }

    /// Get a receiver for role changes.
    ///
    /// The integration layer watches this to restart sqld when the role
    /// changes (e.g., replica promoted to primary on failover).
    #[must_use]
    pub fn watch_role(&self) -> watch::Receiver<ElectedRole> {
        self.role_rx.clone()
    }

    /// Determine the initial role by checking existing gossip state.
    ///
    /// Should be called once before starting the election loop.
    pub async fn determine_initial_role(&self) -> ElectedRole {
        // Check if there's already a primary claim in gossip.
        if let Some(bytes) = self.cluster.gossip_get(PRIMARY_KEY).await {
            if let Some(claim) = PrimaryClaim::decode(&bytes) {
                if claim.node_id != self.cluster.self_node().id {
                    return ElectedRole::Replica {
                        primary_grpc_url: format!("http://{}", claim.grpc_addr),
                    };
                }
            }
        }

        // No valid claim exists — check if we should be primary.
        if self.should_be_primary().await {
            ElectedRole::Primary
        } else {
            // No primary yet and we're not lowest ordinal — wait.
            ElectedRole::Replica {
                primary_grpc_url: String::new(),
            }
        }
    }

    /// Run the election loop. This should be spawned as a tokio task.
    ///
    /// - If primary: heartbeats the claim every 5 seconds.
    /// - Periodically checks if the primary claim has expired and
    ///   re-evaluates whether this node should promote.
    pub async fn run(self) {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);

        loop {
            interval.tick().await;

            let current_role = self.role_rx.borrow().clone();
            let new_role = self.evaluate_role().await;

            if new_role != current_role {
                tracing::info!(
                    old = ?current_role,
                    new = ?new_role,
                    "SQLite election: role changed"
                );
                // Ignore send errors — receiver may have been dropped.
                let _ = self.role_tx.send(new_role);
            }
        }
    }

    /// Evaluate what role this node should have right now.
    async fn evaluate_role(&self) -> ElectedRole {
        let self_node = self.cluster.self_node();

        // Check existing primary claim.
        if let Some(bytes) = self.cluster.gossip_get(PRIMARY_KEY).await {
            if let Some(claim) = PrimaryClaim::decode(&bytes) {
                if claim.node_id == self_node.id {
                    // We are the primary — refresh heartbeat.
                    self.publish_claim().await;
                    return ElectedRole::Primary;
                }

                // Someone else claims primary — check if they're alive.
                let nodes = self.cluster.nodes().await;
                let primary_alive = nodes
                    .iter()
                    .any(|n| n.id == claim.node_id && n.state == NodeState::Alive);

                if primary_alive {
                    return ElectedRole::Replica {
                        primary_grpc_url: format!("http://{}", claim.grpc_addr),
                    };
                }

                // Primary is dead — fall through to re-election.
                tracing::warn!(
                    dead_primary = %claim.node_id,
                    "primary node is dead, triggering re-election"
                );
            }
        }

        // No valid primary claim — elect.
        if self.should_be_primary().await {
            self.publish_claim().await;
            tracing::info!(
                node_id = %self_node.id,
                "elected as SQLite primary"
            );
            ElectedRole::Primary
        } else {
            // Not our turn — wait for the rightful primary to claim.
            ElectedRole::Replica {
                primary_grpc_url: String::new(),
            }
        }
    }

    /// Check if this node is the lowest-ordinal alive node (and should be primary).
    async fn should_be_primary(&self) -> bool {
        let self_id = &self.cluster.self_node().id;
        let nodes = self.cluster.nodes().await;

        let lowest_alive = nodes
            .iter()
            .filter(|n| n.state == NodeState::Alive)
            .min_by(|a, b| a.id.cmp(&b.id));

        lowest_alive.is_some_and(|n| &n.id == self_id)
    }

    /// Publish this node's primary claim to the gossip KV tier.
    async fn publish_claim(&self) {
        let claim = PrimaryClaim {
            node_id: self.cluster.self_node().id.clone(),
            grpc_addr: self.grpc_listen.clone(),
        };
        self.cluster
            .gossip_set(PRIMARY_KEY, &claim.encode(), Some(PRIMARY_TTL))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_claim_roundtrip() {
        let claim = PrimaryClaim {
            node_id: "ephpm-0".into(),
            grpc_addr: "10.0.1.2:5001".into(),
        };
        let encoded = claim.encode();
        let decoded = PrimaryClaim::decode(&encoded).unwrap();
        assert_eq!(decoded.node_id, "ephpm-0");
        assert_eq!(decoded.grpc_addr, "10.0.1.2:5001");
    }

    #[test]
    fn primary_claim_decode_invalid() {
        assert!(PrimaryClaim::decode(b"no-pipe-here").is_none());
        assert!(PrimaryClaim::decode(b"").is_none());
    }

    #[test]
    fn primary_claim_encode_format() {
        let claim = PrimaryClaim {
            node_id: "node-1".into(),
            grpc_addr: "0.0.0.0:5001".into(),
        };
        let bytes = claim.encode();
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            "node-1|0.0.0.0:5001"
        );
    }

    #[test]
    fn elected_role_equality() {
        assert_eq!(ElectedRole::Primary, ElectedRole::Primary);
        assert_ne!(
            ElectedRole::Primary,
            ElectedRole::Replica {
                primary_grpc_url: "http://x:5001".into(),
            }
        );
        assert_eq!(
            ElectedRole::Replica {
                primary_grpc_url: "http://x:5001".into(),
            },
            ElectedRole::Replica {
                primary_grpc_url: "http://x:5001".into(),
            }
        );
    }
}
