//! Diff-aware scanning: restrict an analysis run to files changed versus a
//! git ref (`since:` in the config, `--since` on the CLI) — a fast per-push
//! gate.
//!
//! The changed set is the union of
//!
//! - `git diff --name-only --relative <ref>` — tracked files that differ
//!   between the ref and the working tree, and
//! - `git ls-files --others --exclude-standard` — untracked files, so a
//!   brand-new (not yet committed) file cannot dodge the gate by being
//!   absent from the diff.
//!
//! Both run with the analyzed root as working directory; `--relative` keeps
//! diff paths relative to that root even when it is a subdirectory of the
//! repository. Paths are compared with `/` separators on every platform.
//!
//! **Fail-closed:** any git failure — binary missing, not a repository,
//! unknown ref — is a hard error that aborts the run. A diff-aware gate
//! that silently fell back to "scan nothing" would pass everything.
//!
//! Enforcement is two-fold: the aggregator drops path-anchored findings
//! outside the scope after all analyzers ran (uniform across analyzer
//! kinds), and the native per-file analyzers additionally skip out-of-scope
//! files up front via [`AnalysisCtx::is_in_scope`](crate::AnalysisCtx) for
//! speed. Findings with **no** path anchor are always kept — an unattributable
//! finding must not be droppable by scoping (fail-safe).

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use anyhow::Context as _;

/// The set of tree-relative changed paths, `/`-normalized.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    paths: BTreeSet<String>,
}

/// Normalize a tree-relative path to the `/`-separated form used for scope
/// membership tests.
#[must_use]
pub fn normalize(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

impl Scope {
    /// Build a scope from an iterator of `/`-relative path strings (used by
    /// tests and by [`changed_files`]).
    #[must_use]
    pub fn from_paths<I: IntoIterator<Item = String>>(paths: I) -> Self {
        Self { paths: paths.into_iter().filter(|p| !p.is_empty()).collect() }
    }

    /// Whether the tree-relative `path` is inside the changed set.
    #[must_use]
    pub fn contains(&self, path: &Path) -> bool {
        self.paths.contains(&normalize(path))
    }

    /// Number of files in scope.
    #[must_use]
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    /// Whether the changed set is empty (nothing changed since the ref).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

fn git_lines(root: &Path, args: &[&str]) -> anyhow::Result<Vec<String>> {
    let output =
        Command::new("git").args(args).current_dir(root).output().with_context(|| {
            format!("failed to run `git {}` (is git installed?)", args.join(" "))
        })?;
    anyhow::ensure!(
        output.status.success(),
        "`git {}` failed (exit {:?}): {}",
        args.join(" "),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim_end().to_owned())
        .filter(|l| !l.is_empty())
        .collect())
}

/// Compute the changed-file scope for `root` versus `since_ref`.
///
/// # Errors
///
/// On any git failure (missing binary, not a repository, unknown ref) —
/// fail-closed, see the module docs.
pub fn changed_files(root: &Path, since_ref: &str) -> anyhow::Result<Scope> {
    anyhow::ensure!(
        !since_ref.starts_with('-'),
        "invalid git ref {since_ref:?} (must not start with '-')"
    );
    let mut paths = git_lines(root, &["diff", "--name-only", "--relative", since_ref])
        .context("diff-aware scan (`since`) could not compute changed files")?;
    paths.extend(
        git_lines(root, &["ls-files", "--others", "--exclude-standard"])
            .context("diff-aware scan (`since`) could not list untracked files")?,
    );
    Ok(Scope::from_paths(paths))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_membership_is_separator_agnostic() {
        let scope = Scope::from_paths(vec!["src/a.php".to_owned(), "b.php".to_owned()]);
        assert!(scope.contains(Path::new("src/a.php")));
        assert!(scope.contains(Path::new("src\\a.php")));
        assert!(scope.contains(Path::new("b.php")));
        assert!(!scope.contains(Path::new("src/other.php")));
        assert_eq!(scope.len(), 2);
        assert!(!scope.is_empty());
    }

    #[test]
    fn refs_shaped_like_flags_are_rejected() {
        // A ref beginning with '-' could smuggle git options.
        let err = changed_files(Path::new("."), "--output=/tmp/x").unwrap_err();
        assert!(format!("{err:#}").contains("invalid git ref"));
    }

    #[test]
    fn git_failure_is_an_error_not_an_empty_scope() {
        // Fail-closed: an unknown ref must error, never silently produce an
        // empty scope. Run inside a real repo checkout (this workspace) so
        // git itself is exercised; skip when git is unavailable.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let err = changed_files(root, "definitely-not-a-ref-9f2c").unwrap_err();
        assert!(format!("{err:#}").contains("failed"));
    }
}
