//! Redis-compatible glob matching for `KEYS`.
//!
//! # Semantics
//!
//! Mirrors Redis' `stringmatchlen` (`src/util.c`), byte for byte:
//!
//! - `*` matches any run of bytes; consecutive `*` collapse into one.
//! - `?` matches exactly one byte.
//! - `[abc]` matches one byte from the set; `[^abc]` negates it (`^` is the
//!   only negation marker — like Redis, `[!abc]` is the set `!`, `a`, `b`,
//!   `c`). `[a-z]` is an inclusive byte range, and a reversed range (`[z-a]`)
//!   is swapped. `\` inside a class escapes the next byte. A `]` straight
//!   after `[` (or `[^`) closes an empty class, and an unterminated class
//!   runs to the end of the pattern — both exactly as Redis parses them.
//! - `\x` outside a class matches `x` literally; a trailing lone `\` matches
//!   a literal backslash.
//! - Matching is on bytes, not chars: `?` matches one byte of a multi-byte
//!   UTF-8 character, as in Redis.
//! - A non-empty pattern never matches the empty string (Redis only reaches
//!   its loop with a non-empty string). `KEYS *` still lists an empty key
//!   because [`Store::keys`](super::Store::keys) short-circuits the literal
//!   `*` pattern, as Redis' `keysCommand` does.
//!
//! # Complexity
//!
//! The previous matcher recursed on every `*` without memoisation, which is
//! exponential in the number of stars: `*a*a*a*a*a*a*a*a*a*a*a*a*b` against a
//! 60-byte key ran for more than 30 s (CVE-2022-36021 is the same pathology
//! in Redis itself). This one is the classic iterative two-pointer glob:
//! every token other than `*` consumes exactly one byte, so on a mismatch it
//! is always enough to retry from the **most recent** `*`, letting that star
//! absorb one more byte. Earlier stars never need revisiting — any match
//! they could enable is also reachable by extending the latest star.
//! Worst case is `O(len(text) × len(pattern))` with no recursion and no
//! allocation; typical patterns are linear.

/// Whether `pattern` matches `text` under Redis glob semantics.
pub(crate) fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    if text.is_empty() {
        // Redis' `while (patternLen && stringLen)` never runs, so only the
        // empty pattern matches the empty string.
        return pattern.is_empty();
    }

    let mut pi = 0;
    let mut ti = 0;
    // Backtrack point: (pattern index just past the latest `*` run, text
    // index that star's match currently ends at).
    let mut star: Option<(usize, usize)> = None;

    while ti < text.len() {
        if pattern.get(pi) == Some(&b'*') {
            while pattern.get(pi) == Some(&b'*') {
                pi += 1;
            }
            if pi == pattern.len() {
                // Trailing star swallows the rest of the text.
                return true;
            }
            star = Some((pi, ti));
            continue;
        }

        if pi < pattern.len() {
            let (matched, next) = match_token(pattern, pi, text[ti]);
            if matched {
                pi = next;
                ti += 1;
                continue;
            }
        }

        // Mismatch (or pattern exhausted with text left): let the latest
        // star absorb one more byte and retry the rest from there.
        match star {
            Some((after_star, star_end)) => {
                pi = after_star;
                ti = star_end + 1;
                star = Some((after_star, ti));
            }
            None => return false,
        }
    }

    // Text consumed: whatever is left of the pattern must be stars.
    pattern[pi..].iter().all(|&b| b == b'*')
}

/// Match the single non-`*` token starting at `pattern[pi]` against byte
/// `c`. Returns whether it matched and the index of the next token.
///
/// The `[...]` parse is a transliteration of Redis' `case '['` so malformed
/// classes (`[`, `[^`, `[a-`, `[]`) behave identically.
fn match_token(pattern: &[u8], pi: usize, c: u8) -> (bool, usize) {
    match pattern[pi] {
        b'?' => (true, pi + 1),
        b'[' => {
            let len = pattern.len();
            let mut i = pi + 1;
            let negate = pattern.get(i) == Some(&b'^');
            if negate {
                i += 1;
            }
            let mut matched = false;
            loop {
                let remaining = len.saturating_sub(i);
                if remaining >= 2 && pattern[i] == b'\\' {
                    i += 1;
                    if pattern[i] == c {
                        matched = true;
                    }
                } else if remaining == 0 {
                    // Unterminated class: Redis steps back one byte so its
                    // trailing `pattern++` lands exactly on the end.
                    i = len - 1;
                    break;
                } else if pattern[i] == b']' {
                    break;
                } else if remaining >= 3 && pattern[i + 1] == b'-' {
                    let (mut lo, mut hi) = (pattern[i], pattern[i + 2]);
                    if lo > hi {
                        std::mem::swap(&mut lo, &mut hi);
                    }
                    i += 2;
                    if (lo..=hi).contains(&c) {
                        matched = true;
                    }
                } else if pattern[i] == c {
                    matched = true;
                }
                i += 1;
            }
            (matched != negate, i + 1)
        }
        b'\\' if pi + 1 < pattern.len() => (pattern[pi + 1] == c, pi + 2),
        lit => (lit == c, pi + 1),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::glob_match;

    fn m(pattern: &str, text: &str) -> bool {
        glob_match(pattern.as_bytes(), text.as_bytes())
    }

    /// Expected results are what Redis 7.x `stringmatchlen` returns for the
    /// same (pattern, string) pair; the comment on each odd case says why.
    #[test]
    fn redis_semantics_table() {
        let cases: &[(&str, &str, bool)] = &[
            // Literals and `?`.
            ("hello", "hello", true),
            ("hello", "hell", false),
            ("hello", "helloo", false),
            ("h?llo", "hello", true),
            ("h?llo", "hallo", true),
            ("h?llo", "hllo", false),
            ("h?llo", "heello", false),
            ("???", "abc", true),
            ("???", "ab", false),
            // `*`.
            ("*", "anything", true),
            ("h*llo", "hllo", true),
            ("h*llo", "heeeello", true),
            ("h*o", "ho", true),
            ("a*b*c", "abc", true),
            ("a*b*c", "aXXbYYc", true),
            ("a*b*c", "aXXbYY", false),
            ("a*", "a", true),
            ("a**", "a", true),
            ("**a**", "a", true),
            ("*a", "ba", true),
            ("*a", "ab", false),
            ("user:*", "user:123", true),
            ("user:*", "post:1", false),
            ("*:*:*", "a:b:c", true),
            ("*:*:*", "a:b", false),
            ("*ab", "aab", true),
            ("*aab", "aaab", true),
            // Empty string: only the empty pattern matches it (Redis never
            // enters its match loop with an empty string).
            ("", "", true),
            ("", "x", false),
            ("*", "", false),
            ("**", "", false),
            ("?", "", false),
            // Character classes.
            ("h[ae]llo", "hello", true),
            ("h[ae]llo", "hallo", true),
            ("h[ae]llo", "hillo", false),
            ("h[^e]llo", "hallo", true),
            ("h[^e]llo", "hello", false),
            ("h[a-b]llo", "hbllo", true),
            ("h[a-b]llo", "hcllo", false),
            ("[z-a]", "m", true), // reversed range is swapped
            ("[0-9][0-9]", "42", true),
            ("[0-9][0-9]", "4x", false),
            ("[^0-9]", "x", true),
            ("[^0-9]", "5", false),
            // `!` is not a negation marker in Redis.
            ("[!a]", "!", true),
            ("[!a]", "a", true),
            ("[!a]", "b", false),
            // `*`, `?` and `[` are literal inside a class.
            ("[*]", "*", true),
            ("[*]", "x", false),
            ("[?]", "?", true),
            ("[?]", "x", false),
            ("[[]", "[", true),
            // Escapes inside a class.
            ("[\\]]", "]", true),
            ("[\\-]", "-", true),
            ("[\\^]", "^", true),
            // `]` right after `[` closes an empty class: matches nothing...
            ("[]", "]", false),
            ("[]a", "a", false),
            // ...and `[^]` is the negated empty class: matches any byte.
            ("[^]", "x", true),
            ("[^]", "]", true),
            // Unterminated class runs to the end of the pattern.
            ("[abc", "b", true),
            ("[abc", "d", false),
            ("x[", "xa", false),
            ("x[^", "xa", true),
            // `[a-]` reads `a-]` as the range a..']' (swapped to ']'..'a'),
            // leaving the class unterminated — Redis quirk, preserved.
            ("[a-]", "^", true),
            ("[a-]", "a", true),
            ("[a-]", "b", false),
            // Escapes outside a class.
            ("\\*", "*", true),
            ("\\*", "x", false),
            ("\\?", "?", true),
            ("\\?", "x", false),
            ("\\[a]", "[a]", true),
            ("a\\", "a\\", true), // trailing lone `\` is a literal backslash
            ("\\\\", "\\", true),
            ("h\\ello", "hello", true),
            // Classes and stars together.
            ("*[0-9]", "abc7", true),
            ("*[0-9]", "abc", false),
            ("user:[0-9]*", "user:1abc", true),
            ("user:[0-9]*", "user:abc", false),
        ];
        for &(pattern, text, want) in cases {
            assert_eq!(m(pattern, text), want, "pattern {pattern:?} vs text {text:?}");
        }
    }

    #[test]
    fn matches_bytes_not_chars() {
        // "é" is two UTF-8 bytes; Redis' `?` matches one byte.
        assert!(!m("?", "é"));
        assert!(m("??", "é"));
        assert!(m("*", "é"));
        assert!(m("caf*", "café"));
    }

    /// Regression for the exponential matcher (KV stress report F-0): the
    /// old recursive implementation ran >30 s on this exact pair.
    #[test]
    fn pathological_pattern_completes_quickly() {
        let key = "a".repeat(60);
        let pattern = "*a*a*a*a*a*a*a*a*a*a*a*a*b";

        let start = Instant::now();
        for _ in 0..1_000 {
            assert!(!glob_match(pattern.as_bytes(), key.as_bytes()));
        }
        let elapsed = start.elapsed();
        // 1,000 matches of a 26-byte pattern against a 60-byte key is
        // ~1.5M byte steps at worst — microseconds of work. The bound is
        // three orders of magnitude loose so a slow CI box cannot flake.
        assert!(elapsed < Duration::from_secs(2), "1,000 pathological matches took {elapsed:?}");
    }

    /// The quadratic worst case for a two-pointer glob stays polynomial even
    /// with a much longer key and many more stars.
    #[test]
    fn many_stars_long_key_completes_quickly() {
        let key = "a".repeat(4_096);
        let pattern = format!("{}b", "*a".repeat(200));

        let start = Instant::now();
        assert!(!glob_match(pattern.as_bytes(), key.as_bytes()));
        // Same pattern, but now it can match: the key ends with `b`.
        let key_b = format!("{key}b");
        assert!(glob_match(pattern.as_bytes(), key_b.as_bytes()));
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }
}
