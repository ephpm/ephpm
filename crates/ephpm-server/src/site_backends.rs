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
//! # Shape: one permanent slot per site, one lock per slot
//!
//! The registry is a [`DashMap`] from site key to a **permanent**
//! [`SiteSlot`]. The slot — not the map — owns that site's database:
//!
//! ```text
//!   DashMap<site key, Arc<SiteSlot>>        (sharded; never removes an entry)
//!                       │
//!                       └── SiteSlot { current: RwLock<Option<backend>>, … }
//!                                              ▲
//!                            read  = resolve an already-open database
//!                            write = open it, or evict (close) it
//! ```
//!
//! Consequences, in the order they matter:
//!
//! * **A hit takes no global lock.** Resolving an already-open site is a
//!   sharded map read plus a *read* acquisition of that one site's lock. Two
//!   requests for different sites never touch the same lock, and two requests
//!   for the *same* site share the read side.
//! * **A miss serializes per site, not globally.** Opening `<site>.db` happens
//!   under that slot's **write** lock, so concurrent first-requests for one
//!   site open it once and the rest observe the result — but opening site A
//!   does not delay resolving site B. (Before v0.7.1 a single registry mutex
//!   was held across every open, which is what made the past-cap open/evict
//!   path collapse throughput ~3.6× in the multi-tenant scaling benchmark:
//!   every request, hit or miss, queued behind whichever request was opening a
//!   file.)
//! * **Slots are never removed.** Eviction closes the database *inside* the
//!   slot; the slot itself stays in the map forever. See the invariants below —
//!   this is what makes per-slot locking sound.
//!
//! # The correctness core: three invariants
//!
//! 1. **One slot per site key, for the life of the process.** A slot is the
//!    per-site lock, so two slots for one key would be two independent locks —
//!    and therefore two concurrent opens of one file. `slots` is append-only:
//!    nothing ever removes an entry, so an `Arc<SiteSlot>` cloned out of the
//!    map is *always* the current slot for that key and can be used after the
//!    map guard is dropped. (Removing idle slots would save a few dozen bytes
//!    per site and reintroduce that race; the slot map is bounded by the number
//!    of distinct **valid** site keys the process serves, which is bounded by
//!    the vhost directories on disk — an unknown host resolves to no site key
//!    at all, and the per-site MySQL listener verifies the tenant's password
//!    before it ever resolves. An invalid key is rejected *before* a slot is
//!    created, so no caller can grow the map by naming files.)
//!
//! 2. **Never evict a site with a live session.** The `ephpm_db_*` bridge keeps
//!    a per-thread [`Session`](litewire::Session) that holds a clone of a site's
//!    backend `Arc`, as does every `pdo_mysql` connection on the per-site wire
//!    listener. Eviction is therefore *refcount-aware*: a site is a victim only
//!    when `Arc::strong_count` of its backend is `1` (only the slot holds it →
//!    no live session). This makes the fd cap a **soft** bound — a site pinned
//!    by a live session stays open past the cap — but it guarantees eviction
//!    never yanks a database out from under a running query. The count is read
//!    while holding the slot's **write** lock, and a resolver can only clone
//!    the backend while holding its **read** lock, so the check can never race
//!    a clone: a resolver that got there first has already raised the count to
//!    2 and the slot is skipped.
//!
//! 3. **At most one open handle per file at any instant.** Turso's
//!    multi-*process* restriction is documented, but two `Database` handles on
//!    one file *in one process* is unverified and treated as unsafe. Every
//!    transition of `current` — `None → Some` (open) and `Some → None`
//!    (evict) — happens under that slot's write lock, and the open `await`s
//!    and the close `drop`s *inside* the guarded region. Since invariant 1
//!    guarantees exactly one slot (and therefore one such lock) per file, an
//!    open can never overlap another open or a close of the same file. On the
//!    close side, invariant 2 means the taken `Arc` is the last reference, so
//!    dropping it under the guard closes the file before any re-open can begin.
//!
//! # Cap accounting
//!
//! With slots permanent, "how many databases are open" is no longer the map's
//! length: it is `open_count`, an atomic bumped on `None → Some` and dropped on
//! `Some → None`, both under the owning slot's write lock. Eviction runs after
//! an open pushes the count over `max_open`, walks the open slots oldest-first
//! and closes idle ones until the count is back under the cap. Two concurrent
//! opens can both trigger a pass; that is harmless (each close is individually
//! guarded, and the passes re-read the count), it just means the cap is
//! enforced eventually rather than instantaneously — as it already was, given
//! invariant 2 lets live sessions exceed it outright.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::Context as _;
use dashmap::DashMap;
use ephpm_php::db_bridge::SiteBackendResolver;
use litewire::backend::SharedBackend;
use tokio::sync::RwLock;

use crate::tracked_backend::TrackedBackend;

/// One site's permanent registry slot: its database (when open) plus the lock
/// that serializes opening and closing *that site* and nothing else.
struct SiteSlot {
    /// The site's erased, stats-wrapped Turso backend while it is open.
    ///
    /// Read-locked to resolve (hot path), write-locked to open or evict. The
    /// `Arc::strong_count` of the value inside is what makes eviction
    /// refcount-aware: `1` means only this slot holds it (idle), `> 1` means at
    /// least one thread has a live [`Session`](litewire::Session) or wire
    /// connection into it.
    current: RwLock<Option<SharedBackend>>,
    /// Lock-free hint for the eviction scan: roughly "is `current` populated".
    /// Written under the write lock alongside `current`, read with no lock at
    /// all, and only ever used to *skip* slots — the authoritative check
    /// happens under the write lock in [`SiteBackends::close_if_idle`], so a
    /// stale read can at worst cost one wasted candidate or skip a victim this
    /// pass.
    open: AtomicBool,
    /// Milliseconds since the registry epoch at the last access — the LRU key.
    last_used: AtomicU64,
}

impl SiteSlot {
    /// A fresh slot for a site whose database is not open yet.
    fn new(now: u64) -> Self {
        Self {
            current: RwLock::new(None),
            open: AtomicBool::new(false),
            last_used: AtomicU64::new(now),
        }
    }

    /// Record an access for the LRU.
    fn touch(&self, now: u64) {
        self.last_used.store(now, Ordering::Relaxed);
    }
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
    /// Append-only map of site key → its permanent slot. See the module docs,
    /// invariant 1, for why entries are never removed.
    slots: DashMap<Box<str>, Arc<SiteSlot>>,
    /// How many slots currently hold an open database — the quantity
    /// `max_open` bounds. Maintained under the owning slot's write lock.
    open_count: AtomicUsize,
    /// Total databases opened since startup (diagnostics and tests: it is how
    /// a test proves a concurrent stampede opened one file once).
    opens_total: AtomicU64,
    /// Registry-epoch milliseconds of the last "cannot meet the cap" warning,
    /// `0` for never. Throttles it — see [`SiteBackends::warn_cap_pinned`].
    last_cap_warn: AtomicU64,
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
                slots: DashMap::new(),
                open_count: AtomicUsize::new(0),
                opens_total: AtomicU64::new(0),
                last_cap_warn: AtomicU64::new(0),
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
    fn open_count(&self) -> usize {
        self.inner.open_count.load(Ordering::Relaxed)
    }

    /// Number of database opens performed since startup (test helper): a
    /// concurrent stampede on one cold site must add exactly one.
    #[cfg(test)]
    fn opens_total(&self) -> u64 {
        self.inner.opens_total.load(Ordering::Relaxed)
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
    ///
    /// `pub(crate)` so the site-key agreement test can assert the *file* this
    /// registry would open for a key against the other three derivations
    /// (document root, state root, wire credential) — see
    /// `router::tests::site_key_agreement`.
    ///
    /// # Errors
    ///
    /// Fails when `site_key` is not a valid site key.
    pub(crate) fn db_path_for(&self, site_key: &str) -> anyhow::Result<PathBuf> {
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
        self.inner.opens_total.fetch_add(1, Ordering::Relaxed);
        tracing::info!(site = site_key, path = path_str, "opened per-site database (Turso engine)");
        // Screening sits innermost, against the raw engine: it must see exactly
        // the SQL the engine would run (the wire path's statements arrive here
        // already translated out of the MySQL dialect). Both tenant routes —
        // `pdo_mysql` and the `ephpm_db_*` bridge — go through this backend, so
        // both refuse `ATTACH` and friends for ePHPm's own reasons rather than
        // relying on the pinned engine's defaults. See `screened_backend`.
        let screened = crate::screened_backend::ScreenedBackend::new(turso);
        Ok(Arc::new(TrackedBackend::new(screened, self.inner.query_stats.clone())))
    }

    /// The permanent slot for `site_key`, creating it on first sighting.
    ///
    /// A key that is not a valid site key is rejected **before** a slot is
    /// created, so the slot map can only ever grow by real, router-approved (or
    /// password-verified) tenants — see the module docs, invariant 1.
    ///
    /// # Errors
    ///
    /// Fails when `site_key` is not a valid site key.
    fn slot_for(&self, site_key: &str) -> anyhow::Result<Arc<SiteSlot>> {
        if let Some(slot) = self.inner.slots.get(site_key) {
            return Ok(Arc::clone(slot.value()));
        }
        // First sighting: validate the key (which is also what derives the
        // path) before anything is allocated for it.
        self.db_path_for(site_key)?;
        let now = self.now_millis();
        let entry = self
            .inner
            .slots
            .entry(Box::from(site_key))
            .or_insert_with(|| Arc::new(SiteSlot::new(now)));
        Ok(Arc::clone(entry.value()))
    }

    /// Get-or-open the backend for `site_key`.
    ///
    /// A hit takes no global lock (sharded map read + this site's read lock); a
    /// miss serializes only against other requests for the *same* site. See the
    /// module docs for the invariants that keeps intact.
    ///
    /// Shared by both tenant paths: the `ephpm_db_*` bridge (through
    /// [`SiteBackendResolver::resolve`]) and the multi-tenant MySQL wire
    /// listener (through
    /// [`SiteWireAuth`](crate::site_wire_auth::SiteWireAuth)), so a site's
    /// bridge queries and its `pdo_mysql` connections land on the *same*
    /// backend instance and the same LRU entry rather than two handles on one
    /// file.
    ///
    /// # Errors
    ///
    /// Fails on an invalid site key or if the database cannot be opened. Both
    /// callers treat that as "no database for this request/connection" — never
    /// as a reason to fall back to another site's.
    pub(crate) async fn get_or_open(&self, site_key: &str) -> anyhow::Result<SharedBackend> {
        let slot = self.slot_for(site_key)?;

        // Hot path: already open. The clone happens under the read lock, so
        // eviction (which needs the write lock) can never observe
        // `strong_count == 1` racing it.
        {
            let current = slot.current.read().await;
            if let Some(backend) = current.as_ref() {
                slot.touch(self.now_millis());
                return Ok(Arc::clone(backend));
            }
        }

        // Miss: open under this site's write lock. Every other site's resolve
        // proceeds untouched; concurrent first-requests for *this* site queue
        // here and take the branch below.
        let path = self.db_path_for(site_key)?;
        let mut current = slot.current.write().await;
        if let Some(backend) = current.as_ref() {
            // Someone else opened it while we waited for the lock.
            slot.touch(self.now_millis());
            return Ok(Arc::clone(backend));
        }
        let backend = self.open_backend(site_key, &path).await?;
        *current = Some(Arc::clone(&backend));
        slot.open.store(true, Ordering::Relaxed);
        slot.touch(self.now_millis());
        let open_now = self.inner.open_count.fetch_add(1, Ordering::Relaxed) + 1;
        drop(current);

        if open_now > self.inner.max_open {
            self.evict_over_cap(site_key);
        }
        Ok(backend)
    }

    /// Close least-recently-used **idle** databases until `max_open` is met
    /// again. `protect` (the site just opened) is never a victim.
    ///
    /// One pass: it snapshots the open slots oldest-first and closes what it
    /// can. A pass that frees nothing means every open database is pinned by a
    /// live session or busy being opened — the cap is soft by design (module
    /// docs, invariant 2), so it warns and gives up rather than spinning.
    fn evict_over_cap(&self, protect: &str) {
        let mut candidates: Vec<(u64, Arc<SiteSlot>)> = self
            .inner
            .slots
            .iter()
            .filter(|e| e.key().as_ref() != protect && e.value().open.load(Ordering::Relaxed))
            .map(|e| (e.value().last_used.load(Ordering::Relaxed), Arc::clone(e.value())))
            .collect();
        candidates.sort_unstable_by_key(|(last_used, _)| *last_used);

        let mut closed = 0usize;
        for (_, slot) in candidates {
            if self.inner.open_count.load(Ordering::Relaxed) <= self.inner.max_open {
                break;
            }
            if self.close_if_idle(&slot) {
                closed += 1;
            }
        }

        if closed == 0 {
            self.warn_cap_pinned();
        } else {
            tracing::debug!(closed, "evicted idle per-site databases (LRU, over cap)");
        }
    }

    /// Warn that the cap cannot currently be met — at most once every 10s.
    ///
    /// Throttled because the condition is not rare: whenever `max_open_dbs` is
    /// below the number of *concurrently active* sites, every request finds
    /// every open database pinned, and an unthrottled warning would cost more
    /// than the eviction pass that produced it (and would bury the log).
    fn warn_cap_pinned(&self) {
        const EVERY_MS: u64 = 10_000;
        let now = self.now_millis();
        let last = self.inner.last_cap_warn.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < EVERY_MS {
            return;
        }
        // Lose the race → someone else is emitting it right now; stay quiet.
        if self
            .inner
            .last_cap_warn
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        tracing::warn!(
            open = self.inner.open_count.load(Ordering::Relaxed),
            cap = self.inner.max_open,
            "all open per-site databases have live sessions — cannot evict to meet \
             max_open_dbs; the open-fd count is temporarily over the configured cap \
             (further occurrences within 10s are suppressed)"
        );
    }

    /// Close one slot's database if it is open and idle. Returns whether it
    /// closed anything.
    ///
    /// `try_write` rather than `write`: a slot that is mid-open (or being read
    /// by a resolver right now) is not a victim, and blocking here would put
    /// the evicting request back behind exactly the serialization this registry
    /// exists to avoid. Skipping is always safe — the cap is soft.
    fn close_if_idle(&self, slot: &SiteSlot) -> bool {
        let Ok(mut current) = slot.current.try_write() else {
            return false;
        };
        // Idle == the slot holds the only reference. Checked under the write
        // lock, so no resolver can be cloning it concurrently (module docs,
        // invariant 2).
        match current.as_ref() {
            Some(backend) if Arc::strong_count(backend) == 1 => {}
            _ => return false,
        }
        let backend = current.take();
        slot.open.store(false, Ordering::Relaxed);
        self.inner.open_count.fetch_sub(1, Ordering::Relaxed);
        // Drop the last reference — which closes the file — while the write
        // lock is still held, so no re-open of this database can begin before
        // the old handle is gone (module docs, invariant 3).
        drop(backend);
        drop(current);
        true
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
        assert_eq!(reg.open_count(), 1);
        assert_eq!(reg.opens_total(), 1);
    }

    /// An invalid (traversal) site key is rejected rather than opening a file
    /// outside `dir` — and, since a slot is the per-site open lock, it must not
    /// leave a slot behind either (module docs, invariant 1).
    #[tokio::test]
    async fn invalid_key_is_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let reg = registry(tmp.path().to_path_buf(), 8);
        assert!(reg.get_or_open("../etc/passwd").await.is_err());
        assert!(reg.get_or_open("a/b").await.is_err());
        assert!(reg.get_or_open("").await.is_err());
        assert_eq!(reg.inner.slots.len(), 0, "an invalid key must not allocate a slot");
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
        assert_eq!(reg.open_count(), 1);

        // Open B: over cap, but A is pinned (strong_count > 1), so A cannot be
        // evicted — both stay open (soft bound), and a warning is logged.
        let _b = reg.get_or_open("b.test").await.expect("open b");
        assert_eq!(reg.open_count(), 2, "A is pinned by a live clone; cannot evict it");

        // Drop the live pin; A becomes idle (only the slot holds it).
        drop(a_live);

        // Open C: now the LRU can evict an idle victim to meet the cap of 1.
        let _c = reg.get_or_open("c.test").await.expect("open c");
        assert!(reg.open_count() <= 2, "eviction should reclaim at least one idle db");

        // Re-requesting an evicted site re-opens it correctly.
        let a_again = reg.get_or_open("a.test").await.expect("re-open a");
        let a_conn = a_again.connect().await.expect("connect re-opened a");
        a_conn.query("SELECT 1", &[]).await.expect("evicted db re-opens and works");
    }

    /// A stampede of concurrent resolves for one cold site opens the file
    /// **once** and hands everyone the same backend — the per-site write lock
    /// doing its job (module docs, invariant 3).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_resolve_of_one_cold_site_opens_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let reg = registry(tmp.path().to_path_buf(), 64);

        let tasks: Vec<_> = (0..32)
            .map(|_| {
                let reg = reg.clone();
                tokio::spawn(async move { reg.get_or_open("hot.test").await.expect("resolve") })
            })
            .collect();

        let mut backends = Vec::new();
        for t in tasks {
            backends.push(t.await.expect("join"));
        }

        let first = &backends[0];
        assert!(
            backends.iter().all(|b| Arc::ptr_eq(b, first)),
            "every concurrent resolve must get the one backend instance"
        );
        assert_eq!(reg.opens_total(), 1, "the file must have been opened exactly once");
        assert_eq!(reg.open_count(), 1);
    }

    /// Concurrent resolves of *different* cold sites all succeed and each opens
    /// its own file exactly once — the case a global open lock serialized.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_resolve_of_distinct_sites() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let reg = registry(tmp.path().to_path_buf(), 64);

        let tasks: Vec<_> = (0..24)
            .map(|i| {
                let reg = reg.clone();
                tokio::spawn(async move {
                    let key = format!("site-{i:03}.test");
                    let backend = reg.get_or_open(&key).await.expect("resolve");
                    // Each handle is usable, i.e. it is a real open database.
                    let conn = backend.connect().await.expect("connect");
                    conn.query("SELECT 1", &[]).await.expect("query");
                    key
                })
            })
            .collect();

        for t in tasks {
            let key = t.await.expect("join");
            assert!(tmp.path().join(format!("{key}.db")).exists());
        }
        assert_eq!(reg.opens_total(), 24);
        assert_eq!(reg.open_count(), 24);
    }

    /// The cap is enforced once the pins are gone: opening `cap + 8` sites with
    /// no live sessions leaves at most `cap` open.
    #[tokio::test]
    async fn cap_is_enforced_for_idle_sites() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let reg = registry(tmp.path().to_path_buf(), 4);

        for i in 0..12 {
            // Dropping the returned backend immediately makes each site idle,
            // so the LRU always has a victim available.
            drop(reg.get_or_open(&format!("s-{i:02}.test")).await.expect("open"));
        }

        assert!(
            reg.open_count() <= 4,
            "open databases ({}) must be bounded by the cap once sites are idle",
            reg.open_count()
        );
        assert_eq!(reg.inner.slots.len(), 12, "slots are permanent, one per site key");
    }

    /// Eviction racing concurrent resolves of the same sites: with a tight cap
    /// and many workers churning a small site set, every resolve must still
    /// return a *working* database. A handle closed out from under a caller —
    /// or a second handle opened on a file whose first handle was still alive —
    /// would surface here as a query error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn eviction_races_resolve_without_losing_a_database() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Cap far below the working set, so nearly every resolve triggers an
        // open and an eviction pass.
        let reg = registry(tmp.path().to_path_buf(), 2);

        let tasks: Vec<_> = (0..16)
            .map(|w| {
                let reg = reg.clone();
                tokio::spawn(async move {
                    for round in 0..12 {
                        let key = format!("race-{}.test", (w + round) % 8);
                        let backend = reg.get_or_open(&key).await.expect("resolve");
                        let conn = backend.connect().await.expect("connect");
                        conn.query("SELECT 1", &[]).await.expect("query on a live handle");
                    }
                })
            })
            .collect();

        for t in tasks {
            t.await.expect("worker");
        }

        // Every query above ran on a handle the registry handed out while
        // eviction was actively closing databases; that they all succeeded is
        // the assertion. What is left must still be bounded by the working set,
        // and every site must have been opened at least once.
        assert!(reg.opens_total() >= 8, "each of the 8 sites must have been opened at least once");
        assert!(reg.open_count() <= 8, "open count ({}) ran away under churn", reg.open_count());
    }
}
