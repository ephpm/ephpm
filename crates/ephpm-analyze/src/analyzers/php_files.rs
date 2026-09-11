//! Shared tree walker for the native per-file analyzers.
//!
//! Centralizes the concerns every per-file pass shares so `dangerous-sinks`
//! and `suppression-scan` (and future native analyzers) do not re-implement
//! them: PHP-file selection, `.git` skipping, the oversized-file cutoff,
//! diff-aware scope skipping ([`crate::AnalysisCtx::is_in_scope`]), and the
//! incremental content-hash cache ([`crate::cache`]).

use std::path::Path;

use crate::analyzer::{AnalysisCtx, AnalyzerError};
use crate::finding::Finding;

/// Files larger than this are skipped (a minified/vendored blob would drown
/// the report; the YARA analyzer is the right tool for opaque payloads).
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Whether `path` names a PHP source file this walker scans.
fn is_php_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some(ext) if ext.eq_ignore_ascii_case("php") || ext.eq_ignore_ascii_case("phtml")
    )
}

/// Walk the tree under `ctx.root()`, running `scan(text, rel_path)` over
/// every in-scope PHP file and collecting the findings. Does not follow
/// directory symlinks; skips `.git`. Cached per file when a cache is
/// attached — `scan` must therefore be a pure function of the file content,
/// the relative path, and the run configuration.
///
/// # Errors
///
/// [`AnalyzerError::Io`] on directory-walk or read failures (which gate,
/// fail-closed).
pub(crate) fn scan_php_tree(
    ctx: &AnalysisCtx,
    analyzer_id: &str,
    scan: &dyn Fn(&str, &Path) -> Vec<Finding>,
) -> Result<Vec<Finding>, AnalyzerError> {
    let mut findings = Vec::new();
    walk(ctx, analyzer_id, ctx.root(), scan, &mut findings)?;
    Ok(findings)
}

fn walk(
    ctx: &AnalysisCtx,
    analyzer_id: &str,
    dir: &Path,
    scan: &dyn Fn(&str, &Path) -> Vec<Finding>,
    findings: &mut Vec<Finding>,
) -> Result<(), AnalyzerError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            if entry.file_name() == ".git" {
                continue;
            }
            walk(ctx, analyzer_id, &path, scan, findings)?;
        } else if file_type.is_file() && is_php_file(&path) {
            let rel = path.strip_prefix(ctx.root()).unwrap_or(&path);
            if !ctx.is_in_scope(rel) {
                continue;
            }
            if entry.metadata()?.len() > MAX_FILE_BYTES {
                continue;
            }
            let bytes = std::fs::read(&path)?;
            let file_findings = match ctx.cache() {
                Some(cache) => cache.get_or_scan(analyzer_id, rel, &bytes, || {
                    Ok::<_, AnalyzerError>(scan(&String::from_utf8_lossy(&bytes), rel))
                })?,
                None => scan(&String::from_utf8_lossy(&bytes), rel),
            };
            findings.extend(file_findings);
        }
    }
    Ok(())
}
