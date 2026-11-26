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
//! queue slot.
//!
//! It is consulted **only in multi-tenant mode** (a request with a resolved site
//! key). Single-site deployments and requests to an unmatched host bypass it
//! entirely, so the uncontended common path pays nothing.
//!
//! # The permit is charged to the THREAD, not the request future
//!
//! This is the load-bearing detail. The router does **not** hold the permit for
//! the lifetime of the request handler. It moves the permit into the job and
//! parks it with [`crate::fpm_pool::hold_until_finished`], so it is released only
//! when the PHP thread has *finished the request* (execution **and** teardown),
//! on every exit path — normal, a contained C-stack overflow, a hard-memory-limit
//! `_exit(137)`, or a Rust panic. That is the same lifetime the eBPF tenant tag
//! uses. The alternative — releasing the permit when the handler future returns —
//! is **broken against the exact attack this defends against**: a `sleep(3600)`
//! request 504s at the request deadline, the handler future returns, and if the
//! permit were charged to that future the slot would be freed while the PHP
//! thread runs for another hour. The tenant would never show as "over ceiling",
//! and the pool — whose hung-thread replacement is capped at its size — would
//! drain to zero serving threads while every tenant's ceiling looked satisfied.
//! A permit charged to the thread keeps the tenant's in-flight count honest for
//! as long as its thread is actually occupied.
//!
//! For the same reason the gate measures **serving** capacity
//! ([`crate::fpm_pool::FpmPool::serving_threads`]), not the static pool size: a
//! pool full of hung threads has fewer threads able to serve, and a borrow must
//! not be admitted into a thread that does not exist.
//!
//! # The two levers (per site; both default-safe)
//!
//! * **`max_concurrency`** — the *ceiling*: the most pool requests a site may
//!   hold at once **under contention**. The safety lever. Its zero-config default
//!   divides the pool equally among the configured vhosts
//!   ([`derive_max`]) — `ceil(N / known_sites)`, floored at
//!   [`PER_SITE_MAX_FLOOR`] — so neither one hostile tenant *nor one controlling
//!   several vhosts* can take the whole box out of the box.
//! * **`reserved_concurrency`** — the *floor*: idle execution slots **other**
//!   tenants may not borrow, held available for this site. Default `0`. See the
//!   honest statement of what it does and does not guarantee below.
//!
//! A proportional `weight` for sharing the contended remainder is a planned v2
//! knob; the config surface is forward-compatible to it but it is not implemented
//! here.
//!
//! # The admission algorithm — **work-conserving**
//!
//! We cannot preempt a running PHP request (there is no mid-request pause), so
//! this is *admission-time* fairness. For a request of site `S` with resolved
//! limits `(max, reserved)`, let `inf` be `S`'s current in-flight count, `total`
//! the all-sites in-flight count, `serving` the pool's current serving-thread
//! count and `U` the sum over every site of its *unmet* reservation
//! `max(0, reserved − in_flight)`:
//!
//! 1. **Within its guaranteed share (`inf < max`): admit unconditionally**, even
//!    when the box is saturated. This is what protects the victim — its own
//!    requests always enter the pool, where the pool's existing FIFO admission
//!    orders them fairly against everyone else's. A within-share request never
//!    waits at this gate.
//! 2. **Over its ceiling (`inf >= max`): *borrow* an idle serving slot** iff a
//!    thread is genuinely free *and* no site's unmet reservation would be
//!    consumed — `total + U < serving`. (A borrower's own reservation is fully
//!    used, so its own `U` term is 0 and need not be subtracted.) A lone busy
//!    site thus uses the whole box; a contended one is held to its ceiling.
//!
//! The crucial interaction: a within-share request is admitted *immediately*, so
//! it takes its place in the pool's FIFO queue ahead of any later borrow request
//! from the attacker — the pool then hands each freed thread to the
//! longest-waiting queued job (the victim) first. The gate blocks the attacker's
//! over-ceiling requests under saturation; the pool's FIFO does the final
//! ordering.
//!
//! # What `reserved_concurrency` actually guarantees (honest)
//!
//! It stops **other** tenants from *borrowing* into the last `U` idle serving
//! slots, so those slots stay free for the reserving site's within-share requests
//! to land on promptly rather than queue behind borrowed work. It does **not**
//! guarantee a thread when the *sum of ceilings* already exceeds the pool: two
//! tenants each filling their within-share ceiling can saturate the box with zero
//! borrowing, and this gate never blocks a within-share request, so the
//! reservation does not preempt them. A strict per-site thread *guarantee* needs
//! pool-side priority (per-site queues + a DRR/weighted dispatch), which is
//! roadmap, not this change. Pre-registering every vhost's reservation at startup
//! (so `U` is correct from the first request) is as far as an admission-time gate
//! can honestly take it.
//!
//! # Cost and safety
//!
//! The admission decision is **lock-free atomics** on the hot path — never a
//! lock, never a wait while holding a pool thread. Within-share requests (the
//! victim, and the common case) are admitted on the first try and never park.
//! Over-ceiling requests that must wait park on a [`tokio::sync::Notify`], and the
//! waiter set is **bounded per site** (at the pool's `queue_depth`) so a tenant
//! cannot build an unbounded wait queue — a new DoS the first cut of this gate
//! would have introduced. A release wakes **one** waiter (`notify_one`), which
//! chains another only while capacity and waiters both remain, so a completion is
//! not a thundering-herd wake of every waiter; the release skips the notify
//! entirely when nothing is waiting. The gate cannot deadlock a request that
//! holds resources: a permit is acquired *before* dispatch and never while
//! holding a pool thread, and a borrow is admitted whenever a thread is idle, so
//! a loopback subrequest on an otherwise-idle box is always admitted (keep
//! `max_concurrency >= 2` where loopback subrequests are used — issue #461).
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
    /// Floor: idle execution slots other tenants may not borrow, held for this
    /// site. Always `<= max`.
    pub reserved: usize,
}

/// Per-site admission state. One per site key ever seen; never removed (the set
/// is bounded by the number of configured vhosts).
struct SiteState {
    /// Requests this site currently has admitted to the pool (queued or
    /// executing). The per-site counter the hot-path decision reads.
    in_flight: AtomicUsize,
    /// This site's last-resolved `reserved`, kept so the unmet-reservation sum
    /// can be maintained and a changed override folded in.
    reserved: AtomicUsize,
    /// Requests of this site currently parked at the gate (over ceiling, no idle
    /// slot). Bounded by [`SiteAdmission::max_waiters`]; backs
    /// `ephpm_php_site_queued`.
    waiting: AtomicUsize,
    /// `ephpm_php_site_in_flight{site}` — cached handle so admit/release touch no
    /// label string and allocate nothing on the hot path.
    in_flight_gauge: Gauge,
    /// `ephpm_php_site_queued{site}` — requests of this site waiting at the gate.
    queued_gauge: Gauge,
    /// `ephpm_php_site_shed_total{site}` — requests of this site rejected at the
    /// gate (over ceiling, no borrow, shed policy or waiter bound reached).
    shed_counter: Counter,
}

/// A source of the pool's current *serving*-thread count. The borrow decision
/// reads this, not a static size, so a pool whose threads are hung cannot admit
/// a borrow into a thread that cannot serve it.
pub type ServingFn = Arc<dyn Fn() -> usize + Send + Sync>;

/// The per-tenant concurrency fairness controller. Cheap to clone via `Arc`.
pub struct SiteAdmission {
    /// `N`: the configured pool size, used for the default-ceiling derivation and
    /// as the clamp ceiling. The *borrow* decision uses [`Self::serving`] instead.
    pool_size: usize,
    /// Current serving-thread count (dynamic). See [`ServingFn`].
    serving: ServingFn,
    /// Resolved global default ceiling (already derived and clamped) applied to a
    /// site whose override declares no `max_concurrency`.
    default_max: usize,
    /// Global default `reserved` applied to a site whose override declares none.
    default_reserved: usize,
    /// Per-site cap on parked waiters (the pool's `queue_depth`): a tenant that
    /// would exceed it is shed/504 rather than allowed to grow an unbounded wait
    /// queue.
    max_waiters: usize,
    /// All-sites in-flight count: the `total` in the borrow decision.
    total_in_flight: AtomicUsize,
    /// `U`: the running sum over sites of `max(0, reserved − in_flight)` (the
    /// *unmet* reservation), so the borrow headroom is O(1) and a site using its
    /// reserve does not idle threads for others.
    unmet_reserved: AtomicUsize,
    /// All-sites parked-waiter count, so a release can skip the notify when
    /// nothing waits.
    total_waiting: AtomicUsize,
    /// Per-site state, keyed by the canonical site key.
    sites: DashMap<String, Arc<SiteState>>,
    /// Woken (one at a time) on a release so the next waiter re-checks.
    notify: Notify,
}

impl SiteAdmission {
    /// Build the controller for a pool of `pool_size` serving threads shared by
    /// `known_sites` vhosts.
    ///
    /// `configured_max` is `[php] per_site_max_concurrency` (`0` = derive, see
    /// [`derive_max`]); `configured_reserved` is `[php] per_site_reserved`;
    /// `max_waiters` is the per-site waiter bound (the pool's `queue_depth`). The
    /// serving-thread count is fixed at `pool_size` (used by the tests);
    /// production uses [`Self::new_with_serving`].
    #[must_use]
    pub fn new(
        pool_size: usize,
        known_sites: usize,
        configured_max: usize,
        configured_reserved: usize,
        max_waiters: usize,
    ) -> Arc<Self> {
        let fixed = pool_size.max(1);
        Self::new_with_serving(
            pool_size,
            known_sites,
            Arc::new(move || fixed),
            configured_max,
            configured_reserved,
            max_waiters,
        )
    }

    /// Build the controller with a dynamic serving-thread source (production:
    /// `move || pool.serving_threads()`).
    #[must_use]
    pub fn new_with_serving(
        pool_size: usize,
        known_sites: usize,
        serving: ServingFn,
        configured_max: usize,
        configured_reserved: usize,
        max_waiters: usize,
    ) -> Arc<Self> {
        let pool_size = pool_size.max(1);
        let default_max = derive_max(configured_max, pool_size, known_sites.max(1));
        let default_reserved = configured_reserved.min(default_max);
        Arc::new(Self {
            pool_size,
            serving,
            default_max,
            default_reserved,
            max_waiters: max_waiters.max(1),
            total_in_flight: AtomicUsize::new(0),
            unmet_reserved: AtomicUsize::new(0),
            total_waiting: AtomicUsize::new(0),
            sites: DashMap::new(),
            notify: Notify::new(),
        })
    }

    /// The configured pool size (for startup logging / the clamp ceiling).
    #[must_use]
    pub fn pool_size(&self) -> usize {
        self.pool_size
    }

    /// The resolved global default ceiling (for startup logging).
    #[must_use]
    pub fn default_max(&self) -> usize {
        self.default_max
    }

    /// Current serving capacity, clamped to the pool size.
    fn serving_now(&self) -> usize {
        (self.serving)().min(self.pool_size)
    }

    /// Resolve one site's effective limits from its per-site `[limits]` override
    /// (either value `None` ⇒ take the global default), then clamp: `max` into
    /// `[1, pool_size]`, `reserved` into `[0, max]`.
    #[must_use]
    pub fn resolve_limits(
        &self,
        override_max: Option<usize>,
        override_reserved: Option<usize>,
    ) -> SiteLimits {
        let max = override_max.unwrap_or(self.default_max).clamp(1, self.pool_size);
        let reserved = override_reserved.unwrap_or(self.default_reserved).min(max);
        SiteLimits { max, reserved }
    }

    /// Pre-register a known vhost's resolved limits at startup, so its
    /// reservation is reflected in the unmet-reservation sum from the **first**
    /// request rather than only after the site is first seen. Idempotent (a
    /// later request re-syncs a changed override). Returns whether the operator's
    /// requested reservation was clamped down to the ceiling (for a startup WARN).
    pub fn register_site(
        self: &Arc<Self>,
        key: &str,
        requested_reserved: usize,
        limits: SiteLimits,
    ) -> bool {
        self.state_for(key, limits);
        requested_reserved > limits.reserved
    }

    /// Non-blocking admission. `Some(permit)` admits the request to the pool;
    /// `None` means the site is over its ceiling and no idle capacity can be
    /// borrowed right now (the caller waits or sheds).
    pub fn try_acquire(self: &Arc<Self>, key: &str, limits: SiteLimits) -> Option<SitePermit> {
        let state = self.state_for(key, limits);
        self.try_admit(&state, limits)
    }

    /// Record that this site shed a request at the gate (over its ceiling with no
    /// slot, under shed policy or the waiter bound). Called where no permit is
    /// granted.
    pub fn note_shed(self: &Arc<Self>, key: &str, limits: SiteLimits) {
        self.state_for(key, limits).shed_counter.increment(1);
    }

    /// Wait (work-conserving) until the request can be admitted, then return its
    /// permit. `None` means the per-site waiter bound is already reached and the
    /// request could not even be queued (the caller sheds/504). Cancellation-safe:
    /// dropping the future (an outer deadline, a client disconnect) removes the
    /// waiter and decrements the queued gauge.
    ///
    /// Only over-ceiling requests ever reach the wait loop — a within-share
    /// request is admitted on the first try and never parks here.
    pub async fn acquire(self: &Arc<Self>, key: &str, limits: SiteLimits) -> Option<SitePermit> {
        let state = self.state_for(key, limits);
        if let Some(permit) = self.try_admit(&state, limits) {
            return Some(permit);
        }
        // Reserve a per-site waiter slot (bounds the otherwise-unbounded wait
        // queue — a new DoS the gate must not introduce).
        if state.waiting.fetch_add(1, Ordering::SeqCst) >= self.max_waiters {
            state.waiting.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        self.total_waiting.fetch_add(1, Ordering::SeqCst);
        state.queued_gauge.increment(1.0);
        let _waiter = WaiterGuard { admission: self, state: &state };
        loop {
            // Register for notification BEFORE re-checking so a release between
            // the check and the await is never lost.
            let notified = self.notify.notified();
            if let Some(permit) = self.try_admit(&state, limits) {
                // Handoff: if a slot is still free and someone else waits, pass
                // the signal on — so a multi-slot release cascades without a
                // thundering-herd wake of every waiter per completion.
                if self.total_in_flight.load(Ordering::Acquire) < self.serving_now()
                    && self.total_waiting.load(Ordering::SeqCst) > 1
                {
                    self.notify.notify_one();
                }
                return Some(permit);
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
            // unconditionally. The site counter is the serialization point.
            if inf < limits.max {
                if state
                    .in_flight
                    .compare_exchange_weak(inf, inf + 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    self.total_in_flight.fetch_add(1, Ordering::AcqRel);
                    // If the slot taken sat within the reservation, that unmet
                    // reservation is now met.
                    if inf < state.reserved.load(Ordering::Acquire) {
                        self.unmet_reserved.fetch_sub(1, Ordering::AcqRel);
                    }
                    return Some(self.grant(state));
                }
                continue; // raced; retry with the fresh value
            }
            // Clause 2 — over the ceiling: borrow an idle serving slot only if a
            // thread is free AND no site's unmet reservation is consumed. `total`
            // is the serialization point so two borrowers cannot both pass.
            let total = self.total_in_flight.load(Ordering::Acquire);
            let headroom = self.unmet_reserved.load(Ordering::Acquire);
            if total.saturating_add(headroom) < self.serving_now() {
                if self
                    .total_in_flight
                    .compare_exchange_weak(total, total + 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    // inf >= max >= reserved, so this slot is outside the
                    // reservation — the unmet sum is unchanged.
                    state.in_flight.fetch_add(1, Ordering::AcqRel);
                    return Some(self.grant(state));
                }
                continue; // total changed under us; retry
            }
            return None;
        }
    }

    /// Build the RAII permit for a just-counted admission and bump the gauge.
    fn grant(self: &Arc<Self>, state: &Arc<SiteState>) -> SitePermit {
        state.in_flight_gauge.increment(1.0);
        SitePermit { admission: Arc::clone(self), state: Arc::clone(state) }
    }

    /// Get or create the per-site state, syncing its reservation (and the global
    /// unmet sum) if the resolved value changed since last seen.
    fn state_for(self: &Arc<Self>, key: &str, limits: SiteLimits) -> Arc<SiteState> {
        if let Some(existing) = self.sites.get(key) {
            self.sync_reserved(&existing, limits.reserved);
            return Arc::clone(&existing);
        }
        // First sighting of this site. Build its cached metric handles once.
        let state = Arc::new(SiteState {
            in_flight: AtomicUsize::new(0),
            reserved: AtomicUsize::new(limits.reserved),
            waiting: AtomicUsize::new(0),
            in_flight_gauge: gauge!("ephpm_php_site_in_flight", "site" => key.to_string()),
            queued_gauge: gauge!("ephpm_php_site_queued", "site" => key.to_string()),
            shed_counter: counter!("ephpm_php_site_shed_total", "site" => key.to_string()),
        });
        match self.sites.entry(key.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(e) => {
                // Lost the insert race; adopt the winner and sync its reservation.
                let winner = Arc::clone(e.get());
                self.sync_reserved(&winner, limits.reserved);
                winner
            }
            dashmap::mapref::entry::Entry::Vacant(e) => {
                // A fresh site has in_flight 0, so its whole reservation is unmet.
                self.unmet_reserved.fetch_add(limits.reserved, Ordering::AcqRel);
                e.insert(Arc::clone(&state));
                state
            }
        }
    }

    /// Fold a changed per-site `reserved` into [`Self::unmet_reserved`]. A no-op
    /// when unchanged (the overwhelmingly common case). On a change (a rare
    /// override re-read) the unmet delta is computed against the current
    /// in-flight count; a concurrent admit/release may make it momentarily
    /// approximate, which only shifts a borrow decision slightly and self-corrects
    /// as requests complete — the exact ceilings are unaffected.
    fn sync_reserved(&self, state: &SiteState, reserved: usize) {
        if state.reserved.load(Ordering::Relaxed) == reserved {
            return;
        }
        let old = state.reserved.swap(reserved, Ordering::AcqRel);
        if old == reserved {
            return;
        }
        let inf = state.in_flight.load(Ordering::Acquire);
        let old_unmet = old.saturating_sub(inf);
        let new_unmet = reserved.saturating_sub(inf);
        match new_unmet.cmp(&old_unmet) {
            std::cmp::Ordering::Greater => {
                self.unmet_reserved.fetch_add(new_unmet - old_unmet, Ordering::AcqRel);
            }
            std::cmp::Ordering::Less => {
                self.unmet_reserved.fetch_sub(old_unmet - new_unmet, Ordering::AcqRel);
            }
            std::cmp::Ordering::Equal => {}
        }
    }

    /// Release one admitted slot: decrement the counters, restore any unmet
    /// reservation the freed slot sat within, drop the gauge, and hand the slot
    /// to one waiter. Called from [`SitePermit::drop`].
    fn release(&self, state: &SiteState) {
        let old = state.in_flight.fetch_sub(1, Ordering::AcqRel);
        self.total_in_flight.fetch_sub(1, Ordering::AcqRel);
        // If the freed slot sat within the reservation (`old - 1 < reserved`),
        // that reservation is unmet again.
        if old <= state.reserved.load(Ordering::Acquire) {
            self.unmet_reserved.fetch_add(1, Ordering::AcqRel);
        }
        state.in_flight_gauge.decrement(1.0);
        // Hand the freed slot to exactly one waiter; skip entirely when nothing
        // waits (the hot path). The woken waiter chains the next one only while
        // capacity and waiters remain, so this is not a per-completion herd wake.
        if self.total_waiting.load(Ordering::SeqCst) > 0 {
            self.notify.notify_one();
        }
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

/// Derive the global default ceiling.
///
/// An explicit non-zero `configured` value is used as-is (clamped to the pool).
/// `0` divides the pool equally among the configured vhosts —
/// `ceil(pool_size / known_sites)` — floored at [`PER_SITE_MAX_FLOOR`] and capped
/// at the pool. Equal division (rather than a flat fraction of the pool) keeps
/// the *sum* of default ceilings near the pool size, so a tenant controlling
/// several vhosts cannot default its way to the whole box; for one or two vhosts
/// it is half the pool, as before.
#[must_use]
pub fn derive_max(configured: usize, pool_size: usize, known_sites: usize) -> usize {
    if configured > 0 {
        return configured.min(pool_size);
    }
    let known = known_sites.max(1);
    let per = pool_size.div_ceil(known);
    per.max(PER_SITE_MAX_FLOOR).min(pool_size).max(1)
}

/// Decrements the per-site and global waiter counters and the queued gauge on
/// drop, so they are correct whether the `acquire` future is admitted or
/// cancelled mid-wait.
struct WaiterGuard<'a> {
    admission: &'a SiteAdmission,
    state: &'a SiteState,
}

impl Drop for WaiterGuard<'_> {
    fn drop(&mut self) {
        self.state.waiting.fetch_sub(1, Ordering::SeqCst);
        self.admission.total_waiting.fetch_sub(1, Ordering::SeqCst);
        self.state.queued_gauge.decrement(1.0);
    }
}

/// A held per-site admission slot. **Charged to the PHP thread, not the request
/// future**: the router moves it into the job and parks it with
/// [`crate::fpm_pool::hold_until_finished`], so the slot is occupied for exactly
/// as long as the PHP thread is actually running the request (execution and
/// teardown), and is freed on every exit path. Dropping it (the job finished,
/// or the job was dropped while still queued) frees the slot and wakes the next
/// waiter. Dropping it must not call into PHP — [`SiteAdmission::release`] only
/// touches atomics, a gauge and a `Notify`.
pub struct SitePermit {
    admission: Arc<SiteAdmission>,
    state: Arc<SiteState>,
}

impl Drop for SitePermit {
    fn drop(&mut self) {
        self.admission.release(&self.state);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const KEY_A: &str = "attacker.example";
    const KEY_B: &str = "victim.example";
    const KEY_V: &str = "vip.example";

    /// A controller with a fixed serving capacity and a large waiter bound.
    /// Behaviour tests pass explicit limits, so the derived default is irrelevant.
    fn adm(pool_size: usize) -> Arc<SiteAdmission> {
        SiteAdmission::new(pool_size, 1, 0, 0, 4096)
    }

    /// A controller whose serving capacity is read live from `serving`.
    fn adm_serving(pool_size: usize, serving: Arc<AtomicUsize>) -> Arc<SiteAdmission> {
        let s = Arc::clone(&serving);
        SiteAdmission::new_with_serving(
            pool_size,
            1,
            Arc::new(move || s.load(Ordering::Acquire)),
            0,
            0,
            4096,
        )
    }

    fn limits(max: usize, reserved: usize) -> SiteLimits {
        SiteLimits { max, reserved }
    }

    /// Fill a site to `n` in-flight via the non-blocking path, returning the
    /// held permits. Panics if any admission is refused.
    fn fill(adm: &Arc<SiteAdmission>, key: &str, l: SiteLimits, n: usize) -> Vec<SitePermit> {
        (0..n)
            .map(|i| {
                adm.try_acquire(key, l)
                    .unwrap_or_else(|| panic!("admission {i} for {key} must be granted"))
            })
            .collect()
    }

    #[test]
    fn derive_default_ceiling_divides_the_pool_and_floors_at_two() {
        assert_eq!(derive_max(0, 10, 2), 5, "10 / 2 vhosts");
        assert_eq!(derive_max(0, 16, 4), 4, "16 / 4 vhosts");
        assert_eq!(derive_max(0, 16, 100), 2, "many vhosts floor at 2");
        assert_eq!(derive_max(0, 1, 1), 1, "never above the pool, even below the floor");
        assert_eq!(derive_max(0, 10, 1), 10, "a lone vhost may use the whole pool");
        assert_eq!(derive_max(6, 10, 50), 6, "explicit value honoured, sites ignored");
        assert_eq!(derive_max(40, 10, 1), 10, "explicit value clamped to the pool");
    }

    #[test]
    fn resolve_applies_defaults_and_clamps() {
        // pool 10, 2 vhosts -> default ceiling 5.
        let a = SiteAdmission::new(10, 2, 0, 0, 64);
        assert_eq!(a.resolve_limits(None, None), SiteLimits { max: 5, reserved: 0 });
        assert_eq!(a.resolve_limits(Some(3), Some(2)), SiteLimits { max: 3, reserved: 2 });
        assert_eq!(a.resolve_limits(Some(2), Some(9)).reserved, 2, "reserved clamped to max");
        assert_eq!(a.resolve_limits(Some(99), None).max, 10, "max clamped to the pool");
    }

    /// F-DoS-1 core #1: a site cannot exceed its ceiling once the box is
    /// saturated — the next over-ceiling request for it is refused, not run.
    #[test]
    fn a_site_cannot_exceed_its_max_under_contention() {
        let a = adm(4);
        let l = limits(2, 0);
        let mut held = fill(&a, KEY_A, l, 2); // within-share
        held.push(a.try_acquire(KEY_A, l).expect("borrow idle slot 3"));
        held.push(a.try_acquire(KEY_A, l).expect("borrow idle slot 4"));
        assert_eq!(a.total(), 4, "box is now full");
        assert!(a.try_acquire(KEY_A, l).is_none(), "a saturated box refuses an over-ceiling site");
        drop(held);
    }

    /// F-DoS-1 core #2 (the regression test): an attacker capped at its ceiling
    /// never blocks a *different* site's within-share admission.
    #[test]
    fn a_capped_site_does_not_block_a_different_site() {
        let a = adm(4);
        let l = limits(2, 0);
        let attacker = fill(&a, KEY_A, l, 4); // borrows the whole (idle) box
        assert_eq!(a.in_flight(KEY_A), 4);

        // Victim arrives: within-share admits unconditionally despite the full box.
        let v1 = a.try_acquire(KEY_B, l).expect("victim within-share #1");
        let v2 = a.try_acquire(KEY_B, l).expect("victim within-share #2");
        assert_eq!(a.in_flight(KEY_B), 2, "victim got its full guaranteed share");
        assert!(a.try_acquire(KEY_B, l).is_none(), "victim over ceiling cannot borrow a full box");

        drop(attacker);
        assert_eq!(a.in_flight(KEY_A), 0);
        drop((v1, v2));
    }

    /// F-DoS-1 core #3: `reserved_concurrency` stops other tenants borrowing the
    /// VIP's idle headroom, and the VIP's own within-share admits always land.
    #[test]
    fn reserved_keeps_idle_slots_for_a_site_under_contention() {
        let a = adm(10);
        let vip = a.resolve_limits(Some(5), Some(3));
        let attacker = limits(5, 0);
        // Register the VIP so its unmet reservation (3) is counted from the start.
        a.register_site(KEY_V, 3, vip);
        assert_eq!(a.unmet_reserved.load(Ordering::Acquire), 3);

        // Attacker fills its within-share (5) then borrows — but may not take the
        // 3 idle slots the VIP has reserved. It stalls at total == 7.
        let mut held = fill(&a, KEY_A, attacker, 5);
        while let Some(p) = a.try_acquire(KEY_A, attacker) {
            held.push(p);
            assert!(a.total() <= 7, "borrowers must not eat the reserved headroom");
        }
        assert_eq!(a.total(), 7, "attacker held out of the VIP's 3 reserved slots");

        // The VIP takes its 3 reserved slots despite the flood.
        let v = fill(&a, KEY_V, vip, 3);
        assert_eq!(a.total(), 10);
        drop(held);
        drop(v);
    }

    /// A VIP already *using* its reservation does not keep idling threads for
    /// itself: as it fills its reserved slots the unmet sum falls, so a borrower
    /// may use the freed headroom (P5 — unmet, not static, reservation).
    #[test]
    fn a_used_reservation_releases_borrow_headroom() {
        let a = adm(10);
        let vip = a.resolve_limits(Some(6), Some(3));
        a.register_site(KEY_V, 3, vip);
        assert_eq!(a.unmet_reserved.load(Ordering::Acquire), 3);

        // VIP uses 2 of its 3 reserved -> unmet falls to 1.
        let v = fill(&a, KEY_V, vip, 2);
        assert_eq!(a.unmet_reserved.load(Ordering::Acquire), 1);

        // Attacker may now borrow up to serving - 1 reserved slot.
        let attacker = limits(4, 0);
        let mut held = fill(&a, KEY_A, attacker, 4);
        while let Some(p) = a.try_acquire(KEY_A, attacker) {
            held.push(p);
        }
        assert_eq!(a.total(), 9, "box filled except the VIP's 1 remaining unmet reserved slot");
        drop(v);
        drop(held);
    }

    /// Work-conserving: a lone busy site borrows every idle slot past its ceiling.
    #[test]
    fn a_lone_site_borrows_the_whole_box() {
        let a = adm(8);
        let l = limits(4, 0);
        let held = fill(&a, KEY_A, l, 8);
        assert_eq!(a.in_flight(KEY_A), 8);
        assert!(a.try_acquire(KEY_A, l).is_none(), "but no more than the box");
        drop(held);
    }

    /// Serving capacity, not static size: when threads are hung the borrow gate
    /// shrinks, so a pool with no serving threads admits no borrow (P1).
    #[test]
    fn borrow_follows_serving_capacity_not_static_size() {
        let serving = Arc::new(AtomicUsize::new(8));
        let a = adm_serving(8, Arc::clone(&serving));
        let l = limits(2, 0);
        // Two within-share + borrow up to serving (8).
        let mut held = fill(&a, KEY_A, l, 2);
        while let Some(p) = a.try_acquire(KEY_A, l) {
            held.push(p);
        }
        assert_eq!(a.total(), 8, "borrows up to the serving capacity");

        // Now simulate 6 of 8 threads hung: serving drops to 2. No new borrow.
        drop(held.split_off(2)); // release the 6 borrowed, keep 2 within-share
        assert_eq!(a.total(), 2);
        serving.store(2, Ordering::Release);
        // in_flight == serving == 2: no idle serving slot to borrow into.
        assert!(a.try_acquire(KEY_A, l).is_none(), "no borrow when serving is exhausted");
        drop(held);
    }

    /// A release hands the slot to a waiting over-ceiling request.
    #[tokio::test]
    async fn release_admits_a_waiting_over_ceiling_request() {
        let a = adm(2);
        let l = limits(2, 0);
        let held = fill(&a, KEY_A, l, 2);
        assert!(a.try_acquire(KEY_A, l).is_none(), "A over ceiling, box full");

        let waiter = {
            let a = Arc::clone(&a);
            tokio::spawn(async move { a.acquire(KEY_A, l).await })
        };
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "the over-ceiling request must park");

        drop(held);
        let permit = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("waiter admitted after a release")
            .expect("join")
            .expect("admitted, not bound-rejected");
        assert_eq!(a.in_flight(KEY_A), 1);
        drop(permit);
    }

    /// The per-site waiter bound: a tenant cannot grow an unbounded wait queue —
    /// once `max_waiters` are parked, the next `acquire` returns `None` at once
    /// (the caller sheds/504) instead of parking (P2).
    #[tokio::test]
    async fn waiter_set_is_bounded_per_site() {
        // pool 1, waiter bound 2.
        let a = SiteAdmission::new(1, 1, 0, 0, 2);
        let l = limits(1, 0);
        let _held = a.try_acquire(KEY_A, l).expect("fill the single slot");
        assert!(a.try_acquire(KEY_A, l).is_none(), "over ceiling, box full");

        // Two waiters park (bound = 2).
        let mut parked = Vec::new();
        for _ in 0..2 {
            let a2 = Arc::clone(&a);
            parked.push(tokio::spawn(async move { a2.acquire(KEY_A, l).await }));
        }
        // Wait until both have registered as waiting.
        for _ in 0..200 {
            if a.total_waiting.load(Ordering::SeqCst) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(a.total_waiting.load(Ordering::SeqCst), 2, "both parked");

        // The third would-be waiter is refused immediately, not parked.
        let third = a.acquire(KEY_A, l).await;
        assert!(third.is_none(), "beyond the bound, acquire must refuse rather than park");

        for t in parked {
            t.abort();
        }
    }

    /// The uncontended hot path: one request, one atomic bump; release returns it.
    #[test]
    fn uncontended_admit_and_release_round_trips() {
        let a = adm(16);
        let l = a.resolve_limits(None, None);
        let p = a.try_acquire(KEY_A, l).expect("first admit is immediate");
        assert_eq!(a.total(), 1);
        drop(p);
        assert_eq!(a.total(), 0);
        assert_eq!(a.in_flight(KEY_A), 0);
    }

    /// P1 integration: a permit moved into a pool job and parked with
    /// `hold_until_finished` stays charged until the PHP **thread** finishes the
    /// request — not when the dispatching future returns. A request that "times
    /// out" (we stop awaiting its reply) keeps its site slot until the job's
    /// thread actually completes.
    #[tokio::test]
    async fn permit_parked_on_the_job_is_held_until_the_thread_finishes() {
        use ephpm_php::PhpRuntime;
        use ephpm_php::response::PhpResponse;

        use crate::fpm_pool::{FpmPool, hold_until_finished};

        PhpRuntime::init().expect("stub init");
        let pool = FpmPool::spawn(1, 4, ephpm_config::AdmissionPolicy::Fifo);
        for _ in 0..200 {
            if pool.ready_count() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let a = adm(4);
        let l = limits(2, 0);
        let permit = a.try_acquire(KEY_A, l).expect("admit");
        assert_eq!(a.in_flight(KEY_A), 1);

        // A job that parks the permit (as the router does) and blocks until
        // released — standing in for a slow / hung request.
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let task: crate::fpm_pool::FpmTask = Box::new(move || {
            hold_until_finished(permit);
            let _ = release_rx.recv();
            Ok(PhpResponse { status: 200, headers: Vec::new(), body: Vec::new() })
        });
        let rx = pool.dispatch(task).await.expect("dispatch");

        // "Time out": stop awaiting the reply. The slot MUST stay charged while
        // the thread runs.
        drop(rx);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            a.in_flight(KEY_A),
            1,
            "the permit is held by the running thread, not the future"
        );

        // Let the job finish; the permit is released after teardown.
        release_tx.send(()).expect("release");
        for _ in 0..400 {
            if a.in_flight(KEY_A) == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(a.in_flight(KEY_A), 0, "released only once the thread finished the request");

        pool.drain();
        for _ in 0..400 {
            if pool.live_count() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}
