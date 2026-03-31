//! SQL normalization and digest hashing.
//!
//! Replaces literal values (strings, numbers) with `?` placeholders so
//! queries differing only in parameter values map to the same digest.
//! Uses a character-level state machine — same approach as MySQL's
//! `performance_schema`.

use std::hash::{Hash, Hasher};

/// Normalize a SQL query by replacing literal values with `?`.
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
/// ```
#[must_use]
pub fn normalize(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut state = State::Normal;
    let chars: Vec<char> = sql.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        let c = chars[i];
        match state {
            State::Normal => {
                // Single-quoted string
                if c == '\'' {
                    out.push('?');
                    state = State::SingleQuoted;
                    i += 1;
                }
                // Double-quoted string (treated as identifier in standard SQL,
                // but MySQL uses it for strings with ANSI_QUOTES off)
                else if c == '"' {
                    out.push('?');
                    state = State::DoubleQuoted;
                    i += 1;
                }
                // Backtick identifier — pass through
                else if c == '`' {
                    out.push(c);
                    state = State::Backtick;
                    i += 1;
                }
                // Line comment: -- ...
                else if c == '-' && i + 1 < len && chars[i + 1] == '-' {
                    state = State::LineComment;
                    i += 2;
                }
                // Block comment: /* ... */
                else if c == '/' && i + 1 < len && chars[i + 1] == '*' {
                    state = State::BlockComment;
                    i += 2;
                }
                // Hex literal: 0x... (must check before generic digit)
                else if c == '0'
                    && i + 1 < len
                    && (chars[i + 1] == 'x' || chars[i + 1] == 'X')
                {
                    let prev_is_ident = out
                        .chars()
                        .next_back()
                        .is_some_and(|p| p.is_alphanumeric() || p == '_');
                    if prev_is_ident {
                        out.push(c);
                        i += 1;
                    } else {
                        out.push('?');
                        // Skip 0x and hex digits
                        i += 2;
                        while i < len && chars[i].is_ascii_hexdigit() {
                            i += 1;
                        }
                    }
                }
                // Number (digit, or leading dot like .5)
                else if c.is_ascii_digit()
                    || (c == '.' && i + 1 < len && chars[i + 1].is_ascii_digit())
                {
                    let prev_is_ident = out
                        .chars()
                        .next_back()
                        .is_some_and(|p| p.is_alphanumeric() || p == '_');
                    if prev_is_ident {
                        out.push(c);
                        i += 1;
                    } else {
                        out.push('?');
                        state = State::Number;
                        i += 1;
                    }
                }
                // Whitespace collapse
                else if c.is_ascii_whitespace() {
                    if !out.ends_with(' ') && !out.is_empty() {
                        out.push(' ');
                    }
                    i += 1;
                }
                // Everything else
                else {
                    out.push(c);
                    i += 1;
                }
            }

            State::SingleQuoted => {
                if c == '\'' {
                    // Escaped quote '' — stay in string
                    if i + 1 < len && chars[i + 1] == '\'' {
                        i += 2;
                    } else {
                        state = State::Normal;
                        i += 1;
                    }
                } else if c == '\\' {
                    // Backslash escape — skip next char
                    i += 2;
                } else {
                    i += 1;
                }
            }

            State::DoubleQuoted => {
                if c == '"' {
                    if i + 1 < len && chars[i + 1] == '"' {
                        i += 2;
                    } else {
                        state = State::Normal;
                        i += 1;
                    }
                } else if c == '\\' {
                    i += 2;
                } else {
                    i += 1;
                }
            }

            State::Backtick => {
                out.push(c);
                if c == '`' {
                    state = State::Normal;
                }
                i += 1;
            }

            State::Number => {
                // Consume digits, dots, hex chars, exponent notation
                if c.is_ascii_digit() || c == '.' || c == 'e' || c == 'E' {
                    i += 1;
                } else {
                    state = State::Normal;
                    // Don't advance i — re-process this char in Normal state
                }
            }

            State::LineComment => {
                if c == '\n' {
                    state = State::Normal;
                }
                i += 1;
            }

            State::BlockComment => {
                if c == '*' && i + 1 < len && chars[i + 1] == '/' {
                    state = State::Normal;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        }
    }

    // Collapse IN (?, ?, ?, ...) to IN (?, ...)
    collapse_in_lists(&mut out);

    // Trim trailing whitespace
    while out.ends_with(' ') {
        out.pop();
    }

    out
}

/// Collapse `IN (?, ?, ?, ?)` to `IN (?, ...)`.
fn collapse_in_lists(sql: &mut String) {
    // Simple approach: find "IN (?" followed by ", ?" repetitions
    while let Some(start) = sql.find("IN (?, ?") {
        // Find the closing paren
        let after_in = start + 5; // position after "IN (?"
        let rest = &sql[after_in..];
        let mut end = after_in;
        let chars: Vec<char> = rest.chars().collect();
        let mut j = 0;
        while j + 2 < chars.len() && chars[j] == ',' && chars[j + 1] == ' ' && chars[j + 2] == '?'
        {
            j += 3;
        }
        end += j;
        if end > after_in {
            // Replace the repeated ", ?" with ", ..."
            sql.replace_range(after_in..end, ", ...");
        } else {
            break;
        }
    }
}

/// Compute a 64-bit digest hash of a normalized SQL string.
#[must_use]
pub fn digest_id(normalized: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    normalized.hash(&mut hasher);
    hasher.finish()
}

/// State machine for SQL normalization.
enum State {
    Normal,
    SingleQuoted,
    DoubleQuoted,
    Backtick,
    Number,
    LineComment,
    BlockComment,
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
        assert_eq!(
            normalize("WHERE price > 9.99"),
            "WHERE price > ?"
        );
    }

    #[test]
    fn normalize_escaped_quote() {
        assert_eq!(
            normalize("WHERE name = 'it''s'"),
            "WHERE name = ?"
        );
    }

    #[test]
    fn normalize_backslash_escape() {
        assert_eq!(
            normalize(r"WHERE name = 'it\'s'"),
            "WHERE name = ?"
        );
    }

    #[test]
    fn normalize_in_list() {
        assert_eq!(
            normalize("WHERE id IN (1, 2, 3, 4, 5)"),
            "WHERE id IN (?, ...)"
        );
    }

    #[test]
    fn normalize_in_list_single() {
        // Single value — no collapse
        assert_eq!(
            normalize("WHERE id IN (1)"),
            "WHERE id IN (?)"
        );
    }

    #[test]
    fn normalize_preserves_identifiers() {
        assert_eq!(
            normalize("SELECT col1, col2 FROM table1"),
            "SELECT col1, col2 FROM table1"
        );
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
        assert_eq!(
            normalize("SELECT 1 -- this is a comment\nFROM t"),
            "SELECT ? FROM t"
        );
    }

    #[test]
    fn normalize_strips_block_comment() {
        assert_eq!(
            normalize("SELECT /* comment */ 1 FROM t"),
            "SELECT ? FROM t"
        );
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
        assert_eq!(
            normalize("WHERE val IS NULL"),
            "WHERE val IS NULL"
        );
    }

    #[test]
    fn normalize_hex_literal() {
        assert_eq!(
            normalize("WHERE data = 0xDEADBEEF"),
            "WHERE data = ?"
        );
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
        assert_eq!(
            normalize("WHERE val = -42"),
            "WHERE val = -?"
        );
    }

    #[test]
    fn normalize_double_quoted_string() {
        assert_eq!(
            normalize(r#"WHERE name = "Alice""#),
            "WHERE name = ?"
        );
    }
}
