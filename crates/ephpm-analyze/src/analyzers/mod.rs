//! The built-in analyzers.
//!
//! Three wrap external tools as subprocesses and degrade gracefully when the
//! tool is absent (`composer-audit`, `semgrep-php`, `malware-yara`); two are
//! native with zero external dependencies (`dangerous-sinks`,
//! `suppression-scan`), sharing the per-file walker in `php_files` —
//! diff-aware scope skipping and the incremental cache included — the same
//! path a future opcode analyzer will use.

mod composer_audit;
mod dangerous_sinks;
mod php_files;
mod semgrep;
mod suppression_scan;
mod tool;
mod yara_scan;

pub use composer_audit::ComposerAudit;
pub use dangerous_sinks::DangerousSinks;
pub use semgrep::SemgrepPhp;
pub use suppression_scan::SuppressionScan;
pub use yara_scan::MalwareYara;

use crate::analyzer::Analyzer;

/// All analyzers this binary ships, in their default execution order.
#[must_use]
pub fn built_in() -> Vec<Box<dyn Analyzer>> {
    vec![
        Box::new(ComposerAudit),
        Box::new(SemgrepPhp),
        Box::new(MalwareYara),
        Box::new(DangerousSinks),
        Box::new(SuppressionScan),
    ]
}
