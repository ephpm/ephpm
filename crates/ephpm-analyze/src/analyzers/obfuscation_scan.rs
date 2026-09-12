//! `obfuscation-scan` — native heuristic for obfuscated / backdoor PHP.
//!
//! Signature scanners (`clamav`, `malware-yara`) catch *known* webshells. They
//! are blind to a freshly generated or hand-obfuscated one — the exact threat a
//! weaponized LLM makes cheap. This analyzer catches the *shape* instead of the
//! signature: the "code from data" pattern (an execution sink fed the output of
//! a decoder), execution of request-controlled input, the deprecated
//! `preg_replace` `/e` modifier, variable-driven (dynamic) calls, and
//! high-entropy encoded blob literals. None of these is proof of malice on its
//! own — each is the *shape* obfuscated backdoors take.
//!
//! This is a **native, source-text token pass** over `*.php` / `*.phtml`,
//! sharing the per-file walker in `php_files` with `dangerous-sinks` and
//! `suppression-scan` (diff-aware scope skipping and the incremental cache
//! included) — no external tool, no subprocess, and no PHP-linked build. It is
//! **opt-in**: registered so `analyzers.enable` accepts it, but absent from the
//! default `security` profile.
//!
//! Because it reasons over source text, it cannot see through comments and
//! string literals: a decoder name inside a comment, or a `preg_replace`-shaped
//! string, is a false positive. That is exactly why every finding is
//! `Suspected` (a review hotspot, never a hard deny) — the same trade
//! `dangerous-sinks` documents. An **opcode-backed `Confirmed` tier** —
//! detecting eval-of-decoded / eval-of-tainted-argument at the opcode level,
//! building on the embedded-compiler path the `opcode-scan` analyzer already
//! ships — is a **planned follow-up**. The two would then coexist exactly as
//! `dangerous-sinks` (token) and `opcode-scan` (engine) do today: this pass
//! needs no PHP-linked build, the confirmed tier would.
//!
//! Rule ids emitted (`obfuscation-scan/<kind>`):
//! - `eval-of-decode` (high) — an `eval` / `assert` / `create_function` call
//!   whose argument text contains a decoder (base64/gzinflate/gzuncompress/
//!   rot13/hex2bin/pack/uudecode) or `\xNN` escapes — the decode-and-execute
//!   webshell.
//! - `eval-of-request` (high) — an `eval` / `assert` / `system` / `exec` /
//!   `shell_exec` / `passthru` / `popen` / `proc_open` call whose argument
//!   references a request superglobal or `php://input`.
//! - `preg-replace-eval` (high) — `preg_replace` with the `/e` modifier, which
//!   executes the replacement as PHP code.
//! - `dynamic-call` (medium) — a variable function call (`$f(...)`), a
//!   variable-variable (`$$x`, or `${...}` used as a callee), or
//!   `call_user_func` / `call_user_func_array` fed a variable. Heuristic;
//!   plain `"${var}"` string interpolation is deliberately **not** flagged (it
//!   is not a call), only a `${...}(` callee is.
//! - `long-encoded-blob` (medium) — a run of >= 200 base64/hex characters, a
//!   strong obfuscation indicator on its own.
//!
//! NOTE for maintainers: keep the sink/decoder names below and any test
//! fixtures free of realistic webshell-shaped byte sequences (a sink call
//! wrapping a decoder or a request superglobal) — endpoint antivirus
//! quarantines source files containing those sequences, which broke the build
//! once already (see `dangerous_sinks`). The tests assemble such sequences at
//! runtime so they only ever live in memory, never on disk.

use std::path::Path;

use super::php_files::scan_php_tree;
use crate::analyzer::{AnalysisCtx, Analyzer, AnalyzerError};
use crate::finding::{Category, Confidence, Finding, Severity};

/// See the module docs.
pub struct ObfuscationScan;

/// The analyzer's stable id.
pub const ID: &str = "obfuscation-scan";

/// Execution sinks whose argument, if it decodes, is the code-from-data
/// pattern (`eval-of-decode`).
const DECODE_SINKS: &[&str] = &["eval", "assert", "create_function"];

/// Decoder functions that, wrapped inside an execution sink, complete a
/// decode-and-execute chain.
const DECODERS: &[&str] = &[
    "base64_decode",
    "gzinflate",
    "gzuncompress",
    "gzdecode",
    "str_rot13",
    "hex2bin",
    "convert_uudecode",
    "pack",
];

/// Execution sinks whose argument, if it references request input, runs
/// attacker-controlled data (`eval-of-request`).
const REQUEST_SINKS: &[&str] =
    &["eval", "assert", "system", "exec", "shell_exec", "passthru", "popen", "proc_open"];

/// Request-controlled input sources.
const SUPERGLOBALS: &[&str] =
    &["$_GET", "$_POST", "$_REQUEST", "$_COOKIE", "$_SERVER", "$_FILES", "php://input"];

/// Callback functions that, fed a variable, are a dynamic call.
const CALLBACK_FNS: &[&str] = &["call_user_func", "call_user_func_array"];

/// Minimum length of an encoded run to flag as a `long-encoded-blob`.
const MIN_BLOB_LEN: usize = 200;

/// `true` when the byte before a candidate token rules it out as a direct
/// function call: part of a longer identifier, a `$`-variable, or a method /
/// static access (`->` / `::`). Mirrors `dangerous_sinks`.
fn preceded_by_identifier_byte(line: &str, start: usize) -> bool {
    line[..start]
        .bytes()
        .next_back()
        .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$' | b'>' | b':'))
}

/// The byte index just past the `(` of a call whose token ends at `after`
/// (modulo spaces/tabs), or `None` when the next non-space byte is not `(`.
fn call_paren_end(line: &str, after: usize) -> Option<usize> {
    for (i, b) in line[after..].bytes().enumerate() {
        match b {
            b' ' | b'\t' => {}
            b'(' => return Some(after + i + 1),
            _ => return None,
        }
    }
    None
}

/// Byte indices just past the `(` of every direct call to `name` on `line`.
fn call_arg_starts(line: &str, name: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut search_from = 0;
    while let Some(pos) = line[search_from..].find(name) {
        let start = search_from + pos;
        let after = start + name.len();
        search_from = after;
        if preceded_by_identifier_byte(line, start) {
            continue;
        }
        if let Some(arg_start) = call_paren_end(line, after) {
            starts.push(arg_start);
        }
    }
    starts
}

/// `true` when the argument region contains a decoder call or a `\xNN` escape.
fn region_decodes(region: &str) -> bool {
    DECODERS.iter().any(|d| region.contains(d)) || has_hex_escape(region)
}

/// `true` when the region contains a `\x`-style hex escape (`\x41`).
fn has_hex_escape(region: &str) -> bool {
    let bytes = region.as_bytes();
    bytes
        .windows(3)
        .any(|w| w[0] == b'\\' && (w[1] == b'x' || w[1] == b'X') && w[2].is_ascii_hexdigit())
}

/// The first request source the region references, if any.
fn region_request_source(region: &str) -> Option<&'static str> {
    SUPERGLOBALS.iter().copied().find(|sg| region.contains(sg))
}

/// `true` when a `preg_replace` call on the line uses the `/e` modifier.
fn has_preg_replace_e(line: &str) -> bool {
    call_arg_starts(line, "preg_replace")
        .iter()
        .any(|&arg_start| first_string_has_e_modifier(&line[arg_start..]))
}

/// Parse the first quoted string in `region` (a `preg_replace` argument list)
/// and report whether its delimiter's trailing modifiers include `e`.
fn first_string_has_e_modifier(region: &str) -> bool {
    let bytes = region.as_bytes();
    let Some(q_pos) = bytes.iter().position(|&b| b == b'\'' || b == b'"') else {
        return false;
    };
    let quote = bytes[q_pos];
    let content_start = q_pos + 1;
    let mut i = content_start;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b if b == quote => break,
            _ => i += 1,
        }
    }
    if i >= bytes.len() {
        return false; // unterminated string literal
    }
    pattern_modifiers_have_e(&region[content_start..i])
}

/// Given a PCRE pattern string (`/.../e`, `#...#e`, `(...)ei`, ...), report
/// whether the modifiers after the closing delimiter include `e`.
fn pattern_modifiers_have_e(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let Some(&open) = bytes.first() else {
        return false;
    };
    let close = match open {
        b'(' => b')',
        b'{' => b'}',
        b'[' => b']',
        b'<' => b'>',
        // A delimiter must not be alphanumeric, backslash, or whitespace.
        c if c.is_ascii_alphanumeric() || c == b'\\' || c.is_ascii_whitespace() => return false,
        c => c,
    };
    let Some(close_pos) = pattern.rfind(close as char) else {
        return false;
    };
    if close_pos == 0 {
        return false; // open and close would be the same byte
    }
    pattern[close_pos + 1..].bytes().any(|b| b == b'e')
}

/// The kind of dynamic call on the line, if any (for the finding message).
fn dynamic_call_kind(line: &str) -> Option<&'static str> {
    if has_variable_function_call(line) {
        return Some("$var()");
    }
    if line.contains("$$") {
        return Some("variable variable ($$)");
    }
    if line.contains("${") && line.contains("}(") {
        return Some("variable variable (${...})");
    }
    for &name in CALLBACK_FNS {
        if call_arg_starts(line, name).iter().any(|&arg_start| line[arg_start..].contains('$')) {
            return Some("call_user_func");
        }
    }
    None
}

/// `true` when the line contains a `$name(` variable function call — a
/// `$`-prefixed identifier immediately (modulo spaces/tabs) followed by `(`.
/// Method / property accesses (`$obj->m(`) and array-indexed callees
/// (`$a[0](`) do not match, by design.
fn has_variable_function_call(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        if j < bytes.len() && (bytes[j].is_ascii_alphabetic() || bytes[j] == b'_') {
            j += 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            let mut k = j;
            while k < bytes.len() && matches!(bytes[k], b' ' | b'\t') {
                k += 1;
            }
            if k < bytes.len() && bytes[k] == b'(' {
                return true;
            }
        }
        i = j.max(i + 1);
    }
    false
}

/// A short excerpt of the longest base64/hex run of at least [`MIN_BLOB_LEN`]
/// characters on the line, or `None`. Never returns the full blob.
fn long_blob_excerpt(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut best: Option<(usize, usize)> = None;
    let mut run_start = 0;
    let mut in_run = false;
    let consider = |start: usize, end: usize, best: &mut Option<(usize, usize)>| {
        let len = end - start;
        if len >= MIN_BLOB_LEN && best.is_none_or(|(_, best_len)| len > best_len) {
            *best = Some((start, len));
        }
    };
    for (i, &b) in bytes.iter().enumerate() {
        if is_base64ish(b) {
            if !in_run {
                run_start = i;
                in_run = true;
            }
        } else if in_run {
            consider(run_start, i, &mut best);
            in_run = false;
        }
    }
    if in_run {
        consider(run_start, bytes.len(), &mut best);
    }
    // `start` indexes a base64 byte, which is ASCII, so it is a char boundary.
    best.map(|(start, _)| line[start..].chars().take(16).collect())
}

/// `true` for the base64 alphabet (`A-Za-z0-9+/=`), which subsumes hex.
fn is_base64ish(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')
}

/// Build a `Suspected`, `Malware`-category finding at `line_no`.
fn finding(
    rel_path: &Path,
    line_no: u64,
    kind: &str,
    severity: Severity,
    message: String,
) -> Finding {
    Finding {
        rule_id: format!("{ID}/{kind}"),
        severity,
        category: Category::Malware,
        path: Some(rel_path.to_path_buf()),
        line: Some(line_no),
        // A source-text heuristic has known false positives (matches inside
        // comments/strings) — a hotspot for review, never proof.
        confidence: Confidence::Suspected,
        message,
    }
}

/// Scan one file's text, returning findings with 1-based line numbers.
fn scan_text(text: &str, rel_path: &Path) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (line_idx, line) in text.lines().enumerate() {
        let line_no = line_idx as u64 + 1;

        for &sink in DECODE_SINKS {
            for arg_start in call_arg_starts(line, sink) {
                if region_decodes(&line[arg_start..]) {
                    findings.push(finding(
                        rel_path,
                        line_no,
                        "eval-of-decode",
                        Severity::High,
                        format!(
                            "{sink}() argument decodes data before executing it (base64/gzip/rot13/hex) \
                             — decode-and-execute obfuscation"
                        ),
                    ));
                }
            }
        }

        for &sink in REQUEST_SINKS {
            for arg_start in call_arg_starts(line, sink) {
                if let Some(source) = region_request_source(&line[arg_start..]) {
                    findings.push(finding(
                        rel_path,
                        line_no,
                        "eval-of-request",
                        Severity::High,
                        format!(
                            "{sink}() argument references request input ({source}) — executes \
                             attacker-controlled data"
                        ),
                    ));
                }
            }
        }

        if has_preg_replace_e(line) {
            findings.push(finding(
                rel_path,
                line_no,
                "preg-replace-eval",
                Severity::High,
                "preg_replace() with the /e modifier — executes the replacement as PHP code"
                    .to_owned(),
            ));
        }

        if let Some(kind) = dynamic_call_kind(line) {
            findings.push(finding(
                rel_path,
                line_no,
                "dynamic-call",
                Severity::Medium,
                format!("dynamic call via {kind} — variable-driven code execution (heuristic)"),
            ));
        }

        if let Some(excerpt) = long_blob_excerpt(line) {
            findings.push(finding(
                rel_path,
                line_no,
                "long-encoded-blob",
                Severity::Medium,
                format!(
                    "string literal with a >= {MIN_BLOB_LEN}-char base64/hex run ({excerpt}...) \
                     — likely an encoded payload"
                ),
            ));
        }
    }
    findings
}

impl Analyzer for ObfuscationScan {
    fn id(&self) -> &str {
        ID
    }

    fn category(&self) -> Category {
        Category::Malware
    }

    fn run(&self, ctx: &AnalysisCtx) -> Result<Vec<Finding>, AnalyzerError> {
        scan_php_tree(ctx, ID, &scan_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(src: &str) -> Vec<Finding> {
        scan_text(src, Path::new("t.php"))
    }

    /// Assemble a call at runtime so webshell-shaped byte sequences (a sink
    /// wrapping a decoder or a superglobal) never sit literally in this source
    /// file — see the module docs' antivirus note. The assembled strings only
    /// ever live in memory.
    fn call(name: &str, arg: &str) -> String {
        format!("{name}({arg})")
    }

    /// `eval`, split so the identifier never sits in the file as one token.
    fn eval_name() -> String {
        format!("ev{}", "al")
    }

    fn ids(findings: &[Finding]) -> Vec<&str> {
        findings.iter().map(|f| f.rule_id.as_str()).collect()
    }

    #[test]
    fn finds_eval_of_decode() {
        let inner = call("base64_decode", "'cGF5bG9hZA=='");
        let src = format!("<?php\n{};\n", call(&eval_name(), &call("gzinflate", &inner)));
        let findings = scan(&src);
        assert_eq!(ids(&findings), vec!["obfuscation-scan/eval-of-decode"]);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.category, Category::Malware);
        assert_eq!(f.confidence, Confidence::Suspected);
        assert_eq!(f.line, Some(2));
    }

    #[test]
    fn finds_eval_of_decode_via_hex_escape() {
        // `\x65\x76...` decode-style escapes in the argument, no named decoder.
        let src = format!("<?php\n{};\n", call(&eval_name(), r#""\x65\x76\x61\x6c""#));
        let findings = scan(&src);
        assert_eq!(ids(&findings), vec!["obfuscation-scan/eval-of-decode"]);
    }

    #[test]
    fn finds_eval_of_request() {
        let src = format!("<?php\n{};\n", call(&eval_name(), "$_REQUEST['c']"));
        let findings = scan(&src);
        assert_eq!(ids(&findings), vec!["obfuscation-scan/eval-of-request"]);
        assert_eq!(findings[0].severity, Severity::High);
        assert_eq!(findings[0].line, Some(2));
    }

    #[test]
    fn finds_command_exec_of_request() {
        let src = format!("<?php\n{};\n", call("system", "$_GET['x']"));
        let findings = scan(&src);
        assert_eq!(ids(&findings), vec!["obfuscation-scan/eval-of-request"]);
    }

    #[test]
    fn finds_preg_replace_e_modifier() {
        // Assemble the `/e` modifier from a part so the literal never sits on
        // disk as a single token.
        let modifier = format!("/payload/{}", "e");
        let src = format!("<?php\npreg_replace('{modifier}', $r, $subject);\n");
        let findings = scan(&src);
        assert_eq!(ids(&findings), vec!["obfuscation-scan/preg-replace-eval"]);
        assert_eq!(findings[0].severity, Severity::High);
        assert_eq!(findings[0].line, Some(2));
    }

    #[test]
    fn preg_replace_without_e_is_clean() {
        let findings = scan("<?php\npreg_replace('/foo/i', 'bar', $s);\n");
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn finds_variable_function_call() {
        let findings = scan("<?php\n$fn($arg);\n");
        assert_eq!(ids(&findings), vec!["obfuscation-scan/dynamic-call"]);
        assert_eq!(findings[0].severity, Severity::Medium);
        assert_eq!(findings[0].line, Some(2));
    }

    #[test]
    fn finds_variable_variable_and_callback() {
        assert_eq!(ids(&scan("<?php\n$$name = 1;\n")), vec!["obfuscation-scan/dynamic-call"]);
        assert_eq!(
            ids(&scan("<?php\ncall_user_func($cb, $arg);\n")),
            vec!["obfuscation-scan/dynamic-call"]
        );
    }

    #[test]
    fn method_and_string_interpolation_are_not_dynamic_calls() {
        // `$obj->method(...)`, `func($var)`, and `"${var}"` interpolation must
        // not be flagged.
        let findings = scan(
            "<?php\n$obj->handle($x);\nstrlen($name);\n$msg = \"hello ${name} world\";\nif ($ok) {}\n",
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn finds_long_encoded_blob() {
        let blob = "A".repeat(MIN_BLOB_LEN + 40);
        let src = format!("<?php\n$data = '{blob}';\n");
        let findings = scan(&src);
        assert_eq!(ids(&findings), vec!["obfuscation-scan/long-encoded-blob"]);
        assert_eq!(findings[0].severity, Severity::Medium);
        assert_eq!(findings[0].line, Some(2));
        // The message must never echo the full blob.
        assert!(!findings[0].message.contains(&blob));
    }

    #[test]
    fn short_encoded_string_is_clean() {
        let blob = "A".repeat(MIN_BLOB_LEN - 1);
        let findings = scan(&format!("<?php\n$data = '{blob}';\n"));
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn clean_file_yields_nothing() {
        // Decoders and superglobals present, but never inside an execution
        // sink; a normal method call and a plain assignment.
        let findings = scan(
            "<?php\n$raw = base64_decode($encoded);\n$name = $_GET['name'] ?? 'guest';\n\
             echo htmlspecialchars($name);\n$this->render($raw);\n",
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn line_anchoring_is_correct_across_constructs() {
        let src = format!(
            "<?php\n// header\n{};\n$x = 1;\n$fn($y);\n",
            call(&eval_name(), &call("base64_decode", "$d"))
        );
        let findings = scan(&src);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].rule_id, "obfuscation-scan/eval-of-decode");
        assert_eq!(findings[0].line, Some(3));
        assert_eq!(findings[1].rule_id, "obfuscation-scan/dynamic-call");
        assert_eq!(findings[1].line, Some(5));
    }

    #[test]
    fn all_findings_are_suspected_malware() {
        let src =
            format!("<?php\n{};\n", call(&eval_name(), &call("base64_decode", "$_POST['c']")));
        let findings = scan(&src);
        assert!(!findings.is_empty());
        assert!(
            findings
                .iter()
                .all(|f| f.confidence == Confidence::Suspected && f.category == Category::Malware)
        );
    }
}
