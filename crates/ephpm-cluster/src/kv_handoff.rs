//! Peer handoff of large KV values: push on drain, pull on start.
//!
//! Small values (at or below `[cluster.kv] small_key_threshold`) live in
//! the gossip tier and reach every node on their own; a restarted node gets
//! them back from chitchat's state sync. Large values do not: they are
//! copied to `replication_factor` replicas **at write time only** (see
//! [`crate::clustered_store`]), and nothing rebalances them afterwards.
//! Two consequences a restart used to expose:
//!
//! 1. When node A leaves, every key whose replica set contained A has one
//!    copy fewer until it is next written; with `replication_factor = 1`
//!    (or a replica write that failed) the key is gone.
//! 2. When A comes back, it is again in the replica set of ~1/N of the
//!    large keys — but holds none of them, and `fetch_through` does not
//!    fetch for a node that is itself a replica. Those keys miss on A until
//!    rewritten.
//!
//! [`PeerHandoff::push`] closes (1): on graceful shutdown, after the HTTP
//! drain, the node sends every large value it holds to the nodes that form
//! the key's replica set **without it**, as stamped SETs ([`OP_SET_V2`]).
//! [`PeerHandoff::pull`] closes (2): on startup, before serving, the node
//! asks every alive peer for its share — the peer walks its own large
//! values and streams the ones whose replica set, computed over the peer's
//! alive view **plus the requester**, includes the requester
//! ([`OP_SHARE`]). Computing membership with the requester forced in makes
//! the answer independent of gossip timing: a peer that has not yet seen
//! the newcomer as alive still answers correctly.
//!
//! # Ordering, stated exactly
//!
//! Every handed-off value lands through the receiver's applied-stamp map
//! ([`KvRouter::apply_stamped`]), with the stamp the sender recorded for the
//! key when it has one (the key was written within the applied horizon), or
//! else a stamp that **loses to every real write** (`write = 0`). So a write
//! that reaches the receiver before, during or after the handoff — from a
//! client, gossip or a replica push — is never overwritten by the older
//! handed-off copy: the handoff only ever fills a slot the receiver has no
//! newer record for. The one way a handoff can install a stale value is a
//! key the receiver wrote *more than the applied horizon ago* and nobody
//! has touched since, whose copy on the sender was older than the
//! receiver's own — which requires the sender to have missed that write's
//! replication in the first place.
//!
//! A snapshot-restored entry carries no stamp (fresh process, empty map), so
//! a pulled copy from a peer that stayed online replaces it — the peer's is
//! the one the cluster kept writing to.
//!
//! [`OP_SET_V2`]: crate::kv_data_plane
//! [`OP_SHARE`]: crate::kv_data_plane
//! [`KvRouter::apply_stamped`]: crate::kv_data_plane::KvRouter::apply_stamped

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ephpm_config::ClusterKvConfig;
use ephpm_kv::store::Store;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::clustered_store::{hash_key, node_data_addr, replica_nodes_for};
use crate::gossip_kv::Stamp;
use crate::kv_data_plane::{DataPlaneClient, KvRouter, ShareEntry};
use crate::node::{ClusterHandle, NodeInfo, NodeState};
use crate::secure_transport::ClusterCipher;

/// Concurrent replica writes during a push.
const PUSH_CONCURRENCY: usize = 16;

/// Entries waiting between the store walk and the push tasks.
const PUSH_QUEUE: usize = 64;

/// How long a starting node with join seeds waits for gossip to list a
/// first alive peer before pulling. A gossip round is sub-second; this is
/// the bound for a cluster whose other nodes are all restarting at once.
pub const PEER_DISCOVERY_WAIT: Duration = Duration::from_secs(10);

/// The transport (wire) key of `key` in `site` (`""` = the global store).
#[must_use]
pub fn transport_key(site: &str, key: &str) -> String {
    if site.is_empty() { key.to_string() } else { crate::site_namespace::encode(site, key) }
}

/// Build the entry a handoff sends for `key`: the value, its remaining TTL
/// and the stamp recorded for it, read as one consistent pair through
/// [`KvRouter::read_stamped`]. `None` when the key is gone, expired, or no
/// longer large (the walk that enumerated it ran a moment earlier).
///
/// The walk's own copy of the value is deliberately **not** used: it was
/// taken under the shard lock in a batch, before the stamp lookup, and a
/// write landing in between pairs an old value with a new stamp.
pub(crate) fn stamped_entry(
    router: &KvRouter,
    store: &Store,
    source: &HandoffSource,
    site: &str,
    key: &str,
) -> Option<ShareEntry> {
    let transport_key = transport_key(site, key);
    let ((value, ttl), stamp) = router.read_stamped(&transport_key, || store.get_with_ttl(key))?;
    if value.len() < source.large_min_len() {
        return None;
    }
    Some(ShareEntry {
        transport_key,
        value: value.to_vec(),
        ttl_ms: ttl.map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX).max(1)),
        stamp: stamp.unwrap_or_else(|| source.unknown_stamp()),
    })
}

/// What a push or pull did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HandoffReport {
    /// Entries sent (push: one per replica target) or received (pull).
    pub keys: u64,
    /// Value bytes sent or received.
    pub bytes: u64,
    /// Peers contacted.
    pub peers: usize,
    /// Peers that could not be reached or did not finish.
    pub failed_peers: usize,
    /// Replica writes that failed (push only).
    pub failed_writes: u64,
    /// The deadline passed before the walk finished.
    pub timed_out: bool,
    /// Wall time.
    pub elapsed: Duration,
}

/// The membership-and-ownership side of a handoff: what a node needs to
/// answer "which of my large values does *that* node own a replica of?".
/// Installed on the data-plane [`KvRouter`] (`with_handoff`) so the
/// [`OP_SHARE`](crate::kv_data_plane) handler can answer pulls.
pub struct HandoffSource {
    /// Weak: the source lives inside the data-plane router, which every
    /// connection task clones, and none of those may keep the cluster alive
    /// past its shutdown. A source whose cluster is gone answers with the
    /// requester alone (nothing is shared).
    cluster: std::sync::Weak<ClusterHandle>,
    self_id: String,
    threshold: usize,
    factor: usize,
}

impl std::fmt::Debug for HandoffSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HandoffSource")
            .field("threshold", &self.threshold)
            .field("factor", &self.factor)
            .finish_non_exhaustive()
    }
}

impl HandoffSource {
    /// Build from the cluster handle and `[cluster.kv]`.
    #[must_use]
    pub fn new(cluster: Arc<ClusterHandle>, kv: &ClusterKvConfig) -> Arc<Self> {
        Arc::new(Self {
            self_id: cluster.self_node().id,
            cluster: Arc::downgrade(&cluster),
            threshold: kv.small_key_threshold,
            factor: kv.replication_factor,
        })
    }

    /// Smallest decoded value length that rides the data plane (one above
    /// `small_key_threshold`, which is inclusive for the gossip tier).
    #[must_use]
    pub fn large_min_len(&self) -> usize {
        self.threshold + 1
    }

    /// The stamp for an entry whose write stamp is no longer known: it
    /// loses to every real write.
    #[must_use]
    pub fn unknown_stamp(&self) -> Stamp {
        Stamp::new(0, &self.self_id)
    }

    /// Alive membership as this node sees it, sorted by id, with
    /// `requester` forced in (as alive) when gossip has not caught up yet.
    /// Just the requester when the cluster has already shut down.
    pub async fn membership_with(&self, requester: &str) -> Vec<NodeInfo> {
        let mut alive = match self.cluster.upgrade() {
            Some(cluster) => alive_sorted(&cluster).await,
            None => Vec::new(),
        };
        if !alive.iter().any(|n| n.id == requester) {
            alive.push(NodeInfo::new(requester, "", NodeState::Alive));
            alive.sort_by(|a, b| a.id.cmp(&b.id));
        }
        alive
    }

    /// Whether **this** node is in the replica set of `transport_key` under
    /// `membership` — i.e. its local copy is authoritative, not a leftover.
    ///
    /// A node keeps its own copy of every large value it *wrote*, replica or
    /// not, and such an origin copy goes stale the moment another node
    /// rewrites the key: the new value is fanned out to the key's replica
    /// set, and the origin copy is only cleaned up by a gossip marker. A
    /// handoff must never forward those — a restarted node that received
    /// one served the pre-restart value for minutes in the lab.
    #[must_use]
    pub fn holds_authoritative(&self, membership: &[NodeInfo], transport_key: &str) -> bool {
        self.replica_includes(membership, &self.self_id, transport_key)
    }

    /// This node's id.
    #[must_use]
    pub fn self_id(&self) -> &str {
        &self.self_id
    }

    /// Whether `node_id` is in the replica set of `transport_key` under
    /// `membership`.
    #[must_use]
    pub fn replica_includes(
        &self,
        membership: &[NodeInfo],
        node_id: &str,
        transport_key: &str,
    ) -> bool {
        replica_nodes_for(membership, hash_key(transport_key), self.factor)
            .iter()
            .any(|n| n.id == node_id)
    }
}

/// Every alive node (self included), sorted by id — the ring the replica
/// sets are computed on.
async fn alive_sorted(cluster: &ClusterHandle) -> Vec<NodeInfo> {
    let mut alive: Vec<NodeInfo> =
        cluster.nodes().await.into_iter().filter(|n| n.state == NodeState::Alive).collect();
    alive.sort_by(|a, b| a.id.cmp(&b.id));
    alive
}

/// The push and pull operations for one node.
pub struct PeerHandoff {
    cluster: Arc<ClusterHandle>,
    client: Arc<DataPlaneClient>,
    router: KvRouter,
    source: Arc<HandoffSource>,
    data_port: u16,
}

impl std::fmt::Debug for PeerHandoff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerHandoff").field("source", &self.source).finish_non_exhaustive()
    }
}

impl PeerHandoff {
    /// Build for this node. `router` is the same router the data-plane
    /// listener serves (with its applied map and `source` installed), so a
    /// pulled entry lands exactly as a replica write would.
    #[must_use]
    pub fn new(
        cluster: Arc<ClusterHandle>,
        cipher: Option<Arc<ClusterCipher>>,
        router: KvRouter,
        source: Arc<HandoffSource>,
        kv: &ClusterKvConfig,
    ) -> Arc<Self> {
        Arc::new(Self {
            cluster,
            client: Arc::new(DataPlaneClient::new(cipher)),
            router,
            source,
            data_port: kv.data_port,
        })
    }

    /// Alive peers other than this node, with their data-plane addresses.
    async fn peers(&self) -> Vec<(NodeInfo, SocketAddr)> {
        let self_id = self.cluster.self_node().id;
        alive_sorted(&self.cluster)
            .await
            .into_iter()
            .filter(|n| n.id != self_id)
            .filter_map(|n| node_data_addr(&n, self.data_port).map(|addr| (n, addr)))
            .collect()
    }

    /// Pull this node's share of large values from every alive peer, applying
    /// each entry as a stamped write. Stops at `deadline` with what was
    /// received kept.
    ///
    /// `expect_peers` says the node was configured to join others (it has
    /// seeds): the pull then first waits — up to [`PEER_DISCOVERY_WAIT`] or
    /// the deadline, whichever is sooner — for gossip to list at least one
    /// alive peer. Startup calls this right after the listeners bind, before
    /// a single gossip round has completed, so without the wait a restarted
    /// node asked nobody and warmed from nothing.
    pub async fn pull(&self, deadline: Instant, expect_peers: bool) -> HandoffReport {
        let started = Instant::now();
        let self_id = self.cluster.self_node().id;
        let mut peers = self.peers().await;
        if expect_peers && peers.is_empty() {
            let give_up = deadline.min(started + PEER_DISCOVERY_WAIT);
            while peers.is_empty() && Instant::now() < give_up {
                tokio::time::sleep(Duration::from_millis(100)).await;
                peers = self.peers().await;
            }
            if peers.is_empty() {
                tracing::warn!(
                    waited_ms = started.elapsed().as_millis() as u64,
                    "KV warm-up pull: no alive peer appeared in gossip; warming from nothing"
                );
            } else {
                tracing::debug!(
                    waited_ms = started.elapsed().as_millis() as u64,
                    peers = peers.len(),
                    "KV warm-up pull: gossip membership formed"
                );
            }
        }
        let mut report = HandoffReport { peers: peers.len(), ..HandoffReport::default() };
        // Every peer at once: each shares a different subset, so the pull is
        // bounded by the slowest peer rather than the sum of all of them.
        let mut tasks = JoinSet::new();
        for (peer, addr) in peers {
            let client = Arc::clone(&self.client);
            let router = self.router.clone();
            let self_id = self_id.clone();
            tasks.spawn(async move {
                let result = client
                    .share_from(addr, &self_id, deadline, |entry: ShareEntry| {
                        if router
                            .apply_stamped(
                                &entry.transport_key,
                                entry.value,
                                entry.ttl_ms,
                                entry.stamp,
                            )
                            .is_none()
                        {
                            tracing::debug!(key = %entry.transport_key, "pulled key is not routable here");
                        }
                    })
                    .await;
                (peer, addr, result)
            });
        }
        while let Some(joined) = tasks.join_next().await {
            let Ok((peer, addr, result)) = joined else {
                report.failed_peers += 1;
                continue;
            };
            match result {
                Ok((keys, bytes)) => {
                    report.keys += keys;
                    report.bytes += bytes;
                    tracing::debug!(peer = %peer.id, keys, bytes, "KV warm-up pull from peer done");
                }
                Err(e) => {
                    report.failed_peers += 1;
                    if Instant::now() >= deadline {
                        report.timed_out = true;
                    }
                    tracing::warn!(peer = %peer.id, %addr, "KV warm-up pull from peer failed: {e:#}");
                }
            }
        }
        report.elapsed = started.elapsed();
        report
    }

    /// Push every large value this node holds to the nodes that form its
    /// replica set once this node is gone. Stops at `deadline`; whatever was
    /// sent stays sent.
    pub async fn push(&self, deadline: Instant) -> HandoffReport {
        let started = Instant::now();
        let peers = self.peers().await;
        let mut report = HandoffReport { peers: peers.len(), ..HandoffReport::default() };
        if peers.is_empty() {
            report.elapsed = started.elapsed();
            return report;
        }
        // The ring without this node: where each key's replicas live next.
        let mut remaining: Vec<NodeInfo> = peers.iter().map(|(n, _)| n.clone()).collect();
        remaining.sort_by(|a, b| a.id.cmp(&b.id));
        // And with it: only keys this node holds *authoritatively* (it is a
        // current replica) are pushed. Its origin copies of keys other nodes
        // replicate may be stale, and their real replicas lose nothing when
        // this node leaves.
        let mut full_ring = remaining.clone();
        full_ring.push(self.cluster.self_node());
        full_ring.sort_by(|a, b| a.id.cmp(&b.id));
        let addrs: std::collections::HashMap<String, SocketAddr> =
            peers.iter().map(|(n, a)| (n.id.clone(), *a)).collect();

        // Walk the stores on a blocking thread into a bounded queue; send
        // from tasks with bounded concurrency.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(ShareEntry, Vec<SocketAddr>)>(PUSH_QUEUE);
        let walker = {
            let router = self.router.clone();
            let source = Arc::clone(&self.source);
            let factor = source.factor;
            let targets_of = move |transport_key: &str| -> Vec<SocketAddr> {
                replica_nodes_for(&remaining, hash_key(transport_key), factor)
                    .iter()
                    .filter_map(|n| addrs.get(&n.id).copied())
                    .collect()
            };
            tokio::task::spawn_blocking(move || {
                let mut closed = false;
                for (site, store) in router.stores() {
                    if closed {
                        break;
                    }
                    // The scan only enumerates candidate keys; the value and
                    // its stamp are re-read as a consistent pair per key.
                    store.scan_strings_decoded(source.large_min_len(), |key, _value, _ttl| {
                        if closed {
                            return;
                        }
                        let tk = transport_key(&site, key);
                        if !source.holds_authoritative(&full_ring, &tk) {
                            return;
                        }
                        let targets = targets_of(&tk);
                        if targets.is_empty() {
                            return;
                        }
                        let Some(entry) = stamped_entry(&router, &store, &source, &site, key)
                        else {
                            return;
                        };
                        if tx.blocking_send((entry, targets)).is_err() {
                            closed = true;
                        }
                    });
                }
            })
        };

        let sent = Arc::new(AtomicU64::new(0));
        let sent_bytes = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicU64::new(0));
        let permits = Arc::new(Semaphore::new(PUSH_CONCURRENCY));
        let mut tasks = JoinSet::new();
        let mut timed_out = false;
        while let Some((entry, targets)) = rx.recv().await {
            if Instant::now() >= deadline {
                timed_out = true;
                break;
            }
            let entry = Arc::new(entry);
            for addr in targets {
                let Ok(permit) = Arc::clone(&permits).acquire_owned().await else { break };
                let client = Arc::clone(&self.client);
                let entry = Arc::clone(&entry);
                let (sent, sent_bytes, failed) =
                    (Arc::clone(&sent), Arc::clone(&sent_bytes), Arc::clone(&failed));
                tasks.spawn(async move {
                    let _permit = permit;
                    let ttl = (entry.ttl_ms > 0).then(|| Duration::from_millis(entry.ttl_ms));
                    match tokio::time::timeout_at(
                        deadline,
                        client.store_stamped(
                            addr,
                            &entry.transport_key,
                            &entry.value,
                            ttl,
                            entry.stamp,
                        ),
                    )
                    .await
                    {
                        Ok(Ok(true)) => {
                            sent.fetch_add(1, Ordering::Relaxed);
                            sent_bytes.fetch_add(entry.value.len() as u64, Ordering::Relaxed);
                        }
                        Ok(Ok(false) | Err(_)) | Err(_) => {
                            failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        }
        // Stop the walk (drops the receiver) and wait for in-flight sends.
        drop(rx);
        let _ = walker.await;
        while tasks.join_next().await.is_some() {}
        report.keys = sent.load(Ordering::Relaxed);
        report.bytes = sent_bytes.load(Ordering::Relaxed);
        report.failed_writes = failed.load(Ordering::Relaxed);
        report.timed_out = timed_out;
        report.elapsed = started.elapsed();
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_keys_envelope_only_site_keys() {
        assert_eq!(transport_key("", "k"), "k");
        assert_eq!(transport_key("a.test", "k"), crate::site_namespace::encode("a.test", "k"));
    }
}
