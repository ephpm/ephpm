//! Warm-restart KV snapshots: when they are written, when they are loaded,
//! and how the running server reports on them (`[kv.snapshot]`).
//!
//! The format, encryption and I/O live in `ephpm_kv::snapshot`. This module
//! is the lifecycle around them:
//!
//! * **Startup** — [`KvSnapshotService::load_at_startup`] runs after the
//!   stores exist and before any listener binds, under `load_timeout`. Past
//!   the deadline the server starts with whatever was restored; a file that
//!   does not verify is ignored. Startup is never blocked indefinitely.
//! * **Shutdown** — [`KvSnapshotService::save_at_shutdown`] runs after the
//!   HTTP drain, so the snapshot reflects the last request served. It is
//!   deliberately **not** bounded by `[server.timeouts] shutdown` (that
//!   budget is for draining requests); its own last-resort bound is
//!   [`SHUTDOWN_SAVE_BOUND`].
//! * **Periodically** — `[kv.snapshot] interval`, for a crash-tolerant copy.
//! * **On demand** — `POST /_ephpm/kv/snapshot` (see the router), which
//!   `ephpm kv snapshot save` calls.
//!
//! Every write and load runs on a blocking thread (`spawn_blocking`): the
//! snapshot code is synchronous file + CPU work and must never hold a tokio
//! worker, for the same reason the engine work in `db_shutdown` is kept off
//! them (#621).
//!
//! A snapshot is a cache warm-up, not durability; see `ephpm_kv::snapshot`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context as _;
use ephpm_config::Config;
use ephpm_kv::multi_tenant::MultiTenantStore;
use ephpm_kv::snapshot::{
    LoadOptions, LoadOutcome, LoadReport, SaveReport, SnapshotError, WriteOptions, load_snapshot,
    write_snapshot,
};
use ephpm_kv::store::Store;
use metrics::{counter, gauge, histogram};

/// File extension of a snapshot.
pub const SNAPSHOT_EXTENSION: &str = "kvsnap";

/// Last-resort bound on the shutdown save. Generous: a 1 GiB snapshot is a
/// few seconds of work; this only exists so a stuck disk cannot keep a
/// stopped server alive forever. Past it the process exits and the next
/// start removes the abandoned temporary file.
pub const SHUTDOWN_SAVE_BOUND: Duration = Duration::from_secs(5 * 60);

/// Metric: snapshots written, by outcome and trigger.
pub const METRIC_SAVE_TOTAL: &str = "ephpm_kv_snapshot_save_total";
/// Metric: wall time of a snapshot write.
pub const METRIC_SAVE_DURATION: &str = "ephpm_kv_snapshot_save_duration_seconds";
/// Metric: bytes of the last successful snapshot.
pub const METRIC_LAST_SAVE_BYTES: &str = "ephpm_kv_snapshot_last_save_bytes";
/// Metric: keys in the last successful snapshot.
pub const METRIC_LAST_SAVE_KEYS: &str = "ephpm_kv_snapshot_last_save_keys";
/// Metric: snapshot loads, by outcome and reason.
pub const METRIC_LOAD_TOTAL: &str = "ephpm_kv_snapshot_load_total";
/// Metric: wall time of a snapshot load.
pub const METRIC_LOAD_DURATION: &str = "ephpm_kv_snapshot_load_duration_seconds";
/// Metric: keys restored by the last load.
pub const METRIC_LAST_LOAD_KEYS: &str = "ephpm_kv_snapshot_last_load_keys";

/// What triggered a save (metric label + log field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveTrigger {
    /// Graceful shutdown.
    Shutdown,
    /// `[kv.snapshot] interval`.
    Interval,
    /// `POST /_ephpm/kv/snapshot`.
    Request,
}

impl SaveTrigger {
    fn label(self) -> &'static str {
        match self {
            Self::Shutdown => "shutdown",
            Self::Interval => "interval",
            Self::Request => "request",
        }
    }
}

/// Outcome of the last save, for the status endpoint.
#[derive(Debug, Clone)]
pub struct SaveStatus {
    /// When it finished.
    pub at: SystemTime,
    /// What triggered it.
    pub trigger: SaveTrigger,
    /// The report, or the error.
    pub result: Result<SaveReport, String>,
}

/// Outcome of the last load, for the status endpoint.
#[derive(Debug, Clone)]
pub struct LoadStatus {
    /// When it finished.
    pub at: SystemTime,
    /// The report.
    pub report: LoadReport,
}

/// The running server's snapshot lifecycle. One per process, built by
/// [`KvSnapshotService::from_config`] when `[kv.snapshot] mode != "off"`.
pub struct KvSnapshotService {
    file: PathBuf,
    secret: Vec<u8>,
    node_id: String,
    store: Arc<Store>,
    sites: Option<MultiTenantStore>,
    max_size: u64,
    load_timeout: Duration,
    interval: Option<Duration>,
    on_shutdown: bool,
    load_on_start: bool,
    /// Serialises saves: a second trigger waits for (or, for the HTTP
    /// endpoint, is refused during) the one in flight.
    saving: tokio::sync::Mutex<()>,
    last_save: std::sync::Mutex<Option<SaveStatus>>,
    last_load: std::sync::Mutex<Option<LoadStatus>>,
}

impl std::fmt::Debug for KvSnapshotService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvSnapshotService")
            .field("file", &self.file)
            .field("node_id", &self.node_id)
            .field("multi_tenant", &self.sites.is_some())
            .field("max_size", &self.max_size)
            .field("load_timeout", &self.load_timeout)
            .field("interval", &self.interval)
            .field("on_shutdown", &self.on_shutdown)
            .field("load_on_start", &self.load_on_start)
            .finish_non_exhaustive()
    }
}

impl KvSnapshotService {
    /// Build the service from `[kv.snapshot]`, or `None` when snapshots are
    /// off. Creates the snapshot directory and removes temporary files an
    /// earlier process left behind.
    ///
    /// `node_id` is the effective cluster node id when clustered; it is only
    /// used for the header and as the identity fallback.
    ///
    /// # Errors
    ///
    /// A directory that cannot be created, or an unparsable knob (which
    /// `Config::validate` normally catches first).
    pub fn from_config(
        config: &Config,
        store: Arc<Store>,
        sites: Option<MultiTenantStore>,
        node_id: Option<&str>,
    ) -> anyhow::Result<Option<Arc<Self>>> {
        let snap = &config.kv.snapshot;
        if !snap.enabled() {
            return Ok(None);
        }
        let secret = config
            .kv
            .secret
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .context("[kv.snapshot] requires [kv] secret")?
            .as_bytes()
            .to_vec();
        let dir = PathBuf::from(
            snap.path.as_deref().context("[kv.snapshot] mode = \"local\" requires path")?,
        );
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating [kv.snapshot] path {}", dir.display()))?;
        let identity = snap
            .identity
            .clone()
            .or_else(|| Some(config.cluster.node_id.clone()).filter(|s| !s.is_empty()))
            .unwrap_or_else(host_name);
        let file = dir.join(format!("{identity}.{SNAPSHOT_EXTENSION}"));
        remove_stale_temp_files(&dir, &identity);
        let node_id = node_id.map_or_else(|| identity.clone(), str::to_string);
        tracing::info!(
            file = %file.display(),
            identity,
            on_shutdown = snap.on_shutdown,
            load_on_start = snap.load_on_start,
            interval = %snap.interval,
            "KV warm-restart snapshots enabled (local file)"
        );
        Ok(Some(Arc::new(Self {
            file,
            secret,
            node_id,
            store,
            sites,
            max_size: snap.max_size_bytes()?,
            load_timeout: snap.load_timeout()?,
            interval: snap.interval()?,
            on_shutdown: snap.on_shutdown,
            load_on_start: snap.load_on_start,
            saving: tokio::sync::Mutex::new(()),
            last_save: std::sync::Mutex::new(None),
            last_load: std::sync::Mutex::new(None),
        })))
    }

    /// The snapshot file this node writes and loads.
    #[must_use]
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// Every store with its site key, the default store first (`""`).
    fn stores(&self) -> Vec<(String, Arc<Store>)> {
        let mut out = vec![(String::new(), Arc::clone(&self.store))];
        if let Some(sites) = &self.sites {
            let mut named = sites.site_stores();
            named.sort_by(|a, b| a.0.cmp(&b.0));
            out.extend(named);
        }
        out
    }

    /// Load the snapshot if `load_on_start` is set. Call once, after the
    /// stores exist and before any listener binds.
    pub async fn load_at_startup(&self) -> Option<LoadReport> {
        if !self.load_on_start {
            return None;
        }
        Some(self.load_now().await)
    }

    /// Load the snapshot into the live stores now, under `load_timeout`.
    pub async fn load_now(&self) -> LoadReport {
        let file = self.file.clone();
        let secret = self.secret.clone();
        let store = Arc::clone(&self.store);
        let sites = self.sites.clone();
        let opts = LoadOptions {
            deadline: Some(Instant::now() + self.load_timeout),
            max_size: self.max_size,
            workers: 0,
        };
        let report = tokio::task::spawn_blocking(move || {
            let resolve = move |site: &str| -> Option<Arc<Store>> {
                if site.is_empty() {
                    Some(Arc::clone(&store))
                } else {
                    // A site store is created on demand: the vhost exists
                    // on disk whether or not a request has reached it yet,
                    // and its first request would create the store anyway.
                    sites.as_ref().map(|s| s.get_site_store(site))
                }
            };
            load_snapshot(&file, &secret, &resolve, &opts)
        })
        .await
        .unwrap_or_else(|e| {
            tracing::error!("KV snapshot load task failed: {e}");
            LoadReport {
                outcome: LoadOutcome::Cold(ephpm_kv::snapshot::ColdReason::Io),
                restored: 0,
                expired: 0,
                refused: 0,
                unknown_site: 0,
                blocks: 0,
                bad_blocks: 0,
                unreached_blocks: 0,
                bytes: 0,
                header: None,
                elapsed: Duration::ZERO,
            }
        });
        self.record_load(&report);
        report
    }

    fn record_load(&self, report: &LoadReport) {
        counter!(
            METRIC_LOAD_TOTAL,
            "outcome" => report.outcome.label(),
            "reason" => report.outcome.reason()
        )
        .increment(1);
        histogram!(METRIC_LOAD_DURATION).record(report.elapsed.as_secs_f64());
        gauge!(METRIC_LAST_LOAD_KEYS).set(report.restored as f64);
        match report.outcome {
            LoadOutcome::Ok => tracing::info!(
                file = %self.file.display(),
                keys = report.restored,
                expired_skipped = report.expired,
                blocks = report.blocks,
                bytes = report.bytes,
                elapsed_ms = report.elapsed.as_millis() as u64,
                written_by = report.header.as_ref().map_or("", |h| h.node_id.as_str()),
                "KV snapshot loaded — serving warm"
            ),
            LoadOutcome::Partial(reason) => tracing::warn!(
                file = %self.file.display(),
                reason = reason.label(),
                keys = report.restored,
                refused = report.refused,
                unknown_site = report.unknown_site,
                bad_blocks = report.bad_blocks,
                unreached_blocks = report.unreached_blocks,
                elapsed_ms = report.elapsed.as_millis() as u64,
                "KV snapshot partially loaded"
            ),
            // The cold reasons are logged by the loader itself (missing at
            // INFO, everything else at WARN) with the underlying error.
            LoadOutcome::Cold(_) => {}
        }
        if let Ok(mut slot) = self.last_load.lock() {
            *slot = Some(LoadStatus { at: SystemTime::now(), report: report.clone() });
        }
    }

    /// Write a snapshot now. Waits for a save already in flight.
    ///
    /// # Errors
    ///
    /// The write failed; the previous snapshot (if any) is intact.
    pub async fn save(&self, trigger: SaveTrigger) -> Result<SaveReport, SnapshotError> {
        let _guard = self.saving.lock().await;
        self.save_locked(trigger).await
    }

    /// Write a snapshot now unless one is already being written, in which
    /// case `Ok(None)` — the HTTP endpoint's "409 Conflict".
    ///
    /// # Errors
    ///
    /// As [`save`](Self::save).
    pub async fn try_save(
        &self,
        trigger: SaveTrigger,
    ) -> Result<Option<SaveReport>, SnapshotError> {
        let Ok(_guard) = self.saving.try_lock() else {
            return Ok(None);
        };
        self.save_locked(trigger).await.map(Some)
    }

    async fn save_locked(&self, trigger: SaveTrigger) -> Result<SaveReport, SnapshotError> {
        let file = self.file.clone();
        let secret = self.secret.clone();
        let stores = self.stores();
        let opts = WriteOptions {
            node_id: self.node_id.clone(),
            max_size: self.max_size,
            multi_tenant: self.sites.is_some(),
            ..WriteOptions::default()
        };
        let result =
            tokio::task::spawn_blocking(move || write_snapshot(&file, &secret, &stores, &opts))
                .await
                .unwrap_or_else(|e| {
                    Err(SnapshotError::Io(std::io::Error::other(format!("save task failed: {e}"))))
                });
        match &result {
            Ok(report) => {
                counter!(METRIC_SAVE_TOTAL, "outcome" => "ok", "trigger" => trigger.label())
                    .increment(1);
                histogram!(METRIC_SAVE_DURATION).record(report.elapsed.as_secs_f64());
                gauge!(METRIC_LAST_SAVE_BYTES).set(report.bytes as f64);
                gauge!(METRIC_LAST_SAVE_KEYS).set(report.keys() as f64);
                tracing::info!(
                    file = %self.file.display(),
                    trigger = trigger.label(),
                    keys = report.keys(),
                    sites = report.sites,
                    blocks = report.blocks,
                    bytes = report.bytes,
                    elapsed_ms = report.elapsed.as_millis() as u64,
                    "KV snapshot written"
                );
            }
            Err(e) => {
                counter!(METRIC_SAVE_TOTAL, "outcome" => "error", "trigger" => trigger.label())
                    .increment(1);
                tracing::warn!(
                    file = %self.file.display(),
                    trigger = trigger.label(),
                    "KV snapshot NOT written (the previous snapshot, if any, is kept): {e}"
                );
            }
        }
        if let Ok(mut slot) = self.last_save.lock() {
            *slot = Some(SaveStatus {
                at: SystemTime::now(),
                trigger,
                result: result.as_ref().map(Clone::clone).map_err(ToString::to_string),
            });
        }
        result
    }

    /// The shutdown save, if `on_shutdown` is set. Call after the HTTP drain.
    /// Bounded by [`SHUTDOWN_SAVE_BOUND`]; past it the process exits without
    /// a new snapshot (the old one stays).
    pub async fn save_at_shutdown(&self) {
        if !self.on_shutdown {
            return;
        }
        match tokio::time::timeout(SHUTDOWN_SAVE_BOUND, self.save(SaveTrigger::Shutdown)).await {
            Ok(_) => {}
            Err(_) => tracing::warn!(
                file = %self.file.display(),
                bound_secs = SHUTDOWN_SAVE_BOUND.as_secs(),
                "giving up on the shutdown KV snapshot (the write did not finish in time); \
                 exiting with the previous snapshot in place"
            ),
        }
    }

    /// Start the periodic save task, if `interval` is set. The returned
    /// handle is aborted at shutdown so a periodic save never overlaps the
    /// final one.
    pub fn spawn_periodic(self: &Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        let every = self.interval?;
        let svc = Arc::clone(self);
        Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(every);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // the first tick fires immediately; skip it
            loop {
                ticker.tick().await;
                let _ = svc.save(SaveTrigger::Interval).await;
            }
        }))
    }

    /// The status document served at `GET /_ephpm/kv/snapshot`.
    #[must_use]
    pub fn status_json(&self) -> String {
        let last_save = self.last_save.lock().ok().and_then(|s| s.clone());
        let last_load = self.last_load.lock().ok().and_then(|s| s.clone());
        let file_bytes = std::fs::metadata(&self.file).ok().map(|m| m.len());
        let save = last_save.map(|s| match s.result {
            Ok(r) => serde_json::json!({
                "at_unix_ms": ephpm_kv::snapshot::unix_ms_for(s.at),
                "trigger": s.trigger.label(),
                "ok": true,
                "bytes": r.bytes,
                "keys": r.keys(),
                "sites": r.sites,
                "blocks": r.blocks,
                "elapsed_ms": r.elapsed.as_millis() as u64,
            }),
            Err(e) => serde_json::json!({
                "at_unix_ms": ephpm_kv::snapshot::unix_ms_for(s.at),
                "trigger": s.trigger.label(),
                "ok": false,
                "error": e,
            }),
        });
        let load = last_load.map(|l| {
            let r = l.report;
            serde_json::json!({
                "at_unix_ms": ephpm_kv::snapshot::unix_ms_for(l.at),
                "outcome": r.outcome.label(),
                "reason": r.outcome.reason(),
                "keys": r.restored,
                "expired_skipped": r.expired,
                "refused": r.refused,
                "unknown_site": r.unknown_site,
                "blocks": r.blocks,
                "bad_blocks": r.bad_blocks,
                "unreached_blocks": r.unreached_blocks,
                "bytes": r.bytes,
                "elapsed_ms": r.elapsed.as_millis() as u64,
                "written_by": r.header.as_ref().map(|h| h.node_id.clone()),
                "created_at_unix_ms": r.header.as_ref().map(|h| h.created_at_ms),
            })
        });
        serde_json::json!({
            "mode": "local",
            "file": self.file.display().to_string(),
            "file_bytes": file_bytes,
            "on_shutdown": self.on_shutdown,
            "load_on_start": self.load_on_start,
            "interval_secs": self.interval.map(|d| d.as_secs()),
            "load_timeout_secs": self.load_timeout.as_secs(),
            "max_size": self.max_size,
            "saving": self.saving.try_lock().is_err(),
            "last_save": save,
            "last_load": load,
        })
        .to_string()
    }
}

/// Remove `.<identity>.kvsnap.tmp-*` files a crashed or timed-out writer left
/// in `dir`. They are never loadable (no trailer), only wasted space.
fn remove_stale_temp_files(dir: &Path, identity: &str) {
    let prefix = format!(".{identity}.{SNAPSHOT_EXTENSION}.tmp-");
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(&prefix) {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => tracing::info!(
                    file = %entry.path().display(),
                    "removed an abandoned temporary KV snapshot file"
                ),
                Err(e) => tracing::warn!(
                    file = %entry.path().display(),
                    "could not remove an abandoned temporary KV snapshot file: {e}"
                ),
            }
        }
    }
}

/// This machine's host name — the default snapshot identity.
#[must_use]
pub fn host_name() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: `gethostname` writes at most `buf.len()` bytes into a
        // buffer we own and that outlives the call; it does not retain the
        // pointer. A non-zero return means the buffer is left unspecified, in
        // which case we fall through to the environment.
        #[allow(unsafe_code)]
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            if let Ok(name) = std::str::from_utf8(&buf[..end])
                && !name.is_empty()
            {
                return name.to_string();
            }
        }
    }
    for var in ["HOSTNAME", "COMPUTERNAME"] {
        if let Ok(name) = std::env::var(var)
            && !name.trim().is_empty()
        {
            return name.trim().to_string();
        }
    }
    "ephpm".to_string()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn config_with(path: &Path, extra: &str) -> Config {
        let toml = format!(
            "[kv]\nsecret = \"s3cret\"\n[kv.snapshot]\nmode = \"local\"\npath = {:?}\n{extra}",
            path.to_string_lossy()
        );
        toml::from_str(&toml).expect("test config loads")
    }

    #[tokio::test]
    async fn off_means_no_service() {
        let config: Config = toml::from_str("").unwrap();
        let store = Store::new(ephpm_kv::store::StoreConfig::default());
        assert!(KvSnapshotService::from_config(&config, store, None, None).unwrap().is_none());
    }

    #[tokio::test]
    async fn save_then_load_round_trips_through_the_service_and_cleans_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with(dir.path(), "identity = \"node-a\"\n");
        let store = Store::new(ephpm_kv::store::StoreConfig::default());
        store.set("warm".into(), b"yes".to_vec(), Some(Duration::from_secs(300)));
        // A leftover from a crashed writer: must be removed on construction.
        let stale = dir.path().join(".node-a.kvsnap.tmp-999-1");
        std::fs::write(&stale, b"junk").unwrap();

        let svc =
            KvSnapshotService::from_config(&config, Arc::clone(&store), None, Some("gossip-id"))
                .unwrap()
                .expect("enabled");
        assert!(!stale.exists(), "stale temp file removed");
        assert_eq!(svc.file(), dir.path().join("node-a.kvsnap"));
        let report = svc.save(SaveTrigger::Request).await.unwrap();
        assert_eq!(report.keys(), 1);

        let fresh = Store::new(ephpm_kv::store::StoreConfig::default());
        let svc2 = KvSnapshotService::from_config(&config, Arc::clone(&fresh), None, None)
            .unwrap()
            .unwrap();
        let load = svc2.load_at_startup().await.unwrap();
        assert_eq!(load.outcome, LoadOutcome::Ok, "{load:?}");
        assert_eq!(fresh.get("warm").as_deref(), Some(&b"yes"[..]));
        let status: serde_json::Value = serde_json::from_str(&svc2.status_json()).unwrap();
        assert_eq!(status["last_load"]["outcome"], "ok");
        assert_eq!(status["last_load"]["written_by"], "gossip-id");
        assert!(status["file_bytes"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn load_on_start_false_skips_the_load() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with(dir.path(), "load_on_start = false\n");
        let store = Store::new(ephpm_kv::store::StoreConfig::default());
        let svc = KvSnapshotService::from_config(&config, store, None, None).unwrap().unwrap();
        assert!(svc.load_at_startup().await.is_none());
    }

    #[tokio::test]
    async fn multi_tenant_sites_round_trip_into_their_own_stores() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with(dir.path(), "identity = \"mt\"\n");
        let default = Store::new(ephpm_kv::store::StoreConfig::default());
        let sites =
            MultiTenantStore::new(Arc::clone(&default), ephpm_kv::store::StoreConfig::default());
        sites.get_site_store("alice.test").set("k".into(), b"alice".to_vec(), None);
        sites.get_site_store("bob.test").set("k".into(), b"bob".to_vec(), None);
        default.set("k".into(), b"default".to_vec(), None);
        let svc = KvSnapshotService::from_config(
            &config,
            Arc::clone(&default),
            Some(sites.clone()),
            None,
        )
        .unwrap()
        .unwrap();
        let report = svc.save(SaveTrigger::Shutdown).await.unwrap();
        assert_eq!(report.sites, 3);

        let default2 = Store::new(ephpm_kv::store::StoreConfig::default());
        let sites2 =
            MultiTenantStore::new(Arc::clone(&default2), ephpm_kv::store::StoreConfig::default());
        let svc2 = KvSnapshotService::from_config(
            &config,
            Arc::clone(&default2),
            Some(sites2.clone()),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(svc2.load_now().await.outcome, LoadOutcome::Ok);
        assert_eq!(default2.get("k").as_deref(), Some(&b"default"[..]));
        assert_eq!(sites2.get_site_store("alice.test").get("k").as_deref(), Some(&b"alice"[..]));
        assert_eq!(sites2.get_site_store("bob.test").get("k").as_deref(), Some(&b"bob"[..]));
        assert_eq!(sites2.site_count(), 2);
    }

    #[test]
    fn host_name_is_never_empty() {
        assert_ne!(host_name(), "");
    }
}
