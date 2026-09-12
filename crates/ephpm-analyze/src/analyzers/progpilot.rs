//! `progpilot` — taint-analysis PHP security scan.
//!
//! Wraps the [progpilot](https://github.com/designsecurity/progpilot) CLI as a
//! subprocess (progpilot is an **optional external dependency** — absence
//! skips this analyzer). progpilot performs data-flow *taint tracking* from
//! request sources to dangerous sinks, so it finds injection-class bugs
//! (SQL injection, XSS, file inclusion, command injection, …) that the naive
//! token pass in `dangerous-sinks` cannot. Every finding is a
//! [`Category::Security`] hit reported at [`Severity::High`] with
//! [`Confidence::Confirmed`].
//!
//! **CLI shape (verified against progpilot master, Sept 2026).** The current
//! CLI takes only positional files/folders and an optional
//! `--configuration <file>`; there is *no* `--output-format` / `--json` /
//! `--output` flag (the man-page/blog claims of one are outdated). Machine
//! output is progpilot's **default**: a JSON array printed to stdout
//! (SARIF is reachable only by enabling it inside a `--configuration` file, so
//! this analyzer requires the default JSON output and parses that). progpilot
//! exits `1` when it *finds* vulnerabilities and `0` when it finds none — like
//! `composer audit`, a non-zero exit is not a failure signal, the JSON body
//! is; an unparseable body (e.g. an exception message, which progpilot prints
//! and then exits `0`) is a real failure and gates.
//!
//! **Output objects** (see progpilot's `docs/OUTPUT.md`): each element carries
//! `vuln_type` = `"taint-style"` or `"custom"`. A taint-style finding is
//! anchored at its **sink** (`sink_file` / `sink_line`); a custom finding at
//! `vuln_file` / `vuln_line`. The vulnerability class is `vuln_name`
//! (`sql_injection`, `xss`, `file_inclusion`, `command_injection`, …) and
//! becomes the rule-id suffix: `progpilot/<vuln_name>`.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::tool::{run_tool, stderr_excerpt};
use crate::analyzer::{AnalysisCtx, Analyzer, AnalyzerError};
use crate::finding::{Category, Confidence, Finding, Severity};

/// See the module docs.
pub struct Progpilot;

/// The analyzer's stable id.
pub const ID: &str = "progpilot";

/// Resolve which progpilot program to run: prefer a Composer-local
/// `vendor/bin/progpilot` under the target (the usual install), else the bare
/// `progpilot` name so [`run_tool`] searches `PATH` (and, on Windows, the
/// `.bat`/`.cmd` launcher forms). Absence of both surfaces as
/// [`AnalyzerError::Skipped`] from `run_tool`.
fn progpilot_program(root: &Path) -> String {
    let vendor = root.join("vendor").join("bin");
    // Composer installs a `.bat` shim on Windows; the extension-less script
    // (with a `php` shebang) is the Unix form.
    let candidates: &[&str] =
        if cfg!(windows) { &["progpilot.bat", "progpilot"] } else { &["progpilot"] };
    for name in candidates {
        let candidate = vendor.join(name);
        if candidate.is_file() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    "progpilot".to_owned()
}

/// Lower-case a `vuln_name` and turn internal whitespace into `_` so it is a
/// stable rule-id suffix (progpilot uses a few space-separated names such as
/// `mail command_injection`; taint names like `sql_injection` are unchanged).
fn slug(vuln_name: &str) -> String {
    vuln_name.split_whitespace().collect::<Vec<_>>().join("_").to_lowercase()
}

/// Render `path` relative to `root` when it sits inside it (progpilot emits
/// absolute realpaths); otherwise keep it as reported.
fn rel_path(path: &str, root: &Path) -> PathBuf {
    let p = Path::new(path);
    p.strip_prefix(root).unwrap_or(p).to_path_buf()
}

/// Build one finding from a single progpilot result object.
fn finding_from(result: &Value, root: &Path) -> Option<Finding> {
    let vuln_name = result.get("vuln_name").and_then(Value::as_str)?;
    let cwe = result.get("vuln_cwe").and_then(Value::as_str).unwrap_or("unknown-cwe");
    let is_custom = result.get("vuln_type").and_then(Value::as_str) == Some("custom");

    // A custom finding is anchored at `vuln_file`/`vuln_line`; a taint-style
    // finding at its sink (`sink_file`/`sink_line`).
    let (file_key, line_key) =
        if is_custom { ("vuln_file", "vuln_line") } else { ("sink_file", "sink_line") };
    let path = result.get(file_key).and_then(Value::as_str).map(|p| rel_path(p, root));
    let line = result.get(line_key).and_then(Value::as_u64);

    let message = if is_custom {
        let rule = result.get("vuln_rule").and_then(Value::as_str).unwrap_or(vuln_name);
        format!("progpilot custom rule {rule}: {vuln_name} ({cwe})")
    } else {
        let sink = result.get("sink_name").and_then(Value::as_str).unwrap_or("sink");
        // `source_name` is an array (many sources may reach one sink).
        let source = result
            .pointer("/source_name/0")
            .and_then(Value::as_str)
            .map_or_else(String::new, |s| format!(" from {s}"));
        format!("{vuln_name} ({cwe}): tainted data{source} reaches {sink}()")
    };

    Some(Finding {
        rule_id: format!("{ID}/{}", slug(vuln_name)),
        severity: Severity::High,
        category: Category::Security,
        path,
        line,
        message,
        // A taint-tracked flow from a request source to a dangerous sink is
        // evidence of a real vulnerability, not a pattern hotspot.
        confidence: Confidence::Confirmed,
    })
}

/// Parse progpilot's default JSON-array output into findings.
fn parse_progpilot(stdout: &str, root: &Path) -> Result<Vec<Finding>, AnalyzerError> {
    let doc: Value = serde_json::from_str(stdout.trim())
        .map_err(|e| AnalyzerError::Parse(format!("progpilot output is not JSON: {e}")))?;
    let results = doc.as_array().ok_or_else(|| {
        // A JSON object here means a `--configuration` enabled SARIF output,
        // which this analyzer does not consume — say so rather than silently
        // finding nothing.
        AnalyzerError::Parse(
            "progpilot output is not a JSON array (a --configuration enabling SARIF output is \
             not supported; use the default JSON output)"
                .to_owned(),
        )
    })?;
    Ok(results.iter().filter_map(|r| finding_from(r, root)).collect())
}

impl Analyzer for Progpilot {
    fn id(&self) -> &str {
        ID
    }

    fn category(&self) -> Category {
        Category::Security
    }

    fn run(&self, ctx: &AnalysisCtx) -> Result<Vec<Finding>, AnalyzerError> {
        let root = ctx.root();
        let timeout = ctx.config().analyzers.tool_timeout_ms;

        let mut args: Vec<String> = Vec::new();
        if let Some(cfg) = ctx.config().analyzers.progpilot_config.as_ref() {
            let cfg_path = if cfg.is_absolute() { cfg.clone() } else { root.join(cfg) };
            if !cfg_path.is_file() {
                // A configured-but-missing file is an operator error on a path
                // that should have worked — fail closed, don't skip.
                return Err(AnalyzerError::ToolFailed(format!(
                    "configured progpilot_config {} does not exist",
                    cfg_path.display()
                )));
            }
            args.push("--configuration".to_owned());
            args.push(cfg_path.to_string_lossy().into_owned());
        }
        // Analyze the whole target; cwd is the (canonicalized) root.
        args.push(".".to_owned());

        let program = progpilot_program(root);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let run = run_tool(&program, &arg_refs, root, timeout)?;

        // progpilot exits 1 when it finds vulnerabilities and 0 when it does
        // not, so the exit code is not a failure signal — the JSON body is.
        match parse_progpilot(&run.stdout, root) {
            Ok(findings) => Ok(findings),
            Err(AnalyzerError::Parse(_)) if run.stdout.trim().is_empty() => {
                // No JSON at all: the tool broke before emitting results.
                Err(AnalyzerError::ToolFailed(format!(
                    "progpilot produced no output (exit {:?}): {}",
                    run.exit_code,
                    stderr_excerpt(&run.stderr)
                )))
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_taint_style_sqli_from_sink() {
        // A captured progpilot JSON array (default output) with one taint-style
        // SQL-injection finding — anchored at its sink.
        let stdout = r#"[
          {
            "source_name": ["$_GET[\"id\"]"],
            "source_line": [7],
            "source_column": [12],
            "source_file": ["/srv/app/login.php"],
            "tainted_flow": [[]],
            "sink_name": "mysqli_query",
            "sink_line": 21,
            "sink_column": 5,
            "sink_file": "/srv/app/login.php",
            "vuln_name": "sql_injection",
            "vuln_cwe": "CWE_89",
            "vuln_id": "abc123",
            "vuln_type": "taint-style"
          }
        ]"#;
        let findings = parse_progpilot(stdout, Path::new("/srv/app")).unwrap();
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.rule_id, "progpilot/sql_injection");
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.category, Category::Security);
        assert_eq!(f.confidence, Confidence::Confirmed);
        // Anchored at the sink, tree-relative to the analyzed root.
        assert_eq!(f.path.as_deref(), Some(Path::new("login.php")));
        assert_eq!(f.line, Some(21));
        assert!(f.message.contains("mysqli_query"), "message: {}", f.message);
        assert!(f.message.contains("CWE_89"), "message: {}", f.message);
    }

    #[test]
    fn parses_taint_and_custom_and_normalizes_names() {
        let stdout = r#"[
          {"sink_name": "system", "sink_line": 3, "sink_file": "/r/x.php",
           "source_name": ["$_POST[\"c\"]"],
           "vuln_name": "mail command_injection", "vuln_cwe": "CWE_78",
           "vuln_type": "taint-style"},
          {"vuln_rule": "MUST_NOT_VERIFY_DEFINITION", "vuln_name": "security misconfiguration",
           "vuln_line": 9, "vuln_file": "/r/conf.php", "vuln_cwe": "CWE_1004",
           "vuln_type": "custom"}
        ]"#;
        let findings = parse_progpilot(stdout, Path::new("/r")).unwrap();
        assert_eq!(findings.len(), 2);
        // Spaces in a vuln_name are normalized to `_` in the rule id.
        assert_eq!(findings[0].rule_id, "progpilot/mail_command_injection");
        assert_eq!(findings[0].path.as_deref(), Some(Path::new("x.php")));
        assert_eq!(findings[0].line, Some(3));
        // Custom findings anchor at vuln_file/vuln_line and name their rule.
        assert_eq!(findings[1].rule_id, "progpilot/security_misconfiguration");
        assert_eq!(findings[1].path.as_deref(), Some(Path::new("conf.php")));
        assert_eq!(findings[1].line, Some(9));
        assert!(findings[1].message.contains("MUST_NOT_VERIFY_DEFINITION"));
    }

    #[test]
    fn empty_array_means_no_findings() {
        assert!(parse_progpilot("[]", Path::new("/r")).unwrap().is_empty());
    }

    #[test]
    fn sarif_object_output_is_rejected_not_silently_empty() {
        // A configuration that switched progpilot to SARIF emits a JSON object,
        // which this analyzer must reject rather than treat as zero findings.
        let err = parse_progpilot(r#"{"runs": []}"#, Path::new("/r")).unwrap_err();
        assert!(matches!(err, AnalyzerError::Parse(_)));
    }

    #[test]
    fn non_json_output_is_a_parse_error() {
        let err = parse_progpilot("Fatal error: something broke", Path::new("/r")).unwrap_err();
        assert!(matches!(err, AnalyzerError::Parse(_)));
    }
}
