//! `rector` — automated-refactoring *suggestions* via Rector, in dry-run.
//!
//! Wraps `rector process . --dry-run --output-format=json --no-progress-bar`
//! as a subprocess (Rector is an **optional external dependency** — absence
//! skips this analyzer). **`--dry-run` is mandatory and never omitted**: this
//! analyzer must only *report* the refactorings Rector would apply, never
//! modify the analyzed tree. The binary is resolved first from a project-local
//! Composer install (`vendor/bin/rector`) and otherwise from `PATH`.
//!
//! **CLI + output (verified against `rectorphp/rector-src`,
//! `ProcessConfigureDecorator` + `JsonOutputFormatter`/`JsonOutputFactory`,
//! Sept 2026).** `process` accepts `--dry-run`, `--output-format` (value
//! `json`), and `--no-progress-bar`, all confirmed present. The JSON document
//! has `meta`, `totals` (`{changed_files, errors, …}`), a `changed_files`
//! array, and a **`file_diffs`** array whose elements are
//! `{file, diff, applied_rectors: [ <fully-qualified rector class> … ],
//! original_content, new_content}`. In `--dry-run` with changes present Rector
//! returns a non-zero exit (`ExitCode::CHANGED_CODE`), so — like `progpilot` /
//! `phpmd` — the exit code is not a failure signal; the JSON body is
//! authoritative. A non-empty top-level `errors` array (files Rector could not
//! process) gates (fail-closed), and an unparseable body gates.
//!
//! **Config requirement.** Rector cannot run without a `rector.php` config; a
//! target that has none is a *skip* (like `psalm-taint`'s missing `psalm.xml`),
//! not a failure — checked before Rector is ever spawned. An explicit
//! `analyzers.rector_config` overrides discovery (its existence is left to
//! Rector, which fails loudly on a bad `--config` and thus gates).
//!
//! **Mapping.** One [`Severity::Low`] [`Category::Quality`] finding per
//! file-with-suggested-changes — Rector proposes modernizations/refactors, not
//! bugs — with `rule_id = rector/<first-applied-rector-short-name>` (or
//! `rector/process` when the list is empty). Findings are
//! [`Confidence::Suspected`]: they are refactor suggestions to review, not
//! defects.
//!
//! This analyzer is **not** in the default `security` profile — it is opt-in
//! via `analyzers.enable: [rector]`.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::tool::{run_tool, stderr_excerpt};
use crate::analyzer::{AnalysisCtx, Analyzer, AnalyzerError};
use crate::finding::{Category, Confidence, Finding, Severity};

/// See the module docs.
pub struct Rector;

/// The analyzer's stable id.
pub const ID: &str = "rector";

/// Resolve the Rector program name for [`run_tool`]. Prefers a project-local
/// `vendor/bin/rector` over a global `rector`; on Windows the bare name is
/// used so the shared runner resolves the `.bat`/`.cmd` launcher.
fn resolve_binary(root: &Path) -> String {
    if !cfg!(windows) {
        let vendored = root.join("vendor").join("bin").join("rector");
        if vendored.is_file() {
            return vendored.to_string_lossy().into_owned();
        }
    }
    "rector".to_owned()
}

/// Resolve the Rector config for `root`.
///
/// Returns the explicit `analyzers.rector_config` path (resolved against
/// `root`) when set — existence is left to Rector, which fails loudly on a bad
/// `--config`, so a mistyped path gates rather than silently skipping.
/// Otherwise, when a `rector.php` exists in `root`, returns `Ok(None)` ("let
/// Rector auto-discover it"). Absent both, returns the skip error.
///
/// # Errors
///
/// [`AnalyzerError::Skipped`] when there is no explicit config and no
/// discoverable `rector.php` — Rector cannot run without one.
fn resolve_config(ctx: &AnalysisCtx) -> Result<Option<PathBuf>, AnalyzerError> {
    let root = ctx.root();
    if let Some(cfg) = &ctx.config().analyzers.rector_config {
        let resolved = if cfg.is_absolute() { cfg.clone() } else { root.join(cfg) };
        return Ok(Some(resolved));
    }
    if root.join("rector.php").is_file() {
        return Ok(None);
    }
    Err(AnalyzerError::Skipped("no rector.php in target".to_owned()))
}

/// The short (unqualified) class name of a fully-qualified Rector class:
/// the segment after the last `\`. `Rector\Php80\…\StringableForToStringRector`
/// → `StringableForToStringRector`.
fn short_rector_name(fqcn: &str) -> &str {
    fqcn.rsplit('\\').next().unwrap_or(fqcn)
}

/// Parse a `rector process --output-format=json` document into findings.
///
/// # Errors
///
/// [`AnalyzerError::ToolFailed`] when the top-level `errors` array is
/// non-empty — those are files Rector could not process, so the run gates.
fn parse_rector_json(doc: &Value, root: &Path) -> Result<Vec<Finding>, AnalyzerError> {
    if let Some(errors) = doc.get("errors").and_then(Value::as_array)
        && !errors.is_empty()
    {
        let first = errors
            .iter()
            .find_map(|e| e.get("message").and_then(Value::as_str))
            .unwrap_or("unknown processing error");
        return Err(AnalyzerError::ToolFailed(format!(
            "rector could not process part of the tree: {}",
            first.chars().take(200).collect::<String>()
        )));
    }

    let mut findings = Vec::new();
    let Some(diffs) = doc.get("file_diffs").and_then(Value::as_array) else {
        return Ok(findings);
    };
    for diff in diffs {
        let path = diff.get("file").and_then(Value::as_str).map(|p| {
            let p = Path::new(p);
            p.strip_prefix(root).unwrap_or(p).to_path_buf()
        });
        let applied: Vec<&str> = diff
            .get("applied_rectors")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let rule_suffix = applied.first().map_or("process", |fqcn| short_rector_name(fqcn));
        let message = if applied.is_empty() {
            "rector suggests changes to this file (dry-run)".to_owned()
        } else {
            format!(
                "rector suggests {} refactoring(s) here ({}) — dry-run, no changes applied",
                applied.len(),
                applied.iter().map(|f| short_rector_name(f)).collect::<Vec<_>>().join(", ")
            )
        };
        findings.push(Finding {
            rule_id: format!("{ID}/{rule_suffix}"),
            // A refactor suggestion, not a bug.
            severity: Severity::Low,
            category: Category::Quality,
            path,
            // File-level: a diff spans multiple hunks, no single line anchor.
            line: None,
            message,
            confidence: Confidence::Suspected,
        });
    }
    Ok(findings)
}

impl Analyzer for Rector {
    fn id(&self) -> &str {
        ID
    }

    fn category(&self) -> Category {
        Category::Quality
    }

    fn run(&self, ctx: &AnalysisCtx) -> Result<Vec<Finding>, AnalyzerError> {
        // Rector needs a config; a target without one is a skip (like a
        // missing psalm.xml), not a failure — checked before we spawn Rector.
        let config = resolve_config(ctx)?;
        let root = ctx.root();
        let timeout = ctx.config().analyzers.tool_timeout_ms;
        let program = resolve_binary(root);

        // `--dry-run` is mandatory: never mutate the analyzed tree.
        let mut args: Vec<String> = vec![
            "process".to_owned(),
            ".".to_owned(),
            "--dry-run".to_owned(),
            "--output-format=json".to_owned(),
            "--no-progress-bar".to_owned(),
        ];
        if let Some(cfg) = &config {
            args.push("--config".to_owned());
            args.push(cfg.to_string_lossy().into_owned());
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let run = run_tool(&program, &arg_refs, root, timeout)?;

        // Exit code is not a failure signal (non-zero when changes are found);
        // the JSON body is. An unparseable body gates.
        match serde_json::from_str::<Value>(&run.stdout) {
            Ok(doc) => parse_rector_json(&doc, root),
            Err(_) => Err(AnalyzerError::ToolFailed(format!(
                "rector produced no JSON (exit {:?}): {}",
                run.exit_code,
                stderr_excerpt(&run.stderr)
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AnalyzeConfig;

    /// A captured `rector process --dry-run --output-format=json` body: two
    /// files with suggested changes, empty top-level `errors`.
    const SAMPLE: &str = r#"{
      "meta": {"version": "2.0.0"},
      "totals": {"changed_files": 2, "errors": 0},
      "changed_files": ["src/Foo.php", "src/Bar.php"],
      "file_diffs": [
        {"file": "/srv/app/src/Foo.php",
         "diff": "@@ -1 +1 @@\n-old\n+new",
         "applied_rectors": ["Rector\\Php80\\Rector\\Class_\\StringableForToStringRector",
                             "Rector\\CodeQuality\\Rector\\If_\\SimplifyIfReturnBoolRector"]},
        {"file": "/srv/app/src/Bar.php",
         "diff": "@@ -3 +3 @@\n-a\n+b",
         "applied_rectors": []}
      ]
    }"#;

    #[test]
    fn parses_one_low_quality_suggestion_per_file_diff() {
        let doc: Value = serde_json::from_str(SAMPLE).unwrap();
        let findings = parse_rector_json(&doc, Path::new("/srv/app")).unwrap();
        assert_eq!(findings.len(), 2);

        // rule id is the SHORT name of the first applied rector; low quality,
        // suspected, file-level (no line), path relativized.
        let foo = &findings[0];
        assert_eq!(foo.rule_id, "rector/StringableForToStringRector");
        assert_eq!(foo.severity, Severity::Low);
        assert_eq!(foo.category, Category::Quality);
        assert_eq!(foo.confidence, Confidence::Suspected);
        assert_eq!(foo.path.as_deref(), Some(Path::new("src/Foo.php")));
        assert_eq!(foo.line, None);
        assert!(foo.message.contains("2 refactoring"));
        assert!(foo.message.contains("SimplifyIfReturnBoolRector"));

        // Empty applied_rectors → the `rector/process` fallback rule id.
        let bar = &findings[1];
        assert_eq!(bar.rule_id, "rector/process");
        assert_eq!(bar.path.as_deref(), Some(Path::new("src/Bar.php")));
    }

    #[test]
    fn no_file_diffs_means_no_findings() {
        let doc: Value = serde_json::from_str(
            r#"{"totals": {"changed_files": 0, "errors": 0}, "file_diffs": []}"#,
        )
        .unwrap();
        assert!(parse_rector_json(&doc, Path::new("/r")).unwrap().is_empty());
    }

    #[test]
    fn top_level_errors_gate_instead_of_reporting_clean() {
        let doc: Value = serde_json::from_str(
            r#"{"file_diffs": [], "errors": [{"message": "PHPStan analysis error in x.php",
                "file": "x.php"}]}"#,
        )
        .unwrap();
        assert!(matches!(
            parse_rector_json(&doc, Path::new("/r")),
            Err(AnalyzerError::ToolFailed(_))
        ));
    }

    #[test]
    fn target_without_rector_config_is_skipped_not_an_error() {
        // An empty tree has no rector.php — the analyzer must skip (which does
        // not gate) rather than error, and before spawning Rector, so this
        // test needs no Rector on PATH.
        let dir = tempfile::tempdir().unwrap();
        let ctx = AnalysisCtx::new(dir.path().to_path_buf(), AnalyzeConfig::default());
        let err = Rector.run(&ctx).unwrap_err();
        assert!(err.is_skip(), "expected Skipped, got {err:?}");
        assert!(err.to_string().contains("no rector.php"), "{err}");
    }

    #[test]
    fn discovered_rector_php_does_not_skip_at_config_resolution() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("rector.php"), "<?php return [];").unwrap();
        let ctx = AnalysisCtx::new(dir.path().to_path_buf(), AnalyzeConfig::default());
        assert!(matches!(resolve_config(&ctx), Ok(None)));
    }
}
