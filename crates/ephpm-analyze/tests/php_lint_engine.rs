//! End-to-end test of the `php-lint` analyzer against a **real** embedded PHP
//! engine: a syntactically-invalid file produces exactly one confirmed
//! `php-lint/syntax-error` finding (with the line parsed from PHP's diagnostic)
//! and a clean file produces none — the `php -l` contract, in-process.
//!
//! Compiled only with the `php` feature; meaningful only against a PHP-linked
//! ephpm-php (`PHP_SDK_PATH` at build time), so the test is `#[ignore]`d — the
//! nightly workflow's `cargo nextest run --run-ignored ignored-only` PHP-linked
//! leg runs it, and a stub build returns early. This is an *integration* test
//! target on purpose: the Windows `/FORCE:MULTIPLE` test-link flag emitted by
//! this crate's build.rs applies to `--test` targets, not the lib unittest
//! binary.

#![cfg(feature = "php")]

use ephpm_analyze::analyzers::PhpLint;
use ephpm_analyze::{AnalysisCtx, AnalyzeConfig, Analyzer as _, Category, Confidence, Severity};

#[test]
#[ignore = "requires a PHP-linked build (nightly runs these via --run-ignored ignored-only)"]
fn flags_broken_files_and_passes_clean_ones() {
    if !ephpm_php::opcode::available() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    // A hard syntax error: an unterminated function signature.
    std::fs::write(dir.path().join("broken.php"), "<?php\nfunction f( {\n").expect("write");
    // Valid PHP — must not be flagged.
    std::fs::write(dir.path().join("ok.php"), "<?php\n$x = 1 + 2;\necho $x;\n").expect("write");
    // A .phtml template that is valid PHP too.
    std::fs::write(dir.path().join("view.phtml"), "<p><?= 1 + 1 ?></p>\n").expect("write");

    let ctx = AnalysisCtx::new(dir.path().to_path_buf(), AnalyzeConfig::default());
    let findings = PhpLint.run(&ctx).expect("engine lint");

    // Exactly one finding, on the broken file only.
    assert_eq!(findings.len(), 1, "{findings:?}");
    let f = &findings[0];
    assert_eq!(f.rule_id, "php-lint/syntax-error");
    assert_eq!(f.severity, Severity::High);
    assert_eq!(f.category, Category::Quality);
    assert_eq!(f.confidence, Confidence::Confirmed);
    assert_eq!(f.path.as_deref(), Some(std::path::Path::new("broken.php")));
    // PHP reports the parse error with a concrete line; it must survive into
    // the finding.
    assert!(f.line.is_some(), "expected a line number: {f:?}");
    assert!(f.message.contains("syntax error"), "message: {}", f.message);
}
