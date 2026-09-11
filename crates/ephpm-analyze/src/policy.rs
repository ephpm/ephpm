//! The policy engine: merged findings + analyzer outcomes → a [`Verdict`].
//!
//! # The model
//!
//! Four layers, evaluated strictest-first; the verdict is the **maximum**
//! any layer demands (verdicts are ordered `Allow < Quarantine < Deny`):
//!
//! 1. **Hard denies.** A finding whose `rule_id` matches an
//!    `analyzers.deny_hard` entry (exact, or under it as a `/`-prefix)
//!    forces `Deny` **regardless of confidence** — `deny_hard` is an
//!    explicit operator instruction, and the operator who lists a heuristic
//!    rule there has accepted its false positives (downgrading it would turn
//!    their instruction into a no-op). A *confirmed* finding of `Critical`
//!    severity also forces `Deny`; a **suspected** critical floors the
//!    verdict at `Quarantine` instead — the automatic escalations are where
//!    confidence separates "definitely malicious → deny" from "suspicious →
//!    review" (see [`crate::finding::Confidence`]). No score can talk its
//!    way past a hard deny.
//! 2. **Fail-closed floor.** Any analyzer *failure* — a tool error, timeout,
//!    unparseable output, or a `required` analyzer that could not run —
//!    floors the verdict at `Quarantine`. An analyzer that errored might
//!    have been about to find something, so the result can never be `Allow`.
//!    A plain skip (optional tool absent) does **not** floor. This layer is
//!    active at **every** strictness level — fail-closed is the framework's
//!    invariant, and no level relaxes it.
//! 3. **Weighted score** *(levels 1+)*. Each finding contributes a weight by
//!    severity — info 0, low 1, medium 4, high 10, critical 40 — and the
//!    total is compared against the tunable `policy.quarantine_score`
//!    (default 10) and `policy.deny_score` (default 50) thresholds. The
//!    weights are deliberately super-additive (one high > two mediums >
//!    eight lows) so volume of noise cannot outrank a single serious finding
//!    class, while a large pile of mediums still escalates. Confidence
//!    splits the thresholds: the **quarantine** threshold compares the total
//!    score (suspected findings are review signals), while the **deny**
//!    threshold compares only the confirmed portion (suspected findings
//!    never add up to a hard stop).
//! 4. **Severity floors** *(levels 2+)*. At level 2, any single `High`-or-
//!    worse finding floors the verdict at `Quarantine` regardless of score;
//!    at level 3 (paranoid), any `Medium`-or-worse finding does. Confidence
//!    does not matter here — quarantine *is* the review signal.
//!
//! # Strictness levels (`level:` in the config)
//!
//! | level | name       | score layer | severity floor        |
//! |-------|------------|-------------|-----------------------|
//! | 0     | permissive | off         | none — only hard denies (and the fail-closed floor) gate |
//! | 1     | standard   | on          | none (the default)     |
//! | 2     | strict     | on          | `High`+ ⇒ quarantine   |
//! | 3     | paranoid   | on          | `Medium`+ ⇒ quarantine |
//!
//! The returned [`PolicyDecision`] carries human-readable reasons for every
//! layer that fired, so the text output can say *why* a run gated.

use crate::finding::{Confidence, Finding, Severity};

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
    /// The weighted score across all findings (confirmed **and** suspected).
    pub score: u32,
    /// One line per layer that fired, for the report.
    pub reasons: Vec<String>,
}

/// Everything [`decide`] needs besides the findings themselves.
///
/// A struct rather than positional arguments: the parameter set grew past
/// five with strictness levels, and a struct keeps call sites readable and
/// future additions non-breaking at every call site simultaneously.
#[derive(Debug, Clone, Copy)]
pub struct PolicyParams<'a> {
    /// `analyzers.deny_hard` — rule ids (or `/`-prefixes) that force `Deny`.
    pub deny_hard: &'a [String],
    /// Analyzers that errored (or were required and could not run) — a
    /// non-empty list floors the verdict at `Quarantine`.
    pub failed_analyzers: &'a [String],
    /// `policy.quarantine_score` — compared against the **total** score.
    pub quarantine_score: u32,
    /// `policy.deny_score` — compared against the **confirmed-only** score.
    pub deny_score: u32,
    /// Strictness level `0..=3` — see the module docs for the mapping.
    pub level: u8,
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

/// Evaluate the policy over merged findings — see the module docs for the
/// full layer model.
#[must_use]
pub fn decide(findings: &[Finding], params: &PolicyParams<'_>) -> PolicyDecision {
    let mut reasons = Vec::new();
    let mut verdict = Verdict::Allow;

    // Layer 1a: operator hard denies — explicit instructions, so they fire
    // regardless of confidence.
    for finding in findings {
        if let Some(entry) =
            params.deny_hard.iter().find(|e| deny_hard_matches(e, &finding.rule_id))
        {
            verdict = Verdict::Deny;
            reasons.push(format!(
                "hard deny: finding {} matches deny_hard entry {entry:?}",
                finding.rule_id
            ));
        }
    }
    // Layer 1b: automatic critical escalation — confidence-gated.
    for critical in findings.iter().filter(|f| f.severity == Severity::Critical) {
        if critical.confidence == Confidence::Confirmed {
            verdict = Verdict::Deny;
            reasons.push(format!("hard deny: critical finding {}", critical.rule_id));
        } else {
            verdict = verdict.max(Verdict::Quarantine);
            reasons.push(format!(
                "quarantine: suspected critical finding {} (suspected findings never hard-deny)",
                critical.rule_id
            ));
        }
    }

    // Layer 2: fail-closed floor — active at every strictness level.
    if !params.failed_analyzers.is_empty() {
        verdict = verdict.max(Verdict::Quarantine);
        reasons.push(format!(
            "fail-closed: analyzer(s) failed: {} — result cannot be allow",
            params.failed_analyzers.join(", ")
        ));
    }

    // Layer 3: weighted score (levels 1+). The total (all confidences) is
    // always computed for the report; at level 0 it does not gate.
    let score: u32 = findings.iter().map(|f| severity_weight(f.severity)).sum();
    let confirmed_score: u32 = findings
        .iter()
        .filter(|f| f.confidence == Confidence::Confirmed)
        .map(|f| severity_weight(f.severity))
        .sum();
    if params.level >= 1 {
        if confirmed_score >= params.deny_score {
            verdict = Verdict::Deny;
            reasons.push(format!(
                "confirmed score {confirmed_score} >= deny threshold {}",
                params.deny_score
            ));
        } else if score >= params.quarantine_score {
            verdict = verdict.max(Verdict::Quarantine);
            reasons
                .push(format!("score {score} >= quarantine threshold {}", params.quarantine_score));
        }
    }

    // Layer 4: severity floors (levels 2+).
    let floor = match params.level {
        0 | 1 => None,
        2 => Some(Severity::High),
        _ => Some(Severity::Medium),
    };
    if let Some(floor_severity) = floor
        && let Some(worst) =
            findings.iter().filter(|f| f.severity >= floor_severity).max_by_key(|f| f.severity)
    {
        verdict = verdict.max(Verdict::Quarantine);
        reasons.push(format!(
            "level {} severity floor: {} finding {} is {floor_severity}+",
            params.level, worst.severity, worst.rule_id
        ));
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
            confidence: Confidence::Confirmed,
        }
    }

    fn suspected(rule_id: &str, severity: Severity) -> Finding {
        Finding { confidence: Confidence::Suspected, ..finding(rule_id, severity) }
    }

    fn params<'a>(deny_hard: &'a [String], failed: &'a [String]) -> PolicyParams<'a> {
        PolicyParams {
            deny_hard,
            failed_analyzers: failed,
            quarantine_score: 10,
            deny_score: 50,
            level: 1,
        }
    }

    #[test]
    fn clean_run_allows() {
        let d = decide(&[], &params(&[], &[]));
        assert_eq!(d.verdict, Verdict::Allow);
        assert_eq!(d.score, 0);
        assert!(d.reasons.is_empty());
    }

    #[test]
    fn deny_hard_rule_id_forces_deny_even_for_info() {
        let f = [finding("dangerous-sinks/eval", Severity::Info)];
        let d = decide(&f, &params(&["dangerous-sinks/eval".to_owned()], &[]));
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
        let d = decide(&f, &params(&[], &[]));
        assert_eq!(d.verdict, Verdict::Deny);
    }

    #[test]
    fn suspected_findings_quarantine_on_automatic_escalations() {
        // The confidence split applies to the AUTOMATIC deny triggers: a
        // suspected critical, and a suspected pile crossing the deny score,
        // floor at Quarantine instead of Deny.
        let f = [suspected("x/crit", Severity::Critical)];
        let d = decide(&f, &params(&[], &[]));
        assert_eq!(d.verdict, Verdict::Quarantine);
        assert!(d.reasons.iter().any(|r| r.contains("never hard-deny")));

        // 6 suspected highs = 60 total >= 50, but confirmed score is 0:
        // quarantine (via the quarantine threshold), not deny.
        let f: Vec<_> = (0..6).map(|i| suspected(&format!("a/{i}"), Severity::High)).collect();
        let d = decide(&f, &params(&[], &[]));
        assert_eq!(d.verdict, Verdict::Quarantine);
        assert_eq!(d.score, 60);
    }

    #[test]
    fn deny_hard_fires_regardless_of_confidence() {
        // An operator's explicit deny_hard entry is an instruction, not a
        // heuristic — listing a suspected-producing rule there accepts its
        // false positives, and downgrading it would make the config a no-op.
        let deny_hard = ["dangerous-sinks/eval".to_owned()];
        let f = [suspected("dangerous-sinks/eval", Severity::High)];
        let d = decide(&f, &params(&deny_hard, &[]));
        assert_eq!(d.verdict, Verdict::Deny);
        assert!(d.reasons.iter().any(|r| r.contains("deny_hard")));
    }

    #[test]
    fn confirmed_score_still_denies_alongside_suspected_noise() {
        // 5 confirmed highs = confirmed 50 >= 50 -> deny, regardless of any
        // suspected findings riding along.
        let mut f: Vec<_> = (0..5).map(|i| finding(&format!("a/{i}"), Severity::High)).collect();
        f.push(suspected("b/0", Severity::Medium));
        assert_eq!(decide(&f, &params(&[], &[])).verdict, Verdict::Deny);
    }

    #[test]
    fn analyzer_failure_floors_at_quarantine_with_zero_findings() {
        // The heart of fail-closed: no findings at all, but an analyzer
        // errored — the result must never be Allow.
        let failed = ["semgrep-php".to_owned()];
        let d = decide(&[], &params(&[], &failed));
        assert_eq!(d.verdict, Verdict::Quarantine);
    }

    #[test]
    fn analyzer_failure_does_not_downgrade_a_deny() {
        let failed = ["semgrep-php".to_owned()];
        let f = [finding("x/critical", Severity::Critical)];
        let d = decide(&f, &params(&[], &failed));
        assert_eq!(d.verdict, Verdict::Deny);
    }

    #[test]
    fn score_thresholds_escalate() {
        // 2 mediums = 8 < 10 -> allow
        let f = [finding("a/1", Severity::Medium), finding("a/2", Severity::Medium)];
        assert_eq!(decide(&f, &params(&[], &[])).verdict, Verdict::Allow);

        // 1 high = 10 >= 10 -> quarantine
        let f = [finding("a/1", Severity::High)];
        let d = decide(&f, &params(&[], &[]));
        assert_eq!(d.verdict, Verdict::Quarantine);
        assert_eq!(d.score, 10);

        // 5 highs = 50 >= 50 -> deny
        let f: Vec<_> = (0..5).map(|i| finding(&format!("a/{i}"), Severity::High)).collect();
        assert_eq!(decide(&f, &params(&[], &[])).verdict, Verdict::Deny);
    }

    #[test]
    fn info_findings_never_gate() {
        let f: Vec<_> = (0..1000).map(|i| finding(&format!("a/{i}"), Severity::Info)).collect();
        let d = decide(&f, &params(&[], &[]));
        assert_eq!(d.verdict, Verdict::Allow);
        assert_eq!(d.score, 0);
    }

    #[test]
    fn level_zero_gates_only_on_hard_denies() {
        // A pile of highs that would deny by score at level 1 passes clean
        // at level 0…
        let f: Vec<_> = (0..5).map(|i| finding(&format!("a/{i}"), Severity::High)).collect();
        let mut p = params(&[], &[]);
        p.level = 0;
        let d = decide(&f, &p);
        assert_eq!(d.verdict, Verdict::Allow);
        // …the score is still reported…
        assert_eq!(d.score, 50);

        // …but a critical finding or a deny_hard match still denies…
        let f = [finding("x/crit", Severity::Critical)];
        assert_eq!(decide(&f, &p).verdict, Verdict::Deny);

        // …and the fail-closed floor is NOT relaxed at level 0.
        let failed = ["yara".to_owned()];
        let mut p0 = params(&[], &failed);
        p0.level = 0;
        assert_eq!(decide(&[], &p0).verdict, Verdict::Quarantine);
    }

    #[test]
    fn level_two_floors_any_high_at_quarantine() {
        // One high scores 10 and already quarantines at defaults, so use a
        // raised threshold to isolate the floor.
        let f = [finding("a/1", Severity::High)];
        let mut p = params(&[], &[]);
        p.quarantine_score = 100;
        assert_eq!(decide(&f, &p).verdict, Verdict::Allow);
        p.level = 2;
        let d = decide(&f, &p);
        assert_eq!(d.verdict, Verdict::Quarantine);
        assert!(d.reasons.iter().any(|r| r.contains("severity floor")));

        // A lone medium does not trip the level-2 floor.
        let f = [finding("a/1", Severity::Medium)];
        assert_eq!(decide(&f, &p).verdict, Verdict::Allow);
    }

    #[test]
    fn level_three_floors_any_medium_at_quarantine() {
        let f = [suspected("a/1", Severity::Medium)];
        let mut p = params(&[], &[]);
        p.quarantine_score = 100;
        p.level = 3;
        // Suspected still trips the floor — quarantine IS the review signal.
        assert_eq!(decide(&f, &p).verdict, Verdict::Quarantine);

        // Lows never trip a floor, even paranoid.
        let f = [finding("a/1", Severity::Low)];
        assert_eq!(decide(&f, &p).verdict, Verdict::Allow);
    }

    #[test]
    fn reasons_explain_every_fired_layer() {
        let deny_hard = ["x/crit".to_owned()];
        let failed = ["yara".to_owned()];
        let f = [finding("x/crit", Severity::Critical)];
        let d = decide(&f, &params(&deny_hard, &failed));
        assert_eq!(d.verdict, Verdict::Deny);
        assert!(d.reasons.iter().any(|r| r.contains("deny_hard")));
        assert!(d.reasons.iter().any(|r| r.contains("critical")));
        assert!(d.reasons.iter().any(|r| r.contains("fail-closed")));
    }
}
