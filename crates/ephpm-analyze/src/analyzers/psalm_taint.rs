//! `psalm-taint` — data-flow taint analysis of PHP source.
//!
//! Wraps Psalm's taint mode (`psalm --taint-analysis --output-format=sarif`)
//! as a subprocess. Psalm is an **optional external dependency** — its
//! absence skips this analyzer — and it additionally needs a `psalm.xml`
//! project config to run at all, so a target with no discoverable config is
//! also a skip (mirroring how `composer-audit` skips a missing lockfile)
//! rather than a hard error.
//!
//! Taint analysis is the highest-signal security pass ePHPm ships: Psalm
//! traces untrusted input (superglobals, request data) through the program to
//! dangerous sinks — SQL (`TaintedSql`), HTML/XSS (`TaintedHtml`), shell
//! (`TaintedShell`), file inclusion (`TaintedInclude`), `eval` (`TaintedEval`),
//! and so on — and reports only flows it could actually connect end to end.
//! Every result is therefore a [`Category::Security`] finding at
//! [`Severity::High`] with [`Confidence::Confirmed`]: a completed source→sink
//! flow is a real exploitable path, not a pattern hotspot.
//!
//! This analyzer is **opt-in** — it is *not* in the default `security`
//! profile set. Enable it with `analyzers.enable: [..., psalm-taint]`.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::tool::{run_tool, stderr_excerpt};
use crate::analyzer::{AnalysisCtx, Analyzer, AnalyzerError};
use crate::finding::{Category, Confidence, Finding, Severity};

/// See the module docs.
pub struct PsalmTaint;

/// The analyzer's stable id.
pub const ID: &str = "psalm-taint";

/// Prefer a project-local `vendor/bin/psalm` (how Composer installs it) over a
/// global `psalm` on `PATH`. The local binary is the version the app pins, so
/// it matches the `psalm.xml` in the same tree.
///
/// On Windows the extensionless launcher is not spawnable via `CreateProcess`;
/// there we fall back to the bare `psalm` name, which the shared runner
/// resolves through its `psalm.bat`/`psalm.cmd` PATH search.
fn psalm_program(root: &Path) -> String {
    if !cfg!(windows) {
        let vendor = root.join("vendor").join("bin").join("psalm");
        if vendor.is_file() {
            return vendor.to_string_lossy().into_owned();
        }
    }
    "psalm".to_owned()
}

/// Resolve a Psalm SARIF result to its issue *type* name (e.g. `TaintedSql`).
///
/// Psalm sets a result's `ruleId` to the numeric issue shortcode and carries
/// the human type name in the driver's rule table (`tool.driver.rules[]`,
/// each with `id` = shortcode and `name` = type). We resolve the name via
/// `ruleIndex` first, then by matching `ruleId` against a rule's `id`, and
/// fall back to the raw `ruleId` when neither is present.
fn rule_name(rules: &[Value], result: &Value) -> String {
    let by_index = result
        .get("ruleIndex")
        .and_then(Value::as_u64)
        .and_then(|i| rules.get(usize::try_from(i).ok()?))
        .and_then(|r| r.get("name").and_then(Value::as_str));
    if let Some(name) = by_index {
        return name.to_owned();
    }
    if let Some(id) = result.get("ruleId").and_then(Value::as_str) {
        let by_id = rules
            .iter()
            .find(|r| r.get("id").and_then(Value::as_str) == Some(id))
            .and_then(|r| r.get("name").and_then(Value::as_str));
        return by_id.unwrap_or(id).to_owned();
    }
    "unknown-rule".to_owned()
}

/// Parse a SARIF document produced by Psalm into findings.
fn parse_sarif(doc: &Value) -> Result<Vec<Finding>, AnalyzerError> {
    let runs = doc
        .get("runs")
        .and_then(Value::as_array)
        .ok_or_else(|| AnalyzerError::Parse("SARIF document has no runs".to_owned()))?;
    let mut findings = Vec::new();
    for run in runs {
        let rules = run
            .pointer("/tool/driver/rules")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let Some(results) = run.get("results").and_then(Value::as_array) else {
            continue;
        };
        for result in results {
            let name = rule_name(&rules, result);
            let message = result
                .pointer("/message/text")
                .and_then(Value::as_str)
                .unwrap_or("psalm taint finding")
                .to_owned();
            let location = result.pointer("/locations/0/physicalLocation");
            let path = location
                .and_then(|l| l.pointer("/artifactLocation/uri"))
                .and_then(Value::as_str)
                .map(Into::into);
            let line =
                location.and_then(|l| l.pointer("/region/startLine")).and_then(Value::as_u64);
            findings.push(Finding {
                rule_id: format!("{ID}/{name}"),
                // A traced source→sink flow is a genuine exploitable path.
                severity: Severity::High,
                category: Category::Security,
                path,
                line,
                message,
                // Psalm connected input to sink end to end — not a hotspot.
                confidence: Confidence::Confirmed,
            });
        }
    }
    Ok(findings)
}

/// Locate the Psalm config for `root`.
///
/// Returns the explicit `analyzers.psalm_config` path (resolved against
/// `root`) when set — its existence is left to Psalm, which fails loudly on a
/// bad `--config`, so a mistyped path gates rather than silently skipping.
/// Otherwise auto-discovers `psalm.xml` / `psalm.xml.dist` in `root`;
/// `Ok(None)` means "no explicit config, discovery found one, let Psalm read
/// it"; the `Err` means "nothing to run against — skip".
fn resolve_config(ctx: &AnalysisCtx) -> Result<Option<PathBuf>, AnalyzerError> {
    let root = ctx.root();
    if let Some(cfg) = &ctx.config().analyzers.psalm_config {
        let resolved = if cfg.is_absolute() { cfg.clone() } else { root.join(cfg) };
        return Ok(Some(resolved));
    }
    if root.join("psalm.xml").is_file() || root.join("psalm.xml.dist").is_file() {
        return Ok(None);
    }
    Err(AnalyzerError::Skipped("no psalm.xml in target".to_owned()))
}

impl Analyzer for PsalmTaint {
    fn id(&self) -> &str {
        ID
    }

    fn category(&self) -> Category {
        Category::Security
    }

    fn run(&self, ctx: &AnalysisCtx) -> Result<Vec<Finding>, AnalyzerError> {
        // Taint analysis is meaningless without a project config; a target
        // that has none is a skip (like a missing composer.lock), not a
        // failure — checked before we ever spawn Psalm.
        let config = resolve_config(ctx)?;
        let root = ctx.root();
        let program = psalm_program(root);
        let timeout = ctx.config().analyzers.tool_timeout_ms;

        let mut args: Vec<String> = vec![
            "--taint-analysis".to_owned(),
            "--output-format=sarif".to_owned(),
            "--no-progress".to_owned(),
        ];
        if let Some(cfg) = &config {
            args.push("--config".to_owned());
            args.push(cfg.to_string_lossy().into_owned());
        }
        // Analyze the whole tree (cwd is `root`); Psalm intersects this with
        // its `projectFiles`, so the full source graph is still available for
        // the taint trace.
        args.push(".".to_owned());
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

        let run = run_tool(&program, &arg_refs, root, timeout)?;
        // Psalm exits non-zero when it *finds* taint issues, so the exit code
        // is not the failure signal — the SARIF body is authoritative. A body
        // we cannot parse (a genuine crash, a bad `--config`) is a real
        // failure and gates.
        match serde_json::from_str::<Value>(&run.stdout) {
            Ok(doc) => parse_sarif(&doc),
            Err(_) => Err(AnalyzerError::ToolFailed(format!(
                "psalm produced no SARIF (exit {:?}): {}",
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

    /// A captured `psalm --taint-analysis --output-format=sarif` document: the
    /// numeric `ruleId` on each result, the type name in the driver rule
    /// table, resolved via `ruleIndex`.
    const PSALM_SARIF: &str = r#"{
      "version": "2.1.0",
      "runs": [{
        "tool": {"driver": {"name": "Psalm", "rules": [
          {"id": "205", "name": "TaintedSql"},
          {"id": "246", "name": "TaintedHtml"}
        ]}},
        "results": [
          {"ruleId": "205", "ruleIndex": 0,
           "level": "error",
           "message": {"text": "Detected tainted SQL"},
           "locations": [{"physicalLocation": {
             "artifactLocation": {"uri": "src/db.php"},
             "region": {"startLine": 15}}}]},
          {"ruleId": "246", "ruleIndex": 1,
           "level": "error",
           "message": {"text": "Detected tainted HTML"},
           "locations": [{"physicalLocation": {
             "artifactLocation": {"uri": "src/view.php"},
             "region": {"startLine": 8}}}]}
        ]
      }]
    }"#;

    #[test]
    fn parses_taint_results_as_confirmed_high_security() {
        let doc: Value = serde_json::from_str(PSALM_SARIF).unwrap();
        let findings = parse_sarif(&doc).unwrap();
        assert_eq!(findings.len(), 2);

        let sql = &findings[0];
        assert_eq!(sql.rule_id, "psalm-taint/TaintedSql");
        assert_eq!(sql.severity, Severity::High);
        assert_eq!(sql.category, Category::Security);
        assert_eq!(sql.confidence, Confidence::Confirmed);
        assert_eq!(sql.path.as_deref(), Some(std::path::Path::new("src/db.php")));
        assert_eq!(sql.line, Some(15));
        assert!(sql.message.contains("tainted SQL"));

        assert_eq!(findings[1].rule_id, "psalm-taint/TaintedHtml");
        assert_eq!(findings[1].line, Some(8));
    }

    #[test]
    fn resolves_rule_name_by_id_when_index_absent() {
        // Some SARIF emitters omit `ruleIndex`; the type name must still
        // resolve by matching `ruleId` against the driver rule table.
        let doc: Value = serde_json::from_str(
            r#"{"runs": [{
              "tool": {"driver": {"rules": [{"id": "205", "name": "TaintedSql"}]}},
              "results": [{"ruleId": "205",
                "message": {"text": "x"},
                "locations": [{"physicalLocation": {
                  "artifactLocation": {"uri": "a.php"}, "region": {"startLine": 1}}}]}]
            }]}"#,
        )
        .unwrap();
        let findings = parse_sarif(&doc).unwrap();
        assert_eq!(findings[0].rule_id, "psalm-taint/TaintedSql");
    }

    #[test]
    fn falls_back_to_raw_rule_id_when_unresolvable() {
        // No rule table entry matches: keep the raw ruleId so the finding is
        // still attributable rather than dropped.
        let doc: Value = serde_json::from_str(
            r#"{"runs": [{"tool": {"driver": {"rules": []}},
              "results": [{"ruleId": "TaintedShell",
                "message": {"text": "x"}, "locations": []}]}]}"#,
        )
        .unwrap();
        let findings = parse_sarif(&doc).unwrap();
        assert_eq!(findings[0].rule_id, "psalm-taint/TaintedShell");
        assert_eq!(findings[0].path, None);
    }

    #[test]
    fn sarif_without_runs_is_a_parse_error() {
        let doc: Value = serde_json::from_str("{}").unwrap();
        assert!(matches!(parse_sarif(&doc), Err(AnalyzerError::Parse(_))));
    }

    #[test]
    fn target_without_psalm_config_is_skipped_not_an_error() {
        // An empty tree has no psalm.xml — the analyzer must skip (which does
        // not gate) rather than error, and must do so before spawning Psalm,
        // so this test needs no Psalm on PATH.
        let dir = tempfile::tempdir().unwrap();
        let ctx = AnalysisCtx::new(dir.path().to_path_buf(), AnalyzeConfig::default());
        let err = PsalmTaint.run(&ctx).unwrap_err();
        assert!(err.is_skip(), "expected Skipped, got {err:?}");
        assert!(err.to_string().contains("no psalm.xml"), "{err}");
    }

    #[test]
    fn discovered_psalm_xml_does_not_skip_at_config_resolution() {
        // With a psalm.xml present, config resolution succeeds (returns None =
        // "let Psalm auto-discover") — the skip guard no longer fires.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("psalm.xml"), "<psalm/>").unwrap();
        let ctx = AnalysisCtx::new(dir.path().to_path_buf(), AnalyzeConfig::default());
        assert!(matches!(resolve_config(&ctx), Ok(None)));
    }
}
