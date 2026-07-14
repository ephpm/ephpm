//! `cargo xtask bump-php-pin` — update or verify every PHP SDK pin site.
//!
//! The pinned full PHP version for a given minor lives in several places
//! that must stay in lockstep:
//!
//! 1. `xtask/src/main.rs` — the `PHP_SDK_VERSIONS` table (source of truth)
//! 2. `.github/workflows/release.yml` — 4 matrix sites (linux / macos /
//!    windows / docker), one entry per minor in each
//! 3. `docker/Dockerfile` — `ARG PHP_SDK_VERSION` default (+ its inline
//!    example), only for the default minor
//! 4. `site/content/developer/architecture-overview.md` — the support table
//!    listing the CI-pinned full versions
//!
//! A missed pin site has broken a release before, so every substitution
//! asserts its exact expected match count and the command refuses to write
//! anything (all-or-nothing) if any count mismatches. `--check` runs the
//! same counting against the current pins as a drift detector for CI.

use std::fs;
use std::process::ExitCode;

use crate::{DEFAULT_PHP_MINOR, PHP_SDK_VERSIONS, workspace_root};

/// One exact-substring substitution in one file, with a required match count.
struct Substitution {
    /// Path relative to the workspace root.
    path: &'static str,
    /// Exact substring that must occur exactly `expected` times in the file.
    needle: String,
    /// Text written in place of every `needle` occurrence.
    replacement: String,
    /// Required occurrence count — any other count aborts the whole run.
    expected: usize,
}

/// Build the full substitution set for moving `minor` from `old_full` to
/// `new_full`. With `old_full == new_full` this doubles as the site list for
/// `--check` (counting only, replacement is a no-op).
fn substitutions(minor: &str, old_full: &str, new_full: &str) -> Vec<Substitution> {
    let mut subs = vec![
        // The PHP_SDK_VERSIONS table entry itself.
        Substitution {
            path: "xtask/src/main.rs",
            needle: format!("(\"{minor}\", \"{old_full}\")"),
            replacement: format!("(\"{minor}\", \"{new_full}\")"),
            expected: 1,
        },
        // The 4 release matrix sites (build-linux inline map, build-macos,
        // build-windows, docker-image). Matching the bare quoted version is
        // unambiguous — a full version string can only belong to one minor —
        // and the count assert catches any structural change to the matrix.
        Substitution {
            path: ".github/workflows/release.yml",
            needle: format!("\"{old_full}\""),
            replacement: format!("\"{new_full}\""),
            expected: 4,
        },
        // The PHP support table ("Supported — in CI (pinned X.Y.Z)").
        Substitution {
            path: "site/content/developer/architecture-overview.md",
            needle: format!("(pinned {old_full})"),
            replacement: format!("(pinned {new_full})"),
            expected: 1,
        },
    ];
    // The Dockerfile only pins the default minor: once as the ARG default
    // and once in the comment example right above it ("e.g., v8.5.7").
    if minor == DEFAULT_PHP_MINOR {
        subs.push(Substitution {
            path: "docker/Dockerfile",
            needle: old_full.to_string(),
            replacement: new_full.to_string(),
            expected: 2,
        });
    }
    subs
}

/// Apply one substitution to file contents (pure — no I/O, unit-tested).
///
/// Errors (instead of writing a partial result) when the occurrence count
/// differs from `sub.expected` — the count assert is the whole point: a
/// silently missed pin site broke a release before.
fn apply(contents: &str, sub: &Substitution) -> Result<String, String> {
    let found = contents.matches(&sub.needle).count();
    if found == sub.expected {
        Ok(contents.replace(&sub.needle, &sub.replacement))
    } else {
        Err(format!(
            "{}: expected exactly {} occurrence(s) of `{}`, found {} — \
             pin sites changed shape; update xtask/src/bump.rs to match",
            sub.path, sub.expected, sub.needle, found
        ))
    }
}

/// Validate that `full` is a plausible patch release of `minor`
/// (e.g. minor "8.4" accepts "8.4.23" but not "8.5.1" or "8.4.x").
fn is_patch_of(minor: &str, full: &str) -> bool {
    full.strip_prefix(minor)
        .and_then(|rest| rest.strip_prefix('.'))
        .is_some_and(|patch| !patch.is_empty() && patch.chars().all(|c| c.is_ascii_digit()))
}

/// Entry point for `cargo xtask bump-php-pin [<minor> <full>] [--check]`.
pub fn bump_php_pin(args: &[String]) -> ExitCode {
    let check = args.iter().any(|a| a == "--check");
    let positional: Vec<&str> =
        args.iter().filter(|a| !a.starts_with('-')).map(String::as_str).collect();

    if check {
        // Optional <minor> <full>: assert the table itself maps minor → full
        // before verifying that all sites agree with the table.
        if let [minor, full, ..] = positional[..] {
            match PHP_SDK_VERSIONS.iter().find(|(m, _)| *m == minor) {
                Some((_, pinned)) if *pinned == full => {}
                Some((_, pinned)) => {
                    eprintln!(
                        "error: PHP_SDK_VERSIONS pins {minor} at {pinned}, not {full} — \
                         run `cargo xtask bump-php-pin {minor} {full}` to move it"
                    );
                    return ExitCode::FAILURE;
                }
                None => {
                    eprintln!("error: unknown PHP minor '{minor}' (not in PHP_SDK_VERSIONS)");
                    return ExitCode::FAILURE;
                }
            }
        }
        return check_all();
    }

    let [minor, new_full] = positional[..] else {
        eprintln!("usage: cargo xtask bump-php-pin <minor> <full>   (e.g. 8.4 8.4.24)");
        eprintln!("       cargo xtask bump-php-pin [<minor> <full>] --check");
        return ExitCode::FAILURE;
    };
    let Some((_, old_full)) = PHP_SDK_VERSIONS.iter().find(|(m, _)| *m == minor) else {
        eprintln!("error: unknown PHP minor '{minor}'");
        eprintln!(
            "       supported minors: {:?}",
            PHP_SDK_VERSIONS.iter().map(|(m, _)| *m).collect::<Vec<_>>()
        );
        return ExitCode::FAILURE;
    };
    if !is_patch_of(minor, new_full) {
        eprintln!("error: '{new_full}' is not a patch release of minor {minor}");
        return ExitCode::FAILURE;
    }
    if *old_full == new_full {
        // Idempotent success so automation can re-run harmlessly.
        eprintln!("==> {minor} is already pinned at {new_full} — nothing to do");
        return ExitCode::SUCCESS;
    }

    // Two-phase: apply everything in memory first, write only if ALL sites
    // matched. A partial bump (some files moved, some not) is exactly the
    // broken state this command exists to prevent.
    let root = workspace_root();
    let mut staged: Vec<(std::path::PathBuf, &'static str, String)> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for sub in substitutions(minor, old_full, new_full) {
        let path = root.join(sub.path);
        match fs::read_to_string(&path) {
            Ok(contents) => match apply(&contents, &sub) {
                Ok(updated) => staged.push((path, sub.path, updated)),
                Err(e) => errors.push(e),
            },
            Err(e) => errors.push(format!("{}: cannot read: {e}", sub.path)),
        }
    }
    if !errors.is_empty() {
        eprintln!("error: refusing to bump — no files were modified:");
        for e in &errors {
            eprintln!("  - {e}");
        }
        return ExitCode::FAILURE;
    }
    for (path, rel, contents) in staged {
        if let Err(e) = fs::write(&path, contents) {
            eprintln!("error: failed to write {rel}: {e}");
            return ExitCode::FAILURE;
        }
        eprintln!("==> updated {rel}");
    }
    eprintln!("==> PHP {minor} pin bumped: {old_full} → {new_full}");
    eprintln!("    (PHP_SDK_VERSIONS in xtask/src/main.rs was rewritten — recompile picks it up)");
    ExitCode::SUCCESS
}

/// `--check`: verify every pin site agrees with `PHP_SDK_VERSIONS` for every
/// minor. Pure drift detector — reads files, writes nothing.
fn check_all() -> ExitCode {
    let root = workspace_root();
    let mut errors: Vec<String> = Vec::new();
    for (minor, full) in PHP_SDK_VERSIONS {
        for sub in substitutions(minor, full, full) {
            let path = root.join(sub.path);
            match fs::read_to_string(&path) {
                Ok(contents) => {
                    if let Err(e) = apply(&contents, &sub) {
                        errors.push(e);
                    }
                }
                Err(e) => errors.push(format!("{}: cannot read: {e}", sub.path)),
            }
        }
    }
    if errors.is_empty() {
        eprintln!("==> all PHP SDK pin sites agree with PHP_SDK_VERSIONS:");
        for (minor, full) in PHP_SDK_VERSIONS {
            eprintln!("    {minor} → {full}");
        }
        ExitCode::SUCCESS
    } else {
        eprintln!("error: PHP SDK pin drift detected:");
        for e in &errors {
            eprintln!("  - {e}");
        }
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_replaces_exact_count() {
        let sub = Substitution {
            path: "test.yml",
            needle: "\"1.2.3\"".into(),
            replacement: "\"1.2.4\"".into(),
            expected: 2,
        };
        let input = "a: \"1.2.3\"\nb: \"1.2.3\"\n";
        assert_eq!(apply(input, &sub).unwrap(), "a: \"1.2.4\"\nb: \"1.2.4\"\n");
    }

    #[test]
    fn apply_rejects_too_few_matches() {
        let sub = Substitution {
            path: "test.yml",
            needle: "\"1.2.3\"".into(),
            replacement: "\"1.2.4\"".into(),
            expected: 4,
        };
        let err = apply("only one \"1.2.3\" here", &sub).unwrap_err();
        assert!(err.contains("expected exactly 4"), "unexpected message: {err}");
        assert!(err.contains("found 1"), "unexpected message: {err}");
    }

    #[test]
    fn apply_rejects_too_many_matches() {
        let sub = Substitution {
            path: "Dockerfile",
            needle: "1.2.3".into(),
            replacement: "1.2.4".into(),
            expected: 2,
        };
        let err = apply("1.2.3 1.2.3 1.2.3", &sub).unwrap_err();
        assert!(err.contains("found 3"), "unexpected message: {err}");
    }

    #[test]
    fn apply_check_mode_is_noop_on_match() {
        // --check builds substitutions with old == new; contents must be
        // returned unchanged when the count matches.
        let sub = Substitution {
            path: "test.md",
            needle: "(pinned 8.4.23)".into(),
            replacement: "(pinned 8.4.23)".into(),
            expected: 1,
        };
        let input = "| PHP 8.4 | Active | **Supported — in CI (pinned 8.4.23)** |";
        assert_eq!(apply(input, &sub).unwrap(), input);
    }

    #[test]
    fn release_yml_fixture_hits_all_four_matrix_sites() {
        let fixture = r#"
          - { minor: "8.4", full: "8.4.23" }
          - php_minor: "8.4"
            php_full: "8.4.23"
          - php_minor: "8.4"
            php_full: "8.4.23"
          - php_minor: "8.4"
            php_full: "8.4.23"
"#;
        let subs = substitutions("8.4", "8.4.23", "8.4.30");
        let yml = subs.iter().find(|s| s.path.ends_with("release.yml")).unwrap();
        let out = apply(fixture, yml).unwrap();
        assert_eq!(out.matches("\"8.4.30\"").count(), 4);
        assert!(!out.contains("8.4.23"));
    }

    #[test]
    fn main_rs_needle_is_the_table_pair() {
        let subs = substitutions("8.3", "8.3.31", "8.3.32");
        let rs = subs.iter().find(|s| s.path.ends_with("main.rs")).unwrap();
        let fixture =
            r#"const PHP_SDK_VERSIONS: &[(&str, &str)] = &[("8.3", "8.3.31"), ("8.4", "8.4.23")];"#;
        let out = apply(fixture, rs).unwrap();
        assert!(out.contains(r#"("8.3", "8.3.32")"#));
        assert!(out.contains(r#"("8.4", "8.4.23")"#), "other minors must be untouched");
    }

    #[test]
    fn dockerfile_site_only_for_default_minor() {
        assert!(
            substitutions("8.3", "8.3.31", "8.3.32").iter().all(|s| s.path != "docker/Dockerfile"),
            "non-default minor must not touch the Dockerfile"
        );
        let default_subs = substitutions(DEFAULT_PHP_MINOR, "8.5.7", "8.5.8");
        let docker = default_subs.iter().find(|s| s.path == "docker/Dockerfile").unwrap();
        let fixture = "# releases (e.g., v8.5.7). The default mirrors\nARG PHP_SDK_VERSION=8.5.7\n";
        let out = apply(fixture, docker).unwrap();
        assert!(out.contains("ARG PHP_SDK_VERSION=8.5.8"));
        assert!(out.contains("v8.5.8"));
    }

    #[test]
    fn is_patch_of_validates_shape() {
        assert!(is_patch_of("8.4", "8.4.23"));
        assert!(is_patch_of("8.4", "8.4.0"));
        assert!(!is_patch_of("8.4", "8.5.1"));
        assert!(!is_patch_of("8.4", "8.4"));
        assert!(!is_patch_of("8.4", "8.4."));
        assert!(!is_patch_of("8.4", "8.4.x"));
        assert!(!is_patch_of("8.4", "8.40.1"));
    }
}
