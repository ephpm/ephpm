//! Per-site serving gate for per-site clustered mode's ownership handoff.
//!
//! # The problem
//!
//! In per-site clustered mode a site's owner is a pure function of live
//! membership (HRW), and the serving path follows it immediately: the moment a
//! node computes itself as a site's owner it serves the site from its local
//! database. Ownership moves no bytes, though. A node that becomes the owner of
//! a site it holds **no copy of** (a node joining, a full-cluster restart after
//! the ownership hash changed, a replica that never finished bootstrapping)
//! would serve an empty database: the tenant sees its site empty, its first
//! write lands on that empty file and makes it "warm", and from then on the
//! node's copy has diverged from the data that still sits on another node.
//!
//! # The gate
//!
//! While the replication plane is running ([`HandoffGate::arm`]), a site is
//! served locally only once its per-site driver has **settled** it
//! ([`SiteServeState::Ready`]): the local copy holds data, or a peer's copy was
//! pulled into it, or every alive peer answered that it holds no copy (a
//! genuinely new site). Until then the site is [`SiteServeState::Pending`] and
//! the serving path waits a short, bounded time for it; when the driver cannot
//! settle it because a peer that might hold the data is unreachable, the site is
//! [`SiteServeState::Blocked`] and every statement is refused at once with the
//! reason. Refusing is the deliberate choice: a refused write fails loudly and
//! can be retried, while a write accepted onto an empty copy silently forks the
//! tenant's data.
//!
//! `Ready` is sticky. Once a node's copy of a site holds data it never becomes
//! cold again (a re-bootstrap discards and re-applies inside one scope), so a
//! site never has to be re-settled.
//!
//! An **unarmed** gate (single-node per-site mode, or a registry used without
//! the replication plane, as some tests do) reports every site `Ready`: there
//! is no driver to settle anything and no peer that could hold the data.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::watch;

/// How long a request for a not-yet-settled site waits for its driver before
/// it is refused. Bounded because the waiter can be a PHP worker thread (the
/// `ephpm_db_*` bridge resolves synchronously); a pull of a large site can
/// take longer than this, and those requests are refused with a retryable
/// message rather than pinning the worker pool.
pub(crate) const SERVE_WAIT: Duration = Duration::from_secs(5);

/// Whether a site may be served from this node's local database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SiteServeState {
    /// The site's driver has not settled it yet: its local copy may be empty
    /// while a peer holds the data.
    Pending,
    /// The local copy may be served.
    Ready,
    /// The driver could not settle the site — a peer that might hold its data
    /// did not answer — and keeps retrying. Carries the reason for the refusal.
    Blocked(String),
}

/// The per-node set of [`SiteServeState`]s. See the module docs.
#[derive(Debug, Default)]
pub(crate) struct HandoffGate {
    armed: AtomicBool,
    sites: DashMap<String, watch::Sender<SiteServeState>>,
    /// Node-wide refusal while an alive peer computes site ownership with a
    /// different HRW scheme (see [`set_scheme_block`](Self::set_scheme_block)).
    /// Carries the mismatched peers, for the refusal message.
    scheme_block: std::sync::RwLock<Option<String>>,
}

impl HandoffGate {
    /// Start enforcing the gate. Called once by the per-site clustered
    /// replication plane, which runs the drivers that settle sites.
    pub(crate) fn arm(&self) {
        self.armed.store(true, Ordering::Release);
    }

    fn is_armed(&self) -> bool {
        self.armed.load(Ordering::Acquire)
    }

    fn sender(&self, site: &str) -> watch::Sender<SiteServeState> {
        if let Some(tx) = self.sites.get(site) {
            return tx.value().clone();
        }
        self.sites
            .entry(site.to_string())
            .or_insert_with(|| watch::channel(SiteServeState::Pending).0)
            .value()
            .clone()
    }

    /// Record `site`'s state (the driver's side).
    pub(crate) fn set(&self, site: &str, state: SiteServeState) {
        self.sender(site).send_if_modified(|current| {
            if *current == state {
                false
            } else {
                *current = state;
                true
            }
        });
    }

    /// Refuse (`Some(peers)`) or resume (`None`) serving **every** site as its
    /// owner, node-wide, because of an ownership-hash scheme mismatch.
    ///
    /// # Why fail closed
    ///
    /// Nodes on different HRW schemes (a v0.12.x node and a v0.13.0 node
    /// during a forbidden rolling upgrade) each compute themselves the owner
    /// of different sites, so for some site both would accept writes — two CDC
    /// histories for one tenant that no lineage rule reconciles. An older node
    /// cannot know the new scheme exists, so only the newer side can step
    /// back: while a mismatch is confirmed, this node serves no site from its
    /// local database (reads included — its copy may be behind writes the
    /// older nodes are still accepting, and a read through the resolver is
    /// indistinguishable from the start of a write session). Forwarding is
    /// unaffected: a statement forwarded to an older owner is accepted there
    /// by the older scheme, and the older nodes agree among themselves, so the
    /// cluster keeps exactly one writer per site.
    pub(crate) fn set_scheme_block(&self, peers: Option<String>) {
        let mut guard =
            self.scheme_block.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = peers;
    }

    fn scheme_block(&self) -> Option<String> {
        self.scheme_block.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    /// `site`'s current state. Always `Ready` on an unarmed gate.
    pub(crate) fn state(&self, site: &str) -> SiteServeState {
        if !self.is_armed() {
            return SiteServeState::Ready;
        }
        if let Some(peers) = self.scheme_block() {
            return SiteServeState::Blocked(scheme_message(site, &peers));
        }
        match self.sites.get(site) {
            Some(tx) => tx.value().borrow().clone(),
            None => SiteServeState::Pending,
        }
    }

    /// Wait up to `timeout` for `site` to become servable.
    ///
    /// # Errors
    ///
    /// Returns the refusal message when the site is
    /// [`Blocked`](SiteServeState::Blocked), or still
    /// [`Pending`](SiteServeState::Pending) once `timeout` has elapsed.
    pub(crate) async fn wait_serveable(&self, site: &str, timeout: Duration) -> Result<(), String> {
        if !self.is_armed() {
            return Ok(());
        }
        if let Some(peers) = self.scheme_block() {
            return Err(scheme_message(site, &peers));
        }
        let mut rx = self.sender(site).subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let state = rx.borrow_and_update().clone();
            match state {
                SiteServeState::Ready => return Ok(()),
                SiteServeState::Blocked(reason) => return Err(blocked_message(site, &reason)),
                SiteServeState::Pending => {}
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => {}
                // The sender lives in the map for the process's life, so a
                // closed channel cannot happen; treat it like a timeout.
                Ok(Err(_)) | Err(_) => return Err(pending_message(site)),
            }
        }
    }
}

/// The refusal while an alive peer runs a different ownership-hash scheme.
fn scheme_message(site: &str, peers: &str) -> String {
    format!(
        "site {site} is not being served by this node: cluster peers compute per-site ownership \
         with a different hash scheme ({peers}), so this node and those peers would both accept \
         writes for some sites. Refusing to act as owner until every node runs the same ePHPm \
         version (per-site clustered mode needs a full-cluster restart across v0.13.0, not a \
         rolling upgrade)."
    )
}

/// The refusal for a site whose handoff is still in progress.
fn pending_message(site: &str) -> String {
    format!(
        "site {site} is not being served on this node yet: this node has just become the site's \
         owner and holds no copy of its data, and it is pulling the data from a peer before \
         serving it (ownership handoff). Retry shortly."
    )
}

/// The refusal for a site whose handoff cannot complete right now.
fn blocked_message(site: &str, reason: &str) -> String {
    format!(
        "site {site} is not being served: this node owns the site but holds no copy of its data, \
         and a peer that may hold it could not be reached ({reason}). Refusing to serve an empty \
         database, which would fork the site's data; the handoff keeps retrying."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_unarmed_gate_serves_everything() {
        let gate = HandoffGate::default();
        assert_eq!(gate.state("a.test"), SiteServeState::Ready);
        gate.wait_serveable("a.test", Duration::ZERO).await.expect("unarmed gate never refuses");
    }

    #[tokio::test]
    async fn an_armed_gate_holds_unsettled_sites() {
        let gate = HandoffGate::default();
        gate.arm();
        assert_eq!(gate.state("a.test"), SiteServeState::Pending);
        let err = gate.wait_serveable("a.test", Duration::from_millis(20)).await.unwrap_err();
        assert!(err.contains("ownership handoff"), "{err}");
    }

    #[tokio::test]
    async fn a_blocked_site_is_refused_immediately_with_the_reason() {
        let gate = HandoffGate::default();
        gate.arm();
        gate.set("a.test", SiteServeState::Blocked("peer ephpm-2 timed out".into()));
        let started = std::time::Instant::now();
        let err = gate.wait_serveable("a.test", Duration::from_secs(30)).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5), "a blocked site must not wait");
        assert!(err.contains("peer ephpm-2 timed out"), "{err}");
    }

    /// A confirmed ownership-hash mismatch refuses every site — settled ones
    /// included — naming the peers; once every peer agrees, serving resumes.
    #[tokio::test]
    async fn a_scheme_mismatch_refuses_owner_serving_until_peers_agree() {
        let gate = HandoffGate::default();
        gate.arm();
        gate.set("a.test", SiteServeState::Ready);

        gate.set_scheme_block(Some("ephpm-1 (none: pre-v0.13.0)".into()));
        assert!(matches!(gate.state("a.test"), SiteServeState::Blocked(_)));
        let started = std::time::Instant::now();
        let err = gate.wait_serveable("a.test", Duration::from_secs(30)).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5), "a mismatch must refuse at once");
        assert!(err.contains("ephpm-1") && err.contains("hash scheme"), "{err}");

        gate.set_scheme_block(None);
        assert_eq!(gate.state("a.test"), SiteServeState::Ready);
        gate.wait_serveable("a.test", Duration::ZERO).await.expect("serving resumes");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_waiter_is_released_when_the_driver_settles_the_site() {
        let gate = std::sync::Arc::new(HandoffGate::default());
        gate.arm();
        let waiter = {
            let gate = std::sync::Arc::clone(&gate);
            tokio::spawn(
                async move { gate.wait_serveable("a.test", Duration::from_secs(30)).await },
            )
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        gate.set("a.test", SiteServeState::Ready);
        waiter.await.expect("waiter task").expect("released once the site is ready");
        assert_eq!(gate.state("a.test"), SiteServeState::Ready);
        assert_eq!(gate.state("b.test"), SiteServeState::Pending, "settling is per site");
    }
}
