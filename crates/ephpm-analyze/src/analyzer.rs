//! The `Analyzer` trait, the shared `AnalysisCtx` handed to every analyzer,
//! and the error taxonomy the aggregator's fail-closed logic keys on.

use std::path::{Path, PathBuf};

use crate::config::AnalyzeConfig;
use crate::finding::{Category, Finding};

/// Everything an analyzer may read about the target under analysis.
///
/// Phase 1 carries the checkout root and the effective configuration. The
/// fields are private on purpose — analyzers go through accessors — so later
/// phases can add **lazily populated shared state** without touching any
/// analyzer's signature:
///
/// - *Planned — not yet implemented (Phase 2, opcode analyzers):* a
///   `compiled()` accessor returning a lazily built compiled representation
///   of the tree's PHP files (op_array-level), built at most once and shared
///   by every opcode analyzer.
/// - *Planned — not yet implemented (Phase 4, engine-in-the-loop):* an
///   `engine()` accessor exposing a sandboxed embedded-PHP evaluation handle,
///   gated by `engine.detonate` / `engine.timeout_ms` (parsed today, inert).
///
/// Both slot in as additional private fields plus accessors; the `Analyzer`
/// trait and `run(&self, ctx: &AnalysisCtx)` shape do not change.
#[derive(Debug)]
pub struct AnalysisCtx {
    root: PathBuf,
    config: AnalyzeConfig,
}

impl AnalysisCtx {
    /// Build a context for the checkout at `root` with the given effective
    /// configuration.
    #[must_use]
    pub fn new(root: PathBuf, config: AnalyzeConfig) -> Self {
        Self { root, config }
    }

    /// The directory being analyzed.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The effective (file + CLI-override) configuration.
    #[must_use]
    pub fn config(&self) -> &AnalyzeConfig {
        &self.config
    }
}

/// Why an analyzer did not produce a normal result.
///
/// The distinction between [`AnalyzerError::Skipped`] and every other variant
/// is the crux of the fail-closed model: *absence degrades, errors gate*.
///
/// - `Skipped` means the analyzer could not meaningfully run at all — its
///   external tool is not installed, or the target lacks the inputs it needs
///   (no `composer.json`, no YARA ruleset configured). A skip is reported as
///   a diagnostic and does **not** floor the verdict — unless the analyzer is
///   listed in `analyzers.required`, in which case the aggregator upgrades
///   the skip to a failure.
/// - Every other variant is a real error on a path that *should* have worked
///   (tool crashed, unparseable output, timeout, I/O failure) and floors the
///   verdict at [`crate::policy::Verdict::Quarantine`]: an analyzer that
///   errored might have been about to find something, so the result can
///   never be `Allow`.
#[derive(Debug, thiserror::Error)]
pub enum AnalyzerError {
    /// The analyzer cannot run in this environment / against this target.
    /// Not a failure — see the type-level docs.
    #[error("skipped: {0}")]
    Skipped(String),
    /// The external tool ran but reported a failure the analyzer cannot
    /// interpret as results.
    #[error("tool failed: {0}")]
    ToolFailed(String),
    /// The external tool exceeded `analyzers.tool_timeout_ms` and was killed.
    #[error("timed out after {0} ms")]
    Timeout(u64),
    /// The tool's output could not be parsed.
    #[error("output parse error: {0}")]
    Parse(String),
    /// An I/O error while walking or reading the target tree.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

impl AnalyzerError {
    /// `true` for the [`AnalyzerError::Skipped`] variant — the only variant
    /// that does not trip the fail-closed quarantine floor.
    #[must_use]
    pub fn is_skip(&self) -> bool {
        matches!(self, Self::Skipped(_))
    }
}

/// A single analysis pass over the target.
///
/// Implementations must be side-effect-free with respect to the target tree
/// (read-only) and must map "my tool / inputs are absent" to
/// [`AnalyzerError::Skipped`] rather than erroring — see [`AnalyzerError`]
/// for why the distinction matters.
pub trait Analyzer {
    /// Stable identifier, used in `analyzers.enable`, `analyzers.required`,
    /// diagnostics, and as the namespace prefix of this analyzer's rule ids.
    fn id(&self) -> &str;

    /// The category this analyzer's findings fall under.
    fn category(&self) -> Category;

    /// Run the analysis and return findings (possibly none).
    ///
    /// # Errors
    ///
    /// [`AnalyzerError::Skipped`] when the analyzer cannot run here (tool or
    /// inputs absent); any other variant for a real failure, which the
    /// aggregator treats as fail-closed.
    fn run(&self, ctx: &AnalysisCtx) -> Result<Vec<Finding>, AnalyzerError>;
}
