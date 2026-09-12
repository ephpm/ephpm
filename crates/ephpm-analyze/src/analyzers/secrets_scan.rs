//! `secrets-scan` — native scan for hardcoded secrets committed in source.
//!
//! A line-oriented, dependency-free pass over `*.php` / `*.phtml` files (the
//! same shared per-file walker `dangerous-sinks` and `suppression-scan` use —
//! diff-aware scope skipping and the incremental cache included) that flags
//! credentials committed into the code. It emits one [`Finding`] per match,
//! anchored at the 1-based line. **Opt-in** — registered but not in the
//! default `security` profile.
//!
//! Two confidence tiers:
//!
//! - **Confirmed / High** — self-identifying secret shapes whose format is
//!   unambiguous: AWS access key ids (`AKIA…`), PEM `PRIVATE KEY` blocks,
//!   GitHub tokens (`ghp_`/`gho_`/`ghu_`/`ghs_`/`ghr_`), Slack tokens
//!   (`xox[baprs]-…`), Google API keys (`AIza…`), Stripe live secret keys
//!   (`sk_live_…`), and JSON Web Tokens (`eyJ…`). The prefix identifies the
//!   vendor, so a match is treated as `Confirmed`.
//! - **Suspected / Medium** — a generic high-entropy heuristic: a
//!   variable / array-key / const whose *name* looks secret-bearing
//!   (`password`, `secret`, `api_key`, `token`, `client_secret`, …) assigned
//!   a string literal that is both long enough (≥ 16 chars) and high-entropy
//!   (Shannon entropy ≥ 3.5 bits/char). The length + entropy gate is what
//!   keeps `$password = ''` and `$apiKey = 'changeme'` from flagging. This
//!   tier is `Suspected` — false positives are expected.
//!
//! **The secret value is never echoed.** Confirmed findings quote only the
//! vendor prefix (`AKIA…`), which is public; the generic finding names the
//! offending *variable*, never its value. Regexes are avoided entirely (the
//! `regex` crate is not a workspace dependency) in favour of small hand-rolled
//! byte scanners, so this analyzer adds no new dependency.

use std::path::Path;

use super::php_files::scan_php_tree;
use crate::analyzer::{AnalysisCtx, Analyzer, AnalyzerError};
use crate::finding::{Category, Confidence, Finding, Severity};

/// See the module docs.
pub struct SecretsScan;

/// The analyzer's stable id.
pub const ID: &str = "secrets-scan";

/// A confirmed-tier detector: given the line bytes and a candidate start
/// index, returns the exclusive end of a match starting there, or `None`.
type Detector = fn(&[u8], usize) -> Option<usize>;

/// Minimum string length for the generic high-entropy tier. Below this a
/// literal is too short to be a meaningful secret (and rules out `''`).
const MIN_ENTROPY_LEN: usize = 16;

/// Minimum Shannon entropy (bits/char) for the generic high-entropy tier.
/// Rules out low-variety placeholders like `changeme` / repeated fillers.
const MIN_ENTROPY: f64 = 3.5;

/// `true` when `b` can be part of an identifier (used as a left boundary so a
/// vendor prefix embedded in a longer word is not treated as a token start).
fn ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `true` for a base64url alphabet byte (`[A-Za-z0-9_-]`).
fn is_b64url(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Scan `bytes`, invoking `at(bytes, i)` at every candidate start `i` that is
/// on a left identifier boundary. `at` returns the exclusive end of a match
/// starting exactly at `i`, or `None`. Returns each `(start, end)` byte range.
fn scan_all<F: Fn(&[u8], usize) -> Option<usize>>(bytes: &[u8], at: F) -> Vec<(usize, usize)> {
    let mut hits = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let matched = if i == 0 || !ident_byte(bytes[i - 1]) { at(bytes, i) } else { None };
        if let Some(end) = matched {
            hits.push((i, end));
            i = end;
        } else {
            i += 1;
        }
    }
    hits
}

/// Length of the run of bytes satisfying `ok` starting at `from`.
fn run_len(bytes: &[u8], from: usize, ok: fn(u8) -> bool) -> usize {
    let mut j = from;
    while j < bytes.len() && ok(bytes[j]) {
        j += 1;
    }
    j - from
}

/// A vendor-prefixed token whose random body is exactly `body_len` bytes of
/// `ok` (with a right boundary so a longer run is *not* a match).
fn exact_token(
    bytes: &[u8],
    i: usize,
    prefix: &[u8],
    body_len: usize,
    ok: fn(u8) -> bool,
) -> Option<usize> {
    if !bytes[i..].starts_with(prefix) {
        return None;
    }
    let body_start = i + prefix.len();
    if run_len(bytes, body_start, ok) == body_len { Some(body_start + body_len) } else { None }
}

/// A vendor-prefixed token whose random body is *at least* `min_body` bytes of
/// `ok`.
fn min_token(
    bytes: &[u8],
    i: usize,
    prefix: &[u8],
    min_body: usize,
    ok: fn(u8) -> bool,
) -> Option<usize> {
    if !bytes[i..].starts_with(prefix) {
        return None;
    }
    let body_start = i + prefix.len();
    let len = run_len(bytes, body_start, ok);
    if len >= min_body { Some(body_start + len) } else { None }
}

/// AWS access key id: `AKIA` + exactly 16 `[0-9A-Z]`.
fn aws_at(bytes: &[u8], i: usize) -> Option<usize> {
    exact_token(bytes, i, b"AKIA", 16, |b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// GitHub token: `gh[porsu]_` + exactly 36 `[0-9A-Za-z]`.
fn github_at(bytes: &[u8], i: usize) -> Option<usize> {
    const PREFIXES: [&[u8]; 5] = [b"ghp_", b"gho_", b"ghu_", b"ghs_", b"ghr_"];
    PREFIXES.iter().find_map(|p| exact_token(bytes, i, p, 36, |b| b.is_ascii_alphanumeric()))
}

/// Google API key: `AIza` + exactly 35 `[0-9A-Za-z_-]`.
fn google_at(bytes: &[u8], i: usize) -> Option<usize> {
    exact_token(bytes, i, b"AIza", 35, is_b64url)
}

/// Stripe live secret key: `sk_live_` + at least 24 `[0-9A-Za-z]`.
fn stripe_at(bytes: &[u8], i: usize) -> Option<usize> {
    min_token(bytes, i, b"sk_live_", 24, |b| b.is_ascii_alphanumeric())
}

/// Slack token: `xox[baprs]-` + at least 10 `[A-Za-z0-9-]`.
fn slack_at(bytes: &[u8], i: usize) -> Option<usize> {
    if !bytes[i..].starts_with(b"xox") {
        return None;
    }
    let kind = *bytes.get(i + 3)?;
    if !matches!(kind, b'b' | b'a' | b'p' | b'r' | b's') {
        return None;
    }
    if bytes.get(i + 4) != Some(&b'-') {
        return None;
    }
    let body_start = i + 5;
    let len = run_len(bytes, body_start, |b| b.is_ascii_alphanumeric() || b == b'-');
    if len >= 10 { Some(body_start + len) } else { None }
}

/// JSON Web Token header form: `eyJ` + base64url, `.`, base64url, optional
/// `.` + base64url signature. Requires a non-trivial header and payload.
fn jwt_at(bytes: &[u8], i: usize) -> Option<usize> {
    if !bytes[i..].starts_with(b"eyJ") {
        return None;
    }
    let seg1 = run_len(bytes, i, is_b64url);
    // `eyJ` (3) + at least 10 more of the encoded header.
    if seg1 < 13 || bytes.get(i + seg1) != Some(&b'.') {
        return None;
    }
    let seg2_start = i + seg1 + 1;
    let seg2 = run_len(bytes, seg2_start, is_b64url);
    if seg2 < 5 {
        return None;
    }
    let mut end = seg2_start + seg2;
    if bytes.get(end) == Some(&b'.') {
        let seg3_start = end + 1;
        end = seg3_start + run_len(bytes, seg3_start, is_b64url);
    }
    Some(end)
}

/// The confirmed-tier detectors, paired with their rule kind and description.
const CONFIRMED: &[(Detector, &str, &str)] = &[
    (aws_at, "aws-access-key-id", "AWS access key id"),
    (github_at, "github-token", "GitHub token"),
    (google_at, "google-api-key", "Google API key"),
    (stripe_at, "stripe-live-key", "Stripe live secret key"),
    (slack_at, "slack-token", "Slack token"),
    (jwt_at, "jwt", "JSON Web Token"),
];

/// A short, safe preview of a matched token: only the (public) vendor prefix,
/// never the full value.
fn redacted_prefix(matched: &[u8]) -> String {
    let shown = String::from_utf8_lossy(&matched[..matched.len().min(4)]);
    format!("{shown}…")
}

/// Shannon entropy of `s` in bits per byte.
fn shannon_entropy(s: &str) -> f64 {
    let mut counts = [0u32; 256];
    for &b in s.as_bytes() {
        counts[b as usize] += 1;
    }
    let total: u32 = counts.iter().sum();
    if total == 0 {
        return 0.0;
    }
    let len = f64::from(total);
    let mut entropy = 0.0;
    for &c in &counts {
        if c > 0 {
            let p = f64::from(c) / len;
            entropy -= p * p.log2();
        }
    }
    entropy
}

/// `true` when a name looks like it holds a secret. Separators (`_`/`-`) are
/// stripped so `api_key`, `api-key`, and `apiKey` all normalize alike.
fn name_is_sensitive(name: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "password",
        "passwd",
        "secret",
        "apikey",
        "token",
        "accesskey",
        "privatekey",
        "clientsecret",
        "auth",
    ];
    let norm: String =
        name.chars().filter(|c| *c != '_' && *c != '-').flat_map(char::to_lowercase).collect();
    KEYWORDS.iter().any(|k| norm.contains(k))
}

/// Find the closing `quote` byte at or after `from`, honouring backslash
/// escapes. Returns its index.
fn find_close(bytes: &[u8], from: usize, quote: u8) -> Option<usize> {
    let mut j = from;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 2,
            b if b == quote => return Some(j),
            _ => j += 1,
        }
    }
    None
}

/// If the text immediately before the string literal opening at `quote_idx`
/// is an assignment (`=` or `=>`), return the assigned-to name.
fn assignment_name(line: &str, quote_idx: usize) -> Option<String> {
    let before = line.get(..quote_idx)?.trim_end();
    let lhs = if let Some(stripped) = before.strip_suffix("=>") {
        stripped
    } else {
        let stripped = before.strip_suffix('=')?;
        // Exclude compound / comparison operators (`==`, `.=`, `+=`, `<=`, …):
        // those are not a plain assignment of the literal.
        if stripped.as_bytes().last().is_some_and(|b| {
            matches!(
                b,
                b'=' | b'<'
                    | b'>'
                    | b'!'
                    | b'+'
                    | b'-'
                    | b'*'
                    | b'/'
                    | b'.'
                    | b'%'
                    | b'&'
                    | b'|'
                    | b'^'
                    | b'~'
            )
        }) {
            return None;
        }
        stripped
    };
    let lhs = lhs.trim_end();
    let name = if lhs.ends_with('\'') || lhs.ends_with('"') {
        // A quoted array key: `'api_key' => …`.
        let quote = lhs.as_bytes()[lhs.len() - 1];
        let inner_end = lhs.len() - 1;
        let start = lhs.get(..inner_end)?.rfind(quote as char)? + 1;
        lhs.get(start..inner_end)?.to_owned()
    } else {
        // A bareword / variable / const: take the trailing identifier run.
        let bytes = lhs.as_bytes();
        let mut start = lhs.len();
        while start > 0 && (ident_byte(bytes[start - 1]) || bytes[start - 1] == b'$') {
            start -= 1;
        }
        lhs.get(start..)?.to_owned()
    };
    if name.is_empty() { None } else { Some(name) }
}

/// Emit confirmed-tier findings for one line.
fn scan_confirmed(line: &str, line_no: u64, rel: &Path, out: &mut Vec<Finding>) {
    let bytes = line.as_bytes();

    // PEM private-key block — matched at the line level, not as a token: the
    // secret is on the following lines, the BEGIN marker only identifies it.
    if line.contains("-----BEGIN") && line.contains("PRIVATE KEY-----") {
        out.push(Finding {
            rule_id: format!("{ID}/private-key"),
            severity: Severity::High,
            category: Category::Security,
            path: Some(rel.to_path_buf()),
            line: Some(line_no),
            message: "possible private key block committed in source".to_owned(),
            confidence: Confidence::Confirmed,
        });
    }

    for &(detector, kind, desc) in CONFIRMED {
        for (start, end) in scan_all(bytes, detector) {
            out.push(Finding {
                rule_id: format!("{ID}/{kind}"),
                severity: Severity::High,
                category: Category::Security,
                path: Some(rel.to_path_buf()),
                line: Some(line_no),
                message: format!(
                    "possible {desc} committed in source ({})",
                    redacted_prefix(&bytes[start..end])
                ),
                confidence: Confidence::Confirmed,
            });
        }
    }
}

/// Emit generic high-entropy findings for one line.
fn scan_generic(line: &str, line_no: u64, rel: &Path, out: &mut Vec<Finding>) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let quote = bytes[i];
        if (quote == b'\'' || quote == b'"')
            && let Some(close) = find_close(bytes, i + 1, quote)
        {
            if let Some(name) = assignment_name(line, i)
                && name_is_sensitive(&name)
                && let Some(value) = line.get(i + 1..close)
                && value.chars().count() >= MIN_ENTROPY_LEN
                && shannon_entropy(value) >= MIN_ENTROPY
            {
                out.push(Finding {
                    rule_id: format!("{ID}/generic-high-entropy"),
                    severity: Severity::Medium,
                    category: Category::Security,
                    path: Some(rel.to_path_buf()),
                    line: Some(line_no),
                    message: format!(
                        "high-entropy string assigned to secret-like name `{name}` — possible \
                         hardcoded credential (value redacted)"
                    ),
                    confidence: Confidence::Suspected,
                });
            }
            i = close + 1;
            continue;
        }
        i += 1;
    }
}

/// Scan one file's text, returning findings with 1-based line numbers.
fn scan_text(text: &str, rel_path: &Path) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (line_idx, line) in text.lines().enumerate() {
        let line_no = line_idx as u64 + 1;
        scan_confirmed(line, line_no, rel_path, &mut findings);
        scan_generic(line, line_no, rel_path, &mut findings);
    }
    findings
}

impl Analyzer for SecretsScan {
    fn id(&self) -> &str {
        ID
    }

    fn category(&self) -> Category {
        Category::Security
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

    #[test]
    fn finds_aws_access_key_id() {
        // AWS's own documented example key (public, not a live credential).
        let findings = scan("<?php\n$k = 'AKIAIOSFODNN7EXAMPLE';\n");
        assert_eq!(findings.len(), 1, "{findings:?}");
        let f = &findings[0];
        assert_eq!(f.rule_id, "secrets-scan/aws-access-key-id");
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.confidence, Confidence::Confirmed);
        assert_eq!(f.category, Category::Security);
        assert_eq!(f.line, Some(2));
    }

    #[test]
    fn finds_private_key_block() {
        let findings = scan("<?php\n$pem = \"-----BEGIN RSA PRIVATE KEY-----\";\n");
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].rule_id, "secrets-scan/private-key");
        assert_eq!(findings[0].confidence, Confidence::Confirmed);
        assert_eq!(findings[0].line, Some(2));
    }

    #[test]
    fn finds_private_key_block_without_algorithm() {
        let findings = scan("-----BEGIN PRIVATE KEY-----\n");
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].rule_id, "secrets-scan/private-key");
    }

    #[test]
    fn finds_github_token() {
        // ghp_ + exactly 36 alphanumerics.
        let token = format!("ghp_{}", "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8");
        assert_eq!(token.len(), 40);
        let findings = scan(&format!("<?php\n$t = '{token}';\n"));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].rule_id, "secrets-scan/github-token");
        assert_eq!(findings[0].confidence, Confidence::Confirmed);
    }

    #[test]
    fn finds_google_api_key() {
        // AIza + exactly 35 base64url chars.
        let body = "SyD1234567890abcdefghijklmnopqrstuvw";
        let key = format!("AIza{}", &body[..35]);
        assert_eq!(key.len(), 39);
        let findings = scan(&format!("<?php\n$g = '{key}';\n"));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].rule_id, "secrets-scan/google-api-key");
    }

    #[test]
    fn finds_stripe_live_key() {
        let key = format!("sk_live_{}", "4eC39HqLyjWDarjtT1zdp7dc");
        let findings = scan(&format!("<?php\n$s = '{key}';\n"));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].rule_id, "secrets-scan/stripe-live-key");
    }

    #[test]
    fn finds_slack_token() {
        // Assemble the token at runtime so the literal `xox…` byte sequence
        // never appears in this source file — otherwise platform secret
        // scanners (GitHub push protection) flag the test fixture itself.
        let token = format!("xo{}-{}-{}", "xb", "123456789012", "abcdefGHIJ0123");
        let findings = scan(&format!("<?php\n$s = '{token}';\n"));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].rule_id, "secrets-scan/slack-token");
    }

    #[test]
    fn finds_jwt() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.\
                   eyJzdWIiOiIxMjM0NTY3ODkwIn0.\
                   SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJVadQssw5c";
        let findings = scan(&format!("<?php\n$t = '{jwt}';\n"));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].rule_id, "secrets-scan/jwt");
        assert_eq!(findings[0].confidence, Confidence::Confirmed);
    }

    #[test]
    fn generic_entropy_hits_real_secret() {
        let findings = scan("<?php\n$password = 'Xy7Kp2Lm9Qr4Vn8Bs3Wt6Zc';\n");
        assert_eq!(findings.len(), 1, "{findings:?}");
        let f = &findings[0];
        assert_eq!(f.rule_id, "secrets-scan/generic-high-entropy");
        assert_eq!(f.severity, Severity::Medium);
        assert_eq!(f.confidence, Confidence::Suspected);
        assert_eq!(f.line, Some(2));
    }

    #[test]
    fn generic_entropy_hits_array_key_form() {
        let findings = scan("<?php\n$c = ['api_key' => 'Zx9Wm3Kp7Lq2Vn8Bs4Tc6Yd'];\n");
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].rule_id, "secrets-scan/generic-high-entropy");
    }

    #[test]
    fn generic_entropy_ignores_empty_and_placeholder() {
        // Empty string, short placeholder, and a non-secret-looking name must
        // not flag.
        assert!(scan("<?php\n$password = '';\n").is_empty());
        assert!(scan("<?php\n$apiKey = 'changeme';\n").is_empty());
        assert!(scan("<?php\n$greeting = 'HelloThereFriend123!';\n").is_empty());
    }

    #[test]
    fn generic_entropy_ignores_low_entropy_long_value() {
        // ≥ 16 chars but repetitive → below the entropy floor.
        assert!(scan("<?php\n$secret = 'aaaaaaaaaaaaaaaaaaaa';\n").is_empty());
    }

    #[test]
    fn generic_entropy_ignores_comparisons() {
        // `==` is a comparison, not an assignment of the literal.
        assert!(scan("<?php\nif ($password == 'Xy7Kp2Lm9Qr4Vn8Bs3Wt6Zc') {}\n").is_empty());
    }

    #[test]
    fn redaction_never_echoes_full_secret() {
        let key = "AKIAIOSFODNN7EXAMPLE";
        let findings = scan(&format!("<?php\n$k = '{key}';\n"));
        assert_eq!(findings.len(), 1);
        let msg = &findings[0].message;
        assert!(msg.contains("AKIA…"), "{msg}");
        assert!(!msg.contains(key), "full secret must never appear: {msg}");
    }

    #[test]
    fn empty_input_yields_nothing() {
        assert!(scan("").is_empty());
        assert!(scan("<?php\n$x = 1;\necho 'hello world, nothing secret here';\n").is_empty());
    }
}
