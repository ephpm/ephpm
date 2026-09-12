//! `opcode-scan` — dangerous-call detection at the **opcode** level, using
//! ePHPm's embedded Zend engine.
//!
//! This is the first slice of the engine-backed analysis moat: every PHP
//! file is compiled — never executed — by the *same compiler that would run
//! it* (`ephpm_php::opcode::scan_file`, backed by `zend_compile_file`
//! inside the C wrapper), and the resulting opcode stream is searched for
//! dangerous call sites: the `eval` construct (`ZEND_INCLUDE_OR_EVAL` with
//! the `ZEND_EVAL` type) and statically-named calls
//! (`ZEND_INIT_FCALL` / `ZEND_INIT_FCALL_BY_NAME` /
//! `ZEND_INIT_NS_FCALL_BY_NAME`) to the sink functions below. Nested
//! op_arrays — functions, methods, closures, arrow functions, conditional
//! declarations — are walked too.
//!
//! Why this outclasses the `dangerous-sinks` token pass (which coexists,
//! unchanged): opcodes have no comments and no strings-as-code, and the
//! compiler has already lowered constructs a text scan cannot see (the
//! backtick operator becomes a real `shell_exec` call). A hit here is a
//! genuine call site, so findings are [`Confidence::Confirmed`] — they can
//! participate in hard-deny escalation, where the token pass's `Suspected`
//! hits never can. PHPStan/Psalm/semgrep re-implement PHP parsing; this
//! analyzer asks the runtime's own compiler, so it can never disagree with
//! what would actually execute.
//!
//! **Opt-in**: not part of the `security` profile — enable with
//! `analyzers.enable: [opcode-scan, ...]`. It needs a PHP-linked build of
//! ePHPm; without one (the `php` cargo feature off, or a stub build with
//! no libphp) it degrades to [`AnalyzerError::Skipped`] — list it in
//! `analyzers.required` to make that skip gate instead.
//!
//! Fail-closed: a file that does not compile (parse error, unreadable,
//! fatal compile error) is a **gating analyzer error** — code the engine
//! cannot compile cannot be vouched for, so the run can never `allow`. The
//! scan stops at the first such file (a bailout may leave the engine
//! degraded, so continuing would risk misscanning the rest).
//!
//! The incremental cache is deliberately **not** used here: cached entries
//! are keyed on file content + config + crate version, but these results
//! also depend on the embedded PHP version, which is outside that key — a
//! PHP upgrade must never serve stale verdicts.
//!
//! Later phases (out of scope for this slice, tracked in
//! `site/content/roadmap/opcode-analysis.md`):
//! - TODO(taint): superglobal→sink data-flow — flag when a sink's argument
//!   operand is fed by a `ZEND_FETCH_*` of `$_GET`/`$_POST`/`$_REQUEST`/
//!   `$_COOKIE` and bump severity accordingly.
//! - L2: sandboxed detonation of suspicious inputs (the `engine:` config
//!   section, parsed but inert today).

use crate::analyzer::{AnalysisCtx, Analyzer, AnalyzerError};
use crate::finding::{Category, Severity};

/// See the module docs.
pub struct OpcodeScan;

/// The analyzer's stable id.
pub const ID: &str = "opcode-scan";

/// The sinks flagged at the opcode level, with per-sink severity: High for
/// the RCE-class sinks, Medium for `assert` (RCE only via string payloads,
/// long deprecated) and `unserialize` (object injection — exploitability
/// depends on the available gadget chains).
// Without the `php` feature only the tests read this table — the non-test
// stub build must still compile warning-free.
#[cfg_attr(not(feature = "php"), allow(dead_code))]
const SINKS: &[(&str, Severity)] = &[
    ("eval", Severity::High),
    ("system", Severity::High),
    ("exec", Severity::High),
    ("shell_exec", Severity::High),
    ("passthru", Severity::High),
    ("proc_open", Severity::High),
    ("popen", Severity::High),
    ("create_function", Severity::High),
    ("assert", Severity::Medium),
    ("unserialize", Severity::Medium),
];

impl Analyzer for OpcodeScan {
    fn id(&self) -> &str {
        ID
    }

    fn category(&self) -> Category {
        Category::Security
    }

    fn run(&self, ctx: &AnalysisCtx) -> Result<Vec<crate::finding::Finding>, AnalyzerError> {
        run_impl(ctx)
    }
}

/// Built without the `php` feature: the engine dependency is not even in
/// the graph. Degrade to a skip (which `analyzers.required` can upgrade).
#[cfg(not(feature = "php"))]
fn run_impl(_ctx: &AnalysisCtx) -> Result<Vec<crate::finding::Finding>, AnalyzerError> {
    Err(AnalyzerError::Skipped(
        "opcode-scan requires a PHP-linked build of ephpm (this binary was built without \
         PHP support)"
            .to_owned(),
    ))
}

#[cfg(feature = "php")]
fn run_impl(ctx: &AnalysisCtx) -> Result<Vec<crate::finding::Finding>, AnalyzerError> {
    engine::run(ctx)
}

/// The engine-backed implementation. Compiled only with the `php` feature;
/// whether libphp is actually linked is ephpm-php's build-time concern
/// (`opcode::available()`), checked at runtime here — so this crate never
/// needs the `php_linked` cfg and stub builds stay green.
#[cfg(feature = "php")]
mod engine {
    use std::path::Path;

    use ephpm_php::opcode::{OpcodeScanError, scan_file};

    use super::{ID, SINKS};
    use crate::analyzer::{AnalysisCtx, AnalyzerError};
    use crate::analyzers::php_files;
    use crate::finding::{Category, Confidence, Finding, Severity};

    pub(super) fn run(ctx: &AnalysisCtx) -> Result<Vec<Finding>, AnalyzerError> {
        if !ephpm_php::opcode::available() {
            return Err(AnalyzerError::Skipped(
                "opcode-scan requires a PHP-linked build of ephpm (this binary links no \
                 libphp — stub build)"
                    .to_owned(),
            ));
        }
        // Idempotent; `ephpm analyze` is a one-shot CLI process, so the
        // engine is torn down by process exit, not an explicit shutdown.
        ephpm_php::PhpRuntime::init().map_err(|e| {
            AnalyzerError::ToolFailed(format!("failed to initialize the embedded PHP engine: {e}"))
        })?;

        let mut findings = Vec::new();
        walk(ctx, ctx.root(), &mut findings)?;
        Ok(findings)
    }

    /// Same tree-walk contract as `php_files::scan_php_tree` (PHP files
    /// only, `.git` skipped, oversized files skipped, diff-aware scope) —
    /// but passing absolute paths to the engine and **without** the result
    /// cache (see the module docs for why).
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
                scan_one(&path, &rel, findings)?;
            }
        }
        Ok(())
    }

    fn scan_one(abs: &Path, rel: &Path, findings: &mut Vec<Finding>) -> Result<(), AnalyzerError> {
        let sink_names: Vec<&str> = SINKS.iter().map(|&(name, _)| name).collect();
        let hits = scan_file(abs, &sink_names).map_err(|e| match e {
            // A file the engine cannot compile gates the run (fail-closed):
            // it cannot be vouched for, and a bailout may have degraded the
            // engine, so the scan is terminal here.
            OpcodeScanError::Compile(_) | OpcodeScanError::Fatal(_) => {
                AnalyzerError::ToolFailed(format!("{} in {}", e, rel.display()))
            }
            other => AnalyzerError::ToolFailed(other.to_string()),
        })?;

        for hit in hits {
            let severity = SINKS
                .iter()
                .find(|&&(name, _)| name == hit.sink)
                .map_or(Severity::Medium, |&(_, sev)| sev);
            let message = if hit.sink == "eval" {
                "eval construct compiled at the opcode level — dangerous sink".to_owned()
            } else {
                format!("call to {}() compiled at the opcode level — dangerous sink", hit.sink)
            };
            findings.push(Finding {
                rule_id: format!("{ID}/{}", hit.sink),
                severity,
                category: Category::Security,
                path: Some(rel.to_path_buf()),
                line: Some(u64::from(hit.line)),
                message,
                // The engine's own compiler produced this call site: no
                // comment/string false positives are possible, so the
                // finding is confirmed evidence, not a hotspot.
                confidence: Confidence::Confirmed,
            });
        }
        Ok(())
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
    fn every_sink_has_a_documented_severity_class() {
        // High for RCE-class sinks, Medium for assert/unserialize — the
        // contract the docs state.
        for &(name, severity) in SINKS {
            match name {
                "assert" | "unserialize" => assert_eq!(severity, Severity::Medium, "{name}"),
                _ => assert_eq!(severity, Severity::High, "{name}"),
            }
        }
    }

    /// Without an engine the analyzer must degrade to a skip whose reason
    /// names the PHP-linked requirement — never error, never a silent clean
    /// pass. (With a real engine linked — nightly's PHP-linked test leg —
    /// the skip assertion is meaningless; the end-to-end behaviour is
    /// covered by `detects_and_stays_precise_with_a_real_engine`.)
    #[test]
    fn skips_without_an_engine() {
        #[cfg(feature = "php")]
        if ephpm_php::opcode::available() {
            return;
        }
        let err = OpcodeScan.run(&ctx()).expect_err("must not produce findings");
        assert!(err.is_skip(), "{err}");
        assert!(err.to_string().contains("PHP-linked"), "{err}");
    }

    // The end-to-end test with a real engine lives in
    // `tests/opcode_engine.rs` — it must be an *integration* test target so
    // the Windows `/FORCE:MULTIPLE` test-link flag (see build.rs) applies;
    // cargo's `rustc-link-arg-tests` does not reach the lib unittest
    // binary.
}
