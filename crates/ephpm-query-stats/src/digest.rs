//! SQL normalization and digest hashing.
//!
//! Replaces literal values (strings, numbers) with `?` placeholders so
//! queries differing only in parameter values map to the same digest.
//! Uses a byte-level state machine — same approach as `MySQL`'s
//! `performance_schema`, but working on `&[u8]` rather than `Vec<char>`
//! so we do not allocate a `Vec` of `char`s (4 bytes/char) per
//! statement on the hot path.
//!
//! The output digest for a given SQL string is **stable** across this
//! rewrite: extend the test corpus in `tests` below when adding new
//! edge cases so a future rewrite can compare against the same
//! oracle.

use std::hash::{Hash, Hasher};

/// Normalize a SQL query by replacing literal values with `?`.
///
/// # Design
///
/// Single-pass, byte-oriented state machine — never materialises the
/// input as `Vec<char>`. IN-list collapse happens inline (once the
/// second `?` inside a run of `IN (?, ?, ...)` is emitted, subsequent
/// `, ?` entries are folded into `...`).
///
/// SQL identifiers are ASCII-only in the grammars ePHPm targets
/// (MySQL, SQLite, Postgres, TDS), so the state machine reads bytes
/// directly. Non-ASCII UTF-8 bytes inside string literals are elided
/// as part of the `?` placeholder — the placeholder covers the whole
/// literal.
///
/// # Examples
///
/// ```
/// use ephpm_query_stats::digest::normalize;
///
/// assert_eq!(
///     normalize("SELECT * FROM users WHERE id = 42"),
///     "SELECT * FROM users WHERE id = ?"
/// );
/// assert_eq!(
///     normalize("INSERT INTO t VALUES (1, 'hello', 3.14)"),
///     "INSERT INTO t VALUES (?, ?, ?)"
/// );
/// assert_eq!(
///     normalize("WHERE id IN (1, 2, 3, 4, 5)"),
///     "WHERE id IN (?, ...)"
/// );
/// ```
#[must_use]
pub fn normalize(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let len = bytes.len();

    while i < len {
        let b = bytes[i];
        match b {
            // ── Single-quoted string literal ──────────────────
            b'\'' => {
                emit_placeholder(&mut out);
                i = skip_quoted(bytes, i + 1, b'\'');
            }
            // ── Double-quoted string literal ──────────────────
            b'"' => {
                emit_placeholder(&mut out);
                i = skip_quoted(bytes, i + 1, b'"');
            }
            // ── Backtick-quoted identifier (preserved) ────────
            b'`' => {
                out.push(b);
                i += 1;
                while i < len {
                    out.push(bytes[i]);
                    if bytes[i] == b'`' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            // ── Line comment `-- ...` ─────────────────────────
            b'-' if i + 1 < len && bytes[i + 1] == b'-' => {
                i += 2;
                while i < len && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            // ── Block comment `/* ... */` ─────────────────────
            b'/' if i + 1 < len && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                // Skip past closing `*/` if present.
                if i + 1 < len {
                    i += 2;
                }
            }
            // ── Hex literal `0xDEADBEEF` ──────────────────────
            b'0' if i + 1 < len
                && (bytes[i + 1] == b'x' || bytes[i + 1] == b'X')
                && !prev_is_identifier(&out) =>
            {
                emit_placeholder(&mut out);
                i += 2;
                while i < len && bytes[i].is_ascii_hexdigit() {
                    i += 1;
                }
            }
            // ── Numeric literal (integer, float, leading dot) ─
            b'0'..=b'9' if !prev_is_identifier(&out) => {
                emit_placeholder(&mut out);
                i = skip_number(bytes, i + 1);
            }
            b'.' if i + 1 < len && bytes[i + 1].is_ascii_digit() && !prev_is_identifier(&out) => {
                emit_placeholder(&mut out);
                i = skip_number(bytes, i + 1);
            }
            // ── Whitespace collapse ───────────────────────────
            b' ' | b'\t' | b'\n' | b'\r' => {
                // Emit a single space between tokens, never a leading
                // space and never two in a row.
                if !out.is_empty() && out.last() != Some(&b' ') {
                    out.push(b' ');
                }
                i += 1;
            }
            // ── Everything else is passed through ─────────────
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }

    // Trim any trailing space introduced by whitespace collapse.
    while out.last() == Some(&b' ') {
        out.pop();
    }

    // SAFETY: `out` contains only ASCII bytes we pushed ourselves
    // (identifier bytes copied through, `?`, `,`, whitespace) or
    // backtick-quoted identifier bytes copied verbatim from `bytes`.
    // Non-ASCII UTF-8 sequences that appear only inside quoted string
    // literals are collapsed to `?` before being emitted — they never
    // reach `out`. So `out` is valid UTF-8. (Any pathological input
    // that violates this would already be a bug in the state machine;
    // we validate with `from_utf8` as a belt-and-suspenders check.)
    String::from_utf8(out).unwrap_or_default()
}

/// Emit a `?` placeholder to the normalized output, folding runs of
/// `?, ?, ?, ...` inside an `IN (` list into a single `?, ...`.
///
/// Detection rule: at the moment we're about to push another `?`, if
/// the tail of `out` is already `?, ` and the byte before that first
/// `?` is `(`, `,`, or ` ` (i.e. we really are inside a
/// comma-separated list), replace the trailing `, ` with `, ...` and
/// drop this `?`. Subsequent `?` in the same list will hit the
/// second guard (`..., `) and also be dropped.
fn emit_placeholder(out: &mut Vec<u8>) {
    // Third-or-later placeholder inside an IN-list we've already
    // collapsed. Tail will look like `..., ` (comma+space came in
    // after the previous element and its `?` was folded). Drop this
    // `?` AND the trailing `, ` so the eventual closing `)` sits flush
    // against `...`.
    if ends_with(out, b"..., ") {
        let n = out.len();
        out.truncate(n - 2);
        return;
    }
    // Second placeholder inside an IN-list. Tail is `?, ` — check
    // whether we're actually inside `IN (` (as opposed to `VALUES
    // (`, a function call, a subquery, etc.). Only IN lists collapse,
    // matching the historical `collapse_in_lists` behaviour so digest
    // outputs stay stable across the rewrite.
    if ends_with(out, b"?, ") && preceding_list_is_in(out) {
        let n = out.len();
        // Replace the trailing `, ` with `, ...` and drop the `?`.
        out.truncate(n - 2);
        out.extend_from_slice(b", ...");
        return;
    }
    out.push(b'?');
}

/// Given that `out` ends with `?, `, check whether the opening `(` of
/// the surrounding list was preceded by the `IN` keyword. Only IN
/// lists collapse; `VALUES (?, ?, ?)`, subqueries, and function calls
/// must be left untouched to preserve the historical digest shape.
fn preceding_list_is_in(out: &[u8]) -> bool {
    // Layout at call time (tail): `... IN (?, `
    //                                  ^^^ we walk back from here.
    // n-3 = '?', n-4 = '(' (the list opener we want to check).
    let n = out.len();
    if n < 4 || out[n - 4] != b'(' {
        return false;
    }
    // Scan backwards from just before `(`, skipping whitespace, and
    // check for the two ASCII bytes `N`/`n` then `I`/`i`.
    let mut j = n - 4;
    while j > 0 && matches!(out[j - 1], b' ' | b'\t') {
        j -= 1;
    }
    if j < 2 {
        return false;
    }
    let a = out[j - 2];
    let b = out[j - 1];
    let is_in = (a == b'I' || a == b'i') && (b == b'N' || b == b'n');
    if !is_in {
        return false;
    }
    // Guard against matching the tail of an identifier like `WITHIN`
    // or `MAIN`: the byte before `IN` must not be identifier-like.
    if j >= 3 {
        let before = out[j - 3];
        if before.is_ascii_alphanumeric() || before == b'_' {
            return false;
        }
    }
    true
}

/// Check whether `out` ends with the byte suffix `suffix`.
#[inline]
fn ends_with(out: &[u8], suffix: &[u8]) -> bool {
    out.len() >= suffix.len() && &out[out.len() - suffix.len()..] == suffix
}

/// Advance past a quoted string literal starting at `start`. Handles
/// SQL-standard doubled-quote escapes (`''`, `""`) and backslash
/// escapes (`\'`, `\"`). Returns the index just after the closing
/// quote (or `len` if unterminated).
fn skip_quoted(bytes: &[u8], start: usize, quote: u8) -> usize {
    let len = bytes.len();
    let mut i = start;
    while i < len {
        let b = bytes[i];
        if b == quote {
            // Doubled quote is an escaped quote — skip both and keep
            // going.
            if i + 1 < len && bytes[i + 1] == quote {
                i += 2;
                continue;
            }
            return i + 1;
        }
        if b == b'\\' && i + 1 < len {
            i += 2;
            continue;
        }
        i += 1;
    }
    len
}

/// Advance past a numeric literal starting at `start`. Consumes
/// digits, optional decimal point, and optional `e`/`E` exponent.
fn skip_number(bytes: &[u8], start: usize) -> usize {
    let len = bytes.len();
    let mut i = start;
    while i < len {
        let b = bytes[i];
        if b.is_ascii_digit() || b == b'.' || b == b'e' || b == b'E' {
            i += 1;
        } else {
            break;
        }
    }
    i
}

/// Returns `true` if the last byte in `out` looks like part of an
/// identifier (`[A-Za-z0-9_]`).
#[inline]
fn prev_is_identifier(out: &[u8]) -> bool {
    matches!(out.last(), Some(b) if b.is_ascii_alphanumeric() || *b == b'_')
}

/// Compute a 64-bit digest hash of a normalized SQL string.
///
/// Uses `DefaultHasher` (SipHash-1-3). The follow-up in #141 called
/// out `ahash`/`xxhash` as candidates, but no faster hasher is
/// currently a workspace dependency, so keeping `DefaultHasher`
/// avoids adding one just for this crate. Revisit if a workspace-wide
/// hasher lands.
#[must_use]
pub fn digest_id(normalized: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    normalized.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_integer_literal() {
        assert_eq!(
            normalize("SELECT * FROM users WHERE id = 42"),
            "SELECT * FROM users WHERE id = ?"
        );
    }

    #[test]
    fn normalize_string_literal() {
        assert_eq!(
            normalize("SELECT * FROM users WHERE name = 'Alice'"),
            "SELECT * FROM users WHERE name = ?"
        );
    }

    #[test]
    fn normalize_multiple_literals() {
        assert_eq!(
            normalize("INSERT INTO t VALUES (1, 'hello', 3.14)"),
            "INSERT INTO t VALUES (?, ?, ?)"
        );
    }

    #[test]
    fn normalize_float() {
        assert_eq!(normalize("WHERE price > 9.99"), "WHERE price > ?");
    }

    #[test]
    fn normalize_escaped_quote() {
        assert_eq!(normalize("WHERE name = 'it''s'"), "WHERE name = ?");
    }

    #[test]
    fn normalize_backslash_escape() {
        assert_eq!(normalize(r"WHERE name = 'it\'s'"), "WHERE name = ?");
    }

    #[test]
    fn normalize_in_list() {
        assert_eq!(normalize("WHERE id IN (1, 2, 3, 4, 5)"), "WHERE id IN (?, ...)");
    }

    #[test]
    fn normalize_in_list_single() {
        // Single value — no collapse
        assert_eq!(normalize("WHERE id IN (1)"), "WHERE id IN (?)");
    }

    #[test]
    fn normalize_in_list_two_values() {
        // Two values also compresses to `?, ...` — matches the old
        // behaviour of `collapse_in_lists` which triggered at `?, ?`.
        assert_eq!(normalize("WHERE id IN (1, 2)"), "WHERE id IN (?, ...)");
    }

    #[test]
    fn normalize_preserves_identifiers() {
        assert_eq!(normalize("SELECT col1, col2 FROM table1"), "SELECT col1, col2 FROM table1");
    }

    #[test]
    fn normalize_preserves_backtick_identifiers() {
        assert_eq!(
            normalize("SELECT `id` FROM `users` WHERE `id` = 1"),
            "SELECT `id` FROM `users` WHERE `id` = ?"
        );
    }

    #[test]
    fn normalize_strips_line_comment() {
        assert_eq!(normalize("SELECT 1 -- this is a comment\nFROM t"), "SELECT ? FROM t");
    }

    #[test]
    fn normalize_strips_block_comment() {
        assert_eq!(normalize("SELECT /* comment */ 1 FROM t"), "SELECT ? FROM t");
    }

    #[test]
    fn normalize_collapses_whitespace() {
        assert_eq!(
            normalize("SELECT   *   FROM   t   WHERE   id = 1"),
            "SELECT * FROM t WHERE id = ?"
        );
    }

    #[test]
    fn normalize_preserves_null_keyword() {
        assert_eq!(normalize("WHERE val IS NULL"), "WHERE val IS NULL");
    }

    #[test]
    fn normalize_hex_literal() {
        assert_eq!(normalize("WHERE data = 0xDEADBEEF"), "WHERE data = ?");
    }

    #[test]
    fn digest_same_query_same_hash() {
        let a = normalize("SELECT * FROM t WHERE id = 1");
        let b = normalize("SELECT * FROM t WHERE id = 999");
        assert_eq!(digest_id(&a), digest_id(&b));
    }

    #[test]
    fn digest_different_queries_different_hash() {
        let a = normalize("SELECT * FROM t WHERE id = 1");
        let b = normalize("SELECT * FROM t WHERE name = 'x'");
        assert_ne!(digest_id(&a), digest_id(&b));
    }

    #[test]
    fn normalize_negative_number() {
        // -42 is unary minus + number literal
        assert_eq!(normalize("WHERE val = -42"), "WHERE val = -?");
    }

    #[test]
    fn normalize_double_quoted_string() {
        assert_eq!(normalize(r#"WHERE name = "Alice""#), "WHERE name = ?");
    }

    // ── New edge cases: unicode in literals, nested parens, tab/CRLF ─

    #[test]
    fn normalize_unicode_in_string_literal() {
        // Non-ASCII UTF-8 bytes inside a quoted literal are collapsed
        // into the `?` placeholder just like any other content; the
        // output is still valid UTF-8.
        assert_eq!(
            normalize("SELECT * FROM t WHERE name = 'café ☕'"),
            "SELECT * FROM t WHERE name = ?"
        );
    }

    #[test]
    fn normalize_unicode_in_double_quoted_literal() {
        assert_eq!(
            normalize(r#"SELECT * FROM t WHERE name = "北京""#),
            "SELECT * FROM t WHERE name = ?"
        );
    }

    #[test]
    fn normalize_escaped_double_quote_in_string() {
        // \" inside a double-quoted literal must not terminate it.
        assert_eq!(normalize(r#"WHERE name = "he said \"hi\"""#), "WHERE name = ?");
    }

    #[test]
    fn normalize_doubled_double_quote_in_string() {
        // "" inside a double-quoted literal is an escaped quote.
        assert_eq!(normalize(r#"WHERE name = "he said ""hi""""#), "WHERE name = ?");
    }

    #[test]
    fn normalize_in_list_with_nested_paren_in_string() {
        // A `(` inside a string literal must not confuse the IN-list
        // collapse — the literal is elided first.
        assert_eq!(normalize("WHERE id IN ('a(b', 'c(d', 'e(f')"), "WHERE id IN (?, ...)");
    }

    #[test]
    fn normalize_nested_parens_in_expression() {
        // Non-IN parens (subqueries, function calls) should not
        // trigger IN collapse.
        assert_eq!(
            normalize("SELECT COALESCE((SELECT 1), (SELECT 2), 3)"),
            "SELECT COALESCE((SELECT ?), (SELECT ?), ?)"
        );
    }

    #[test]
    fn normalize_tab_and_crlf_whitespace() {
        assert_eq!(
            normalize("SELECT\t*\r\nFROM\tt\r\nWHERE\tid\t=\t1"),
            "SELECT * FROM t WHERE id = ?"
        );
    }

    #[test]
    fn normalize_multiple_in_lists_in_same_query() {
        // Each independent IN list collapses; the second must not
        // pollute the first.
        assert_eq!(
            normalize("WHERE a IN (1, 2, 3) AND b IN (4, 5, 6)"),
            "WHERE a IN (?, ...) AND b IN (?, ...)"
        );
    }

    #[test]
    fn normalize_unterminated_string_does_not_panic() {
        // Adversarial input: missing closing quote. Must produce
        // *some* string without panicking.
        let out = normalize("WHERE name = 'unterminated");
        assert!(out.contains('?'));
    }

    #[test]
    fn normalize_empty_string_literal() {
        assert_eq!(normalize("WHERE name = ''"), "WHERE name = ?");
    }

    #[test]
    fn normalize_empty_input() {
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn normalize_only_whitespace() {
        assert_eq!(normalize("   \t\n"), "");
    }
}
