//! Graceful leave of a per-site clustered node.
//!
//! # Why this exists
//!
//! Per-site ownership is rendezvous hashing over the *alive* nodes, and a node
//! is alive to its peers until the failure detector stops hearing from it.
//! Chitchat has no leave message, so a node shutting down stayed the owner of
//! its sites until its process had exited **and** the detector had noticed
//! (24–40 s for a recently joined peer). Shutdown itself closes the cluster
//! channel and then runs the final WAL checkpoint, which under load took
//! 128 s in the v0.13.0 release-candidate stress run. For that whole time the
//! peers kept forwarding the node's sites to a channel that was already
//! closed: 42,676 HTTP 500s on the two surviving nodes.
//!
//! # The order
//!
//! `serve()` runs this module between the HTTP drain and the database shutdown:
//!
//! 1. **HTTP drain.** The node is still a full owner. Peers' forwarded
//!    statements are served as before, and this node's own PHP requests finish.
//! 2. **Announce the leave** ([`announce_leave`]).
//!    [`ClusterHandle::mark_draining`] sets a gossip key, and from that moment
//!    `hrw_owner` skips this node on every node: here at once, on each peer at
//!    its next gossip exchange (about a second). The node's sites re-home to
//!    the next node in each site's HRW order, which is also the node that
//!    would take them if this one died. Right after it, `serve()` stops the
//!    per-site replication drivers ([`crate::turso_cdc::stop_replication_drivers`];
//!    process-wide, which is why it is not part of [`announce_leave`] and the
//!    in-process multi-node tests can announce one node's leave alone).
//!    Otherwise a driver would follow its site's new owner as a replica, and a
//!    leaving node has no use for that copy. On a divergence it could even
//!    discard the copy that a cold new owner is about to pull.
//! 3. **Hand over** ([`Leave::await_handover`]). The channel stays open for
//!    at least [`MIN_HANDOVER`] after the announcement, then for as long as a
//!    snapshot is being served, up to [`MAX_HANDOVER`]. During that window:
//!    - forwarded statements are **refused** with a connection-shaped error
//!      (`sql_forward`: the HRW gate refuses new streams, the per-statement
//!      drain check ends existing ones). The peer's client recycles its session
//!      and re-resolves the site. Only a peer that has not yet seen the
//!      announcement still dials this node, so the errors last about one
//!      gossip round instead of the whole shutdown. Serving them would accept
//!      writes on a node that is no longer the owner, while the new owner may
//!      already be accepting writes of its own (a fork).
//!    - `snapshot/<site>` is **still served** to each site's new owner. A cold
//!      new owner asks its peers in HRW rank order, the draining node ranks by
//!      its real score (usually first), and it holds the freshest copy of the
//!      sites it owned. So a cold new owner pulls the data from the node that is
//!      leaving rather than from a replica that may be behind, or from nobody.
//!    - `cdc/<site>` streams are refused. They are owner-only, and this node
//!      owns nothing now.
//! 4. **Close the channel, stop writers, final checkpoint** (`db_shutdown`, with
//!    the #613 ordering unchanged). No peer is forwarding to the node by then,
//!    so however long the checkpoint takes, it costs no requests.
//!
//! # What this does not do
//!
//! - A warm replica that becomes the owner serves from its own copy at once.
//!   Writes the leaving owner acknowledged but had not yet shipped to it are
//!   not carried over. That is the same window as any ownership move (a node
//!   joining, a node dying), and the reason the upgrade guide asks for zero
//!   lag before a planned restart.
//! - Whole-database clustered mode does not use HRW. Its single primary is
//!   elected by lowest node id over the alive nodes, so the announcement does
//!   not move it. `serve()` announces only in per-site clustered mode.
//! - KV large-value ownership (`hash(key) % alive`) is unchanged. The KV
//!   handoff push in `serve()` covers it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ephpm_cluster::ClusterHandle;
use tokio::time::Instant;

/// The shortest time the cluster channel stays open after the announcement.
///
/// Gossip runs every second, so peers see the announcement within about a
/// second or two. A cold new owner's serving gate re-checks ownership every
/// 2 s (`HANDOFF_OWNERSHIP_RECHECK` in `turso_cdc`). Five seconds covers both,
/// so a new owner that needs this node's copy has started pulling it before
/// the wait below looks at the in-flight count.
pub const MIN_HANDOVER: Duration = Duration::from_secs(5);

/// The longest the channel stays open after the announcement while snapshots
/// are still being served. After this the node closes the channel anyway, and
/// a new owner still pulling falls back to its other peers. Shutdown must not
/// wait forever on a slow peer.
pub const MAX_HANDOVER: Duration = Duration::from_secs(30);

/// Snapshot streams being served right now, by `turso_cdc::serve_snapshot`.
///
/// Process-wide. One process is one node, except in in-process multi-node
/// tests, where the count is shared and only makes [`Leave::await_handover`]
/// wait longer (still bounded by [`MAX_HANDOVER`]).
static SNAPSHOTS_SERVING: AtomicUsize = AtomicUsize::new(0);

/// Counts one snapshot serve for as long as it lives (see
/// [`snapshot_serve_started`]).
pub(crate) struct SnapshotServe(());

impl Drop for SnapshotServe {
    fn drop(&mut self) {
        SNAPSHOTS_SERVING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Count a snapshot serve until the returned guard drops, whichever way it
/// ends.
pub(crate) fn snapshot_serve_started() -> SnapshotServe {
    SNAPSHOTS_SERVING.fetch_add(1, Ordering::SeqCst);
    SnapshotServe(())
}

/// Snapshot serves in flight on this process.
#[must_use]
pub fn snapshots_in_flight() -> usize {
    SNAPSHOTS_SERVING.load(Ordering::SeqCst)
}

/// A leave that has been announced; see [`announce_leave`].
#[derive(Debug, Clone, Copy)]
pub struct Leave {
    announced_at: Instant,
}

/// Announce that this node is leaving: mark it draining in gossip. Step 2 of
/// the module docs. The caller stops the per-site replication drivers right
/// after (`serve()` does).
///
/// From this call on, this node computes itself as owner of no site (unless
/// every alive node is draining, see `hrw_owner`). So its own HRW gates refuse
/// forwarded statements and CDC subscriptions immediately, and its peers follow
/// as the announcement reaches them.
pub async fn announce_leave(cluster: &ClusterHandle) -> Leave {
    cluster.mark_draining().await;
    Leave { announced_at: Instant::now() }
}

impl Leave {
    /// Keep serving snapshots until the handover is done; step 3 of the module
    /// docs. Returns once [`MIN_HANDOVER`] has passed since the announcement
    /// and no snapshot is being served, or at [`MAX_HANDOVER`] at the latest.
    pub async fn await_handover(&self) {
        self.await_handover_counting(snapshots_in_flight).await;
    }

    /// [`Self::await_handover`] over an injectable in-flight count, so the
    /// timing is testable without the process-wide counter.
    async fn await_handover_counting(&self, in_flight: impl Fn() -> usize) {
        let deadline = self.announced_at + MAX_HANDOVER;
        tokio::time::sleep_until(self.announced_at + MIN_HANDOVER).await;
        let mut logged = false;
        loop {
            let serving = in_flight();
            if serving == 0 {
                break;
            }
            if Instant::now() >= deadline {
                tracing::warn!(
                    serving,
                    waited_secs = MAX_HANDOVER.as_secs(),
                    "closing the cluster channel with snapshots still being served to new site \
                     owners; they will pull from another peer"
                );
                return;
            }
            if !logged {
                tracing::info!(serving, "waiting for new site owners to finish pulling snapshots");
                logged = true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tracing::info!(
            elapsed_ms = u64::try_from(self.announced_at.elapsed().as_millis()).unwrap_or(u64::MAX),
            "per-site handover finished; closing the cluster channel"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_guard_counts_and_releases() {
        let before = snapshots_in_flight();
        let a = snapshot_serve_started();
        let b = snapshot_serve_started();
        assert!(snapshots_in_flight() >= before + 2);
        drop(a);
        drop(b);
        // Other tests may be serving snapshots concurrently in this process;
        // ours are released.
        assert!(snapshots_in_flight() <= before + 2);
    }

    #[tokio::test(start_paused = true)]
    async fn handover_waits_the_minimum_then_for_in_flight_serves_up_to_the_cap() {
        // Paused clock: sleeps advance virtual time instantly. A private
        // counter, so snapshot serves elsewhere in this process cannot skew it.
        let serving = std::sync::Arc::new(AtomicUsize::new(0));
        let count = || {
            let serving = std::sync::Arc::clone(&serving);
            move || serving.load(Ordering::SeqCst)
        };

        // Nothing in flight: exactly the minimum.
        let leave = Leave { announced_at: Instant::now() };
        let started = Instant::now();
        leave.await_handover_counting(count()).await;
        assert_eq!(started.elapsed(), MIN_HANDOVER);

        // A serve that outlasts the minimum is waited for.
        serving.store(1, Ordering::SeqCst);
        let release = {
            let serving = std::sync::Arc::clone(&serving);
            tokio::spawn(async move {
                tokio::time::sleep(MIN_HANDOVER + Duration::from_secs(3)).await;
                serving.store(0, Ordering::SeqCst);
            })
        };
        let leave = Leave { announced_at: Instant::now() };
        let started = Instant::now();
        leave.await_handover_counting(count()).await;
        let waited = started.elapsed();
        assert!(waited >= MIN_HANDOVER + Duration::from_secs(3), "waited {waited:?}");
        assert!(waited < MAX_HANDOVER, "waited {waited:?}");
        release.await.unwrap();

        // A serve that never ends is abandoned at the cap.
        serving.store(1, Ordering::SeqCst);
        let leave = Leave { announced_at: Instant::now() };
        let started = Instant::now();
        leave.await_handover_counting(count()).await;
        let waited = started.elapsed();
        assert!(waited >= MAX_HANDOVER && waited < MAX_HANDOVER + Duration::from_secs(1));
    }
}
