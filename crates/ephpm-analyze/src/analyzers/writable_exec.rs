//! `writable-exec` — executable PHP living in a data-only directory.
//!
//! Every other analyzer inspects source for *vulnerabilities*; this one
//! detects the *result* of a compromise. A `.php` (or `.phtml` / `.php5` /
//! `.phar`) file under an upload directory is almost always a dropped
//! webshell — nothing legitimate writes executable code into a path that
//! should only ever hold uploaded data. It complements the malware scanners
//! (`clamav`, `malware-yara`, `obfuscation-scan`), which reason about a file's
//! *contents*, by flagging a file purely by its *location* — so a novel
//! webshell whose bytes no signature has ever seen is still caught the moment
//! it lands somewhere it has no business being.
//!
//! **Native, offline, and path-driven — not a per-file content pass.** Like
//! `wp-vuln`, it walks the tree and reasons about paths rather than scanning
//! bytes: the file is never read, only its location is judged. There is no
//! external tool, no new dependency, and a tree with nothing suspicious yields
//! no findings and no error. **Opt-in** — registered but not in the default
//! `security` profile.
//!
//! Two tiers:
//!
//! - **Confirmed / High** — `writable-exec/uploads-php`: executable PHP
//!   anywhere under `wp-content/uploads/`, the canonical webshell-drop
//!   location. That a PHP file exists there is a *fact* about the tree, not a
//!   heuristic, so it is `Confirmed`. This tier runs by default.
//! - **Suspected / Medium** — `writable-exec/writable-dir-php`: executable PHP
//!   under an operator-listed writable directory
//!   ([`crate::config::AnalyzersConfig::writable_exec_dirs`]). When that knob
//!   is unset, only the `wp-content/uploads/` confirmed rule runs — keeping
//!   the defaults high-signal.
//!
//! ## Known false positives (why the defaults are narrow)
//!
//! Several frameworks legitimately write `.php` into data directories, so
//! flagging every writable dir by default would drown real hits in noise:
//!
//! - **Laravel** compiles Blade templates to `.php` under
//!   `storage/framework/views/`.
//! - **WordPress page-cache plugins** write `.php` cache files under
//!   `wp-content/cache/`.
//!
//! That is exactly why only `wp-content/uploads/` is confirmed-by-default and
//! everything else is opt-in via `writable_exec_dirs`: an operator who *knows*
//! their app never writes code into `storage/` or `wp-content/cache/` can add
//! those paths and accept the `Suspected` false-positive trade — the same
//! trade `dangerous-sinks` and `obfuscation-scan` make for their heuristic
//! tiers.

use std::path::{Component, Path, PathBuf};

use crate::analyzer::{AnalysisCtx, Analyzer, AnalyzerError};
use crate::finding::{Category, Confidence, Finding, Severity};

/// See the module docs.
pub struct WritableExec;

/// The analyzer's stable id (and rule-id namespace prefix).
pub const ID: &str = "writable-exec";

/// Extensions PHP will execute. Broader than the per-file walker's
/// `*.php`/`*.phtml` set (`php_files::is_php_file`) on purpose: a webshell is
/// just as dangerous dropped as `shell.php5` or `shell.phar`, and this pass
/// judges by location, not content, so it enumerates every executable form.
const EXEC_PHP_EXTENSIONS: &[&str] = &["php", "phtml", "php5", "phar"];

/// `true` when `path` names a file PHP would execute.
fn is_executable_php(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| EXEC_PHP_EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

/// The tree-relative path's `Normal` components as strings (dropping `.`, `..`,
/// and any prefix/root components, which never appear in a tree-relative path).
fn normal_components(rel: &Path) -> Vec<&str> {
    rel.components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect()
}

/// `true` when `rel` (tree-relative) is a file below a `wp-content/uploads/`
/// directory. The `wp-content` → `uploads` pair is matched *anywhere* in the
/// path so a WordPress install nested in a subdirectory still counts, and at
/// least one component must follow the pair (the file itself).
fn under_wp_uploads(rel: &Path) -> bool {
    let comps = normal_components(rel);
    comps.windows(2).enumerate().any(|(i, pair)| {
        pair[0].eq_ignore_ascii_case("wp-content")
            && pair[1].eq_ignore_ascii_case("uploads")
            // A component after `uploads` (the file) must exist.
            && i + 2 < comps.len()
    })
}

/// Build the confirmed finding for an executable PHP file under uploads.
fn uploads_finding(rel: PathBuf) -> Finding {
    let message = format!(
        "executable PHP file {} under wp-content/uploads/ — an upload directory should hold \
         uploaded data only; a PHP file here is almost always a dropped webshell",
        rel.display()
    );
    Finding {
        rule_id: format!("{ID}/uploads-php"),
        severity: Severity::High,
        category: Category::Malware,
        path: Some(rel),
        // The finding is about the file's existence/location, not a line.
        line: None,
        message,
        // The file being here is a fact, not a heuristic — like a ClamAV hit.
        confidence: Confidence::Confirmed,
    }
}

/// Build the suspected finding for an executable PHP file under an
/// operator-listed writable directory.
fn writable_dir_finding(rel: PathBuf) -> Finding {
    let message = format!(
        "executable PHP file {} under an operator-listed writable directory \
         (analyzers.writable_exec_dirs) — verify this is not a dropped webshell",
        rel.display()
    );
    Finding {
        rule_id: format!("{ID}/writable-dir-php"),
        severity: Severity::Medium,
        category: Category::Malware,
        path: Some(rel),
        line: None,
        message,
        // A framework may legitimately write .php into some data dirs, so this
        // operator-opted tier is a review hotspot, never a hard deny.
        confidence: Confidence::Suspected,
    }
}

/// Walk the tree under `dir`, appending a finding for every executable PHP file
/// found under `wp-content/uploads/` (confirmed) or under one of
/// `writable_dirs` (suspected). Does not follow directory symlinks; skips
/// `.git`. Paths are reported relative to `root`.
fn walk(
    root: &Path,
    dir: &Path,
    writable_dirs: &[PathBuf],
    out: &mut Vec<Finding>,
) -> Result<(), AnalyzerError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            if entry.file_name() == ".git" {
                continue;
            }
            walk(root, &path, writable_dirs, out)?;
        } else if file_type.is_file() && is_executable_php(&path) {
            let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            // Confirmed wins: a file under uploads that an operator also listed
            // in writable_exec_dirs is reported once, as the stronger tier.
            if under_wp_uploads(&rel) {
                out.push(uploads_finding(rel));
            } else if writable_dirs.iter().any(|d| path.starts_with(d)) {
                // `Path::starts_with` is component-wise, so `storage2` does not
                // match a listed `storage`.
                out.push(writable_dir_finding(rel));
            }
        }
    }
    Ok(())
}

impl Analyzer for WritableExec {
    fn id(&self) -> &str {
        ID
    }

    fn category(&self) -> Category {
        Category::Malware
    }

    fn run(&self, ctx: &AnalysisCtx) -> Result<Vec<Finding>, AnalyzerError> {
        let root = ctx.root();
        // Operator-listed writable dirs resolve against the analyzed root when
        // relative, like `yara_rules` / `clamav_db`.
        let writable_dirs: Vec<PathBuf> = ctx
            .config()
            .analyzers
            .writable_exec_dirs
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|d| if d.is_absolute() { d.clone() } else { root.join(d) })
            .collect();

        let mut findings = Vec::new();
        walk(root, root, &writable_dirs, &mut findings)?;
        // read_dir order is OS-dependent; sort for a deterministic report.
        findings.sort_by(|a, b| (&a.path, &a.rule_id).cmp(&(&b.path, &b.rule_id)));
        Ok(findings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AnalyzeConfig;

    /// A config with `writable_exec_dirs` set to `dirs` (relative to root).
    fn config_with_dirs(dirs: Option<&[&str]>) -> AnalyzeConfig {
        let mut cfg = AnalyzeConfig::default();
        cfg.analyzers.writable_exec_dirs = dirs.map(|v| v.iter().map(PathBuf::from).collect());
        cfg
    }

    /// Write a benign PHP file at `root/<rel>` (creating parent dirs). The
    /// content is never read by the analyzer — location is all that matters —
    /// so a harmless body is fine.
    fn write_php(root: &Path, rel: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "<?php return 1;\n").unwrap();
    }

    fn run(root: &Path, cfg: AnalyzeConfig) -> Vec<Finding> {
        let ctx = AnalysisCtx::new(root.to_path_buf(), cfg);
        WritableExec.run(&ctx).expect("run succeeds")
    }

    #[test]
    fn uploads_php_is_confirmed_high() {
        let dir = tempfile::tempdir().unwrap();
        write_php(dir.path(), "wp-content/uploads/2024/01/dropped.php");
        let findings = run(dir.path(), AnalyzeConfig::default());
        assert_eq!(findings.len(), 1, "{findings:?}");
        let f = &findings[0];
        assert_eq!(f.rule_id, "writable-exec/uploads-php");
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.confidence, Confidence::Confirmed);
        assert_eq!(f.category, Category::Malware);
        assert_eq!(f.line, None);
        assert_eq!(f.path.as_deref(), Some(Path::new("wp-content/uploads/2024/01/dropped.php")));
    }

    #[test]
    fn uploads_catches_every_executable_extension() {
        let dir = tempfile::tempdir().unwrap();
        write_php(dir.path(), "wp-content/uploads/a.php");
        write_php(dir.path(), "wp-content/uploads/b.phtml");
        write_php(dir.path(), "wp-content/uploads/c.php5");
        write_php(dir.path(), "wp-content/uploads/d.phar");
        // Non-executable upload (a real upload) must not flag.
        write_php(dir.path(), "wp-content/uploads/photo.jpg");
        let findings = run(dir.path(), AnalyzeConfig::default());
        assert_eq!(findings.len(), 4, "{findings:?}");
        assert!(findings.iter().all(|f| f.rule_id == "writable-exec/uploads-php"));
    }

    #[test]
    fn uploads_match_is_case_insensitive_on_dir_names() {
        let dir = tempfile::tempdir().unwrap();
        write_php(dir.path(), "WP-Content/Uploads/evil.php");
        let findings = run(dir.path(), AnalyzeConfig::default());
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].rule_id, "writable-exec/uploads-php");
    }

    #[test]
    fn ordinary_plugin_php_is_not_flagged() {
        let dir = tempfile::tempdir().unwrap();
        write_php(dir.path(), "wp-content/plugins/foo/bar.php");
        write_php(dir.path(), "index.php");
        let findings = run(dir.path(), AnalyzeConfig::default());
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn operator_listed_writable_dir_is_suspected_medium() {
        let dir = tempfile::tempdir().unwrap();
        write_php(dir.path(), "wp-content/cache/page.php");
        let cfg = config_with_dirs(Some(&["wp-content/cache"]));
        let findings = run(dir.path(), cfg);
        assert_eq!(findings.len(), 1, "{findings:?}");
        let f = &findings[0];
        assert_eq!(f.rule_id, "writable-exec/writable-dir-php");
        assert_eq!(f.severity, Severity::Medium);
        assert_eq!(f.confidence, Confidence::Suspected);
        assert_eq!(f.category, Category::Malware);
    }

    #[test]
    fn laravel_compiled_views_are_not_flagged_by_default() {
        let dir = tempfile::tempdir().unwrap();
        // Laravel compiles Blade to .php here — a legitimate data-dir write.
        write_php(dir.path(), "storage/framework/views/abc123.php");
        // WordPress page-cache plugins write .php here too.
        write_php(dir.path(), "wp-content/cache/index.php");
        let findings = run(dir.path(), AnalyzeConfig::default());
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn listed_dir_prefix_is_component_wise() {
        // A listed `storage` must not match a sibling `storage2`.
        let dir = tempfile::tempdir().unwrap();
        write_php(dir.path(), "storage2/x.php");
        let cfg = config_with_dirs(Some(&["storage"]));
        let findings = run(dir.path(), cfg);
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn uploads_wins_when_also_listed() {
        // A file both under uploads and in a listed dir is reported once, as
        // the stronger confirmed tier.
        let dir = tempfile::tempdir().unwrap();
        write_php(dir.path(), "wp-content/uploads/evil.php");
        let cfg = config_with_dirs(Some(&["wp-content/uploads"]));
        let findings = run(dir.path(), cfg);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].rule_id, "writable-exec/uploads-php");
    }

    #[test]
    fn empty_tree_yields_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(run(dir.path(), AnalyzeConfig::default()).is_empty());
    }

    #[test]
    fn under_wp_uploads_requires_a_file_after_the_pair() {
        // The directory itself is not a file below the pair.
        assert!(!under_wp_uploads(Path::new("wp-content/uploads")));
        assert!(under_wp_uploads(Path::new("wp-content/uploads/x.php")));
        // Nested install still counts.
        assert!(under_wp_uploads(Path::new("sites/blog/wp-content/uploads/x.php")));
        // uploads not under wp-content is not the WordPress upload dir.
        assert!(!under_wp_uploads(Path::new("uploads/x.php")));
    }
}
