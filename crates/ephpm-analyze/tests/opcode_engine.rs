//! End-to-end test of the `opcode-scan` analyzer against a **real** embedded
//! PHP engine: detection is opcode-level (`Confidence::Confirmed`) and
//! precise (a sink name inside a comment or string literal produces
//! nothing) — the claim that separates it from the `dangerous-sinks` token
//! pass.
//!
//! Compiled only with the `php` feature; meaningful only against a
//! PHP-linked ephpm-php (`PHP_SDK_PATH` at build time), so the test is
//! `#[ignore]`d — the nightly workflow's
//! `cargo nextest run --run-ignored ignored-only` PHP-linked leg runs it,
//! and a stub build returns early. This is an *integration* test target on
//! purpose: the Windows `/FORCE:MULTIPLE` test-link flag emitted by this
//! crate's build.rs applies to `--test` targets, not the lib unittest
//! binary.
//!
//! Fixture source is assembled at runtime so sink-call byte sequences never
//! sit in this file on disk (endpoint antivirus quarantines
//! webshell-shaped sources — see dangerous_sinks' module docs).

#![cfg(feature = "php")]

use ephpm_analyze::analyzers::OpcodeScan;
use ephpm_analyze::{AnalysisCtx, AnalyzeConfig, Analyzer as _, Confidence};

#[test]
#[ignore = "requires a PHP-linked build (nightly runs these via --run-ignored ignored-only)"]
fn detects_and_stays_precise_with_a_real_engine() {
    if !ephpm_php::opcode::available() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let sys = "system";
    std::fs::write(
        dir.path().join("hot.php"),
        format!("<?php\n{sys}($cmd);\nfunction f($c) {{ return ev{}($c); }}\n", "al"),
    )
    .expect("write");
    std::fs::write(
        dir.path().join("clean.php"),
        format!("<?php\n// {sys}($cmd) in a comment\n$s = '{sys}($cmd)';\necho $s;\n"),
    )
    .expect("write");

    let ctx = AnalysisCtx::new(dir.path().to_path_buf(), AnalyzeConfig::default());
    let findings = OpcodeScan.run(&ctx).expect("engine scan");
    let rules: Vec<&str> = findings.iter().map(|f| f.rule_id.as_str()).collect();
    assert!(rules.contains(&"opcode-scan/system"), "{rules:?}");
    assert!(rules.contains(&"opcode-scan/eval"), "{rules:?}");
    assert!(findings.iter().all(|f| f.confidence == Confidence::Confirmed), "{findings:?}");
    // The precision claim: nothing at all from clean.php.
    assert!(
        findings.iter().all(|f| f.path.as_deref() != Some(std::path::Path::new("clean.php"))),
        "{findings:?}"
    );
    // Line anchoring survives the whole pipeline.
    let sys_hit = findings.iter().find(|f| f.rule_id == "opcode-scan/system").expect("system hit");
    assert_eq!(sys_hit.line, Some(2));
}
