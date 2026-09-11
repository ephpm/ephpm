//! ephpm-analyze — static-analysis aggregator and fail-closed deploy gate
//! for PHP applications (`ephpm analyze`).
//!
//! # Architecture (Phase 1)
//!
//! ```text
//!   .ephpm-analyze.yml ──► AnalyzeConfig ──► AnalysisCtx
//!                                               │
//!                     ┌─────────────────────────┼──────────────┐
//!                     ▼                         ▼              ▼
//!             external-tool analyzers    native analyzers   (planned:
//!             composer-audit             dangerous-sinks     opcode/L1,
//!             semgrep-php                                    engine/L2)
//!             malware-yara
//!                     │                         │
//!                     └────────► Findings ◄─────┘
//!                                   │
//!                            policy::decide ──► Verdict ──► exit code
//!                                   │
//!                        output::{to_text, to_sarif}
//! ```
//!
//! The aggregator runs every enabled [`Analyzer`], merges their
//! [`Finding`]s, and hands the lot to the policy engine, which produces a
//! [`Verdict`] (`Allow` / `Quarantine` / `Deny`). The whole pipeline is
//! **fail-closed**: an analyzer that errors, times out, panics, or is
//! `required` but cannot run floors the verdict at `Quarantine` — only a run
//! where everything that should have spoken actually spoke can `Allow`. An
//! *optional* analyzer whose external tool is simply not installed is
//! reported as skipped and does not gate (see [`analyzer::AnalyzerError`]).
//!
//! Later phases (opcode-level analysis of compiled PHP, engine-in-the-loop
//! detonation) slot in as more [`Analyzer`] implementations reading richer
//! state off [`AnalysisCtx`] — the trait, the finding shape, the policy
//! engine, and both output formats are already final for them.

pub mod analyzer;
pub mod analyzers;
pub mod config;
pub mod finding;
pub mod output;
pub mod policy;
pub mod report;

use std::path::Path;

use anyhow::Context as _;

pub use crate::analyzer::{AnalysisCtx, Analyzer, AnalyzerError};
pub use crate::config::{AnalyzeConfig, FailOn, OutputFormat, Profile};
pub use crate::finding::{Category, Finding, Severity};
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

    // Most severe first, then stable by rule id for deterministic output.
    findings.sort_by(|a, b| b.severity.cmp(&a.severity).then_with(|| a.rule_id.cmp(&b.rule_id)));

    let failed: Vec<String> = statuses
        .iter()
        .filter(|s| matches!(s.state, AnalyzerState::Failed { .. }))
        .map(|s| s.id.clone())
        .collect();
    let decision = policy::decide(
        &findings,
        &ctx.config().analyzers.deny_hard,
        &failed,
        ctx.config().policy.quarantine_score,
        ctx.config().policy.deny_score,
    );

    AnalysisReport {
        findings,
        statuses,
        verdict: decision.verdict,
        score: decision.score,
        reasons: decision.reasons,
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
/// On a missing/non-directory `root`, an unknown analyzer id, or a
/// `required` entry that is not enabled. Individual analyzer failures do
/// **not** error — they are folded into the report fail-closed.
pub fn analyze(root: &Path, config: AnalyzeConfig) -> anyhow::Result<AnalysisReport> {
    anyhow::ensure!(root.is_dir(), "analysis target {} is not a directory", root.display());

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
    let ctx = AnalysisCtx::new(root, config);
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
                    Finding {
                        rule_id: "multi/low".to_owned(),
                        severity: Severity::Low,
                        category: Category::Quality,
                        path: None,
                        line: None,
                        message: "l".to_owned(),
                    },
                    Finding {
                        rule_id: "multi/high".to_owned(),
                        severity: Severity::High,
                        category: Category::Security,
                        path: None,
                        line: None,
                        message: "h".to_owned(),
                    },
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
}
