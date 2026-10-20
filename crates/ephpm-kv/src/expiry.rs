//! Background active expiry for every KV store in the process.
//!
//! Reads never return an expired key (every read path checks the deadline and
//! reaps lazily), but a key that is written with a TTL and never read again
//! still holds its memory — and still counts against `memory_limit` — until
//! something removes it. This module runs that something: one thread that,
//! every [`ACTIVE_EXPIRE_INTERVAL`], runs
//! [`Store::active_expire_cycle`] on the default store and on **every**
//! per-site store, sharing an [`ACTIVE_EXPIRE_BUDGET`] between them.
//!
//! # Why it lives here and not in the RESP server
//!
//! The reaper used to be spawned by the RESP listener, so with
//! `[kv.redis_compat] enabled = false` — the default, and the normal setup for
//! a PHP-only node — nothing reaped at all, and even with RESP on it only ever
//! covered the process-wide store, never the per-site stores of a multi-tenant
//! node. Expiry is a property of the store, so it is started with the store
//! (`ephpm-server`'s `start_kv_service`), independent of any listener.
//!
//! # Lifetime
//!
//! The thread holds only weak references. It exits on its own once the
//! default store has been dropped, so a short-lived store (a test, an embedder)
//! does not leak a thread that keeps it alive. Strong references are taken for
//! the length of one tick and released before sleeping.
//!
//! # Why a thread, not a tokio task
//!
//! A cycle is synchronous, CPU-bound work of up to [`ACTIVE_EXPIRE_BUDGET`].
//! On a tokio worker it would stall every task scheduled there for that long,
//! ten times a second; on its own thread it competes only for a core.

use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dashmap::DashMap;

use crate::multi_tenant::MultiTenantStore;
use crate::store::Store;

/// How often the expiry thread wakes (Redis' default `hz 10`).
pub const ACTIVE_EXPIRE_INTERVAL: Duration = Duration::from_millis(100);

/// Most time one wake-up spends expiring keys, across all stores — a quarter
/// of [`ACTIVE_EXPIRE_INTERVAL`], so active expiry uses at most about 25% of
/// one core (Redis' default budget).
pub const ACTIVE_EXPIRE_BUDGET: Duration = Duration::from_millis(25);

/// Start the active-expiry thread for `default` and, when present, every
/// per-site store in `sites` — including site stores created after this call.
///
/// The thread exits by itself once `default` has been dropped (see the module
/// docs), so the returned handle may simply be dropped.
///
/// # Errors
///
/// Returns an error if the OS refuses to create the thread.
pub fn spawn(
    default: &Arc<Store>,
    sites: Option<&MultiTenantStore>,
) -> std::io::Result<JoinHandle<()>> {
    spawn_with(default, sites, ACTIVE_EXPIRE_INTERVAL, ACTIVE_EXPIRE_BUDGET)
}

/// [`spawn`] with an explicit interval and budget (tests use a short interval).
pub(crate) fn spawn_with(
    default: &Arc<Store>,
    sites: Option<&MultiTenantStore>,
    interval: Duration,
    budget: Duration,
) -> std::io::Result<JoinHandle<()>> {
    let default = Arc::downgrade(default);
    let sites = sites.map(MultiTenantStore::sites_weak);
    std::thread::Builder::new()
        .name("ephpm-kv-expiry".into())
        .spawn(move || run(&default, sites.as_ref(), interval, budget))
}

/// The thread body: one [`tick`] per `interval` until the default store is
/// gone.
fn run(
    default: &Weak<Store>,
    sites: Option<&Weak<DashMap<String, Arc<Store>>>>,
    interval: Duration,
    budget: Duration,
) {
    tracing::debug!("KV active expiry started");
    let mut stores: Vec<Arc<Store>> = Vec::new();
    let mut tick_no: usize = 0;
    loop {
        std::thread::sleep(interval);
        let Some(default) = default.upgrade() else {
            tracing::debug!("KV store dropped; active expiry stopping");
            return;
        };
        stores.push(default);
        if let Some(map) = sites.and_then(Weak::upgrade) {
            stores.extend(map.iter().map(|site| Arc::clone(site.value())));
        }
        tick(&stores, tick_no, budget);
        // Release every strong reference before sleeping, so a store dropped
        // by its owner is freed now rather than kept alive by this thread.
        stores.clear();
        tick_no = tick_no.wrapping_add(1);
    }
}

/// Run one expiry cycle on each store, sharing `budget` between them.
///
/// The starting store rotates with `tick_no`, so when the budget runs out
/// before the last store, a different store goes short next time — no site is
/// starved by the ones ahead of it. Each store gets at least one sample per
/// tick whatever the remaining budget (`active_expire_cycle` always samples
/// once), so progress never stops on any of them.
fn tick(stores: &[Arc<Store>], tick_no: usize, budget: Duration) -> usize {
    let n = stores.len();
    if n == 0 {
        return 0;
    }
    let deadline = Instant::now() + budget;
    let mut reaped = 0;
    for i in 0..n {
        let store = &stores[(tick_no.wrapping_add(i)) % n];
        reaped += store.active_expire_cycle(deadline.saturating_duration_since(Instant::now()));
    }
    reaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::StoreConfig;

    const FAST: Duration = Duration::from_millis(5);

    /// Write `count` keys that expire after 1 ms, plus one persistent key, and
    /// wait until the short-TTL ones are past their deadline.
    fn fill(store: &Store, prefix: &str, count: usize) {
        for i in 0..count {
            assert!(store.set(
                format!("{prefix}{i}"),
                b"v".to_vec(),
                Some(Duration::from_millis(1))
            ));
        }
        assert!(store.set(format!("{prefix}live"), b"v".to_vec(), None));
    }

    /// Poll until `cond` holds, failing after a generous timeout. The thread
    /// runs every 5 ms, so in practice this returns within a few ticks.
    fn wait_until(what: &str, cond: impl Fn() -> bool) {
        let give_up = Instant::now() + Duration::from_secs(10);
        while !cond() {
            assert!(Instant::now() < give_up, "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// F-1: nothing reaped expired keys unless the RESP listener was running,
    /// and even then never in a per-site store. The thread must reach the
    /// default store and every site store.
    #[test]
    fn reaps_default_store_and_every_site_store() {
        let default = Store::new(StoreConfig::default());
        let mt = MultiTenantStore::new(Arc::clone(&default), StoreConfig::default());
        let alice = mt.get_site_store("alice.test");
        let bob = mt.get_site_store("bob.test");
        fill(&default, "d:", 500);
        fill(&alice, "a:", 500);
        fill(&bob, "b:", 500);
        std::thread::sleep(Duration::from_millis(5));

        let _handle = spawn_with(&default, Some(&mt), FAST, ACTIVE_EXPIRE_BUDGET).unwrap();
        // `len()` counts not-yet-reaped expired keys, so it only drops to the
        // single live key once the thread has reaped them. No read touches
        // the expired keys, so lazy expiry cannot be what removes them.
        wait_until("default store reaped", || default.len() == 1);
        wait_until("alice's store reaped", || alice.len() == 1);
        wait_until("bob's store reaped", || bob.len() == 1);
        assert!(default.exists("d:live") && alice.exists("a:live") && bob.exists("b:live"));
    }

    /// A site store created after the thread started is still covered — sites
    /// are created lazily, on a tenant's first request.
    #[test]
    fn covers_site_stores_created_after_start() {
        let default = Store::new(StoreConfig::default());
        let mt = MultiTenantStore::new(Arc::clone(&default), StoreConfig::default());
        let _handle = spawn_with(&default, Some(&mt), FAST, ACTIVE_EXPIRE_BUDGET).unwrap();

        let late = mt.get_site_store("late.test");
        fill(&late, "l:", 300);
        wait_until("late site store reaped", || late.len() == 1);
    }

    /// The thread holds no strong reference between ticks and exits once the
    /// stores are gone, so it never keeps a dropped store alive.
    #[test]
    fn thread_exits_once_the_store_is_dropped() {
        let default = Store::new(StoreConfig::default());
        let mt = MultiTenantStore::new(Arc::clone(&default), StoreConfig::default());
        let handle = spawn_with(&default, Some(&mt), FAST, ACTIVE_EXPIRE_BUDGET).unwrap();
        let weak = Arc::downgrade(&default);
        drop(mt);
        drop(default);
        wait_until("expiry thread exits", || handle.is_finished());
        assert!(weak.upgrade().is_none(), "the thread must not keep the store alive");
    }

    /// The budget is shared across stores, but every store still gets at least
    /// one sample per tick even when the budget is already spent.
    #[test]
    fn exhausted_budget_still_samples_every_store() {
        let stores: Vec<Arc<Store>> = (0..3).map(|_| Store::new(StoreConfig::default())).collect();
        for (i, s) in stores.iter().enumerate() {
            fill(s, &format!("s{i}:"), 10);
        }
        std::thread::sleep(Duration::from_millis(5));
        // A zero budget: each store still runs its one guaranteed sample,
        // which (with only 11 keys) visits all of them.
        let reaped = tick(&stores, 0, Duration::ZERO);
        assert_eq!(reaped, 30);
        for s in &stores {
            assert_eq!(s.len(), 1);
        }
    }
}
