//! Open file cache for static file metadata and content.
//!
//! Caches filesystem metadata (`stat` results), MIME types, `ETag` values,
//! and optionally small file content to avoid repeated disk I/O for
//! frequently accessed static files.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use dashmap::DashMap;
use ephpm_config::FileCacheConfig;

use crate::router::CompressionSettings;

/// Cached metadata and optional content for a static file.
#[derive(Clone)]
pub struct CacheEntry {
    /// File size in bytes.
    pub size: u64,
    /// Last modification time from filesystem stat.
    pub mtime: SystemTime,
    /// Pre-computed `ETag` based on mtime and size.
    pub etag: String,
    /// MIME type string (e.g. `"text/css"`).
    pub mime: String,
    /// Cached file content for small files. `None` for files above inline threshold.
    pub content: Option<Bytes>,
    /// Pre-compressed gzip content (if precompress enabled and file is compressible).
    pub gzip_content: Option<Bytes>,
    /// Last time we checked the filesystem stat.
    last_validated: Instant,
    /// Last time this entry was accessed.
    last_accessed: Instant,
}

/// Open file cache for static files.
///
/// Thread-safe — backed by [`DashMap`] for concurrent access.
pub struct FileCache {
    entries: DashMap<PathBuf, CacheEntry>,
    valid_duration: Duration,
    inactive_duration: Duration,
    inline_threshold: usize,
    max_entries: usize,
    precompress: bool,
}

impl FileCache {
    /// Create a new file cache from configuration.
    #[must_use]
    pub fn new(config: &FileCacheConfig) -> Self {
        Self {
            entries: DashMap::new(),
            valid_duration: Duration::from_secs(config.valid_secs),
            inactive_duration: Duration::from_secs(config.inactive_secs),
            inline_threshold: config.inline_threshold,
            max_entries: config.max_entries,
            precompress: config.precompress,
        }
    }

    /// Look up a cached entry for the given path.
    ///
    /// Returns `Some(entry)` if the cache has a valid entry. Re-validates
    /// against the filesystem if the validation interval has elapsed.
    /// Returns `None` on miss or if the file has changed on disk.
    pub async fn lookup(&self, path: &Path) -> Option<CacheEntry> {
        let mut entry = self.entries.get_mut(path)?;
        let now = Instant::now();

        // Update access time for LRU tracking.
        entry.last_accessed = now;

        // Check if re-validation is needed.
        if now.duration_since(entry.last_validated) < self.valid_duration {
            return Some(entry.clone());
        }

        // Re-stat the file to check if it changed.
        let metadata = tokio::fs::metadata(path).await.ok()?;
        let mtime = metadata.modified().ok()?;

        if mtime != entry.mtime {
            // File changed — invalidate.
            drop(entry);
            self.entries.remove(path);
            return None;
        }

        // Still valid — update validation time.
        entry.last_validated = now;
        Some(entry.clone())
    }

    /// Insert a file into the cache.
    ///
    /// Computes metadata-based `ETag`, optionally caches content and
    /// pre-compressed variant. Evicts old entries if at capacity.
    pub fn insert(
        &self,
        path: &Path,
        content: &[u8],
        mtime: SystemTime,
        mime: &str,
        compression: CompressionSettings,
    ) -> CacheEntry {
        // Evict if over capacity.
        if self.entries.len() >= self.max_entries {
            self.evict_oldest();
        }

        let size = content.len() as u64;
        let etag = compute_mtime_etag(mtime, size);
        let now = Instant::now();

        let cached_content = if content.len() <= self.inline_threshold {
            Some(Bytes::copy_from_slice(content))
        } else {
            None
        };

        let gzip_content = if self.precompress && cached_content.is_some() {
            crate::router::gzip_compress(content, mime, compression).map(Bytes::from)
        } else {
            None
        };

        let entry = CacheEntry {
            size,
            mtime,
            etag,
            mime: mime.to_string(),
            content: cached_content,
            gzip_content,
            last_validated: now,
            last_accessed: now,
        };

        self.entries.insert(path.to_path_buf(), entry.clone());
        entry
    }

    /// Remove entries not accessed within the inactive duration.
    pub fn evict_inactive(&self) {
        let cutoff = Instant::now() - self.inactive_duration;
        self.entries.retain(|_, entry| entry.last_accessed > cutoff);
    }

    /// Number of cached entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Evict the oldest entry by last access time.
    fn evict_oldest(&self) {
        let mut oldest_key: Option<PathBuf> = None;
        let mut oldest_time = Instant::now();

        for entry in &self.entries {
            if entry.last_accessed < oldest_time {
                oldest_time = entry.last_accessed;
                oldest_key = Some(entry.key().clone());
            }
        }

        if let Some(key) = oldest_key {
            self.entries.remove(&key);
        }
    }
}

/// Compute an `ETag` from file metadata (mtime + size).
///
/// Format: `W/"{mtime_secs:x}-{size:x}"`. This avoids reading file
/// content for ETag generation — a significant win for large files.
fn compute_mtime_etag(mtime: SystemTime, size: u64) -> String {
    let secs = mtime
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("W/\"{secs:x}-{size:x}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cache() -> FileCache {
        FileCache::new(&FileCacheConfig {
            enabled: true,
            max_entries: 10,
            valid_secs: 60,
            inactive_secs: 120,
            inline_threshold: 1024,
            precompress: false,
        })
    }

    fn test_compression() -> CompressionSettings {
        CompressionSettings {
            enabled: true,
            level: 1,
            min_size: 1024,
        }
    }

    #[test]
    fn insert_and_lookup_sync() {
        let cache = test_cache();
        let path = PathBuf::from("/tmp/test.css");
        let content = b"body { color: red; }";
        let mtime = SystemTime::now();

        let entry = cache.insert(&path, content, mtime, "text/css", test_compression());
        assert_eq!(entry.size, 20);
        assert!(entry.content.is_some());
        assert_eq!(entry.mime, "text/css");
        assert!(!entry.etag.is_empty());
    }

    #[test]
    fn eviction_at_capacity() {
        let cache = test_cache(); // max_entries = 10
        let mtime = SystemTime::now();

        for i in 0..15 {
            let path = PathBuf::from(format!("/tmp/file{i}.txt"));
            cache.insert(&path, b"data", mtime, "text/plain", test_compression());
        }

        // Should not exceed max_entries.
        assert!(cache.len() <= 10);
    }

    #[test]
    fn mtime_etag_format() {
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(0x6789_abcd);
        let etag = compute_mtime_etag(mtime, 1024);
        assert_eq!(etag, "W/\"6789abcd-400\"");
    }

    #[test]
    fn large_file_no_content_cached() {
        let cache = test_cache(); // inline_threshold = 1024
        let path = PathBuf::from("/tmp/big.bin");
        let content = vec![0u8; 2048];
        let mtime = SystemTime::now();

        let entry = cache.insert(&path, &content, mtime, "application/octet-stream", test_compression());
        assert!(entry.content.is_none(), "files above inline_threshold should not cache content");
        assert_eq!(entry.size, 2048);
    }
}
