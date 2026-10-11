//! php-cli personality: make the `ephpm` binary answer like `php` when it is
//! re-invoked *as* PHP.
//!
//! Under `ephpm php`, the `PHP_BINARY` constant names the running `ephpm`
//! executable (issue #339) — there is no separate `php` file to point it at in a
//! single-binary distribution. Libraries that re-invoke PHP do exactly what
//! php-cli promises them they can: `PHP_BINARY script.php`, `PHP_BINARY -r …`,
//! `PHP_BINARY -d k=v -n file`, or plain `PHP_BINARY` with code on stdin
//! (spatie/async and every Symfony Process-based tool through
//! `PhpExecutableFinder`, Composer's `@php` script handler, PHPUnit process
//! isolation). Before this module those argv shapes hit `ephpm`'s clap parser:
//! a usage error for `script.php`/`-r`, and — worst — a bare `PHP_BINARY`
//! started the **dev server** (no subcommand means `ephpm dev`).
//!
//! The binary now takes on the php-cli personality, with every argument handed
//! verbatim to the embedded PHP CLI exactly as `ephpm php <args…>` would, when
//! any of these holds:
//!
//! 1. **argv\[0\] is php-named** — the file stem is `php` or `php` followed by
//!    a version (`php8`, `php8.4`, `php84`; `.exe` stripped on Windows). This
//!    is the `ln -s ephpm /usr/local/bin/php` install: no flags, no marker,
//!    the binary simply *is* `php`.
//! 2. **The inherited marker is set and argv\[1\] is not an `ephpm`
//!    subcommand.** `ephpm php` and `ephpm composer` export [`MARKER_ENV`]
//!    `=1` into their own environment before PHP (or vivacity) runs, so every
//!    child they spawn inherits it. A child `ephpm` that sees the marker and is
//!    *not* asked for a real subcommand (`ephpm kv …`, `ephpm cache reset`, …
//!    keep working from inside PHP scripts) is a re-invocation of
//!    `PHP_BINARY`, including the no-argument stdin form and `-v`/`-h`/
//!    `--version`, which then mean what they mean to php.
//! 3. **No marker, but argv\[1\] is php-shaped** — an option clap would have
//!    rejected anyway (anything starting with `-` other than ePHPm's own
//!    `-h`/`--help`/`-V`/`--version`), or an existing file that is not a
//!    subcommand name. This catches re-invocations whose environment was
//!    replaced wholesale (`proc_open(…, env: [...])` with an explicit array),
//!    and only ever turns what used to be a clap usage error into a PHP run;
//!    a typo'd subcommand (`ephpm sevre`) that names no file still gets clap's
//!    "did you mean" diagnostic. The no-argument form cannot be recovered
//!    without the marker — bare `ephpm` is the dev server by contract.
//!
//! The rules are a pure function ([`php_args`]) so they are unit-tested
//! without a PHP-linked build.

use std::ffi::{OsStr, OsString};
use std::path::Path;

/// Environment variable `ephpm php` / `ephpm composer` export so that a child
/// `ephpm` started through `PHP_BINARY` knows it is being run as `php`.
///
/// Deliberately not under `[php]`'s `EPHPM_PHP__` nesting: figment's
/// `Env::prefixed("EPHPM_")` maps this to the top-level key `php_cli`, which the
/// lenient top-level `Config` ignores, so an `ephpm serve` started from inside a
/// PHP script is unaffected.
pub(crate) const MARKER_ENV: &str = "EPHPM_PHP_CLI";

/// ePHPm's own top-level flags. Without the marker these keep their ePHPm
/// meaning (`ephpm -V` is ePHPm's version); with it they are php's.
const EPHPM_TOP_LEVEL_FLAGS: &[&str] = &["-h", "--help", "-V", "--version"];

/// Decide whether this process should run as php-cli, and if so return the
/// argument list for the embedded PHP CLI (argv\[1..\], verbatim).
///
/// `is_subcommand` answers whether a token names an `ephpm` subcommand
/// (including clap's implicit `help`). `is_file` answers whether a token names
/// an existing regular file; it is injected so the rules stay pure.
///
/// # Errors
///
/// Returns an error when the php personality applies but an argument is not
/// valid UTF-8 — the embedded CLI takes `String`s, and silently rewriting a
/// filename lossily would run (or create) the wrong file.
pub(crate) fn php_args(
    argv: &[OsString],
    marker: bool,
    is_subcommand: impl Fn(&str) -> bool,
    is_file: impl Fn(&str) -> bool,
) -> anyhow::Result<Option<Vec<String>>> {
    let rest = argv.get(1..).unwrap_or_default();
    let applies = if argv.first().is_some_and(|a0| is_php_name(a0)) {
        true
    } else {
        match rest.first().map(|a| a.to_str()) {
            // Bare `ephpm` (php reading stdin under the marker, else the dev
            // server), or a non-UTF-8 argv[1], which is never a subcommand.
            None | Some(None) => marker,
            Some(Some(first)) if is_subcommand(first) => false,
            Some(Some(_)) if marker => true,
            Some(Some(first)) => {
                (first.starts_with('-') && !EPHPM_TOP_LEVEL_FLAGS.contains(&first))
                    || is_file(first)
            }
        }
    };
    if !applies {
        return Ok(None);
    }
    rest.iter()
        .map(|a| {
            a.to_str().map(str::to_owned).ok_or_else(|| {
                anyhow::anyhow!(
                    "argument `{}` is not valid UTF-8; the embedded php CLI needs UTF-8",
                    a.display()
                )
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()
        .map(Some)
}

/// `true` when `argv0`'s file stem is `php` or `php` + a version made only of
/// digits and dots (`php8`, `php8.4`, `php84`). Anything else (`php-fpm`,
/// `phpunit`, `ephpm`) is not.
fn is_php_name(argv0: &OsStr) -> bool {
    let Some(stem) = Path::new(argv0).file_stem().and_then(OsStr::to_str) else {
        return false;
    };
    // `Path::file_stem` keeps everything before the LAST dot, so `php8.4`
    // arrives as `php8` + extension `4`. Re-attach a purely numeric extension
    // (a version, not `.exe`) so the check sees the whole token.
    let name = match Path::new(argv0).extension().and_then(OsStr::to_str) {
        Some(ext) if !ext.is_empty() && ext.bytes().all(|b| b.is_ascii_digit()) => {
            format!("{stem}.{ext}")
        }
        _ => stem.to_owned(),
    };
    // Windows file names are case-insensitive: `PHP.EXE` is `php.exe`.
    let name = if cfg!(windows) { name.to_ascii_lowercase() } else { name };
    let Some(version) = name.strip_prefix("php") else {
        return false;
    };
    version.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && !version.starts_with('.')
        && !version.ends_with('.')
}

/// Whether the inherited marker is set in this process's environment.
pub(crate) fn marker_set() -> bool {
    std::env::var_os(MARKER_ENV).is_some_and(|v| v == "1")
}

/// Export [`MARKER_ENV`]`=1` (and, for `ephpm composer`, `VIVACITY_PHP`) so the
/// children PHP or vivacity spawn can re-invoke this binary as `php`.
///
/// `vivacity` runs its platform probe as `$VIVACITY_PHP <probe.php>` (falling
/// back to `php` on `PATH`); pointing it at this binary is what lets
/// `ephpm composer update`/`require`/`remove` work with no system PHP. Only
/// done when PHP is actually linked into this binary (a stub build cannot run
/// the probe) and the operator has not chosen a `VIVACITY_PHP` themselves.
///
/// Must be called while the process is still single-threaded: before the
/// `--site` database runtime, PHP startup, or `vivacity::run`.
pub(crate) fn export_for_children(point_vivacity_here: bool) {
    let vivacity_php = if point_vivacity_here
        && ephpm_php::PHP_LINKED
        // An empty value counts as unset: vivacity would try to run "".
        && std::env::var_os("VIVACITY_PHP").is_none_or(|v| v.is_empty())
    {
        std::env::current_exe().ok()
    } else {
        None
    };

    #[allow(unsafe_code)]
    // SAFETY: `set_var` is unsound only when another thread reads or writes
    // the environment concurrently. Every caller runs on the main thread at
    // the top of its subcommand, before any thread is spawned (no tokio
    // runtime, no PHP threads, no vivacity/reqwest workers exist yet), and the
    // fatal-signal handler installed earlier never touches the environment.
    unsafe {
        std::env::set_var(MARKER_ENV, "1");
        if let Some(exe) = vivacity_php {
            std::env::set_var("VIVACITY_PHP", exe);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUBCOMMANDS: &[&str] = &["serve", "dev", "php", "composer", "kv", "cache", "help"];

    fn argv(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    fn decide(items: &[&str], marker: bool, files: &[&str]) -> Option<Vec<String>> {
        php_args(&argv(items), marker, |s| SUBCOMMANDS.contains(&s), |s| files.contains(&s))
            .expect("utf-8 argv")
    }

    #[test]
    fn php_named_argv0_is_always_php() {
        // `\` is a path separator only on Windows.
        // Windows names are case-insensitive.
        let windows_path: &[&str] = if cfg!(windows) {
            &[r"C:\tools\php8.4.exe", r"C:\PHP\PHP.EXE", "Php8.exe"]
        } else {
            &[]
        };
        for &a0 in
            ["php", "/usr/local/bin/php", "php8", "php8.4", "/opt/bin/php8.5", "php84", "php.exe"]
                .iter()
                .chain(windows_path)
        {
            assert_eq!(
                decide(&[a0, "serve", "-x"], false, &[]),
                Some(vec!["serve".to_owned(), "-x".to_owned()]),
                "argv0 {a0} must select php even for a subcommand-shaped argv[1]"
            );
            assert_eq!(decide(&[a0], false, &[]), Some(vec![]), "bare {a0} reads stdin");
        }
    }

    #[test]
    fn non_php_argv0_is_not_php() {
        for a0 in ["ephpm", "php-fpm", "phpunit", "phpize", "php8-x", "ephpm.exe", "xphp"] {
            assert_eq!(decide(&[a0, "serve"], false, &[]), None, "argv0 {a0}");
        }
        // Unix names are case-sensitive: `PHP` is not `php` there.
        if !cfg!(windows) {
            assert_eq!(decide(&["PHP", "serve"], false, &[]), None);
        }
    }

    #[test]
    fn marker_turns_non_subcommands_into_php() {
        assert_eq!(decide(&["ephpm"], true, &[]), Some(vec![]), "bare PHP_BINARY reads stdin");
        assert_eq!(
            decide(&["ephpm", "-d", "x=1", "-r", "echo 1;"], true, &[]),
            Some(argv_s(&["-d", "x=1", "-r", "echo 1;"]))
        );
        assert_eq!(decide(&["ephpm", "script.php"], true, &[]), Some(argv_s(&["script.php"])));
        // php's -v/-h/--version, not ePHPm's.
        for flag in ["-v", "-h", "--help", "-V", "--version"] {
            assert_eq!(decide(&["ephpm", flag], true, &[]), Some(argv_s(&[flag])), "{flag}");
        }
        // `--` and everything after it survive verbatim (no clap in the way).
        assert_eq!(
            decide(&["ephpm", "--", "a", "-b"], true, &[]),
            Some(argv_s(&["--", "a", "-b"]))
        );
    }

    #[test]
    fn marker_keeps_real_subcommands() {
        assert_eq!(decide(&["ephpm", "kv", "get", "k"], true, &[]), None);
        assert_eq!(decide(&["ephpm", "help"], true, &[]), None);
        // `php` stays the subcommand (its own --config/--site handling).
        assert_eq!(decide(&["ephpm", "php", "-r", "1;"], true, &[]), None);
    }

    #[test]
    fn without_marker_only_clap_rejected_shapes_become_php() {
        assert_eq!(decide(&["ephpm"], false, &[]), None, "bare ephpm is the dev server");
        assert_eq!(decide(&["ephpm", "serve"], false, &[]), None);
        for flag in ["-h", "--help", "-V", "--version"] {
            assert_eq!(decide(&["ephpm", flag], false, &[]), None, "{flag} stays ePHPm's");
        }
        assert_eq!(
            decide(&["ephpm", "-r", "echo 1;"], false, &[]),
            Some(argv_s(&["-r", "echo 1;"]))
        );
        assert_eq!(decide(&["ephpm", "-v"], false, &[]), Some(argv_s(&["-v"])));
        assert_eq!(
            decide(&["ephpm", "/tmp/x/probe.php", "a"], false, &["/tmp/x/probe.php"]),
            Some(argv_s(&["/tmp/x/probe.php", "a"]))
        );
        // A typo that names no file keeps clap's diagnostic.
        assert_eq!(decide(&["ephpm", "sevre"], false, &[]), None);
        // A file named like a subcommand still runs the subcommand.
        assert_eq!(decide(&["ephpm", "serve"], false, &["serve"]), None);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_argument_is_an_error_not_a_lossy_rewrite() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(vec![b'a', 0xff]);
        let argv = vec![OsString::from("php"), bad];
        assert!(php_args(&argv, false, |_| false, |_| false).is_err());
    }

    fn argv_s(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }
}
