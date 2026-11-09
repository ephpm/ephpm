//! The ownership gate — the primary exactly-once guarantee.
//!
//! A node only *considers* a job it is responsible for: a job with a `site`
//! runs on that site's rendezvous-hash (HRW) owner; a siteless job runs on the
//! elected cluster leader (lowest-ordinal alive, non-draining — the same rule
//! the single-database primary election uses). In single-node mode this node
//! owns everything.
//!
//! The decisions here are **pure functions over a membership snapshot** the
//! orchestrator takes once per tick (`ClusterHandle::nodes().await`), which
//! keeps them deterministic and unit-testable, and reuses `hrw_owner` directly
//! (as `sql_forward` does) rather than reinventing ownership. The gate is the
//! cheap usual answer; the per-tick KV lease ([`super::lease`]) is the
//! correctness backstop for the brief window during an ownership move when two
//! nodes might both pass the gate.
//!
//! **Fail-closed:** when responsibility cannot be confirmed (no alive owner /
//! leader in the snapshot) the result is [`Responsibility::Unknown`], which the
//! orchestrator treats as "do not run" — never as "assume mine".

use ephpm_cluster::{NodeInfo, NodeState, hrw_owner};

/// Whether this node is responsible for a job this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Responsibility {
    /// This node owns the job and should consider firing it.
    Yes,
    /// Another node owns it; skip quietly.
    No,
    /// Ownership could not be determined from the snapshot; skip loudly and
    /// never run (fail closed).
    Unknown,
}

/// Is `self_id` the HRW owner of `site` among the alive, non-draining members?
///
/// Delegates to `hrw_owner`, which filters alive and skips draining nodes
/// (falling back to all alive only when every node is draining), so ownership
/// moves off a draining node exactly as the write path's does.
#[must_use]
pub fn responsible_for_site(nodes: &[NodeInfo], self_id: &str, site: &str) -> Responsibility {
    match hrw_owner(nodes, site) {
        Some(owner) if owner.id == self_id => Responsibility::Yes,
        Some(_) => Responsibility::No,
        None => Responsibility::Unknown,
    }
}

/// Is `self_id` the elected cron leader — the lowest-ordinal alive,
/// non-draining node?
#[must_use]
pub fn responsible_as_leader(nodes: &[NodeInfo], self_id: &str) -> Responsibility {
    let leader = nodes
        .iter()
        .filter(|n| n.state == NodeState::Alive && !n.draining)
        .min_by(|a, b| a.id.cmp(&b.id));
    match leader {
        Some(n) if n.id == self_id => Responsibility::Yes,
        Some(_) => Responsibility::No,
        None => Responsibility::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alive(id: &str) -> NodeInfo {
        NodeInfo::new(id, format!("{id}:7000"), NodeState::Alive)
    }

    fn draining(id: &str) -> NodeInfo {
        let mut n = alive(id);
        n.draining = true;
        n
    }

    #[test]
    fn leader_is_lowest_ordinal_alive() {
        let nodes = [alive("ephpm-2"), alive("ephpm-0"), alive("ephpm-1")];
        assert_eq!(responsible_as_leader(&nodes, "ephpm-0"), Responsibility::Yes);
        assert_eq!(responsible_as_leader(&nodes, "ephpm-1"), Responsibility::No);
        assert_eq!(responsible_as_leader(&nodes, "ephpm-2"), Responsibility::No);
    }

    #[test]
    fn leader_skips_draining_and_dead() {
        let mut dead = alive("ephpm-0");
        dead.state = NodeState::Dead;
        let nodes = [dead, draining("ephpm-1"), alive("ephpm-2")];
        // 0 is dead, 1 is draining → 2 leads.
        assert_eq!(responsible_as_leader(&nodes, "ephpm-2"), Responsibility::Yes);
        assert_eq!(responsible_as_leader(&nodes, "ephpm-1"), Responsibility::No);
    }

    #[test]
    fn leader_unknown_when_none_eligible() {
        let nodes = [draining("ephpm-0"), draining("ephpm-1")];
        assert_eq!(responsible_as_leader(&nodes, "ephpm-0"), Responsibility::Unknown);
    }

    #[test]
    fn site_owner_is_exactly_one_node() {
        let nodes = [alive("ephpm-0"), alive("ephpm-1"), alive("ephpm-2")];
        // Whatever the hash picks, exactly one node is Yes and the rest No —
        // this is the exactly-once gate for a per-site job.
        let yes: Vec<_> = ["ephpm-0", "ephpm-1", "ephpm-2"]
            .iter()
            .filter(|id| responsible_for_site(&nodes, id, "wholesale") == Responsibility::Yes)
            .collect();
        assert_eq!(yes.len(), 1, "exactly one node owns a site");
    }

    #[test]
    fn site_owner_unknown_without_members() {
        assert_eq!(responsible_for_site(&[], "ephpm-0", "s"), Responsibility::Unknown);
    }

    #[test]
    fn site_ownership_moves_off_a_draining_node() {
        // Find the site a given node owns, then mark it draining and confirm
        // ownership moves to a different, still-alive node (clean move).
        let site = "catalog";
        let all = ["ephpm-0", "ephpm-1", "ephpm-2"];
        let nodes: Vec<NodeInfo> = all.iter().map(|id| alive(id)).collect();
        let owner = all
            .iter()
            .find(|id| responsible_for_site(&nodes, id, site) == Responsibility::Yes)
            .copied()
            .unwrap();

        let drained: Vec<NodeInfo> =
            all.iter().map(|id| if *id == owner { draining(id) } else { alive(id) }).collect();
        let new_owner = all
            .iter()
            .find(|id| responsible_for_site(&drained, id, site) == Responsibility::Yes)
            .copied()
            .unwrap();
        assert_ne!(owner, new_owner, "ownership moves off the draining node");
        // And the old owner now answers No for that site.
        assert_eq!(responsible_for_site(&drained, owner, site), Responsibility::No);
    }
}
