//! Baseline files — adopt an existing codebase by recording its current
//! findings, then gate only on *new* ones (the PHPStan model).
//!
//! A baseline is a JSON document listing one entry per known finding,
//! keyed by a **stable fingerprint**: SHA-256 over the finding's rule id,
//! its path (normalized to `/` separators so Windows and Unix agree), and
//! its message — deliberately **not** the line number, so reformatting or
//! unrelated edits that shift a finding vertically do not resurrect it.
//! Alongside the fingerprint each entry carries the human-readable fields it
//! was derived from, so a baseline diff in code review is legible.
//!
//! SHA-256 rather than a fast non-cryptographic hash on purpose: the
//! analyzed tree is *tenant-controlled adversarial input*, and with a weak
//! hash an attacker could craft a malicious finding whose fingerprint
//! collides with a baselined benign one, silently waiving it.
//!
//! Generation is a CLI concern (`ephpm analyze --baseline <file>` writes the
//! file); consumption is config (`baseline: <path>` suppresses matches). A
//! configured-but-missing baseline file is a hard error — fail-closed,
//! consistent with `analyzers.yara_rules`.

use std::collections::HashSet;
use std::path::Path;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::finding::Finding;

/// Format version written to (and required from) baseline files.
const BASELINE_VERSION: u32 = 1;

/// One remembered finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaselineEntry {
    /// The stable fingerprint — see [`fingerprint`].
    pub fingerprint: String,
    /// The rule id at generation time (informational, for reviewability).
    pub rule_id: String,
    /// The `/`-normalized path at generation time (informational).
    #[serde(default)]
    pub path: Option<String>,
    /// The message at generation time (informational).
    pub message: String,
}

/// A parsed baseline file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    /// Format version — must equal 1.
    pub version: u32,
    /// The remembered findings, sorted by fingerprint for stable diffs.
    pub findings: Vec<BaselineEntry>,
}

/// A finding's path normalized to forward slashes, for fingerprinting and
/// suppression matching that agrees across host platforms.
#[must_use]
pub fn normalized_path(finding: &Finding) -> Option<String> {
    finding.path.as_ref().map(|p| p.to_string_lossy().replace('\\', "/"))
}

/// The stable fingerprint of a finding: hex SHA-256 over rule id, normalized
/// path, and message (never the line number — see the module docs). Fields
/// are length-prefixed so no two distinct triples concatenate identically.
#[must_use]
pub fn fingerprint(finding: &Finding) -> String {
    let path = normalized_path(finding).unwrap_or_default();
    let mut hasher = Sha256::new();
    for field in [finding.rule_id.as_str(), path.as_str(), finding.message.as_str()] {
        hasher.update(u64::try_from(field.len()).unwrap_or(u64::MAX).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

impl Baseline {
    /// Build a baseline from the given findings, deduplicating identical
    /// fingerprints and sorting for stable diffs.
    #[must_use]
    pub fn from_findings(findings: &[Finding]) -> Self {
        let mut seen = HashSet::new();
        let mut entries: Vec<BaselineEntry> = findings
            .iter()
            .filter_map(|f| {
                let fingerprint = fingerprint(f);
                seen.insert(fingerprint.clone()).then(|| BaselineEntry {
                    fingerprint,
                    rule_id: f.rule_id.clone(),
                    path: normalized_path(f),
                    message: f.message.clone(),
                })
            })
            .collect();
        entries.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
        Self { version: BASELINE_VERSION, findings: entries }
    }

    /// Load and validate a baseline file.
    ///
    /// # Errors
    ///
    /// On a missing/unreadable file, invalid JSON, unknown fields, or an
    /// unsupported `version` — all hard errors, so a corrupt baseline never
    /// silently degrades into a full un-suppressed (or worse, an
    /// over-suppressed) run.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read baseline {}", path.display()))?;
        let baseline: Self = serde_json::from_str(&text)
            .with_context(|| format!("invalid baseline {}", path.display()))?;
        anyhow::ensure!(
            baseline.version == BASELINE_VERSION,
            "baseline {} has unsupported version {} (this build reads version {BASELINE_VERSION})",
            path.display(),
            baseline.version
        );
        Ok(baseline)
    }

    /// Write the baseline as pretty JSON.
    ///
    /// # Errors
    ///
    /// On serialization or write failure.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(self).context("failed to serialize baseline")?;
        std::fs::write(path, json)
            .with_context(|| format!("failed to write baseline {}", path.display()))
    }

    /// The set of fingerprints this baseline suppresses.
    #[must_use]
    pub fn fingerprints(&self) -> HashSet<&str> {
        self.findings.iter().map(|e| e.fingerprint.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::finding::{Category, Confidence, Finding, Severity};

    fn finding(rule_id: &str, path: &str, line: Option<u64>, message: &str) -> Finding {
        Finding {
            rule_id: rule_id.to_owned(),
            severity: Severity::Medium,
            category: Category::Security,
            path: (!path.is_empty()).then(|| PathBuf::from(path)),
            line,
            message: message.to_owned(),
            confidence: Confidence::Confirmed,
        }
    }

    #[test]
    fn fingerprint_ignores_line_number() {
        // The whole point: a finding that moved vertically (reformatting)
        // keeps its fingerprint.
        let a = finding("x/r", "src/a.php", Some(10), "msg");
        let b = finding("x/r", "src/a.php", Some(999), "msg");
        assert_eq!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn fingerprint_distinguishes_rule_path_and_message() {
        let base = finding("x/r", "src/a.php", None, "msg");
        assert_ne!(fingerprint(&base), fingerprint(&finding("x/r2", "src/a.php", None, "msg")));
        assert_ne!(fingerprint(&base), fingerprint(&finding("x/r", "src/b.php", None, "msg")));
        assert_ne!(fingerprint(&base), fingerprint(&finding("x/r", "src/a.php", None, "msg2")));
        // Length-prefixing: shifting a byte across a field boundary must not
        // collide ("ab"+"c" vs "a"+"bc").
        assert_ne!(
            fingerprint(&finding("ab", "c", None, "")),
            fingerprint(&finding("a", "bc", None, ""))
        );
    }

    #[test]
    fn fingerprint_is_platform_agnostic_over_separators() {
        let unix = finding("x/r", "src/a.php", None, "msg");
        let windows = finding("x/r", "src\\a.php", None, "msg");
        assert_eq!(fingerprint(&unix), fingerprint(&windows));
    }

    #[test]
    fn roundtrip_and_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("baseline.json");
        let findings = [
            finding("x/r", "a.php", Some(1), "m"),
            finding("x/r", "a.php", Some(2), "m"), // same fingerprint (line differs)
            finding("y/s", "", None, "n"),
        ];
        let baseline = Baseline::from_findings(&findings);
        assert_eq!(baseline.findings.len(), 2);
        baseline.save(&path).unwrap();
        let loaded = Baseline::load(&path).unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.fingerprints(), baseline.fingerprints());
    }

    #[test]
    fn load_rejects_wrong_version_and_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("baseline.json");
        std::fs::write(&path, r#"{"version": 2, "findings": []}"#).unwrap();
        assert!(format!("{:#}", Baseline::load(&path).unwrap_err()).contains("version"));
        std::fs::write(&path, r#"{"version": 1, "findings": [], "extra": 1}"#).unwrap();
        assert!(format!("{:#}", Baseline::load(&path).unwrap_err()).contains("invalid baseline"));
    }

    #[test]
    fn load_missing_file_is_an_error() {
        assert!(Baseline::load(Path::new("Z:/definitely/not/here.json")).is_err());
    }
}
