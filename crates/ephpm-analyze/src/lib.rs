//! ephpm-analyze — static-analysis aggregator and fail-closed deploy gate
//! for PHP applications (`ephpm analyze`).
//!
//! # Architecture
//!
//! ```text
//!   .ephpm-analyze.yml ──► AnalyzeConfig ──► AnalysisCtx
//!                             (+ scope, cache, baseline)
//!                                               │
//!                     ┌─────────────────────────┼──────────────┐
//!                     ▼                         ▼              ▼
//!             external-tool analyzers    native analyzers   engine-backed
//!             composer-audit             dangerous-sinks    opcode-scan (L1,
//!             semgrep-php                suppression-scan   opt-in; planned:
//!             malware-yara                                  taint, L2)
//!                     │                         │              │
//!                     └────────► Findings ◄─────┴──────────────┘
//!                                   │
//!               scope filter ─► operator suppress ─► baseline
//!                                   │
//!                            policy::decide ──► Verdict ──► exit code
//!                                   │
//!                        output::{to_text, to_sarif}
//! ```
//!
//! The aggregator runs every enabled [`Analyzer`], merges their
//! [`Finding`]s, post-processes them (diff-aware scope filter, operator-only
//! suppressions, baseline — in that order), and hands the survivors to the
//! policy engine, which produces a [`Verdict`] (`Allow` / `Quarantine` /
//! `Deny`). The whole pipeline is **fail-closed**: an analyzer that errors,
//! times out, panics, or is `required` but cannot run floors the verdict at
//! `Quarantine` — only a run where everything that should have spoken
//! actually spoke can `Allow`. An *optional* analyzer whose external tool is
//! simply not installed is reported as skipped and does not gate (see
//! [`analyzer::AnalyzerError`]). The same discipline runs through the
//! supporting features: a `since` git failure, a missing configured
//! baseline, and an invalid config are all hard errors, never silent
//! degradations.
//!
//! The opt-in `opcode-scan` analyzer is the first engine-backed pass: it
//! compiles each PHP file with the embedded Zend compiler (never executing
//! it) and detects dangerous call sites in the opcode stream — on builds
//! without a linked libphp it degrades to a skip. Later phases (superglobal
//! taint tracking, engine-in-the-loop detonation) slot in as more
//! [`Analyzer`] implementations reading richer state off [`AnalysisCtx`] —
//! the trait, the finding shape, the policy engine, and both output formats
//! are already final for them.

pub mod analyzer;
pub mod analyzers;
pub mod baseline;
pub mod cache;
pub mod config;
pub mod finding;
pub mod output;
pub mod policy;
pub mod report;
pub mod scope;
pub mod suppress;

use std::path::Path;

use anyhow::Context as _;

pub use crate::analyzer::{AnalysisCtx, Analyzer, AnalyzerError};
pub use crate::baseline::Baseline;
pub use crate::config::{AnalyzeConfig, FailOn, OutputFormat, Profile};
pub use crate::finding::{Category, Confidence, Finding, Severity};
pub use crate::policy::Verdict;
pub use crate::report::{AnalysisReport, AnalyzerState, AnalyzerStatus};

/// Run `analyzers` against `ctx`, merge findings, and decide the verdict.
///
/// This is the aggregator core, kept separate from [`analyze`] so tests (and
/// future embedders) can inject their own analyzer set. Fail-closed behavior
/// implemented here:
///
/// - an `Err` other than `Skipped` becomes [`AnalyzerState::Failed`];
/// - a panic inside an analyzer is caught and becomes `Failed` (an analyzer
///   bug must gate the run, not crash the gate);
/// - a `Skipped` analyzer listed in `analyzers.required` is upgraded to
///   `Failed`;
/// - any `Failed` analyzer floors the verdict at `Quarantine` (see
///   [`policy::decide`]).
#[must_use]
pub fn run_analyzers(ctx: &AnalysisCtx, analyzers: &[Box<dyn Analyzer>]) -> AnalysisReport {
    let required = &ctx.config().analyzers.required;
    let mut findings = Vec::new();
    let mut statuses = Vec::new();

    for analyzer in analyzers {
        let id = analyzer.id().to_owned();
        tracing::debug!(analyzer = %id, "running analyzer");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| analyzer.run(ctx)));
        let state = match result {
            Ok(Ok(mut analyzer_findings)) => {
                let count = analyzer_findings.len();
                findings.append(&mut analyzer_findings);
                AnalyzerState::Completed { findings: count }
            }
            Ok(Err(e)) if e.is_skip() && !required.contains(&id) => {
                tracing::info!(analyzer = %id, reason = %e, "analyzer skipped");
                AnalyzerState::Skipped { reason: e.to_string() }
            }
            Ok(Err(e)) if e.is_skip() => {
                // Required analyzers must speak: a skip is a failure.
                tracing::warn!(analyzer = %id, reason = %e, "required analyzer could not run");
                AnalyzerState::Failed { error: format!("required but could not run — {e}") }
            }
            Ok(Err(e)) => {
                tracing::warn!(analyzer = %id, error = %e, "analyzer failed");
                AnalyzerState::Failed { error: e.to_string() }
            }
            Err(_) => {
                tracing::error!(analyzer = %id, "analyzer panicked");
                AnalyzerState::Failed { error: "panicked".to_owned() }
            }
        };
        statuses.push(AnalyzerStatus { id, state });
    }

    // Post-processing, in a deliberate order:
    //
    // 1. Diff-aware scope: on a `since` run, drop findings anchored at a
    //    path outside the changed set. Pathless findings are always kept —
    //    an unattributable finding must not be droppable by scoping.
    let mut out_of_scope = 0;
    if let Some(scope) = ctx.scope() {
        let before = findings.len();
        findings.retain(|f| f.path.as_deref().is_none_or(|p| scope.contains(p)));
        out_of_scope = before - findings.len();
    }

    // 2. Operator suppressions — the only waiver mechanism (crate::suppress).
    let (mut findings, waived) = suppress::apply(findings, &ctx.config().suppress);

    // 3. Baseline: drop findings whose stable fingerprint is remembered, so
    //    only new findings gate.
    let mut baseline_suppressed = 0;
    if let Some(baseline) = ctx.baseline() {
        let known = baseline.fingerprints();
        let before = findings.len();
        findings.retain(|f| !known.contains(baseline::fingerprint(f).as_str()));
        baseline_suppressed = before - findings.len();
    }

    // Most severe first, then stable by rule id for deterministic output.
    findings.sort_by(|a, b| b.severity.cmp(&a.severity).then_with(|| a.rule_id.cmp(&b.rule_id)));

    let failed: Vec<String> = statuses
        .iter()
        .filter(|s| matches!(s.state, AnalyzerState::Failed { .. }))
        .map(|s| s.id.clone())
        .collect();
    let decision = policy::decide(
        &findings,
        &policy::PolicyParams {
            deny_hard: &ctx.config().analyzers.deny_hard,
            failed_analyzers: &failed,
            quarantine_score: ctx.config().policy.quarantine_score,
            deny_score: ctx.config().policy.deny_score,
            level: ctx.config().level,
        },
    );

    AnalysisReport {
        findings,
        statuses,
        verdict: decision.verdict,
        score: decision.score,
        reasons: decision.reasons,
        out_of_scope,
        waived,
        baseline_suppressed,
    }
}

/// Analyze the checkout at `root` with `config`, using the built-in
/// analyzer registry.
///
/// Resolves `analyzers.enable` (or the profile's default set) against the
/// registry — an unknown analyzer id is a hard error, never silently
/// ignored, and `analyzers.required` must be a subset of the enabled set.
///
/// # Errors
///
/// On a missing/non-directory `root`, an invalid configuration (unknown
/// analyzer id, `required` entry that is not enabled, out-of-range `level`),
/// a git failure on a `since` run, or a configured-but-missing baseline —
/// all fail-closed. Individual analyzer failures do **not** error — they are
/// folded into the report fail-closed.
pub fn analyze(root: &Path, config: AnalyzeConfig) -> anyhow::Result<AnalysisReport> {
    anyhow::ensure!(root.is_dir(), "analysis target {} is not a directory", root.display());
    config.validate()?;

    if config.engine.any_set() {
        tracing::warn!(
            "engine.detonate / engine.timeout_ms are parsed but not yet implemented \
             (engine-in-the-loop analysis is Phase 4) — the settings have no effect"
        );
    }

    let mut registry = analyzers::built_in();
    let enabled = config.enabled_analyzers();
    let mut selected: Vec<Box<dyn Analyzer>> = Vec::new();
    for id in &enabled {
        match registry.iter().position(|a| a.id() == id) {
            Some(pos) => selected.push(registry.remove(pos)),
            None if selected.iter().any(|a| a.id() == id) => {
                anyhow::bail!("analyzer {id:?} is listed twice in analyzers.enable");
            }
            None => {
                let known: Vec<String> =
                    analyzers::built_in().iter().map(|a| a.id().to_owned()).collect();
                anyhow::bail!("unknown analyzer {id:?} (known: {})", known.join(", "));
            }
        }
    }
    for required in &config.analyzers.required {
        anyhow::ensure!(
            enabled.contains(required),
            "analyzers.required lists {required:?}, which is not in the enabled set {enabled:?}"
        );
    }

    let root = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", root.display()))?;

    // Diff-aware scope: any git failure is a hard error (fail-closed).
    let scope = match &config.since {
        Some(since_ref) => {
            let scope = scope::changed_files(&root, since_ref)?;
            tracing::info!(since = %since_ref, changed_files = scope.len(), "diff-aware scan");
            Some(scope)
        }
        None => None,
    };

    // Baseline: a configured-but-missing file is a hard error — an operator
    // asked for baseline gating, so silently gating without one (or with a
    // stale corrupt one) would be a no-op knob.
    let baseline = match &config.baseline {
        Some(path) => {
            let path = if path.is_absolute() { path.clone() } else { root.join(path) };
            Some(Baseline::load(&path)?)
        }
        None => None,
    };

    // Cache open failures degrade to an uncached run (warn) — the cache is
    // an optimization; only its *contents* are correctness-relevant, and
    // those are keyed safely (see crate::cache).
    let cache = if config.cache.enabled {
        let dir = config.cache.dir.clone().unwrap_or_else(cache::default_dir);
        match cache::FileCache::open(dir, &config) {
            Ok(cache) => Some(cache),
            Err(e) => {
                tracing::warn!(error = %e, "analysis cache unavailable — running uncached");
                None
            }
        }
    } else {
        None
    };

    let mut ctx = AnalysisCtx::new(root, config);
    if let Some(scope) = scope {
        ctx = ctx.with_scope(scope);
    }
    if let Some(baseline) = baseline {
        ctx = ctx.with_baseline(baseline);
    }
    if let Some(cache) = cache {
        ctx = ctx.with_cache(cache);
    }
    Ok(run_analyzers(&ctx, &selected))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeAnalyzer {
        id: &'static str,
        result: fn() -> Result<Vec<Finding>, AnalyzerError>,
    }

    impl Analyzer for FakeAnalyzer {
        fn id(&self) -> &str {
            self.id
        }
        fn category(&self) -> Category {
            Category::Security
        }
        fn run(&self, _ctx: &AnalysisCtx) -> Result<Vec<Finding>, AnalyzerError> {
            (self.result)()
        }
    }

    fn ctx_with(config: AnalyzeConfig) -> AnalysisCtx {
        AnalysisCtx::new(std::env::temp_dir(), config)
    }

    fn test_finding(rule_id: &str, severity: Severity, path: Option<&str>) -> Finding {
        Finding {
            rule_id: rule_id.to_owned(),
            severity,
            category: Category::Security,
            path: path.map(std::path::PathBuf::from),
            line: Some(1),
            message: format!("finding {rule_id}"),
            confidence: Confidence::Confirmed,
        }
    }

    /// A boxed analyzer emitting a fixed finding set (built per call so the
    /// `fn()`-pointer shape of `FakeAnalyzer` is not a constraint here).
    struct EmitAnalyzer(Vec<Finding>);

    impl Analyzer for EmitAnalyzer {
        // The trait fixes the signature; the literal cannot be
        // `&'static str` here without diverging from it.
        #[allow(clippy::unnecessary_literal_bound)]
        fn id(&self) -> &str {
            "emit"
        }
        fn category(&self) -> Category {
            Category::Security
        }
        fn run(&self, _ctx: &AnalysisCtx) -> Result<Vec<Finding>, AnalyzerError> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn erroring_analyzer_quarantines_the_run() {
        let analyzers: Vec<Box<dyn Analyzer>> = vec![Box::new(FakeAnalyzer {
            id: "broken",
            result: || Err(AnalyzerError::ToolFailed("boom".to_owned())),
        })];
        let report = run_analyzers(&ctx_with(AnalyzeConfig::default()), &analyzers);
        assert_eq!(report.verdict, Verdict::Quarantine);
        assert_eq!(report.failed_analyzers(), vec!["broken"]);
    }

    #[test]
    fn skipped_optional_analyzer_does_not_gate() {
        let analyzers: Vec<Box<dyn Analyzer>> = vec![Box::new(FakeAnalyzer {
            id: "absent",
            result: || Err(AnalyzerError::Skipped("tool not found".to_owned())),
        })];
        let report = run_analyzers(&ctx_with(AnalyzeConfig::default()), &analyzers);
        assert_eq!(report.verdict, Verdict::Allow);
        assert!(matches!(report.statuses[0].state, AnalyzerState::Skipped { .. }));
    }

    #[test]
    fn skipped_required_analyzer_gates() {
        let mut config = AnalyzeConfig::default();
        config.analyzers.required = vec!["absent".to_owned()];
        let analyzers: Vec<Box<dyn Analyzer>> = vec![Box::new(FakeAnalyzer {
            id: "absent",
            result: || Err(AnalyzerError::Skipped("tool not found".to_owned())),
        })];
        let report = run_analyzers(&ctx_with(config), &analyzers);
        assert_eq!(report.verdict, Verdict::Quarantine);
        assert!(matches!(report.statuses[0].state, AnalyzerState::Failed { .. }));
    }

    #[test]
    fn panicking_analyzer_gates_instead_of_crashing() {
        let analyzers: Vec<Box<dyn Analyzer>> =
            vec![Box::new(FakeAnalyzer { id: "buggy", result: || panic!("bug") })];
        let report = run_analyzers(&ctx_with(AnalyzeConfig::default()), &analyzers);
        assert_eq!(report.verdict, Verdict::Quarantine);
    }

    #[test]
    fn findings_sorted_most_severe_first() {
        let analyzers: Vec<Box<dyn Analyzer>> = vec![Box::new(FakeAnalyzer {
            id: "multi",
            result: || {
                Ok(vec![
                    test_finding("multi/low", Severity::Low, Some("a.php")),
                    test_finding("multi/high", Severity::High, Some("a.php")),
                ])
            },
        })];
        let report = run_analyzers(&ctx_with(AnalyzeConfig::default()), &analyzers);
        assert_eq!(report.findings[0].rule_id, "multi/high");
    }

    #[test]
    fn analyze_rejects_unknown_analyzer_id() {
        let config = AnalyzeConfig::from_yaml("analyzers:\n  enable: [does-not-exist]").unwrap();
        let err = analyze(&std::env::temp_dir(), config).unwrap_err();
        assert!(format!("{err:#}").contains("unknown analyzer"));
    }

    #[test]
    fn analyze_rejects_required_not_enabled() {
        let config = AnalyzeConfig::from_yaml(
            "analyzers:\n  enable: [dangerous-sinks]\n  required: [malware-yara]",
        )
        .unwrap();
        let err = analyze(&std::env::temp_dir(), config).unwrap_err();
        assert!(format!("{err:#}").contains("not in the enabled set"));
    }

    #[test]
    fn analyze_rejects_missing_root() {
        let err =
            analyze(Path::new("Z:/definitely/not/here"), AnalyzeConfig::default()).unwrap_err();
        assert!(format!("{err:#}").contains("not a directory"));
    }

    #[test]
    fn scope_filter_drops_out_of_scope_findings_but_keeps_pathless() {
        let analyzers: Vec<Box<dyn Analyzer>> = vec![Box::new(EmitAnalyzer(vec![
            test_finding("e/changed", Severity::High, Some("changed.php")),
            test_finding("e/unchanged", Severity::High, Some("unchanged.php")),
            test_finding("e/pathless", Severity::High, None),
        ]))];
        let ctx = ctx_with(AnalyzeConfig::default())
            .with_scope(scope::Scope::from_paths(vec!["changed.php".to_owned()]));
        let report = run_analyzers(&ctx, &analyzers);
        let ids: Vec<&str> = report.findings.iter().map(|f| f.rule_id.as_str()).collect();
        assert_eq!(report.out_of_scope, 1);
        assert!(ids.contains(&"e/changed"));
        assert!(!ids.contains(&"e/unchanged"));
        // Pathless findings can never be dropped by scoping (fail-safe).
        assert!(ids.contains(&"e/pathless"));
    }

    #[test]
    fn operator_suppression_waives_and_is_counted() {
        let mut config = AnalyzeConfig::default();
        config.suppress = vec![config::SuppressRule {
            rule: "e/known".to_owned(),
            path: Some("legacy.php".to_owned()),
            reason: "vetted".to_owned(),
        }];
        let analyzers: Vec<Box<dyn Analyzer>> = vec![Box::new(EmitAnalyzer(vec![
            test_finding("e/known", Severity::High, Some("legacy.php")),
            test_finding("e/other", Severity::Low, Some("legacy.php")),
        ]))];
        let report = run_analyzers(&ctx_with(config), &analyzers);
        assert_eq!(report.waived, 1);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].rule_id, "e/other");
        assert_eq!(report.verdict, Verdict::Allow);
    }

    #[test]
    fn baseline_suppresses_known_findings_and_gates_only_new_ones() {
        // The baseline remembers e/known at line 5; the current run sees it
        // at line 50 (reformatted) — still suppressed. e/new gates.
        let mut known = test_finding("e/known", Severity::High, Some("a.php"));
        known.line = Some(5);
        let baseline = Baseline::from_findings(std::slice::from_ref(&known));
        let mut moved = known.clone();
        moved.line = Some(50);
        let analyzers: Vec<Box<dyn Analyzer>> = vec![Box::new(EmitAnalyzer(vec![
            moved,
            test_finding("e/new", Severity::High, Some("a.php")),
        ]))];
        let ctx = ctx_with(AnalyzeConfig::default()).with_baseline(baseline);
        let report = run_analyzers(&ctx, &analyzers);
        assert_eq!(report.baseline_suppressed, 1);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].rule_id, "e/new");
    }

    #[test]
    fn analyze_fails_closed_on_missing_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let config = AnalyzeConfig {
            baseline: Some(std::path::PathBuf::from("no-such-baseline.json")),
            ..AnalyzeConfig::default()
        };
        let err = analyze(dir.path(), config).unwrap_err();
        assert!(format!("{err:#}").contains("baseline"));
    }

    #[test]
    fn analyze_fails_closed_when_since_git_fails() {
        // A tempdir is not a git repository — the diff-aware run must error,
        // never silently degrade to an empty (or full) scan.
        let dir = tempfile::tempdir().unwrap();
        let mut config =
            AnalyzeConfig { since: Some("HEAD".to_owned()), ..AnalyzeConfig::default() };
        config.cache.enabled = false;
        assert!(analyze(dir.path(), config).is_err());
    }

    #[test]
    fn tenant_suppression_marker_is_flagged_and_never_honored() {
        // The two halves of operator-only suppression, end to end on a real
        // tree: (a) a tenant comment shaped like a suppression directive
        // does NOT waive the finding it sits next to; (b) the comment itself
        // becomes a finding. Then: (c) an operator suppress rule — the only
        // honored mechanism — waives the sink finding, while the
        // tenant-suppression-attempt finding still stands.
        let dir = tempfile::tempdir().unwrap();
        // The assert() sink with an inline "ignore" marker (assembled here,
        // never a webshell-shaped byte sequence — see dangerous_sinks docs).
        std::fs::write(dir.path().join("t.php"), "<?php\nassert($cond); // ephpm-analyze-ignore\n")
            .unwrap();
        let mut config =
            AnalyzeConfig::from_yaml("analyzers:\n  enable: [dangerous-sinks, suppression-scan]")
                .unwrap();
        config.cache.enabled = false;

        let report = analyze(dir.path(), config.clone()).unwrap();
        let ids: Vec<&str> = report.findings.iter().map(|f| f.rule_id.as_str()).collect();
        assert!(ids.contains(&"dangerous-sinks/assert"), "marker must not waive: {ids:?}");
        assert!(ids.contains(&"suppression-scan/tenant-suppression-attempt"), "{ids:?}");

        config.suppress = vec![config::SuppressRule {
            rule: "dangerous-sinks/assert".to_owned(),
            path: Some("t.php".to_owned()),
            reason: "vetted".to_owned(),
        }];
        let report = analyze(dir.path(), config).unwrap();
        let ids: Vec<&str> = report.findings.iter().map(|f| f.rule_id.as_str()).collect();
        assert!(!ids.contains(&"dangerous-sinks/assert"), "operator suppress waives: {ids:?}");
        assert!(ids.contains(&"suppression-scan/tenant-suppression-attempt"), "{ids:?}");
        assert_eq!(report.waived, 1);
    }
}
