//! `phpmd` — mess-detector heuristics via PHP Mess Detector.
//!
//! Wraps PHPMD as a subprocess (PHPMD is an **optional external dependency**
//! — absence skips this analyzer, the graceful-degradation half of the
//! fail-closed model). The binary is resolved first from a project-local
//! Composer install (`vendor/bin/phpmd`) and otherwise from `PATH`; when
//! neither is present the shared runner returns [`AnalyzerError::Skipped`].
//!
//! **CLI + output (verified against `phpmd/phpmd`,
//! `src/main/php/PHPMD/TextUI/CommandLineOptions.php` and
//! `.../Renderer/JSONRenderer.php`, Sept 2026).** PHPMD takes its arguments
//! **positionally**, *not* as flags — the historical
//! `phpmd <path> <report-format> <ruleset>` order (there is no
//! `--report-format` option). We run `phpmd . json <ruleset>` with the cwd set
//! to the analyzed root. The JSON renderer emits `{version, package,
//! timestamp, files: [ … ], errors: [ … ]}`, where **`files` is an array**
//! (not a map) of `{file, violations: [ … ]}`, and each violation carries
//! `beginLine`, `endLine`, `rule`, `ruleSet`, `priority` (1 = highest),
//! `description`, and the enclosing `class`/`method`/`function`/`package`.
//! A top-level `errors` array (`{fileName, message}`) lists files PHPMD could
//! not process.
//!
//! PHPMD exits non-zero when it *finds* violations (2) as well as on a real
//! error (1), so — like `progpilot` — the exit code is not a failure signal;
//! the JSON body is authoritative. Two things still gate (fail-closed): an
//! unparseable body (PHPMD broke before rendering), and a non-empty top-level
//! `errors` array (files that failed to parse — an analysis that could not
//! read part of the tree must not look clean, mirroring `phpstan`'s handling
//! of its top-level `errors`).
//!
//! **Mapping.** `priority = 1` → [`Severity::Medium`], any lower priority →
//! [`Severity::Low`] — PHPMD's own ranking, not an invented one.
//! `rule_id = phpmd/<rule>`, [`Category::Quality`]. Findings are
//! [`Confidence::Suspected`]: PHPMD is a heuristic metrics/smell detector
//! (long methods, unused variables, complexity), so its hits are review
//! signals, not proven defects.
//!
//! This analyzer is **not** in the default `security` profile — it is opt-in
//! via `analyzers.enable: [phpmd]`.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::tool::{run_tool, stderr_excerpt};
use crate::analyzer::{AnalysisCtx, Analyzer, AnalyzerError};
use crate::finding::{Category, Confidence, Finding, Severity};

/// See the module docs.
pub struct Phpmd;

/// The analyzer's stable id.
pub const ID: &str = "phpmd";

/// The default ruleset when `analyzers.phpmd_ruleset` is unset — the full set
/// of built-in PHPMD rulesets except the noisiest edge cases, expressed as the
/// comma-separated positional argument PHPMD expects.
const DEFAULT_RULESET: &str = "cleancode,codesize,controversial,design,naming,unusedcode";

/// Resolve the PHPMD program name for [`run_tool`]. Prefers a project-local
/// `vendor/bin/phpmd` (a `php` script on Unix) over a global `phpmd` on
/// `PATH`. On Windows the extensionless launcher is not spawnable via
/// `CreateProcess`, so the bare `phpmd` name is used and the shared runner
/// resolves its `.bat`/`.cmd` form.
fn resolve_binary(root: &Path) -> String {
    if !cfg!(windows) {
        let vendored = root.join("vendor").join("bin").join("phpmd");
        if vendored.is_file() {
            return vendored.to_string_lossy().into_owned();
        }
    }
    "phpmd".to_owned()
}

/// Severity from a PHPMD `priority`: 1 (highest) → medium, anything lower →
/// low. PHPMD's priorities are relative smell weights, not exploit severity.
fn severity_for(priority: Option<u64>) -> Severity {
    match priority {
        Some(1) => Severity::Medium,
        _ => Severity::Low,
    }
}

/// Parse a PHPMD JSON document into findings.
///
/// # Errors
///
/// [`AnalyzerError::ToolFailed`] when the top-level `errors` array is
/// non-empty — those are files PHPMD could not process, and a run that could
/// not read part of the tree must gate, not report clean.
fn parse_phpmd_json(doc: &Value) -> Result<Vec<Finding>, AnalyzerError> {
    if let Some(errors) = doc.get("errors").and_then(Value::as_array)
        && !errors.is_empty()
    {
        let first = errors
            .iter()
            .find_map(|e| e.get("message").and_then(Value::as_str))
            .unwrap_or("unknown processing error");
        return Err(AnalyzerError::ToolFailed(format!(
            "phpmd could not process part of the tree: {}",
            first.chars().take(200).collect::<String>()
        )));
    }

    let mut findings = Vec::new();
    let Some(files) = doc.get("files").and_then(Value::as_array) else {
        return Ok(findings);
    };
    for file in files {
        let path = file.get("file").and_then(Value::as_str);
        let Some(violations) = file.get("violations").and_then(Value::as_array) else {
            continue;
        };
        for violation in violations {
            let rule = violation
                .get("rule")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("unknown");
            let description = violation
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("phpmd violation")
                .to_owned();
            findings.push(Finding {
                rule_id: format!("{ID}/{rule}"),
                severity: severity_for(violation.get("priority").and_then(Value::as_u64)),
                category: Category::Quality,
                path: path.map(PathBuf::from),
                line: violation.get("beginLine").and_then(Value::as_u64),
                message: description,
                // Heuristic smell/metrics detector — a review signal.
                confidence: Confidence::Suspected,
            });
        }
    }
    Ok(findings)
}

/// Rewrite each finding's path relative to `root` when it is an absolute path
/// under `root` (PHPMD emits absolute paths); leave others untouched.
fn relativize(findings: &mut [Finding], root: &Path) {
    for finding in findings {
        if let Some(path) = &finding.path
            && let Ok(rel) = path.strip_prefix(root)
        {
            finding.path = Some(rel.to_path_buf());
        }
    }
}

impl Analyzer for Phpmd {
    fn id(&self) -> &str {
        ID
    }

    fn category(&self) -> Category {
        Category::Quality
    }

    fn run(&self, ctx: &AnalysisCtx) -> Result<Vec<Finding>, AnalyzerError> {
        let root = ctx.root();
        let timeout = ctx.config().analyzers.tool_timeout_ms;
        let program = resolve_binary(root);

        let ruleset = ctx.config().analyzers.phpmd_ruleset.as_deref().unwrap_or(DEFAULT_RULESET);
        // Positional argv: <path> <report-format> <ruleset>. cwd is the root,
        // so `.` is the target.
        let args = [".", "json", ruleset];
        let run = run_tool(&program, &args, root, timeout)?;

        // Exit code is not a failure signal (non-zero on any violation) — the
        // JSON body is. An unparseable body is a real failure; a parseable body
        // with a non-empty top-level `errors` array gates inside the parser.
        match serde_json::from_str::<Value>(&run.stdout) {
            Ok(doc) => {
                let mut findings = parse_phpmd_json(&doc)?;
                relativize(&mut findings, root);
                Ok(findings)
            }
            Err(_) => Err(AnalyzerError::ToolFailed(format!(
                "phpmd produced no JSON (exit {:?}): {}",
                run.exit_code,
                stderr_excerpt(&run.stderr)
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A captured `phpmd . json <ruleset>` body: one file with a priority-1
    /// and a priority-3 violation, empty `errors`.
    const SAMPLE: &str = r#"{
      "version": "2.15.0",
      "package": "phpmd",
      "timestamp": "2026-09-12T00:00:00+00:00",
      "files": [
        {
          "file": "/srv/app/src/Service.php",
          "violations": [
            {"beginLine": 40, "endLine": 95, "package": "App", "function": "",
             "class": "Service", "method": "handle",
             "description": "The method handle() has an NPath complexity of 2400.",
             "rule": "NPathComplexity", "ruleSet": "Code Size Rules",
             "externalInfoUrl": "https://phpmd.org/rules/codesize.html", "priority": 1},
            {"beginLine": 12, "endLine": 12, "package": "App",
             "class": "Service", "method": "handle",
             "description": "Avoid unused local variables such as '$tmp'.",
             "rule": "UnusedLocalVariable", "ruleSet": "Unused Code Rules",
             "externalInfoUrl": "https://phpmd.org/rules/unusedcode.html", "priority": 3}
          ]
        }
      ],
      "errors": []
    }"#;

    #[test]
    fn parses_violations_with_priority_severity_and_suspected_confidence() {
        let doc: Value = serde_json::from_str(SAMPLE).unwrap();
        let mut findings = parse_phpmd_json(&doc).unwrap();
        relativize(&mut findings, Path::new("/srv/app"));
        assert_eq!(findings.len(), 2);

        let by_rule = |id: &str| findings.iter().find(|f| f.rule_id == id).unwrap();

        // priority 1 → medium; everything is quality + suspected; path relative.
        let npath = by_rule("phpmd/NPathComplexity");
        assert_eq!(npath.severity, Severity::Medium);
        assert_eq!(npath.category, Category::Quality);
        assert_eq!(npath.confidence, Confidence::Suspected);
        assert_eq!(npath.path.as_deref(), Some(Path::new("src/Service.php")));
        assert_eq!(npath.line, Some(40));
        assert!(npath.message.contains("NPath complexity"));

        // priority 3 → low.
        let unused = by_rule("phpmd/UnusedLocalVariable");
        assert_eq!(unused.severity, Severity::Low);
        assert_eq!(unused.line, Some(12));
    }

    #[test]
    fn empty_files_means_no_findings() {
        let doc: Value = serde_json::from_str(r#"{"files": [], "errors": []}"#).unwrap();
        assert!(parse_phpmd_json(&doc).unwrap().is_empty());
    }

    #[test]
    fn top_level_errors_gate_instead_of_reporting_clean() {
        // A file PHPMD could not parse: it lists the file under `errors`. A run
        // that could not read part of the tree must gate, never look clean.
        let doc: Value = serde_json::from_str(
            r#"{"files": [], "errors": [{"fileName": "/srv/app/broken.php",
                "message": "Unexpected token in broken.php"}]}"#,
        )
        .unwrap();
        assert!(matches!(parse_phpmd_json(&doc), Err(AnalyzerError::ToolFailed(_))));
    }

    #[test]
    fn missing_rule_falls_back_to_unknown() {
        let doc: Value = serde_json::from_str(
            r#"{"files": [{"file": "a.php", "violations": [
                {"beginLine": 1, "description": "x", "priority": 2}]}], "errors": []}"#,
        )
        .unwrap();
        let findings = parse_phpmd_json(&doc).unwrap();
        assert_eq!(findings[0].rule_id, "phpmd/unknown");
        assert_eq!(findings[0].severity, Severity::Low);
    }
}
