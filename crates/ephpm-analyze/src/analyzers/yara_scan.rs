//! `malware-yara` — known-malware scan via YARA rules.
//!
//! Wraps the `yara` CLI as a subprocess (an **optional external
//! dependency**). Two prerequisites, both of whose absence *skips* rather
//! than fails: the `yara` binary on `PATH`, and a ruleset configured at
//! `analyzers.yara_rules` (no ruleset ships with ePHPm). Matches are parsed
//! from the standard `RuleName<space>path` output lines.

use super::tool::{run_tool, stderr_excerpt};
use crate::analyzer::{AnalysisCtx, Analyzer, AnalyzerError};
use crate::finding::{Category, Finding, Severity};

/// See the module docs.
pub struct MalwareYara;

/// The analyzer's stable id.
pub const ID: &str = "malware-yara";

/// Parse `yara -r` output (`RuleName /path/to/file` per line).
fn parse_matches(stdout: &str, root: &std::path::Path) -> Vec<Finding> {
    stdout
        .lines()
        .filter_map(|line| {
            let (rule, path) = line.trim_end().split_once(' ')?;
            if rule.is_empty() || path.is_empty() {
                return None;
            }
            // Report tree-relative paths when the match is inside the root.
            let rel =
                std::path::Path::new(path).strip_prefix(root).unwrap_or(std::path::Path::new(path));
            Some(Finding {
                rule_id: format!("{ID}/{rule}"),
                severity: Severity::High,
                category: Category::Malware,
                path: Some(rel.to_path_buf()),
                line: None,
                message: format!("YARA rule {rule} matched"),
            })
        })
        .collect()
}

impl Analyzer for MalwareYara {
    fn id(&self) -> &str {
        ID
    }

    fn category(&self) -> Category {
        Category::Malware
    }

    fn run(&self, ctx: &AnalysisCtx) -> Result<Vec<Finding>, AnalyzerError> {
        let Some(rules) = ctx.config().analyzers.yara_rules.as_ref() else {
            return Err(AnalyzerError::Skipped(
                "no ruleset configured (set analyzers.yara_rules)".to_owned(),
            ));
        };
        let rules = if rules.is_absolute() { rules.clone() } else { ctx.root().join(rules) };
        if !rules.exists() {
            // A configured-but-missing ruleset is an operator error on a
            // path that should have worked — fail closed, don't skip.
            return Err(AnalyzerError::ToolFailed(format!(
                "configured ruleset {} does not exist",
                rules.display()
            )));
        }
        let timeout = ctx.config().analyzers.tool_timeout_ms;
        let rules_arg = rules.to_string_lossy();
        let root_arg = ctx.root().to_string_lossy();
        let run = run_tool("yara", &["-r", "-w", &rules_arg, &root_arg], ctx.root(), timeout)?;
        match run.exit_code {
            // yara exits 0 whether or not rules matched; matches are on
            // stdout.
            Some(0) => Ok(parse_matches(&run.stdout, ctx.root())),
            code => Err(AnalyzerError::ToolFailed(format!(
                "yara failed (exit {code:?}): {}",
                stderr_excerpt(&run.stderr)
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_match_lines_relative_to_root() {
        let root = std::path::Path::new("/scan/root");
        let findings = parse_matches(
            "Php_Webshell /scan/root/uploads/x.php\nEicar_Test /elsewhere/y.bin\n",
            root,
        );
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].rule_id, "malware-yara/Php_Webshell");
        assert_eq!(findings[0].path.as_deref(), Some(std::path::Path::new("uploads/x.php")));
        assert_eq!(findings[0].severity, Severity::High);
        // A path outside the root stays as-is.
        assert_eq!(findings[1].path.as_deref(), Some(std::path::Path::new("/elsewhere/y.bin")));
    }

    #[test]
    fn empty_output_means_no_matches() {
        assert!(parse_matches("", std::path::Path::new("/r")).is_empty());
    }
}
