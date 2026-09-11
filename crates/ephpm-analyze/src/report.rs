//! The aggregated result of one analysis run: merged findings, per-analyzer
//! outcomes, and the policy decision.

use crate::finding::Finding;
use crate::policy::Verdict;

/// How one analyzer ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnalyzerState {
    /// Ran to completion (possibly with zero findings).
    Completed {
        /// Number of findings this analyzer contributed.
        findings: usize,
    },
    /// Could not run here (tool or inputs absent) and was not required —
    /// surfaced as a diagnostic, does not gate.
    Skipped {
        /// Why it was skipped.
        reason: String,
    },
    /// Errored, timed out, or was required but could not run — trips the
    /// fail-closed quarantine floor.
    Failed {
        /// The error, rendered.
        error: String,
    },
}

/// One analyzer's outcome within a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzerStatus {
    /// The analyzer's stable id.
    pub id: String,
    /// How it ended.
    pub state: AnalyzerState,
}

/// Everything one `ephpm analyze` run produced.
#[derive(Debug, Clone)]
pub struct AnalysisReport {
    /// All findings across all analyzers, sorted most severe first.
    pub findings: Vec<Finding>,
    /// Per-analyzer outcome, in execution order.
    pub statuses: Vec<AnalyzerStatus>,
    /// The gate's decision.
    pub verdict: Verdict,
    /// The weighted score (see `crate::policy` for the model).
    pub score: u32,
    /// Why the verdict is what it is — one line per policy layer that fired.
    pub reasons: Vec<String>,
}

impl AnalysisReport {
    /// Ids of analyzers whose state is [`AnalyzerState::Failed`].
    #[must_use]
    pub fn failed_analyzers(&self) -> Vec<&str> {
        self.statuses
            .iter()
            .filter(|s| matches!(s.state, AnalyzerState::Failed { .. }))
            .map(|s| s.id.as_str())
            .collect()
    }
}
