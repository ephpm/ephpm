//! `semgrep-php` — pattern-based PHP security scan.
//!
//! Wraps `semgrep --config <ref> --sarif` as a subprocess (Semgrep is an
//! **optional external dependency** — absence skips this analyzer; the
//! default `p/php` registry ref needs network access, and a fetch failure is
//! a real error, which gates). The SARIF it emits is parsed back into
//! findings.

use serde_json::Value;

use super::tool::{run_tool, stderr_excerpt};
use crate::analyzer::{AnalysisCtx, Analyzer, AnalyzerError};
use crate::finding::{Category, Confidence, Finding, Severity};

/// See the module docs.
pub struct SemgrepPhp;

/// The analyzer's stable id.
pub const ID: &str = "semgrep-php";

fn severity_from_level(level: Option<&str>) -> Severity {
    match level {
        Some("error") => Severity::High,
        Some("note") => Severity::Low,
        // Semgrep's default level, and the safe middle for anything odd.
        _ => Severity::Medium,
    }
}

/// Parse a SARIF document produced by Semgrep into findings.
fn parse_sarif(doc: &Value) -> Result<Vec<Finding>, AnalyzerError> {
    let runs = doc
        .get("runs")
        .and_then(Value::as_array)
        .ok_or_else(|| AnalyzerError::Parse("SARIF document has no runs".to_owned()))?;
    let mut findings = Vec::new();
    for run in runs {
        let Some(results) = run.get("results").and_then(Value::as_array) else {
            continue;
        };
        for result in results {
            let rule = result.get("ruleId").and_then(Value::as_str).unwrap_or("unknown-rule");
            let message = result
                .pointer("/message/text")
                .and_then(Value::as_str)
                .unwrap_or("semgrep finding")
                .to_owned();
            let location = result.pointer("/locations/0/physicalLocation");
            let path = location
                .and_then(|l| l.pointer("/artifactLocation/uri"))
                .and_then(Value::as_str)
                .map(Into::into);
            let line =
                location.and_then(|l| l.pointer("/region/startLine")).and_then(Value::as_u64);
            findings.push(Finding {
                rule_id: format!("{ID}/{rule}"),
                severity: severity_from_level(result.get("level").and_then(Value::as_str)),
                category: Category::Security,
                path,
                line,
                message,
                // Pattern matches carry false positives by nature — review
                // signals (hotspots), not confirmed evidence.
                confidence: Confidence::Suspected,
            });
        }
    }
    Ok(findings)
}

impl Analyzer for SemgrepPhp {
    fn id(&self) -> &str {
        ID
    }

    fn category(&self) -> Category {
        Category::Security
    }

    fn run(&self, ctx: &AnalysisCtx) -> Result<Vec<Finding>, AnalyzerError> {
        let config = &ctx.config().analyzers.semgrep_config;
        let timeout = ctx.config().analyzers.tool_timeout_ms;
        // `--disable-nosem`: Semgrep honors inline `// nosemgrep` comments
        // by default — a tenant-controlled mute. Suppressions must come only
        // from operator config (see `crate::suppress`), and the
        // `suppression-scan` analyzer flags the comment itself; honoring it
        // here would hollow both out.
        let run = run_tool(
            "semgrep",
            &[
                "--config",
                config,
                "--sarif",
                "--quiet",
                "--disable-version-check",
                "--disable-nosem",
                ".",
            ],
            ctx.root(),
            timeout,
        )?;
        // Semgrep exits 0 whether or not it has findings; non-zero means the
        // scan itself broke (bad config ref, registry unreachable, crash).
        // That is a real failure — fail closed — unless it still produced a
        // parseable SARIF body, which is then authoritative.
        match serde_json::from_str::<Value>(&run.stdout) {
            Ok(doc) => parse_sarif(&doc),
            Err(_) => Err(AnalyzerError::ToolFailed(format!(
                "semgrep produced no SARIF (exit {:?}): {}",
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
    fn parses_semgrep_sarif_results() {
        let doc: Value = serde_json::from_str(
            r#"{"version": "2.1.0", "runs": [{"results": [
              {"ruleId": "php.lang.security.eval-use",
               "level": "error",
               "message": {"text": "eval of user input"},
               "locations": [{"physicalLocation": {
                 "artifactLocation": {"uri": "src/handler.php"},
                 "region": {"startLine": 42}}}]},
              {"ruleId": "php.lang.best-practice.x",
               "level": "note",
               "message": {"text": "style nit"},
               "locations": []}
            ]}]}"#,
        )
        .unwrap();
        let findings = parse_sarif(&doc).unwrap();
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].rule_id, "semgrep-php/php.lang.security.eval-use");
        assert_eq!(findings[0].severity, Severity::High);
        assert_eq!(findings[0].path.as_deref(), Some(std::path::Path::new("src/handler.php")));
        assert_eq!(findings[0].line, Some(42));
        assert_eq!(findings[1].severity, Severity::Low);
        assert_eq!(findings[1].path, None);
    }

    #[test]
    fn sarif_without_runs_is_a_parse_error() {
        let doc: Value = serde_json::from_str("{}").unwrap();
        assert!(matches!(parse_sarif(&doc), Err(AnalyzerError::Parse(_))));
    }
}
