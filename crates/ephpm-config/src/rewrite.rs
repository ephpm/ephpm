//! Per-site rewrite rules (`[[rewrite]]`) — an ordered list of
//! `regex → target` rules with capture-group substitution and query-string
//! output, evaluated at routing time **before** the `fallback` chain.
//!
//! This is what lets ePHPm host a legacy `.htaccess` PHP app (many top-level
//! scripts plus "pretty" URLs) rather than only a single-front-controller
//! framework. The motivating case is **Sendy**, whose click/open/unsubscribe
//! links are multi-segment (`/l/<sub>/<link>/<campaign>`) and whose scripts
//! read `$_GET['i']` after splitting on `/`:
//!
//! ```apache
//! RewriteRule ^l/([a-zA-Z0-9/]+)$ l.php?i=$1 [L]
//! ```
//!
//! becomes
//!
//! ```toml
//! [[rewrite]]
//! regex = "^/l/([a-zA-Z0-9/]+)$"
//! to    = "/l.php?i=${1}"
//! ```
//!
//! # Capture-substitution syntax
//!
//! `to` is expanded with the [`regex`] crate's own replacement syntax:
//! `${1}`..`${9}` (and the unbraced `$1`..`$9`, and `$name` for a named
//! group). The **braced** form `${N}` is the documented, recommended spelling
//! because it is unambiguous when a literal digit follows a group reference
//! (`${1}0` is "group 1 then the character `0`", whereas `$10` means group 10).
//! `$$` is a literal `$`. At least nine groups are supported (the engine has no
//! lower cap).
//!
//! # Query output
//!
//! `to` is split on its **first** `?` at compile time: the part before is the
//! rewritten **path**, the part after is the rewritten **query string**. The
//! query *replaces* the original (Apache's default; not `[QSA]`). The router
//! hands the rewritten query to PHP as `QUERY_STRING` verbatim, so a captured
//! value keeps its slashes — `$_GET['i'] === "AAA/BBB/CCC"` — instead of being
//! percent-encoded the way a path-regex middleware (Traefik `ReplacePathRegex`)
//! would. This is the whole point of the feature; see the reference docs.
//!
//! # Fail-closed validation
//!
//! Compilation happens at startup ([`compile_rules`]). A rule is rejected,
//! naming it, when:
//!
//! * the `regex` does not compile, or compiles larger than
//!   [`REGEX_SIZE_LIMIT`];
//! * the `to` path (before `?`) is not a root-relative path, or its **literal**
//!   (non-capture) text escapes the document root (`..`, a backslash, or a NUL);
//! * the list holds more than [`MAX_RULES`] rules.
//!
//! For `[server] rewrite` (single-site) a compile failure is a hard startup
//! error from `Config::validate`. For a per-site override file it takes that
//! **one** site out of service (503), the same fail-closed-per-site contract
//! the rest of the override file follows.
//!
//! # ReDoS
//!
//! The `regex` crate's engine runs in time linear in the input length and has
//! no backtracking, so no operator-authored pattern can be a
//! catastrophic-backtracking vector on the per-request path. The two remaining
//! costs — compiled program size and rule count — are bounded by
//! [`REGEX_SIZE_LIMIT`] and [`MAX_RULES`]. Matching allocates nothing until a
//! rule actually matches.

use regex::Regex;
use serde::Deserialize;

/// Maximum number of `[[rewrite]]` rules in one list.
///
/// Rules are evaluated in order on every request until one matches, so the list
/// length is a direct per-request cost. Sixty-four is far above any realistic
/// `.htaccess` port (Sendy needs five) and keeps a pathological config from
/// turning the hot path into a long linear scan.
pub const MAX_RULES: usize = 64;

/// Upper bound on a single compiled regex program, in bytes.
///
/// The engine is linear-time, so this bounds *compile* memory/time rather than
/// match time. One MiB is generous for the anchored, small patterns these rules
/// are (`^/l/([a-zA-Z0-9/]+)$`) and refuses a pattern crafted to blow up the
/// compiled program.
pub const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// One rewrite rule as written in config — raw strings, before compilation.
///
/// Used for both `[server] rewrite` (single-site) and the per-site override
/// file's `[[rewrite]]`. `deny_unknown_fields` so a typo'd key (`regexp`,
/// `too`) fails the load naming it, consistent with every config section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RewriteRule {
    /// The regex matched against the request's (percent-decoded) path. Not
    /// implicitly anchored — the operator writes `^…$` as in `.htaccess`.
    pub regex: String,
    /// The rewrite target: a root-relative path, optionally followed by `?` and
    /// a query template. `${1}`..`${9}` are substituted from the regex's
    /// capture groups.
    pub to: String,
}

/// A compiled, validated rewrite rule ready for the request path.
///
/// Not `PartialEq`/`Eq` (a [`Regex`] is neither), so it is carried separately
/// from the raw [`RewriteRule`] the override reader keeps for diffing.
#[derive(Debug, Clone)]
pub struct CompiledRewrite {
    regex: Regex,
    /// The `to` path template (everything before the first `?`), e.g.
    /// `/l.php`. Expanded with the match's captures at request time.
    to_path: String,
    /// The `to` query template (everything after the first `?`), e.g.
    /// `i=${1}`. `None` when `to` has no `?` — then the rewrite clears the
    /// query string, matching Apache's behaviour for a target with no query.
    to_query: Option<String>,
}

/// The result of running a request path through a [`RewriteSet`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewriteOutcome {
    /// No rule matched — route exactly as if the feature were off.
    NoMatch,
    /// A rule matched. `path` is the rewritten, containment-checked path;
    /// `query` is the rewritten query string (empty string = present but
    /// empty; the original query is always replaced, never appended).
    Rewrite {
        /// Rewritten path (`/l.php`), used for script resolution and
        /// `SCRIPT_NAME`/`SCRIPT_FILENAME`.
        path: String,
        /// Rewritten query string (`i=AAA/BBB/CCC`), delivered to PHP as
        /// `QUERY_STRING` verbatim — slashes preserved, not re-encoded.
        query: String,
    },
    /// A rule matched but its expanded **path** failed containment (a capture
    /// substituted into the path produced a `..` segment, a NUL, or a
    /// backslash). The caller must refuse the request (`403`) rather than route
    /// it — first-match-wins (`[L]`) means no later rule is tried.
    Unsafe,
}

/// A compiled, ordered list of rewrite rules for one site (or the whole server
/// in single-site mode).
#[derive(Debug, Clone, Default)]
pub struct RewriteSet {
    rules: Vec<CompiledRewrite>,
}

impl RewriteSet {
    /// `true` when there are no rules — the common case, in which routing is
    /// byte-identical to a build without this feature.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Number of compiled rules.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Evaluate `path` against the rules in order, first-match-wins (`[L]`).
    ///
    /// `path` is the request's percent-decoded path with no query string.
    /// Returns the first matching rule's outcome, or [`RewriteOutcome::NoMatch`]
    /// when none match.
    #[must_use]
    pub fn apply(&self, path: &str) -> RewriteOutcome {
        for rule in &self.rules {
            if let Some(caps) = rule.regex.captures(path) {
                // Expand the path and query templates separately so a `?` in a
                // captured value can never be mistaken for the path/query split
                // (that split was decided from the literal `to` at compile
                // time).
                let mut new_path = String::new();
                caps.expand(&rule.to_path, &mut new_path);

                if !path_is_contained(&new_path) {
                    // The matched rule won (`[L]`); its path escaped. Refuse
                    // rather than fall through to a later rule or to normal
                    // routing with the original path.
                    return RewriteOutcome::Unsafe;
                }

                let query = rule.to_query.as_ref().map_or_else(String::new, |q| {
                    let mut out = String::new();
                    caps.expand(q, &mut out);
                    out
                });

                return RewriteOutcome::Rewrite { path: new_path, query };
            }
        }
        RewriteOutcome::NoMatch
    }
}

/// Compile and validate an ordered list of raw [`RewriteRule`]s.
///
/// # Errors
///
/// Returns a human-readable message naming the offending rule when the list is
/// too long, a `regex` does not compile (or is too large), or a `to` target is
/// malformed / escapes the document root. The caller decides whether that is a
/// hard startup error (`[server] rewrite`) or a per-site `unusable` (the
/// override file).
pub fn compile_rules(rules: &[RewriteRule]) -> Result<RewriteSet, String> {
    if rules.len() > MAX_RULES {
        return Err(format!(
            "too many [[rewrite]] rules ({}, max {MAX_RULES}) — a long list is evaluated in \
             order on every request",
            rules.len()
        ));
    }

    let mut compiled = Vec::with_capacity(rules.len());
    for (idx, rule) in rules.iter().enumerate() {
        compiled.push(compile_one(idx, rule)?);
    }
    Ok(RewriteSet { rules: compiled })
}

/// Compile one rule, naming it (1-based index + its regex) in any error.
fn compile_one(idx: usize, rule: &RewriteRule) -> Result<CompiledRewrite, String> {
    let name = || format!("[[rewrite]] #{} (regex = {:?})", idx + 1, rule.regex);

    let regex = regex::RegexBuilder::new(&rule.regex)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|e| format!("{}: regex does not compile: {e}", name()))?;

    // Split `to` into path + query on the FIRST `?`. Everything after it is the
    // query template; the query replaces (never appends to) the original.
    let (to_path, to_query) = match rule.to.split_once('?') {
        Some((p, q)) => (p.to_string(), Some(q.to_string())),
        None => (rule.to.clone(), None),
    };

    validate_to_path(&to_path).map_err(|e| format!("{}: {e}", name()))?;

    Ok(CompiledRewrite { regex, to_path, to_query })
}

/// Validate the **literal** text of a `to` path template at compile time.
///
/// Capture placeholders (`${1}`, `$1`, `$name`) are not knowable until a
/// request matches, so this checks only what is fixed in the template: it must
/// be a root-relative path, with no backslash, no NUL, and no `..` segment in
/// the literal text. A path that contains a capture is additionally re-checked
/// after expansion at request time (see [`path_is_contained`]).
fn validate_to_path(to_path: &str) -> Result<(), String> {
    if to_path.is_empty() || !to_path.starts_with('/') {
        return Err(format!(
            "`to` target path must be root-relative (start with `/`), got {to_path:?}"
        ));
    }
    if to_path.contains('\0') {
        return Err("`to` target path contains a NUL byte".to_string());
    }
    if to_path.contains('\\') {
        return Err("`to` target path contains a backslash — use `/` as the separator".to_string());
    }
    // A literal `..` segment escapes the document root regardless of any
    // capture, so reject it up front with a clear message.
    if to_path.split('/').any(|seg| seg == "..") {
        return Err(format!(
            "`to` target path escapes the document root with a `..` segment: {to_path:?}"
        ));
    }
    Ok(())
}

/// Whether an **expanded** rewrite path is safe to resolve: root-relative, no
/// NUL, no backslash, and no `..` segment.
///
/// Runs at request time on the post-substitution path, so a capture that
/// introduces a traversal segment is caught even though the literal template
/// passed [`validate_to_path`]. A decoded request path can legitimately contain
/// `/`, so slashes are allowed; `..` as a whole segment is not.
fn path_is_contained(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains('\0')
        && !path.contains('\\')
        && !path.split('/').any(|seg| seg == "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(regex: &str, to: &str) -> RewriteRule {
        RewriteRule { regex: regex.to_string(), to: to.to_string() }
    }

    /// The five Sendy capture rules — the acceptance fixture.
    fn sendy() -> RewriteSet {
        compile_rules(&[
            rule("^/l/([a-zA-Z0-9/]+)$", "/l.php?i=${1}"),
            rule("^/t/([a-zA-Z0-9/]+)$", "/t.php?i=${1}"),
            rule("^/w/([a-zA-Z0-9/]+)$", "/w.php?i=${1}"),
            rule("^/unsubscribe/(.*)$", "/unsubscribe.php?i=${1}"),
            rule("^/subscribe/(.*)$", "/subscribe.php?i=${1}"),
        ])
        .expect("Sendy rules must compile")
    }

    #[test]
    fn multi_segment_capture_preserves_slashes_in_query() {
        let set = sendy();
        assert_eq!(
            set.apply("/l/AAA/BBB/CCC"),
            RewriteOutcome::Rewrite { path: "/l.php".into(), query: "i=AAA/BBB/CCC".into() },
            "the captured value keeps its slashes and lands in the query"
        );
    }

    #[test]
    fn each_sendy_rule_rewrites_its_prefix() {
        let set = sendy();
        assert_eq!(
            set.apply("/t/AA/BB"),
            RewriteOutcome::Rewrite { path: "/t.php".into(), query: "i=AA/BB".into() }
        );
        assert_eq!(
            set.apply("/w/AA/BB/CC"),
            RewriteOutcome::Rewrite { path: "/w.php".into(), query: "i=AA/BB/CC".into() }
        );
        assert_eq!(
            set.apply("/unsubscribe/AA/BB/CC"),
            RewriteOutcome::Rewrite { path: "/unsubscribe.php".into(), query: "i=AA/BB/CC".into() }
        );
        assert_eq!(
            set.apply("/subscribe/x"),
            RewriteOutcome::Rewrite { path: "/subscribe.php".into(), query: "i=x".into() }
        );
    }

    #[test]
    fn first_match_wins() {
        // Two rules that both match `/a/b`; the first must win (`[L]`).
        let set = compile_rules(&[
            rule("^/a/(.*)$", "/first.php?x=${1}"),
            rule("^/a/(.*)$", "/second.php?x=${1}"),
        ])
        .unwrap();
        assert_eq!(
            set.apply("/a/b"),
            RewriteOutcome::Rewrite { path: "/first.php".into(), query: "x=b".into() }
        );
    }

    #[test]
    fn no_match_is_passthrough() {
        let set = sendy();
        for p in ["/", "/css/app.css", "/login", "/index.php", "/l", "/subscribe"] {
            assert_eq!(set.apply(p), RewriteOutcome::NoMatch, "{p} must not match a Sendy rule");
        }
    }

    #[test]
    fn target_with_no_query_clears_the_query_string() {
        let set = compile_rules(&[rule("^/clean/(.*)$", "/clean.php")]).unwrap();
        assert_eq!(
            set.apply("/clean/foo"),
            RewriteOutcome::Rewrite { path: "/clean.php".into(), query: String::new() }
        );
    }

    #[test]
    fn braced_group_disambiguates_a_following_digit() {
        let set = compile_rules(&[rule("^/x/([a-z]+)$", "/x.php?v=${1}0")]).unwrap();
        assert_eq!(
            set.apply("/x/ab"),
            RewriteOutcome::Rewrite { path: "/x.php".into(), query: "v=ab0".into() },
            "${{1}}0 is group 1 followed by a literal 0"
        );
    }

    #[test]
    fn malformed_regex_is_rejected_naming_the_rule() {
        let err = compile_rules(&[rule("^/l/([a-z", "/l.php?i=${1}")]).unwrap_err();
        assert!(err.contains("#1"), "error must name the rule: {err}");
        assert!(err.contains("regex does not compile"), "error must say why: {err}");
    }

    #[test]
    fn to_path_escaping_the_docroot_is_rejected_at_compile_time() {
        for bad in ["../etc/passwd.php", "/../secrets.php", "/a/../../b.php", r"/a\b.php"] {
            assert!(
                compile_rules(&[rule("^/x/(.*)$", bad)]).is_err(),
                "to = {bad:?} escapes the document root and must be rejected at compile time"
            );
        }
    }

    #[test]
    fn to_path_must_be_root_relative() {
        assert!(compile_rules(&[rule("^/x/(.*)$", "relative.php?i=${1}")]).is_err());
        assert!(compile_rules(&[rule("^/x/(.*)$", "")]).is_err());
    }

    /// A capture substituted into the **path** that produces a `..` segment is
    /// caught at request time even though the literal template was clean.
    #[test]
    fn capture_producing_a_traversal_path_is_unsafe_at_request_time() {
        let set = compile_rules(&[rule("^/p/(.*)$", "/${1}.php")]).unwrap();
        assert_eq!(set.apply("/p/../../etc/passwd"), RewriteOutcome::Unsafe);
        // A clean capture through the same rule is fine.
        assert_eq!(
            set.apply("/p/about"),
            RewriteOutcome::Rewrite { path: "/about.php".into(), query: String::new() }
        );
    }

    #[test]
    fn rule_count_is_bounded() {
        let many: Vec<RewriteRule> =
            (0..=MAX_RULES).map(|i| rule(&format!("^/r{i}/(.*)$"), "/r.php?i=${1}")).collect();
        let err = compile_rules(&many).unwrap_err();
        assert!(err.contains("too many"), "over-long list must be refused: {err}");
    }

    #[test]
    fn empty_set_applies_cleanly() {
        let set = RewriteSet::default();
        assert!(set.is_empty());
        assert_eq!(set.apply("/anything"), RewriteOutcome::NoMatch);
    }
}
