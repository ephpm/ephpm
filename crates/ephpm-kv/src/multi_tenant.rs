//! Multi-tenant KV store with per-site isolation.
//!
//! Each virtual host gets its own [`Store`] instance, created lazily on
//! first access. Provides physical key isolation â€” a site's store is a
//! separate `DashMap`, not a prefix in a shared map.

use std::sync::{Arc, RwLock};

use dashmap::DashMap;

use crate::store::{Replicator, Store, StoreConfig};

/// Builds the [`Replicator`] to install on a site's [`Store`] the moment that
/// store is created.
///
/// Site stores are created **lazily**, so a replicator cannot be installed
/// up-front the way it is on the process-wide store: a site may first come into
/// existence on a request, on a RESP `AUTH`, or on an inbound replicated write
/// from a peer. Registering a factory (see
/// [`MultiTenantStore::set_replicator_factory`]) makes every one of those paths
/// produce a store that already replicates â€” there is no window in which a
/// vhost's writes are silently node-local.
///
/// The factory receives the site key **and the freshly-built store it is being
/// installed on**, and returns that site's replicator (which namespaces the
/// site's keys on the wire â€” see `ephpm-cluster`'s `site_namespace`).
///
/// # The store is passed in, never looked up
///
/// Handing the store to the factory is a hard requirement, not a convenience.
/// A factory that instead resolved its own store by calling back into
/// [`MultiTenantStore::get_site_store`] would re-enter this type while the
/// caller was mid-creation for that same site â€” and, when creation ran inside a
/// `DashMap` entry closure, that re-entry deadlocked on the shard lock and hung
/// **every** PHP request on the node. The signature now makes that shape
/// unrepresentable: there is nothing for the factory to look up.
///
/// # Factories must be pure construction
///
/// A factory runs on the request thread during a site's first access. It must
/// not block, `block_on`, await a task, or acquire a lock that cluster
/// machinery might hold â€” build the replicator value and return. Anything that
/// could wait belongs in a spawned task inside the replicator's own methods.
/// (It must also not call [`tokio::runtime::Handle::current`], which panics off
/// a runtime thread; capture the handle when the factory is built instead.)
///
/// # Re-entrancy
///
/// A factory **may** call back into the registry for a *different* site â€”
/// [`MultiTenantStore::get_site_store`] holds no lock while a factory runs, so
/// that is safe. It must **not** ask for the site it is currently being built
/// for: that is a request for the very store under construction, and it
/// recurses. There is never a reason to â€” the store is the factory's second
/// argument.
pub type SiteReplicatorFactory =
    Arc<dyn Fn(&str, &Arc<Store>) -> Arc<dyn Replicator> + Send + Sync>;

/// A multi-tenant KV store that manages per-site [`Store`] instances.
///
/// Sites are created lazily on first access. Each site gets its own
/// `DashMap` with independent memory limits and key spaces.
///
/// Thread-safe and cheaply cloneable.
#[derive(Clone)]
pub struct MultiTenantStore {
    sites: Arc<DashMap<String, Arc<Store>>>,
    /// Config template used when creating new site stores.
    site_config: StoreConfig,
    /// Fallback store for single-site mode (when no hostname is provided).
    default_store: Arc<Store>,
    /// Installed on every site store at creation, when clustering is active.
    /// `None` (the default) leaves site stores purely node-local, which is the
    /// correct single-node behaviour.
    replicator_factory: Arc<RwLock<Option<SiteReplicatorFactory>>>,
}

impl MultiTenantStore {
    /// Create a new multi-tenant store.
    ///
    /// `default_store` is used when no hostname is specified (single-site mode).
    /// `site_config` is the template for creating per-site stores.
    #[must_use]
    pub fn new(default_store: Arc<Store>, site_config: StoreConfig) -> Self {
        Self {
            sites: Arc::new(DashMap::new()),
            site_config,
            default_store,
            replicator_factory: Arc::new(RwLock::new(None)),
        }
    }

    /// Install the factory that gives each site store its [`Replicator`].
    ///
    /// Called once at startup when clustering is enabled, **before** any site
    /// store is created, so every vhost keyspace replicates from its first
    /// write. Shared with every clone of this handle (the factory lives behind
    /// the same `Arc`), which is what lets the RESP listener, the PHP path, and
    /// the inbound-replication applier all agree.
    ///
    /// Applies to stores created *after* this call; any site store that already
    /// exists is also updated, so ordering cannot leave a vhost unreplicated.
    pub fn set_replicator_factory(&self, factory: Option<SiteReplicatorFactory>) {
        if let Ok(mut slot) = self.replicator_factory.write() {
            slot.clone_from(&factory);
        }
        // Retro-fit any store that was created before the factory landed.
        //
        // The existing sites are **snapshotted first** so no shard lock is held
        // while a factory runs: `DashMap::iter` holds each shard's lock for the
        // duration of the visit, and running user code under it is the same
        // hazard that deadlocked `get_site_store`. Collect, drop the iterator,
        // then build and install.
        if let Some(factory) = factory {
            let existing: Vec<(String, Arc<Store>)> =
                self.sites.iter().map(|e| (e.key().clone(), Arc::clone(e.value()))).collect();
            for (site, store) in existing {
                store.set_replicator(Some(factory(&site, &store)));
            }
        }
    }

    /// Get or create a store for the given hostname.
    ///
    /// The store is created lazily on first access with the template config,
    /// and â€” when a [`SiteReplicatorFactory`] is installed â€” with that site's
    /// replicator already wired, so its very first write fans out to peers.
    /// Subsequent calls return the same store instance.
    ///
    /// # Locking
    ///
    /// **No user code ever runs while a shard lock is held.** The store and its
    /// replicator are built entirely outside the map; the map is touched only
    /// for the final insert-if-absent, which runs no callback. This is load
    /// bearing: an earlier revision built the store inside a `DashMap` entry
    /// closure â€” which holds the shard lock for the closure's duration â€” and a
    /// factory that re-entered this method deadlocked that shard, hanging every
    /// PHP request on the node. Building first also means a factory is free to
    /// be arbitrarily involved without any lock-ordering obligation.
    ///
    /// Losing a creation race is safe: the loser's store is discarded *before*
    /// it is returned to anyone, so no write can ever land in a store that is
    /// not the one in the map. Every caller â€” winner and loser alike â€” gets the
    /// single store the map holds.
    #[must_use]
    pub fn get_site_store(&self, hostname: &str) -> Arc<Store> {
        if hostname.is_empty() {
            return Arc::clone(&self.default_store);
        }

        // Fast path: already created. A sharded read, no allocation.
        if let Some(store) = self.sites.get(hostname) {
            return Arc::clone(store.value());
        }

        // Slow path: build a candidate store (and its replicator) with NO map
        // lock held. Both `Store::new` and the factory run here, on this
        // thread, free of the shard lock.
        let factory = self.replicator_factory.read().ok().and_then(|f| f.clone());
        let candidate = Store::new(self.site_config.clone());
        if let Some(factory) = &factory {
            candidate.set_replicator(Some(factory(hostname, &candidate)));
        }

        // Publish it. `or_insert` takes the shard lock only to insert-or-read â€”
        // it runs no closure â€” and returns whatever the map holds, so a racing
        // pair converges on one store instead of the loser's being dropped out
        // from under a caller that had already been handed it.
        let winner = {
            let entry = self.sites.entry(hostname.to_string()).or_insert(candidate);
            Arc::clone(entry.value())
        };
        // Logged outside the entry guard: `tracing` subscribers are user code
        // too, and nothing arbitrary may run under a shard lock.
        tracing::info!(hostname, replicated = factory.is_some(), "created KV store for site");
        winner
    }

    /// Get the default store (for single-site mode or admin access).
    #[must_use]
    pub fn default_store(&self) -> &Arc<Store> {
        &self.default_store
    }

    /// Number of site stores currently active.
    #[must_use]
    pub fn site_count(&self) -> usize {
        self.sites.len()
    }

    /// Remove a site's store (e.g., when a preview is torn down).
    ///
    /// Returns `true` if the site existed.
    #[must_use]
    pub fn remove_site(&self, hostname: &str) -> bool {
        self.sites.remove(hostname).is_some()
    }

    /// Authenticate a RESP connection for a specific site.
    ///
    /// Returns the site's store if the hostname is valid.
    /// Password validation is handled by the caller.
    #[must_use]
    pub fn auth_site(&self, hostname: &str) -> Option<Arc<Store>> {
        if hostname.is_empty() {
            return None;
        }
        Some(self.get_site_store(hostname))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn test_config() -> StoreConfig {
        StoreConfig {
            memory_limit: 1024 * 1024, // 1 MB per site
            ..StoreConfig::default()
        }
    }

    #[test]
    fn get_creates_store_on_first_access() {
        let default = Store::new(StoreConfig::default());
        let mt = MultiTenantStore::new(default, test_config());

        assert_eq!(mt.site_count(), 0);
        let store = mt.get_site_store("alice.com");
        assert_eq!(mt.site_count(), 1);

        // Second access returns same store
        let store2 = mt.get_site_store("alice.com");
        assert!(Arc::ptr_eq(&store, &store2));
        assert_eq!(mt.site_count(), 1);
    }

    #[test]
    fn sites_are_isolated() {
        let default = Store::new(StoreConfig::default());
        let mt = MultiTenantStore::new(default, test_config());

        let alice = mt.get_site_store("alice.com");
        let bob = mt.get_site_store("bob.com");

        alice.set("key".into(), b"alice-data".to_vec(), None);
        bob.set("key".into(), b"bob-data".to_vec(), None);

        assert_eq!(alice.get("key").as_deref(), Some(&b"alice-data"[..]));
        assert_eq!(bob.get("key").as_deref(), Some(&b"bob-data"[..]));
    }

    #[test]
    fn empty_hostname_returns_default() {
        let default_arc = Store::new(StoreConfig::default());
        let mt = MultiTenantStore::new(Arc::clone(&default_arc), test_config());

        let store = mt.get_site_store("");
        assert!(Arc::ptr_eq(&store, &default_arc));
        assert_eq!(mt.site_count(), 0);
    }

    #[test]
    fn remove_site_deletes_store() {
        let default = Store::new(StoreConfig::default());
        let mt = MultiTenantStore::new(default, test_config());

        let _ = mt.get_site_store("temp.com");
        assert_eq!(mt.site_count(), 1);

        assert!(mt.remove_site("temp.com"));
        assert_eq!(mt.site_count(), 0);

        assert!(!mt.remove_site("nonexistent.com"));
    }

    #[test]
    fn auth_site_returns_store() {
        let default = Store::new(StoreConfig::default());
        let mt = MultiTenantStore::new(default, test_config());

        assert!(mt.auth_site("").is_none());
        assert!(mt.auth_site("site.com").is_some());
        assert_eq!(mt.site_count(), 1);
    }

    #[test]
    fn clones_share_the_same_site_stores() {
        // `Clone` shares the `sites` map, so handing one instance to the PHP
        // path and a clone to the RESP listener keeps a vhost on one keyspace.
        let default = Store::new(StoreConfig::default());
        let php_side = MultiTenantStore::new(default, test_config());
        let resp_side = php_side.clone();

        let from_php = php_side.get_site_store("alice.com");
        let from_resp = resp_side.get_site_store("alice.com");
        assert!(Arc::ptr_eq(&from_php, &from_resp));

        from_php.set("k".into(), b"v".to_vec(), None);
        assert_eq!(resp_side.get_site_store("alice.com").get("k").as_deref(), Some(&b"v"[..]));
        assert_eq!(php_side.site_count(), 1);
        assert_eq!(resp_side.site_count(), 1);
    }

    #[test]
    fn separate_instances_do_not_share_site_stores() {
        // Why `Clone` (above) is mandatory rather than merely convenient:
        // `new()` always allocates a fresh `sites` map, so two instances over
        // the same default store still give a vhost two disjoint keyspaces.
        let default = Store::new(StoreConfig::default());
        let one = MultiTenantStore::new(Arc::clone(&default), test_config());
        let two = MultiTenantStore::new(default, test_config());

        let from_one = one.get_site_store("alice.com");
        let from_two = two.get_site_store("alice.com");
        assert!(!Arc::ptr_eq(&from_one, &from_two));

        from_one.set("k".into(), b"v".to_vec(), None);
        assert_eq!(from_two.get("k"), None);
    }

    #[test]
    fn site_stores_inherit_the_template_config() {
        let default = Store::new(StoreConfig::default());
        let mt = MultiTenantStore::new(default, test_config());
        let site = mt.get_site_store("alice.com");
        assert_eq!(site.config().memory_limit, test_config().memory_limit);
    }

    /// Every lazily-created site store gets the factory's replicator, so a
    /// vhost replicates from its very first write â€” including a site created
    /// by an inbound replicated write rather than a local request.
    #[test]
    fn replicator_factory_is_installed_on_lazily_created_site_stores() {
        use std::sync::Mutex;

        #[derive(Debug, Default)]
        struct Recorder {
            site: String,
            sets: Mutex<Vec<String>>,
        }
        impl Replicator for Recorder {
            fn replicate_set(&self, key: String, _v: Vec<u8>, _t: Option<Duration>) -> bool {
                self.sets.lock().unwrap().push(format!("{}:{key}", self.site));
                true
            }
            fn replicate_remove(&self, _key: &str) -> bool {
                true
            }
            fn replicate_expire(&self, _key: &str, _ttl: Duration) -> bool {
                true
            }
            fn replicate_published(&self, key: String, _v: Vec<u8>, _t: Option<Duration>) {
                self.sets.lock().unwrap().push(format!("{}:{key}", self.site));
            }
        }

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let recorders: Arc<DashMap<String, Arc<Recorder>>> = Arc::new(DashMap::new());

        let default = Store::new(StoreConfig::default());
        let mt = MultiTenantStore::new(default, test_config());

        let rec_for_factory = Arc::clone(&recorders);
        mt.set_replicator_factory(Some(Arc::new(move |site: &str, _store: &Arc<Store>| {
            let r = Arc::new(Recorder { site: site.to_string(), sets: Mutex::new(Vec::new()) });
            rec_for_factory.insert(site.to_string(), Arc::clone(&r));
            r as Arc<dyn Replicator>
        })));

        // A public `set` on a lazily-created site store goes through that
        // site's replicator (it does NOT stay node-local).
        mt.get_site_store("alice.test").set("k".into(), b"v".to_vec(), None);
        mt.get_site_store("bob.test").set("k".into(), b"v".to_vec(), None);

        for site in ["alice.test", "bob.test"] {
            let r = recorders.get(site).expect("a replicator was built for this site").clone();
            let sets = r.sets.lock().unwrap().clone();
            seen.lock().unwrap().extend(sets);
        }
        let mut got = seen.lock().unwrap().clone();
        got.sort();
        assert_eq!(
            got,
            vec!["alice.test:k".to_string(), "bob.test:k".to_string()],
            "each site's write must reach that site's own replicator"
        );
    }

    /// A store created before the factory is installed is retro-fitted, so
    /// startup ordering cannot leave a vhost silently unreplicated.
    #[test]
    fn pre_existing_site_stores_are_retrofitted_with_the_factory() {
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Debug)]
        struct Counter(Arc<AtomicUsize>);
        impl Replicator for Counter {
            fn replicate_set(&self, _k: String, _v: Vec<u8>, _t: Option<Duration>) -> bool {
                self.0.fetch_add(1, Ordering::Relaxed);
                true
            }
            fn replicate_remove(&self, _key: &str) -> bool {
                true
            }
            fn replicate_expire(&self, _key: &str, _ttl: Duration) -> bool {
                true
            }
            fn replicate_published(&self, _k: String, _v: Vec<u8>, _t: Option<Duration>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let _ = Mutex::new(());

        let default = Store::new(StoreConfig::default());
        let mt = MultiTenantStore::new(default, test_config());
        // Site exists BEFORE clustering wires the factory.
        let early = mt.get_site_store("early.test");

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_for_factory = Arc::clone(&hits);
        mt.set_replicator_factory(Some(Arc::new(move |_site: &str, _store: &Arc<Store>| {
            Arc::new(Counter(Arc::clone(&hits_for_factory))) as Arc<dyn Replicator>
        })));

        early.set("k".into(), b"v".to_vec(), None);
        assert_eq!(hits.load(Ordering::Relaxed), 1, "a pre-existing site store must replicate too");
    }

    /// **Deadlock regression.** A factory that calls back into the same
    /// `MultiTenantStore` must not hang.
    ///
    /// This is the shape that took the live cluster down: the real factory
    /// resolved its own store via `get_site_store`, creation ran inside a
    /// `DashMap` entry closure (which holds the shard lock for the closure's
    /// duration), and the re-entry deadlocked that shard â€” hanging *every* PHP
    /// request on the node, since multi-tenant dispatch calls `get_site_store`
    /// per request. The earlier tests all used trivial closures and passed
    /// straight through it.
    ///
    /// The signature now hands the store to the factory so production has no
    /// reason to re-enter â€” but a factory *may* still legitimately touch the
    /// registry for a **different** site (e.g. to consult a sibling), and that
    /// must not be able to hang. (Asking for the site being built is forbidden
    /// by the factory contract â€” it is a request for the store under
    /// construction.) Wrapped in a watchdog thread: a deadlock here would
    /// otherwise hang the whole test binary rather than fail it.
    #[test]
    fn factory_that_reenters_the_registry_does_not_deadlock() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;

        #[derive(Debug)]
        struct Noop;
        impl Replicator for Noop {
            fn replicate_set(&self, _k: String, _v: Vec<u8>, _t: Option<Duration>) -> bool {
                true
            }
            fn replicate_remove(&self, _key: &str) -> bool {
                true
            }
            fn replicate_expire(&self, _key: &str, _ttl: Duration) -> bool {
                true
            }
            fn replicate_published(&self, _k: String, _v: Vec<u8>, _t: Option<Duration>) {}
        }

        let done = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();

        let done_worker = Arc::clone(&done);
        std::thread::spawn(move || {
            let default = Store::new(StoreConfig::default());
            let mt = MultiTenantStore::new(default, test_config());

            let reentrant = mt.clone();
            mt.set_replicator_factory(Some(Arc::new(move |site: &str, _store: &Arc<Store>| {
                // Re-enter the registry from inside the factory for a DIFFERENT
                // site. Under the old entry-closure creation this took the same
                // shard lock the caller already held and deadlocked; with the
                // store built outside the map it simply works.
                //
                // The `!=` guard keeps the factory inside its own contract: it
                // never asks for the site it is being built for (which would be
                // a request for the store under construction).
                if site != "other.test" {
                    let _sibling = reentrant.get_site_store("other.test");
                }
                Arc::new(Noop) as Arc<dyn Replicator>
            })));

            let store = mt.get_site_store("alice.test");
            assert!(store.set("k".into(), b"v".to_vec(), None));
            done_worker.store(true, Ordering::SeqCst);
            let _ = tx.send(());
        });

        // 10s is enormous for what is a handful of map operations; a deadlock
        // never finishes, so this cleanly separates the two outcomes.
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok()
                && done.load(Ordering::SeqCst),
            "get_site_store deadlocked with a re-entrant factory â€” no user code may run \
             while a shard lock is held"
        );
    }

    /// Concurrent first access to the **same new site** from many threads, with
    /// a factory installed: everyone must get the one store the map holds, the
    /// factory must not deadlock, and no write may land in a discarded store.
    #[test]
    fn concurrent_first_access_to_one_new_site_converges_on_one_store() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::mpsc;

        #[derive(Debug)]
        struct Noop;
        impl Replicator for Noop {
            fn replicate_set(&self, _k: String, _v: Vec<u8>, _t: Option<Duration>) -> bool {
                true
            }
            fn replicate_remove(&self, _key: &str) -> bool {
                true
            }
            fn replicate_expire(&self, _key: &str, _ttl: Duration) -> bool {
                true
            }
            fn replicate_published(&self, _k: String, _v: Vec<u8>, _t: Option<Duration>) {}
        }

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let default = Store::new(StoreConfig::default());
            let mt = MultiTenantStore::new(default, test_config());
            let builds = Arc::new(AtomicUsize::new(0));
            let builds_for_factory = Arc::clone(&builds);
            mt.set_replicator_factory(Some(Arc::new(move |_s: &str, _store: &Arc<Store>| {
                builds_for_factory.fetch_add(1, Ordering::Relaxed);
                Arc::new(Noop) as Arc<dyn Replicator>
            })));

            let stores: Vec<Arc<Store>> = std::thread::scope(|scope| {
                let handles: Vec<_> = (0..16)
                    .map(|_| {
                        let mt = mt.clone();
                        scope.spawn(move || mt.get_site_store("hot.test"))
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().expect("worker")).collect()
            });

            let first = &stores[0];
            let all_same = stores.iter().all(|s| Arc::ptr_eq(s, first));
            // Every racer's write must be visible through the registry â€” proof
            // that nobody was handed a store that was then discarded.
            // `set_local` bypasses the installed replicator (whose `Noop`
            // implementation deliberately writes nothing), so this measures
            // store identity rather than replication behaviour.
            for (i, s) in stores.iter().enumerate() {
                s.set_local(format!("k{i}"), b"v".to_vec(), None);
            }
            let via_registry = mt.get_site_store("hot.test");
            let all_visible =
                (0..stores.len()).all(|i| via_registry.get(&format!("k{i}")).is_some());
            let _ = tx.send((all_same, all_visible, mt.site_count()));
        });

        let (all_same, all_visible, site_count) = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("concurrent get_site_store deadlocked or panicked");
        assert!(all_same, "every racing caller must receive the one store the map holds");
        assert!(all_visible, "a write through any racer's handle must be in the registry's store");
        assert_eq!(site_count, 1, "one site key must yield exactly one store");
    }

    /// With no factory installed (single-node multi-tenant), site stores stay
    /// purely local â€” the pre-existing behaviour.
    #[test]
    fn without_a_factory_site_stores_stay_node_local() {
        let default = Store::new(StoreConfig::default());
        let mt = MultiTenantStore::new(default, test_config());
        let site = mt.get_site_store("alice.test");
        // No replicator installed â†’ `set` writes straight to the local map.
        assert!(site.set("k".into(), b"v".to_vec(), None));
        assert_eq!(site.get("k").as_deref(), Some(&b"v"[..]));
    }

    #[test]
    fn site_data_not_visible_from_default() {
        let default = Store::new(StoreConfig::default());
        let mt = MultiTenantStore::new(default, test_config());

        let site = mt.get_site_store("secret.com");
        site.set("password".into(), b"hunter2".to_vec(), None);

        // Default store should NOT see site data
        assert_eq!(mt.default_store().get("password"), None);

        // Another site should NOT see it either
        let other = mt.get_site_store("other.com");
        assert_eq!(other.get("password"), None);
    }
}
