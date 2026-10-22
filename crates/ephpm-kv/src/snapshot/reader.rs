//! Snapshot loading, inspection and verification.
//!
//! A load opens the file in a fixed order — header, trailer, index — and
//! stops cold at the first thing that is wrong: a bad magic or CRC, a format
//! version this binary does not read, a key check that fails (the secret has
//! changed), a missing trailer (the file was truncated). Nothing is applied to
//! any store before all three have verified.
//!
//! Data blocks are then applied in parallel: each worker takes the next block
//! from the index, reads it, opens it, and restores its entries into the
//! store the block's site resolves to. A block that fails to open or decode
//! is **skipped and counted** (every block is independently authenticated,
//! so a bad block cannot affect another); the load reports `Partial`. A
//! deadline is checked between blocks and every few hundred entries; past it
//! the load stops with whatever has been restored so far and reports
//! `Partial` — it never blocks readiness.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use super::crypto::FileCipher;
use super::format::{
    BLOCK_KIND_DATA, BLOCK_KIND_INDEX, BLOCK_PREFIX_LEN, Cursor, DataBlockHead, DecodedHeader,
    EntryRecord, FormatError, HEADER_FIXED_LEN, Header, IndexEntry, TRAILER_LEN, Trailer,
    decode_entry, decode_index, unpack_block, value_compression_algo,
};
use super::{SnapshotError, unix_ms_for};
use crate::store::{HashFieldRecord, Store};

/// Largest plaintext payload a single block may decompress to. A block is
/// written at ~1 MiB, but one entry can be larger than that; this only guards
/// against a crafted length field, not real data.
const MAX_BLOCK_RAW: usize = 1 << 31;

/// How often a worker checks the deadline while restoring a block's entries.
const DEADLINE_CHECK_EVERY: u32 = 256;

/// How a snapshot is loaded.
#[derive(Debug, Clone)]
pub struct LoadOptions {
    /// Stop restoring once this instant passes.
    pub deadline: Option<Instant>,
    /// Refuse a file larger than this (bytes).
    pub max_size: u64,
    /// Worker threads. `0` = one per available core, at most 8.
    pub workers: usize,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self { deadline: None, max_size: 1 << 30, workers: 0 }
    }
}

/// Why a load restored nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColdReason {
    /// No snapshot at the path.
    Missing,
    /// The file is larger than `max_size`.
    TooLarge,
    /// The format version is not one this binary reads.
    Version,
    /// Header, trailer or index did not verify — truncated, damaged, or not a
    /// snapshot.
    Corrupt,
    /// Sealed with a different `[kv] secret`.
    WrongKey,
    /// Could not be read (I/O error).
    Io,
}

impl ColdReason {
    /// A stable lowercase label (metrics, logs, CLI).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::TooLarge => "too_large",
            Self::Version => "version",
            Self::Corrupt => "corrupt",
            Self::WrongKey => "wrong_key",
            Self::Io => "io",
        }
    }
}

/// Why a load stopped before restoring everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartialReason {
    /// The deadline passed.
    Timeout,
    /// One or more blocks failed to open or decode.
    CorruptBlocks,
    /// A store refused entries (memory limit under `noeviction`).
    MemoryLimit,
    /// Blocks named a site this node has no store for.
    UnknownSite,
}

impl PartialReason {
    /// A stable lowercase label (metrics, logs, CLI).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::CorruptBlocks => "corrupt_blocks",
            Self::MemoryLimit => "memory_limit",
            Self::UnknownSite => "unknown_site",
        }
    }
}

/// The outcome of a load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadOutcome {
    /// Every block was restored.
    Ok,
    /// Some entries were restored; see the reason for what was not.
    Partial(PartialReason),
    /// Nothing was restored.
    Cold(ColdReason),
}

impl LoadOutcome {
    /// `"ok"`, `"partial"` or `"cold"`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Partial(_) => "partial",
            Self::Cold(_) => "cold",
        }
    }

    /// The reason label, or `"none"` for [`LoadOutcome::Ok`].
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::Ok => "none",
            Self::Partial(r) => r.label(),
            Self::Cold(r) => r.label(),
        }
    }
}

/// What a load did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadReport {
    /// Outcome.
    pub outcome: LoadOutcome,
    /// Entries (strings + hashes) restored.
    pub restored: u64,
    /// Entries skipped because their expiry had passed.
    pub expired: u64,
    /// Entries a store refused (memory limit).
    pub refused: u64,
    /// Entries in blocks whose site resolved to no store.
    pub unknown_site: u64,
    /// Data blocks in the file.
    pub blocks: usize,
    /// Data blocks that failed to open or decode.
    pub bad_blocks: usize,
    /// Data blocks not reached before the deadline.
    pub unreached_blocks: usize,
    /// Bytes of the file.
    pub bytes: u64,
    /// The header, when it was readable.
    pub header: Option<Header>,
    /// Wall time of the load.
    pub elapsed: Duration,
}

impl LoadReport {
    /// A report for a load that restored nothing.
    #[must_use]
    pub fn cold(reason: ColdReason, header: Option<Header>, bytes: u64, elapsed: Duration) -> Self {
        Self {
            outcome: LoadOutcome::Cold(reason),
            restored: 0,
            expired: 0,
            refused: 0,
            unknown_site: 0,
            blocks: 0,
            bad_blocks: 0,
            unreached_blocks: 0,
            bytes,
            header,
            elapsed,
        }
    }
}

/// Maps a site key (`""` = default store) to the store its entries belong in.
/// `None` means this node has no such store: the block is skipped.
pub type SiteResolver<'a> = &'a (dyn Fn(&str) -> Option<Arc<Store>> + Sync);

/// A snapshot file whose header, trailer and index have verified.
struct Opened {
    file: File,
    size: u64,
    header: DecodedHeader,
    cipher: FileCipher,
    index: Vec<IndexEntry>,
}

/// How an open failed, mapped to a [`ColdReason`] plus the header if it was
/// readable (so a report can still say what the file claims to be).
struct OpenFailure {
    reason: ColdReason,
    error: SnapshotError,
    header: Option<Header>,
}

fn open(path: &Path, secret: &[u8], max_size: u64) -> Result<Opened, OpenFailure> {
    let fail = |reason, error, header| Err(OpenFailure { reason, error, header });
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return fail(ColdReason::Missing, SnapshotError::Io(e), None);
        }
        Err(e) => return fail(ColdReason::Io, SnapshotError::Io(e), None),
    };
    let size = match file.metadata() {
        Ok(m) => m.len(),
        Err(e) => return fail(ColdReason::Io, SnapshotError::Io(e), None),
    };
    if size > max_size {
        return fail(ColdReason::TooLarge, SnapshotError::TooLarge { size, max: max_size }, None);
    }
    if size < (HEADER_FIXED_LEN + TRAILER_LEN) as u64 {
        return fail(
            ColdReason::Corrupt,
            SnapshotError::Corrupt("file is too short to be a snapshot".into()),
            None,
        );
    }

    // Header.
    let mut prefix = [0u8; HEADER_FIXED_LEN];
    if let Err(e) = file.read_exact(&mut prefix) {
        return fail(ColdReason::Io, SnapshotError::Io(e), None);
    }
    let header_len = match Header::declared_len(&prefix) {
        Ok(n) => n,
        Err(FormatError::UnsupportedVersion(v)) => {
            return fail(ColdReason::Version, SnapshotError::UnsupportedVersion(v), None);
        }
        Err(e) => return fail(ColdReason::Corrupt, SnapshotError::Corrupt(e.to_string()), None),
    };
    let mut header_bytes = vec![0u8; header_len];
    header_bytes[..HEADER_FIXED_LEN].copy_from_slice(&prefix);
    if let Err(e) = file.read_exact(&mut header_bytes[HEADER_FIXED_LEN..]) {
        return fail(ColdReason::Io, SnapshotError::Io(e), None);
    }
    let header = match Header::decode(&header_bytes) {
        Ok(h) => h,
        Err(e) => return fail(ColdReason::Corrupt, SnapshotError::Corrupt(e.to_string()), None),
    };
    let cipher = FileCipher::derive(secret, &header.header.salt);
    let prefix_len = header_bytes.len() - 4 - super::format::TAG_LEN;
    if !cipher.verify_key_check(&header_bytes[..prefix_len], &header.key_check) {
        return fail(ColdReason::WrongKey, SnapshotError::WrongKey, Some(header.header));
    }

    // Trailer.
    let mut trailer_bytes = [0u8; TRAILER_LEN];
    if let Err(e) = file
        .seek(SeekFrom::Start(size - TRAILER_LEN as u64))
        .and_then(|_| file.read_exact(&mut trailer_bytes))
    {
        return fail(ColdReason::Io, SnapshotError::Io(e), Some(header.header));
    }
    let trailer = match Trailer::decode(&trailer_bytes) {
        Ok(t) => t,
        Err(e) => {
            return fail(
                ColdReason::Corrupt,
                SnapshotError::Corrupt(e.to_string()),
                Some(header.header),
            );
        }
    };

    // Index block.
    let index_end = trailer.index_offset + BLOCK_PREFIX_LEN as u64 + u64::from(trailer.index_len);
    if trailer.index_offset < header_len as u64 || index_end != size - TRAILER_LEN as u64 {
        return fail(
            ColdReason::Corrupt,
            SnapshotError::Corrupt("index offset does not match the file".into()),
            Some(header.header),
        );
    }
    let index_ct =
        match read_block(&mut file, trailer.index_offset, trailer.index_len, BLOCK_KIND_INDEX) {
            Ok(ct) => ct,
            Err(e) => return fail(ColdReason::Corrupt, e, Some(header.header)),
        };
    let Some(opened) =
        cipher.open_block(&header.bytes, index_seq_for(trailer.index_offset), &index_ct)
    else {
        return fail(
            ColdReason::Corrupt,
            SnapshotError::Corrupt("index block failed to authenticate".into()),
            Some(header.header),
        );
    };
    let index = match unpack_block(&opened, MAX_BLOCK_RAW).and_then(|p| decode_index(&p)) {
        Ok(i) => i,
        Err(e) => {
            return fail(
                ColdReason::Corrupt,
                SnapshotError::Corrupt(format!("index: {e}")),
                Some(header.header),
            );
        }
    };
    // Every index entry must lie inside the data region.
    for e in &index {
        let end = e.offset + BLOCK_PREFIX_LEN as u64 + u64::from(e.len);
        if e.offset < header_len as u64 || end > trailer.index_offset {
            return fail(
                ColdReason::Corrupt,
                SnapshotError::Corrupt("index entry lies outside the data region".into()),
                Some(header.header),
            );
        }
    }
    Ok(Opened { file, size, header, cipher, index })
}

/// The AEAD sequence number of the index block. Data blocks use `0..n`; the
/// index uses a value no data block can (the high bit set), derived from its
/// own file offset so it is bound to its position.
#[must_use]
pub(crate) fn index_seq_for(index_offset: u64) -> u64 {
    index_offset | (1 << 63)
}

fn read_block(file: &mut File, offset: u64, len: u32, kind: u8) -> Result<Vec<u8>, SnapshotError> {
    file.seek(SeekFrom::Start(offset))?;
    let mut prefix = [0u8; BLOCK_PREFIX_LEN];
    file.read_exact(&mut prefix)?;
    if prefix[0] != kind {
        return Err(SnapshotError::Corrupt("block kind does not match the index".into()));
    }
    let declared = u32::from_le_bytes([prefix[1], prefix[2], prefix[3], prefix[4]]);
    if declared != len {
        return Err(SnapshotError::Corrupt("block length does not match the index".into()));
    }
    let mut ct = vec![0u8; len as usize];
    file.read_exact(&mut ct)?;
    Ok(ct)
}

/// Load the snapshot at `path` into the stores `resolve` names.
///
/// Blocking: call from a thread that may block on disk and CPU.
///
/// # Panics
///
/// If the OS refuses to spawn a loader thread.
#[must_use]
pub fn load_snapshot(
    path: &Path,
    secret: &[u8],
    resolve: SiteResolver<'_>,
    opts: &LoadOptions,
) -> LoadReport {
    let started = Instant::now();
    let opened = match open(path, secret, opts.max_size) {
        Ok(o) => o,
        Err(f) => {
            let bytes = std::fs::metadata(path).map_or(0, |m| m.len());
            match f.reason {
                ColdReason::Missing => {
                    tracing::info!(path = %path.display(), "no KV snapshot to load (cold start)");
                }
                reason => tracing::warn!(
                    path = %path.display(),
                    reason = reason.label(),
                    "KV snapshot not loaded (cold start): {}",
                    f.error
                ),
            }
            return LoadReport::cold(f.reason, f.header, bytes, started.elapsed());
        }
    };
    let Opened { file, size, header, cipher, index } = opened;
    drop(file);
    let total_blocks = index.len();
    let next = AtomicUsize::new(0);
    let stats = Stats::default();
    let workers = if opts.workers == 0 {
        std::thread::available_parallelism().map_or(1, |n| n.get().min(8))
    } else {
        opts.workers
    }
    .min(total_blocks.max(1));

    let header_bytes = &header.bytes;
    let cipher = &cipher;
    let index = &index;
    let stats = &stats;
    let next = &next;
    let deadline = opts.deadline;
    std::thread::scope(|scope| {
        for i in 0..workers {
            std::thread::Builder::new()
                .name(format!("kv-snap-load-{i}"))
                .spawn_scoped(scope, move || {
                    let Ok(mut file) = File::open(path) else {
                        stats.io_failed.fetch_add(1, Ordering::Relaxed);
                        return;
                    };
                    loop {
                        if deadline.is_some_and(|d| Instant::now() >= d) {
                            return;
                        }
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(entry) = index.get(i) else { return };
                        match apply_block(
                            &mut file,
                            entry,
                            i as u64,
                            cipher,
                            header_bytes,
                            resolve,
                            deadline,
                            stats,
                        ) {
                            Ok(()) => {}
                            Err(BlockFailure::Corrupt(e)) => {
                                stats.bad_blocks.fetch_add(1, Ordering::Relaxed);
                                tracing::warn!(
                                    block = i,
                                    site = %entry.site,
                                    "KV snapshot block skipped: {e}"
                                );
                            }
                            Err(BlockFailure::Timeout) => return,
                        }
                    }
                })
                .expect("spawn snapshot load thread");
        }
    });

    let reached = next.load(Ordering::Relaxed).min(total_blocks);
    let timed_out = deadline.is_some_and(|d| Instant::now() >= d)
        && (reached < total_blocks || stats.timed_out_mid_block.load(Ordering::Relaxed) > 0);
    let bad_blocks =
        stats.bad_blocks.load(Ordering::Relaxed) + stats.io_failed.load(Ordering::Relaxed);
    let refused = stats.refused.load(Ordering::Relaxed);
    let unknown_site = stats.unknown_site.load(Ordering::Relaxed);
    let outcome = if timed_out {
        LoadOutcome::Partial(PartialReason::Timeout)
    } else if bad_blocks > 0 {
        LoadOutcome::Partial(PartialReason::CorruptBlocks)
    } else if refused > 0 {
        LoadOutcome::Partial(PartialReason::MemoryLimit)
    } else if unknown_site > 0 {
        LoadOutcome::Partial(PartialReason::UnknownSite)
    } else {
        LoadOutcome::Ok
    };
    LoadReport {
        outcome,
        restored: stats.restored.load(Ordering::Relaxed),
        expired: stats.expired.load(Ordering::Relaxed),
        refused,
        unknown_site,
        blocks: total_blocks,
        bad_blocks,
        unreached_blocks: total_blocks - reached,
        bytes: size,
        header: Some(header.header),
        elapsed: started.elapsed(),
    }
}

#[derive(Default)]
struct Stats {
    restored: AtomicU64,
    expired: AtomicU64,
    refused: AtomicU64,
    unknown_site: AtomicU64,
    bad_blocks: AtomicUsize,
    io_failed: AtomicUsize,
    timed_out_mid_block: AtomicUsize,
}

enum BlockFailure {
    Corrupt(SnapshotError),
    Timeout,
}

fn apply_block(
    file: &mut File,
    entry: &IndexEntry,
    seq: u64,
    cipher: &FileCipher,
    header_bytes: &[u8],
    resolve: SiteResolver<'_>,
    deadline: Option<Instant>,
    stats: &Stats,
) -> Result<(), BlockFailure> {
    let ct = read_block(file, entry.offset, entry.len, BLOCK_KIND_DATA)
        .map_err(BlockFailure::Corrupt)?;
    let opened = cipher.open_block(header_bytes, seq, &ct).ok_or_else(|| {
        BlockFailure::Corrupt(SnapshotError::Corrupt("block failed to authenticate".into()))
    })?;
    drop(ct);
    let payload = unpack_block(&opened, MAX_BLOCK_RAW)
        .map_err(|e| BlockFailure::Corrupt(SnapshotError::Corrupt(e.to_string())))?;
    drop(opened);
    let mut c = Cursor::new(&payload);
    let head = DataBlockHead::decode(&mut c)
        .map_err(|e| BlockFailure::Corrupt(SnapshotError::Corrupt(e.to_string())))?;
    if head.site != entry.site {
        return Err(BlockFailure::Corrupt(SnapshotError::Corrupt(
            "block site does not match the index".into(),
        )));
    }
    let Some(store) = resolve(&head.site) else {
        stats.unknown_site.fetch_add(u64::from(head.count), Ordering::Relaxed);
        return Ok(());
    };
    let value_algo = value_compression_algo(head.value_compression)
        .ok_or_else(|| BlockFailure::Corrupt(SnapshotError::Corrupt("value compression".into())))?;
    let same_algo = value_algo == store.config().compression.algo;
    let clock = LoadClock::now();
    for n in 0..head.count {
        if n % DEADLINE_CHECK_EVERY == DEADLINE_CHECK_EVERY - 1
            && deadline.is_some_and(|d| Instant::now() >= d)
        {
            stats.timed_out_mid_block.fetch_add(1, Ordering::Relaxed);
            return Err(BlockFailure::Timeout);
        }
        let Some(rec) = decode_entry(&mut c)
            .map_err(|e| BlockFailure::Corrupt(SnapshotError::Corrupt(e.to_string())))?
        else {
            continue;
        };
        match rec {
            EntryRecord::String { key, data, compressed, expires_at_ms } => {
                let expires_at = match clock.deadline(expires_at_ms) {
                    Ok(d) => d,
                    Err(Expired) => {
                        stats.expired.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                };
                let ok = if compressed && !same_algo {
                    // Taken under a different value compression: decode with
                    // the snapshot's algorithm and let the store re-encode.
                    match crate::store::decompress_value(&data, value_algo) {
                        Some(raw) => {
                            let ttl =
                                expires_at.map(|at| at.saturating_duration_since(Instant::now()));
                            store.set_local(key, raw.to_vec(), ttl)
                        }
                        None => false,
                    }
                } else {
                    store.restore_string(key, data, compressed, expires_at)
                };
                if ok {
                    stats.restored.fetch_add(1, Ordering::Relaxed);
                } else {
                    stats.refused.fetch_add(1, Ordering::Relaxed);
                }
            }
            EntryRecord::Hash { key, fields, expires_at_ms } => {
                let expires_at = match clock.deadline(expires_at_ms) {
                    Ok(d) => d,
                    Err(Expired) => {
                        stats.expired.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                };
                let fields: Vec<HashFieldRecord> = fields
                    .into_iter()
                    .filter_map(|f| match clock.deadline(f.expires_at_ms) {
                        Ok(expires_at) => {
                            Some(HashFieldRecord { name: f.name, value: f.value, expires_at })
                        }
                        Err(Expired) => None,
                    })
                    .collect();
                if fields.is_empty() {
                    stats.expired.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                if store.restore_hash(key, fields, expires_at) {
                    stats.restored.fetch_add(1, Ordering::Relaxed);
                } else {
                    stats.refused.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
    if c.remaining() != 0 {
        return Err(BlockFailure::Corrupt(SnapshotError::Corrupt(
            "trailing bytes in block".into(),
        )));
    }
    Ok(())
}

/// Marker: the entry's expiry has passed.
struct Expired;

/// Wall/monotonic clock pair taken once per block, so converting a Unix
/// millisecond deadline back to an `Instant` costs no syscalls per entry.
struct LoadClock {
    instant: Instant,
    unix_ms: u64,
}

impl LoadClock {
    fn now() -> Self {
        Self { instant: Instant::now(), unix_ms: unix_ms_for(SystemTime::now()) }
    }

    fn deadline(&self, expires_at_ms: u64) -> Result<Option<Instant>, Expired> {
        if expires_at_ms == 0 {
            return Ok(None);
        }
        if expires_at_ms <= self.unix_ms {
            return Err(Expired);
        }
        Ok(Some(self.instant + Duration::from_millis(expires_at_ms - self.unix_ms)))
    }
}

// ── Inspect / verify ─────────────────────────────────────────────────────────

/// Per-site totals from the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteSummary {
    /// Site key (`""` = default store).
    pub site: String,
    /// Data blocks.
    pub blocks: usize,
    /// Entries.
    pub entries: u64,
    /// Plaintext bytes before compression.
    pub plain_bytes: u64,
}

/// What `inspect` learned about a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspection {
    /// The header.
    pub header: Header,
    /// Bytes of the file.
    pub bytes: u64,
    /// Per-site totals, when the index could be opened (requires the secret).
    pub sites: Option<Vec<SiteSummary>>,
    /// Total data blocks, when the index could be opened.
    pub blocks: Option<usize>,
}

/// Read a snapshot's header and — given the secret — its index, without
/// touching any store or any data block.
///
/// # Errors
///
/// A file that is not a readable snapshot. Without `secret`, only the header
/// is read; with the wrong one, [`SnapshotError::WrongKey`].
pub fn inspect(
    path: &Path,
    secret: Option<&[u8]>,
    max_size: u64,
) -> Result<Inspection, SnapshotError> {
    let Some(secret) = secret else {
        return inspect_header_only(path, max_size);
    };
    let opened = open(path, secret, max_size).map_err(|f| f.error)?;
    let mut sites: Vec<SiteSummary> = Vec::new();
    for e in &opened.index {
        match sites.iter_mut().find(|s| s.site == e.site) {
            Some(s) => {
                s.blocks += 1;
                s.entries += u64::from(e.entries);
                s.plain_bytes += e.plain_bytes;
            }
            None => sites.push(SiteSummary {
                site: e.site.clone(),
                blocks: 1,
                entries: u64::from(e.entries),
                plain_bytes: e.plain_bytes,
            }),
        }
    }
    Ok(Inspection {
        header: opened.header.header,
        bytes: opened.size,
        sites: Some(sites),
        blocks: Some(opened.index.len()),
    })
}

fn inspect_header_only(path: &Path, max_size: u64) -> Result<Inspection, SnapshotError> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    if size > max_size {
        return Err(SnapshotError::TooLarge { size, max: max_size });
    }
    let mut prefix = [0u8; HEADER_FIXED_LEN];
    file.read_exact(&mut prefix)?;
    let header_len = match Header::declared_len(&prefix) {
        Ok(n) => n,
        Err(FormatError::UnsupportedVersion(v)) => {
            return Err(SnapshotError::UnsupportedVersion(v));
        }
        Err(e) => return Err(SnapshotError::Corrupt(e.to_string())),
    };
    let mut header_bytes = vec![0u8; header_len];
    header_bytes[..HEADER_FIXED_LEN].copy_from_slice(&prefix);
    file.read_exact(&mut header_bytes[HEADER_FIXED_LEN..])?;
    let header =
        Header::decode(&header_bytes).map_err(|e| SnapshotError::Corrupt(e.to_string()))?;
    Ok(Inspection { header: header.header, bytes: size, sites: None, blocks: None })
}

/// What `verify` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// The header.
    pub header: Header,
    /// Data blocks checked.
    pub blocks: usize,
    /// Blocks that failed to authenticate or decode, by index position.
    pub bad_blocks: Vec<usize>,
    /// Entries decoded across all good blocks.
    pub entries: u64,
}

impl VerifyReport {
    /// Whether every block verified.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.bad_blocks.is_empty()
    }
}

/// Authenticate and decode every block of the snapshot without restoring
/// anything.
///
/// # Errors
///
/// A file whose header, trailer or index does not verify (as for a load);
/// per-block failures are reported, not returned.
pub fn verify(path: &Path, secret: &[u8], max_size: u64) -> Result<VerifyReport, SnapshotError> {
    let Opened { mut file, header, cipher, index, .. } =
        open(path, secret, max_size).map_err(|f| f.error)?;
    let mut bad_blocks = Vec::new();
    let mut entries = 0u64;
    for (i, e) in index.iter().enumerate() {
        let ok = read_block(&mut file, e.offset, e.len, BLOCK_KIND_DATA)
            .ok()
            .and_then(|ct| cipher.open_block(&header.bytes, i as u64, &ct))
            .and_then(|opened| unpack_block(&opened, MAX_BLOCK_RAW).ok())
            .and_then(|payload| {
                let mut c = Cursor::new(&payload);
                let head = DataBlockHead::decode(&mut c).ok()?;
                if head.site != e.site {
                    return None;
                }
                let mut n = 0u64;
                for _ in 0..head.count {
                    decode_entry(&mut c).ok()?;
                    n += 1;
                }
                (c.remaining() == 0).then_some(n)
            });
        match ok {
            Some(n) => entries += n,
            None => bad_blocks.push(i),
        }
    }
    Ok(VerifyReport { header: header.header, blocks: index.len(), bad_blocks, entries })
}
