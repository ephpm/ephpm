//! Primary election for clustered `SQLite`.
//!
//! Uses the gossip KV tier to elect a primary node for replication. As of
//! v0.7.0 the consumer is the in-process Turso CDC path (`turso_cdc.rs`); the
//! election itself is engine-agnostic, and `ElectedRole::Replica`'s
//! `primary_grpc_url` field carries whatever address peers must dial to reach
//! the primary (the primary's cluster-channel address in the CDC path).
//! The lowest-ordinal alive node wins. The primary heartbeats its claim
//! every 5 seconds with a 10-second TTL. On primary failure, the next
//! lowest-ordinal node promotes itself.
//!
//! # Incumbent protection (issue #314)
//!
//! Two rules keep a node that *joins* a running cluster from taking the
//! primary role away from a healthy incumbent (observed in the wild: a cold
//! node re-rooted the cluster onto its own empty database):
//!
//! 1. **Startup grace.** A freshly started node must not publish a first
//!    primary claim until [`STARTUP_GRACE`] has elapsed since boot. At boot,
//!    gossip has not converged: `cluster.nodes()` holds only the local node
//!    (so "lowest-ordinal alive" is trivially true for everyone) and an
//!    incumbent's claim may not have gossiped over yet. The grace window is
//!    longer than claim TTL + heartbeat, so a live incumbent's claim is
//!    guaranteed multiple opportunities to arrive before this node may
//!    self-elect. A restarting incumbent whose own (unexpired) claim is
//!    still in gossip reclaims immediately — that is not a theft.
//! 2. **Deterministic conflict resolution.** The gossip KV tier is
//!    last-write-wins, so two simultaneous claimants would otherwise
//!    flip-flop on every heartbeat. If this node currently holds the
//!    primary role and sees a *live* foreign claim, the tie is broken by
//!    node id — lowest wins (the documented election rule) — instead of
//!    silently yielding to whoever wrote last. With rule 1 in place this
//!    only triggers when two nodes genuinely elected concurrently (e.g.
//!    a symmetric partition heal), where either database is as good as
//!    the other and determinism is what matters.

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

/// How long a freshly started node must wait before it may publish its
/// *first* primary claim (issue #314 — see the module docs).
///
/// Must exceed `PRIMARY_TTL + HEARTBEAT_INTERVAL`: a live incumbent
/// refreshes its claim every [`HEARTBEAT_INTERVAL`], and gossip delivers a
/// KV write in low single-digit seconds, so within this window a joining
/// node is guaranteed to observe the incumbent's claim if one exists.
/// The cost is that a genuinely fresh cluster elects its first primary
/// ~15s after boot instead of immediately.
const STARTUP_GRACE: Duration =
    Duration::from_secs(PRIMARY_TTL.as_secs() + HEARTBEAT_INTERVAL.as_secs());

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
        Some(Self { node_id: node_id.to_string(), grpc_addr: grpc_addr.to_string() })
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
    /// When this election manager was created. Gates the first
    /// self-election behind [`STARTUP_GRACE`] (issue #314).
    boot: tokio::time::Instant,
}

impl SqliteElection {
    /// Create a new election manager.
    ///
    /// `grpc_listen` is this node's sqld gRPC address that replicas will
    /// connect to if this node becomes primary.
    #[must_use]
    pub fn new(cluster: Arc<ClusterHandle>, grpc_listen: String) -> Self {
        // Start as replica with empty URL — will be resolved on first tick.
        let (role_tx, role_rx) =
            watch::channel(ElectedRole::Replica { primary_grpc_url: String::new() });

        Self { cluster, grpc_listen, role_tx, role_rx, boot: tokio::time::Instant::now() }
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
    ///
    /// A fresh node never returns `Primary` from here unless the existing
    /// gossip claim already names *this* node (a fast restart of the
    /// incumbent, within the claim TTL). At boot, gossip has not converged
    /// — the membership view may hold only the local node and an
    /// incumbent's claim may not have arrived yet — so deciding "I am the
    /// lowest-ordinal alive node, therefore primary" here is exactly the
    /// role-theft bug of issue #314. The election loop ([`run`](Self::run))
    /// makes the first claim instead, after [`STARTUP_GRACE`].
    ///
    /// The decision is also published to the role watch channel so
    /// [`watch_role`](Self::watch_role) receivers and the loop's own
    /// current-role view agree with the value returned here.
    pub async fn determine_initial_role(&self) -> ElectedRole {
        let role = self.initial_role_inner().await;
        self.role_tx.send_replace(role.clone());
        role
    }

    async fn initial_role_inner(&self) -> ElectedRole {
        // Check if there's already a primary claim in gossip.
        if let Some(bytes) = self.cluster.gossip_get(PRIMARY_KEY).await
            && let Some(claim) = PrimaryClaim::decode(&bytes)
        {
            if claim.node_id == self.cluster.self_node().id {
                // Our own (unexpired) claim survived a restart —
                // reclaiming is not a theft; refresh and carry on.
                self.publish_claim().await;
                tracing::info!("SQLite election: reclaiming our own surviving primary claim");
                return ElectedRole::Primary;
            }
            if let Some(url) = self.replica_url_for(&claim).await {
                return ElectedRole::Replica { primary_grpc_url: url };
            }
            // Claim points at a host that is not a known member.
            // Refuse to dial it (defense in depth against a forged
            // gossip claim) and wait for a valid one.
            return ElectedRole::Replica { primary_grpc_url: String::new() };
        }

        // No valid claim visible. That means either the cluster genuinely
        // has no primary, or gossip simply has not delivered the
        // incumbent's claim yet — indistinguishable this early. Do NOT
        // claim: start as an unresolved replica and let the election loop
        // claim after the startup grace (issue #314).
        ElectedRole::Replica { primary_grpc_url: String::new() }
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
            let new_role = self.evaluate_role(&current_role).await;

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
    ///
    /// `current` is the role this node currently holds (from the watch
    /// channel). It matters in exactly one place: when a *live* foreign
    /// claim conflicts with our own primary role, the tie is broken
    /// deterministically instead of by whoever gossiped last (issue #314).
    async fn evaluate_role(&self, current: &ElectedRole) -> ElectedRole {
        let self_node = self.cluster.self_node();

        // Check existing primary claim.
        if let Some(bytes) = self.cluster.gossip_get(PRIMARY_KEY).await
            && let Some(claim) = PrimaryClaim::decode(&bytes)
        {
            if claim.node_id == self_node.id {
                // We are the primary — refresh heartbeat.
                self.publish_claim().await;
                return ElectedRole::Primary;
            }

            // Someone else claims primary -- check if they're alive AND
            // that the advertised gRPC address belongs to a known member
            // (defense in depth: a forged claim from a plaintext gossip
            // injection must not make us dial an arbitrary host).
            let nodes = self.cluster.nodes().await;
            let primary_alive =
                nodes.iter().any(|n| n.id == claim.node_id && n.state == NodeState::Alive);

            if primary_alive {
                // Conflict: we hold the primary role, yet a live peer
                // claims it too (gossip KV is last-write-wins, so its
                // newer write shadows ours). Do not yield
                // unconditionally — that is how a joining node stole
                // the role from a healthy incumbent. Break the tie by
                // the documented rule: lowest node id wins.
                if matches!(current, ElectedRole::Primary) {
                    if incumbent_wins_tie(&self_node.id, &claim.node_id) {
                        tracing::warn!(
                            claimant = %claim.node_id,
                            "SQLite election: live conflicting primary claim from a \
                             higher-ordinal node; keeping the primary role and \
                             re-asserting our claim (lowest node id wins)"
                        );
                        self.publish_claim().await;
                        return ElectedRole::Primary;
                    }
                    tracing::warn!(
                        claimant = %claim.node_id,
                        "SQLite election: live conflicting primary claim from a \
                         lower-ordinal node; stepping down (lowest node id wins)"
                    );
                }
                if let Some(url) = self.replica_url_for(&claim).await {
                    return ElectedRole::Replica { primary_grpc_url: url };
                }
                // Alive primary but the gRPC host is not a known member --
                // refuse to dial it and wait for a valid claim.
                return ElectedRole::Replica { primary_grpc_url: String::new() };
            }

            // Primary is dead — fall through to re-election.
            tracing::warn!(
                dead_primary = %claim.node_id,
                "primary node is dead, triggering re-election"
            );
        }

        // No valid primary claim visible. If this node started recently,
        // that absence is not evidence there is no primary — gossip may
        // simply not have delivered the incumbent's claim yet. Defer the
        // first self-election until the startup grace has passed
        // (issue #314). A node that already holds the primary role is by
        // definition past its first election and is not gated.
        if !matches!(current, ElectedRole::Primary) && self.boot.elapsed() < STARTUP_GRACE {
            tracing::debug!(
                elapsed_ms = self.boot.elapsed().as_millis(),
                grace_ms = STARTUP_GRACE.as_millis(),
                "SQLite election: no primary claim visible, but within the startup \
                 grace window — deferring self-election until gossip has converged"
            );
            return ElectedRole::Replica { primary_grpc_url: String::new() };
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
            ElectedRole::Replica { primary_grpc_url: String::new() }
        }
    }

    /// Check if this node is the lowest-ordinal alive node (and should be primary).
    async fn should_be_primary(&self) -> bool {
        let self_id = &self.cluster.self_node().id;
        let nodes = self.cluster.nodes().await;

        let lowest_alive =
            nodes.iter().filter(|n| n.state == NodeState::Alive).min_by(|a, b| a.id.cmp(&b.id));

        lowest_alive.is_some_and(|n| &n.id == self_id)
    }

    /// Validate a primary claim and build the replica gRPC URL for it.
    ///
    /// Returns `Some(url)` only when the claim's advertised `grpc_addr`
    /// host matches a currently-known gossip member's address. This is
    /// defense in depth on top of the mandatory cluster secret (see
    /// `ClusterConfig::ensure_secure`): even if an attacker managed to
    /// inject a claim into gossip, a replica will not dial a `host:port`
    /// that does not belong to a live cluster member (blocks SSRF and
    /// pointing replication at an attacker-controlled sqld).
    ///
    /// The comparison is by host only, because a node's gossip address
    /// and its sqld gRPC address use different ports on the same host.
    async fn replica_url_for(&self, claim: &PrimaryClaim) -> Option<String> {
        let claim_host = host_of(&claim.grpc_addr)?;
        let nodes = self.cluster.nodes().await;
        let known = nodes.iter().filter_map(|n| host_of(&n.gossip_addr));

        if member_hosts_contain(known, &claim_host) {
            Some(format!("http://{}", claim.grpc_addr))
        } else {
            tracing::warn!(
                primary = %claim.node_id,
                grpc_addr = %claim.grpc_addr,
                "refusing to dial SQLite primary: advertised gRPC host is not a known cluster \
                 member (possible forged gossip claim)"
            );
            None
        }
    }

    /// Publish this node's primary claim to the gossip KV tier.
    async fn publish_claim(&self) {
        let claim = PrimaryClaim {
            node_id: self.cluster.self_node().id.clone(),
            grpc_addr: self.grpc_listen.clone(),
        };
        self.cluster.gossip_set(PRIMARY_KEY, &claim.encode(), Some(PRIMARY_TTL)).await;
    }
}

/// Deterministic resolution for two live, conflicting primary claims:
/// the incumbent keeps the role iff its node id sorts strictly lower than
/// the claimant's — the same "lowest ordinal wins" rule the election uses
/// for a fresh cluster, applied to conflicts (issue #314).
fn incumbent_wins_tie(self_id: &str, claimant_id: &str) -> bool {
    self_id < claimant_id
}

/// Extract the host portion of a `host:port` (or `[ipv6]:port`) address.
///
/// Returns `None` if the string has no port separator. Parsing as a
/// [`SocketAddr`](std::net::SocketAddr) first canonicalizes IPv6 forms
/// (e.g. `[::1]` vs `[0:0:...:1]`) so two spellings of the same address
/// compare equal; a bare `host:port` that is not a literal socket addr
/// falls back to splitting on the last colon.
fn host_of(addr: &str) -> Option<String> {
    if let Ok(sock) = addr.parse::<std::net::SocketAddr>() {
        return Some(sock.ip().to_string());
    }
    // Not a literal IP:port (e.g. a DNS name). Split off the final `:port`.
    let (host, _port) = addr.rsplit_once(':')?;
    if host.is_empty() { None } else { Some(host.to_string()) }
}

/// Whether any known member host equals `claim_host`.
fn member_hosts_contain<I>(mut members: I, claim_host: &str) -> bool
where
    I: Iterator<Item = String>,
{
    members.any(|h| h == claim_host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_claim_roundtrip() {
        let claim = PrimaryClaim { node_id: "ephpm-0".into(), grpc_addr: "10.0.1.2:5001".into() };
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
        let claim = PrimaryClaim { node_id: "node-1".into(), grpc_addr: "0.0.0.0:5001".into() };
        let bytes = claim.encode();
        assert_eq!(std::str::from_utf8(&bytes).unwrap(), "node-1|0.0.0.0:5001");
    }

    #[test]
    fn elected_role_equality() {
        assert_eq!(ElectedRole::Primary, ElectedRole::Primary);
        assert_ne!(
            ElectedRole::Primary,
            ElectedRole::Replica { primary_grpc_url: "http://x:5001".into() }
        );
        assert_eq!(
            ElectedRole::Replica { primary_grpc_url: "http://x:5001".into() },
            ElectedRole::Replica { primary_grpc_url: "http://x:5001".into() }
        );
    }

    #[test]
    fn elected_role_different_replicas_not_equal() {
        assert_ne!(
            ElectedRole::Replica { primary_grpc_url: "http://a:5001".into() },
            ElectedRole::Replica { primary_grpc_url: "http://b:5001".into() }
        );
    }

    #[test]
    fn primary_claim_with_ipv6() {
        let claim = PrimaryClaim { node_id: "node-v6".into(), grpc_addr: "[::1]:5001".into() };
        let encoded = claim.encode();
        let decoded = PrimaryClaim::decode(&encoded).unwrap();
        assert_eq!(decoded.node_id, "node-v6");
        assert_eq!(decoded.grpc_addr, "[::1]:5001");
    }

    #[test]
    fn primary_claim_decode_multiple_pipes() {
        // Only the first pipe is the separator.
        let decoded = PrimaryClaim::decode(b"a|b|c").unwrap();
        assert_eq!(decoded.node_id, "a");
        assert_eq!(decoded.grpc_addr, "b|c");
    }

    #[test]
    fn primary_claim_with_long_node_id() {
        let long_id = "ephpm-".to_string() + &"x".repeat(200);
        let claim = PrimaryClaim { node_id: long_id.clone(), grpc_addr: "10.0.1.2:5001".into() };
        let roundtripped = PrimaryClaim::decode(&claim.encode()).unwrap();
        assert_eq!(roundtripped.node_id, long_id);
    }

    /// Verify that lowest-ordinal wins: when comparing node IDs
    /// alphabetically, the smallest should become primary.
    #[test]
    fn lowest_ordinal_election_logic() {
        // Simulate the election logic: filter alive, pick min by id.
        let nodes = [
            ("ephpm-c", true),
            ("ephpm-a", true),
            ("ephpm-b", false), // dead
            ("ephpm-d", true),
        ];

        let lowest_alive =
            nodes.iter().filter(|(_, alive)| *alive).min_by(|a, b| a.0.cmp(b.0)).map(|(id, _)| *id);

        assert_eq!(lowest_alive, Some("ephpm-a"));
    }

    /// Verify that when the primary dies, the next lowest becomes primary.
    #[test]
    fn failover_to_next_lowest() {
        let nodes = [
            ("ephpm-a", false), // was primary, now dead
            ("ephpm-b", true),
            ("ephpm-c", true),
        ];

        let lowest_alive =
            nodes.iter().filter(|(_, alive)| *alive).min_by(|a, b| a.0.cmp(b.0)).map(|(id, _)| *id);

        assert_eq!(lowest_alive, Some("ephpm-b"));
    }

    #[test]
    fn host_of_ipv4_socket() {
        assert_eq!(host_of("10.0.1.2:5001").as_deref(), Some("10.0.1.2"));
    }

    #[test]
    fn host_of_ipv6_socket_canonicalizes() {
        // Both spellings of loopback must yield the same host string so a
        // member advertised one way still matches a claim spelled another.
        let a = host_of("[::1]:5001").unwrap();
        let b = host_of("[0:0:0:0:0:0:0:1]:7946").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn host_of_dns_name_splits_last_colon() {
        assert_eq!(host_of("node-a.internal:5001").as_deref(), Some("node-a.internal"));
    }

    #[test]
    fn host_of_no_port_is_none() {
        assert_eq!(host_of("10.0.1.2"), None);
        assert_eq!(host_of(""), None);
    }

    #[test]
    fn member_validation_accepts_known_host() {
        // A claim's gRPC host (different port) matching a member's gossip
        // host is accepted.
        let members = ["10.0.1.2:7946".to_string(), "10.0.1.3:7946".to_string()];
        let claim_host = host_of("10.0.1.2:5001").unwrap();
        let known = members.iter().filter_map(|m| host_of(m));
        assert!(member_hosts_contain(known, &claim_host));
    }

    #[test]
    fn member_validation_rejects_unknown_host() {
        // A forged claim pointing at an attacker host not in the member
        // list must be rejected.
        let members = ["10.0.1.2:7946".to_string(), "10.0.1.3:7946".to_string()];
        let claim_host = host_of("6.6.6.6:5001").unwrap();
        let known = members.iter().filter_map(|m| host_of(m));
        assert!(!member_hosts_contain(known, &claim_host));
    }

    #[test]
    fn heartbeat_ttl_constants_valid() {
        // TTL must be greater than heartbeat interval for liveness detection.
        assert!(PRIMARY_TTL > HEARTBEAT_INTERVAL);
        // The ratio should allow at least one missed heartbeat.
        assert!(PRIMARY_TTL >= HEARTBEAT_INTERVAL * 2);
    }

    /// The startup grace must outlast a full claim TTL plus one heartbeat:
    /// only then is a joining node guaranteed to have had the chance to
    /// observe a live incumbent's (re-published) claim before it may
    /// self-elect. Issue #314.
    #[test]
    fn startup_grace_covers_claim_ttl_and_a_heartbeat() {
        assert!(STARTUP_GRACE >= PRIMARY_TTL + HEARTBEAT_INTERVAL);
    }

    /// Conflicting live claims resolve by node id, lowest wins — in both
    /// directions, so exactly one of the two conflicting nodes keeps the
    /// role. Issue #314.
    #[test]
    fn conflicting_claims_resolve_to_lowest_node_id() {
        // Observed field failure: cdc-node-3 (joiner) took the role from
        // cdc-node-2 (incumbent). The incumbent must win this tie...
        assert!(incumbent_wins_tie("cdc-node-2", "cdc-node-3"));
        // ...and symmetrically, if the *joiner* somehow held the role, it
        // must yield to the lower-ordinal claimant.
        assert!(!incumbent_wins_tie("cdc-node-3", "cdc-node-2"));
        // Equal ids never conflict (that is the refresh-own-claim path),
        // but the tie-break must not let an equal id "win" as incumbent
        // AND as claimant on two nodes at once.
        assert!(!incumbent_wins_tie("cdc-node-1", "cdc-node-1"));
    }
}
