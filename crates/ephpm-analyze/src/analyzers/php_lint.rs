//! `php-lint` — native syntax validity check via the **embedded Zend
//! compiler** (`php -l`, done in-process).
//!
//! Every `*.php` / `*.phtml` file is compiled — never executed — by the same
//! compiler that would run it, and a file that fails to compile becomes a
//! [`Severity::High`] finding carrying PHP's own diagnostic. This is the
//! `php -l` semantics ("No syntax errors detected" vs a `PHP Parse error`),
//! but in-process and over the whole tree.
//!
//! **Implementation: reuse, not a second FFI surface.** This compiles each
//! file through the *existing* [`ephpm_php::opcode::scan_file`] entry point —
//! the very same `zend_compile_file`-under-`ZEND_COMPILE_WITHOUT_EXECUTION`
//! path `opcode-scan` uses — passing an **empty sink list**. With no sinks the
//! wrapper walks the opcodes but matches nothing, so the only outcome that
//! matters is whether compilation *succeeded*: a clean compile yields no hits
//! (no finding), a parse error surfaces as [`OpcodeScanError::Compile`]. Adding
//! a dedicated `ephpm_php_lint_file` FFI symbol would duplicate the wrapper's
//! `zend_try`/`zend_catch` guard, the thread-registration dance, and the
//! symbol-table pruning for no behavioural gain — the compile-only outcome is
//! already exactly what `scan_file` reports.
//!
//! **Severity/category.** A file that does not compile is a genuine, engine-
//! confirmed defect, so findings are [`Confidence::Confirmed`] and
//! [`Severity::High`]. They are [`Category::Quality`] — the finding taxonomy
//! has no `Correctness` variant, and `Quality` is documented as "correctness /
//! robustness problems not directly exploitable", which is precisely a syntax
//! error (the same reasoning `phpstan` uses). `rule_id = php-lint/syntax-error`;
//! the line is parsed from PHP's own ` on line N` suffix.
//!
//! **Recoverable vs terminal.** A plain parse error (`Compile`) leaves the
//! engine clean — the wrapper prunes any partially-registered symbols on that
//! path — so linting **continues** to the next file, collecting one finding
//! per broken file. A *fatal* compile error / zend bailout (`Fatal`, e.g. an
//! in-file redeclaration) may leave the engine mid-mutation; that is treated as
//! a **gating analyzer error** (fail-closed) and stops the run, mirroring
//! `opcode-scan`. Documented limitation: the rare in-file-fatal case gates the
//! run rather than producing a per-file `syntax-error` finding.
//!
//! **Opt-in**: not part of the `security` profile — enable with
//! `analyzers.enable: [php-lint, ...]`. It needs a PHP-linked build of ePHPm;
//! without one (the `php` cargo feature off, or a stub build with no libphp)
//! it degrades to [`AnalyzerError::Skipped`] — list it in `analyzers.required`
//! to make that skip gate instead.
//!
//! The incremental cache is deliberately **not** used (same reason as
//! `opcode-scan`): results depend on the embedded PHP version, which is outside
//! the cache key, so a PHP upgrade must never serve a stale verdict.

use crate::analyzer::{AnalysisCtx, Analyzer, AnalyzerError};
use crate::finding::{Category, Finding};

/// See the module docs.
pub struct PhpLint;

/// The analyzer's stable id.
pub const ID: &str = "php-lint";

impl Analyzer for PhpLint {
    fn id(&self) -> &str {
        ID
    }

    fn category(&self) -> Category {
        Category::Quality
    }

    fn run(&self, ctx: &AnalysisCtx) -> Result<Vec<Finding>, AnalyzerError> {
        run_impl(ctx)
    }
}

/// Built without the `php` feature: the engine dependency is not even in the
/// graph. Degrade to a skip (which `analyzers.required` can upgrade).
#[cfg(not(feature = "php"))]
fn run_impl(_ctx: &AnalysisCtx) -> Result<Vec<Finding>, AnalyzerError> {
    Err(AnalyzerError::Skipped(
        "php-lint requires a PHP-linked build of ephpm (this binary was built without PHP \
         support)"
            .to_owned(),
    ))
}

#[cfg(feature = "php")]
fn run_impl(ctx: &AnalysisCtx) -> Result<Vec<Finding>, AnalyzerError> {
    engine::run(ctx)
}

/// Parse the 1-based line from PHP's diagnostic, which the C wrapper renders
/// with a trailing ` on line N`. `None` when absent (e.g. an unopenable file).
/// Shared by the engine path and its tests.
#[cfg_attr(not(feature = "php"), allow(dead_code))]
fn line_from_message(msg: &str) -> Option<u64> {
    let (_, tail) = msg.rsplit_once(" on line ")?;
    tail.trim().parse::<u64>().ok()
}

/// The engine-backed implementation. Compiled only with the `php` feature;
/// whether libphp is actually linked is ephpm-php's build-time concern
/// (`opcode::available()`), checked at runtime here — so this crate never
/// needs the `php_linked` cfg and stub builds stay green.
#[cfg(feature = "php")]
mod engine {
    use std::path::Path;

    use ephpm_php::opcode::{OpcodeScanError, scan_file};

    use super::{ID, line_from_message};
    use crate::analyzer::{AnalysisCtx, AnalyzerError};
    use crate::analyzers::php_files;
    use crate::finding::{Category, Confidence, Finding, Severity};

    pub(super) fn run(ctx: &AnalysisCtx) -> Result<Vec<Finding>, AnalyzerError> {
        if !ephpm_php::opcode::available() {
            return Err(AnalyzerError::Skipped(
                "php-lint requires a PHP-linked build of ephpm (this binary links no libphp — \
                 stub build)"
                    .to_owned(),
            ));
        }
        // Idempotent; `ephpm analyze` is a one-shot CLI process, so the engine
        // is torn down by process exit, not an explicit shutdown.
        ephpm_php::PhpRuntime::init().map_err(|e| {
            AnalyzerError::ToolFailed(format!("failed to initialize the embedded PHP engine: {e}"))
        })?;

        let mut findings = Vec::new();
        walk(ctx, ctx.root(), &mut findings)?;
        Ok(findings)
    }

    /// Same tree-walk contract as `opcode-scan` (PHP files only, `.git`
    /// skipped, oversized files skipped, diff-aware scope) — absolute paths to
    /// the engine, and **no** result cache (see the module docs).
    fn walk(
        ctx: &AnalysisCtx,
        dir: &Path,
        findings: &mut Vec<Finding>,
    ) -> Result<(), AnalyzerError> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let path = entry.path();
            if file_type.is_dir() {
                if entry.file_name() == ".git" {
                    continue;
                }
                walk(ctx, &path, findings)?;
            } else if file_type.is_file() && php_files::is_php_file(&path) {
                let rel = path.strip_prefix(ctx.root()).unwrap_or(&path).to_path_buf();
                if !ctx.is_in_scope(&rel) {
                    continue;
                }
                if entry.metadata()?.len() > php_files::MAX_FILE_BYTES {
                    continue;
                }
                lint_one(&path, &rel, findings)?;
            }
        }
        Ok(())
    }

    fn lint_one(abs: &Path, rel: &Path, findings: &mut Vec<Finding>) -> Result<(), AnalyzerError> {
        // Empty sink list: we only care whether the file *compiles*, not what
        // it calls. A clean compile returns no hits; a parse error surfaces as
        // `Compile`; a fatal bailout as `Fatal`.
        match scan_file(abs, &[]) {
            // Compiled cleanly — no syntax-error finding.
            Ok(_) => Ok(()),
            // A parse error: the wrapper left the engine clean (pruned), so we
            // record the finding and keep linting the rest of the tree.
            Err(OpcodeScanError::Compile(msg)) => {
                findings.push(Finding {
                    rule_id: format!("{ID}/syntax-error"),
                    severity: Severity::High,
                    category: Category::Quality,
                    path: Some(rel.to_path_buf()),
                    line: line_from_message(&msg),
                    message: format!("PHP syntax error: {msg}"),
                    // The engine's own compiler rejected the file — a fact.
                    confidence: Confidence::Confirmed,
                });
                Ok(())
            }
            // A fatal compile error / bailout may have left the engine
            // mid-mutation; treat as terminal and gate (fail-closed), mirroring
            // opcode-scan. Rare (e.g. an in-file redeclaration).
            Err(e @ (OpcodeScanError::Fatal(_) | OpcodeScanError::NoEngine)) => {
                Err(AnalyzerError::ToolFailed(format!("{e} in {}", rel.display())))
            }
            Err(e) => Err(AnalyzerError::ToolFailed(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AnalyzeConfig, Profile};

    fn ctx() -> AnalysisCtx {
        AnalysisCtx::new(std::env::temp_dir(), AnalyzeConfig::default())
    }

    #[test]
    fn registered_but_not_in_the_security_profile() {
        // Opt-in: known to the registry (so `analyzers.enable` accepts it),
        // absent from the default profile (so default gate behaviour is
        // unchanged).
        let ids: Vec<String> =
            crate::analyzers::built_in().iter().map(|a| a.id().to_owned()).collect();
        assert!(ids.contains(&ID.to_owned()), "{ids:?}");
        assert!(!Profile::Security.default_analyzers().contains(&ID.to_owned()));
    }

    #[test]
    fn line_is_parsed_from_phps_on_line_suffix() {
        assert_eq!(line_from_message("syntax error, unexpected token \"}\" on line 5"), Some(5));
        // The last ` on line N` wins even if the text mentions lines earlier.
        assert_eq!(line_from_message("unexpected end on line 3 on line 42"), Some(42));
        // No suffix (e.g. an unopenable file) → no line.
        assert_eq!(line_from_message("could not open or compile file"), None);
    }

    /// Without an engine the analyzer must degrade to a skip whose reason names
    /// the PHP-linked requirement — never error, never a silent clean pass.
    /// (With a real engine linked — nightly's PHP-linked test leg — the skip
    /// assertion is meaningless; the end-to-end behaviour is covered by
    /// `tests/php_lint_engine.rs`.)
    #[test]
    fn skips_without_an_engine() {
        #[cfg(feature = "php")]
        if ephpm_php::opcode::available() {
            return;
        }
        let err = PhpLint.run(&ctx()).expect_err("must not produce findings");
        assert!(err.is_skip(), "{err}");
        assert!(err.to_string().contains("PHP-linked"), "{err}");
    }

    // The end-to-end test with a real engine lives in
    // `tests/php_lint_engine.rs` — it must be an *integration* test target so
    // the Windows `/FORCE:MULTIPLE` test-link flag (see build.rs) applies;
    // cargo's `rustc-link-arg-tests` does not reach the lib unittest binary.
}
