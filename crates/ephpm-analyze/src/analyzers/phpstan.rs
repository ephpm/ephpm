//! `phpstan` — full static analysis of PHP via PHPStan.
//!
//! Wraps `phpstan analyse --error-format=json` as a subprocess (PHPStan is an
//! **optional external dependency** — absence skips this analyzer, the
//! graceful-degradation half of the fail-closed model). The binary is
//! resolved first from a project-local Composer install
//! (`vendor/bin/phpstan`) and otherwise from `PATH`; when neither is present
//! the shared runner returns [`AnalyzerError::Skipped`], never a crash.
//!
//! PHPStan's JSON has three parts: a `totals` block, a `files` map
//! (path → `{ messages: [ { message, line, identifier, ignorable } ] }`), and
//! a top-level `errors` array of **non-file** diagnostics — configuration
//! problems and internal errors. Per-file messages become [`Finding`]s;
//! a non-empty top-level `errors` array is a *broken run* and gates (a PHPStan
//! that could not analyse must never look clean).
//!
//! Findings are `confidence = Confirmed`: PHPStan is a real static analyzer
//! reasoning over types, not a heuristic token scan. They are
//! `category = Quality` — the [`Category::Quality`] variant is documented as
//! "correctness / robustness problems that are not directly exploitable",
//! which is exactly what a PHPStan report is (no `Correctness` variant exists,
//! and `Quality` already means precisely this). Severity is a flat `Medium`:
//! PHPStan reports bugs, but it does not itself rank them, so the analyzer
//! does not invent a ranking.
//!
//! This analyzer is **not** in the default `security` profile — it is opt-in
//! via `analyzers.enable: [phpstan]`, so the out-of-the-box gate behaviour is
//! unchanged.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::tool::{run_tool, stderr_excerpt};
use crate::analyzer::{AnalysisCtx, Analyzer, AnalyzerError};
use crate::finding::{Category, Confidence, Finding, Severity};

/// See the module docs.
pub struct PhpStan;

/// The analyzer's stable id.
pub const ID: &str = "phpstan";

/// Resolve the PHPStan program name for [`run_tool`].
///
/// Prefers a project-local Composer install at `vendor/bin/phpstan` (the file
/// carries a `#!/usr/bin/env php` shebang and spawns directly on Unix); falls
/// back to `phpstan` on `PATH`, where the shared runner also handles the
/// Windows `.bat`/`.cmd` launcher forms. A vendored Windows `.bat` is not
/// spawned by absolute path (that is [`run_tool`]'s PATH-only concern), so on
/// Windows a globally installed `phpstan` is used instead — a clean fallback,
/// never a crash.
fn resolve_binary(root: &Path) -> String {
    let vendored = root.join("vendor").join("bin").join("phpstan");
    if vendored.is_file() {
        return vendored.to_string_lossy().into_owned();
    }
    "phpstan".to_owned()
}

/// Build the `rule_id` for one message: `phpstan/<identifier>` when PHPStan
/// emits an `identifier` (1.11+; e.g. `method.notFound`), else the stable
/// fallback `phpstan/analyse` (PHPStan does not otherwise carry rule ids).
fn rule_id_for(message: &Value) -> String {
    match message.get("identifier").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        Some(identifier) => format!("{ID}/{identifier}"),
        None => format!("{ID}/analyse"),
    }
}

/// Parse a `phpstan analyse --error-format=json` document into findings.
///
/// # Errors
///
/// [`AnalyzerError::ToolFailed`] when the top-level `errors` array is
/// non-empty — those are configuration/internal failures, and a run that
/// could not analyse must gate, not report clean.
fn parse_phpstan_json(doc: &Value) -> Result<Vec<Finding>, AnalyzerError> {
    // Top-level `errors` are non-file diagnostics (bad config, internal
    // error). Any such entry means the analysis itself broke — fail closed.
    if let Some(errors) = doc.get("errors").and_then(Value::as_array) {
        if let Some(first) = errors.iter().find_map(Value::as_str) {
            return Err(AnalyzerError::ToolFailed(format!(
                "phpstan reported a non-file error: {}",
                first.chars().take(200).collect::<String>()
            )));
        }
        if !errors.is_empty() {
            return Err(AnalyzerError::ToolFailed("phpstan reported non-file errors".to_owned()));
        }
    }

    let mut findings = Vec::new();
    let Some(files) = doc.get("files").and_then(Value::as_object) else {
        return Ok(findings);
    };
    for (path, entry) in files {
        let Some(messages) = entry.get("messages").and_then(Value::as_array) else {
            continue;
        };
        for message in messages {
            let text = message
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("phpstan finding")
                .to_owned();
            // File-level messages carry a null `line`.
            let line = message.get("line").and_then(Value::as_u64);
            findings.push(Finding {
                rule_id: rule_id_for(message),
                // PHPStan reports bugs but does not itself rank them; keep a
                // flat medium rather than inventing a severity.
                severity: Severity::Medium,
                // "Correctness / robustness problems not directly
                // exploitable" — see the module docs on the variant choice.
                category: Category::Quality,
                path: Some(PathBuf::from(path)),
                line,
                message: text,
                // A type-level static analyzer, not a token heuristic.
                confidence: Confidence::Confirmed,
            });
        }
    }
    Ok(findings)
}

/// Rewrite a finding's path relative to `root` when it is an absolute path
/// under `root`; leave relative paths (PHPStan's default output) untouched.
fn relativize(findings: &mut [Finding], root: &Path) {
    for finding in findings {
        if let Some(path) = &finding.path
            && let Ok(rel) = path.strip_prefix(root)
        {
            finding.path = Some(rel.to_path_buf());
        }
    }
}

impl Analyzer for PhpStan {
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

        // Build the argv. `.` because the cwd is the analyzed root (matching
        // `semgrep-php`). PHPStan auto-discovers `phpstan.neon`/
        // `phpstan.neon.dist` in the target; the two knobs below only add to
        // that when the operator sets them.
        let mut args: Vec<String> = vec![
            "analyse".to_owned(),
            "--error-format=json".to_owned(),
            "--no-progress".to_owned(),
            "--no-interaction".to_owned(),
        ];
        if let Some(config) = &ctx.config().analyzers.phpstan_config {
            args.push("-c".to_owned());
            args.push(config.to_string_lossy().into_owned());
        }
        if let Some(level) = ctx.config().analyzers.phpstan_level {
            args.push("--level".to_owned());
            args.push(level.to_string());
        }
        args.push(".".to_owned());

        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let run = run_tool(&program, &arg_refs, root, timeout)?;

        // PHPStan exits non-zero whenever it finds errors, so the exit code is
        // not itself a failure signal — the JSON body is the authority
        // (mirrors `composer-audit` / `semgrep-php`). An unparseable body is a
        // real failure; a parseable body with a non-empty top-level `errors`
        // array is a broken run, handled inside `parse_phpstan_json`.
        match serde_json::from_str::<Value>(&run.stdout) {
            Ok(doc) => {
                let mut findings = parse_phpstan_json(&doc)?;
                relativize(&mut findings, root);
                Ok(findings)
            }
            Err(_) => Err(AnalyzerError::ToolFailed(format!(
                "phpstan produced no JSON (exit {:?}): {}",
                run.exit_code,
                stderr_excerpt(&run.stderr)
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A captured `phpstan analyse --error-format=json` body: two file
    /// messages (one with an `identifier`, one file-level with a null `line`
    /// and no `identifier`) and an empty top-level `errors` array.
    const SAMPLE: &str = r#"{
      "totals": {"errors": 0, "file_errors": 2},
      "files": {
        "src/Controller/UserController.php": {
          "errors": 1,
          "messages": [
            {"message": "Call to an undefined method App\\User::save().",
             "line": 42, "ignorable": true, "identifier": "method.notFound"}
          ]
        },
        "src/bootstrap.php": {
          "errors": 1,
          "messages": [
            {"message": "Class App\\Kernel not found.",
             "line": null, "ignorable": false}
          ]
        }
      },
      "errors": []
    }"#;

    #[test]
    fn parses_phpstan_file_messages() {
        let doc: Value = serde_json::from_str(SAMPLE).unwrap();
        let findings = parse_phpstan_json(&doc).unwrap();
        assert_eq!(findings.len(), 2);

        let by_rule = |id: &str| findings.iter().find(|f| f.rule_id == id).unwrap();

        let with_id = by_rule("phpstan/method.notFound");
        assert_eq!(with_id.severity, Severity::Medium);
        assert_eq!(with_id.category, Category::Quality);
        assert_eq!(with_id.confidence, Confidence::Confirmed);
        assert_eq!(with_id.path.as_deref(), Some(Path::new("src/Controller/UserController.php")));
        assert_eq!(with_id.line, Some(42));
        assert!(with_id.message.contains("undefined method"));

        // No `identifier` → the stable fallback rule id; null line → None.
        let fallback = by_rule("phpstan/analyse");
        assert_eq!(fallback.path.as_deref(), Some(Path::new("src/bootstrap.php")));
        assert_eq!(fallback.line, None);
        assert_eq!(fallback.confidence, Confidence::Confirmed);
    }

    #[test]
    fn top_level_errors_gate_instead_of_reporting_clean() {
        // A config failure: PHPStan emits an empty `files` map but a non-empty
        // top-level `errors` array. This must be a gating error, never an
        // empty (clean) result — a broken analysis cannot pass the gate.
        let doc: Value = serde_json::from_str(
            r#"{"totals": {"errors": 1, "file_errors": 0},
                "files": {},
                "errors": ["Configuration file phpstan.neon does not exist."]}"#,
        )
        .unwrap();
        let result = parse_phpstan_json(&doc);
        assert!(matches!(result, Err(AnalyzerError::ToolFailed(_))), "{result:?}");
    }

    #[test]
    fn clean_run_yields_no_findings() {
        let doc: Value = serde_json::from_str(
            r#"{"totals": {"errors": 0, "file_errors": 0}, "files": {}, "errors": []}"#,
        )
        .unwrap();
        assert!(parse_phpstan_json(&doc).unwrap().is_empty());
    }

    #[test]
    fn absolute_paths_under_root_are_relativized() {
        let mut findings = vec![Finding {
            rule_id: "phpstan/analyse".to_owned(),
            severity: Severity::Medium,
            category: Category::Quality,
            path: Some(PathBuf::from("/srv/app/src/Foo.php")),
            line: Some(1),
            message: "x".to_owned(),
            confidence: Confidence::Confirmed,
        }];
        relativize(&mut findings, Path::new("/srv/app"));
        assert_eq!(findings[0].path.as_deref(), Some(Path::new("src/Foo.php")));
    }
}
