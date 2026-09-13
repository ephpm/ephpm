//! The built-in analyzers.
//!
//! Ten wrap external tools as subprocesses and degrade gracefully when the
//! tool is absent (`composer-audit`, `semgrep-php`, `malware-yara`, `clamav`,
//! `phpstan`, `psalm-taint`, `progpilot`, `phpcs`, `phpmd`, `rector`); four
//! are native with zero external dependencies (`dangerous-sinks`,
//! `suppression-scan`, `obfuscation-scan`, `secrets-scan`), sharing the
//! per-file walker in
//! `php_files` — diff-aware scope skipping and the incremental cache included.
//! `opcode-scan` and `php-lint` are native too but engine-backed:
//! `opcode-scan` compiles each file with the embedded Zend compiler (no
//! execution) and detects sinks in the opcode stream; `php-lint` compiles each
//! file for syntax validity alone (`php -l`, in-process). Both degrade to a
//! skip on builds without a linked libphp. `wp-vuln` is native and offline
//! too, but data-driven rather than per-file: it matches installed WordPress
//! plugin/theme/core versions against an operator-supplied Wordfence
//! Intelligence vulnerability feed (unset → skip; configured-but-missing →
//! error), the WordPress analogue of `composer-audit`. `writable-exec` is
//! native and offline too and likewise path-driven rather than
//! content-scanning: it flags executable PHP (`*.php`/`*.phtml`/`*.php5`/
//! `*.phar`) living where only uploaded data belongs — confirmed under
//! `wp-content/uploads/`, suspected under operator-listed
//! `analyzers.writable_exec_dirs` — catching a dropped webshell by *location*,
//! complementing the content scanners.
//!
//! `clamav`, `phpstan`, `psalm-taint`, `progpilot`, `phpcs`, `phpmd`,
//! `rector`, `opcode-scan`, `php-lint`, `wp-vuln`, `obfuscation-scan`,
//! `secrets-scan`, and `writable-exec` are
//! registered but **not** in the default `security` profile
//! ([`crate::config::Profile::default_analyzers`]) — they are opt-in via
//! `analyzers.enable`.

mod clamav;
mod composer_audit;
mod dangerous_sinks;
mod obfuscation_scan;
mod opcode_scan;
mod php_files;
mod php_lint;
mod phpcs;
mod phpmd;
mod phpstan;
mod progpilot;
mod psalm_taint;
mod rector;
mod secrets_scan;
mod semgrep;
mod suppression_scan;
mod tool;
mod wp_vuln;
mod writable_exec;
mod yara_scan;

pub use clamav::Clamav;
pub use composer_audit::ComposerAudit;
pub use dangerous_sinks::DangerousSinks;
pub use obfuscation_scan::ObfuscationScan;
pub use opcode_scan::OpcodeScan;
pub use php_lint::PhpLint;
pub use phpcs::Phpcs;
pub use phpmd::Phpmd;
pub use phpstan::PhpStan;
pub use progpilot::Progpilot;
pub use psalm_taint::PsalmTaint;
pub use rector::Rector;
pub use secrets_scan::SecretsScan;
pub use semgrep::SemgrepPhp;
pub use suppression_scan::SuppressionScan;
pub use wp_vuln::WpVuln;
pub use writable_exec::WritableExec;
pub use yara_scan::MalwareYara;

use crate::analyzer::Analyzer;

/// All analyzers this binary ships, in their default execution order.
#[must_use]
pub fn built_in() -> Vec<Box<dyn Analyzer>> {
    vec![
        Box::new(ComposerAudit),
        Box::new(SemgrepPhp),
        Box::new(MalwareYara),
        Box::new(Clamav),
        Box::new(PhpStan),
        Box::new(PsalmTaint),
        Box::new(Progpilot),
        Box::new(Phpcs),
        Box::new(Phpmd),
        Box::new(Rector),
        Box::new(DangerousSinks),
        Box::new(OpcodeScan),
        Box::new(PhpLint),
        Box::new(WpVuln),
        Box::new(SuppressionScan),
        Box::new(ObfuscationScan),
        Box::new(SecretsScan),
        Box::new(WritableExec),
    ]
}

#[cfg(test)]
mod tests {
    use super::built_in;
    use crate::config::Profile;

    #[test]
    fn wave2_analyzers_are_registered_but_opt_in() {
        // Registered (so `analyzers.enable` accepts them) yet absent from the
        // default `security` profile (so the out-of-the-box gate is unchanged).
        let ids: Vec<String> = built_in().iter().map(|a| a.id().to_owned()).collect();
        let profile = Profile::Security.default_analyzers();
        for id in [
            "clamav",
            "phpcs",
            "phpmd",
            "rector",
            "php-lint",
            "wp-vuln",
            "obfuscation-scan",
            "secrets-scan",
            "writable-exec",
        ] {
            assert!(ids.contains(&id.to_owned()), "{id} must be registered: {ids:?}");
            assert!(!profile.contains(&id.to_owned()), "{id} must be opt-in, not in the profile");
        }
    }
}
