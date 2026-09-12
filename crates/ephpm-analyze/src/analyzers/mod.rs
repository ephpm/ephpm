//! The built-in analyzers.
//!
//! Six wrap external tools as subprocesses and degrade gracefully when the
//! tool is absent (`composer-audit`, `semgrep-php`, `malware-yara`,
//! `phpstan`, `psalm-taint`, `progpilot`); two are native with zero external
//! dependencies (`dangerous-sinks`, `suppression-scan`), sharing the per-file
//! walker in `php_files` — diff-aware scope skipping and the incremental cache
//! included. `opcode-scan` is native too but engine-backed: it compiles each
//! file with the embedded Zend compiler (no execution) and detects sinks in
//! the opcode stream, degrading to a skip on builds without a linked libphp.
//!
//! `phpstan`, `psalm-taint`, `progpilot`, and `opcode-scan` are registered but
//! **not** in the default `security` profile
//! ([`crate::config::Profile::default_analyzers`]) — they are opt-in via
//! `analyzers.enable`.

mod composer_audit;
mod dangerous_sinks;
mod opcode_scan;
mod php_files;
mod phpstan;
mod progpilot;
mod psalm_taint;
mod semgrep;
mod suppression_scan;
mod tool;
mod yara_scan;

pub use composer_audit::ComposerAudit;
pub use dangerous_sinks::DangerousSinks;
pub use opcode_scan::OpcodeScan;
pub use phpstan::PhpStan;
pub use progpilot::Progpilot;
pub use psalm_taint::PsalmTaint;
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
        Box::new(PhpStan),
        Box::new(PsalmTaint),
        Box::new(Progpilot),
        Box::new(DangerousSinks),
        Box::new(OpcodeScan),
        Box::new(SuppressionScan),
    ]
}
