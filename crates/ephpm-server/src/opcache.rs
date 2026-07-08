//! Cluster-wide OPcache invalidation watcher (Phase 1).
//!
//! Every PHP request routes through [`OpcacheWatcher::check`], which reads
//! `opcache:version:<vhost>` from the in-process KV store and compares it to
//! the last version this node acted on for that vhost. On a mismatch, the
//! caller (currently the `spawn_blocking` PHP dispatch closure in `router.rs`)
//! runs the FFI invalidator under the vhost's docroot, then advances the
//! stored version.
//!
//! Design: `site/content/roadmap/opcache-clustering.md` (Phase 1).
//!
//! # Concurrency
//!
//! - Fast path is one atomic load + one `DashMap::get` — sub-microsecond, no
//!   contention.
//! - When a new version is observed, a per-vhost `Mutex` serialises the
//!   actual invalidation so two concurrent requests for the same vhost do
//!   not both walk OPcache. Sibling vhosts hold independent mutexes and
//!   never contend.
//! - Double-checked locking after the mutex acquire covers the race where
//!   two requests read the stale version concurrently, both notice, and
//!   both queue on the mutex — only the first performs the invalidation.

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};

use ::metrics::counter;
use dashmap::DashMap;
use ephpm_kv::store::Store;

/// KV key prefix for the per-vhost version counter (`opcache:version:<vhost>`).
///
/// The value is treated as an opaque monotonically-nondecreasing counter — any
/// change (even to a smaller value) is treated as a deploy event. In practice
/// the CLI writes an `epoch_ms` timestamp; the KV store gossip-replicates it
/// across the cluster.
pub const KV_VERSION_PREFIX: &str = "opcache:version:";

/// The fallback vhost name for `[server] document_root` when no `sites_dir` is
/// configured, or when the CLI runs `ephpm cache reset` without `--site`.
pub const DEFAULT_VHOST: &str = "_default";

/// Broadcast key written by `ephpm deploy --all`. When present, the watcher
/// treats it as a "cluster-wide invalidate every vhost" event and folds its
/// version into each per-vhost check (`max(per_vhost, broadcast)`). Lets a
/// single-write cover every site without requiring the CLI to enumerate
/// them — a brand-new site whose per-vhost key has never been written still
/// picks up the broadcast on its first request.
pub const BROADCAST_VHOST: &str = "_all";

/// What triggered an invalidation. Used as a Prometheus label so operators can
/// distinguish `ephpm deploy` (KV) from `ephpm cache reset` (local CLI).
#[derive(Debug, Clone, Copy)]
pub enum InvalidationTrigger {
    /// The KV version key advanced (typically because a peer wrote to it).
    Kv,
    /// A local `ephpm cache reset` request bypassed the KV path.
    Cli,
}

impl InvalidationTrigger {
    /// Prometheus label value.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            InvalidationTrigger::Kv => "kv",
            InvalidationTrigger::Cli => "cli",
        }
    }
}

/// Per-vhost invalidation bookkeeping.
///
/// Kept behind an `Arc` inside the watcher so both the fast-path load and the
/// serialising mutex live in the same allocation without extra indirection at
/// the call site.
#[derive(Debug, Default)]
struct SiteOpcacheState {
    /// Last cluster-wide version this node invalidated OPcache against.
    ///
    /// Loaded on every request (`Acquire`); stored under the mutex after a
    /// successful invalidation (`Release`).
    last_invalidated_version: AtomicU64,
    /// Serialises invalidation walks for this vhost. Never held during the
    /// KV read or the atomic load — only around the actual OPcache walk so
    /// concurrent requests for the same vhost coalesce.
    invalidation_mutex: StdMutex<()>,
}

/// Decision returned by [`OpcacheWatcher::check`].
///
/// When [`Decision::Invalidate`] is returned, the caller must invoke the
/// OPcache invalidator (see [`OpcacheWatcher::mark_invalidated`]) on a
/// TSRM-registered thread and then advance the recorded version.
#[derive(Debug)]
pub enum Decision {
    /// No invalidation required — the fast path.
    NoOp,
    /// The KV version has advanced. Invalidate under `docroot`, then call
    /// [`OpcacheWatcher::mark_invalidated`] with `version` to record it.
    Invalidate {
        /// Current cluster-wide version to record after the invalidation runs.
        version: u64,
    },
}

/// Per-vhost OPcache-invalidation coordinator.
///
/// Cheap to clone (all state is behind `Arc`) — one instance lives on the
/// [`Router`](crate::router::Router) and is consulted before every PHP
/// dispatch.
#[derive(Clone, Debug)]
pub struct OpcacheWatcher {
    /// Per-vhost state keyed by the lowercased vhost name. `DashMap` for
    /// lock-free reads on the hot path; new entries are inserted lazily on
    /// first sight of a vhost.
    sites: Arc<DashMap<String, Arc<SiteOpcacheState>>>,
    /// Whether cluster invalidation is enabled. When `false`,
    /// [`OpcacheWatcher::check`] short-circuits to [`Decision::NoOp`] before
    /// touching the KV store.
    enabled: bool,
}

/// Fetch and parse `opcache:version:<vhost>` from the store. Returns `None`
/// on miss or malformed value.
fn read_version(store: &Store, vhost: &str) -> Option<u64> {
    let raw = store.get(&format!("{KV_VERSION_PREFIX}{vhost}"))?;
    let s = std::str::from_utf8(&raw).ok()?.trim();
    s.parse::<u64>().ok()
}

impl OpcacheWatcher {
    /// Construct a new watcher. `enabled` typically comes from
    /// [`OpcacheConfig::effective_cluster_invalidation`](ephpm_config::OpcacheConfig::effective_cluster_invalidation).
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self { sites: Arc::new(DashMap::new()), enabled }
    }

    /// Whether the watcher is enabled. When `false`, [`OpcacheWatcher::check`]
    /// always returns [`Decision::NoOp`] — no KV lookup, no atomic load.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Decide whether the current request should trigger an OPcache
    /// invalidation for `vhost`.
    ///
    /// Fast path: one `AtomicU64::load(Acquire)` on the site's counter plus a
    /// `DashMap::get` in the store — both are sub-microsecond in practice.
    ///
    /// Returns [`Decision::Invalidate`] when the KV version is strictly
    /// greater than the last recorded version on this node. The caller then
    /// runs the invalidator and calls [`OpcacheWatcher::mark_invalidated`].
    #[must_use]
    pub fn check(&self, store: &Store, vhost: &str) -> Decision {
        if !self.enabled {
            return Decision::NoOp;
        }

        // Fold the broadcast key into the per-vhost version so `ephpm deploy
        // --all` fans out without the CLI having to enumerate vhosts.
        let per_vhost = read_version(store, vhost).unwrap_or(0);
        let broadcast = read_version(store, BROADCAST_VHOST).unwrap_or(0);
        let current_version = per_vhost.max(broadcast);
        if current_version == 0 {
            return Decision::NoOp;
        }

        let state = self.state_for(vhost);
        if current_version > state.last_invalidated_version.load(Ordering::Acquire) {
            Decision::Invalidate { version: current_version }
        } else {
            Decision::NoOp
        }
    }

    /// Serialise the invalidation walk under the per-vhost mutex and, after
    /// the caller's `invalidator` runs, advance the recorded version.
    ///
    /// The double-checked pattern re-loads the recorded version after
    /// acquiring the mutex so a concurrent request that already ran the walk
    /// short-circuits without invalidating twice.
    ///
    /// `invalidator` receives the vhost's document root and returns the
    /// number of scripts invalidated (or `None` when OPcache is unavailable).
    /// Passing a closure keeps this module free of `#[cfg(php_linked)]` gating
    /// and makes unit-testing possible without libphp linked.
    pub fn mark_invalidated<F>(
        &self,
        vhost: &str,
        docroot: &Path,
        version: u64,
        trigger: InvalidationTrigger,
        invalidator: F,
    ) where
        F: FnOnce(&Path) -> Option<i64>,
    {
        let state = self.state_for(vhost);
        // Serialise siblings-of-same-vhost on the actual walk. If the mutex is
        // poisoned we still proceed — a poisoned mutex here just means another
        // thread panicked mid-walk, and the atomic advance below is the
        // correctness guard, not the mutex.
        let _guard = match state.invalidation_mutex.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        // Re-check under the mutex: another request may have run the walk
        // while we were queued.
        if state.last_invalidated_version.load(Ordering::Acquire) >= version {
            return;
        }
        let count = invalidator(docroot);
        state.last_invalidated_version.store(version, Ordering::Release);
        match count {
            Some(n) => {
                counter!(
                    "ephpm_opcache_invalidations_total",
                    "vhost" => vhost.to_string(),
                    "trigger" => trigger.label(),
                )
                .increment(1);
                tracing::info!(
                    vhost = %vhost,
                    docroot = %docroot.display(),
                    scripts_invalidated = n,
                    version,
                    trigger = trigger.label(),
                    "OPcache invalidated"
                );
            }
            None => {
                tracing::debug!(
                    vhost = %vhost,
                    docroot = %docroot.display(),
                    version,
                    trigger = trigger.label(),
                    "OPcache invalidation skipped (unavailable or bailout)"
                );
            }
        }
    }

    /// Look up (or create) the per-vhost state entry. Idempotent.
    fn state_for(&self, vhost: &str) -> Arc<SiteOpcacheState> {
        if let Some(existing) = self.sites.get(vhost) {
            return Arc::clone(existing.value());
        }
        // Insert-if-absent so two concurrent unknown vhosts don't create
        // parallel states.
        let state = self
            .sites
            .entry(vhost.to_string())
            .or_insert_with(|| Arc::new(SiteOpcacheState::default()));
        Arc::clone(state.value())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    use ephpm_kv::store::StoreConfig;

    use super::*;

    fn store() -> Arc<Store> {
        Store::new(StoreConfig::default())
    }

    fn write_version(store: &Store, vhost: &str, v: u64) {
        store.set(
            format!("{KV_VERSION_PREFIX}{vhost}"),
            v.to_string().into_bytes(),
            None,
        );
    }

    #[test]
    fn disabled_watcher_short_circuits() {
        let store = store();
        write_version(&store, "blog", 42);
        let watcher = OpcacheWatcher::new(false);
        assert!(matches!(watcher.check(&store, "blog"), Decision::NoOp));
    }

    #[test]
    fn missing_key_is_noop() {
        let watcher = OpcacheWatcher::new(true);
        assert!(matches!(watcher.check(&store(), "blog"), Decision::NoOp));
    }

    #[test]
    fn malformed_key_is_noop() {
        let store = store();
        store.set(
            format!("{KV_VERSION_PREFIX}blog"),
            b"not-a-number".to_vec(),
            None,
        );
        let watcher = OpcacheWatcher::new(true);
        assert!(matches!(watcher.check(&store, "blog"), Decision::NoOp));
    }

    #[test]
    fn advancing_version_triggers_invalidate() {
        let store = store();
        write_version(&store, "blog", 100);
        let watcher = OpcacheWatcher::new(true);
        match watcher.check(&store, "blog") {
            Decision::Invalidate { version } => assert_eq!(version, 100),
            Decision::NoOp => panic!("expected Invalidate on first check with version present"),
        }
    }

    #[test]
    fn mark_advances_and_deduplicates() {
        let store = store();
        write_version(&store, "blog", 100);
        let watcher = OpcacheWatcher::new(true);
        let calls = Arc::new(AtomicUsize::new(0));

        // First check + mark runs the invalidator once.
        let Decision::Invalidate { version } = watcher.check(&store, "blog") else {
            panic!("expected Invalidate");
        };
        let calls_c = Arc::clone(&calls);
        watcher.mark_invalidated(
            "blog",
            &PathBuf::from("/srv/blog"),
            version,
            InvalidationTrigger::Kv,
            move |_| {
                calls_c.fetch_add(1, Ordering::SeqCst);
                Some(3)
            },
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // A second check at the same version is a no-op.
        assert!(matches!(watcher.check(&store, "blog"), Decision::NoOp));

        // Even if a caller races past check() (e.g. we deliberately re-mark
        // with the same version), the double-checked pattern under the mutex
        // must not re-invoke the invalidator.
        let calls_c = Arc::clone(&calls);
        watcher.mark_invalidated(
            "blog",
            &PathBuf::from("/srv/blog"),
            version,
            InvalidationTrigger::Kv,
            move |_| {
                calls_c.fetch_add(1, Ordering::SeqCst);
                Some(3)
            },
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "mark_invalidated must dedupe under mutex");
    }

    #[test]
    fn siblings_are_isolated() {
        let store = store();
        write_version(&store, "blog", 100);
        write_version(&store, "shop", 200);
        let watcher = OpcacheWatcher::new(true);

        // Marking blog must not touch shop's recorded version.
        let Decision::Invalidate { version: bv } = watcher.check(&store, "blog") else {
            panic!("expected Invalidate for blog");
        };
        watcher.mark_invalidated(
            "blog",
            &PathBuf::from("/srv/blog"),
            bv,
            InvalidationTrigger::Kv,
            |_| Some(1),
        );

        // shop still needs invalidation.
        match watcher.check(&store, "shop") {
            Decision::Invalidate { version } => assert_eq!(version, 200),
            Decision::NoOp => panic!("shop's state was contaminated by blog's mark"),
        }
    }

    #[test]
    fn broadcast_key_triggers_all_vhosts() {
        // ephpm deploy --all writes opcache:version:_all. Every vhost that
        // has never had a per-vhost key still picks up the broadcast on its
        // first check.
        let store = store();
        write_version(&store, BROADCAST_VHOST, 500);
        let watcher = OpcacheWatcher::new(true);

        for vhost in ["blog", "shop", "docs"] {
            match watcher.check(&store, vhost) {
                Decision::Invalidate { version } => assert_eq!(version, 500),
                Decision::NoOp => panic!("broadcast should trigger {vhost}"),
            }
        }
    }

    #[test]
    fn broadcast_and_per_vhost_take_max() {
        let store = store();
        write_version(&store, BROADCAST_VHOST, 300);
        write_version(&store, "blog", 500);
        write_version(&store, "shop", 100);
        let watcher = OpcacheWatcher::new(true);

        // blog's per-vhost key wins.
        assert!(matches!(
            watcher.check(&store, "blog"),
            Decision::Invalidate { version: 500 }
        ));
        // shop's per-vhost key loses to the broadcast.
        assert!(matches!(
            watcher.check(&store, "shop"),
            Decision::Invalidate { version: 300 }
        ));
    }

    #[test]
    fn unavailable_opcache_still_advances_version() {
        // If the invalidator returns None (OPcache unavailable), we still
        // advance the recorded version — otherwise every subsequent request
        // would re-invoke the (failing) invalidator. The version is a
        // "we've seen this deploy" marker, not a "we successfully applied it"
        // marker.
        let store = store();
        write_version(&store, "blog", 100);
        let watcher = OpcacheWatcher::new(true);
        let Decision::Invalidate { version } = watcher.check(&store, "blog") else {
            panic!("expected Invalidate");
        };
        watcher.mark_invalidated(
            "blog",
            &PathBuf::from("/srv/blog"),
            version,
            InvalidationTrigger::Kv,
            |_| None,
        );
        assert!(matches!(watcher.check(&store, "blog"), Decision::NoOp));
    }
}
