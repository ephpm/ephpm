//! Periodic anti-entropy for the large (data-plane) KV tier.
//!
//! Large values (above `[cluster.kv] small_key_threshold`) are copied to a
//! key's replica set — `hash(key) mod sorted-alive-nodes` plus the next
//! `replication_factor - 1` nodes — **at write time only**. Nothing rebalances
//! them afterwards (see [`crate::clustered_store`]), so two kinds of local copy
//! drift stale and keep being served (the local store is consulted before any
//! fetch-through) until a gossip delete marker happens to arrive:
//!
//! 1. **Origin copies** — the writing node keeps its own copy whether or not it
//!    is in the key's replica set. When another node rewrites the key, the new
//!    value fans out to the replica set; the origin's copy is only cleaned up
//!    by a gossip marker.
//! 2. **Ex-replica copies** — a node that was a replica under an older
//!    membership keeps its copy after the ring changes; later writes never
//!    reach it.
//!
//! A gossip marker can take seconds on a long-running node and **minutes** on a
//! freshly restarted one (its gossip catch-up is slow). This module closes that
//! gap with an active reconciliation pass (issue #632).
//!
//! # What a pass does
//!
//! Every [`ClusterKvConfig::anti_entropy_interval_secs`] a node walks its own
//! large values and, for each one, recomputes the authoritative replica set
//! from the **current** alive membership:
//!
//! - **Disowned copy** (this node is *not* in the replica set): the pass asks
//!   the replica set for the key and acts only on **proof**. A copy is
//!   **dropped** only when a reachable replica reports a **real stamp strictly
//!   newer** than this copy's (ours is superseded; readers then fetch through),
//!   or — for a snapshot-restored copy of unknown order — when a replica holds
//!   it with any real stamp (#630). A copy strictly **newer** than every
//!   replica is instead **pushed** to the replica set to repair the divergence.
//!   Everything else is **kept**.
//! - **Owned copy** (this node *is* a replica): the pass asks the other
//!   replicas and, if one holds a **strictly newer** stamp, **adopts** it
//!   through [`KvRouter::apply_stamped`] (newest-stamp-wins).
//!
//! ## Two invariants behind "drop"
//!
//! The naive rule "drop once a replica holds an equal-or-newer copy" is wrong
//! in two ways this module must avoid:
//!
//! - The applied-map stamp of a key not written in the last
//!   [`APPLIED_HORIZON`](crate::clustered_store::APPLIED_HORIZON) (10 minutes)
//!   is pruned, so a local copy reads as stamp-`None` and a replica reports
//!   [`Stamp::ZERO`]. Treating `ZERO` as "equal-or-newer" would drop a
//!   newer-but-fanout-failed origin copy once any replica merely holds the key.
//!   So a drop requires a **real** peer stamp, and a stamp-`None` local copy is
//!   dropped **only** when it is snapshot-restored (its order is genuinely
//!   unknown); an unrestored stamp-`None` copy is kept.
//! - A **broadcast** value (written to every node at one shared stamp, outside
//!   the replica-set model — an ACME certificate above all) would be dropped on
//!   every non-replica node the moment "equal-or-newer" fired on its identical
//!   stamp. So a drop requires the replica stamp to be **strictly newer**; an
//!   equal stamp is the same version, never a supersession. This replaces the
//!   need to recognise broadcast keys by name.
//!
//! # Why it is replication-safe
//!
//! The pass never deletes an *owned* copy, and the one case where it writes
//! outward (a **push**) lands through the same applied-stamp map every write
//! uses, so by construction it can neither **publish a winning stamp over newer
//! data** nor **resurrect a deleted key**:
//!
//! - A **drop** only removes a local copy; it emits no gossip and no stamp, so
//!   it cannot resurrect anything. It happens only when a reachable replica
//!   proves a strictly-newer real copy (or holds a restored copy's key with a
//!   real stamp), so it cannot lose the newest data. When no replica proves a
//!   newer copy the copy is **kept** — an orphan (a write that never
//!   replicated), a horizon-aged copy of unknown order, or a broadcast value
//!   must not be dropped, and a genuine delete is cleaned by the gossip
//!   tombstone, not here. A drop re-reads the stamp immediately beforehand and
//!   skips if it changed, so a write that landed locally mid-pass (and already
//!   fanned out to the replica set) is never dropped from under a reader.
//! - A **push** and an **adopt** both land through the same applied-stamp map
//!   every write uses
//!   ([`apply_if_newer`](crate::clustered_store::apply_if_newer)), so a stamp
//!   that is not strictly newer than the receiver's record — including a delete
//!   tombstone's stamp — is acknowledged but not applied. A pass can only ever
//!   move a copy *forward* to a value a replica does not yet hold.
//!
//! Small (gossip-tier) values are untouched: chitchat already converges them,
//! and the local-store walk filters to `small_key_threshold + 1` bytes and up,
//! exactly the data-plane tier.

use std::sync::{Arc, Weak};
use std::time::Duration;

use ephpm_config::ClusterKvConfig;
use ephpm_kv::store::Store;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::clustered_store::{hash_key, node_data_addr, replica_nodes_for};
use crate::gossip_kv::Stamp;
use crate::kv_data_plane::{DataPlaneClient, KvRouter, StampedValue};
use crate::kv_handoff::transport_key;
use crate::node::{ClusterHandle, NodeInfo, NodeState};
use crate::secure_transport::ClusterCipher;

/// Counter: disowned large-tier copies dropped because a reachable replica
/// proved a strictly-newer real stamp (or, for a snapshot-restored copy, holds
/// it with a real stamp).
pub const METRIC_DROPPED: &str = "ephpm_kv_anti_entropy_dropped_total";

/// Counter: owned large-tier copies refreshed from a co-replica holding a
/// strictly newer stamp.
pub const METRIC_REFRESHED: &str = "ephpm_kv_anti_entropy_refreshed_total";

/// Counter: large-tier keys examined by anti-entropy.
pub const METRIC_SCANNED: &str = "ephpm_kv_anti_entropy_scanned_total";

/// Counter: anti-entropy passes completed.
pub const METRIC_CYCLES: &str = "ephpm_kv_anti_entropy_cycles_total";

/// Concurrent replica probes within one pass.
const PROBE_CONCURRENCY: usize = 16;

/// Per-probe budget for a single stamped fetch from a replica.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// What [`decide_disowned`] concluded for a local copy this node does not own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disowned {
    /// Leave the local copy: nothing proved it superseded. No replica could be
    /// reached; or no replica reported a **real** stamp (one whose own record
    /// aged out reports [`Stamp::ZERO`], which orders nothing); or the newest
    /// real replica stamp is **equal** to ours (the same version — e.g. a
    /// broadcast value present cluster-wide, Fix 1 / #632); or it is **older**
    /// (an orphan we must not lose; a genuine delete is cleaned by the gossip
    /// tombstone, not here).
    Keep,
    /// Drop the local string copy: a reachable replica proved a **real stamp
    /// strictly newer** than ours (or, for a snapshot-restored copy of unknown
    /// order, any real stamp), so readers fetch through to the newer copy.
    Drop,
    /// Repair forward: our disowned copy is **strictly newer** than every
    /// reachable replica's real stamp — a write that never reached the current
    /// replica set — so push it to the replica set (safe under
    /// `apply_if_newer`) instead of leaving the cluster divergent (#632).
    Push,
}

/// Decide the fate of a **disowned** copy (this node is not a replica) from
/// what the replica set reported.
///
/// `local` is this node's recorded stamp for the key (`None` when it predates
/// the applied-map horizon or was restored from a snapshot). `local_restored`
/// says this node's copy was materialised by a snapshot restore rather than a
/// live write — the only case in which a copy of unknown order may still defer
/// to a replica (#630). `best_held` is the newest stamp any reachable replica
/// holds (`None` when no replica holds it; a replica holding it with no
/// recorded stamp contributes [`Stamp::ZERO`]). `any_reachable` says at least
/// one replica answered.
///
/// The governing invariant: **drop only a copy a reachable replica proves is
/// superseded by a real, strictly-newer stamp** (or, for a snapshot-restored
/// copy whose own order is unknown, any real stamp). A replica's `Stamp::ZERO`
/// (its stamp aged out of the 10-minute applied-map horizon) proves nothing and
/// never licenses a drop — otherwise a never-replicated newer origin copy whose
/// own stamp has aged out would be lost once any replica merely holds the key
/// (the horizon bug, #632). An **equal** real stamp is the same version, not a
/// supersession, so it is kept — which is what lets a broadcast/ACME value
/// (identical stamp on every node) survive on a non-replica node.
pub(crate) fn decide_disowned(
    local: Option<Stamp>,
    local_restored: bool,
    best_held: Option<Stamp>,
    any_reachable: bool,
) -> Disowned {
    if !any_reachable {
        // Can't verify against the authoritative set, and a reader could not
        // fetch through either — keep what we have.
        return Disowned::Keep;
    }
    // Only a REAL stamp orders anything. A replica that holds the key with no
    // recorded stamp (its own applied record aged past the horizon) reports
    // Stamp::ZERO; that is "I have a copy of unknown age", not "mine is newer".
    let real = best_held.filter(|s| *s != Stamp::ZERO);
    match (local, real) {
        // Our copy is strictly newer than every replica's proven copy: a write
        // that never reached the current replica set. Repair the replicas
        // forward rather than leave them serving the older value (Fix 3).
        (Some(ls), Some(r)) if ls > r => Disowned::Push,
        // A replica proved a strictly-newer copy: ours is superseded, drop it.
        (Some(ls), Some(r)) if r > ls => Disowned::Drop,
        // Unknown local order: a snapshot-restored copy defers to a replica
        // holding a real stamp (#630). A copy whose stamp merely aged out of
        // the horizon is NOT restored and must be kept — it could be a
        // fanout-failed newer write (the horizon bug, #632).
        (None, Some(_)) if local_restored => Disowned::Drop,
        // Everything else — equal real stamps (same version, e.g. a broadcast
        // value, Fix 1), an older/absent replica copy (orphan or propagating
        // delete), or an unknown-order copy with no real replica stamp — cannot
        // be proved superseded: keep it.
        _ => Disowned::Keep,
    }
}

/// Whether an **owned** copy (this node is a replica) should adopt a
/// co-replica's value: only when a co-replica holds a **strictly newer** stamp
/// than this node's record. `None` local is treated as [`Stamp::ZERO`], so any
/// real stamp a peer holds wins.
pub(crate) fn should_adopt(local: Option<Stamp>, best_peer: Option<Stamp>) -> bool {
    match best_peer {
        None => false,
        Some(p) => p > local.unwrap_or(Stamp::ZERO),
    }
}

/// What one key's reconciliation did (tallied into a [`ReconcileReport`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyOutcome {
    /// A disowned copy was dropped.
    Dropped,
    /// An owned copy adopted a newer co-replica value.
    Refreshed,
    /// A disowned copy strictly newer than the replica set was pushed to it to
    /// repair the divergence. The local copy is kept: once the replicas hold
    /// our stamp it is equal to theirs, and an equal stamp is never dropped
    /// (the broadcast-safety rule), so the redundant copy is reclaimed only
    /// when a newer write or a gossip delete supersedes it — fetch-through
    /// serves the replica set meanwhile.
    Repaired,
    /// Nothing to do, or deliberately kept (orphan / local newest).
    Kept,
    /// No replica (or co-replica) could be reached for this key.
    Unreachable,
    /// The key vanished locally before it could be reconciled.
    Gone,
}

/// Counters for one anti-entropy pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Large-tier keys examined.
    pub scanned: u64,
    /// Disowned copies dropped.
    pub dropped: u64,
    /// Owned copies refreshed from a newer co-replica.
    pub refreshed: u64,
    /// Keys deliberately left as they were.
    pub kept: u64,
    /// Keys whose replica set could not be reached this pass.
    pub unreachable: u64,
}

impl ReconcileReport {
    fn record(&mut self, outcome: KeyOutcome) {
        match outcome {
            KeyOutcome::Dropped => self.dropped += 1,
            KeyOutcome::Refreshed => self.refreshed += 1,
            // A repair leaves the local copy in place and only pushes outward,
            // so it is tallied with the "kept" copies — it is not a drop or a
            // local refresh.
            KeyOutcome::Repaired | KeyOutcome::Kept | KeyOutcome::Gone => self.kept += 1,
            KeyOutcome::Unreachable => self.unreachable += 1,
        }
    }
}

/// What a replica answered for a probed key.
enum Probe {
    /// The replica holds the key, with the stamp it recorded (if any).
    Held(StampedValue),
    /// The replica answered and does not hold the key.
    Absent,
    /// The replica could not be reached (connect/IO failure or timeout).
    Down,
}

/// The periodic large-tier anti-entropy reconciler for one node.
pub struct AntiEntropy {
    /// Weak so the reconcile task never keeps the cluster alive past its
    /// shutdown — a pass whose cluster is gone sees no peers and does nothing.
    cluster: Weak<ClusterHandle>,
    client: Arc<DataPlaneClient>,
    /// The same router the data plane serves (shared stores + applied map), so
    /// a reconciled value lands exactly as a replica write would.
    router: KvRouter,
    self_id: String,
    /// `small_key_threshold`: the gossip/data-plane tier boundary.
    threshold: usize,
    /// `replication_factor`: how many copies a large key keeps.
    factor: usize,
    data_port: u16,
    interval: Duration,
}

impl std::fmt::Debug for AntiEntropy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AntiEntropy")
            .field("self_id", &self.self_id)
            .field("factor", &self.factor)
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
}

impl AntiEntropy {
    /// Build for this node. `router` is the data-plane router (its applied map
    /// and stores), `cipher` matches the peers' data-plane listeners.
    #[must_use]
    pub fn new(
        cluster: &Arc<ClusterHandle>,
        cipher: Option<Arc<ClusterCipher>>,
        router: KvRouter,
        kv: &ClusterKvConfig,
    ) -> Arc<Self> {
        Arc::new(Self {
            cluster: Arc::downgrade(cluster),
            client: Arc::new(DataPlaneClient::new(cipher)),
            router,
            self_id: cluster.self_node().id,
            threshold: kv.small_key_threshold,
            factor: kv.replication_factor,
            data_port: kv.data_port,
            interval: Duration::from_secs(kv.anti_entropy_interval_secs),
        })
    }

    /// Whether anti-entropy is enabled (a non-zero interval).
    #[must_use]
    pub fn enabled(&self) -> bool {
        !self.interval.is_zero()
    }

    /// Run reconciliation passes forever, one every [`Self::interval`]. The
    /// caller aborts the task at shutdown. A zero interval returns at once
    /// (disabled). The first pass waits one interval so startup — which already
    /// runs the warm-up pull — is not contended.
    pub async fn run(self: Arc<Self>) {
        if !self.enabled() {
            return;
        }
        let mut ticker = tokio::time::interval(self.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The first tick fires immediately; consume it so the first real pass
        // is one interval out.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let report = self.reconcile_once().await;
            if report.dropped > 0 || report.refreshed > 0 {
                tracing::info!(
                    scanned = report.scanned,
                    dropped = report.dropped,
                    refreshed = report.refreshed,
                    unreachable = report.unreachable,
                    "KV large-tier anti-entropy reconciled copies"
                );
            } else {
                tracing::debug!(
                    scanned = report.scanned,
                    unreachable = report.unreachable,
                    "KV large-tier anti-entropy pass (no change)"
                );
            }
        }
    }

    /// One reconciliation pass over every local large-tier key.
    pub async fn reconcile_once(self: &Arc<Self>) -> ReconcileReport {
        let alive = self.alive_sorted().await;
        if alive.len() <= 1 {
            // Alone in the cluster: nothing to reconcile against.
            metrics::counter!(METRIC_CYCLES).increment(1);
            return ReconcileReport::default();
        }
        let alive = Arc::new(alive);

        // Enumerate candidate large keys off the runtime: the walk takes each
        // shard's lock in turn (the same walk the handoff push uses).
        let stores = self.router.stores();
        let min_len = self.threshold.saturating_add(1);
        let candidates = tokio::task::spawn_blocking(move || {
            let mut out: Vec<(String, Arc<Store>, String)> = Vec::new();
            for (site, store) in stores {
                store.scan_strings_decoded(min_len, |key, _value, _ttl| {
                    out.push((site.clone(), Arc::clone(&store), key.to_string()));
                });
            }
            out
        })
        .await
        .unwrap_or_default();

        let mut report =
            ReconcileReport { scanned: candidates.len() as u64, ..ReconcileReport::default() };

        let sem = Arc::new(Semaphore::new(PROBE_CONCURRENCY));
        let mut tasks = JoinSet::new();
        for (site, store, key) in candidates {
            let me = Arc::clone(self);
            let alive = Arc::clone(&alive);
            // `acquire_owned` never errors here: the semaphore is never closed.
            let Ok(permit) = Arc::clone(&sem).acquire_owned().await else { break };
            tasks.spawn(async move {
                let _permit = permit;
                me.reconcile_key(&alive, &site, &store, &key).await
            });
        }
        while let Some(joined) = tasks.join_next().await {
            if let Ok(outcome) = joined {
                report.record(outcome);
            }
        }

        metrics::counter!(METRIC_CYCLES).increment(1);
        metrics::counter!(METRIC_SCANNED).increment(report.scanned);
        if report.dropped > 0 {
            metrics::counter!(METRIC_DROPPED).increment(report.dropped);
        }
        if report.refreshed > 0 {
            metrics::counter!(METRIC_REFRESHED).increment(report.refreshed);
        }
        report
    }

    /// Reconcile one local large value against its current replica set.
    async fn reconcile_key(
        &self,
        alive: &[NodeInfo],
        site: &str,
        store: &Arc<Store>,
        key: &str,
    ) -> KeyOutcome {
        let transport = transport_key(site, key);
        let replicas = replica_nodes_for(alive, hash_key(&transport), self.factor);
        let self_is_replica = replicas.iter().any(|n| n.id == self.self_id);

        // The stamp this node currently records for the key, paired — under the
        // applied-map slot, so the pair is consistent — with whether the local
        // copy is snapshot-restored. `None` => the key is gone (reaped,
        // deleted, overwritten small), nothing to do.
        let Some((local_restored, local_stamp)) =
            self.router.read_stamped(&transport, || store.string_restored(key))
        else {
            return KeyOutcome::Gone;
        };

        if self_is_replica {
            self.refresh_owned(&replicas, &transport, local_stamp).await
        } else {
            self.drop_disowned(&replicas, &transport, store, key, local_restored, local_stamp).await
        }
    }

    /// Owned-copy path: adopt the newest strictly-newer co-replica value.
    async fn refresh_owned(
        &self,
        replicas: &[NodeInfo],
        transport: &str,
        local_stamp: Option<Stamp>,
    ) -> KeyOutcome {
        let mut best: Option<(Stamp, Vec<u8>, u64)> = None;
        let mut any_reachable = false;
        for node in replicas.iter().filter(|n| n.id != self.self_id) {
            let Some(addr) = node_data_addr(node, self.data_port) else { continue };
            match self.probe(addr, transport).await {
                Probe::Held(sv) => {
                    any_reachable = true;
                    // Only a peer that reports a stamp can be ordered; one
                    // without a recorded stamp is left alone.
                    if let Some(stamp) = sv.stamp
                        && best.as_ref().is_none_or(|(b, ..)| stamp > *b)
                    {
                        best = Some((stamp, sv.value, ttl_to_ms(sv.ttl)));
                    }
                }
                Probe::Absent => any_reachable = true,
                Probe::Down => {}
            }
        }

        let best_stamp = best.as_ref().map(|(s, ..)| *s);
        if should_adopt(local_stamp, best_stamp) {
            let (stamp, value, ttl_ms) = best.expect("should_adopt is false when best is None");
            // Lands through the applied map: newest-stamp-wins, so a newer
            // local write (or a delete tombstone's stamp) that arrived mid-pass
            // is never regressed. Count a refresh ONLY when the value was
            // genuinely written: `apply_stamped_wrote` answers `Some(true)` for
            // a real write, but `Some(false)` when the write was ordered out as
            // stale (a newer stamp landed between the probe and here) or the
            // store declined it — neither moved this copy forward.
            if self.router.apply_stamped_wrote(transport, value, ttl_ms, stamp) == Some(true) {
                return KeyOutcome::Refreshed;
            }
        }
        if any_reachable { KeyOutcome::Kept } else { KeyOutcome::Unreachable }
    }

    /// Disowned-copy path: drop the local copy only when a reachable replica
    /// proves a strictly-newer real copy (or, for a snapshot-restored copy, any
    /// real stamp); push it to the replica set when it is strictly newer than
    /// them (Fix 3); otherwise keep it.
    async fn drop_disowned(
        &self,
        replicas: &[NodeInfo],
        transport: &str,
        store: &Arc<Store>,
        key: &str,
        local_restored: bool,
        local_stamp: Option<Stamp>,
    ) -> KeyOutcome {
        let mut best_held: Option<Stamp> = None;
        let mut any_reachable = false;
        for node in replicas.iter().filter(|n| n.id != self.self_id) {
            let Some(addr) = node_data_addr(node, self.data_port) else { continue };
            match self.probe(addr, transport).await {
                Probe::Held(sv) => {
                    any_reachable = true;
                    let s = sv.stamp.unwrap_or(Stamp::ZERO);
                    best_held = Some(best_held.map_or(s, |b| b.max(s)));
                }
                Probe::Absent => any_reachable = true,
                Probe::Down => {}
            }
        }

        match decide_disowned(local_stamp, local_restored, best_held, any_reachable) {
            Disowned::Drop => {
                // Re-check under the applied slot: a write that landed locally
                // since we read the stamp (which also fanned out to the replica
                // set) must not be dropped from under a reader. If the stamp is
                // unchanged, the copy we verified is the one we remove.
                let still = self.router.read_stamped(transport, || store.exists(key).then_some(()));
                let unchanged = matches!(still, Some(((), s)) if s == local_stamp);
                if unchanged && store.remove_string_local(key) {
                    tracing::debug!(key = transport, "anti-entropy dropped a disowned large copy");
                    KeyOutcome::Dropped
                } else {
                    KeyOutcome::Kept
                }
            }
            Disowned::Push => self.push_to_replicas(replicas, transport, store, key).await,
            Disowned::Keep if any_reachable => KeyOutcome::Kept,
            Disowned::Keep => KeyOutcome::Unreachable,
        }
    }

    /// Repair a divergent replica set: this disowned copy is strictly newer
    /// than every replica holds, so push it to them. Safe because every peer
    /// lands it through `apply_if_newer` (it loses to anything newer already
    /// there); the local copy is left in place — once the replicas hold our
    /// stamp it is equal to theirs, which anti-entropy keeps (an equal stamp is
    /// never a supersession), so the redundant copy stays until a newer write
    /// or a gossip delete supersedes it. Value and stamp are read as one
    /// consistent pair, so a write that lands mid-pass is never shipped under a
    /// stale stamp.
    async fn push_to_replicas(
        &self,
        replicas: &[NodeInfo],
        transport: &str,
        store: &Arc<Store>,
        key: &str,
    ) -> KeyOutcome {
        let Some(((value, ttl), Some(stamp))) =
            self.router.read_stamped(transport, || store.get_with_ttl(key))
        else {
            // The key vanished or lost its stamp since the decision — nothing
            // safe to push.
            return KeyOutcome::Kept;
        };
        let mut pushed = false;
        for node in replicas.iter().filter(|n| n.id != self.self_id) {
            let Some(addr) = node_data_addr(node, self.data_port) else { continue };
            if self
                .client
                .store_stamped(addr, transport, value.as_ref(), ttl, stamp)
                .await
                .unwrap_or(false)
            {
                pushed = true;
            }
        }
        if pushed {
            tracing::debug!(
                key = transport,
                "anti-entropy pushed a strictly-newer disowned copy to its replica set"
            );
            KeyOutcome::Repaired
        } else {
            KeyOutcome::Kept
        }
    }

    /// Probe one replica for a key, mapping the data-plane result to a
    /// [`Probe`]. A timeout trips the per-peer stall breaker so a dead peer is
    /// skipped quickly on the next pass.
    async fn probe(&self, addr: std::net::SocketAddr, transport: &str) -> Probe {
        match tokio::time::timeout(PROBE_TIMEOUT, self.client.fetch_stamped(addr, transport)).await
        {
            Ok(Ok(Some(sv))) => Probe::Held(sv),
            Ok(Ok(None)) => Probe::Absent,
            Ok(Err(_)) => Probe::Down,
            Err(_) => {
                self.client.record_stall(addr);
                Probe::Down
            }
        }
    }

    /// Alive membership, sorted by id — the ring replica sets are computed on.
    /// Empty once the cluster has shut down (the weak handle is gone).
    async fn alive_sorted(&self) -> Vec<NodeInfo> {
        let Some(cluster) = self.cluster.upgrade() else {
            return Vec::new();
        };
        let mut alive: Vec<NodeInfo> =
            cluster.nodes().await.into_iter().filter(|n| n.state == NodeState::Alive).collect();
        alive.sort_by(|a, b| a.id.cmp(&b.id));
        alive
    }
}

/// Remaining-TTL duration to the wire's milliseconds (`0` = no expiry; a live
/// TTL never rounds down to `0`).
fn ttl_to_ms(ttl: Option<Duration>) -> u64 {
    ttl.map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX).max(1))
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use ephpm_kv::store::{Store, StoreConfig};

    use super::*;
    use crate::clustered_store::new_applied_write_map;
    use crate::gossip_kv::StampClock;
    use crate::kv_data_plane::serve_router;

    // ── Pure decision functions ───────────────────────────────────────────

    fn at(write: u64, node: &str) -> Stamp {
        Stamp::new(write, node)
    }

    #[test]
    fn disowned_kept_when_no_replica_reachable() {
        assert_eq!(
            decide_disowned(Some(at(10, "a")), false, Some(at(10, "a")), false),
            Disowned::Keep
        );
        assert_eq!(decide_disowned(None, true, None, false), Disowned::Keep);
    }

    #[test]
    fn disowned_kept_when_no_replica_holds_the_key() {
        // Orphan (never replicated) or a delete still propagating: do not drop.
        assert_eq!(decide_disowned(Some(at(10, "a")), false, None, true), Disowned::Keep);
        assert_eq!(decide_disowned(None, true, None, true), Disowned::Keep);
    }

    #[test]
    fn disowned_dropped_only_for_a_restored_copy_a_replica_holds_with_a_real_stamp() {
        // The snapshot-restored case (#630): no local stamp, replica is truth —
        // but only with a REAL stamp, and only because the copy is restored.
        assert_eq!(decide_disowned(None, true, Some(at(1, "a")), true), Disowned::Drop);
        // A real replica stamp is required: ZERO (the replica's own stamp aged
        // out of the horizon) proves nothing, even for a restored copy.
        assert_eq!(decide_disowned(None, true, Some(Stamp::ZERO), true), Disowned::Keep);
    }

    #[test]
    fn disowned_horizon_aged_copy_is_kept_not_dropped() {
        // Fix 2 / #632: a local copy whose stamp aged out of the applied-map
        // horizon (local None) that is NOT snapshot-restored could be a
        // never-replicated newer write. It must be kept even when a replica
        // holds the key with a real stamp — the "+10min fanout-failed" case.
        assert_eq!(decide_disowned(None, false, Some(at(5, "b")), true), Disowned::Keep);
        assert_eq!(decide_disowned(None, false, Some(Stamp::ZERO), true), Disowned::Keep);
    }

    #[test]
    fn disowned_dropped_only_when_replica_is_strictly_newer_real() {
        assert_eq!(
            decide_disowned(Some(at(10, "a")), false, Some(at(20, "b")), true),
            Disowned::Drop
        );
        // Fix 1 / #632: an EQUAL real stamp is the same version (e.g. a
        // broadcast value present cluster-wide) and must be kept, not dropped.
        assert_eq!(
            decide_disowned(Some(at(10, "a")), false, Some(at(10, "a")), true),
            Disowned::Keep
        );
        // A replica whose stamp aged out (ZERO) proves nothing: keep.
        assert_eq!(
            decide_disowned(Some(at(10, "a")), false, Some(Stamp::ZERO), true),
            Disowned::Keep
        );
    }

    #[test]
    fn disowned_strictly_newer_than_the_replica_set_is_pushed() {
        // Fix 3 / #632: our disowned copy is strictly newer than the replica's
        // real stamp — a never-replicated newer write. Repair the replicas.
        assert_eq!(
            decide_disowned(Some(at(20, "a")), false, Some(at(10, "b")), true),
            Disowned::Push
        );
        // ...but only when the replica's newer-or-older stamp is real; ZERO
        // cannot be ordered against, so keep.
        assert_eq!(
            decide_disowned(Some(at(20, "a")), false, Some(Stamp::ZERO), true),
            Disowned::Keep
        );
    }

    #[test]
    fn adopt_only_a_strictly_newer_stamp() {
        assert!(!should_adopt(Some(at(10, "a")), None));
        assert!(should_adopt(Some(at(10, "a")), Some(at(20, "b"))));
        assert!(!should_adopt(Some(at(10, "a")), Some(at(10, "a"))));
        assert!(!should_adopt(Some(at(20, "a")), Some(at(10, "b"))));
        // Unknown local stamp: adopt any real stamp, but not another unknown.
        assert!(should_adopt(None, Some(at(1, "a"))));
        assert!(!should_adopt(None, Some(Stamp::ZERO)));
    }

    // ── End-to-end reconciliation over a real data-plane listener ──────────

    fn large_value(byte: u8) -> Vec<u8> {
        vec![byte; 1024] // above the default small_key_threshold (512)
    }

    /// Bind a peer data-plane listener serving `router` and return its address
    /// once it accepts connections.
    async fn spawn_peer(router: KvRouter) -> SocketAddr {
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);
        tokio::spawn(async move {
            let _ = serve_router(router, addr, None).await;
        });
        for _ in 0..200 {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        addr
    }

    /// A node with a data-plane address derived from `addr`.
    fn node_at(id: &str, addr: SocketAddr) -> NodeInfo {
        let mut n = NodeInfo::new(id, format!("127.0.0.1:{}", addr.port()), NodeState::Alive);
        n.data_port = Some(addr.port());
        n
    }

    /// A node with no reachable data plane (used only for ring membership).
    fn node_placeholder(id: &str) -> NodeInfo {
        let mut n = NodeInfo::new(id, "127.0.0.1:1", NodeState::Alive);
        n.data_port = Some(1);
        n
    }

    fn test_reconciler(self_id: &str, router: KvRouter, data_port: u16) -> Arc<AntiEntropy> {
        Arc::new(AntiEntropy {
            cluster: Weak::new(),
            client: Arc::new(DataPlaneClient::new(None)),
            router,
            self_id: self_id.to_string(),
            threshold: 512,
            factor: 1,
            data_port,
            interval: Duration::ZERO,
        })
    }

    #[tokio::test]
    async fn disowned_copy_dropped_when_a_replica_holds_a_newer_copy() {
        let clock = StampClock::new("origin");
        let older = clock.next();
        let newer = clock.next();

        // Peer (the owner) holds the newer value.
        let peer_store = Store::new(StoreConfig::default());
        let peer_applied = new_applied_write_map();
        let peer_router =
            KvRouter::global(Arc::clone(&peer_store)).with_applied(Arc::clone(&peer_applied));
        assert!(
            peer_router.apply_stamped("big", large_value(2), 0, newer).unwrap(),
            "peer accepts the newer copy"
        );
        let peer_addr = spawn_peer(peer_router).await;

        // This node holds an older, disowned copy.
        let local_store = Store::new(StoreConfig::default());
        let local_applied = new_applied_write_map();
        let local_router =
            KvRouter::global(Arc::clone(&local_store)).with_applied(Arc::clone(&local_applied));
        assert!(local_router.apply_stamped("big", large_value(1), 0, older).unwrap());
        assert!(local_store.exists("big"));

        // Two-node ring, factor 1: exactly one owner. Put the listener on the
        // owner and make this node the other (disowned) one.
        let mut ring = vec![node_placeholder("n1"), node_placeholder("n2")];
        ring.sort_by(|a, b| a.id.cmp(&b.id));
        let owner_id = replica_nodes_for(&ring, hash_key("big"), 1)[0].id.clone();
        let self_id = if owner_id == "n1" { "n2" } else { "n1" };
        let alive = vec![node_at(&owner_id, peer_addr), node_placeholder(self_id)];
        let mut alive = alive;
        alive.sort_by(|a, b| a.id.cmp(&b.id));

        let ae = test_reconciler(self_id, local_router, peer_addr.port());
        let outcome = ae.reconcile_key(&alive, "", &local_store, "big").await;
        assert_eq!(outcome, KeyOutcome::Dropped);
        assert!(!local_store.exists("big"), "the disowned copy was dropped");
    }

    #[tokio::test]
    async fn disowned_copy_kept_when_no_replica_holds_it() {
        let clock = StampClock::new("origin");
        let local = clock.next();

        // Peer owner holds nothing for the key.
        let peer_store = Store::new(StoreConfig::default());
        let peer_router =
            KvRouter::global(Arc::clone(&peer_store)).with_applied(new_applied_write_map());
        let peer_addr = spawn_peer(peer_router).await;

        let local_store = Store::new(StoreConfig::default());
        let local_router =
            KvRouter::global(Arc::clone(&local_store)).with_applied(new_applied_write_map());
        assert!(local_router.apply_stamped("orphan", large_value(1), 0, local).unwrap());

        let mut ring = vec![node_placeholder("n1"), node_placeholder("n2")];
        ring.sort_by(|a, b| a.id.cmp(&b.id));
        let owner_id = replica_nodes_for(&ring, hash_key("orphan"), 1)[0].id.clone();
        let self_id = if owner_id == "n1" { "n2" } else { "n1" };
        let mut alive = vec![node_at(&owner_id, peer_addr), node_placeholder(self_id)];
        alive.sort_by(|a, b| a.id.cmp(&b.id));

        let ae = test_reconciler(self_id, local_router, peer_addr.port());
        let outcome = ae.reconcile_key(&alive, "", &local_store, "orphan").await;
        assert_eq!(outcome, KeyOutcome::Kept);
        assert!(local_store.exists("orphan"), "an orphan copy is never dropped");
    }

    #[tokio::test]
    async fn owned_copy_refreshed_from_a_newer_coreplica() {
        let clock = StampClock::new("origin");
        let older = clock.next();
        let newer = clock.next();

        // Co-replica holds the newer value.
        let peer_store = Store::new(StoreConfig::default());
        let peer_router =
            KvRouter::global(Arc::clone(&peer_store)).with_applied(new_applied_write_map());
        assert!(peer_router.apply_stamped("big", large_value(9), 0, newer).unwrap());
        let peer_addr = spawn_peer(peer_router).await;

        // This node holds the older value and is also a replica (factor 2).
        let local_store = Store::new(StoreConfig::default());
        let local_applied = new_applied_write_map();
        let local_router =
            KvRouter::global(Arc::clone(&local_store)).with_applied(Arc::clone(&local_applied));
        assert!(local_router.apply_stamped("big", large_value(1), 0, older).unwrap());

        // Two nodes, factor 2: both are replicas.
        let mut alive = vec![node_at("peer", peer_addr), node_placeholder("selfnode")];
        alive.sort_by(|a, b| a.id.cmp(&b.id));

        let mut ae = test_reconciler("selfnode", local_router, peer_addr.port());
        Arc::get_mut(&mut ae).unwrap().factor = 2;
        let outcome = ae.reconcile_key(&alive, "", &local_store, "big").await;
        assert_eq!(outcome, KeyOutcome::Refreshed);
        assert_eq!(
            local_store.get("big").as_deref(),
            Some(large_value(9).as_slice()),
            "adopted the co-replica's newer value"
        );
        assert_eq!(*local_applied.get("big").unwrap(), newer, "and its stamp");
    }

    #[tokio::test]
    async fn owned_copy_kept_when_local_is_newest() {
        let clock = StampClock::new("origin");
        let older = clock.next();
        let newer = clock.next();

        // Co-replica holds an OLDER value than this node.
        let peer_store = Store::new(StoreConfig::default());
        let peer_router =
            KvRouter::global(Arc::clone(&peer_store)).with_applied(new_applied_write_map());
        assert!(peer_router.apply_stamped("big", large_value(1), 0, older).unwrap());
        let peer_addr = spawn_peer(peer_router).await;

        let local_store = Store::new(StoreConfig::default());
        let local_router =
            KvRouter::global(Arc::clone(&local_store)).with_applied(new_applied_write_map());
        assert!(local_router.apply_stamped("big", large_value(9), 0, newer).unwrap());

        let mut alive = vec![node_at("peer", peer_addr), node_placeholder("selfnode")];
        alive.sort_by(|a, b| a.id.cmp(&b.id));

        let mut ae = test_reconciler("selfnode", local_router, peer_addr.port());
        Arc::get_mut(&mut ae).unwrap().factor = 2;
        let outcome = ae.reconcile_key(&alive, "", &local_store, "big").await;
        assert_eq!(outcome, KeyOutcome::Kept);
        assert_eq!(
            local_store.get("big").as_deref(),
            Some(large_value(9).as_slice()),
            "the newest local value is untouched"
        );
    }

    /// Fix 1 / #632: a broadcast value is written to **every** node (outside
    /// the replica-set model, for cluster-wide state such as an ACME
    /// certificate) at **one shared stamp**. The old "equal-or-newer → drop"
    /// rule dropped it on every non-replica node, so the cert vanished. A copy
    /// a replica holds at the **same** stamp is the same version and must
    /// survive a reconcile pass.
    #[tokio::test]
    async fn broadcast_copy_survives_reconcile_on_a_non_replica() {
        let clock = StampClock::new("origin");
        let shared = clock.next();

        // The replica (owner) holds the broadcast value at the shared stamp.
        let peer_store = Store::new(StoreConfig::default());
        let peer_router =
            KvRouter::global(Arc::clone(&peer_store)).with_applied(new_applied_write_map());
        assert!(peer_router.apply_stamped("big", large_value(7), 0, shared).unwrap());
        let peer_addr = spawn_peer(peer_router).await;

        // This (non-replica) node holds the SAME value at the SAME stamp — what
        // a broadcast leaves behind on a node outside the replica set.
        let local_store = Store::new(StoreConfig::default());
        let local_router =
            KvRouter::global(Arc::clone(&local_store)).with_applied(new_applied_write_map());
        assert!(local_router.apply_stamped("big", large_value(7), 0, shared).unwrap());
        assert!(local_store.exists("big"));

        let mut ring = vec![node_placeholder("n1"), node_placeholder("n2")];
        ring.sort_by(|a, b| a.id.cmp(&b.id));
        let owner_id = replica_nodes_for(&ring, hash_key("big"), 1)[0].id.clone();
        let self_id = if owner_id == "n1" { "n2" } else { "n1" };
        let mut alive = vec![node_at(&owner_id, peer_addr), node_placeholder(self_id)];
        alive.sort_by(|a, b| a.id.cmp(&b.id));

        let ae = test_reconciler(self_id, local_router, peer_addr.port());
        let outcome = ae.reconcile_key(&alive, "", &local_store, "big").await;
        assert_eq!(outcome, KeyOutcome::Kept, "an equal-stamp (broadcast) copy is never dropped");
        assert!(local_store.exists("big"), "the broadcast copy survived the pass");
    }

    /// Fix 3 / #632: a disowned copy strictly newer than the replica set — a
    /// write that never fanned out — is pushed to the replicas to repair the
    /// divergence rather than left with the cluster serving the older value.
    #[tokio::test]
    async fn disowned_copy_strictly_newer_than_replica_is_pushed() {
        let clock = StampClock::new("origin");
        let older = clock.next();
        let newer = clock.next();

        // The replica (owner) holds only the OLDER value.
        let peer_store = Store::new(StoreConfig::default());
        let peer_applied = new_applied_write_map();
        let peer_router =
            KvRouter::global(Arc::clone(&peer_store)).with_applied(Arc::clone(&peer_applied));
        assert!(peer_router.apply_stamped("big", large_value(1), 0, older).unwrap());
        let peer_addr = spawn_peer(peer_router).await;

        // This (non-replica) node holds the NEWER value whose fan-out failed.
        let local_store = Store::new(StoreConfig::default());
        let local_router =
            KvRouter::global(Arc::clone(&local_store)).with_applied(new_applied_write_map());
        assert!(local_router.apply_stamped("big", large_value(2), 0, newer).unwrap());

        let mut ring = vec![node_placeholder("n1"), node_placeholder("n2")];
        ring.sort_by(|a, b| a.id.cmp(&b.id));
        let owner_id = replica_nodes_for(&ring, hash_key("big"), 1)[0].id.clone();
        let self_id = if owner_id == "n1" { "n2" } else { "n1" };
        let mut alive = vec![node_at(&owner_id, peer_addr), node_placeholder(self_id)];
        alive.sort_by(|a, b| a.id.cmp(&b.id));

        let ae = test_reconciler(self_id, local_router, peer_addr.port());
        let outcome = ae.reconcile_key(&alive, "", &local_store, "big").await;
        assert_eq!(outcome, KeyOutcome::Repaired);
        // The replica now holds the newer value and its stamp.
        assert_eq!(
            peer_store.get("big").as_deref(),
            Some(large_value(2).as_slice()),
            "the replica adopted the pushed strictly-newer copy"
        );
        assert_eq!(*peer_applied.get("big").unwrap(), newer, "and its stamp");
        // The local copy is left in place: it now equals the replica set's
        // stamp, and an equal copy is never dropped (broadcast-safety).
        assert!(local_store.exists("big"), "the local copy is kept after a repair push");
    }
}
