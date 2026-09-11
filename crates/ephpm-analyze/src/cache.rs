//! Incremental content-hash result cache for per-file analyzers.
//!
//! # Key model (correctness-safe by construction)
//!
//! A cache entry's filename is hex SHA-256 over, length-prefixed:
//!
//! 1. this crate's version — a new build never reuses old results,
//! 2. the analyzer id,
//! 3. a hash of the **effective configuration** (the whole `AnalyzeConfig`
//!    serialized to JSON, CLI overrides included) — *any* config change
//!    invalidates everything,
//! 4. the file's tree-relative path — same content at a different path is a
//!    miss, because findings embed the path,
//! 5. the file's content hash.
//!
//! SHA-256 rather than a fast non-cryptographic hash on purpose: the
//! analyzed tree is tenant-controlled adversarial input, and a weak content
//! hash would let an attacker craft a malicious file that collides with a
//! previously scanned benign one, replaying its clean cached findings.
//!
//! # Who uses it
//!
//! Only the native per-file analyzers (`dangerous-sinks`,
//! `suppression-scan`) — via [`FileCache::get_or_scan`] through
//! `AnalysisCtx`. External-tool analyzers are deliberately never cached:
//! their results depend on state outside the tree (advisory databases,
//! remote rule packs, ruleset files), so "unchanged file" does not imply
//! "unchanged result" for them. Phase-2 opcode analyzers are the intended
//! main beneficiary.
//!
//! # Failure and trust model
//!
//! Cache *reads* that fail (missing, corrupt, unparseable) are misses; cache
//! *writes* that fail are logged at debug and ignored — the cache can only
//! ever cost a rescan, never a wrong verdict, **provided the cache directory
//! is trusted**. The directory is operator state: whoever can write it can
//! plant findings (or their absence). The default lives under the system
//! temp directory (created `0o700` on Unix); on shared hosts point
//! `cache.dir` / `--cache-dir` at a path only the operator can write.

use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use crate::config::AnalyzeConfig;
use crate::finding::Finding;

/// An open cache handle, scoped to one effective configuration.
#[derive(Debug)]
pub struct FileCache {
    dir: PathBuf,
    config_hash: [u8; 32],
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn update_field(hasher: &mut Sha256, field: &[u8]) {
    hasher.update(u64::try_from(field.len()).unwrap_or(u64::MAX).to_le_bytes());
    hasher.update(field);
}

/// The default cache directory: `ephpm-analyze-cache` under the system temp
/// directory.
#[must_use]
pub fn default_dir() -> PathBuf {
    std::env::temp_dir().join("ephpm-analyze-cache")
}

impl FileCache {
    /// Open (creating if needed) the cache at `dir` for the given effective
    /// configuration.
    ///
    /// # Errors
    ///
    /// On failure to create the cache directory or serialize the config.
    pub fn open(dir: PathBuf, config: &AnalyzeConfig) -> anyhow::Result<Self> {
        let config_json = serde_json::to_vec(config)
            .map_err(|e| anyhow::anyhow!("failed to serialize config for cache keying: {e}"))?;
        let mut hasher = Sha256::new();
        update_field(&mut hasher, config_json.as_slice());
        let config_hash: [u8; 32] = hasher.finalize().into();

        std::fs::create_dir_all(&dir)
            .map_err(|e| anyhow::anyhow!("failed to create cache dir {}: {e}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            // Best-effort: keep the default temp-dir cache private to the
            // invoking user. An existing dir keeps its permissions.
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        Ok(Self { dir, config_hash })
    }

    /// The entry path for `(analyzer_id, rel_path, content)`.
    fn entry_path(&self, analyzer_id: &str, rel_path: &Path, content: &[u8]) -> PathBuf {
        let mut hasher = Sha256::new();
        update_field(&mut hasher, env!("CARGO_PKG_VERSION").as_bytes());
        update_field(&mut hasher, analyzer_id.as_bytes());
        update_field(&mut hasher, &self.config_hash);
        update_field(&mut hasher, crate::scope::normalize(rel_path).as_bytes());
        update_field(&mut hasher, content);
        self.dir.join(format!("{}.json", hex(&hasher.finalize())))
    }

    /// Return the cached findings for this (analyzer, file, content, config)
    /// tuple, or run `scan` and cache its result.
    ///
    /// # Errors
    ///
    /// Only `scan`'s own error, propagated verbatim (and never cached).
    /// Cache reads that fail are misses; cache writes that fail are logged
    /// and ignored.
    pub fn get_or_scan<E>(
        &self,
        analyzer_id: &str,
        rel_path: &Path,
        content: &[u8],
        scan: impl FnOnce() -> Result<Vec<Finding>, E>,
    ) -> Result<Vec<Finding>, E> {
        let entry = self.entry_path(analyzer_id, rel_path, content);
        if let Ok(text) = std::fs::read_to_string(&entry)
            && let Ok(findings) = serde_json::from_str::<Vec<Finding>>(&text)
        {
            tracing::trace!(analyzer = analyzer_id, path = %rel_path.display(), "cache hit");
            return Ok(findings);
        }
        let findings = scan()?;
        match serde_json::to_string(&findings) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&entry, json) {
                    tracing::debug!(error = %e, entry = %entry.display(), "cache write failed");
                }
            }
            Err(e) => tracing::debug!(error = %e, "cache serialize failed"),
        }
        Ok(findings)
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::finding::{Category, Confidence, Severity};

    fn sample_finding() -> Finding {
        Finding {
            rule_id: "t/rule".to_owned(),
            severity: Severity::Medium,
            category: Category::Security,
            path: Some(PathBuf::from("a.php")),
            line: Some(3),
            message: "m".to_owned(),
            confidence: Confidence::Suspected,
        }
    }

    fn scan_counted(
        cache: &FileCache,
        analyzer: &str,
        rel: &Path,
        content: &[u8],
        counter: &AtomicUsize,
    ) -> Vec<Finding> {
        cache
            .get_or_scan(analyzer, rel, content, || {
                counter.fetch_add(1, Ordering::Relaxed);
                Ok::<_, Infallible>(vec![sample_finding()])
            })
            .unwrap()
    }

    #[test]
    fn hit_skips_the_scan_and_preserves_findings() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache::open(dir.path().to_path_buf(), &AnalyzeConfig::default()).unwrap();
        let runs = AtomicUsize::new(0);
        let first = scan_counted(&cache, "a", Path::new("x.php"), b"<?php 1;", &runs);
        let second = scan_counted(&cache, "a", Path::new("x.php"), b"<?php 1;", &runs);
        assert_eq!(runs.load(Ordering::Relaxed), 1, "second call must be a cache hit");
        assert_eq!(first, second);
        assert_eq!(second[0].confidence, Confidence::Suspected);
    }

    #[test]
    fn content_path_and_analyzer_all_key_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache::open(dir.path().to_path_buf(), &AnalyzeConfig::default()).unwrap();
        let runs = AtomicUsize::new(0);
        scan_counted(&cache, "a", Path::new("x.php"), b"one", &runs);
        scan_counted(&cache, "a", Path::new("x.php"), b"two", &runs); // content differs
        scan_counted(&cache, "a", Path::new("y.php"), b"one", &runs); // path differs
        scan_counted(&cache, "b", Path::new("x.php"), b"one", &runs); // analyzer differs
        assert_eq!(runs.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn any_config_change_invalidates() {
        let dir = tempfile::tempdir().unwrap();
        let runs = AtomicUsize::new(0);
        let cache = FileCache::open(dir.path().to_path_buf(), &AnalyzeConfig::default()).unwrap();
        scan_counted(&cache, "a", Path::new("x.php"), b"c", &runs);

        // Even a config knob irrelevant to this analyzer invalidates —
        // whole-config hashing is the correctness-safe contract.
        let mut config = AnalyzeConfig::default();
        config.policy.deny_score = 51;
        let cache = FileCache::open(dir.path().to_path_buf(), &config).unwrap();
        scan_counted(&cache, "a", Path::new("x.php"), b"c", &runs);
        assert_eq!(runs.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn corrupt_entry_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache::open(dir.path().to_path_buf(), &AnalyzeConfig::default()).unwrap();
        let runs = AtomicUsize::new(0);
        scan_counted(&cache, "a", Path::new("x.php"), b"c", &runs);
        // Corrupt every entry file, then re-query: must rescan, not error.
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            std::fs::write(entry.unwrap().path(), b"{not json").unwrap();
        }
        scan_counted(&cache, "a", Path::new("x.php"), b"c", &runs);
        assert_eq!(runs.load(Ordering::Relaxed), 2);
    }
}
