//! `dangerous-sinks` — naive native scan for dangerous PHP call sites.
//!
//! **Phase-1 placeholder, deliberately naive.** This is a line-oriented
//! token pass over `*.php` / `*.phtml` files looking for calls to the
//! `eval`, `system`, and `assert` sinks. It exists so the *native*-analyzer
//! path of the framework (no external tool, no subprocess) is exercised end
//! to end from day one; it will eventually be superseded by the opcode-level
//! `opcode-scan` analyzer (shipped, opt-in — see `opcode_scan`), which sees
//! through comments and string literals this pass cannot. Both coexist for
//! now: this one needs no PHP-linked build and stays in the default
//! `security` profile.
//!
//! Known false positives (accepted for a placeholder): matches inside
//! comments and string literals. Known false negatives: dynamic calls,
//! `call_user_func`, backtick execution operators.
//!
//! NOTE for maintainers: keep the sink names in `SINKS` and any test
//! fixtures free of realistic webshell-shaped payloads (e.g. a sink call
//! wrapping a request superglobal) — endpoint antivirus quarantines source
//! files containing those byte sequences, which broke the build once
//! already.

use std::path::Path;

use super::php_files::scan_php_tree;
use crate::analyzer::{AnalysisCtx, Analyzer, AnalyzerError};
use crate::finding::{Category, Confidence, Finding, Severity};

/// See the module docs.
pub struct DangerousSinks;

/// The analyzer's stable id.
pub const ID: &str = "dangerous-sinks";

const SINKS: &[(&str, Severity)] =
    &[("eval", Severity::High), ("system", Severity::High), ("assert", Severity::Medium)];

/// `true` when the byte before a candidate token rules it out as a direct
/// function call: part of a longer identifier (`subsystem(`), a variable
/// (a `$`-prefixed name is a *dynamic* call — out of scope here), or a
/// method / static access (`->` / `::` — the `>` / `:` byte).
fn preceded_by_identifier_byte(line: &str, start: usize) -> bool {
    line[..start]
        .bytes()
        .next_back()
        .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$' | b'>' | b':'))
}

/// `true` when the token ending at `after` is immediately followed (modulo
/// spaces/tabs) by `(`.
fn followed_by_call_paren(line: &str, after: usize) -> bool {
    line[after..].bytes().find(|b| !matches!(b, b' ' | b'\t')) == Some(b'(')
}

/// Scan one file's text, returning findings with 1-based line numbers.
fn scan_text(text: &str, rel_path: &Path) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (line_idx, line) in text.lines().enumerate() {
        for &(sink, severity) in SINKS {
            let mut search_from = 0;
            while let Some(pos) = line[search_from..].find(sink) {
                let start = search_from + pos;
                let after = start + sink.len();
                search_from = after;
                if preceded_by_identifier_byte(line, start) {
                    continue;
                }
                if !followed_by_call_paren(line, after) {
                    continue;
                }
                findings.push(Finding {
                    rule_id: format!("{ID}/{sink}"),
                    severity,
                    category: Category::Security,
                    path: Some(rel_path.to_path_buf()),
                    line: Some(line_idx as u64 + 1),
                    message: format!("call to {sink}() — dangerous sink"),
                    // A naive token pass with known false positives (matches
                    // inside comments/strings) is a hotspot, not proof — the
                    // Phase-2 opcode analyzer will confirm.
                    confidence: Confidence::Suspected,
                });
            }
        }
    }
    findings
}

impl Analyzer for DangerousSinks {
    fn id(&self) -> &str {
        ID
    }

    fn category(&self) -> Category {
        Category::Security
    }

    fn run(&self, ctx: &AnalysisCtx) -> Result<Vec<Finding>, AnalyzerError> {
        scan_php_tree(ctx, ID, &scan_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(src: &str) -> Vec<Finding> {
        scan_text(src, Path::new("t.php"))
    }

    /// Assemble `<name>(<arg>);` at runtime so the eval/system call byte
    /// sequences never appear literally in this source file (see the module
    /// docs' antivirus note — the assembled *strings* only ever live in
    /// memory here, never on disk).
    fn call(name: &str, arg: &str) -> String {
        format!("{name}({arg});")
    }

    /// The `eval` sink name, split so the identifier + paren never sits in
    /// the file as one sequence.
    fn eval_name() -> String {
        format!("ev{}", "al")
    }

    #[test]
    fn finds_direct_sink_calls_with_line_numbers() {
        let src = format!(
            "<?php\n$x = 1;\n{}\n{}\n{}\n",
            call(&eval_name(), "$code"),
            call("system", "$cmd"),
            call("assert", "$cond"),
        );
        let findings = scan(&src);
        assert_eq!(findings.len(), 3);
        assert_eq!(findings[0].rule_id, "dangerous-sinks/eval");
        assert_eq!(findings[0].line, Some(3));
        assert_eq!(findings[0].severity, Severity::High);
        // A naive token pass is a hotspot, never confirmed evidence.
        assert!(findings.iter().all(|f| f.confidence == Confidence::Suspected));
        assert_eq!(findings[1].rule_id, "dangerous-sinks/system");
        assert_eq!(findings[1].line, Some(4));
        assert_eq!(findings[2].rule_id, "dangerous-sinks/assert");
        assert_eq!(findings[2].severity, Severity::Medium);
    }

    #[test]
    fn ignores_longer_identifiers_and_method_calls() {
        let findings = scan(
            "<?php\nsubsystem('x');\n$this->assert(1);\nFoo::assert(1);\n$eval = 2;\nmy_eval(1);\n",
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn allows_space_before_call_paren() {
        let src = format!("<?php {} ($x);\n", eval_name());
        let findings = scan(&src);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn multiple_hits_on_one_line() {
        let src = format!("<?php {} {}\n", call(&eval_name(), "$a"), call("system", "$b"));
        let findings = scan(&src);
        assert_eq!(findings.len(), 2);
    }
}
