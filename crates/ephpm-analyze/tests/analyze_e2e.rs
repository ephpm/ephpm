//! End-to-end aggregator tests over a real fixture directory, using the
//! native `dangerous-sinks` analyzer (zero external dependencies) plus a
//! faked external analyzer, so the whole pipeline — config → ctx → analyzers
//! → policy → output — runs without composer/semgrep/yara installed.
//!
//! NOTE for maintainers: the on-disk PHP fixtures deliberately use the
//! `assert` sink, not `eval`/`system`. Endpoint antivirus quarantines files
//! (including THIS source file, and the temp fixtures the tests write)
//! whose bytes look like a PHP webshell, and that broke the build once
//! already. `assert(` is one of the analyzer's sinks and is AV-benign.

use std::fs;

use ephpm_analyze::{
    AnalysisCtx, AnalyzeConfig, Analyzer, AnalyzerError, AnalyzerState, Category, Finding,
    Severity, Verdict, analyze, analyzers, output, run_analyzers,
};

/// Build a small PHP project fixture with one `assert($cond)` sink call.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/upload.php"), "<?php\n// handler\nassert($cond);\n").unwrap();
    fs::write(dir.path().join("src/clean.php"), "<?php\necho 'hello';\n").unwrap();
    fs::write(dir.path().join("notes.txt"), "assert(this is not php)\n").unwrap();
    dir
}

#[test]
fn native_stub_finds_sink_in_fixture_and_quarantines() {
    let dir = fixture();
    // quarantine_score lowered to 4 so the single medium finding (weight 4)
    // crosses it — also exercises threshold tunability end to end.
    let config = AnalyzeConfig::from_yaml(
        "analyzers:\n  enable: [dangerous-sinks]\npolicy:\n  quarantine_score: 4",
    )
    .unwrap();
    let report = analyze(dir.path(), config).expect("analysis runs");

    // Exactly one finding: the assert() in upload.php (the .txt is ignored).
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    let finding = &report.findings[0];
    assert_eq!(finding.rule_id, "dangerous-sinks/assert");
    assert_eq!(finding.severity, Severity::Medium);
    assert_eq!(finding.line, Some(3));
    let path = finding.path.as_ref().unwrap().to_string_lossy().replace('\\', "/");
    assert_eq!(path, "src/upload.php");

    assert_eq!(report.score, 4);
    assert_eq!(report.verdict, Verdict::Quarantine);

    // Both output formats render it.
    let text = output::to_text(&report);
    assert!(text.contains("dangerous-sinks/assert"));
    assert!(text.contains("verdict: quarantine"));
    let sarif: serde_json::Value =
        serde_json::from_str(&output::to_sarif(&report).unwrap()).unwrap();
    assert_eq!(sarif["runs"][0]["results"][0]["ruleId"], "dangerous-sinks/assert");
}

#[test]
fn default_thresholds_let_a_single_medium_pass() {
    // Same fixture, default thresholds: one medium (4) < quarantine (10).
    let dir = fixture();
    let config = AnalyzeConfig::from_yaml("analyzers:\n  enable: [dangerous-sinks]").unwrap();
    let report = analyze(dir.path(), config).expect("analysis runs");
    assert_eq!(report.score, 4);
    assert_eq!(report.verdict, Verdict::Allow);
}

#[test]
fn deny_hard_escalates_the_same_fixture_to_deny() {
    let dir = fixture();
    let config = AnalyzeConfig::from_yaml(
        "analyzers:\n  enable: [dangerous-sinks]\n  deny_hard: [dangerous-sinks/assert]",
    )
    .unwrap();
    let report = analyze(dir.path(), config).expect("analysis runs");
    assert_eq!(report.verdict, Verdict::Deny);
    assert!(report.reasons.iter().any(|r| r.contains("deny_hard")));
}

#[test]
fn clean_tree_allows() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("index.php"), "<?php\necho 'ok';\n").unwrap();
    let config = AnalyzeConfig::from_yaml("analyzers:\n  enable: [dangerous-sinks]").unwrap();
    let report = analyze(dir.path(), config).expect("analysis runs");
    assert!(report.findings.is_empty());
    assert_eq!(report.verdict, Verdict::Allow);
}

/// A fake "external tool" analyzer whose tool always explodes — standing in
/// for composer/semgrep/yara erroring at runtime.
struct ExplodingExternal;

impl Analyzer for ExplodingExternal {
    // The trait fixes the signature; the literal cannot be `&'static str`
    // here without diverging from it.
    #[allow(clippy::unnecessary_literal_bound)]
    fn id(&self) -> &str {
        "exploding-external"
    }
    fn category(&self) -> Category {
        Category::SupplyChain
    }
    fn run(&self, _ctx: &AnalysisCtx) -> Result<Vec<Finding>, AnalyzerError> {
        Err(AnalyzerError::ToolFailed("simulated tool crash".to_owned()))
    }
}

#[test]
fn failing_external_analyzer_fails_closed_alongside_native_findings() {
    let dir = fixture();
    let config = AnalyzeConfig::from_yaml("profile: none").unwrap();
    let ctx = AnalysisCtx::new(dir.path().to_path_buf(), config);
    let set: Vec<Box<dyn Analyzer>> =
        vec![Box::new(analyzers::DangerousSinks), Box::new(ExplodingExternal)];
    let report = run_analyzers(&ctx, &set);

    // Native findings still collected...
    assert_eq!(report.findings.len(), 1);
    // ...and the failed external analyzer is recorded and floors the verdict.
    assert!(
        report.statuses.iter().any(
            |s| s.id == "exploding-external" && matches!(s.state, AnalyzerState::Failed { .. })
        )
    );
    assert!(report.verdict >= Verdict::Quarantine);
    assert!(report.reasons.iter().any(|r| r.contains("fail-closed")));
}

#[test]
fn absent_external_tools_skip_but_do_not_gate() {
    // composer-audit and malware-yara over a tree with no composer.json and
    // no yara ruleset: both must SKIP (not fail). Their skip conditions are
    // input-driven and machine-independent, so this holds whether or not the
    // tools are installed on the host.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("index.php"), "<?php echo 1;\n").unwrap();
    let config = AnalyzeConfig::from_yaml(
        "analyzers:\n  enable: [composer-audit, malware-yara, dangerous-sinks]",
    )
    .unwrap();
    let report = analyze(dir.path(), config).expect("analysis runs");
    for id in ["composer-audit", "malware-yara"] {
        let status = report.statuses.iter().find(|s| s.id == id).unwrap();
        assert!(
            matches!(status.state, AnalyzerState::Skipped { .. }),
            "{id} should skip, got {:?}",
            status.state
        );
    }
    assert_eq!(report.verdict, Verdict::Allow);
}
