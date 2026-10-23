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
//! Two targets. **Local** (`mode = "local"`): the file at
//! `<path>/<identity>.kvsnap`. **Object** (`mode = "object"`): the same file
//! is written to a staging directory, uploaded as one object
//! (`s3://bucket/<prefix>/<identity>.kvsnap`, see [`crate::s3`]) and removed
//! locally; a load downloads the object to the staging file, loads it and
//! removes it. The object is the only source of truth in that mode — a pod
//! with an empty disk warms up from it, and a failed download is a cold
//! start, never a fallback to a stale local copy.
//!
//! Every write and load runs on a blocking thread (`spawn_blocking`): the
//! snapshot code is synchronous file + CPU work and must never hold a tokio
//! worker, for the same reason the engine work in `db_shutdown` is kept off
//! them (#621). The upload and download are async reqwest streams.
//!
//! A snapshot is a cache warm-up, not durability; see `ephpm_kv::snapshot`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context as _;
use ephpm_config::Config;
use ephpm_kv::multi_tenant::MultiTenantStore;
use ephpm_kv::snapshot::{
    ColdReason, LoadOptions, LoadOutcome, LoadReport, SaveReport, SnapshotError, ValueFilter,
    WriteOptions, load_snapshot, write_snapshot,
};
use ephpm_kv::store::Store;
use metrics::{counter, gauge, histogram};

use crate::s3::{S3Client, S3Location};
use crate::sigv4::Credentials;

/// File extension of a snapshot.
pub const SNAPSHOT_EXTENSION: &str = "kvsnap";

/// Last-resort bound on the shutdown save. Generous: a 1 GiB snapshot is a
/// few seconds of work plus, in object mode, the upload; this only exists so
/// a stuck disk or network cannot keep a stopped server alive forever. Past
/// it the process exits and the next start removes the abandoned temporary
/// file.
pub const SHUTDOWN_SAVE_BOUND: Duration = Duration::from_secs(5 * 60);

/// Metric: snapshots written, by outcome and trigger.
pub const METRIC_SAVE_TOTAL: &str = "ephpm_kv_snapshot_save_total";
/// Metric: wall time of a snapshot write (upload included in object mode).
pub const METRIC_SAVE_DURATION: &str = "ephpm_kv_snapshot_save_duration_seconds";
/// Metric: bytes of the last successful snapshot.
pub const METRIC_LAST_SAVE_BYTES: &str = "ephpm_kv_snapshot_last_save_bytes";
/// Metric: keys in the last successful snapshot.
pub const METRIC_LAST_SAVE_KEYS: &str = "ephpm_kv_snapshot_last_save_keys";
/// Metric: snapshot loads, by outcome and reason.
pub const METRIC_LOAD_TOTAL: &str = "ephpm_kv_snapshot_load_total";
/// Metric: wall time of a snapshot load (download included in object mode).
pub const METRIC_LOAD_DURATION: &str = "ephpm_kv_snapshot_load_duration_seconds";
/// Metric: keys restored by the last load.
pub const METRIC_LAST_LOAD_KEYS: &str = "ephpm_kv_snapshot_last_load_keys";
/// Metric: large KV values handed to peers (`direction="out"`, one per
/// replica target) or pulled from them (`direction="in"`).
pub const METRIC_HANDOFF_KEYS: &str = "ephpm_kv_handoff_keys_total";
/// Metric: value bytes handed off, by direction.
pub const METRIC_HANDOFF_BYTES: &str = "ephpm_kv_handoff_bytes_total";
/// Metric: wall time of a push (`out`) or pull (`in`).
pub const METRIC_HANDOFF_DURATION: &str = "ephpm_kv_handoff_duration_seconds";

/// Record a peer handoff's outcome (metrics + one log line).
pub fn record_handoff(direction: &'static str, report: &ephpm_cluster::HandoffReport) {
    counter!(METRIC_HANDOFF_KEYS, "direction" => direction).increment(report.keys);
    counter!(METRIC_HANDOFF_BYTES, "direction" => direction).increment(report.bytes);
    histogram!(METRIC_HANDOFF_DURATION, "direction" => direction)
        .record(report.elapsed.as_secs_f64());
    let what = if direction == "in" { "KV warm-up pull from peers" } else { "KV handoff to peers" };
    if report.timed_out || report.failed_peers > 0 || report.failed_writes > 0 {
        tracing::warn!(
            keys = report.keys,
            bytes = report.bytes,
            peers = report.peers,
            failed_peers = report.failed_peers,
            failed_writes = report.failed_writes,
            timed_out = report.timed_out,
            elapsed_ms = report.elapsed.as_millis() as u64,
            "{what} finished with problems"
        );
    } else {
        tracing::info!(
            keys = report.keys,
            bytes = report.bytes,
            peers = report.peers,
            elapsed_ms = report.elapsed.as_millis() as u64,
            "{what} done"
        );
    }
}

/// `[kv.snapshot] load_timeout` as a duration (the default when it does
/// not parse — `Config::validate` has already refused that case).
#[must_use]
pub fn load_timeout_from(config: &Config) -> Duration {
    config.kv.snapshot.load_timeout().unwrap_or(Duration::from_secs(30))
}

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

/// Where the snapshot lives.
enum Target {
    /// `<path>/<identity>.kvsnap` is the snapshot.
    Local,
    /// The object is the snapshot; the local file is a staging copy that
    /// exists only during a save or a load.
    Object { client: Arc<S3Client>, key: String },
}

/// The running server's snapshot lifecycle. One per process, built by
/// [`KvSnapshotService::from_config`] when `[kv.snapshot] mode != "off"`.
pub struct KvSnapshotService {
    file: PathBuf,
    target: Target,
    secret: Vec<u8>,
    node_id: String,
    store: Arc<Store>,
    sites: Option<MultiTenantStore>,
    max_size: u64,
    load_timeout: Duration,
    interval: Option<Duration>,
    on_shutdown: bool,
    load_on_start: bool,
    /// Clustered node with peer handoff: the startup load is deferred to
    /// [`load_after_pull`](Self::load_after_pull), which restores the snapshot
    /// only when no alive peer was found.
    defer_to_peers: bool,
    /// Object mode with no `path`: the private staging directory this
    /// process created, removed when the service is dropped.
    _staging: Option<PrivateStagingDir>,
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
            .field("object", &self.object_url())
            .field("node_id", &self.node_id)
            .field("multi_tenant", &self.sites.is_some())
            .field("max_size", &self.max_size)
            .field("load_timeout", &self.load_timeout)
            .field("interval", &self.interval)
            .field("on_shutdown", &self.on_shutdown)
            .field("load_on_start", &self.load_on_start)
            .field("defer_to_peers", &self.defer_to_peers)
            .finish_non_exhaustive()
    }
}

/// The object client `[kv.snapshot]` describes, with the key prefix
/// (`""` or `"a/b"`, no trailing slash). Shared with the CLI
/// (`ephpm kv snapshot list|rm`).
///
/// # Errors
///
/// Not object mode, an unparsable `url`, or missing `AWS_ACCESS_KEY_ID` /
/// `AWS_SECRET_ACCESS_KEY`.
pub fn object_client_from_config(config: &Config) -> anyhow::Result<(S3Client, String)> {
    let snap = &config.kv.snapshot;
    anyhow::ensure!(snap.is_object(), "[kv.snapshot] mode is not \"object\"");
    let url = snap.url.as_deref().context("[kv.snapshot] url is required in object mode")?;
    let (bucket, prefix) = S3Location::parse_url(url.trim()).context("[kv.snapshot] url")?;
    Ok((object_client_for_bucket(config, bucket)?, prefix))
}

/// An object client for `bucket` using `[kv.snapshot]`'s endpoint, region
/// and addressing style — for the CLI's `inspect`/`verify` of an arbitrary
/// `s3://` URL.
///
/// # Errors
///
/// Missing `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`.
pub fn object_client_for_bucket(config: &Config, bucket: String) -> anyhow::Result<S3Client> {
    let snap = &config.kv.snapshot;
    let creds = Credentials::from_env().context(
        "[kv.snapshot] mode = \"object\" needs credentials in the environment: set \
         AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY (and AWS_SESSION_TOKEN for temporary \
         credentials). They are read from the environment only — never from ephpm.toml.",
    )?;
    let location = S3Location {
        bucket,
        endpoint: snap
            .endpoint
            .clone()
            .unwrap_or_else(|| S3Location::aws_endpoint(snap.region.trim())),
        region: snap.region.trim().to_string(),
        path_style: snap.effective_path_style(),
    };
    S3Client::new(location, creds)
}

/// The object key of `identity`'s snapshot under `prefix`.
#[must_use]
pub fn object_key(prefix: &str, identity: &str) -> String {
    if prefix.is_empty() {
        format!("{identity}.{SNAPSHOT_EXTENSION}")
    } else {
        format!("{prefix}/{identity}.{SNAPSHOT_EXTENSION}")
    }
}

/// The identity `[kv.snapshot]` resolves to: `identity`, else
/// `[cluster] node_id`, else the host name.
#[must_use]
pub fn resolve_identity(config: &Config) -> String {
    config
        .kv
        .snapshot
        .identity
        .clone()
        .or_else(|| Some(config.cluster.node_id.clone()).filter(|s| !s.is_empty()))
        .unwrap_or_else(host_name)
}

impl KvSnapshotService {
    /// Build the service from `[kv.snapshot]`, or `None` when snapshots are
    /// off. Creates the snapshot (or staging) directory, removes temporary
    /// files an earlier process left behind, and in object mode checks the
    /// credentials are present.
    ///
    /// `node_id` is the effective cluster node id when clustered; it is only
    /// used for the header.
    ///
    /// # Errors
    ///
    /// A directory that cannot be created, missing object credentials, or
    /// an unparsable knob (which `Config::validate` normally catches first).
    pub fn from_config(
        config: &Config,
        store: Arc<Store>,
        sites: Option<MultiTenantStore>,
        node_id: Option<&str>,
        defer_to_peers: bool,
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
        let identity = resolve_identity(config);
        let (dir, target, staging) = if snap.is_object() {
            let (client, prefix) = object_client_from_config(config)?;
            let target =
                Target::Object { client: Arc::new(client), key: object_key(&prefix, &identity) };
            match snap.path.as_deref() {
                Some(path) => (PathBuf::from(path), target, None),
                // No `path`: stage in a fresh private directory, never a
                // predictable shared one. A fixed `$TMPDIR/ephpm-kv-snapshot`
                // could be pre-created (or symlinked) by another local user
                // before this process — still root, ahead of the privilege
                // drop — downloaded into it.
                None => {
                    let staging = PrivateStagingDir::create(&std::env::temp_dir())
                        .context("creating the [kv.snapshot] staging directory")?;
                    (staging.path().to_path_buf(), target, Some(staging))
                }
            }
        } else {
            let dir = PathBuf::from(
                snap.path.as_deref().context("[kv.snapshot] mode = \"local\" requires path")?,
            );
            (dir, Target::Local, None)
        };
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating [kv.snapshot] directory {}", dir.display()))?;
        let file = dir.join(format!("{identity}.{SNAPSHOT_EXTENSION}"));
        remove_stale_temp_files(&dir, &identity);
        let node_id = node_id.map_or_else(|| identity.clone(), str::to_string);
        if config.server.diagnostics.token.as_deref().is_none_or(|t| t.trim().is_empty()) {
            tracing::info!(
                "[server.diagnostics] token is unset: POST /_ephpm/kv/snapshot and \
                 /_ephpm/kv/snapshot/load (`ephpm kv snapshot save|load`) are refused until it \
                 is set (or the controls are reached over an mTLS [server.metrics] listen); \
                 periodic and shutdown saves are unaffected"
            );
        }
        match &target {
            Target::Local => tracing::info!(
                file = %file.display(),
                identity,
                on_shutdown = snap.on_shutdown,
                load_on_start = snap.load_on_start,
                interval = %snap.interval,
                "KV warm-restart snapshots enabled (local file)"
            ),
            Target::Object { client, key } => tracing::info!(
                object = %format!("s3://{}/{key}", client.location().bucket),
                endpoint = %client.location().endpoint,
                staging = %file.display(),
                identity,
                on_shutdown = snap.on_shutdown,
                load_on_start = snap.load_on_start,
                interval = %snap.interval,
                "KV warm-restart snapshots enabled (object storage)"
            ),
        }
        Ok(Some(Arc::new(Self {
            file,
            target,
            secret,
            node_id,
            store,
            sites,
            max_size: snap.max_size_bytes()?,
            load_timeout: snap.load_timeout()?,
            interval: snap.interval()?,
            on_shutdown: snap.on_shutdown,
            load_on_start: snap.load_on_start,
            defer_to_peers,
            _staging: staging,
            saving: tokio::sync::Mutex::new(()),
            last_save: std::sync::Mutex::new(None),
            last_load: std::sync::Mutex::new(None),
        })))
    }

    /// The snapshot file this node writes and loads (the staging file in
    /// object mode).
    #[must_use]
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// The paths this service keeps writing to after startup — the snapshot
    /// (or staging) directory and, when it exists, the snapshot file — for
    /// the privilege drop to hand to the serving user. Not recursive: the
    /// directory may be an operator-chosen path that holds other files.
    #[must_use]
    pub fn runtime_paths(&self) -> Vec<PathBuf> {
        let mut out = Vec::with_capacity(2);
        if let Some(dir) = self.file.parent() {
            out.push(dir.to_path_buf());
        }
        if self.file.exists() {
            out.push(self.file.clone());
        }
        out
    }

    /// `s3://bucket/key` in object mode.
    #[must_use]
    pub fn object_url(&self) -> Option<String> {
        match &self.target {
            Target::Local => None,
            Target::Object { client, key } => {
                Some(format!("s3://{}/{key}", client.location().bucket))
            }
        }
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
    ///
    /// On a clustered node with peer handoff (`defer_to_peers`) this does
    /// **nothing**: the decision whether to restore anything from the
    /// snapshot is taken after the peer pull, by
    /// [`load_after_pull`](Self::load_after_pull). The reasoning, stated
    /// plainly: a cache that serves a value another node rewrote while this
    /// one was down is worse than a miss — sessions, lock tokens and
    /// invalidations can all go wrong on it — and a restarted node gets the
    /// gossip tier back from its peers over the following minutes, during
    /// which a snapshot-restored small value would be exactly that stale
    /// value (0.6 % of the restarted node's reads in the lab). Large values
    /// come from the pull, stamped and current. So with peers present the
    /// snapshot is not used at all: misses until gossip has caught up, never
    /// stale.
    pub async fn load_at_startup(&self) -> Option<LoadReport> {
        if !self.load_on_start {
            return None;
        }
        if self.defer_to_peers {
            tracing::info!(
                "KV snapshot load deferred until peer discovery: with alive peers present a \
                 clustered node takes small values from gossip and large values from its peers; \
                 the snapshot is restored only when no peer is found"
            );
            return None;
        }
        Some(self.load_with(ValueFilter::All).await)
    }

    /// The deferred half of a clustered startup, after the peer pull has
    /// run: restore the whole snapshot only when the pull found **no** alive
    /// peer (this node is alone — a whole-cluster cold start, or the first
    /// node up). With peers present nothing is restored, and the log says so
    /// (`restored_small_tier=false reason=peers_present`).
    ///
    /// Two consequences worth knowing: a whole-cluster restart where every
    /// node comes up at once is cold (each sees the others within a gossip
    /// round), and a value restored by a lone node lives in that node's
    /// store only — it is not re-published into gossip, so peers that join
    /// later do not receive it.
    pub async fn load_after_pull(&self, alive_peers: usize) -> Option<LoadReport> {
        if !self.defer_to_peers || !self.load_on_start {
            return None;
        }
        if alive_peers > 0 {
            tracing::info!(
                restored_small_tier = false,
                restored_large_tier = false,
                reason = "peers_present",
                alive_peers,
                "KV snapshot not restored: small values arrive through gossip, large values \
                 came from the peer pull (a restored copy could be stale)"
            );
            return None;
        }
        tracing::info!(
            restored_small_tier = true,
            restored_large_tier = true,
            reason = "no_peers",
            "no alive peer found within the join wait; restoring the KV snapshot"
        );
        Some(self.load_with(ValueFilter::All).await)
    }

    /// Load the snapshot into the live stores now, under `load_timeout`
    /// (every entry). In object mode the object is downloaded first (within
    /// the same budget).
    pub async fn load_now(&self) -> LoadReport {
        self.load_with(ValueFilter::All).await
    }

    async fn load_with(&self, filter: ValueFilter) -> LoadReport {
        let started = Instant::now();
        let deadline = started + self.load_timeout;
        let report = match &self.target {
            Target::Local => self.load_file(deadline, filter).await,
            Target::Object { client, key } => match self.download(client, key, deadline).await {
                Ok(true) => {
                    let report = self.load_file(deadline, filter).await;
                    // The object is the snapshot; the downloaded copy has
                    // done its job.
                    let _ = std::fs::remove_file(&self.file);
                    report
                }
                Ok(false) => {
                    tracing::info!(
                        object = %self.object_url().unwrap_or_default(),
                        "no KV snapshot object to load (cold start)"
                    );
                    LoadReport::cold(ColdReason::Missing, None, 0, started.elapsed())
                }
                Err(e) => {
                    tracing::warn!(
                        object = %self.object_url().unwrap_or_default(),
                        reason = ColdReason::Io.label(),
                        "KV snapshot not loaded (cold start): {e:#}"
                    );
                    LoadReport::cold(ColdReason::Io, None, 0, started.elapsed())
                }
            },
        };
        let report = LoadReport { elapsed: started.elapsed(), ..report };
        self.record_load(&report);
        report
    }

    /// Download the object to the staging file. `Ok(false)` = no object.
    async fn download(
        &self,
        client: &S3Client,
        key: &str,
        deadline: Instant,
    ) -> anyhow::Result<bool> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, client.get_to_file(key, &self.file)).await {
            Ok(Ok(Some(bytes))) => {
                tracing::debug!(bytes, key, "KV snapshot object downloaded");
                Ok(true)
            }
            Ok(Ok(None)) => Ok(false),
            Ok(Err(e)) => Err(e),
            Err(_) => anyhow::bail!("download did not finish within load_timeout"),
        }
    }

    async fn load_file(&self, deadline: Instant, filter: ValueFilter) -> LoadReport {
        let file = self.file.clone();
        let secret = self.secret.clone();
        let store = Arc::clone(&self.store);
        let sites = self.sites.clone();
        let opts = LoadOptions {
            deadline: Some(deadline),
            max_size: self.max_size,
            workers: 0,
            value_filter: filter,
        };
        tokio::task::spawn_blocking(move || {
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
            LoadReport::cold(ColdReason::Io, None, 0, Duration::ZERO)
        })
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
        let source = self.object_url().unwrap_or_else(|| self.file.display().to_string());
        match report.outcome {
            LoadOutcome::Ok => tracing::info!(
                source,
                keys = report.restored,
                expired_skipped = report.expired,
                blocks = report.blocks,
                bytes = report.bytes,
                elapsed_ms = report.elapsed.as_millis() as u64,
                written_by = report.header.as_ref().map_or("", |h| h.node_id.as_str()),
                "KV snapshot loaded — serving warm"
            ),
            LoadOutcome::Partial(reason) => tracing::warn!(
                source,
                reason = reason.label(),
                keys = report.restored,
                refused = report.refused,
                unknown_site = report.unknown_site,
                bad_blocks = report.bad_blocks,
                unreached_blocks = report.unreached_blocks,
                elapsed_ms = report.elapsed.as_millis() as u64,
                "KV snapshot partially loaded"
            ),
            // The cold reasons are logged where they are detected (missing
            // at INFO, everything else at WARN) with the underlying error.
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
    /// The write (or, in object mode, the upload) failed; the previous
    /// snapshot, if any, is intact.
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
        let started = Instant::now();
        let file = self.file.clone();
        let secret = self.secret.clone();
        let stores = self.stores();
        let opts = WriteOptions {
            node_id: self.node_id.clone(),
            max_size: self.max_size,
            multi_tenant: self.sites.is_some(),
            ..WriteOptions::default()
        };
        let mut result =
            tokio::task::spawn_blocking(move || write_snapshot(&file, &secret, &stores, &opts))
                .await
                .unwrap_or_else(|e| {
                    Err(SnapshotError::Io(std::io::Error::other(format!("save task failed: {e}"))))
                });
        if let (Ok(_), Target::Object { client, key }) = (&result, &self.target) {
            if let Err(e) = client.put_file(key, &self.file).await {
                result = Err(SnapshotError::Io(std::io::Error::other(format!("upload: {e:#}"))));
            }
            // The object is the snapshot; the staging copy has done its job
            // either way (on failure it is a snapshot nobody will load).
            let _ = std::fs::remove_file(&self.file);
        }
        if let Ok(report) = &mut result {
            report.elapsed = started.elapsed();
        }
        let target = self.object_url().unwrap_or_else(|| self.file.display().to_string());
        match &result {
            Ok(report) => {
                counter!(METRIC_SAVE_TOTAL, "outcome" => "ok", "trigger" => trigger.label())
                    .increment(1);
                histogram!(METRIC_SAVE_DURATION).record(report.elapsed.as_secs_f64());
                gauge!(METRIC_LAST_SAVE_BYTES).set(report.bytes as f64);
                gauge!(METRIC_LAST_SAVE_KEYS).set(report.keys() as f64);
                tracing::info!(
                    target,
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
                    target,
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
        let file_bytes = match self.target {
            Target::Local => std::fs::metadata(&self.file).ok().map(|m| m.len()),
            Target::Object { .. } => None,
        };
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
        let (mode, endpoint) = match &self.target {
            Target::Local => ("local", None),
            Target::Object { client, .. } => ("object", Some(client.location().endpoint.clone())),
        };
        serde_json::json!({
            "mode": mode,
            "file": self.file.display().to_string(),
            "file_bytes": file_bytes,
            "object": self.object_url(),
            "endpoint": endpoint,
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
/// A staging directory this process created for itself: a fresh,
/// unpredictable name under the system temp directory, made with `mkdir`
/// (which fails rather than following anything already at that path) and
/// mode `0700` on Unix. Removed on drop.
///
/// Object mode only needs the staging file for the duration of one transfer,
/// and the object is authoritative, so nothing is lost by not reusing a
/// previous process's directory. A process killed without dropping the
/// service leaves its directory behind (ciphertext only); set
/// `[kv.snapshot] path` for a stable location instead.
#[derive(Debug)]
struct PrivateStagingDir {
    path: PathBuf,
}

impl PrivateStagingDir {
    /// Create `<base>/ephpm-kv-snapshot-<pid>-<random>`.
    fn create(base: &Path) -> std::io::Result<Self> {
        let pid = std::process::id();
        let mut last_err = None;
        for _ in 0..16 {
            let mut raw = [0u8; 8];
            getrandom::fill(&mut raw).map_err(|e| std::io::Error::other(e.to_string()))?;
            let nonce = u64::from_le_bytes(raw);
            let path = base.join(format!("ephpm-kv-snapshot-{pid}-{nonce:016x}"));
            #[cfg(unix)]
            let builder = {
                use std::os::unix::fs::DirBuilderExt as _;
                let mut builder = std::fs::DirBuilder::new();
                builder.mode(0o700);
                builder
            };
            // Windows: the system temp dir is already per-user.
            #[cfg(not(unix))]
            let builder = std::fs::DirBuilder::new();
            match builder.create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last_err = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last_err.unwrap_or_else(|| std::io::Error::other("no staging directory name free")))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PrivateStagingDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::debug!(dir = %self.path.display(), "could not remove KV snapshot staging dir: {e}");
        }
    }
}

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
        assert!(
            KvSnapshotService::from_config(&config, store, None, None, false).unwrap().is_none()
        );
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

        let svc = KvSnapshotService::from_config(
            &config,
            Arc::clone(&store),
            None,
            Some("gossip-id"),
            false,
        )
        .unwrap()
        .expect("enabled");
        assert!(!stale.exists(), "stale temp file removed");
        assert_eq!(svc.file(), dir.path().join("node-a.kvsnap"));
        assert_eq!(svc.object_url(), None);
        let report = svc.save(SaveTrigger::Request).await.unwrap();
        assert_eq!(report.keys(), 1);

        let fresh = Store::new(ephpm_kv::store::StoreConfig::default());
        let svc2 = KvSnapshotService::from_config(&config, Arc::clone(&fresh), None, None, false)
            .unwrap()
            .unwrap();
        let load = svc2.load_at_startup().await.unwrap();
        assert_eq!(load.outcome, LoadOutcome::Ok, "{load:?}");
        assert_eq!(fresh.get("warm").as_deref(), Some(&b"yes"[..]));
        let status: serde_json::Value = serde_json::from_str(&svc2.status_json()).unwrap();
        assert_eq!(status["mode"], "local");
        assert_eq!(status["last_load"]["outcome"], "ok");
        assert_eq!(status["last_load"]["written_by"], "gossip-id");
        assert!(status["file_bytes"].as_u64().unwrap() > 0);
    }

    /// The object-mode staging directory (no `path`) is fresh, private and
    /// removed on drop — never a predictable shared path another local user
    /// could pre-create or symlink before startup.
    #[test]
    fn private_staging_dir_is_fresh_private_and_removed_on_drop() {
        let base = tempfile::tempdir().unwrap();
        let a = PrivateStagingDir::create(base.path()).unwrap();
        let b = PrivateStagingDir::create(base.path()).unwrap();
        assert_ne!(a.path(), b.path(), "unpredictable, per-call names");
        let meta = std::fs::symlink_metadata(a.path()).unwrap();
        assert!(meta.is_dir() && !meta.file_type().is_symlink());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        }
        std::fs::write(a.path().join("x.kvsnap"), b"sealed").unwrap();
        let path = a.path().to_path_buf();
        drop(a);
        assert!(!path.exists(), "removed with its contents on drop");
        assert!(b.path().exists());
    }

    #[tokio::test]
    async fn load_on_start_false_skips_the_load() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with(dir.path(), "load_on_start = false\n");
        let store = Store::new(ephpm_kv::store::StoreConfig::default());
        let svc =
            KvSnapshotService::from_config(&config, store, None, None, false).unwrap().unwrap();
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
            false,
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
            false,
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
    fn object_keys_and_identity_resolution() {
        assert_eq!(object_key("", "web-1"), "web-1.kvsnap");
        assert_eq!(object_key("prod/kv", "web-1"), "prod/kv/web-1.kvsnap");
        let cfg: Config =
            toml::from_str("[cluster]\nnode_id = \"n-7\"\n[kv.snapshot]\nidentity = \"x\"\n")
                .unwrap();
        assert_eq!(resolve_identity(&cfg), "x");
        let cfg: Config = toml::from_str("[cluster]\nnode_id = \"n-7\"\n").unwrap();
        assert_eq!(resolve_identity(&cfg), "n-7");
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(resolve_identity(&cfg), host_name());
    }

    #[test]
    fn host_name_is_never_empty() {
        assert_ne!(host_name(), "");
    }
}
