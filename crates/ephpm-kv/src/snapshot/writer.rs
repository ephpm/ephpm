//! Streaming snapshot writer with bounded memory.
//!
//! The store is walked one shard at a time (see
//! `Store::for_each_string_shard`), so the writer never materialises the
//! whole store. Records flow through a three-stage pipeline on their own OS
//! threads, joined by bounded channels:
//!
//! ```text
//! producer ──(plaintext blocks)──▶ sealers ×N ──(sealed blocks)──▶ writer
//!   walks shards, batches entries    LZ4 + AEAD, CPU-bound         in-order
//!   into ~1 MiB blocks                                             file writes
//! ```
//!
//! Each channel holds at most a few blocks, so memory in flight is bounded by
//! `block_target × (2 × sealers + a small reorder window)` regardless of the
//! store's size. The producer blocks when the sealers are busy — with **no**
//! shard lock held, since every batch leaves its lock before being queued.
//!
//! The file is written as `<final>.tmp-<pid>-<nanos>` beside its destination,
//! `fsync`ed, renamed over the destination, and the directory `fsync`ed
//! (Unix), so a reader sees either the previous complete snapshot or the new
//! complete one — never a partial file. The trailer is the last thing
//! written, and a file without a valid trailer never loads.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::crypto::FileCipher;
use super::format::{
    BLOCK_COMPRESSION_LZ4, BLOCK_KIND_DATA, BLOCK_KIND_INDEX, BLOCK_PREFIX_LEN,
    CIPHER_CHACHA20_POLY1305, DataBlockHead, EntryRecord, FLAG_ENCRYPTED, FLAG_MULTI_TENANT,
    FORMAT_VERSION, FieldRecord, Header, IndexEntry, KDF_HKDF_SHA256, PutLe, SALT_LEN, Trailer,
    encode_entry, encode_index, entry_fits, pack_block, value_compression_id,
};
use super::{SnapshotError, unix_ms_for};
use crate::store::{HashRecord, Store, StringRecord};

/// Default target size of one block's plaintext payload. Large enough that
/// per-block overhead (AEAD tag, index entry, syscall) is negligible, small
/// enough that a loader can spread blocks across cores and a corrupt block
/// costs little.
pub const DEFAULT_BLOCK_TARGET: usize = 1 << 20;

/// How a snapshot is written.
#[derive(Debug, Clone)]
pub struct WriteOptions {
    /// Recorded in the header (truncated to 255 bytes). Informational.
    pub node_id: String,
    /// Abort the write (and keep the previous snapshot) once the file would
    /// exceed this many bytes.
    pub max_size: u64,
    /// Target plaintext bytes per block; see [`DEFAULT_BLOCK_TARGET`].
    pub block_target: usize,
    /// Set [`FLAG_MULTI_TENANT`] in the header.
    pub multi_tenant: bool,
    /// Sealer threads. `0` = one per available core, at most 8.
    pub sealers: usize,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            node_id: String::new(),
            max_size: 1 << 30,
            block_target: DEFAULT_BLOCK_TARGET,
            multi_tenant: false,
            sealers: 0,
        }
    }
}

/// What a completed write produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SaveReport {
    /// Bytes of the finished file.
    pub bytes: u64,
    /// String entries written.
    pub strings: u64,
    /// Hash entries written.
    pub hashes: u64,
    /// Stores (sites) that contributed at least one entry.
    pub sites: usize,
    /// Data blocks written.
    pub blocks: usize,
    /// Wall time of the write.
    pub elapsed: Duration,
}

impl SaveReport {
    /// String plus hash entries.
    #[must_use]
    pub fn keys(&self) -> u64 {
        self.strings + self.hashes
    }
}

/// One block as it leaves the producer.
struct PlainBlock {
    seq: u64,
    site: String,
    entries: u32,
    payload: Vec<u8>,
}

/// One block as it leaves a sealer.
struct SealedBlock {
    seq: u64,
    site: String,
    entries: u32,
    plain_bytes: u64,
    ciphertext: Vec<u8>,
}

/// Write a snapshot of `stores` to `path` atomically.
///
/// `stores` pairs each site key (`""` for the default store) with its store.
/// Blocking: call from a thread that may block on disk and CPU (never from
/// an async worker).
///
/// # Errors
///
/// I/O failures, or [`SnapshotError::TooLarge`] when the file would exceed
/// `opts.max_size`. On any error the temporary file is removed and the
/// previous snapshot at `path`, if any, is left untouched.
pub fn write_snapshot(
    path: &Path,
    secret: &[u8],
    stores: &[(String, Arc<Store>)],
    opts: &WriteOptions,
) -> Result<SaveReport, SnapshotError> {
    let started = Instant::now();
    let tmp = temp_path(path);
    let result = write_to_temp(&tmp, secret, stores, opts);
    match result {
        Ok(mut report) => {
            if let Err(e) = finish_atomic(&tmp, path) {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
            }
            report.elapsed = started.elapsed();
            Ok(report)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// The temporary file beside `path` the writer streams into.
fn temp_path(path: &Path) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    path.with_file_name(format!(".{name}.tmp-{}-{nanos}", std::process::id()))
}

/// Rename the finished temporary over the destination and make the rename
/// durable.
fn finish_atomic(tmp: &Path, path: &Path) -> std::io::Result<()> {
    std::fs::rename(tmp, path)?;
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        // The rename is only durable once the directory entry is: fsync the
        // directory. Not available (and not needed for the atomic-visibility
        // property) on Windows, where `rename` over an existing file is
        // `MoveFileEx(MOVEFILE_REPLACE_EXISTING)`.
        let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

fn write_to_temp(
    tmp: &Path,
    secret: &[u8],
    stores: &[(String, Arc<Store>)],
    opts: &WriteOptions,
) -> Result<SaveReport, SnapshotError> {
    let mut salt = [0u8; SALT_LEN];
    getrandom::fill(&mut salt).map_err(|e| {
        SnapshotError::Io(std::io::Error::other(format!("OS entropy for the snapshot salt: {e}")))
    })?;
    let cipher = FileCipher::derive(secret, &salt);
    let mut node_id = opts.node_id.clone();
    while node_id.len() > 255 {
        node_id.pop();
    }
    let header = Header {
        version: FORMAT_VERSION,
        flags: FLAG_ENCRYPTED | if opts.multi_tenant { FLAG_MULTI_TENANT } else { 0 },
        created_at_ms: unix_ms_for(SystemTime::now()),
        cipher: CIPHER_CHACHA20_POLY1305,
        block_compression: BLOCK_COMPRESSION_LZ4,
        kdf: KDF_HKDF_SHA256,
        salt,
        node_id,
    };
    let key_check = cipher.key_check(&header.encode_prefix());
    let header_bytes = Arc::new(header.encode(&key_check));

    let mut file = File::create(tmp)?;
    file.write_all(&header_bytes)?;
    let header_len = header_bytes.len() as u64;
    if header_len > opts.max_size {
        return Err(SnapshotError::TooLarge { size: header_len, max: opts.max_size });
    }

    let sealers = if opts.sealers == 0 {
        std::thread::available_parallelism().map_or(1, |n| n.get().min(8))
    } else {
        opts.sealers
    };
    let block_target = opts.block_target.max(4096);
    let abort = Arc::new(AtomicBool::new(false));
    let cipher = Arc::new(cipher);

    let (plain_tx, plain_rx) = sync_channel::<PlainBlock>(sealers * 2);
    let (sealed_tx, sealed_rx) = sync_channel::<SealedBlock>(sealers * 2);
    let plain_rx = Arc::new(std::sync::Mutex::new(plain_rx));

    let outcome = std::thread::scope(|scope| {
        for i in 0..sealers {
            let rx = Arc::clone(&plain_rx);
            let tx = sealed_tx.clone();
            let cipher = Arc::clone(&cipher);
            let header_bytes = Arc::clone(&header_bytes);
            std::thread::Builder::new()
                .name(format!("kv-snap-seal-{i}"))
                .spawn_scoped(scope, move || seal_loop(&rx, &tx, &cipher, &header_bytes))
                .expect("spawn snapshot sealer thread");
        }
        // The sealers hold their own clones; drop ours so the writer sees EOF
        // once every sealer has exited.
        drop(sealed_tx);

        let writer_abort = Arc::clone(&abort);
        let max_size = opts.max_size;
        let writer = std::thread::Builder::new()
            .name("kv-snap-write".into())
            .spawn_scoped(scope, move || {
                write_loop(&sealed_rx, &mut file, header_len, max_size, &writer_abort)
                    .map(|(index, offset)| (index, offset, file))
            })
            .expect("spawn snapshot writer thread");

        let produced = produce(stores, &plain_tx, block_target, &abort);
        // Closing the plaintext channel lets the sealers drain and exit, which
        // closes the sealed channel, which ends the writer.
        drop(plain_tx);
        let written = writer.join().expect("snapshot writer thread panicked");
        (produced, written)
    });

    let ((strings, hashes, sites), (index, mut offset, mut file)) = match outcome {
        (produced, Ok(written)) => (produced, written),
        (_, Err(e)) => return Err(e),
    };

    // Index block, sealed under a sequence number bound to its own offset
    // (see `reader::index_seq_for`): a reader learns the offset from the
    // trailer before it can decode anything else.
    let index_offset = offset;
    let index_payload = encode_index(&index);
    let packed = pack_block(&index_payload, BLOCK_COMPRESSION_LZ4);
    let sealed =
        cipher.seal_block(&header_bytes, super::reader::index_seq_for(index_offset), &packed);
    let mut prefix = Vec::with_capacity(BLOCK_PREFIX_LEN);
    prefix.put_u8(BLOCK_KIND_INDEX);
    prefix.put_u32(u32::try_from(sealed.len()).expect("index block fits u32"));
    offset += (prefix.len() + sealed.len()) as u64 + super::format::TRAILER_LEN as u64;
    if offset > opts.max_size {
        return Err(SnapshotError::TooLarge { size: offset, max: opts.max_size });
    }
    file.write_all(&prefix)?;
    file.write_all(&sealed)?;
    let trailer = Trailer {
        index_offset,
        index_len: u32::try_from(sealed.len()).expect("index block fits u32"),
    };
    file.write_all(&trailer.encode())?;
    file.sync_all()?;
    drop(file);

    Ok(SaveReport {
        bytes: offset,
        strings,
        hashes,
        sites,
        blocks: index.len(),
        elapsed: Duration::ZERO,
    })
}

/// Walk every store, batching entries into blocks. Returns
/// `(strings, hashes, sites)` counts.
fn produce(
    stores: &[(String, Arc<Store>)],
    tx: &SyncSender<PlainBlock>,
    block_target: usize,
    abort: &AtomicBool,
) -> (u64, u64, usize) {
    let mut strings = 0u64;
    let mut hashes = 0u64;
    let mut sites = 0usize;
    let mut seq = 0u64;
    for (site, store) in stores {
        if abort.load(Ordering::Relaxed) {
            break;
        }
        let value_compression = value_compression_id(store.config().compression.algo);
        let mut batcher = Batcher::new(site, value_compression, block_target);
        let mut site_entries = 0u64;

        store.for_each_string_shard(|batch| {
            if abort.load(Ordering::Relaxed) {
                return;
            }
            let clock = Clock::now();
            for rec in batch {
                let StringRecord { key, data, compressed, expires_at } = rec;
                let Some(expires_at_ms) = clock.expiry_ms(expires_at) else { continue };
                if batcher.push(&EntryRecord::String { key, data, compressed, expires_at_ms }) {
                    site_entries += 1;
                    strings += 1;
                }
                batcher.flush_if_full(tx, &mut seq);
            }
        });
        store.for_each_hash_shard(|batch| {
            if abort.load(Ordering::Relaxed) {
                return;
            }
            let clock = Clock::now();
            for rec in batch {
                let HashRecord { key, fields, expires_at } = rec;
                let Some(expires_at_ms) = clock.expiry_ms(expires_at) else { continue };
                let fields: Vec<FieldRecord> = fields
                    .into_iter()
                    .filter_map(|f| {
                        clock.expiry_ms(f.expires_at).map(|expires_at_ms| FieldRecord {
                            name: f.name,
                            value: f.value,
                            expires_at_ms,
                        })
                    })
                    .collect();
                if fields.is_empty() {
                    continue;
                }
                if batcher.push(&EntryRecord::Hash { key, fields, expires_at_ms }) {
                    site_entries += 1;
                    hashes += 1;
                }
                batcher.flush_if_full(tx, &mut seq);
            }
        });
        batcher.flush(tx, &mut seq);
        if site_entries > 0 {
            sites += 1;
        }
    }
    (strings, hashes, sites)
}

/// Monotonic/wall clock pair taken once per batch, so converting an entry's
/// `Instant` deadline to Unix milliseconds costs no syscalls per entry.
struct Clock {
    instant: Instant,
    unix_ms: u64,
}

impl Clock {
    fn now() -> Self {
        Self { instant: Instant::now(), unix_ms: unix_ms_for(SystemTime::now()) }
    }

    /// `Some(0)` for no expiry, `Some(ms)` for a future deadline, `None` for
    /// an entry that has already expired (skip it).
    fn expiry_ms(&self, expires_at: Option<Instant>) -> Option<u64> {
        match expires_at {
            None => Some(0),
            Some(at) => {
                let remaining = at.saturating_duration_since(self.instant);
                if remaining.is_zero() {
                    None
                } else {
                    Some(self.unix_ms.saturating_add(remaining.as_millis() as u64).max(1))
                }
            }
        }
    }
}

/// Accumulates one site's entries into blocks of about `block_target` bytes.
struct Batcher<'a> {
    site: &'a str,
    value_compression: u8,
    block_target: usize,
    buf: Vec<u8>,
    count: u32,
}

impl<'a> Batcher<'a> {
    fn new(site: &'a str, value_compression: u8, block_target: usize) -> Self {
        let mut b = Self {
            site,
            value_compression,
            block_target,
            buf: Vec::with_capacity(block_target + 4096),
            count: 0,
        };
        b.start();
        b
    }

    fn start(&mut self) {
        self.buf.clear();
        // The head is patched with the final count at flush time.
        DataBlockHead {
            site: self.site.to_string(),
            value_compression: self.value_compression,
            count: 0,
        }
        .encode(&mut self.buf);
        self.count = 0;
    }

    /// Append an entry; one too large for the format's length fields is
    /// skipped (and `false` returned) rather than written truncated.
    fn push(&mut self, entry: &EntryRecord) -> bool {
        if !entry_fits(entry) {
            tracing::warn!(key = entry.key(), "KV snapshot: entry exceeds 4 GiB, not written");
            return false;
        }
        encode_entry(&mut self.buf, entry);
        self.count += 1;
        true
    }

    fn flush_if_full(&mut self, tx: &SyncSender<PlainBlock>, seq: &mut u64) {
        if self.buf.len() >= self.block_target {
            self.flush(tx, seq);
        }
    }

    fn flush(&mut self, tx: &SyncSender<PlainBlock>, seq: &mut u64) {
        if self.count == 0 {
            return;
        }
        // Patch the count: it sits right after the u16-prefixed site key and
        // the value-compression byte.
        let at = 2 + self.site.len() + 1;
        self.buf[at..at + 4].copy_from_slice(&self.count.to_le_bytes());
        let payload =
            std::mem::replace(&mut self.buf, Vec::with_capacity(self.block_target + 4096));
        let block =
            PlainBlock { seq: *seq, site: self.site.to_string(), entries: self.count, payload };
        *seq += 1;
        // A closed channel means the writer aborted; the abort flag stops the
        // walk on the next batch. Nothing to do with the block here.
        let _ = tx.send(block);
        self.start();
    }
}

fn seal_loop(
    rx: &std::sync::Mutex<Receiver<PlainBlock>>,
    tx: &SyncSender<SealedBlock>,
    cipher: &FileCipher,
    header: &[u8],
) {
    loop {
        let block = {
            let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.recv()
        };
        let Ok(block) = block else { return };
        let plain_bytes = block.payload.len() as u64;
        let packed = pack_block(&block.payload, BLOCK_COMPRESSION_LZ4);
        drop(block.payload);
        let ciphertext = cipher.seal_block(header, block.seq, &packed);
        let sealed = SealedBlock {
            seq: block.seq,
            site: block.site,
            entries: block.entries,
            plain_bytes,
            ciphertext,
        };
        if tx.send(sealed).is_err() {
            return;
        }
    }
}

/// Write sealed blocks to the file in sequence order. Returns the index and
/// the offset just past the last data block.
fn write_loop(
    rx: &Receiver<SealedBlock>,
    file: &mut File,
    start_offset: u64,
    max_size: u64,
    abort: &AtomicBool,
) -> Result<(Vec<IndexEntry>, u64), SnapshotError> {
    let mut index: Vec<IndexEntry> = Vec::new();
    let mut offset = start_offset;
    let mut next_seq = 0u64;
    let mut pending: Vec<SealedBlock> = Vec::new();
    let mut failure: Option<SnapshotError> = None;

    while let Ok(block) = rx.recv() {
        if failure.is_some() {
            // Keep draining so the sealers and producer are never blocked on
            // a full channel; everything is discarded.
            continue;
        }
        pending.push(block);
        // Emit every block whose turn has come. Out-of-order arrivals wait in
        // `pending`, which can hold at most the sealers' in-flight blocks.
        while let Some(pos) = pending.iter().position(|b| b.seq == next_seq) {
            let b = pending.swap_remove(pos);
            let len = u32::try_from(b.ciphertext.len()).expect("block fits u32");
            let end = offset + BLOCK_PREFIX_LEN as u64 + u64::from(len);
            if end > max_size {
                failure = Some(SnapshotError::TooLarge { size: end, max: max_size });
                abort.store(true, Ordering::Relaxed);
                break;
            }
            let mut prefix = Vec::with_capacity(BLOCK_PREFIX_LEN);
            prefix.put_u8(BLOCK_KIND_DATA);
            prefix.put_u32(len);
            if let Err(e) = file.write_all(&prefix).and_then(|()| file.write_all(&b.ciphertext)) {
                failure = Some(e.into());
                abort.store(true, Ordering::Relaxed);
                break;
            }
            index.push(IndexEntry {
                offset,
                len,
                site: b.site,
                entries: b.entries,
                plain_bytes: b.plain_bytes,
            });
            offset = end;
            next_seq += 1;
        }
    }
    match failure {
        Some(e) => Err(e),
        None => Ok((index, offset)),
    }
}
