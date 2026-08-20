//! Differential CLI conformance harness: `ephpm php` vs upstream `php`.
//!
//! Runs every case in the corpus (`tests/cli-conformance/`) against BOTH
//! binaries with an identical minimal environment and compares stdout,
//! stderr, and the exit code byte-for-byte. Any unexplained mismatch fails
//! the run. The point is to catch drift between the embedded PHP CLI
//! (`ephpm php`, `ephpm_cli_main` in `crates/ephpm-php/ephpm_wrapper.c`)
//! and the genuine php-cli SAPI at the same PHP version.
//!
//! Corpus format (one case = one stem `NNN-name`):
//! - `NNN-name.php`    — the script (optional when `.args` is present)
//! - `NNN-name.args`   — CLI args, one per line; `{SCRIPT}` expands to the
//!   absolute `.php` path, `{TMPDIR}` to the per-side scratch dir; full-line
//!   `#` comments and empty lines are ignored
//! - `NNN-name.stdin`  — bytes piped to stdin (default: empty, closed)
//! - `NNN-name.meta`   — TOML-subset sidecar:
//!   `skip = "reason"`, `xfail = "reason (#NNN)"`,
//!   `normalize = ["strip-versions", ...]`, `no_default_ini = true`
//!
//! Unless `no_default_ini = true`, `-n` is injected as the first PHP arg on
//! BOTH sides, so the comparison is compiled-default against compiled-default
//! rather than "whatever the distro php.ini says" (a distro `log_errors=On`
//! artifact burned the #317 diff-testing). Case `-d` flags still apply on top.
//!
//! Both children run with a cleared environment plus a fixed, documented set
//! (`TZ=UTC`, `LC_ALL=C`, ...), cwd'd into their own scratch directory that
//! is also exported as `CONFORMANCE_TMPDIR`. Cases must not write anywhere
//! else, use wall-clock time, unseeded randomness, or the network.
//!
//! Identity assertion (by construction, not convention): before any case
//! runs, the harness executes `<ephpm> php -n -v` and requires a `PHP x.y.z`
//! banner. A plain `php` binary handed to `--ephpm` by mistake would try to
//! run a script literally named "php" and fail this check — a guard against
//! the WSL `$VAR`-expansion bridge bug that has repeatedly made agents
//! measure the system php while believing they measured ephpm.
//!
//! Version skew: the harness refuses to compare across PHP *minors* and
//! prints a loud warning (plus a `::warning::` annotation under GitHub
//! Actions) when the *patch* versions differ — upstream patch availability
//! is best-effort in CI (see `.github/workflows/cli-conformance.yml`).

use std::fmt::Write as _;
use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use crate::workspace_root;

/// Corpus location, relative to the workspace root. Lives under `tests/`
/// alongside the other non-workspace test assets (`tests/php`, `tests/smoke`)
/// rather than a new top-level directory.
const CORPUS_DIR: &str = "tests/cli-conformance";

/// Per-case wall-clock budget. Generous — every case is small — but bounded
/// so a corpus bug (accidental infinite loop) can't wedge a nightly runner
/// until the job-level timeout fires.
const CASE_TIMEOUT: Duration = Duration::from_secs(60);

/// The full registry of named normalizers a `.meta` file may reference.
/// Keep this list SMALL: every entry exists because some output is
/// *definitionally* different between the two binaries, never as a way to
/// paper over a behavioral divergence.
const NORMALIZERS: &[&str] = &[
    // `X.Y.Z[suffix]` version tokens and `(built: ...)` stamps. For banner
    // shape tests (`php -v`) where the embedded SDK and a distro php can
    // never agree on the literal version/build strings.
    "strip-versions",
    // `(ZTS)` / `(NTS)` → `(TS)`. ephpm's Linux libphp is ZTS; upstream
    // php-cli is almost always NTS. A real, permanent build-flavor
    // difference, not drift.
    "strip-zts-marker",
    // Drops `with Zend OPcache ...` banner lines. Distro php loads opcache
    // as a zend_extension and advertises it in `-v`; the embedded build
    // does not print this line.
    "strip-opcache-banner",
    // Replaces each side's own binary path with `<BINARY>`. For PHP_BINARY
    // and friends: the two processes are by definition different files.
    "strip-binary-path",
];

// ---------------------------------------------------------------------------
// CLI entry point
// ---------------------------------------------------------------------------

struct Options {
    php: PathBuf,
    ephpm: Option<PathBuf>,
    filter: Option<String>,
    report: Option<PathBuf>,
    corpus: PathBuf,
}

fn print_usage() {
    eprintln!(
        "\
Usage: cargo xtask cli-conformance --php <upstream-php> [options]

Options:
  --php <path>       Path to the upstream php CLI binary (required)
  --ephpm <path>     Path to the ephpm binary (default: target/<triple>/release/ephpm)
  --filter <substr>  Only run cases whose name contains <substr>
  --report <path>    Write the full report to <path> (summary still prints)
  --corpus <dir>     Corpus directory (default: {CORPUS_DIR} under the workspace root)"
    );
}

/// Entry point for `cargo xtask cli-conformance`.
pub fn run(args: &[String]) -> ExitCode {
    let opts = match parse_options(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}");
            print_usage();
            return ExitCode::FAILURE;
        }
    };

    let ephpm = match opts.ephpm.clone().or_else(default_ephpm_binary) {
        Some(p) if p.exists() => p,
        Some(p) => {
            eprintln!("error: ephpm binary not found at {}", p.display());
            return ExitCode::FAILURE;
        }
        None => {
            eprintln!(
                "error: no ephpm binary found under target/ — build one with \
                 `cargo xtask release` or pass --ephpm <path>"
            );
            return ExitCode::FAILURE;
        }
    };

    match run_conformance(&opts, &ephpm) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut php = None;
    let mut ephpm = None;
    let mut filter = None;
    let mut report = None;
    let mut corpus = None;
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let mut take = |name: &str| -> Result<String, String> {
            i += 1;
            args.get(i).cloned().ok_or_else(|| format!("{name} requires a value"))
        };
        match flag {
            "--php" => php = Some(PathBuf::from(take("--php")?)),
            "--ephpm" => ephpm = Some(PathBuf::from(take("--ephpm")?)),
            "--filter" => filter = Some(take("--filter")?),
            "--report" => report = Some(PathBuf::from(take("--report")?)),
            "--corpus" => corpus = Some(PathBuf::from(take("--corpus")?)),
            "--help" | "-h" => return Err("help requested".to_string()),
            other => return Err(format!("unknown option '{other}'")),
        }
        i += 1;
    }
    let php = php.ok_or("--php <upstream-php> is required")?;
    let corpus = corpus.unwrap_or_else(|| workspace_root().join(CORPUS_DIR));
    Ok(Options { php, ephpm, filter, report, corpus })
}

/// Default ephpm binary location: the deterministic per-triple release path
/// `cargo xtask release` produces, falling back to plain `target/release`.
fn default_ephpm_binary() -> Option<PathBuf> {
    let root = workspace_root();
    let arch = env::consts::ARCH;
    let candidates = [
        root.join(format!("target/{arch}-unknown-linux-gnu/release/ephpm")),
        root.join(format!("target/{arch}-apple-darwin/release/ephpm")),
        root.join("target/x86_64-pc-windows-msvc/release/ephpm.exe"),
        root.join("target/release/ephpm"),
        root.join("target/release/ephpm.exe"),
    ];
    candidates.into_iter().find(|p| p.exists())
}

// ---------------------------------------------------------------------------
// Case model & corpus discovery
// ---------------------------------------------------------------------------

/// Sidecar metadata for one case (`NNN-name.meta`).
#[derive(Debug, Default, PartialEq)]
struct Meta {
    /// Don't run the case at all. The reason string must say WHY the
    /// comparison is meaningless (legitimate, permanent difference).
    skip: Option<String>,
    /// Run the case, expect a mismatch. The reason must reference the issue
    /// tracking the divergence (e.g. "#331"). An xfail that MATCHES is
    /// reported as XPASS and fails the run: stale markers must be removed.
    xfail: Option<String>,
    /// Named normalizers (see [`NORMALIZERS`]) applied to both sides'
    /// output before comparison.
    normalize: Vec<String>,
    /// Suppress the default `-n` injection (both sides). For cases that
    /// test ini-loading behavior itself.
    no_default_ini: bool,
}

/// One discovered conformance case.
struct Case {
    stem: String,
    script: Option<PathBuf>,
    /// Raw arg tokens (placeholders not yet expanded).
    args: Vec<String>,
    stdin: Vec<u8>,
    meta: Meta,
}

/// Parse the TOML-subset `.meta` format. Only the four known keys are
/// accepted; anything else is an error so typos can't silently disable a
/// marker.
fn parse_meta(text: &str) -> Result<Meta, String> {
    let mut meta = Meta::default();
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("line {}: expected `key = value`", lineno + 1))?;
        let (key, value) = (key.trim(), value.trim());
        match key {
            "skip" => meta.skip = Some(parse_string(value, lineno)?),
            "xfail" => meta.xfail = Some(parse_string(value, lineno)?),
            "normalize" => {
                meta.normalize = parse_string_array(value, lineno)?;
                for n in &meta.normalize {
                    if !NORMALIZERS.contains(&n.as_str()) {
                        return Err(format!(
                            "line {}: unknown normalizer '{n}' (known: {})",
                            lineno + 1,
                            NORMALIZERS.join(", ")
                        ));
                    }
                }
            }
            "no_default_ini" => {
                meta.no_default_ini = match value {
                    "true" => true,
                    "false" => false,
                    other => {
                        return Err(format!(
                            "line {}: expected true/false, got {other}",
                            lineno + 1
                        ));
                    }
                };
            }
            other => return Err(format!("line {}: unknown key '{other}'", lineno + 1)),
        }
    }
    if meta.skip.is_some() && meta.xfail.is_some() {
        return Err("skip and xfail are mutually exclusive".to_string());
    }
    if let Some(reason) = meta.skip.as_deref().or(meta.xfail.as_deref())
        && reason.trim().is_empty()
    {
        return Err("skip/xfail reason must not be empty".to_string());
    }
    Ok(meta)
}

fn parse_string(value: &str, lineno: usize) -> Result<String, String> {
    let inner = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .ok_or_else(|| format!("line {}: expected a double-quoted string", lineno + 1))?;
    if inner.contains('"') {
        return Err(format!("line {}: embedded quotes are not supported", lineno + 1));
    }
    Ok(inner.to_string())
}

fn parse_string_array(value: &str, lineno: usize) -> Result<Vec<String>, String> {
    let inner = value
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .ok_or_else(|| format!("line {}: expected [\"a\", \"b\"]", lineno + 1))?;
    let mut out = Vec::new();
    for item in inner.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        out.push(parse_string(item, lineno)?);
    }
    Ok(out)
}

/// Parse an `.args` file: one argv token per line, `#` full-line comments and
/// blank lines ignored. Trailing `\r` is stripped so a CRLF checkout can't
/// smuggle carriage returns into argv.
fn parse_args_file(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Discover the corpus: every unique `NNN-name` stem with a `.php` or
/// `.args` file. Orphan `.meta`/`.stdin` files (typo'd stems) are an error.
fn discover_cases(corpus: &Path, filter: Option<&str>) -> Result<Vec<Case>, String> {
    let entries = fs::read_dir(corpus)
        .map_err(|e| format!("cannot read corpus dir {}: {e}", corpus.display()))?;
    let mut stems: Vec<String> = Vec::new();
    let mut sidecars: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(stem) = name.strip_suffix(".php").or_else(|| name.strip_suffix(".args")) {
            if !stems.iter().any(|s| s == stem) {
                stems.push(stem.to_string());
            }
        } else if let Some(stem) =
            name.strip_suffix(".meta").or_else(|| name.strip_suffix(".stdin"))
        {
            sidecars.push(stem.to_string());
        }
    }
    stems.sort();
    if let Some(orphan) = sidecars.iter().find(|s| !stems.contains(s)) {
        return Err(format!(
            "sidecar file for '{orphan}' has no matching .php/.args case — typo in the stem?"
        ));
    }

    let mut cases = Vec::new();
    for stem in stems {
        if let Some(f) = filter
            && !stem.contains(f)
        {
            continue;
        }
        let script = corpus.join(format!("{stem}.php"));
        let args_path = corpus.join(format!("{stem}.args"));
        let stdin_path = corpus.join(format!("{stem}.stdin"));
        let meta_path = corpus.join(format!("{stem}.meta"));

        let script = script.exists().then(|| script.canonicalize().unwrap_or(script));
        let args = if args_path.exists() {
            parse_args_file(
                &fs::read_to_string(&args_path)
                    .map_err(|e| format!("{}: {e}", args_path.display()))?,
            )
        } else if script.is_some() {
            vec!["{SCRIPT}".to_string()]
        } else {
            return Err(format!("case '{stem}' has neither a .php nor an .args file"));
        };
        if args.iter().any(|a| a.contains("{SCRIPT}")) && script.is_none() {
            return Err(format!("case '{stem}': args reference {{SCRIPT}} but no .php exists"));
        }
        let stdin = if stdin_path.exists() {
            fs::read(&stdin_path).map_err(|e| e.to_string())?
        } else {
            Vec::new()
        };
        let meta = if meta_path.exists() {
            parse_meta(&fs::read_to_string(&meta_path).map_err(|e| e.to_string())?)
                .map_err(|e| format!("{}: {e}", meta_path.display()))?
        } else {
            Meta::default()
        };
        cases.push(Case { stem, script, args, stdin, meta });
    }
    Ok(cases)
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// Captured output of one binary run.
#[derive(Debug, Clone, PartialEq)]
struct Capture {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Exit code, or the signal description when killed.
    exit: ExitKind,
}

#[derive(Debug, Clone, PartialEq)]
enum ExitKind {
    Code(i32),
    /// Killed by the harness watchdog or a signal — never comparable-equal.
    Abnormal(String),
}

impl std::fmt::Display for ExitKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExitKind::Code(c) => write!(f, "{c}"),
            ExitKind::Abnormal(s) => write!(f, "abnormal ({s})"),
        }
    }
}

/// One side of the comparison: which binary, and the fixed argv prefix that
/// makes it a PHP CLI (`["php"]` for ephpm, empty for upstream).
struct Side<'a> {
    label: &'static str,
    bin: &'a Path,
    prefix: &'static [&'static str],
}

/// Expand `{SCRIPT}` / `{TMPDIR}` placeholders in one arg token.
fn expand_arg(arg: &str, script: Option<&Path>, tmpdir: &Path) -> String {
    let mut out = arg.to_string();
    if let Some(s) = script {
        out = out.replace("{SCRIPT}", &s.to_string_lossy());
    }
    out.replace("{TMPDIR}", &tmpdir.to_string_lossy())
}

/// Build the full argv (after the binary) for one side of one case.
fn build_argv(side_prefix: &[&str], case: &Case, tmpdir: &Path) -> Vec<String> {
    let mut argv: Vec<String> = side_prefix.iter().map(|s| (*s).to_string()).collect();
    if !case.meta.no_default_ini {
        argv.push("-n".to_string());
    }
    for a in &case.args {
        argv.push(expand_arg(a, case.script.as_deref(), tmpdir));
    }
    argv
}

/// Run one binary with the harness's minimal, fixed environment. stdin is
/// always piped and closed after writing, so a program that reads stdin sees
/// clean EOF. A watchdog kills the child after [`CASE_TIMEOUT`].
fn run_side(
    bin: &Path,
    argv: &[String],
    stdin_data: &[u8],
    tmpdir: &Path,
) -> Result<Capture, String> {
    let mut cmd = Command::new(bin);
    cmd.args(argv)
        .current_dir(tmpdir)
        .env_clear()
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("HOME", tmpdir)
        .env("CONFORMANCE_TMPDIR", tmpdir)
        // Fixed value for the getenv-passthrough case.
        .env("CONFORMANCE_TEST_VAR", "hello-conformance")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Keep PATH: PHP itself doesn't need it, but libc/loader quirks on some
    // distros do, and both sides get the identical value.
    if let Ok(path) = env::var("PATH") {
        cmd.env("PATH", path);
    }
    // SystemRoot is required for any process start on Windows.
    if cfg!(windows)
        && let Ok(sysroot) = env::var("SystemRoot")
    {
        cmd.env("SystemRoot", sysroot);
    }

    let mut child = cmd.spawn().map_err(|e| format!("spawn {}: {e}", bin.display()))?;

    let mut stdin = child.stdin.take().expect("stdin piped");
    // Ignore EPIPE: a case that never reads stdin may exit first.
    let _ = stdin.write_all(stdin_data);
    drop(stdin);

    let mut out_pipe = child.stdout.take().expect("stdout piped");
    let mut err_pipe = child.stderr.take().expect("stderr piped");
    let out_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        buf
    });
    let err_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        buf
    });

    let start = Instant::now();
    let exit = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => {
                break match status.code() {
                    Some(c) => ExitKind::Code(c),
                    None => ExitKind::Abnormal(format!("{status}")),
                };
            }
            None if start.elapsed() > CASE_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                break ExitKind::Abnormal(format!(
                    "killed after {}s timeout",
                    CASE_TIMEOUT.as_secs()
                ));
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    };

    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    Ok(Capture { stdout, stderr, exit })
}

// ---------------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------------

/// Per-side context handed to normalizers.
struct NormCtx<'a> {
    /// The binary path exactly as invoked (for `strip-binary-path`).
    binary: &'a str,
    /// The per-side scratch dir (always normalized — it is harness-provided
    /// and definitionally differs between the sides).
    tmpdir: &'a str,
}

/// Replace every occurrence of `needle` with `replacement` (byte-level).
fn replace_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(needle) {
            out.extend_from_slice(replacement);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

/// `strip-versions`: `X.Y.Z[+suffix]` → `<VERSION>`, `(built: ...)` →
/// `(built: <DATE>)`. Suffix chars cover distro version decorations like
/// `8.5.4-1+ubuntu24.04.1+deb.sury.org+1`.
fn strip_versions(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        // `(built: Jul 16 2026 18:56:38)` — replace the payload.
        if input[i..].starts_with(b"(built: ") {
            if let Some(close) = input[i..].iter().position(|&b| b == b')') {
                out.extend_from_slice(b"(built: <DATE>)");
                i += close + 1;
                continue;
            }
        }
        // A version token must not start mid-number: letters before it are
        // fine (`Zend Engine v4.5.7`), digits or dots are not.
        let at_boundary = i == 0 || (!input[i - 1].is_ascii_digit() && input[i - 1] != b'.');
        if at_boundary && input[i].is_ascii_digit() {
            if let Some(len) = match_version(&input[i..]) {
                out.extend_from_slice(b"<VERSION>");
                i += len;
                continue;
            }
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

/// Match `\d+\.\d+\.\d+` plus any trailing `[0-9A-Za-z.+~_-]` decoration at
/// the start of `s`; returns the matched length.
fn match_version(s: &[u8]) -> Option<usize> {
    let mut i = 0;
    for part in 0..3 {
        if part > 0 {
            if s.get(i) != Some(&b'.') {
                return None;
            }
            i += 1;
        }
        let start = i;
        while s.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return None;
        }
    }
    while s.get(i).is_some_and(|&b| {
        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'~' | b'_' | b'-')
    }) {
        i += 1;
    }
    Some(i)
}

/// `strip-opcache-banner`: drop whole lines that mention the Zend OPcache
/// `-v` banner.
fn strip_opcache_banner(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    for line in input.split_inclusive(|&b| b == b'\n') {
        if !contains_bytes(line, b"with Zend OPcache") {
            out.extend_from_slice(line);
        }
    }
    out
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Apply the always-on tmpdir normalization plus the case's named
/// normalizers to one side's output channel.
fn normalize(input: &[u8], names: &[String], ctx: &NormCtx<'_>) -> Vec<u8> {
    let mut out = replace_bytes(input, ctx.tmpdir.as_bytes(), b"<TMPDIR>");
    for name in names {
        out = match name.as_str() {
            "strip-versions" => strip_versions(&out),
            "strip-zts-marker" => {
                let t = replace_bytes(&out, b"(ZTS)", b"(TS)");
                replace_bytes(&t, b"(NTS)", b"(TS)")
            }
            "strip-opcache-banner" => strip_opcache_banner(&out),
            "strip-binary-path" => replace_bytes(&out, ctx.binary.as_bytes(), b"<BINARY>"),
            // Unknown names are rejected at meta-parse time.
            other => unreachable!("unvalidated normalizer {other}"),
        };
    }
    out
}

// ---------------------------------------------------------------------------
// Comparison & diffing
// ---------------------------------------------------------------------------

/// Outcome of comparing the two sides of one case (post-normalization).
#[derive(Debug, PartialEq)]
struct Mismatch {
    stdout: bool,
    stderr: bool,
    exit: bool,
}

impl Mismatch {
    fn any(&self) -> bool {
        self.stdout || self.stderr || self.exit
    }
}

fn compare(
    upstream: &Capture,
    ephpm: &Capture,
    names: &[String],
    up_ctx: &NormCtx<'_>,
    ep_ctx: &NormCtx<'_>,
) -> (Mismatch, Capture, Capture) {
    let up = Capture {
        stdout: normalize(&upstream.stdout, names, up_ctx),
        stderr: normalize(&upstream.stderr, names, up_ctx),
        exit: upstream.exit.clone(),
    };
    let ep = Capture {
        stdout: normalize(&ephpm.stdout, names, ep_ctx),
        stderr: normalize(&ephpm.stderr, names, ep_ctx),
        exit: ephpm.exit.clone(),
    };
    let mismatch = Mismatch {
        stdout: up.stdout != ep.stdout,
        stderr: up.stderr != ep.stderr,
        exit: up.exit != ep.exit,
    };
    (mismatch, up, ep)
}

/// Line-based unified-ish diff (whole-output, no hunk headers — conformance
/// outputs are small). `-` lines are upstream php, `+` lines are ephpm.
fn unified_diff(upstream: &[u8], ephpm: &[u8]) -> String {
    let a: Vec<&str> = lossy_lines(upstream);
    let b: Vec<&str> = lossy_lines(ephpm);
    // Guard the O(n*m) LCS on pathological outputs.
    if a.len() > 500 || b.len() > 500 {
        return format!(
            "(output too large to diff: upstream {} lines, ephpm {} lines)\n",
            a.len(),
            b.len()
        );
    }
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] =
                if a[i] == b[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
        }
    }
    let mut out = String::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            let _ = writeln!(out, " {}", a[i]);
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            let _ = writeln!(out, "-{}", a[i]);
            i += 1;
        } else {
            let _ = writeln!(out, "+{}", b[j]);
            j += 1;
        }
    }
    for line in &a[i..] {
        let _ = writeln!(out, "-{line}");
    }
    for line in &b[j..] {
        let _ = writeln!(out, "+{line}");
    }
    out
}

/// Leak-free lossy line split for diff display. Non-UTF-8 bytes render as
/// `\xNN` escapes so binary-output differences are still visible.
fn lossy_lines(bytes: &[u8]) -> Vec<&str> {
    // We need &str lines borrowed from a stable buffer; escape lazily only
    // when invalid UTF-8 is present by leaking a boxed string (bounded: only
    // on failing cases, only in the report path).
    match std::str::from_utf8(bytes) {
        Ok(s) => s.lines().collect(),
        Err(_) => {
            let escaped: String = bytes
                .iter()
                .flat_map(|&b| {
                    if b == b'\n' || (b' '..=b'~').contains(&b) {
                        vec![b as char]
                    } else {
                        format!("\\x{b:02x}").chars().collect()
                    }
                })
                .collect();
            Box::leak(escaped.into_boxed_str()).lines().collect()
        }
    }
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum Status {
    Pass,
    Fail,
    Skip,
    Xfail,
    /// An `xfail` case that unexpectedly matched — the marker is stale.
    /// Fails the run so fixed divergences get their markers removed.
    Xpass,
}

/// Pure classification: given the case's markers and whether the outputs
/// mismatched, what is the verdict?
fn classify(meta: &Meta, mismatched: bool) -> Status {
    if meta.skip.is_some() {
        return Status::Skip;
    }
    match (&meta.xfail, mismatched) {
        (Some(_), true) => Status::Xfail,
        (Some(_), false) => Status::Xpass,
        (None, true) => Status::Fail,
        (None, false) => Status::Pass,
    }
}

impl Status {
    fn label(&self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::Skip => "SKIP",
            Status::Xfail => "XFAIL",
            Status::Xpass => "XPASS",
        }
    }
    fn fails_run(&self) -> bool {
        matches!(self, Status::Fail | Status::Xpass)
    }
}

// ---------------------------------------------------------------------------
// Version handling
// ---------------------------------------------------------------------------

/// Extract `x.y.z` from a `PHP x.y.z[suffix] (cli) ...` banner line.
fn parse_php_banner(first_line: &str) -> Option<(u32, u32, u32)> {
    let rest = first_line.strip_prefix("PHP ")?;
    let token: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let mut parts = token.split('.');
    let maj = parts.next()?.parse().ok()?;
    let min = parts.next()?.parse().ok()?;
    let pat = parts.next()?.parse().ok()?;
    Some((maj, min, pat))
}

/// Version-skew verdict between the two sides.
#[derive(Debug, PartialEq)]
enum Skew {
    None,
    Patch,
    /// Different minor (or major): the comparison is meaningless; refuse.
    Minor,
}

fn version_skew(upstream: (u32, u32, u32), ephpm: (u32, u32, u32)) -> Skew {
    if upstream.0 != ephpm.0 || upstream.1 != ephpm.1 {
        Skew::Minor
    } else if upstream.2 != ephpm.2 {
        Skew::Patch
    } else {
        Skew::None
    }
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn run_conformance(opts: &Options, ephpm_bin: &Path) -> Result<bool, String> {
    let php_bin = &opts.php;
    if !php_bin.exists() {
        return Err(format!("upstream php not found at {}", php_bin.display()));
    }

    // Identity assertion: `<ephpm> php -n -v` must produce a PHP banner. A
    // plain php binary mistakenly passed as --ephpm treats `php` as a script
    // filename and fails here. Never skip or weaken this check.
    let scratch = mk_scratch_root()?;
    let ephpm_v = run_side(ephpm_bin, &["php".into(), "-n".into(), "-v".into()], &[], &scratch)?;
    let ephpm_banner = String::from_utf8_lossy(&ephpm_v.stdout);
    let ephpm_first = ephpm_banner.lines().next().unwrap_or("");
    let Some(ephpm_ver) =
        (ephpm_v.exit == ExitKind::Code(0)).then(|| parse_php_banner(ephpm_first)).flatten()
    else {
        return Err(format!(
            "identity check failed: `{} php -n -v` did not print a PHP banner \
             (exit {}, first line: {ephpm_first:?}). Is --ephpm really an ephpm \
             binary? (This guard exists because WSL bridge quoting has \
             previously made harnesses measure the system php by accident.)",
            ephpm_bin.display(),
            ephpm_v.exit,
        ));
    };

    let php_v = run_side(php_bin, &["-n".into(), "-v".into()], &[], &scratch)?;
    let php_banner = String::from_utf8_lossy(&php_v.stdout);
    let php_first = php_banner.lines().next().unwrap_or("");
    let Some(php_ver) =
        (php_v.exit == ExitKind::Code(0)).then(|| parse_php_banner(php_first)).flatten()
    else {
        return Err(format!(
            "`{} -n -v` did not print a PHP banner (exit {}, first line: {php_first:?})",
            php_bin.display(),
            php_v.exit,
        ));
    };

    let mut report = String::new();
    let _ = writeln!(report, "ephpm CLI conformance report");
    let _ = writeln!(report, "============================");
    let _ = writeln!(report, "upstream php : {} — {}", php_bin.display(), php_first);
    let _ = writeln!(report, "ephpm php    : {} — {}", ephpm_bin.display(), ephpm_first);

    match version_skew(php_ver, ephpm_ver) {
        Skew::Minor => {
            return Err(format!(
                "PHP minor version mismatch: upstream {}.{}.{} vs ephpm {}.{}.{} — \
                 comparing across minors is meaningless; use a matching upstream php",
                php_ver.0, php_ver.1, php_ver.2, ephpm_ver.0, ephpm_ver.1, ephpm_ver.2,
            ));
        }
        Skew::Patch => {
            let warning = format!(
                "PATCH VERSION SKEW: upstream php {}.{}.{} vs ephpm (SDK) {}.{}.{}. \
                 Behavioral differences may be upstream patch changes, not ephpm drift. \
                 Verify against the pinned patch before filing bugs.",
                php_ver.0, php_ver.1, php_ver.2, ephpm_ver.0, ephpm_ver.1, ephpm_ver.2,
            );
            let _ = writeln!(report, "\n!!! WARNING !!! {warning}");
            eprintln!("\n!!! WARNING !!! {warning}\n");
            if env::var_os("GITHUB_ACTIONS").is_some() {
                println!("::warning title=cli-conformance patch skew::{warning}");
            }
        }
        Skew::None => {
            let _ = writeln!(report, "versions match exactly.");
        }
    }
    let _ = writeln!(report);

    let cases = discover_cases(&opts.corpus, opts.filter.as_deref())?;
    if cases.is_empty() {
        return Err(format!(
            "no cases found in {} (filter: {:?})",
            opts.corpus.display(),
            opts.filter
        ));
    }

    let upstream_side = Side { label: "upstream php", bin: php_bin, prefix: &[] };
    let ephpm_side = Side { label: "ephpm php", bin: ephpm_bin, prefix: &["php"] };

    let mut rows: Vec<(String, Status, String)> = Vec::new();
    let mut details = String::new();
    let mut counts = [0usize; 5];

    for case in &cases {
        if let Some(reason) = &case.meta.skip {
            counts[2] += 1;
            rows.push((case.stem.clone(), Status::Skip, reason.clone()));
            continue;
        }

        let case_root = scratch.join(&case.stem);
        let up_dir = case_root.join("upstream");
        let ep_dir = case_root.join("ephpm");
        fs::create_dir_all(&up_dir).map_err(|e| e.to_string())?;
        fs::create_dir_all(&ep_dir).map_err(|e| e.to_string())?;

        let up_argv = build_argv(upstream_side.prefix, case, &up_dir);
        let ep_argv = build_argv(ephpm_side.prefix, case, &ep_dir);
        let up_cap = run_side(upstream_side.bin, &up_argv, &case.stdin, &up_dir)?;
        let ep_cap = run_side(ephpm_side.bin, &ep_argv, &case.stdin, &ep_dir)?;

        let up_ctx = NormCtx {
            binary: &upstream_side.bin.to_string_lossy(),
            tmpdir: &up_dir.to_string_lossy(),
        };
        let ep_ctx = NormCtx {
            binary: &ephpm_side.bin.to_string_lossy(),
            tmpdir: &ep_dir.to_string_lossy(),
        };
        let (mismatch, up_norm, ep_norm) =
            compare(&up_cap, &ep_cap, &case.meta.normalize, &up_ctx, &ep_ctx);

        let status = classify(&case.meta, mismatch.any());
        let note = match &status {
            Status::Xfail | Status::Xpass => case.meta.xfail.clone().unwrap_or_default(),
            Status::Fail => {
                let mut chans = Vec::new();
                if mismatch.stdout {
                    chans.push("stdout");
                }
                if mismatch.stderr {
                    chans.push("stderr");
                }
                if mismatch.exit {
                    chans.push("exit");
                }
                chans.join("+")
            }
            _ => String::new(),
        };

        if matches!(status, Status::Fail | Status::Xpass) || (matches!(status, Status::Xfail)) {
            let _ = writeln!(details, "### {} — {}", case.stem, status.label());
            if !note.is_empty() {
                let _ = writeln!(details, "    marker/channels: {note}");
            }
            let _ = writeln!(details, "    argv: upstream={:?}  ephpm={:?}", up_argv, ep_argv);
            if mismatch.exit {
                let _ = writeln!(
                    details,
                    "    exit code: upstream={} ephpm={}",
                    up_norm.exit, ep_norm.exit
                );
            }
            if mismatch.stdout {
                let _ = writeln!(details, "  --- {} (stdout)", upstream_side.label);
                let _ = writeln!(details, "  +++ {} (stdout)", ephpm_side.label);
                let _ =
                    write!(details, "{}", indent(&unified_diff(&up_norm.stdout, &ep_norm.stdout)));
            }
            if mismatch.stderr {
                let _ = writeln!(details, "  --- {} (stderr)", upstream_side.label);
                let _ = writeln!(details, "  +++ {} (stderr)", ephpm_side.label);
                let _ =
                    write!(details, "{}", indent(&unified_diff(&up_norm.stderr, &ep_norm.stderr)));
            }
            let _ = writeln!(details);
        }

        let idx = match status {
            Status::Pass => 0,
            Status::Fail => 1,
            Status::Skip => 2,
            Status::Xfail => 3,
            Status::Xpass => 4,
        };
        counts[idx] += 1;
        rows.push((case.stem.clone(), status, note));
    }

    // Summary table.
    let width = rows.iter().map(|(s, _, _)| s.len()).max().unwrap_or(10);
    let _ = writeln!(report, "{:width$}  {:6}  note", "case", "status");
    let _ = writeln!(report, "{}  {}  {}", "-".repeat(width), "-".repeat(6), "-".repeat(4));
    for (stem, status, note) in &rows {
        let _ = writeln!(report, "{stem:width$}  {:6}  {note}", status.label());
    }
    let _ = writeln!(
        report,
        "\ntotal {}: {} pass, {} fail, {} skip, {} xfail, {} xpass (stale xfail)",
        rows.len(),
        counts[0],
        counts[1],
        counts[2],
        counts[3],
        counts[4]
    );

    if !details.is_empty() {
        let _ = writeln!(report, "\nDetails (failures, stale xfails, and expected failures)");
        let _ = writeln!(report, "--------------------------------------------------------");
        let _ = write!(report, "{details}");
    }

    let ok = rows.iter().all(|(_, status, _)| !status.fails_run());
    if !ok {
        let _ = writeln!(
            report,
            "\nRESULT: FAIL — {} unexplained mismatch(es), {} stale xfail marker(s)",
            counts[1], counts[4]
        );
    } else {
        let _ = writeln!(report, "\nRESULT: OK");
    }

    if let Some(path) = &opts.report {
        fs::write(path, &report).map_err(|e| format!("write report {}: {e}", path.display()))?;
        eprintln!("report written to {}", path.display());
    }
    // Always print the report to stdout as well — CI logs are the first
    // place anyone looks.
    println!("{report}");

    let _ = fs::remove_dir_all(&scratch);
    Ok(ok)
}

fn indent(s: &str) -> String {
    s.lines().map(|l| format!("    {l}\n")).collect()
}

/// Create the run's scratch root under the system temp dir.
fn mk_scratch_root() -> Result<PathBuf, String> {
    let dir = env::temp_dir().join(format!("ephpm-cli-conformance-{}", std::process::id()));
    fs::create_dir_all(&dir).map_err(|e| format!("create scratch dir: {e}"))?;
    Ok(dir)
}

// ---------------------------------------------------------------------------
// Unit tests — pure logic only, no PHP binary required (stub-mode safe).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_parses_all_keys() {
        let meta = parse_meta(
            "# comment\n\
             skip = \"legit difference\"\n\
             normalize = [\"strip-versions\", \"strip-zts-marker\"]\n\
             no_default_ini = true\n",
        )
        .unwrap();
        assert_eq!(meta.skip.as_deref(), Some("legit difference"));
        assert_eq!(meta.normalize, vec!["strip-versions", "strip-zts-marker"]);
        assert!(meta.no_default_ini);
        assert!(meta.xfail.is_none());
    }

    #[test]
    fn meta_rejects_unknown_key_and_normalizer() {
        assert!(parse_meta("skipp = \"typo\"").is_err());
        assert!(parse_meta("normalize = [\"no-such-normalizer\"]").is_err());
        assert!(parse_meta("xfail = \"a\"\nskip = \"b\"").is_err());
        assert!(parse_meta("xfail = \"\"").is_err());
        assert!(parse_meta("skip = unquoted").is_err());
    }

    #[test]
    fn args_file_parsing() {
        let args = parse_args_file("-r\nvar_dump($argv);\n# comment\n\n--\nalpha\r\n");
        assert_eq!(args, vec!["-r", "var_dump($argv);", "--", "alpha"]);
    }

    #[test]
    fn arg_placeholder_expansion() {
        let script = PathBuf::from("/corpus/001-x.php");
        let tmp = PathBuf::from("/scratch/one");
        assert_eq!(expand_arg("{SCRIPT}", Some(&script), &tmp), "/corpus/001-x.php");
        assert_eq!(expand_arg("{TMPDIR}/out.txt", Some(&script), &tmp), "/scratch/one/out.txt");
        assert_eq!(expand_arg("-r", Some(&script), &tmp), "-r");
    }

    #[test]
    fn default_ini_injection() {
        let case = Case {
            stem: "x".into(),
            script: None,
            args: vec!["-r".into(), "echo 1;".into()],
            stdin: Vec::new(),
            meta: Meta::default(),
        };
        assert_eq!(
            build_argv(&["php"], &case, Path::new("/t")),
            vec!["php", "-n", "-r", "echo 1;"]
        );
        let mut no_ini = case;
        no_ini.meta.no_default_ini = true;
        assert_eq!(build_argv(&[], &no_ini, Path::new("/t")), vec!["-r", "echo 1;"]);
    }

    #[test]
    fn strip_versions_normalizer() {
        let input =
            b"PHP 8.5.4-1+ubuntu24.04.1+deb.sury.org+1 (cli) (built: Jul 16 2026 18:56:38) (NTS)";
        let out = strip_versions(input);
        assert_eq!(String::from_utf8(out).unwrap(), "PHP <VERSION> (cli) (built: <DATE>) (NTS)");
        // Zend engine line too.
        let out2 = strip_versions(b"Zend Engine v4.5.7, Copyright (c) Zend Technologies");
        assert_eq!(
            String::from_utf8(out2).unwrap(),
            "Zend Engine v<VERSION>, Copyright (c) Zend Technologies"
        );
        // Two-part numbers are NOT versions.
        assert_eq!(strip_versions(b"value 1.5 stays"), b"value 1.5 stays");
    }

    #[test]
    fn zts_and_binary_normalizers() {
        let ctx = NormCtx { binary: "/usr/bin/php8.5", tmpdir: "/tmp/x" };
        let out = normalize(
            b"PHP (ZTS) at /usr/bin/php8.5 in /tmp/x/file",
            &["strip-zts-marker".to_string(), "strip-binary-path".to_string()],
            &ctx,
        );
        assert_eq!(String::from_utf8(out).unwrap(), "PHP (TS) at <BINARY> in <TMPDIR>/file");
    }

    #[test]
    fn opcache_banner_normalizer() {
        let input =
            b"PHP 8.5.4 (cli)\n    with Zend OPcache v8.5.4, by Zend Technologies\nZend Engine\n";
        let out = strip_opcache_banner(input);
        assert_eq!(String::from_utf8(out).unwrap(), "PHP 8.5.4 (cli)\nZend Engine\n");
    }

    #[test]
    fn compare_detects_channel_mismatches() {
        let ctx_a = NormCtx { binary: "a", tmpdir: "/ta" };
        let ctx_b = NormCtx { binary: "b", tmpdir: "/tb" };
        let up =
            Capture { stdout: b"same".to_vec(), stderr: b"warn".to_vec(), exit: ExitKind::Code(0) };
        let ep =
            Capture { stdout: b"same".to_vec(), stderr: b"WARN".to_vec(), exit: ExitKind::Code(1) };
        let (m, _, _) = compare(&up, &ep, &[], &ctx_a, &ctx_b);
        assert!(!m.stdout);
        assert!(m.stderr);
        assert!(m.exit);
        assert!(m.any());
    }

    #[test]
    fn tmpdir_always_normalized() {
        let ctx_a = NormCtx { binary: "a", tmpdir: "/scratch/up" };
        let ctx_b = NormCtx { binary: "b", tmpdir: "/scratch/ep" };
        let up = Capture {
            stdout: b"cwd=/scratch/up".to_vec(),
            stderr: vec![],
            exit: ExitKind::Code(0),
        };
        let ep = Capture {
            stdout: b"cwd=/scratch/ep".to_vec(),
            stderr: vec![],
            exit: ExitKind::Code(0),
        };
        let (m, _, _) = compare(&up, &ep, &[], &ctx_a, &ctx_b);
        assert!(!m.any(), "harness-provided tmpdir paths must never cause a mismatch");
    }

    #[test]
    fn classification_matrix() {
        let plain = Meta::default();
        let xfail = Meta { xfail: Some("(#331)".into()), ..Meta::default() };
        let skip = Meta { skip: Some("legit".into()), ..Meta::default() };
        assert_eq!(classify(&plain, false), Status::Pass);
        assert_eq!(classify(&plain, true), Status::Fail);
        assert_eq!(classify(&xfail, true), Status::Xfail);
        assert_eq!(classify(&xfail, false), Status::Xpass);
        assert_eq!(classify(&skip, true), Status::Skip);
        assert!(Status::Fail.fails_run());
        assert!(Status::Xpass.fails_run());
        assert!(!Status::Xfail.fails_run());
        assert!(!Status::Pass.fails_run());
        assert!(!Status::Skip.fails_run());
    }

    #[test]
    fn banner_parse_and_skew() {
        assert_eq!(parse_php_banner("PHP 8.5.7 (cli) (built: x) (ZTS)"), Some((8, 5, 7)));
        assert_eq!(
            parse_php_banner("PHP 8.5.4-1+ubuntu24.04.1+deb.sury.org+1 (cli)"),
            Some((8, 5, 4))
        );
        assert_eq!(parse_php_banner("Zend Engine v4"), None);
        assert_eq!(version_skew((8, 5, 7), (8, 5, 7)), Skew::None);
        assert_eq!(version_skew((8, 5, 4), (8, 5, 7)), Skew::Patch);
        assert_eq!(version_skew((8, 4, 23), (8, 5, 7)), Skew::Minor);
    }

    #[test]
    fn diff_shape() {
        let d = unified_diff(b"a\nb\nc\n", b"a\nX\nc\n");
        assert_eq!(d, " a\n-b\n+X\n c\n");
        let d2 = unified_diff(b"", b"new\n");
        assert_eq!(d2, "+new\n");
    }

    #[test]
    fn exit_kind_comparison() {
        assert_eq!(ExitKind::Code(255), ExitKind::Code(255));
        assert_ne!(ExitKind::Code(0), ExitKind::Abnormal("killed".into()));
        assert_ne!(ExitKind::Abnormal("a".into()), ExitKind::Abnormal("b".into()));
    }
}
