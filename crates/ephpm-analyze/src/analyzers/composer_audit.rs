//! `composer-audit` — known-advisory scan of Composer dependencies.
//!
//! Wraps `composer audit --no-scripts --format=json` as a subprocess (the
//! Composer binary is an **optional external dependency** — absence skips
//! this analyzer). Parses the JSON advisory map into findings, one per
//! advisory, plus `Info` findings for abandoned packages.

use serde_json::Value;

use super::tool::{run_tool, stderr_excerpt};
use crate::analyzer::{AnalysisCtx, Analyzer, AnalyzerError};
use crate::finding::{Category, Finding, Severity};

/// See the module docs.
pub struct ComposerAudit;

/// The analyzer's stable id.
pub const ID: &str = "composer-audit";

fn advisory_severity(advisory: &Value) -> Severity {
    match advisory.get("severity").and_then(Value::as_str) {
        Some("critical") => Severity::Critical,
        Some("high") => Severity::High,
        Some("low") => Severity::Low,
        // Composer omits the field for some sources; a known advisory with
        // unknown severity is at least worth a medium.
        _ => Severity::Medium,
    }
}

fn advisory_finding(package: &str, advisory: &Value) -> Finding {
    let id = advisory
        .get("cve")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| advisory.get("advisoryId").and_then(Value::as_str))
        .unwrap_or("unknown");
    let title = advisory.get("title").and_then(Value::as_str).unwrap_or("known advisory");
    Finding {
        rule_id: format!("{ID}/{id}"),
        severity: advisory_severity(advisory),
        category: Category::SupplyChain,
        path: Some("composer.lock".into()),
        line: None,
        message: format!("{package}: {title}"),
    }
}

/// Parse `composer audit --format=json` output into findings.
fn parse_audit_json(json: &Value) -> Vec<Finding> {
    let mut findings = Vec::new();
    if let Some(advisories) = json.get("advisories").and_then(Value::as_object) {
        for (package, list) in advisories {
            // Each value is a list of advisories (or, from some Composer
            // versions, a map keyed by advisory id).
            match list {
                Value::Array(items) => {
                    findings.extend(items.iter().map(|a| advisory_finding(package, a)));
                }
                Value::Object(map) => {
                    findings.extend(map.values().map(|a| advisory_finding(package, a)));
                }
                _ => {}
            }
        }
    }
    if let Some(abandoned) = json.get("abandoned").and_then(Value::as_object) {
        for (package, replacement) in abandoned {
            let suggestion = replacement
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|r| format!(" (suggested replacement: {r})"))
                .unwrap_or_default();
            findings.push(Finding {
                rule_id: format!("{ID}/abandoned"),
                severity: Severity::Info,
                category: Category::SupplyChain,
                path: Some("composer.lock".into()),
                line: None,
                message: format!("{package} is abandoned{suggestion}"),
            });
        }
    }
    findings
}

impl Analyzer for ComposerAudit {
    fn id(&self) -> &str {
        ID
    }

    fn category(&self) -> Category {
        Category::SupplyChain
    }

    fn run(&self, ctx: &AnalysisCtx) -> Result<Vec<Finding>, AnalyzerError> {
        let root = ctx.root();
        if !root.join("composer.json").is_file() {
            return Err(AnalyzerError::Skipped("no composer.json in target".to_owned()));
        }
        if !root.join("composer.lock").is_file() {
            return Err(AnalyzerError::Skipped(
                "no composer.lock in target (audit needs a lockfile)".to_owned(),
            ));
        }
        let timeout = ctx.config().analyzers.tool_timeout_ms;
        let run = run_tool(
            "composer",
            &["audit", "--no-scripts", "--format=json", "--no-interaction"],
            root,
            timeout,
        )?;
        // `composer audit` exits non-zero when it FINDS advisories, so the
        // exit code alone is not a failure signal — the JSON body is the
        // authority. Only an unparseable body is a real failure.
        match serde_json::from_str::<Value>(&run.stdout) {
            Ok(json) => Ok(parse_audit_json(&json)),
            Err(_) => Err(AnalyzerError::ToolFailed(format!(
                "composer audit produced no JSON (exit {:?}): {}",
                run.exit_code,
                stderr_excerpt(&run.stderr)
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_advisories_and_abandoned() {
        let json: Value = serde_json::from_str(
            r#"{
              "advisories": {
                "acme/widget": [
                  {"advisoryId": "PKSA-x1", "cve": "CVE-2024-1234",
                   "title": "RCE in widget parser", "severity": "critical"},
                  {"advisoryId": "PKSA-x2", "cve": "",
                   "title": "XSS", "severity": "medium"}
                ]
              },
              "abandoned": {"acme/old": "acme/new", "acme/dead": null}
            }"#,
        )
        .unwrap();
        let findings = parse_audit_json(&json);
        assert_eq!(findings.len(), 4);
        let cve = findings.iter().find(|f| f.rule_id == "composer-audit/CVE-2024-1234").unwrap();
        assert_eq!(cve.severity, Severity::Critical);
        assert!(cve.message.contains("acme/widget"));
        // Empty CVE falls back to the advisory id.
        let xss = findings.iter().find(|f| f.rule_id == "composer-audit/PKSA-x2").unwrap();
        assert_eq!(xss.severity, Severity::Medium);
        let abandoned: Vec<_> =
            findings.iter().filter(|f| f.rule_id == "composer-audit/abandoned").collect();
        assert_eq!(abandoned.len(), 2);
        assert!(abandoned.iter().all(|f| f.severity == Severity::Info));
    }

    #[test]
    fn advisory_map_form_is_accepted() {
        // Some Composer versions key advisories by id instead of a list.
        let json: Value = serde_json::from_str(
            r#"{"advisories": {"acme/widget": {"PKSA-x1":
              {"advisoryId": "PKSA-x1", "title": "bug", "severity": "high"}}}}"#,
        )
        .unwrap();
        let findings = parse_audit_json(&json);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::High);
    }

    #[test]
    fn clean_audit_yields_no_findings() {
        let json: Value = serde_json::from_str(r#"{"advisories": {}, "abandoned": {}}"#).unwrap();
        assert!(parse_audit_json(&json).is_empty());
    }
}
