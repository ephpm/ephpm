//! Per-site chain-global version and the warm-copy settle decision (#654).
//!
//! # Why a version exists
//!
//! In per-site clustered mode a site's `turso_cdc.change_id` sequence **resets
//! every ownership stint** (each owner captures into its own physical log,
//! keyed by its own `__ephpm_cdc_log_id`). So a litewire watermark is only
//! comparable against the one log an owner's `Hello` names, and there is no
//! cluster-global "how fresh is this copy of the site" number that two nodes
//! can compare. That absence is what let a **returning owner serve its stale
//! copy** and silently drop the writes an interim owner accepted while it was
//! away (#654, failure mode 1), and what let a **restarting node briefly own
//! every site** on self-only membership and take writes that are then lost
//! (#654, failure mode 2).
//!
//! [`SiteVersion`] closes that gap. It is `(epoch, head)` ordered
//! lexicographically:
//!
//! - `epoch` is a per-site monotonic counter bumped once each time a node
//!   **settles as a new owner and begins serving** — i.e. once per genuine
//!   ownership handover, never on a plain heartbeat. It orders copies **across**
//!   ownership stints: a copy written under a later stint always out-ranks one
//!   written under an earlier stint, whatever each stint's local `change_id`
//!   happens to be.
//! - `head` is `MAX(turso_cdc.change_id)` within the current stint. It orders
//!   copies **within** one stint (a replica that has applied more of the
//!   current owner's log out-ranks one that has applied less).
//!
//! The current owner advertises its site version in gossip; a node settling a
//! **warm** local copy reads that value and refuses to serve while it is behind
//! it (see [`decide_warm_settle`]), catching up first. A monotonic epoch is
//! exactly the "per-log positions as a version vector" the design calls for,
//! reduced to the single comparison the serving gate needs.

use std::cmp::Ordering;

/// A per-site copy's position in the site's chain of ownership stints.
///
/// Ordered lexicographically by `(epoch, head)`: `epoch` dominates (it orders
/// across ownership stints), `head` breaks ties within one stint. Two copies
/// are equal only when both match, which is the "same owner, same applied
/// position" case — a genuine fast restart, where the warm copy is
/// authoritative and may be served at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SiteVersion {
    /// Per-site ownership epoch; strictly increases on each settle-as-new-owner.
    pub(crate) epoch: u64,
    /// `MAX(turso_cdc.change_id)` within the current stint; monotone per stint.
    pub(crate) head: i64,
}

impl Ord for SiteVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.epoch.cmp(&other.epoch).then_with(|| self.head.cmp(&other.head))
    }
}

impl PartialOrd for SiteVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Encode a site version for the gossip advertisement value
/// (`kv:sqlite:ver:<site>`): `"<epoch>:<head>"`. Pure ASCII, no separators that
/// collide (both fields are plain integers).
pub(crate) fn encode(v: SiteVersion) -> Vec<u8> {
    format!("{}:{}", v.epoch, v.head).into_bytes()
}

/// Decode a gossip-advertised site version. `None` on anything malformed, which
/// the caller treats as "no usable peer version" — fail-safe (it will not block
/// a settle on a value it cannot parse, and it will not serve stale either,
/// because a missing peer version only ever *permits*, never *forces*, serving).
pub(crate) fn decode(bytes: &[u8]) -> Option<SiteVersion> {
    let s = std::str::from_utf8(bytes).ok()?;
    let (epoch, head) = s.split_once(':')?;
    Some(SiteVersion { epoch: epoch.parse().ok()?, head: head.parse().ok()? })
}

/// The next epoch a node adopts when it settles as a **new** owner of a site:
/// strictly greater than both its own recorded epoch and the freshest epoch any
/// peer advertises, so the version is monotonic across the whole cluster even
/// when ownership ping-pongs. A plain fast restart (own epoch already ≥ every
/// peer's) still advances by one, which is harmless — the point is only that it
/// never goes backwards.
pub(crate) fn next_owner_epoch(own: u64, best_peer: u64) -> u64 {
    own.max(best_peer).saturating_add(1)
}

/// What [`decide_warm_settle`] concludes for a node whose local copy of a site
/// is **warm** (holds data). The cold path is unchanged and handled elsewhere
/// (`settle_round`'s ownership-handoff probe).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WarmSettle {
    /// The warm copy may be served from this node now.
    Settled,
    /// Membership has not converged yet; do not serve the warm copy as owner
    /// until it has (closes failure mode 2 — the self-only-membership window).
    AwaitMembership,
    /// This node owns the site but a peer advertises a strictly fresher copy:
    /// an interim owner advanced the site while this node was away. Refuse to
    /// serve the stale copy and catch up first (closes failure mode 1).
    Blocked,
}

/// Decide whether a **warm** local copy of a site may be served from this node.
///
/// This is the whole of #654's serving rule, as a pure function so the
/// ownership/settle/restart orderings are unit-testable without a live cluster
/// (the same style as `turso_cdc::handoff_verdict` and
/// `plan_replica_attempt`). Inputs:
///
/// - `converged`: has membership been stable long enough to trust (the same
///   `HANDOFF_SETTLE_GRACE` the cold "new site" verdict already requires)?
///   Right after a restart the view is self-only, so every site looks locally
///   owned; waiting for convergence is what stops a restarting node serving —
///   and accepting writes onto — copies it does not really own.
/// - `is_owner`: is this node the site's uncontested HRW owner over the
///   **non-draining** alive set? A warm **replica** (`is_owner == false`) is
///   fine to settle: reads are served from it and the write path forwards to
///   the owner without consulting this gate, so a replica serving a slightly
///   stale read is the design's eventual consistency, not the bug.
/// - `own` / `best_peer`: this copy's [`SiteVersion`] and the freshest a peer
///   advertises for the site (`None` when no peer advertises one).
///
/// The truth table (warm copy only):
///
/// | converged | is_owner | peer strictly ahead | verdict |
/// |-----------|----------|---------------------|---------|
/// | no        | —        | —                   | `AwaitMembership` |
/// | yes       | no       | —                   | `Settled` |
/// | yes       | yes      | no                  | `Settled` |
/// | yes       | yes      | yes                 | `Blocked` |
///
/// The fast-restart case (a node that was the owner, restarts, and comes back
/// owner with no interim owner having taken over) has `best_peer <= own`, so it
/// still serves immediately: the epoch is precisely what lets a legitimate fast
/// restart avoid paying a convergence-length wait every time.
pub(crate) fn decide_warm_settle(
    converged: bool,
    is_owner: bool,
    own: SiteVersion,
    best_peer: Option<SiteVersion>,
) -> WarmSettle {
    if !converged {
        return WarmSettle::AwaitMembership;
    }
    if !is_owner {
        return WarmSettle::Settled;
    }
    match best_peer {
        Some(peer) if peer > own => WarmSettle::Blocked,
        _ => WarmSettle::Settled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(epoch: u64, head: i64) -> SiteVersion {
        SiteVersion { epoch, head }
    }

    #[test]
    fn epoch_dominates_head_in_ordering() {
        // A later stint beats an earlier one regardless of local head, because
        // change_id resets each stint.
        assert!(v(2, 0) > v(1, 1_000_000));
        // Within a stint, more applied wins.
        assert!(v(3, 50) > v(3, 49));
        // Equal is equal (same owner, same position — a genuine fast restart).
        assert_eq!(v(4, 7), v(4, 7));
        // The all-zero version (a copy that never settled) loses to every real
        // version — the fail-safe direction (it never out-ranks a peer).
        assert!(v(0, 0) < v(1, 0));
        assert!(v(0, 0) < v(0, 1));
    }

    #[test]
    fn encode_decode_roundtrip() {
        for ver in [v(0, 0), v(1, 0), v(7, 42), v(u64::MAX, i64::MAX)] {
            assert_eq!(decode(&encode(ver)), Some(ver));
        }
        // Malformed values decode to None (treated as "no peer version").
        assert_eq!(decode(b"not-a-version"), None);
        assert_eq!(decode(b"1"), None);
        assert_eq!(decode(b"1:2:3"), None);
        assert_eq!(decode(b""), None);
    }

    #[test]
    fn next_owner_epoch_is_strictly_monotonic() {
        // Beats both own and the freshest peer, so a ping-ponging ownership
        // never reuses or lowers an epoch.
        assert_eq!(next_owner_epoch(5, 3), 6);
        assert_eq!(next_owner_epoch(3, 5), 6);
        assert_eq!(next_owner_epoch(0, 0), 1);
        // Saturates rather than wrapping (a wrap would be a silent regression
        // that re-opens mode 1).
        assert_eq!(next_owner_epoch(u64::MAX, 0), u64::MAX);
    }

    #[test]
    fn unconverged_membership_never_serves_a_warm_copy_as_owner() {
        // Failure mode 2: right after a restart the view is self-only and every
        // site looks locally owned. Regardless of versions, do not serve yet.
        assert_eq!(decide_warm_settle(false, true, v(9, 9), None), WarmSettle::AwaitMembership);
        assert_eq!(
            decide_warm_settle(false, true, v(9, 9), Some(v(1, 0))),
            WarmSettle::AwaitMembership
        );
    }

    #[test]
    fn a_warm_replica_serves_once_converged() {
        // Not the owner: reads are served from the local replica, the write
        // path forwards to the owner and never consults this gate.
        assert_eq!(decide_warm_settle(true, false, v(1, 0), Some(v(5, 0))), WarmSettle::Settled);
    }

    #[test]
    fn a_returning_owner_behind_a_peer_blocks_until_caught_up() {
        // Failure mode 1: this node owns the site again but an interim owner
        // advanced it (higher epoch). Refuse the stale copy; catch up first.
        assert_eq!(decide_warm_settle(true, true, v(1, 100), Some(v(2, 0))), WarmSettle::Blocked);
        // Behind within the same stint, too (an owner restart that missed the
        // tail of its own stint — the warm replica interim-owner case).
        assert_eq!(decide_warm_settle(true, true, v(2, 100), Some(v(2, 250))), WarmSettle::Blocked);
    }

    /// **#654 crash-restart, closed by the durable epoch.** The dead owner A
    /// settled at epoch 1. The interim owner B was a warm replica that
    /// **adopted** A's epoch durably (from `Frame::Hello` / `SnapshotHeader`)
    /// before it was promoted; with A dead there is **no live advertisement**
    /// to read, so B bumps from the *adopted* epoch, landing strictly above A's
    /// stint rather than colliding with it. A then returns warm and must refuse
    /// its stale copy — decided by epoch, not by the per-stint `change_id` head
    /// (which resets each stint and cannot order across them).
    #[test]
    fn durable_epoch_blocks_a_returning_owner_after_a_crash_handover() {
        // A dies at epoch 1, head 500 in its stint.
        let a_stale = v(1, 500);

        // B adopted A's epoch (1) durably as a replica, then is promoted with A
        // dead (no live advert ⇒ best_peer epoch 0). It bumps from the adopted
        // value, clearing A's stint.
        let b_epoch = next_owner_epoch(1, 0);
        assert_eq!(b_epoch, 2, "adopted-then-bumped lands above the dead owner's stint");

        // B serves at epoch 2. Its head is small — a fresh stint resets
        // change_id — and deliberately *below* A's stale head, to prove epoch,
        // not head, decides.
        let b_now = v(b_epoch, 3);
        assert!(b_now > a_stale, "epoch 2 out-ranks stale epoch 1 regardless of head");
        assert_eq!(
            decide_warm_settle(true, true, a_stale, Some(b_now)),
            WarmSettle::Blocked,
            "the returning owner must refuse its stale copy and catch up"
        );

        // Regression guard for the gap this fix closes: WITHOUT durable
        // adoption, B would have bumped from its own never-adopted 0 to 1,
        // colliding with A's epoch 1 — and then the per-stint head (A's 500 >
        // B's 3) would have let the returning owner wrongly serve stale.
        assert_eq!(next_owner_epoch(0, 0), 1, "the collision durable adoption removes");
        let b_collided = v(1, 3);
        assert_eq!(
            decide_warm_settle(true, true, a_stale, Some(b_collided)),
            WarmSettle::Settled,
            "pre-fix: a colliding epoch lets head decide, and A wrongly wins"
        );
    }

    #[test]
    fn a_fast_restart_owner_serves_immediately() {
        // No interim owner took over: our version is at least the freshest a
        // peer advertises, so we serve at once — no convergence-length wait.
        assert_eq!(decide_warm_settle(true, true, v(5, 300), Some(v(5, 300))), WarmSettle::Settled);
        assert_eq!(decide_warm_settle(true, true, v(5, 300), Some(v(5, 10))), WarmSettle::Settled);
        // No peer advertises a version at all (a genuinely isolated owner).
        assert_eq!(decide_warm_settle(true, true, v(5, 300), None), WarmSettle::Settled);
    }
}
