//! `clamav` — known-malware scan via ClamAV signatures.
//!
//! Wraps the `clamscan` CLI as a subprocess (an **optional external
//! dependency**, absent → skip). Where `malware-yara` matches an
//! operator-supplied ruleset, ClamAV brings its own signature database — the
//! standard freshclam feed plus any add-on webshell/PHP-malware feeds — so it
//! catches families the hand-rolled YARA rules never enumerate. The two are
//! complementary; run both.
//!
//! `clamscan` reports one `<path>: <Signature> FOUND` line per infected file
//! and distinguishes its outcome by exit code — `0` clean, `1` infected, `2`
//! an internal error (which gates, fail-closed). We invoke the standalone
//! scanner rather than `clamdscan` so the analyzer needs no running `clamd`
//! daemon or socket access; the cost is that `clamscan` reloads the signature
//! database on each run.
//!
//! `analyzers.clamav_db` optionally points at an alternate signature database
//! (file or directory), passed as `clamscan -d <path>` — the mechanism for a
//! curated webshell feed, analogous to `malware-yara`'s `yara_rules`. When
//! unset, ClamAV uses its system database (kept fresh out of band by
//! `freshclam`). A configured-but-missing path is a hard error, not a skip.

use super::tool::{run_tool, stderr_excerpt};
use crate::analyzer::{AnalysisCtx, Analyzer, AnalyzerError};
use crate::finding::{Category, Confidence, Finding, Severity};

/// See the module docs.
pub struct Clamav;

/// The analyzer's stable id.
pub const ID: &str = "clamav";

/// Parse `clamscan --infected` output — `<path>: <Signature> FOUND` per line.
fn parse_matches(stdout: &str, root: &std::path::Path) -> Vec<Finding> {
    stdout
        .lines()
        .filter_map(|line| {
            // Only infected-file lines end in ` FOUND`; everything else
            // (progress, blank lines, stray diagnostics) is ignored.
            let body = line.trim_end().strip_suffix(" FOUND")?;
            // `<path>: <Signature>` — the signature has no spaces, and the
            // last `": "` separates it from the path (Windows `C:\` uses a
            // bare colon, never colon-space, so this stays unambiguous).
            let (path, sig) = body.rsplit_once(": ")?;
            if path.is_empty() || sig.is_empty() {
                return None;
            }
            // Report tree-relative paths when the match is inside the root.
            let rel =
                std::path::Path::new(path).strip_prefix(root).unwrap_or(std::path::Path::new(path));
            Some(Finding {
                rule_id: format!("{ID}/{sig}"),
                severity: Severity::High,
                category: Category::Malware,
                path: Some(rel.to_path_buf()),
                line: None,
                message: format!("ClamAV signature {sig} matched"),
                // A signature match from ClamAV's curated database is treated
                // as confirmed — pair with `deny_hard: [clamav]` for an
                // instant deny.
                confidence: Confidence::Confirmed,
            })
        })
        .collect()
}

impl Analyzer for Clamav {
    fn id(&self) -> &str {
        ID
    }

    fn category(&self) -> Category {
        Category::Malware
    }

    fn run(&self, ctx: &AnalysisCtx) -> Result<Vec<Finding>, AnalyzerError> {
        let timeout = ctx.config().analyzers.tool_timeout_ms;
        let root_arg = ctx.root().to_string_lossy();

        // Base args: recurse, print only infected files, drop the summary
        // block, and force all output to stdout (ClamAV otherwise splits it).
        let mut args: Vec<String> = vec![
            "--recursive".to_owned(),
            "--infected".to_owned(),
            "--no-summary".to_owned(),
            "--stdout".to_owned(),
        ];

        // Optional alternate signature database (a curated webshell feed).
        if let Some(db) = ctx.config().analyzers.clamav_db.as_ref() {
            let db = if db.is_absolute() { db.clone() } else { ctx.root().join(db) };
            if !db.exists() {
                // A configured-but-missing DB is an operator error on a path
                // that should have worked — fail closed, don't skip.
                return Err(AnalyzerError::ToolFailed(format!(
                    "configured clamav_db {} does not exist",
                    db.display()
                )));
            }
            args.push("--database".to_owned());
            args.push(db.to_string_lossy().into_owned());
        }

        args.push(root_arg.into_owned());
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let run = run_tool("clamscan", &arg_refs, ctx.root(), timeout)?;
        match run.exit_code {
            // 0 = no infections; 1 = infections found (on stdout).
            Some(0) => Ok(Vec::new()),
            Some(1) => Ok(parse_matches(&run.stdout, ctx.root())),
            // 2 (or anything else) = a scan error: fail closed.
            code => Err(AnalyzerError::ToolFailed(format!(
                "clamscan failed (exit {code:?}): {}",
                stderr_excerpt(&run.stderr)
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_found_lines_relative_to_root() {
        let root = std::path::Path::new("/scan/root");
        let findings = parse_matches(
            "/scan/root/uploads/x.php: Php.Webshell.Agent-1 FOUND\n\
             /elsewhere/y.bin: Win.Trojan.Generic-2 FOUND\n",
            root,
        );
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].rule_id, "clamav/Php.Webshell.Agent-1");
        assert_eq!(findings[0].path.as_deref(), Some(std::path::Path::new("uploads/x.php")));
        assert_eq!(findings[0].severity, Severity::High);
        assert_eq!(findings[0].confidence, Confidence::Confirmed);
        assert_eq!(findings[0].category, Category::Malware);
        // A path outside the root stays as-is.
        assert_eq!(findings[1].path.as_deref(), Some(std::path::Path::new("/elsewhere/y.bin")));
    }

    #[test]
    fn ignores_non_found_lines() {
        // Progress/OK lines (only present without `--infected`) and blanks are
        // not infections and must not become findings.
        let findings = parse_matches(
            "/scan/root/clean.php: OK\n\n/scan/root/bad.php: Php.Malware.X FOUND\n",
            std::path::Path::new("/scan/root"),
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule_id, "clamav/Php.Malware.X");
    }

    #[test]
    fn empty_output_means_no_matches() {
        assert!(parse_matches("", std::path::Path::new("/r")).is_empty());
    }
}
