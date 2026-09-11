//! The policy engine: merged findings + analyzer outcomes → a [`Verdict`].
//!
//! # The model
//!
//! Three layers, evaluated strictest-first; the verdict is the **maximum**
//! any layer demands (verdicts are ordered `Allow < Quarantine < Deny`):
//!
//! 1. **Hard denies.** A finding whose `rule_id` matches an
//!    `analyzers.deny_hard` entry (exact, or under it as a `/`-prefix), or
//!    any finding of `Critical` severity, forces `Deny` outright. No score
//!    can talk its way past these.
//! 2. **Fail-closed floor.** Any analyzer *failure* — a tool error, timeout,
//!    unparseable output, or a `required` analyzer that could not run —
//!    floors the verdict at `Quarantine`. An analyzer that errored might
//!    have been about to find something, so the result can never be `Allow`.
//!    A plain skip (optional tool absent) does **not** floor; it is surfaced
//!    as a diagnostic instead.
//! 3. **Weighted score.** Each finding contributes a weight by severity —
//!    info 0, low 1, medium 4, high 10, critical 40 — and the total is
//!    compared against the tunable `policy.quarantine_score` (default 10)
//!    and `policy.deny_score` (default 50) thresholds. The weights are
//!    deliberately super-additive (one high > two mediums > eight lows) so
//!    volume of noise cannot outrank a single serious finding class, while a
//!    large pile of mediums still escalates.
//!
//! The returned [`PolicyDecision`] carries human-readable reasons for every
//! layer that fired, so the text output can say *why* a run gated.

use crate::finding::{Finding, Severity};

/// The gate's final decision for a run, ordered least to most severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Nothing gate-worthy found.
    Allow,
    /// Needs human review before deploy — findings crossed the quarantine
    /// threshold, or an analyzer failed (fail-closed).
    Quarantine,
    /// Hard stop — a hard-deny rule fired, a critical finding exists, or the
    /// score crossed the deny threshold.
    Deny,
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Allow => "allow",
            Self::Quarantine => "quarantine",
            Self::Deny => "deny",
        })
    }
}

/// A verdict plus the evidence that produced it.
#[derive(Debug, Clone)]
pub struct PolicyDecision {
    /// The final verdict (already the max across all layers).
    pub verdict: Verdict,
    /// The weighted score across all findings.
    pub score: u32,
    /// One line per layer that fired, for the report.
    pub reasons: Vec<String>,
}

/// Score weight per severity — see the module docs for why super-additive.
#[must_use]
pub fn severity_weight(severity: Severity) -> u32 {
    match severity {
        Severity::Info => 0,
        Severity::Low => 1,
        Severity::Medium => 4,
        Severity::High => 10,
        Severity::Critical => 40,
    }
}

/// `true` when `rule_id` matches a `deny_hard` entry: exact, or falling
/// under the entry as a `/`-separated prefix (`malware-yara` matches
/// `malware-yara/AnyRule` but not `malware-yara2/x`).
#[must_use]
pub fn deny_hard_matches(entry: &str, rule_id: &str) -> bool {
    rule_id == entry
        || (rule_id.len() > entry.len()
            && rule_id.starts_with(entry)
            && rule_id.as_bytes()[entry.len()] == b'/')
}

/// Evaluate the policy over merged findings.
///
/// `failed_analyzers` lists the analyzers that errored (or were required and
/// could not run) — a non-empty list floors the verdict at `Quarantine`.
#[must_use]
pub fn decide(
    findings: &[Finding],
    deny_hard: &[String],
    failed_analyzers: &[String],
    quarantine_score: u32,
    deny_score: u32,
) -> PolicyDecision {
    let mut reasons = Vec::new();
    let mut verdict = Verdict::Allow;

    // Layer 1: hard denies.
    for finding in findings {
        if let Some(entry) = deny_hard.iter().find(|e| deny_hard_matches(e, &finding.rule_id)) {
            verdict = Verdict::Deny;
            reasons.push(format!(
                "hard deny: finding {} matches deny_hard entry {entry:?}",
                finding.rule_id
            ));
        }
    }
    if let Some(critical) = findings.iter().find(|f| f.severity == Severity::Critical) {
        verdict = Verdict::Deny;
        reasons.push(format!("hard deny: critical finding {}", critical.rule_id));
    }

    // Layer 2: fail-closed floor.
    if !failed_analyzers.is_empty() {
        verdict = verdict.max(Verdict::Quarantine);
        reasons.push(format!(
            "fail-closed: analyzer(s) failed: {} — result cannot be allow",
            failed_analyzers.join(", ")
        ));
    }

    // Layer 3: weighted score.
    let score: u32 = findings.iter().map(|f| severity_weight(f.severity)).sum();
    if score >= deny_score {
        verdict = Verdict::Deny;
        reasons.push(format!("score {score} >= deny threshold {deny_score}"));
    } else if score >= quarantine_score {
        verdict = verdict.max(Verdict::Quarantine);
        reasons.push(format!("score {score} >= quarantine threshold {quarantine_score}"));
    }

    PolicyDecision { verdict, score, reasons }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::finding::Category;

    fn finding(rule_id: &str, severity: Severity) -> Finding {
        Finding {
            rule_id: rule_id.to_owned(),
            severity,
            category: Category::Security,
            path: Some(PathBuf::from("index.php")),
            line: Some(1),
            message: "test".to_owned(),
        }
    }

    #[test]
    fn clean_run_allows() {
        let d = decide(&[], &[], &[], 10, 50);
        assert_eq!(d.verdict, Verdict::Allow);
        assert_eq!(d.score, 0);
        assert!(d.reasons.is_empty());
    }

    #[test]
    fn deny_hard_rule_id_forces_deny_even_for_info() {
        let f = [finding("dangerous-sinks/eval", Severity::Info)];
        let d = decide(&f, &["dangerous-sinks/eval".to_owned()], &[], 10, 50);
        assert_eq!(d.verdict, Verdict::Deny);
    }

    #[test]
    fn deny_hard_prefix_matches_analyzer_namespace() {
        assert!(deny_hard_matches("malware-yara", "malware-yara/EvilRule"));
        assert!(deny_hard_matches("malware-yara/EvilRule", "malware-yara/EvilRule"));
        assert!(!deny_hard_matches("malware-yara", "malware-yara2/x"));
        assert!(!deny_hard_matches("malware-yara/Evil", "malware-yara/EvilRule"));
    }

    #[test]
    fn critical_finding_forces_deny() {
        let f = [finding("composer-audit/CVE-2024-1", Severity::Critical)];
        let d = decide(&f, &[], &[], 10, 50);
        assert_eq!(d.verdict, Verdict::Deny);
    }

    #[test]
    fn analyzer_failure_floors_at_quarantine_with_zero_findings() {
        // The heart of fail-closed: no findings at all, but an analyzer
        // errored — the result must never be Allow.
        let d = decide(&[], &[], &["semgrep-php".to_owned()], 10, 50);
        assert_eq!(d.verdict, Verdict::Quarantine);
    }

    #[test]
    fn analyzer_failure_does_not_downgrade_a_deny() {
        let f = [finding("x/critical", Severity::Critical)];
        let d = decide(&f, &[], &["semgrep-php".to_owned()], 10, 50);
        assert_eq!(d.verdict, Verdict::Deny);
    }

    #[test]
    fn score_thresholds_escalate() {
        // 2 mediums = 8 < 10 -> allow
        let f = [finding("a/1", Severity::Medium), finding("a/2", Severity::Medium)];
        assert_eq!(decide(&f, &[], &[], 10, 50).verdict, Verdict::Allow);

        // 1 high = 10 >= 10 -> quarantine
        let f = [finding("a/1", Severity::High)];
        let d = decide(&f, &[], &[], 10, 50);
        assert_eq!(d.verdict, Verdict::Quarantine);
        assert_eq!(d.score, 10);

        // 5 highs = 50 >= 50 -> deny
        let f: Vec<_> = (0..5).map(|i| finding(&format!("a/{i}"), Severity::High)).collect();
        assert_eq!(decide(&f, &[], &[], 10, 50).verdict, Verdict::Deny);
    }

    #[test]
    fn info_findings_never_gate() {
        let f: Vec<_> = (0..1000).map(|i| finding(&format!("a/{i}"), Severity::Info)).collect();
        let d = decide(&f, &[], &[], 10, 50);
        assert_eq!(d.verdict, Verdict::Allow);
        assert_eq!(d.score, 0);
    }

    #[test]
    fn reasons_explain_every_fired_layer() {
        let f = [finding("x/crit", Severity::Critical)];
        let d = decide(&f, &["x/crit".to_owned()], &["yara".to_owned()], 10, 50);
        assert_eq!(d.verdict, Verdict::Deny);
        assert!(d.reasons.iter().any(|r| r.contains("deny_hard")));
        assert!(d.reasons.iter().any(|r| r.contains("critical")));
        assert!(d.reasons.iter().any(|r| r.contains("fail-closed")));
    }
}
