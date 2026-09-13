//! `weak-crypto` — native heuristic for insecure cryptography (CWE-327/328/330).
//!
//! Signature scanners and taint engines cover injection-class bugs well, but
//! *insecure cryptography* — disabled TLS verification, ECB block-cipher mode,
//! the removed `mcrypt` extension, non-cryptographic randomness, and weak hash
//! functions — is a well-defined security category only partially and
//! generically covered by the opt-in `semgrep-php` rulesets today. This
//! analyzer flags it directly.
//!
//! This is a **native, source-text token pass** over `*.php` / `*.phtml`,
//! sharing the per-file walker in `php_files` with `dangerous-sinks` and the
//! other native passes (diff-aware scope skipping and the incremental cache
//! included) — no external tool, no subprocess, no PHP-linked build, and no new
//! dependency. It is **opt-in**: registered so `analyzers.enable` accepts it,
//! but absent from the default `security` profile.
//!
//! Because it reasons over source text, it cannot see through comments and
//! string literals: a cipher name inside a comment, or `md5()` mentioned in a
//! docblock, is a false positive. That is exactly why every finding is
//! `Suspected` (a review hotspot, never a hard deny) — the same trade
//! `dangerous-sinks` documents.
//!
//! Rule ids emitted (`weak-crypto/<kind>`):
//! - `tls-verify-disabled` (high) — `CURLOPT_SSL_VERIFYPEER` set to
//!   `false`/`0`, `CURLOPT_SSL_VERIFYHOST` set to `0`/`false`, or a Guzzle
//!   `'verify' => false` option. Disabling TLS verification defeats HTTPS
//!   entirely, so this is the most dangerous rule here — high.
//! - `ecb-mode` (medium) — the ECB block-cipher mode: the `MCRYPT_MODE_ECB`
//!   constant, or an OpenSSL cipher string ending `-ecb` (e.g. `'aes-256-ecb'`).
//!   ECB leaks plaintext structure (identical blocks encrypt identically).
//! - `mcrypt` (medium) — any `mcrypt_*()` call; the extension is removed as of
//!   PHP 7.2 and was insecure (no authenticated modes, PKCS#7 padding bugs).
//! - `insecure-random` (medium) — `rand()`, `mt_rand()`, or `uniqid()`. These
//!   are frequently used for non-security purposes (jitter, cache-buster
//!   suffixes), which a token pass cannot distinguish — hence medium /
//!   suspected. The security concern is using them for tokens / nonces / salts,
//!   where `random_int()` / `random_bytes()` is required.
//! - `weak-hash` (low) — `md5()` or `sha1()` calls. Very frequently legitimate
//!   (cache keys, ETags, file checksums), so low / suspected; a password- or
//!   token-hashing context would be the real concern, but a token pass cannot
//!   confirm the context.

use std::path::Path;

use super::php_files::scan_php_tree;
use crate::analyzer::{AnalysisCtx, Analyzer, AnalyzerError};
use crate::finding::{Category, Confidence, Finding, Severity};

/// See the module docs.
pub struct WeakCrypto;

/// The analyzer's stable id.
pub const ID: &str = "weak-crypto";

/// cURL options whose disabled value (`false` / `0`) turns off TLS certificate
/// verification.
const TLS_VERIFY_CONSTANTS: &[&str] = &["CURLOPT_SSL_VERIFYPEER", "CURLOPT_SSL_VERIFYHOST"];

/// Direct-call rules: `(function name, rule kind, severity, note)`. One
/// [`Finding`] per call site, `rule_id = weak-crypto/<kind>`.
const CALL_RULES: &[(&str, &str, Severity, &str)] = &[
    (
        "rand",
        "insecure-random",
        Severity::Medium,
        "not a cryptographically secure RNG — use random_int()/random_bytes() for tokens, nonces, \
         or salts",
    ),
    (
        "mt_rand",
        "insecure-random",
        Severity::Medium,
        "not a cryptographically secure RNG — use random_int()/random_bytes() for tokens, nonces, \
         or salts",
    ),
    (
        "uniqid",
        "insecure-random",
        Severity::Medium,
        "not a cryptographically secure source of uniqueness — use random_bytes() for \
         unguessable tokens",
    ),
    (
        "md5",
        "weak-hash",
        Severity::Low,
        "a weak hash — fine for checksums/cache keys, but not for passwords, tokens, or signatures",
    ),
    (
        "sha1",
        "weak-hash",
        Severity::Low,
        "a weak hash — fine for checksums/cache keys, but not for passwords, tokens, or signatures",
    ),
];

/// `true` when the byte before a candidate token rules it out as a direct
/// function call: part of a longer identifier, a `$`-variable, or a method /
/// static access (`->` / `::`). Mirrors `dangerous_sinks`.
fn preceded_by_identifier_byte(line: &str, start: usize) -> bool {
    line[..start]
        .bytes()
        .next_back()
        .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$' | b'>' | b':'))
}

/// `true` when the token ending at `after` is immediately followed (modulo
/// spaces/tabs) by `(`.
fn followed_by_call_paren(line: &str, after: usize) -> bool {
    line[after..].bytes().find(|b| !matches!(b, b' ' | b'\t')) == Some(b'(')
}

/// `true` when `s` begins with `word` (ASCII case-insensitive) as a whole
/// token — the byte after it must not continue an identifier.
fn starts_with_word(s: &str, word: &str) -> bool {
    let b = s.as_bytes();
    let w = word.as_bytes();
    if b.len() < w.len() || !b[..w.len()].eq_ignore_ascii_case(w) {
        return false;
    }
    b.get(w.len()).is_none_or(|c| !(c.is_ascii_alphanumeric() || *c == b'_'))
}

/// `true` when `s` begins with an integer literal `0` (and not `0x…`, `0.5`,
/// or a longer number like `08`).
fn is_zero_literal(s: &str) -> bool {
    let b = s.as_bytes();
    b.first() == Some(&b'0') && b.get(1).is_none_or(|c| !(c.is_ascii_alphanumeric() || *c == b'.'))
}

/// Given the text immediately after a cURL option constant, `true` when the
/// value it is assigned (across a `,` argument separator or a `=>` array-key
/// arrow) is a verification-disabling `false` or `0`.
fn value_is_disabled(after: &str) -> bool {
    let s = after.trim_start();
    let s = s.strip_prefix("=>").or_else(|| s.strip_prefix(',')).unwrap_or(s);
    let s = s.trim_start();
    starts_with_word(s, "false") || is_zero_literal(s)
}

/// `true` when the line sets the Guzzle request option `'verify'` (or
/// `"verify"`) to `false`.
fn guzzle_verify_disabled(line: &str) -> bool {
    for pat in ["'verify'", "\"verify\""] {
        if let Some(pos) = line.find(pat) {
            let after = line[pos + pat.len()..].trim_start();
            if let Some(rest) = after.strip_prefix("=>")
                && starts_with_word(rest.trim_start(), "false")
            {
                return true;
            }
        }
    }
    false
}

/// Build a `Suspected`, `Security`-category finding at `line_no`.
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
        category: Category::Security,
        path: Some(rel_path.to_path_buf()),
        line: Some(line_no),
        // A source-text token pass has known false positives (matches inside
        // comments/strings, and non-security uses of md5/rand) — a hotspot for
        // review, never proof.
        confidence: Confidence::Suspected,
        message,
    }
}

/// Emit `tls-verify-disabled` findings for one line.
fn scan_tls(line: &str, line_no: u64, rel: &Path, out: &mut Vec<Finding>) {
    for &konst in TLS_VERIFY_CONSTANTS {
        let mut search_from = 0;
        while let Some(pos) = line[search_from..].find(konst) {
            let start = search_from + pos;
            let after = start + konst.len();
            search_from = after;
            if value_is_disabled(&line[after..]) {
                out.push(finding(
                    rel,
                    line_no,
                    "tls-verify-disabled",
                    Severity::High,
                    format!(
                        "{konst} set to a verification-disabling value — TLS certificate \
                         verification turned off"
                    ),
                ));
            }
        }
    }
    if guzzle_verify_disabled(line) {
        out.push(finding(
            rel,
            line_no,
            "tls-verify-disabled",
            Severity::High,
            "Guzzle 'verify' => false — TLS certificate verification turned off".to_owned(),
        ));
    }
}

/// Emit a single `ecb-mode` finding for one line, if it references ECB mode.
fn scan_ecb(line: &str, line_no: u64, rel: &Path, out: &mut Vec<Finding>) {
    let lower = line.to_ascii_lowercase();
    let ecb_cipher = lower.contains("-ecb'") || lower.contains("-ecb\"");
    if line.contains("MCRYPT_MODE_ECB") || ecb_cipher {
        out.push(finding(
            rel,
            line_no,
            "ecb-mode",
            Severity::Medium,
            "ECB block-cipher mode — identical plaintext blocks encrypt identically, leaking \
             structure; use an authenticated mode (GCM) or at least CBC"
                .to_owned(),
        ));
    }
}

/// Emit `mcrypt` findings for one line — any direct `mcrypt_*()` call.
fn scan_mcrypt(line: &str, line_no: u64, rel: &Path, out: &mut Vec<Finding>) {
    const PREFIX: &str = "mcrypt_";
    let bytes = line.as_bytes();
    let mut search_from = 0;
    while let Some(pos) = line[search_from..].find(PREFIX) {
        let start = search_from + pos;
        let mut end = start + PREFIX.len();
        while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
            end += 1;
        }
        search_from = end.max(start + 1);
        // A bare `mcrypt_` with no suffix, a member/identifier continuation, or
        // no call paren is not a direct `mcrypt_*()` call.
        if end == start + PREFIX.len()
            || preceded_by_identifier_byte(line, start)
            || !followed_by_call_paren(line, end)
        {
            continue;
        }
        let name = &line[start..end];
        out.push(finding(
            rel,
            line_no,
            "mcrypt",
            Severity::Medium,
            format!("{name}() — the mcrypt extension is removed (PHP 7.2+) and insecure; use openssl_* or sodium_*"),
        ));
    }
}

/// Emit `insecure-random` / `weak-hash` findings for one line — direct calls to
/// the functions in [`CALL_RULES`].
fn scan_calls(line: &str, line_no: u64, rel: &Path, out: &mut Vec<Finding>) {
    for &(name, kind, severity, note) in CALL_RULES {
        let mut search_from = 0;
        while let Some(pos) = line[search_from..].find(name) {
            let start = search_from + pos;
            let after = start + name.len();
            search_from = after;
            if preceded_by_identifier_byte(line, start) || !followed_by_call_paren(line, after) {
                continue;
            }
            out.push(finding(rel, line_no, kind, severity, format!("{name}() — {note}")));
        }
    }
}

/// Scan one file's text, returning findings with 1-based line numbers.
fn scan_text(text: &str, rel_path: &Path) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (line_idx, line) in text.lines().enumerate() {
        let line_no = line_idx as u64 + 1;
        scan_tls(line, line_no, rel_path, &mut findings);
        scan_ecb(line, line_no, rel_path, &mut findings);
        scan_mcrypt(line, line_no, rel_path, &mut findings);
        scan_calls(line, line_no, rel_path, &mut findings);
    }
    findings
}

impl Analyzer for WeakCrypto {
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

    fn ids(findings: &[Finding]) -> Vec<&str> {
        findings.iter().map(|f| f.rule_id.as_str()).collect()
    }

    #[test]
    fn finds_curl_verifypeer_false() {
        let findings = scan("<?php\ncurl_setopt($ch, CURLOPT_SSL_VERIFYPEER, false);\n");
        assert_eq!(ids(&findings), vec!["weak-crypto/tls-verify-disabled"]);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.category, Category::Security);
        assert_eq!(f.confidence, Confidence::Suspected);
        assert_eq!(f.line, Some(2));
    }

    #[test]
    fn finds_curl_verifyhost_zero() {
        let findings = scan("<?php\ncurl_setopt($ch, CURLOPT_SSL_VERIFYHOST, 0);\n");
        assert_eq!(ids(&findings), vec!["weak-crypto/tls-verify-disabled"]);
        assert_eq!(findings[0].severity, Severity::High);
    }

    #[test]
    fn finds_curl_verify_in_array_arrow_form() {
        let findings = scan("<?php\n$opts = [CURLOPT_SSL_VERIFYPEER => false];\n");
        assert_eq!(ids(&findings), vec!["weak-crypto/tls-verify-disabled"]);
    }

    #[test]
    fn finds_guzzle_verify_false() {
        let findings = scan("<?php\n$client->get($url, ['verify' => false]);\n");
        assert_eq!(ids(&findings), vec!["weak-crypto/tls-verify-disabled"]);
        assert_eq!(findings[0].severity, Severity::High);
    }

    #[test]
    fn curl_verify_enabled_is_clean() {
        // The secure values (`true`, `2`) must not flag.
        let findings = scan(
            "<?php\ncurl_setopt($ch, CURLOPT_SSL_VERIFYPEER, true);\n\
             curl_setopt($ch, CURLOPT_SSL_VERIFYHOST, 2);\n",
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn finds_mcrypt_mode_ecb_constant() {
        let findings = scan("<?php\n$mode = MCRYPT_MODE_ECB;\n");
        let kinds = ids(&findings);
        assert!(kinds.contains(&"weak-crypto/ecb-mode"), "{kinds:?}");
        let ecb = findings.iter().find(|f| f.rule_id == "weak-crypto/ecb-mode").unwrap();
        assert_eq!(ecb.severity, Severity::Medium);
        assert_eq!(ecb.line, Some(2));
    }

    #[test]
    fn finds_openssl_ecb_cipher_string() {
        let findings = scan("<?php\n$c = openssl_encrypt($d, 'aes-256-ecb', $k);\n");
        assert_eq!(ids(&findings), vec!["weak-crypto/ecb-mode"]);
        assert_eq!(findings[0].severity, Severity::Medium);
    }

    #[test]
    fn ecb_emitted_once_per_line() {
        // Both indicators on one line still yields a single ecb-mode finding.
        let findings = scan("<?php\n$x = MCRYPT_MODE_ECB; $c = 'aes-128-ecb';\n");
        assert_eq!(ids(&findings), vec!["weak-crypto/ecb-mode"]);
    }

    #[test]
    fn cbc_cipher_string_is_clean() {
        let findings = scan("<?php\n$c = openssl_encrypt($d, 'aes-256-cbc', $k);\n");
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn finds_mcrypt_call() {
        let findings = scan("<?php\n$out = mcrypt_encrypt($cipher, $key, $data, $mode);\n");
        assert_eq!(ids(&findings), vec!["weak-crypto/mcrypt"]);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Medium);
        assert_eq!(f.line, Some(2));
    }

    #[test]
    fn mcrypt_variable_and_member_are_not_calls() {
        // A `$mcrypt_*` variable, a `->mcrypt_*` member call, and a bareword
        // with no call paren must not flag.
        let findings =
            scan("<?php\n$mcrypt_key = 1;\n$obj->mcrypt_helper($x);\n$name = 'mcrypt_encrypt';\n");
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn finds_insecure_random() {
        let findings = scan("<?php\n$token = mt_rand();\n");
        assert_eq!(ids(&findings), vec!["weak-crypto/insecure-random"]);
        assert_eq!(findings[0].severity, Severity::Medium);
        assert_eq!(findings[0].line, Some(2));
    }

    #[test]
    fn finds_rand_and_uniqid() {
        assert_eq!(ids(&scan("<?php\n$n = rand(1, 10);\n")), vec!["weak-crypto/insecure-random"]);
        assert_eq!(ids(&scan("<?php\n$id = uniqid();\n")), vec!["weak-crypto/insecure-random"]);
    }

    #[test]
    fn finds_weak_hash_md5_is_low() {
        let findings = scan("<?php\n$h = md5($password);\n");
        assert_eq!(ids(&findings), vec!["weak-crypto/weak-hash"]);
        assert_eq!(findings[0].severity, Severity::Low);
        assert_eq!(findings[0].confidence, Confidence::Suspected);
        assert_eq!(findings[0].line, Some(2));
    }

    #[test]
    fn finds_weak_hash_sha1() {
        let findings = scan("<?php\n$h = sha1($data);\n");
        assert_eq!(ids(&findings), vec!["weak-crypto/weak-hash"]);
        assert_eq!(findings[0].severity, Severity::Low);
    }

    #[test]
    fn secure_primitives_are_clean() {
        // The recommended replacements must never flag.
        let findings = scan(
            "<?php\n$n = random_int(1, 10);\n$b = random_bytes(16);\n\
             $h = hash('sha256', $data);\ncurl_setopt($ch, CURLOPT_SSL_VERIFYPEER, true);\n",
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn longer_identifiers_do_not_match_random_or_hash() {
        // `array_rand`, `str_rand`-like, `md5_file`, and `sha1_file` share a
        // prefix/suffix but are distinct tokens.
        let findings =
            scan("<?php\n$k = array_rand($a);\n$h = md5_file($p);\n$s = sha1_file($p);\n");
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn clean_file_yields_nothing() {
        let findings = scan("<?php\n$x = 1;\necho 'hello world';\n$y = strlen($x);\n");
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn line_anchoring_across_multiple_rules() {
        let findings = scan(
            "<?php\n// header comment\n$h = md5($p);\n$n = mt_rand();\n\
             curl_setopt($ch, CURLOPT_SSL_VERIFYPEER, false);\n",
        );
        // Findings are collected line by line, so each anchors at its own line.
        assert_eq!(findings.len(), 3);
        let weak = findings.iter().find(|f| f.rule_id == "weak-crypto/weak-hash").unwrap();
        assert_eq!(weak.line, Some(3));
        let rng = findings.iter().find(|f| f.rule_id == "weak-crypto/insecure-random").unwrap();
        assert_eq!(rng.line, Some(4));
        let tls = findings.iter().find(|f| f.rule_id == "weak-crypto/tls-verify-disabled").unwrap();
        assert_eq!(tls.line, Some(5));
    }

    #[test]
    fn all_findings_are_suspected_security() {
        let findings = scan(
            "<?php\ncurl_setopt($ch, CURLOPT_SSL_VERIFYPEER, false);\n$h = md5($p);\n\
             $m = MCRYPT_MODE_ECB;\n$out = mcrypt_encrypt($a, $b, $c, $m);\n$n = rand();\n",
        );
        assert!(!findings.is_empty());
        assert!(
            findings
                .iter()
                .all(|f| f.confidence == Confidence::Suspected && f.category == Category::Security)
        );
    }

    #[test]
    fn empty_input_yields_nothing() {
        assert!(scan("").is_empty());
    }
}
