//! Per-tenant PHP concurrency fairness for multi-tenant (shared-hosting) mode —
//! the mitigation for red-team finding **F-DoS-1**.
//!
//! # The problem
//!
//! The per-request PHP pool ([`crate::fpm_pool`]) is **one global pool** with a
//! fixed number of execution threads ([`ephpm_config::PhpConfig::effective_concurrency`])
//! and strictly FIFO *per-request* admission. One tenant can therefore park every
//! pool thread with slow requests (`sleep()`, a slow upstream, a `while(true)`)
//! and starve every other tenant on the box: the attacker holds the threads, and
//! the victim's requests queue behind them until the attacker's finish. Admission
//! is fair *between requests* but blind to *which tenant* each belongs to, so it
//! does nothing to stop a single tenant monopolising the pool.
//!
//! # The fix: a per-site admission gate in front of the pool
//!
//! This controller sits in front of [`crate::fpm_pool::FpmPool::dispatch`]. Each
//! PHP request for a site acquires a [`SitePermit`] here **before** it occupies a
//! pool thread; a site that is over its ceiling and cannot borrow idle capacity
//! waits (or sheds) at the gate holding **nothing** — no pool thread, no pool
//! queue slot — so a noisy tenant can never occupy slots its neighbours need.
//! The permit is held across the whole pool dispatch + reply, released on drop.
//!
//! It is consulted **only in multi-tenant mode** (a request with a resolved site
//! key). Single-site deployments and requests to an unmatched host bypass it
//! entirely, so the uncontended common path pays nothing.
//!
//! # The two levers (per site; both default-safe)
//!
//! * **`max_concurrency`** — the *ceiling*: the most pool threads a site may hold
//!   at once **under contention**. The safety lever. Its zero-config default is
//!   [`PER_SITE_MAX_FRACTION`] of the pool, floored at
//!   [`PER_SITE_MAX_FLOOR`] — so one hostile site degrades neighbours but cannot
//!   take the whole box out of the box.
//! * **`reserved_concurrency`** — the *floor*: execution slots **other** tenants
//!   may not borrow, held available for this site even under saturation so a VIP
//!   never starves. Default `0`.
//!
//! A proportional `weight` for sharing the contended remainder is a planned v2
//! knob; the config surface is forward-compatible to it but it is not implemented
//! here.
//!
//! # The admission algorithm — **work-conserving**
//!
//! We cannot preempt a running PHP request (there is no mid-request pause), so
//! this is *admission-time* fairness, not preemption. For a request of site `S`
//! with resolved limits `(max, reserved)`, let `inf` be `S`'s current in-flight
//! count, `total` the all-sites in-flight count, `N` the pool's execution-thread
//! count ([`SiteAdmission::capacity`]) and `R` the sum of every site's
//! `reserved`:
//!
//! 1. **Within its guaranteed share (`inf < max`): admit unconditionally**, even
//!    when the box is saturated. This is what protects the victim — its own
//!    requests always enter the pool, where the pool's existing FIFO admission
//!    then orders them fairly against everyone else's. A within-share request
//!    never waits at this gate.
//! 2. **Over its ceiling (`inf >= max`): *borrow* an idle slot** iff a thread is
//!    genuinely idle *and* no other site's reservation would be consumed —
//!    `total + (R - reserved) < N`. A lone busy site thus uses the whole box; a
//!    contended one is held to its ceiling because the box is full
//!    (`total >= N`), so its borrow is refused and it waits/sheds at the gate.
//!
//! The crucial interaction: because a within-share request is admitted to the
//! pool *immediately*, it takes its place in the pool's FIFO queue ahead of any
//! later borrow request from the attacker — so even as the attacker's finished
//! requests try to re-borrow, the pool hands each freed thread to the
//! longest-waiting queued job (the victim) first. The gate blocks the attacker's
//! *over-ceiling* requests under saturation; the pool's FIFO does the final
//! ordering. Neither mechanism alone is enough; together they converge a
//! contended box to each site's ceiling while a lone site still uses all of it.
//!
//! `reserved` is honoured by clause 2's headroom term: borrowers of any site may
//! not consume the last `R` idle slots, so `reserved` threads stay available for
//! the sites that reserved them even when others flood. A site's own `reserved`
//! is subtracted from the headroom it must respect (a site never reserves against
//! itself).
//!
//! # Cost and safety
//!
//! The admission decision is **lock-free atomics** (a per-site `in_flight`
//! counter and two process-wide counters) on the hot path — never a lock, never
//! a wait while holding a pool thread. Over-ceiling requests that must wait park
//! on a [`tokio::sync::Notify`] woken on every release and re-check; within-share
//! requests (the victim, and the common case) never reach that path. The gate
//! cannot deadlock a request that holds resources: a permit is acquired *before*
//! dispatch and never while holding a pool thread, so there is no circular wait —
//! and because clause 2 admits a borrow whenever a thread is idle, a loopback
//! subrequest on an otherwise-idle box is always admitted (the pool's own floor-2
//! thread rule, issue #461, covers the saturated case; keep `max_concurrency >=
//! 2` where loopback subrequests are used).
//!
//! # Future resource envelope
//!
//! The per-site `[limits]` override block that feeds this controller is designed
//! to also carry the F-DoS-2 resource quotas (`max_db_bytes`, `max_memory`); only
//! the two concurrency knobs are implemented today.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use dashmap::DashMap;
use metrics::{Counter, Gauge, counter, gauge};
use tokio::sync::Notify;

/// Default per-site ceiling as a fraction of the pool's execution-thread count,
/// used when `[php] per_site_max_concurrency` is `0` (derive). Named so the
/// derivation is trivial to retune against live DoS numbers. `0.5` means one
/// hostile tenant can degrade its neighbours but cannot, out of the box, take the
/// whole box: it is held to half the pool under contention.
pub const PER_SITE_MAX_FRACTION: f64 = 0.5;

/// Floor under the *derived* default ceiling. Never below `2`: a 1-slot per-site
/// ceiling deadlocks a PHP loopback subrequest (a script that HTTP-requests its
/// own server holds its only slot while waiting for a response that needs
/// another — the same hazard the pool's floor-2 thread rule addresses, #461).
pub const PER_SITE_MAX_FLOOR: usize = 2;

/// The effective per-site concurrency limits for one request, after applying the
/// global `[php]` defaults, any per-site `[limits]` override, and clamping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SiteLimits {
    /// Ceiling: most in-flight pool requests this site may hold under
    /// contention. Always `>= 1` and `<= N`.
    pub max: usize,
    /// Floor: execution slots other tenants may not borrow, held for this site.
    /// Always `<= max`.
    pub reserved: usize,
}

/// Per-site admission state. One per site key ever seen; never removed (the set
/// is bounded by the number of configured vhosts).
struct SiteState {
    /// Requests this site currently has admitted to the pool (queued or
    /// executing). The per-site counter the hot-path decision reads.
    in_flight: AtomicUsize,
    /// This site's last-resolved `reserved`, kept so a changed override can
    /// adjust [`SiteAdmission::reserved_total`] by the delta.
    reserved: AtomicUsize,
    /// `ephpm_php_site_in_flight{site}` — cached handle so admit/release touch no
    /// label string and allocate nothing on the hot path.
    in_flight_gauge: Gauge,
    /// `ephpm_php_site_queued{site}` — requests of this site waiting at the gate.
    queued_gauge: Gauge,
    /// `ephpm_php_site_shed_total{site}` — requests of this site rejected at the
    /// gate (over ceiling, no borrow, shed policy).
    shed_counter: Counter,
}

/// The per-tenant concurrency fairness controller. Cheap to clone via `Arc`.
pub struct SiteAdmission {
    /// `N`: the pool's execution-thread count. The saturation threshold for the
    /// work-conserving borrow decision (`total < N` ⇒ a thread is idle).
    capacity: usize,
    /// Resolved global default ceiling (already derived from the pool and
    /// clamped) applied to a site whose override declares no `max_concurrency`.
    default_max: usize,
    /// Global default `reserved` applied to a site whose override declares none.
    default_reserved: usize,
    /// All-sites in-flight count: the `total` in the borrow decision.
    total_in_flight: AtomicUsize,
    /// `R`: the running sum of every registered site's `reserved`, so the borrow
    /// headroom term is O(1).
    reserved_total: AtomicUsize,
    /// Per-site state, keyed by the canonical site key.
    sites: DashMap<String, Arc<SiteState>>,
    /// Woken on every [`SitePermit`] release so waiters at the gate re-check.
    notify: Notify,
}

impl SiteAdmission {
    /// Build the controller for a pool of `capacity` execution threads.
    ///
    /// `configured_max` is `[php] per_site_max_concurrency` (`0` = derive
    /// [`PER_SITE_MAX_FRACTION`] of `capacity`, floored at [`PER_SITE_MAX_FLOOR`]);
    /// `configured_reserved` is `[php] per_site_reserved`. Both are the global
    /// defaults a per-site `[limits]` override may replace.
    #[must_use]
    pub fn new(capacity: usize, configured_max: usize, configured_reserved: usize) -> Arc<Self> {
        let capacity = capacity.max(1);
        let default_max = derive_max(configured_max, capacity);
        // The global default reserved can never exceed the default ceiling (a
        // reservation above the ceiling is meaningless) nor the pool.
        let default_reserved = configured_reserved.min(default_max);
        Arc::new(Self {
            capacity,
            default_max,
            default_reserved,
            total_in_flight: AtomicUsize::new(0),
            reserved_total: AtomicUsize::new(0),
            sites: DashMap::new(),
            notify: Notify::new(),
        })
    }

    /// The pool execution-thread count this controller gates against.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The resolved global default ceiling (for startup logging).
    #[must_use]
    pub fn default_max(&self) -> usize {
        self.default_max
    }

    /// Resolve one site's effective limits from its per-site `[limits]` override
    /// (either value `None` ⇒ take the global default), then clamp: `max` into
    /// `[1, capacity]`, `reserved` into `[0, max]`.
    #[must_use]
    pub fn resolve_limits(
        &self,
        override_max: Option<usize>,
        override_reserved: Option<usize>,
    ) -> SiteLimits {
        let max = override_max.unwrap_or(self.default_max).clamp(1, self.capacity);
        let reserved = override_reserved.unwrap_or(self.default_reserved).min(max);
        SiteLimits { max, reserved }
    }

    /// Non-blocking admission. `Some(permit)` admits the request to the pool;
    /// `None` means the site is over its ceiling and no idle capacity can be
    /// borrowed right now (the caller waits or sheds).
    pub fn try_acquire(self: &Arc<Self>, key: &str, limits: SiteLimits) -> Option<SitePermit> {
        let state = self.state_for(key, limits);
        self.try_admit(&state, limits)
    }

    /// Wait (work-conserving) until the request can be admitted, then return its
    /// permit. Cancellation-safe: dropping the future (e.g. an outer deadline or
    /// client disconnect) removes the waiter and decrements the queued gauge.
    ///
    /// Only over-ceiling requests ever reach the wait loop — a within-share
    /// request is admitted on the first try and never parks here.
    pub async fn acquire(self: &Arc<Self>, key: &str, limits: SiteLimits) -> SitePermit {
        let state = self.state_for(key, limits);
        if let Some(permit) = self.try_admit(&state, limits) {
            return permit;
        }
        // Slow path: a contended, over-ceiling request. Count it as queued
        // (observability only — never an input to the decision) and re-check on
        // every release until admitted.
        let _queued = QueuedGuard::enter(&state);
        loop {
            // Register for notification BEFORE re-checking so a release between
            // the check and the await is never lost.
            let notified = self.notify.notified();
            if let Some(permit) = self.try_admit(&state, limits) {
                return permit;
            }
            notified.await;
        }
    }

    /// The lock-free admission decision. See the module docs for the algorithm.
    fn try_admit(
        self: &Arc<Self>,
        state: &Arc<SiteState>,
        limits: SiteLimits,
    ) -> Option<SitePermit> {
        loop {
            let inf = state.in_flight.load(Ordering::Acquire);
            // Clause 1 — within the site's guaranteed share: admit
            // unconditionally, even under saturation.
            if inf < limits.max {
                if state
                    .in_flight
                    .compare_exchange_weak(inf, inf + 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    self.total_in_flight.fetch_add(1, Ordering::AcqRel);
                    return Some(self.grant(state));
                }
                continue; // raced; retry with the fresh value
            }
            // Clause 2 — over the ceiling: borrow an idle slot only if a thread
            // is genuinely free AND no other site's reservation is consumed.
            let total = self.total_in_flight.load(Ordering::Acquire);
            let headroom =
                self.reserved_total.load(Ordering::Acquire).saturating_sub(limits.reserved);
            if total.saturating_add(headroom) < self.capacity {
                if state
                    .in_flight
                    .compare_exchange_weak(inf, inf + 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    self.total_in_flight.fetch_add(1, Ordering::AcqRel);
                    return Some(self.grant(state));
                }
                continue; // raced; retry
            }
            return None;
        }
    }

    /// Build the RAII permit for a just-counted admission and bump the gauge.
    fn grant(self: &Arc<Self>, state: &Arc<SiteState>) -> SitePermit {
        state.in_flight_gauge.increment(1.0);
        SitePermit { admission: Arc::clone(self), state: Arc::clone(state) }
    }

    /// Get or create the per-site state, syncing its limits (and the global
    /// reserved sum) if the resolved values changed since last seen.
    fn state_for(self: &Arc<Self>, key: &str, limits: SiteLimits) -> Arc<SiteState> {
        if let Some(existing) = self.sites.get(key) {
            self.sync_reserved(&existing, limits.reserved);
            return Arc::clone(&existing);
        }
        // First sighting of this site. Build its cached metric handles once.
        let state = Arc::new(SiteState {
            in_flight: AtomicUsize::new(0),
            reserved: AtomicUsize::new(limits.reserved),
            in_flight_gauge: gauge!("ephpm_php_site_in_flight", "site" => key.to_string()),
            queued_gauge: gauge!("ephpm_php_site_queued", "site" => key.to_string()),
            shed_counter: counter!("ephpm_php_site_shed_total", "site" => key.to_string()),
        });
        match self.sites.entry(key.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(e) => {
                // Lost the insert race; adopt the winner and sync its reserved.
                let winner = Arc::clone(e.get());
                self.sync_reserved(&winner, limits.reserved);
                winner
            }
            dashmap::mapref::entry::Entry::Vacant(e) => {
                self.reserved_total.fetch_add(limits.reserved, Ordering::AcqRel);
                e.insert(Arc::clone(&state));
                state
            }
        }
    }

    /// Fold a changed per-site `reserved` into [`Self::reserved_total`]. The
    /// `swap` is the serialization point, so concurrent updates telescope to the
    /// correct sum. A no-op when unchanged (the overwhelmingly common case).
    fn sync_reserved(&self, state: &SiteState, reserved: usize) {
        if state.reserved.load(Ordering::Relaxed) == reserved {
            return;
        }
        let old = state.reserved.swap(reserved, Ordering::AcqRel);
        match reserved.cmp(&old) {
            std::cmp::Ordering::Greater => {
                self.reserved_total.fetch_add(reserved - old, Ordering::AcqRel);
            }
            std::cmp::Ordering::Less => {
                self.reserved_total.fetch_sub(old - reserved, Ordering::AcqRel);
            }
            std::cmp::Ordering::Equal => {}
        }
    }

    /// Release one admitted slot: decrement the counters, drop the gauge, and
    /// wake every gate waiter to re-check. Called from [`SitePermit::drop`].
    fn release(&self, state: &SiteState) {
        state.in_flight.fetch_sub(1, Ordering::AcqRel);
        self.total_in_flight.fetch_sub(1, Ordering::AcqRel);
        state.in_flight_gauge.decrement(1.0);
        self.notify.notify_waiters();
    }

    /// All-sites in-flight count (test / introspection).
    #[cfg(test)]
    fn total(&self) -> usize {
        self.total_in_flight.load(Ordering::Acquire)
    }

    /// One site's current in-flight count (test / introspection).
    #[cfg(test)]
    fn in_flight(&self, key: &str) -> usize {
        self.sites.get(key).map_or(0, |s| s.in_flight.load(Ordering::Acquire))
    }
}

/// Derive the global default ceiling: an explicit non-zero value is used as-is
/// (clamped to the pool); `0` derives [`PER_SITE_MAX_FRACTION`] of the pool,
/// floored at [`PER_SITE_MAX_FLOOR`] and capped at the pool.
fn derive_max(configured: usize, capacity: usize) -> usize {
    if configured > 0 {
        return configured.min(capacity);
    }
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let derived = (capacity as f64 * PER_SITE_MAX_FRACTION).ceil() as usize;
    derived.max(PER_SITE_MAX_FLOOR).min(capacity).max(1)
}

/// Increments the per-site queued gauge on entry and decrements it on drop, so
/// the gauge is correct even when the `acquire` future is cancelled mid-wait.
struct QueuedGuard<'a> {
    gauge: &'a Gauge,
}

impl<'a> QueuedGuard<'a> {
    fn enter(state: &'a SiteState) -> Self {
        state.queued_gauge.increment(1.0);
        Self { gauge: &state.queued_gauge }
    }
}

impl Drop for QueuedGuard<'_> {
    fn drop(&mut self) {
        self.gauge.decrement(1.0);
    }
}

/// A held per-site admission slot. The request occupies one of its site's
/// concurrency slots for exactly as long as this guard lives — acquired before
/// the pool dispatch and dropped after the reply — so dropping it (answered,
/// timed out, or the handler future cancelled) frees the slot and wakes the next
/// waiter.
pub struct SitePermit {
    admission: Arc<SiteAdmission>,
    state: Arc<SiteState>,
}

impl SitePermit {
    /// Record that this site shed a request at the gate (over its ceiling, shed
    /// policy). Called on the shed path where no permit is granted; exposed on
    /// the controller via the site state the caller already resolved.
    pub fn note_shed(admission: &Arc<SiteAdmission>, key: &str, limits: SiteLimits) {
        let state = admission.state_for(key, limits);
        state.shed_counter.increment(1);
    }
}

impl Drop for SitePermit {
    fn drop(&mut self) {
        self.admission.release(&self.state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: &str = "attacker.example";
    const KEY_B: &str = "victim.example";
    const KEY_V: &str = "vip.example";

    /// Fill a site to `n` in-flight via the non-blocking path, returning the
    /// held permits. Panics if any admission is refused.
    fn fill(adm: &Arc<SiteAdmission>, key: &str, limits: SiteLimits, n: usize) -> Vec<SitePermit> {
        (0..n)
            .map(|i| {
                adm.try_acquire(key, limits)
                    .unwrap_or_else(|| panic!("admission {i} for {key} must be granted"))
            })
            .collect()
    }

    #[test]
    fn derive_default_ceiling_is_half_the_pool_floored_at_two() {
        assert_eq!(derive_max(0, 10), 5, "half of 10");
        assert_eq!(derive_max(0, 8), 4, "half of 8");
        assert_eq!(derive_max(0, 3), 2, "ceil(1.5)=2");
        assert_eq!(derive_max(0, 2), 2, "floored at 2");
        assert_eq!(derive_max(0, 1), 1, "never above the pool, even below the floor");
        assert_eq!(derive_max(6, 10), 6, "explicit value honoured");
        assert_eq!(derive_max(40, 10), 10, "explicit value clamped to the pool");
    }

    #[test]
    fn resolve_applies_defaults_and_clamps() {
        let adm = SiteAdmission::new(10, 0, 0);
        assert_eq!(adm.resolve_limits(None, None), SiteLimits { max: 5, reserved: 0 });
        assert_eq!(adm.resolve_limits(Some(3), Some(2)), SiteLimits { max: 3, reserved: 2 });
        assert_eq!(
            adm.resolve_limits(Some(2), Some(9)).reserved,
            2,
            "reserved is clamped down to max"
        );
        assert_eq!(adm.resolve_limits(Some(99), None).max, 10, "max is clamped to the pool");
    }

    /// F-DoS-1 core #1: a site cannot exceed its ceiling once the box is
    /// saturated — the next over-ceiling request for it is refused, not run.
    #[test]
    fn a_site_cannot_exceed_its_max_under_contention() {
        let adm = SiteAdmission::new(4, 0, 0); // default ceiling = 2
        let limits = adm.resolve_limits(None, None);
        assert_eq!(limits.max, 2);

        // Two within-share admits for A, then A borrows the two idle slots
        // (work-conserving) until the box is full.
        let mut held = fill(&adm, KEY_A, limits, 2);
        held.push(adm.try_acquire(KEY_A, limits).expect("borrow idle slot 3"));
        held.push(adm.try_acquire(KEY_A, limits).expect("borrow idle slot 4"));
        assert_eq!(adm.total(), 4, "box is now full");

        // The box is saturated: A is over its ceiling and cannot borrow.
        assert!(
            adm.try_acquire(KEY_A, limits).is_none(),
            "a saturated box must refuse a site already over its ceiling"
        );
        drop(held);
    }

    /// F-DoS-1 core #2 (the regression test): an attacker capped at its ceiling
    /// never blocks a *different* site's within-share admission. The victim is
    /// served while the attacker is held back.
    #[test]
    fn a_capped_site_does_not_block_a_different_site() {
        let adm = SiteAdmission::new(4, 0, 0); // ceiling = 2 each
        let limits = adm.resolve_limits(None, None);

        // Attacker floods and borrows the whole box (idle, so borrowing is fine).
        let attacker = fill(&adm, KEY_A, limits, 4);
        assert_eq!(adm.in_flight(KEY_A), 4);
        assert_eq!(adm.total(), 4, "attacker alone fills the box");

        // Victim arrives. Even with the box full, its within-share requests are
        // admitted unconditionally (they take their place in the pool FIFO).
        let v1 = adm.try_acquire(KEY_B, limits).expect("victim within-share admit #1");
        let v2 = adm.try_acquire(KEY_B, limits).expect("victim within-share admit #2");
        assert_eq!(adm.in_flight(KEY_B), 2, "victim got its full guaranteed share");

        // The victim past its own ceiling cannot borrow a saturated box either —
        // symmetric with the attacker.
        assert!(
            adm.try_acquire(KEY_B, limits).is_none(),
            "victim over its ceiling cannot borrow a full box"
        );

        // And once the attacker is capped, it cannot re-grab a freed slot ahead
        // of nothing — but crucially the attacker over its ceiling stays blocked
        // while the box is full, leaving freed slots for the victim.
        drop(attacker);
        assert_eq!(adm.in_flight(KEY_A), 0, "attacker drained");
        drop(v1);
        drop(v2);
    }

    /// F-DoS-1 core #3: `reserved_concurrency` keeps slots available for a VIP
    /// even when another tenant floods — borrowers may not consume the reserved
    /// headroom, and the VIP's own within-share admits always land.
    #[test]
    fn reserved_keeps_slots_for_a_site_under_contention() {
        let adm = SiteAdmission::new(10, 0, 0);
        // VIP reserves 3 (ceiling 5). Attacker has the default ceiling and no
        // reservation.
        let vip = adm.resolve_limits(Some(5), Some(3));
        let attacker = adm.resolve_limits(None, None); // ceiling 5
        // Touch the VIP once so its reservation registers in the global sum.
        drop(adm.try_acquire(KEY_V, vip).expect("register vip"));
        assert_eq!(adm.reserved_total.load(Ordering::Acquire), 3);

        // Attacker fills its within-share (5) then borrows — but may not take the
        // last 3 slots reserved for the VIP. It should stall at total == 7.
        let mut held = fill(&adm, KEY_A, attacker, 5);
        while let Some(p) = adm.try_acquire(KEY_A, attacker) {
            held.push(p);
            assert!(adm.total() <= 7, "borrowers must not eat the reserved headroom");
        }
        assert_eq!(adm.total(), 7, "attacker is held out of the VIP's 3 reserved slots");

        // The VIP can still take its 3 reserved slots despite the flood.
        let v1 = adm.try_acquire(KEY_V, vip).expect("vip reserved slot 1");
        let v2 = adm.try_acquire(KEY_V, vip).expect("vip reserved slot 2");
        let v3 = adm.try_acquire(KEY_V, vip).expect("vip reserved slot 3");
        assert_eq!(adm.total(), 10, "box full with attacker:7 + vip:3");
        drop(held);
        drop((v1, v2, v3));
    }

    /// Work-conserving: a lone busy site may borrow every idle slot, far past its
    /// own ceiling, because no one else is contending.
    #[test]
    fn a_lone_site_borrows_the_whole_box() {
        let adm = SiteAdmission::new(8, 0, 0); // ceiling 4
        let limits = adm.resolve_limits(None, None);
        assert_eq!(limits.max, 4);
        let held = fill(&adm, KEY_A, limits, 8); // 4 within-share + 4 borrowed
        assert_eq!(adm.in_flight(KEY_A), 8, "a lone site uses the whole box");
        assert!(adm.try_acquire(KEY_A, limits).is_none(), "but no more than the box");
        drop(held);
    }

    /// A release wakes a waiter parked at the gate, which is then admitted.
    #[tokio::test]
    async fn release_admits_a_waiting_over_ceiling_request() {
        let adm = SiteAdmission::new(2, 0, 0); // ceiling 2
        let limits = adm.resolve_limits(None, None);
        // Fill the box (2 within-share for A).
        let held = fill(&adm, KEY_A, limits, 2);
        assert!(adm.try_acquire(KEY_A, limits).is_none(), "A over ceiling, box full");

        // A third A request waits at the gate (over ceiling, saturated).
        let waiter = {
            let adm = Arc::clone(&adm);
            tokio::spawn(async move { adm.acquire(KEY_A, limits).await })
        };
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "the over-ceiling request must park");

        // Freeing a slot wakes it — now total < capacity, so A borrows.
        drop(held);
        let permit = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("waiter must be admitted after a release")
            .expect("join");
        assert_eq!(adm.in_flight(KEY_A), 1, "exactly the woken request is in flight");
        drop(permit);
    }

    /// The single-tenant / uncontended hot path: one request, one atomic bump.
    /// Releasing it returns the box to empty.
    #[test]
    fn uncontended_admit_and_release_round_trips() {
        let adm = SiteAdmission::new(16, 0, 0);
        let limits = adm.resolve_limits(None, None);
        let p = adm.try_acquire(KEY_A, limits).expect("first admit is immediate");
        assert_eq!(adm.total(), 1);
        drop(p);
        assert_eq!(adm.total(), 0, "release returns the slot");
        assert_eq!(adm.in_flight(KEY_A), 0);
    }
}
