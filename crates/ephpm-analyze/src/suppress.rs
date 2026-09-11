//! Operator-side suppressions — the **only** way a finding is waived.
//!
//! Suppressions come exclusively from `.ephpm-analyze.yml` (`suppress:`),
//! which the *operator* controls. Nothing in the pipeline ever reads
//! suppression directives out of the analyzed code: an inline
//! `// ephpm-analyze-ignore`, `@phpstan-ignore`, `// nosemgrep`, or similar
//! comment authored by the tenant is **never honored** — instead the
//! `suppression-scan` analyzer flags it as a finding
//! (`suppression-scan/tenant-suppression-attempt`), because an attempt to
//! silence the scanner is itself a signal. This module is the honoring half;
//! `crate::analyzers::suppression_scan` is the detecting half.

use crate::baseline::normalized_path;
use crate::config::SuppressRule;
use crate::finding::Finding;
use crate::policy::deny_hard_matches;

/// Whether `rule` waives `finding`: the rule id matches exactly or as a
/// `/`-prefix (same semantics as `analyzers.deny_hard`), and — when the rule
/// names a path — the finding is anchored at exactly that `/`-normalized
/// tree-relative path. A path-scoped rule never waives a pathless finding.
#[must_use]
pub fn rule_matches(rule: &SuppressRule, finding: &Finding) -> bool {
    if !deny_hard_matches(&rule.rule, &finding.rule_id) {
        return false;
    }
    match &rule.path {
        None => true,
        Some(rule_path) => {
            normalized_path(finding).is_some_and(|p| p == rule_path.replace('\\', "/"))
        }
    }
}

/// Drop findings waived by `rules`, returning the survivors and the number
/// waived.
#[must_use]
pub fn apply(findings: Vec<Finding>, rules: &[SuppressRule]) -> (Vec<Finding>, usize) {
    if rules.is_empty() {
        return (findings, 0);
    }
    let before = findings.len();
    let kept: Vec<Finding> = findings
        .into_iter()
        .filter(|f| {
            let waived = rules.iter().find(|r| rule_matches(r, f));
            if let Some(rule) = waived {
                tracing::debug!(
                    rule_id = %f.rule_id,
                    reason = %rule.reason,
                    "finding waived by operator suppression"
                );
            }
            waived.is_none()
        })
        .collect();
    let waived = before - kept.len();
    (kept, waived)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::finding::{Category, Confidence, Severity};

    fn finding(rule_id: &str, path: Option<&str>) -> Finding {
        Finding {
            rule_id: rule_id.to_owned(),
            severity: Severity::High,
            category: Category::Security,
            path: path.map(PathBuf::from),
            line: Some(1),
            message: "m".to_owned(),
            confidence: Confidence::Confirmed,
        }
    }

    fn rule(rule: &str, path: Option<&str>) -> SuppressRule {
        SuppressRule {
            rule: rule.to_owned(),
            path: path.map(str::to_owned),
            reason: "vetted".to_owned(),
        }
    }

    #[test]
    fn pathless_rule_waives_at_any_path() {
        let rules = [rule("x/r", None)];
        let (kept, waived) =
            apply(vec![finding("x/r", Some("a.php")), finding("x/other", Some("a.php"))], &rules);
        assert_eq!(waived, 1);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].rule_id, "x/other");
    }

    #[test]
    fn path_scoped_rule_waives_only_that_path() {
        let rules = [rule("x/r", Some("legacy/a.php"))];
        let findings = vec![
            finding("x/r", Some("legacy/a.php")),
            finding("x/r", Some("legacy\\a.php")), // Windows separators
            finding("x/r", Some("src/b.php")),
            finding("x/r", None), // pathless never matches a path-scoped rule
        ];
        let (kept, waived) = apply(findings, &rules);
        assert_eq!(waived, 2);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn prefix_rule_waives_whole_analyzer_namespace() {
        let rules = [rule("noisy-analyzer", None)];
        let (kept, waived) = apply(
            vec![finding("noisy-analyzer/a", None), finding("noisy-analyzer2/a", None)],
            &rules,
        );
        assert_eq!(waived, 1);
        assert_eq!(kept[0].rule_id, "noisy-analyzer2/a");
    }

    #[test]
    fn no_rules_is_a_no_op() {
        let (kept, waived) = apply(vec![finding("x/r", None)], &[]);
        assert_eq!(waived, 0);
        assert_eq!(kept.len(), 1);
    }
}
