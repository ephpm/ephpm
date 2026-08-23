//! `cargo xtask php-versions` — emit the pinned PHP SDK build matrix as JSON.
//!
//! This is the single-source-of-truth bridge for `.github/workflows/release.yml`.
//! The release build matrices are *derived at runtime* from the
//! `PHP_SDK_VERSIONS` table in `xtask/src/main.rs` rather than being hardcoded
//! as `php_minor`/`php_full` pairs inside the workflow file.
//!
//! Why that matters (issue #374): when the pins lived inside a workflow file,
//! every PHP bump had to edit `.github/workflows/release.yml`, and the nightly
//! automated bump runs as `GITHUB_TOKEN` — which the GitHub API forbids from
//! pushing changes under `.github/workflows/`. So the bump PR failed at push
//! every night and no bump could ever land (this once shipped Windows PHP 8.3
//! with OPcache silently off for three weeks). With the matrix derived here, a
//! bump touches only Rust (`PHP_SDK_VERSIONS`) plus docs, both of which
//! `GITHUB_TOKEN` may push.
//!
//! The `setup` job in `release.yml` runs `php-versions --github-matrix` and
//! pipes the `key=value` lines into `$GITHUB_OUTPUT`; the build jobs then
//! consume each matrix with `strategy.matrix: ${{ fromJSON(...) }}`.
//!
//! No `serde` dependency: the emitted objects are flat and their values
//! (version strings, booleans) never need JSON escaping, so the small builders
//! below assemble the JSON by hand and are unit-tested for shape.

use std::process::ExitCode;

use crate::{DEFAULT_PHP_MINOR, PHP_SDK_VERSIONS, TAILCALL_MINORS};

/// JSON array of every pinned `{minor, full}` pair.
///
/// The matrix for `build-linux`, `build-linux-arm64`, `build-macos` and
/// `build-windows` — every platform builds every supported minor.
fn php_matrix_json() -> String {
    let items: Vec<String> = PHP_SDK_VERSIONS
        .iter()
        .map(|(minor, full)| format!(r#"{{"minor":"{minor}","full":"{full}"}}"#))
        .collect();
    format!("[{}]", items.join(","))
}

/// JSON array of the TAILCALL minors' `{minor, full}` pairs.
///
/// The matrix for the experimental, non-gating `build-windows-tailcall` job.
/// `TAILCALL_MINORS` is 8.5-only today — the TAILCALL VM does not exist in
/// PHP 8.3/8.4.
fn tailcall_matrix_json() -> String {
    let items: Vec<String> = PHP_SDK_VERSIONS
        .iter()
        .filter(|(minor, _)| TAILCALL_MINORS.contains(minor))
        .map(|(minor, full)| format!(r#"{{"minor":"{minor}","full":"{full}"}}"#))
        .collect();
    format!("[{}]", items.join(","))
}

/// JSON array of every pinned `{minor, full, default}` triple.
///
/// The matrix for `docker-image`. `default` marks the minor whose images get
/// the rolling `:latest` / `:vX.Y.Z` tags (see the tag logic in that job); it
/// is `true` for exactly `DEFAULT_PHP_MINOR` and `false` otherwise. Emitting an
/// explicit `false` (rather than omitting the key, as the old hand-written
/// matrix did for 8.3) is behaviour-identical: the job's `[ "$IS_DEFAULT" =
/// "true" ]` test reads an unset `matrix.default` as the empty string, which is
/// also not `"true"`.
fn docker_matrix_json() -> String {
    let items: Vec<String> = PHP_SDK_VERSIONS
        .iter()
        .map(|(minor, full)| {
            let is_default = *minor == DEFAULT_PHP_MINOR;
            format!(r#"{{"minor":"{minor}","full":"{full}","default":{is_default}}}"#)
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// Entry point for `cargo xtask php-versions [--github-matrix | --json]`.
///
/// * `--github-matrix` prints the three `key=value` lines the `release.yml`
///   `setup` job appends to `$GITHUB_OUTPUT`.
/// * otherwise (or with `--json`) prints one combined JSON object — a
///   human/debug view and a stable contract for any other consumer.
pub fn run(args: &[String]) -> ExitCode {
    let github = args.iter().any(|a| a == "--github-matrix");

    if github {
        // Consumed by `>> "$GITHUB_OUTPUT"`; each value is a single-line JSON
        // array (no newlines inside), so the plain `key=value` form is safe.
        println!("php_matrix={}", php_matrix_json());
        println!("tailcall_matrix={}", tailcall_matrix_json());
        println!("docker_matrix={}", docker_matrix_json());
    } else {
        println!(
            r#"{{"php_matrix":{},"tailcall_matrix":{},"docker_matrix":{}}}"#,
            php_matrix_json(),
            tailcall_matrix_json(),
            docker_matrix_json()
        );
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `php_matrix` has exactly one `{minor, full}` object per pinned minor,
    /// each carrying the table's own values. Asserted by derivation from
    /// `PHP_SDK_VERSIONS` (not hardcoded versions) so a bump never has to touch
    /// this test — that is the whole point of moving the pins out of files.
    #[test]
    fn php_matrix_has_one_object_per_pinned_minor() {
        let json = php_matrix_json();
        for (minor, full) in PHP_SDK_VERSIONS {
            assert!(
                json.contains(&format!(r#"{{"minor":"{minor}","full":"{full}"}}"#)),
                "php_matrix missing {minor} → {full}: {json}"
            );
        }
        assert_eq!(
            json.matches(r#""minor""#).count(),
            PHP_SDK_VERSIONS.len(),
            "php_matrix object count must equal the table length: {json}"
        );
    }

    /// `tailcall_matrix` is exactly the TAILCALL subset of the table — same
    /// full versions the table pins, no more, no fewer.
    #[test]
    fn tailcall_matrix_is_the_tailcall_subset() {
        let json = tailcall_matrix_json();
        for (minor, full) in PHP_SDK_VERSIONS {
            let present = json.contains(&format!(r#"{{"minor":"{minor}","full":"{full}"}}"#));
            assert_eq!(
                present,
                TAILCALL_MINORS.contains(minor),
                "tailcall_matrix membership wrong for {minor}: {json}"
            );
        }
        assert_eq!(json.matches(r#""minor""#).count(), TAILCALL_MINORS.len(), "{json}");
    }

    /// `docker_matrix` marks exactly the default minor `true` and every other
    /// minor `false`, and covers every pinned minor.
    #[test]
    fn docker_matrix_flags_only_the_default_minor() {
        let json = docker_matrix_json();
        for (minor, full) in PHP_SDK_VERSIONS {
            let is_default = *minor == DEFAULT_PHP_MINOR;
            assert!(
                json.contains(&format!(
                    r#"{{"minor":"{minor}","full":"{full}","default":{is_default}}}"#
                )),
                "docker_matrix wrong entry for {minor} (expected default={is_default}): {json}"
            );
        }
        assert_eq!(json.matches(r#""default":true"#).count(), 1, "exactly one default: {json}");
        assert_eq!(json.matches(r#""minor""#).count(), PHP_SDK_VERSIONS.len(), "{json}");
    }

    /// Every emitted matrix is valid, non-empty JSON array syntax (balanced
    /// brackets, comma-separated objects) — a cheap guard that `fromJSON` in
    /// the workflow will not choke.
    #[test]
    fn matrices_are_well_formed_arrays() {
        for json in [php_matrix_json(), tailcall_matrix_json(), docker_matrix_json()] {
            assert!(json.starts_with('[') && json.ends_with(']'), "not an array: {json}");
            let inner = &json[1..json.len() - 1];
            let objects = inner.split("},{").count();
            // No trailing/leading commas, one more `{` than `},{` separators.
            assert_eq!(inner.matches('{').count(), objects, "object/comma mismatch: {json}");
            assert!(!inner.contains(",,") && !inner.starts_with(',') && !inner.ends_with(','));
        }
    }
}
