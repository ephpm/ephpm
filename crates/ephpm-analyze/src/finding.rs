//! The `Finding` record every analyzer produces, plus its `Severity` and
//! `Category` axes.
//!
//! A finding is deliberately flat and analyzer-agnostic: the policy engine
//! (`crate::policy`) and both output formats (`crate::output`) consume only
//! this shape, so a future opcode or engine-in-the-loop analyzer plugs in by
//! producing the same records — nothing downstream changes.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// How bad a single finding is, ordered from least to most severe.
///
/// The ordering is load-bearing: `Severity` derives `Ord` and the policy
/// engine compares severities directly (`Critical` hard-denies, and each
/// level maps to a score weight — see `crate::policy` for the model).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Informational — never gates on its own (score weight 0).
    Info,
    /// Low severity (score weight 1).
    Low,
    /// Medium severity (score weight 4).
    Medium,
    /// High severity (score weight 10).
    High,
    /// Critical severity — a single critical finding is an automatic
    /// [`crate::policy::Verdict::Deny`].
    Critical,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Info => "info",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        };
        f.write_str(s)
    }
}

/// What kind of problem a finding (or an analyzer as a whole) describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    /// Vulnerabilities and insecure code patterns.
    Security,
    /// Correctness / robustness problems that are not directly exploitable.
    Quality,
    /// Formatting and stylistic conventions.
    Style,
    /// Dependency-chain problems (known advisories, abandoned packages).
    SupplyChain,
    /// Known-malicious content (e.g. YARA rule matches).
    Malware,
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Security => "security",
            Self::Quality => "quality",
            Self::Style => "style",
            Self::SupplyChain => "supply-chain",
            Self::Malware => "malware",
        };
        f.write_str(s)
    }
}

/// How sure the producing analyzer is that a finding is real — the axis that
/// separates *hotspots* from confirmed problems.
///
/// The policy engine (`crate::policy`) treats the two very differently:
/// `Confirmed` findings can drive a hard `Deny` (deny-hard match, critical
/// severity, deny-score threshold), while `Suspected` findings are review
/// signals — they contribute to the score and can floor the verdict at
/// `Quarantine`, but never force `Deny` on their own. "Definitely malicious →
/// deny" stays separate from "suspicious → quarantine for review".
///
/// The ordering (`Suspected < Confirmed`) is load-bearing for comparisons.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// A heuristic hit that may be a false positive — a hotspot worth human
    /// review, not proof.
    Suspected,
    /// The analyzer is confident the finding is real (a known advisory
    /// against a pinned lockfile, a malware-signature match, a fact about
    /// the file's content). The default: absence of doubt is confirmed.
    #[default]
    Confirmed,
}

impl fmt::Display for Confidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Suspected => "suspected",
            Self::Confirmed => "confirmed",
        })
    }
}

/// One result produced by an analyzer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Stable rule identifier, namespaced by analyzer id — e.g.
    /// `dangerous-sinks/eval` or `composer-audit/CVE-2024-1234`. This is the
    /// key `analyzers.deny_hard` entries match against.
    pub rule_id: String,
    /// How severe the finding is.
    pub severity: Severity,
    /// What kind of problem it is.
    pub category: Category,
    /// File the finding points at, relative to the analyzed root when the
    /// producing analyzer can express it that way. `None` for findings with
    /// no file anchor (e.g. a dependency advisory when no lockfile path is
    /// meaningful).
    pub path: Option<PathBuf>,
    /// 1-based line number within `path`, when known.
    pub line: Option<u64>,
    /// Human-readable description.
    pub message: String,
    /// How sure the analyzer is — see [`Confidence`] for the policy
    /// consequences. Defaults to `Confirmed` when absent in serialized form.
    #[serde(default)]
    pub confidence: Confidence,
}
