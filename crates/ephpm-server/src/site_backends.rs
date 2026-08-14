//! Per-site Turso backend registry with an LRU of open databases.
//!
//! # Why this exists
//!
//! Turso (like SQLite) has no authorizer, no `GRANT`, and no per-schema ACL,
//! so the only tenant-isolation unit is the **database file**. In multi-site
//! mode ePHPm therefore gives every virtual host its own database at
//! `<dir>/<site_key>.db`, opened lazily on that site's first query. This
//! registry owns that mapping and hands the same backend to the in-process
//! `ephpm_db_*` bridge for a given request's site — so tenant A's PHP and
//! tenant B's PHP resolve to different files and cannot read each other's data
//! (closing the cross-tenant primitive of issue #274 / pentest finding C1).
//!
//! # Bounding open file descriptors (the LRU)
//!
//! Turso holds a file open per `Database` factory (roughly `db` + `-wal`), so a
//! box with thousands of sites cannot keep them all open. The registry caps the
//! number of simultaneously-open databases at `max_open_dbs` and, when full,
//! evicts the least-recently-used **idle** site (one with no live bridge
//! session) to make room. A later request for an evicted site re-opens it.
//!
//! # The correctness core: LRU eviction vs. live sessions, and single-open
//!
//! Two invariants must hold together:
//!
//! 1. **Never evict a site with a live session.** The `ephpm_db_*` bridge keeps
//!    a per-thread [`Session`](litewire::Session) that holds a clone of a site's
//!    backend `Arc`. Eviction is therefore *refcount-aware*: a site is a victim
//!    only when `Arc::strong_count` of its backend is `1` (only the registry
//!    holds it → no live session). This makes the fd cap a **soft** bound — a
//!    site pinned by a live session stays open past the cap — but it guarantees
//!    eviction never yanks a database out from under a running query.
//!
//! 2. **At most one open handle per file at any instant.** Turso's
//!    multi-*process* restriction is documented, but two `Database` handles on
//!    one file *in one process* is unverified and treated as unsafe. To make it
//!    impossible, the registry serializes all opens/evicts/lookups under a
//!    single async mutex and holds it **across** the open. Combined with
//!    invariant 1 (a removed entry always had `strong_count == 1`, so removing
//!    it drops the last reference and closes the file before any re-open can
//!    begin), no two handles for one file can ever coexist. The cost — opens
//!    serialize behind the mutex — is paid only on a site's first request
//!    (rare) and naturally damps cold-start open storms.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::Context as _;
use ephpm_php::db_bridge::SiteBackendResolver;
use litewire::backend::SharedBackend;
use tokio::sync::Mutex;

use crate::tracked_backend::TrackedBackend;

/// One cached open database.
struct Entry {
    /// The erased, stats-wrapped Turso backend for this site. Its
    /// `Arc::strong_count` is what makes eviction refcount-aware: `1` means
    /// only the registry holds it (idle), `> 1` means at least one thread has a
    /// live [`Session`](litewire::Session) into it.
    backend: SharedBackend,
    /// Milliseconds since the registry epoch at the last access — the LRU key.
    last_used: AtomicU64,
}

/// Shared inner state, held behind an `Arc` so [`SiteBackends`] is cheap to
/// clone (one instance is shared by the wire-startup path and the PHP bridge).
struct Inner {
    /// Directory holding `<site_key>.db` files.
    dir: PathBuf,
    /// Maximum number of databases held open at once.
    max_open: usize,
    /// Query-stats collector wrapped around every per-site backend, so bridge
    /// queries land on the same metrics surface as the single-node path.
    query_stats: ephpm_query_stats::QueryStats,
    /// The server's tokio runtime, pinned so the synchronous
    /// [`SiteBackendResolver::resolve`] can `block_on` the async open path.
    handle: tokio::runtime::Handle,
    /// Monotonic base for `last_used` timestamps.
    epoch: Instant,
    /// The open-database cache. A single async mutex guards it and is held
    /// across opens — see the module docs, invariant 2.
    open: Mutex<HashMap<Box<str>, Entry>>,
}

/// Per-site backend registry. Cheap to clone (shares one [`Inner`]).
#[derive(Clone)]
pub struct SiteBackends {
    inner: Arc<Inner>,
}

impl SiteBackends {
    /// Build a registry over `dir` (created if absent).
    ///
    /// # Errors
    ///
    /// Fails if `dir` cannot be created.
    pub fn new(
        dir: PathBuf,
        max_open: usize,
        query_stats: ephpm_query_stats::QueryStats,
        handle: tokio::runtime::Handle,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&dir).with_context(|| {
            format!("failed to create per-site database directory: {}", dir.display())
        })?;
        // A cap of zero would make every request unserviceable; clamp to 1 and
        // warn rather than deadlock.
        let max_open = if max_open == 0 {
            tracing::warn!("[db.sqlite] max_open_dbs = 0 is invalid; using 1");
            1
        } else {
            max_open
        };
        Ok(Self {
            inner: Arc::new(Inner {
                dir,
                max_open,
                query_stats,
                handle,
                epoch: Instant::now(),
                open: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// Hand this registry to the PHP bridge as a [`SiteBackendResolver`].
    #[must_use]
    pub fn as_resolver(&self) -> Arc<dyn SiteBackendResolver> {
        Arc::new(self.clone())
    }

    /// Number of databases currently held open (test/observability helper).
    #[cfg(test)]
    async fn open_count(&self) -> usize {
        self.inner.open.lock().await.len()
    }

    /// Milliseconds since the registry epoch, saturating.
    fn now_millis(&self) -> u64 {
        u64::try_from(self.inner.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Derive and validate the on-disk path for `site_key`.
    ///
    /// Defense-in-depth: even though the router already validated the key
    /// before it reached here, re-check it — a database path must never be
    /// derived from anything but a traversal-safe `[a-z0-9._-]` key, so the
    /// join can only ever name a direct child of `dir`.
    fn db_path_for(&self, site_key: &str) -> anyhow::Result<PathBuf> {
        anyhow::ensure!(
            crate::router::is_valid_site_key(site_key),
            "refusing to derive a database path from an invalid site key: {site_key:?}"
        );
        let path = self.inner.dir.join(format!("{site_key}.db"));
        // The key has no separators or `..`, so this always holds; assert it so
        // any future change to key validation can't silently open the escape.
        debug_assert_eq!(path.parent(), Some(self.inner.dir.as_path()));
        Ok(path)
    }

    /// Open a fresh per-site backend (Turso engine) wrapped in the query-stats
    /// tracker.
    async fn open_backend(&self, site_key: &str, path: &Path) -> anyhow::Result<SharedBackend> {
        let path_str = path.to_str().with_context(|| {
            format!("per-site database path is not valid UTF-8: {}", path.display())
        })?;
        let turso = litewire::Turso::open(path_str).await.with_context(|| {
            format!("failed to open per-site database for {site_key}: {path_str}")
        })?;
        tracing::info!(site = site_key, path = path_str, "opened per-site database (Turso engine)");
        Ok(Arc::new(TrackedBackend::new(turso, self.inner.query_stats.clone())))
    }

    /// Get-or-open the backend for `site_key`. Serialized under the registry
    /// mutex (held across the open) — see the module docs, invariant 2.
    async fn get_or_open(&self, site_key: &str) -> anyhow::Result<SharedBackend> {
        let mut open = self.inner.open.lock().await;

        if let Some(entry) = open.get(site_key) {
            entry.last_used.store(self.now_millis(), Ordering::Relaxed);
            // Clone under the lock: eviction (which needs the same lock) can
            // never observe `strong_count == 1` racing this clone.
            return Ok(Arc::clone(&entry.backend));
        }

        // Miss: derive+validate the path, then open with the lock still held so
        // no second handle for this file can be created concurrently.
        let path = self.db_path_for(site_key)?;
        let backend = self.open_backend(site_key, &path).await?;
        open.insert(
            Box::from(site_key),
            Entry { backend: Arc::clone(&backend), last_used: AtomicU64::new(self.now_millis()) },
        );
        self.evict_over_cap(&mut open, site_key);
        Ok(backend)
    }

    /// Evict least-recently-used **idle** databases until the cap is met.
    /// Runs under the held registry lock. `protect` (the site just opened) is
    /// never evicted.
    fn evict_over_cap(&self, open: &mut HashMap<Box<str>, Entry>, protect: &str) {
        while open.len() > self.inner.max_open {
            // Pick the idle (strong_count == 1) entry with the oldest last_used.
            let victim = open
                .iter()
                .filter(|(k, e)| k.as_ref() != protect && Arc::strong_count(&e.backend) == 1)
                .min_by_key(|(_, e)| e.last_used.load(Ordering::Relaxed))
                .map(|(k, _)| k.clone());

            match victim {
                Some(k) => {
                    // Removing the last reference (strong_count was 1) drops the
                    // backend and closes the file — before any re-open can start,
                    // because that would need this same lock.
                    open.remove(&k);
                    tracing::debug!(site = %k, "evicted idle per-site database (LRU, over cap)");
                }
                None => {
                    tracing::warn!(
                        open = open.len(),
                        cap = self.inner.max_open,
                        "all open per-site databases have live sessions — cannot evict to meet \
                         max_open_dbs; the open-fd count is temporarily over the configured cap"
                    );
                    break;
                }
            }
        }
    }
}

impl SiteBackendResolver for SiteBackends {
    fn resolve(&self, site_key: &str) -> Result<SharedBackend, String> {
        // block_on is legal: `resolve` is only ever called from the bridge on a
        // PHP worker / spawn_blocking thread, never an async task (the same
        // invariant that licenses the bridge's own block_on).
        self.inner.handle.clone().block_on(self.get_or_open(site_key)).map_err(|e| format!("{e:#}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats() -> ephpm_query_stats::QueryStats {
        ephpm_query_stats::QueryStats::new(ephpm_query_stats::StatsConfig {
            enabled: false,
            slow_query_threshold: std::time::Duration::from_secs(1),
            max_digests: 16,
            metric_label_series_max: 16,
        })
    }

    fn registry(dir: PathBuf, max_open: usize) -> SiteBackends {
        SiteBackends::new(dir, max_open, stats(), tokio::runtime::Handle::current())
            .expect("build registry")
    }

    /// Two sites get two distinct files and their data is isolated.
    #[tokio::test]
    async fn per_site_files_created_and_isolated() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let reg = registry(tmp.path().to_path_buf(), 8);

        let a = reg.get_or_open("site-a.test").await.expect("open a");
        let b = reg.get_or_open("site-b.test").await.expect("open b");

        let a_conn = a.connect().await.expect("connect a");
        a_conn.execute("CREATE TABLE t (v TEXT)", &[]).await.expect("create on a");
        a_conn.execute("INSERT INTO t (v) VALUES ('a-secret')", &[]).await.expect("insert on a");

        // Both files exist and are distinct.
        assert!(tmp.path().join("site-a.test.db").exists());
        assert!(tmp.path().join("site-b.test.db").exists());

        // Site B's database has no such table — full physical isolation.
        let b_conn = b.connect().await.expect("connect b");
        let err = b_conn.query("SELECT v FROM t", &[]).await.expect_err("b must not see a's table");
        let msg = format!("{err}").to_ascii_lowercase();
        assert!(msg.contains("no such table"), "expected a missing-table error, got: {msg}");
    }

    /// A second open returns the same cached backend instance.
    #[tokio::test]
    async fn open_is_cached() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let reg = registry(tmp.path().to_path_buf(), 8);

        let a1 = reg.get_or_open("site-a.test").await.expect("open");
        let a2 = reg.get_or_open("site-a.test").await.expect("open again");
        assert!(Arc::ptr_eq(&a1, &a2), "second open must reuse the cached backend");
        assert_eq!(reg.open_count().await, 1);
    }

    /// An invalid (traversal) site key is rejected rather than opening a file
    /// outside `dir`.
    #[tokio::test]
    async fn invalid_key_is_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let reg = registry(tmp.path().to_path_buf(), 8);
        assert!(reg.get_or_open("../etc/passwd").await.is_err());
        assert!(reg.get_or_open("a/b").await.is_err());
        assert!(reg.get_or_open("").await.is_err());
    }

    /// LRU evicts an idle database when over cap, but never one pinned by a
    /// live session (a held backend clone), and a re-request re-opens.
    #[tokio::test]
    async fn lru_evicts_idle_but_not_live() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let reg = registry(tmp.path().to_path_buf(), 1);

        // Open A and hold a clone — simulates a live per-thread session pinning
        // A's database open.
        let a_live = reg.get_or_open("a.test").await.expect("open a");
        assert_eq!(reg.open_count().await, 1);

        // Open B: over cap, but A is pinned (strong_count > 1), so A cannot be
        // evicted — both stay open (soft bound), and a warning is logged.
        let _b = reg.get_or_open("b.test").await.expect("open b");
        assert_eq!(reg.open_count().await, 2, "A is pinned by a live clone; cannot evict it");

        // Drop the live pin; A becomes idle (strong_count == 1 in the registry).
        drop(a_live);

        // Open C: now the LRU can evict an idle victim to meet the cap of 1.
        let _c = reg.get_or_open("c.test").await.expect("open c");
        assert!(reg.open_count().await <= 2, "eviction should reclaim at least one idle db");

        // Re-requesting an evicted site re-opens it correctly.
        let a_again = reg.get_or_open("a.test").await.expect("re-open a");
        let a_conn = a_again.connect().await.expect("connect re-opened a");
        a_conn.query("SELECT 1", &[]).await.expect("evicted db re-opens and works");
    }
}
