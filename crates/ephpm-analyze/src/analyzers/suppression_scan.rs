//! `suppression-scan` — flags tenant-authored suppression-shaped comments.
//!
//! ePHPm-analyze **never** honors suppression directives found inside the
//! analyzed code — waivers come exclusively from the operator's config
//! (`suppress:` — see `crate::suppress`). This analyzer is the enforcement
//! inversion: instead of obeying an inline `// ephpm-analyze-ignore`,
//! `@phpstan-ignore`, `// nosemgrep`, `# noqa`, or similar marker, it emits
//! it as a finding (`suppression-scan/tenant-suppression-attempt`, Medium,
//! Confirmed) — a tenant trying to silence a scanner is itself a signal.
//!
//! Complementarily, the `semgrep-php` analyzer passes `--disable-nosem` so
//! Semgrep itself cannot be muted by `nosemgrep` comments either — detection
//! here would be hollow if the underlying tool still obeyed the marker.
//!
//! To keep vendored trees (where `@phpstan-ignore` / `phpcs:ignore` are
//! routine developer hygiene) reviewable rather than overwhelming, findings
//! are deduplicated to **one per (file, marker)**, anchored at the first
//! occurrence. Operators who have vetted a tree tune the noise with ordinary
//! suppressions (e.g. `rule: suppression-scan, path: <vendored file>`) —
//! which, being operator config, remains the only honored mechanism.
//!
//! Matching is case-insensitive over raw lines: this deliberately does not
//! parse PHP, so a marker inside a string literal is still flagged (an
//! acceptable false positive for a review-signal rule; the confidence is
//! about the marker's *presence*, which is a fact).

use std::path::Path;

use super::php_files::scan_php_tree;
use crate::analyzer::{AnalysisCtx, Analyzer, AnalyzerError};
use crate::finding::{Category, Confidence, Finding, Severity};

/// See the module docs.
pub struct SuppressionScan;

/// The analyzer's stable id.
pub const ID: &str = "suppression-scan";

/// The single rule id this analyzer emits.
pub const RULE: &str = "tenant-suppression-attempt";

/// Suppression-shaped markers, lowercase (matched case-insensitively).
/// `@phpstan-ignore` also covers `@phpstan-ignore-line` / `-next-line`;
/// `phpcs:` covers `phpcs:ignore` / `phpcs:disable` / `phpcs:ignoreFile`.
const MARKERS: &[&str] = &[
    "ephpm-analyze-ignore",
    "@phpstan-ignore",
    "@psalm-suppress",
    "@codingstandardsignore",
    "phpcs:ignore",
    "phpcs:disable",
    "nosemgrep",
    "noqa",
    "nosonar",
];

/// Scan one file's text: one finding per marker present, at its first
/// occurrence.
fn scan_text(text: &str, rel_path: &Path) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut remaining: Vec<&str> = MARKERS.to_vec();
    for (line_idx, line) in text.lines().enumerate() {
        if remaining.is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        remaining.retain(|marker| {
            if lower.contains(marker) {
                findings.push(Finding {
                    rule_id: format!("{ID}/{RULE}"),
                    severity: Severity::Medium,
                    category: Category::Security,
                    path: Some(rel_path.to_path_buf()),
                    line: Some(line_idx as u64 + 1),
                    message: format!(
                        "tenant-authored suppression marker {marker:?} — inline suppression \
                         directives are never honored; only operator config (`suppress:`) can \
                         waive findings"
                    ),
                    confidence: Confidence::Confirmed,
                });
                false
            } else {
                true
            }
        });
    }
    findings
}

impl Analyzer for SuppressionScan {
    fn id(&self) -> &str {
        ID
    }

    fn category(&self) -> Category {
        Category::Security
    }

    fn run(&self, ctx: &AnalysisCtx) -> Result<Vec<Finding>, AnalyzerError> {
        scan_php_tree(ctx, ID, &scan_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(src: &str) -> Vec<Finding> {
        scan_text(src, Path::new("t.php"))
    }

    #[test]
    fn flags_each_marker_kind() {
        for marker in
            ["ephpm-analyze-ignore", "@phpstan-ignore-next-line", "nosemgrep", "NOQA", "NOSONAR"]
        {
            let src = format!("<?php\n$x = 1; // {marker}\n");
            let findings = scan(&src);
            assert_eq!(findings.len(), 1, "marker {marker} must be flagged");
            let f = &findings[0];
            assert_eq!(f.rule_id, "suppression-scan/tenant-suppression-attempt");
            assert_eq!(f.severity, Severity::Medium);
            assert_eq!(f.confidence, Confidence::Confirmed);
            assert_eq!(f.line, Some(2));
        }
    }

    #[test]
    fn dedupes_to_one_finding_per_marker_at_first_occurrence() {
        let src = "<?php\n// nosemgrep\n// nosemgrep\n/* noqa */\n";
        let findings = scan(src);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].line, Some(2));
        assert_eq!(findings[1].line, Some(4));
    }

    #[test]
    fn clean_file_yields_nothing() {
        assert!(scan("<?php\n// regular comment about ignoring nothing relevant\n").is_empty());
    }
}
