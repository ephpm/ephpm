//! Report serializers: SARIF v2.1.0 JSON and the concise human text form.
//!
//! Both render the same [`AnalysisReport`]; selection is `output:` in the
//! config or `--format` on the CLI.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use serde::Serialize;

use crate::finding::{Confidence, Finding, Severity};
use crate::report::{AnalysisReport, AnalyzerState};

/// SARIF `level` for a severity.
fn sarif_level(severity: Severity) -> &'static str {
    match severity {
        Severity::Info | Severity::Low => "note",
        Severity::Medium => "warning",
        Severity::High | Severity::Critical => "error",
    }
}

/// SARIF `rank` (0.0–100.0, "importance for triage") from confidence:
/// confirmed findings outrank suspected hotspots at equal severity.
fn sarif_rank(confidence: Confidence) -> f64 {
    match confidence {
        Confidence::Suspected => 40.0,
        Confidence::Confirmed => 80.0,
    }
}

#[derive(Serialize)]
struct Sarif {
    #[serde(rename = "$schema")]
    schema: &'static str,
    version: &'static str,
    runs: Vec<SarifRun>,
}

#[derive(Serialize)]
struct SarifRun {
    tool: SarifTool,
    results: Vec<SarifResult>,
    invocations: Vec<SarifInvocation>,
    properties: SarifRunProperties,
}

#[derive(Serialize)]
struct SarifTool {
    driver: SarifDriver,
}

#[derive(Serialize)]
struct SarifDriver {
    name: &'static str,
    #[serde(rename = "informationUri")]
    information_uri: &'static str,
    version: &'static str,
    rules: Vec<SarifRule>,
}

#[derive(Serialize)]
struct SarifRule {
    id: String,
}

#[derive(Serialize)]
struct SarifInvocation {
    #[serde(rename = "executionSuccessful")]
    execution_successful: bool,
    #[serde(rename = "toolExecutionNotifications")]
    tool_execution_notifications: Vec<SarifNotification>,
}

#[derive(Serialize)]
struct SarifNotification {
    level: &'static str,
    message: SarifMessage,
}

#[derive(Serialize)]
struct SarifRunProperties {
    /// The gate's verdict, so CI consumers reading only the SARIF still see
    /// the decision.
    #[serde(rename = "ephpm/verdict")]
    verdict: String,
    #[serde(rename = "ephpm/score")]
    score: u32,
    #[serde(rename = "ephpm/outOfScope")]
    out_of_scope: usize,
    #[serde(rename = "ephpm/waived")]
    waived: usize,
    #[serde(rename = "ephpm/baselineSuppressed")]
    baseline_suppressed: usize,
}

#[derive(Serialize)]
struct SarifResult {
    #[serde(rename = "ruleId")]
    rule_id: String,
    level: &'static str,
    /// Triage importance derived from confidence — see [`sarif_rank`].
    rank: f64,
    message: SarifMessage,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    locations: Vec<SarifLocation>,
    properties: SarifResultProperties,
}

#[derive(Serialize)]
struct SarifResultProperties {
    #[serde(rename = "ephpm/severity")]
    severity: String,
    #[serde(rename = "ephpm/category")]
    category: String,
    #[serde(rename = "ephpm/confidence")]
    confidence: String,
}

#[derive(Serialize)]
struct SarifMessage {
    text: String,
}

#[derive(Serialize)]
struct SarifLocation {
    #[serde(rename = "physicalLocation")]
    physical_location: SarifPhysicalLocation,
}

#[derive(Serialize)]
struct SarifPhysicalLocation {
    #[serde(rename = "artifactLocation")]
    artifact_location: SarifArtifactLocation,
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<SarifRegion>,
}

#[derive(Serialize)]
struct SarifArtifactLocation {
    uri: String,
}

#[derive(Serialize)]
struct SarifRegion {
    #[serde(rename = "startLine")]
    start_line: u64,
}

fn sarif_result(finding: &Finding) -> SarifResult {
    let locations = finding
        .path
        .as_ref()
        .map(|path| {
            vec![SarifLocation {
                physical_location: SarifPhysicalLocation {
                    artifact_location: SarifArtifactLocation {
                        // SARIF wants a URI; forward slashes regardless of
                        // host platform.
                        uri: path.to_string_lossy().replace('\\', "/"),
                    },
                    region: finding.line.map(|line| SarifRegion { start_line: line }),
                },
            }]
        })
        .unwrap_or_default();
    SarifResult {
        rule_id: finding.rule_id.clone(),
        level: sarif_level(finding.severity),
        rank: sarif_rank(finding.confidence),
        message: SarifMessage { text: finding.message.clone() },
        locations,
        properties: SarifResultProperties {
            severity: finding.severity.to_string(),
            category: finding.category.to_string(),
            confidence: finding.confidence.to_string(),
        },
    }
}

/// Render the report as a SARIF v2.1.0 document (one run).
///
/// Skipped analyzers surface as `note` tool-execution notifications and
/// failed analyzers as `error` notifications with
/// `executionSuccessful: false`, so the fail-closed situation is visible in
/// SARIF-only consumers too.
///
/// # Errors
///
/// Only on JSON serialization failure, which for this fully in-memory shape
/// would indicate a bug.
pub fn to_sarif(report: &AnalysisReport) -> anyhow::Result<String> {
    let rules: BTreeSet<&str> = report.findings.iter().map(|f| f.rule_id.as_str()).collect();
    let mut notifications = Vec::new();
    let mut execution_successful = true;
    for status in &report.statuses {
        match &status.state {
            AnalyzerState::Completed { .. } => {}
            AnalyzerState::Skipped { reason } => notifications.push(SarifNotification {
                level: "note",
                message: SarifMessage { text: format!("analyzer {} skipped: {reason}", status.id) },
            }),
            AnalyzerState::Failed { error } => {
                execution_successful = false;
                notifications.push(SarifNotification {
                    level: "error",
                    message: SarifMessage {
                        text: format!("analyzer {} failed: {error}", status.id),
                    },
                });
            }
        }
    }
    let doc = Sarif {
        schema: "https://json.schemastore.org/sarif-2.1.0.json",
        version: "2.1.0",
        runs: vec![SarifRun {
            tool: SarifTool {
                driver: SarifDriver {
                    name: "ephpm-analyze",
                    information_uri: "https://github.com/ephpm/ephpm",
                    version: env!("CARGO_PKG_VERSION"),
                    rules: rules.into_iter().map(|id| SarifRule { id: id.to_owned() }).collect(),
                },
            },
            results: report.findings.iter().map(sarif_result).collect(),
            invocations: vec![SarifInvocation {
                execution_successful,
                tool_execution_notifications: notifications,
            }],
            properties: SarifRunProperties {
                verdict: report.verdict.to_string(),
                score: report.score,
                out_of_scope: report.out_of_scope,
                waived: report.waived,
                baseline_suppressed: report.baseline_suppressed,
            },
        }],
    };
    Ok(serde_json::to_string_pretty(&doc)?)
}

/// Render the report as concise human-readable text.
#[must_use]
pub fn to_text(report: &AnalysisReport) -> String {
    let mut out = String::new();
    for status in &report.statuses {
        match &status.state {
            AnalyzerState::Completed { findings } => {
                let _ = writeln!(out, "  {:<16} completed ({findings} finding(s))", status.id);
            }
            AnalyzerState::Skipped { reason } => {
                let _ = writeln!(out, "  {:<16} skipped: {reason}", status.id);
            }
            AnalyzerState::Failed { error } => {
                let _ = writeln!(out, "  {:<16} FAILED: {error}", status.id);
            }
        }
    }
    if !report.findings.is_empty() {
        let _ = writeln!(out);
    }
    for finding in &report.findings {
        let location = match (&finding.path, finding.line) {
            (Some(path), Some(line)) => format!("{}:{line}", path.display()),
            (Some(path), None) => path.display().to_string(),
            _ => "-".to_owned(),
        };
        // Suspected findings are hotspots (review signals); the marker keeps
        // the common confirmed case visually quiet.
        let suffix = match finding.confidence {
            Confidence::Confirmed => "",
            Confidence::Suspected => " [suspected]",
        };
        let _ = writeln!(
            out,
            "  [{:<8}] {:<32} {location}: {}{suffix}",
            finding.severity, finding.rule_id, finding.message
        );
    }
    let _ = writeln!(out);
    if report.out_of_scope > 0 {
        let _ = writeln!(out, "out of scope: {} finding(s) (diff-aware run)", report.out_of_scope);
    }
    if report.waived > 0 {
        let _ = writeln!(out, "waived:  {} finding(s) by operator suppressions", report.waived);
    }
    if report.baseline_suppressed > 0 {
        let _ =
            writeln!(out, "baseline: {} known finding(s) suppressed", report.baseline_suppressed);
    }
    let _ = writeln!(out, "score:   {}", report.score);
    let _ = writeln!(out, "verdict: {}", report.verdict);
    for reason in &report.reasons {
        let _ = writeln!(out, "  - {reason}");
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::finding::Category;
    use crate::policy::Verdict;
    use crate::report::AnalyzerStatus;

    fn sample_report() -> AnalysisReport {
        AnalysisReport {
            findings: vec![
                Finding {
                    rule_id: "dangerous-sinks/eval".to_owned(),
                    severity: Severity::High,
                    category: Category::Security,
                    path: Some(PathBuf::from("src\\index.php")),
                    line: Some(12),
                    message: "eval() call".to_owned(),
                    confidence: Confidence::Suspected,
                },
                Finding {
                    rule_id: "composer-audit/CVE-2024-1".to_owned(),
                    severity: Severity::Medium,
                    category: Category::SupplyChain,
                    path: None,
                    line: None,
                    message: "vulnerable dependency".to_owned(),
                    confidence: Confidence::Confirmed,
                },
            ],
            statuses: vec![
                AnalyzerStatus {
                    id: "dangerous-sinks".to_owned(),
                    state: AnalyzerState::Completed { findings: 1 },
                },
                AnalyzerStatus {
                    id: "malware-yara".to_owned(),
                    state: AnalyzerState::Skipped { reason: "yara not found".to_owned() },
                },
                AnalyzerStatus {
                    id: "semgrep-php".to_owned(),
                    state: AnalyzerState::Failed { error: "timed out".to_owned() },
                },
            ],
            verdict: Verdict::Quarantine,
            score: 14,
            reasons: vec!["score 14 >= quarantine threshold 10".to_owned()],
            out_of_scope: 3,
            waived: 2,
            baseline_suppressed: 1,
        }
    }

    #[test]
    fn sarif_has_required_shape() {
        let json: serde_json::Value =
            serde_json::from_str(&to_sarif(&sample_report()).unwrap()).unwrap();
        assert_eq!(json["version"], "2.1.0");
        let run = &json["runs"][0];
        assert_eq!(run["tool"]["driver"]["name"], "ephpm-analyze");
        let results = run["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        // High -> error, backslashes normalized to URI form.
        assert_eq!(results[0]["ruleId"], "dangerous-sinks/eval");
        assert_eq!(results[0]["level"], "error");
        let loc = &results[0]["locations"][0]["physicalLocation"];
        assert_eq!(loc["artifactLocation"]["uri"], "src/index.php");
        assert_eq!(loc["region"]["startLine"], 12);
        // Pathless finding has no locations key at all.
        assert!(results[1].get("locations").is_none());
        assert_eq!(results[1]["level"], "warning");
        // Confidence surfaces as rank + a property: suspected hotspots rank
        // below confirmed findings.
        assert_eq!(results[0]["rank"], 40.0);
        assert_eq!(results[0]["properties"]["ephpm/confidence"], "suspected");
        assert_eq!(results[1]["rank"], 80.0);
        assert_eq!(results[1]["properties"]["ephpm/confidence"], "confirmed");
        // Rule index covers both rule ids.
        let rules = run["tool"]["driver"]["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 2);
        // Verdict rides in run properties; failed analyzer flips
        // executionSuccessful and produces an error notification.
        assert_eq!(run["properties"]["ephpm/verdict"], "quarantine");
        assert_eq!(run["properties"]["ephpm/outOfScope"], 3);
        assert_eq!(run["properties"]["ephpm/waived"], 2);
        assert_eq!(run["properties"]["ephpm/baselineSuppressed"], 1);
        let invocation = &run["invocations"][0];
        assert_eq!(invocation["executionSuccessful"], false);
        let notes = invocation["toolExecutionNotifications"].as_array().unwrap();
        assert_eq!(notes.len(), 2);
    }

    #[test]
    fn text_reports_statuses_findings_and_verdict() {
        let text = to_text(&sample_report());
        assert!(text.contains("dangerous-sinks"));
        assert!(text.contains("skipped: yara not found"));
        assert!(text.contains("FAILED: timed out"));
        assert!(text.contains("verdict: quarantine"));
        assert!(text.contains("score:   14"));
        assert!(text.contains("quarantine threshold"));
        // Confidence marker only on the suspected finding.
        assert!(text.contains("eval() call [suspected]"));
        assert!(text.contains("vulnerable dependency\n"));
        // Post-processing counters.
        assert!(text.contains("out of scope: 3"));
        assert!(text.contains("waived:  2"));
        assert!(text.contains("baseline: 1 known finding(s)"));
    }

    #[test]
    fn text_omits_zero_counters() {
        let mut report = sample_report();
        report.out_of_scope = 0;
        report.waived = 0;
        report.baseline_suppressed = 0;
        let text = to_text(&report);
        assert!(!text.contains("out of scope"));
        assert!(!text.contains("waived"));
        assert!(!text.contains("baseline:"));
    }
}
