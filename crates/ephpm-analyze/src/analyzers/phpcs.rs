//! `phpcs` — coding-standard analysis via PHP_CodeSniffer.
//!
//! Wraps `phpcs --report=json -q .` as a subprocess (PHP_CodeSniffer is an
//! **optional external dependency** — absence skips this analyzer, the
//! graceful-degradation half of the fail-closed model). The binary is
//! resolved first from a project-local Composer install (`vendor/bin/phpcs`)
//! and otherwise from `PATH`; when neither is present the shared runner
//! returns [`AnalyzerError::Skipped`], never a crash.
//!
//! **CLI + output (verified against `PHPCSStandards/PHP_CodeSniffer`,
//! `src/Reports/Json.php`, Sept 2026).** `--report=json` prints a document to
//! stdout with two top-level keys: `totals` (`{errors, warnings, fixable}`)
//! and `files` — a **map** keyed by file path, each value
//! `{errors, warnings, messages: [ … ]}`. Each message carries `line`,
//! `column`, `type` (`"ERROR"` / `"WARNING"`), `source` (the sniff code, e.g.
//! `Generic.Files.LineLength`), `message`, `severity`, and `fixable`. `-q`
//! suppresses the progress/summary so only the JSON reaches stdout. PHPCS
//! exits non-zero whenever it finds anything (1 = violations, 2 = with
//! fixables, 3 = processing error), so — like `composer-audit` / `phpstan` —
//! the exit code is not itself a failure signal; the JSON body is the
//! authority, and an unparseable body is a real failure that gates.
//!
//! **Mapping.** `type = ERROR` → [`Severity::Medium`], `WARNING` →
//! [`Severity::Low`] (PHPCS is a style/quality linter; its "error" is a
//! standards violation, not an exploit). `rule_id = phpcs/<source>`. The
//! category is [`Category::Quality`] for the ordinary style/robustness sniff,
//! upgraded to [`Category::Security`] when the sniff's `source` names a
//! `Security` segment (e.g. `WordPress.Security.EscapeOutput`, the security
//! sniffs shipped by the WordPress / VIP standards) — a cosmetic label only,
//! since category does not drive the gate (severity + confidence do).
//! Findings are [`Confidence::Confirmed`]: PHPCS reports a concrete violation
//! at a concrete line, not a heuristic hotspot.
//!
//! This analyzer is **not** in the default `security` profile — it is opt-in
//! via `analyzers.enable: [phpcs]`, so the out-of-the-box gate behaviour is
//! unchanged.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::tool::{run_tool, stderr_excerpt};
use crate::analyzer::{AnalysisCtx, Analyzer, AnalyzerError};
use crate::finding::{Category, Confidence, Finding, Severity};

/// See the module docs.
pub struct Phpcs;

/// The analyzer's stable id.
pub const ID: &str = "phpcs";

/// Resolve the PHP_CodeSniffer program name for [`run_tool`].
///
/// Prefers a project-local Composer install at `vendor/bin/phpcs` (a
/// `#!/usr/bin/env php` script, spawned directly on Unix); falls back to
/// `phpcs` on `PATH`, where the shared runner also handles the Windows
/// `.bat`/`.cmd` launcher forms. A vendored Windows `.bat` is not spawned by
/// absolute path (that is [`run_tool`]'s PATH-only concern), so on Windows a
/// globally installed `phpcs` is used instead — a clean fallback, never a
/// crash.
fn resolve_binary(root: &Path) -> String {
    if !cfg!(windows) {
        let vendored = root.join("vendor").join("bin").join("phpcs");
        if vendored.is_file() {
            return vendored.to_string_lossy().into_owned();
        }
    }
    "phpcs".to_owned()
}

/// Severity from a PHPCS message `type`: `ERROR` → medium, everything else
/// (`WARNING`, or an unexpected value) → low. PHPCS ranks nothing higher: a
/// coding-standards violation is never critical on its own.
fn severity_for(msg_type: Option<&str>) -> Severity {
    match msg_type {
        Some(t) if t.eq_ignore_ascii_case("error") => Severity::Medium,
        _ => Severity::Low,
    }
}

/// Category for a sniff `source`: [`Category::Security`] when a dot-delimited
/// segment is `Security` (the convention the WordPress / VIP security sniffs
/// follow, e.g. `WordPress.Security.EscapeOutput`), else [`Category::Quality`].
fn category_for(source: &str) -> Category {
    if source.split('.').any(|seg| seg.eq_ignore_ascii_case("security")) {
        Category::Security
    } else {
        Category::Quality
    }
}

/// Parse a `phpcs --report=json` document into findings. Paths are kept as
/// PHPCS reports them here; [`relativize`] normalizes them against the root.
fn parse_phpcs_json(doc: &Value) -> Vec<Finding> {
    let mut findings = Vec::new();
    let Some(files) = doc.get("files").and_then(Value::as_object) else {
        return findings;
    };
    for (path, entry) in files {
        let Some(messages) = entry.get("messages").and_then(Value::as_array) else {
            continue;
        };
        for message in messages {
            let source = message
                .get("source")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("unknown");
            let text = message
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("phpcs finding")
                .to_owned();
            let msg_type = message.get("type").and_then(Value::as_str);
            findings.push(Finding {
                rule_id: format!("{ID}/{source}"),
                severity: severity_for(msg_type),
                category: category_for(source),
                path: Some(PathBuf::from(path)),
                line: message.get("line").and_then(Value::as_u64),
                message: text,
                confidence: Confidence::Confirmed,
            });
        }
    }
    findings
}

/// Rewrite each finding's path relative to `root` when it is an absolute path
/// under `root` (PHPCS emits absolute realpaths); leave others untouched.
fn relativize(findings: &mut [Finding], root: &Path) {
    for finding in findings {
        if let Some(path) = &finding.path
            && let Ok(rel) = path.strip_prefix(root)
        {
            finding.path = Some(rel.to_path_buf());
        }
    }
}

impl Analyzer for Phpcs {
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

        // `.` because the cwd is the analyzed root (matching `phpstan` /
        // `semgrep-php`). `-q` keeps everything but the JSON off stdout.
        let mut args: Vec<String> = vec!["--report=json".to_owned(), "-q".to_owned()];
        if let Some(standard) = &ctx.config().analyzers.phpcs_standard {
            args.push(format!("--standard={standard}"));
        }
        args.push(".".to_owned());

        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let run = run_tool(&program, &arg_refs, root, timeout)?;

        // Exit code is not a failure signal (PHPCS exits non-zero on any
        // finding) — the JSON body is. An unparseable body is a real failure.
        match serde_json::from_str::<Value>(&run.stdout) {
            Ok(doc) => {
                let mut findings = parse_phpcs_json(&doc);
                relativize(&mut findings, root);
                Ok(findings)
            }
            Err(_) => Err(AnalyzerError::ToolFailed(format!(
                "phpcs produced no JSON (exit {:?}): {}",
                run.exit_code,
                stderr_excerpt(&run.stderr)
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A captured `phpcs --report=json` body: two files, an ERROR and a
    /// WARNING, plus one message whose sniff `source` names a `Security`
    /// segment.
    const SAMPLE: &str = r#"{
      "totals": {"errors": 2, "warnings": 1, "fixable": 1},
      "files": {
        "/srv/app/src/Foo.php": {
          "errors": 1,
          "warnings": 1,
          "messages": [
            {"message": "Line exceeds 120 characters.", "source": "Generic.Files.LineLength.TooLong",
             "severity": 5, "fixable": false, "type": "WARNING", "line": 12, "column": 121},
            {"message": "Expected 1 space after comma.", "source": "Squiz.Functions.FunctionDeclarationArgumentSpacing.NoSpaceBeforeComma",
             "severity": 5, "fixable": true, "type": "ERROR", "line": 7, "column": 30}
          ]
        },
        "/srv/app/src/View.php": {
          "errors": 1,
          "warnings": 0,
          "messages": [
            {"message": "Output should be escaped.", "source": "WordPress.Security.EscapeOutput.OutputNotEscaped",
             "severity": 5, "fixable": false, "type": "ERROR", "line": 3, "column": 10}
          ]
        }
      }
    }"#;

    #[test]
    fn parses_error_and_warning_with_severity_and_category_mapping() {
        let doc: Value = serde_json::from_str(SAMPLE).unwrap();
        let mut findings = parse_phpcs_json(&doc);
        relativize(&mut findings, Path::new("/srv/app"));
        assert_eq!(findings.len(), 3);

        let by_rule = |id: &str| findings.iter().find(|f| f.rule_id == id).unwrap();

        // WARNING → low, quality; path relativized; rule id is phpcs/<source>.
        let warn = by_rule("phpcs/Generic.Files.LineLength.TooLong");
        assert_eq!(warn.severity, Severity::Low);
        assert_eq!(warn.category, Category::Quality);
        assert_eq!(warn.confidence, Confidence::Confirmed);
        assert_eq!(warn.path.as_deref(), Some(Path::new("src/Foo.php")));
        assert_eq!(warn.line, Some(12));

        // ERROR → medium.
        let err =
            by_rule("phpcs/Squiz.Functions.FunctionDeclarationArgumentSpacing.NoSpaceBeforeComma");
        assert_eq!(err.severity, Severity::Medium);
        assert_eq!(err.category, Category::Quality);
        assert_eq!(err.line, Some(7));

        // A `.Security.` sniff is categorized Security.
        let sec = by_rule("phpcs/WordPress.Security.EscapeOutput.OutputNotEscaped");
        assert_eq!(sec.severity, Severity::Medium);
        assert_eq!(sec.category, Category::Security);
        assert_eq!(sec.path.as_deref(), Some(Path::new("src/View.php")));
    }

    #[test]
    fn clean_run_yields_no_findings() {
        let doc: Value = serde_json::from_str(
            r#"{"totals": {"errors": 0, "warnings": 0, "fixable": 0}, "files": {}}"#,
        )
        .unwrap();
        assert!(parse_phpcs_json(&doc).is_empty());
    }

    #[test]
    fn missing_source_falls_back_to_unknown() {
        let doc: Value = serde_json::from_str(
            r#"{"files": {"a.php": {"messages": [{"message": "x", "type": "ERROR", "line": 1}]}}}"#,
        )
        .unwrap();
        let findings = parse_phpcs_json(&doc);
        assert_eq!(findings[0].rule_id, "phpcs/unknown");
        assert_eq!(findings[0].severity, Severity::Medium);
    }
}
