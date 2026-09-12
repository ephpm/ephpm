//! `wp-vuln` — known-vulnerability scan of installed WordPress components.
//!
//! The WordPress equivalent of `composer-audit`: `composer-audit` only ever
//! sees Composer dependencies, so it is blind to the real WordPress
//! compromise vector — outdated, exploitable wp.org plugins and themes. This
//! analyzer walks a WordPress tree, reads the *installed version* of every
//! plugin, theme, and core, and matches those `(type, slug, version)` triples
//! against an operator-supplied **vulnerability feed** (Wordfence
//! Intelligence). Each installed version that falls inside a known
//! vulnerability's affected-version range becomes a `SupplyChain` finding.
//!
//! **Native and offline, like `malware-yara` — not a subprocess.** There are
//! no network calls at scan time: the operator downloads and refreshes the
//! feed JSON out of band (a nightly cron, a CI artifact — see the reference
//! docs) and points [`crate::config::AnalyzersConfig::wp_vuln_feed`] at it.
//! This keeps a run deterministic and air-gap-friendly, exactly like the
//! `yara_rules` ruleset. Two prerequisites mirror `malware-yara`:
//!
//! - Feed **unset** → the analyzer *skips* (no feed, nothing to check).
//! - Feed **configured but the path is unreadable / unparseable** → a real
//!   error that gates the run (fail-closed) — an operator pointed at a feed
//!   that should have worked.
//!
//! A tree that is not WordPress at all (no `wp-content`, no
//! `wp-includes/version.php`) simply yields no findings — it is not an error.
//!
//! ## Feed schema (Wordfence Intelligence, verified)
//!
//! The feed is a JSON **object keyed by vulnerability UUID**; only the fields
//! this analyzer reads are modelled below (the feed carries many more). The
//! schema was pinned against the official `wordfence-cli` feed validator and
//! record parser (`wordfence/api/intelligence.py`,
//! `wordfence/intel/vulnerabilities.py`):
//!
//! ```json
//! {
//!   "b0d…uuid": {
//!     "id": "b0d…uuid",
//!     "title": "Example Plugin <= 1.2.3 - SQL Injection",
//!     "cve": "CVE-2024-1234",
//!     "cvss": { "vector": "…", "score": 9.8, "rating": "Critical" },
//!     "software": [{
//!       "type": "plugin",
//!       "slug": "example-plugin",
//!       "affected_versions": {
//!         "* - 1.2.3": {
//!           "from_version": "*",  "from_inclusive": true,
//!           "to_version": "1.2.3","to_inclusive": true
//!         }
//!       },
//!       "patched": true,
//!       "patched_versions": ["1.2.4"]
//!     }]
//!   }
//! }
//! ```
//!
//! The "not patched at/below the installed version" condition is encoded
//! *inside* the range: Wordfence's own scanner matches on range containment
//! alone (`Vulnerability.get_matched_software`), because the range's upper
//! bound is the last affected version — a patched install sorts above it and
//! falls out of range. This analyzer mirrors that exactly; `patched_versions`
//! is used only to tell the operator what to upgrade to.
//!
//! ## Version comparison
//!
//! WordPress version strings are **not** strict semver (`1.2`, `1.2.3-beta2`,
//! `2.0RC1`, `1.0.0-patched`). Range containment therefore needs a comparator
//! with PHP `version_compare` semantics. [`version::compare`] is a faithful
//! port of Wordfence's comparator (`wordfence/util/versioning.py`) — the same
//! algorithm the feed's ranges are authored against — so a range that
//! Wordfence considers to contain a version is judged the same way here.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::analyzer::{AnalysisCtx, Analyzer, AnalyzerError};
use crate::finding::{Category, Confidence, Finding, Severity};

/// See the module docs.
pub struct WpVuln;

/// The analyzer's stable id (and rule-id namespace prefix).
pub const ID: &str = "wp-vuln";

/// WordPress reads extension file headers from only the first 8 KiB of a
/// file; matching that bound keeps header parsing cheap and correct.
const HEADER_SCAN_BYTES: usize = 8 * 1024;

// --------------------------------------------------------------------------
// Feed model — only the fields this analyzer reads. Unknown keys are ignored
// on purpose (the feed is a large, evolving external document; a strict model
// would break every time Wordfence adds a field).
// --------------------------------------------------------------------------

/// One vulnerability record from the feed (object value, keyed by UUID).
#[derive(Debug, Deserialize)]
struct FeedRecord {
    /// Wordfence's own id for the record (JSON key `id`); the rule-id fallback
    /// when a record carries no CVE (e.g. the scanner feed).
    #[serde(default)]
    id: Option<String>,
    /// Human-readable title (e.g. `Foo <= 1.2 - Stored XSS`).
    #[serde(default)]
    title: Option<String>,
    /// CVE identifier when assigned. Absent in the scanner feed.
    #[serde(default)]
    cve: Option<String>,
    /// CVSS scoring. Absent in the scanner feed.
    #[serde(default)]
    cvss: Option<FeedCvss>,
    /// The affected software (plugins/themes/core) this record covers.
    #[serde(default)]
    software: Vec<FeedSoftware>,
    /// Non-security informational advisory — excluded from findings, matching
    /// the Wordfence scanner's default filter.
    #[serde(default)]
    informational: bool,
}

/// CVSS block — score plus qualitative rating.
#[derive(Debug, Deserialize)]
struct FeedCvss {
    /// Numeric CVSS base score (0.0–10.0). May be integer or float in JSON.
    #[serde(default)]
    score: Option<f64>,
    /// Qualitative rating (`Critical` / `High` / `Medium` / `Low` / `None`).
    #[serde(default)]
    rating: Option<String>,
}

/// One affected-software entry within a record.
#[derive(Debug, Deserialize)]
struct FeedSoftware {
    /// `core`, `plugin`, or `theme`.
    #[serde(rename = "type")]
    kind: String,
    /// wp.org slug (a plugin/theme directory name; `wordpress` for core).
    #[serde(default)]
    slug: String,
    /// Affected version ranges, keyed by a human label we do not read.
    #[serde(default)]
    affected_versions: HashMap<String, FeedVersionRange>,
    /// Versions that carry the fix — used only for the advisory message.
    #[serde(default)]
    patched_versions: Vec<String>,
}

/// A single affected-version range.
#[derive(Debug, Deserialize)]
struct FeedVersionRange {
    /// Lower bound, or `*` for "no lower bound".
    from_version: String,
    /// Whether `from_version` itself is affected.
    from_inclusive: bool,
    /// Upper bound, or `*` for "no upper bound".
    to_version: String,
    /// Whether `to_version` itself is affected.
    to_inclusive: bool,
}

impl FeedVersionRange {
    /// Whether `version` falls inside this range, using PHP-`version_compare`
    /// ordering. Mirrors `VersionRange.includes` in `wordfence-cli`.
    fn includes(&self, version: &str) -> bool {
        use std::cmp::Ordering;
        // Lower bound: unbounded, or from < version, or (inclusive and equal).
        if self.from_version != "*" {
            match version::compare(&self.from_version, version) {
                Ordering::Less => {}
                Ordering::Equal if self.from_inclusive => {}
                _ => return false,
            }
        }
        // Upper bound: unbounded, or to > version, or (inclusive and equal).
        if self.to_version != "*" {
            match version::compare(&self.to_version, version) {
                Ordering::Greater => {}
                Ordering::Equal if self.to_inclusive => {}
                _ => return false,
            }
        }
        true
    }
}

// --------------------------------------------------------------------------
// Installed-component discovery.
// --------------------------------------------------------------------------

/// What kind of WordPress component was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComponentKind {
    Plugin,
    Theme,
    Core,
}

impl ComponentKind {
    /// The feed's `software.type` string for this kind.
    fn feed_type(self) -> &'static str {
        match self {
            Self::Plugin => "plugin",
            Self::Theme => "theme",
            Self::Core => "core",
        }
    }

    /// The word used in a finding message.
    fn label(self) -> &'static str {
        self.feed_type()
    }
}

/// An installed component with the version we resolved and the file the
/// version came from (for the finding anchor).
#[derive(Debug)]
struct Component {
    kind: ComponentKind,
    slug: String,
    version: String,
    /// Tree-relative path to the file the version was read from.
    path: PathBuf,
}

/// Read at most the first [`HEADER_SCAN_BYTES`] of a file, lossily as UTF-8.
/// Header/version metadata always lives at the top of these files.
fn read_head(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut buf = Vec::with_capacity(HEADER_SCAN_BYTES);
    BufReader::new(file).take(HEADER_SCAN_BYTES as u64).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Extract a WordPress-style file header value (`Name: value`) from `text`,
/// tolerating comment leaders (` * `, `//`, `#`). Case-insensitive on the
/// field name; returns the trimmed value of the first match.
fn header_value(text: &str, field: &str) -> Option<String> {
    let needle = format!("{}:", field.to_ascii_lowercase());
    for line in text.lines() {
        let trimmed = line.trim_start_matches([' ', '\t', '*', '/', '#', '@']).trim_start();
        if trimmed.len() < needle.len() {
            continue;
        }
        if trimmed[..needle.len()].eq_ignore_ascii_case(&needle) {
            let value = trimmed[needle.len()..].trim();
            if !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    None
}

/// Find a plugin directory's main file: the `.php` whose header declares a
/// `Plugin Name:`. Returns `(main_file_path, version)` when both are present.
fn plugin_main(dir: &Path) -> Option<(PathBuf, String)> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("php")))
        .collect();
    // Deterministic order so a multi-file plugin resolves the same every run.
    entries.sort();
    for php in entries {
        let Some(head) = read_head(&php) else { continue };
        if header_value(&head, "Plugin Name").is_some() {
            if let Some(version) = header_value(&head, "Version") {
                return Some((php, version));
            }
            // A plugin with a name header but no version can't be matched.
            return None;
        }
    }
    None
}

/// Discover installed plugins, themes, and core under `root`. Returns an empty
/// vector for a non-WordPress tree.
fn discover(root: &Path) -> Vec<Component> {
    let mut out = Vec::new();
    let wp_content = root.join("wp-content");

    // Plugins: wp-content/plugins/<slug>/<main>.php, plus single-file plugins
    // living directly under plugins/.
    let plugins_dir = wp_content.join("plugins");
    if let Ok(entries) = std::fs::read_dir(&plugins_dir) {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                let slug = entry.file_name().to_string_lossy().into_owned();
                if let Some((main, version)) = plugin_main(&path) {
                    let rel = main.strip_prefix(root).unwrap_or(&main).to_path_buf();
                    out.push(Component { kind: ComponentKind::Plugin, slug, version, path: rel });
                }
            } else if ft.is_file()
                && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("php"))
            {
                // Single-file plugin: slug is the file stem.
                if let (Some(head), Some(stem)) =
                    (read_head(&path), path.file_stem().and_then(|s| s.to_str()))
                    && header_value(&head, "Plugin Name").is_some()
                    && let Some(version) = header_value(&head, "Version")
                {
                    let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                    out.push(Component {
                        kind: ComponentKind::Plugin,
                        slug: stem.to_owned(),
                        version,
                        path: rel,
                    });
                }
            }
        }
    }

    // Themes: wp-content/themes/<slug>/style.css.
    let themes_dir = wp_content.join("themes");
    if let Ok(entries) = std::fs::read_dir(&themes_dir) {
        for entry in entries.filter_map(Result::ok) {
            if !entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                continue;
            }
            let slug = entry.file_name().to_string_lossy().into_owned();
            let style = entry.path().join("style.css");
            if let Some(head) = read_head(&style)
                && let Some(version) = header_value(&head, "Version")
            {
                let rel = style.strip_prefix(root).unwrap_or(&style).to_path_buf();
                out.push(Component { kind: ComponentKind::Theme, slug, version, path: rel });
            }
        }
    }

    // Core: wp-includes/version.php -> $wp_version = '6.4.1';
    let version_php = root.join("wp-includes").join("version.php");
    if let Some(text) = read_head(&version_php)
        && let Some(version) = core_version(&text)
    {
        out.push(Component {
            kind: ComponentKind::Core,
            slug: "wordpress".to_owned(),
            version,
            path: PathBuf::from("wp-includes").join("version.php"),
        });
    }

    out
}

/// Parse `$wp_version = '6.4.1';` (single or double quotes) from
/// `wp-includes/version.php`.
fn core_version(text: &str) -> Option<String> {
    let idx = text.find("$wp_version")?;
    let rest = &text[idx + "$wp_version".len()..];
    let eq = rest.find('=')?;
    let after = rest[eq + 1..].trim_start();
    let quote = after.chars().next().filter(|c| *c == '\'' || *c == '"')?;
    let body = &after[1..];
    let end = body.find(quote)?;
    let value = body[..end].trim();
    (!value.is_empty()).then(|| value.to_owned())
}

// --------------------------------------------------------------------------
// Matching + finding construction.
// --------------------------------------------------------------------------

/// Map a feed record's CVSS to a [`Severity`]. Prefers the qualitative
/// rating, falls back to the numeric score's CVSS v3 bucket, and finally to
/// `Medium` — a known vulnerability of unknown severity is still worth at
/// least a medium (mirrors `composer-audit`'s treatment of a missing
/// severity, and covers the scanner feed which carries no CVSS at all).
fn severity_of(cvss: Option<&FeedCvss>) -> Severity {
    if let Some(cvss) = cvss {
        if let Some(rating) = &cvss.rating {
            match rating.to_ascii_lowercase().as_str() {
                "critical" => return Severity::Critical,
                "high" => return Severity::High,
                "medium" => return Severity::Medium,
                "low" => return Severity::Low,
                "none" => return Severity::Info,
                _ => {}
            }
        }
        if let Some(score) = cvss.score {
            return match score {
                s if s >= 9.0 => Severity::Critical,
                s if s >= 7.0 => Severity::High,
                s if s >= 4.0 => Severity::Medium,
                s if s > 0.0 => Severity::Low,
                _ => Severity::Info,
            };
        }
    }
    Severity::Medium
}

/// The identifier used in the rule id and message: the CVE when present, else
/// the feed record's own id, else a placeholder.
fn record_identifier(record: &FeedRecord) -> &str {
    record
        .cve
        .as_deref()
        .filter(|s| !s.is_empty())
        .or(record.id.as_deref())
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown")
}

/// Build the findings for every installed component that matches a feed
/// record. One finding per (component, matched record).
fn match_components(components: &[Component], feed: &HashMap<String, FeedRecord>) -> Vec<Finding> {
    let mut findings = Vec::new();
    for component in components {
        for record in feed.values() {
            if record.informational {
                continue;
            }
            let Some(software) = record
                .software
                .iter()
                .find(|sw| sw.kind == component.kind.feed_type() && sw.slug == component.slug)
            else {
                continue;
            };
            if !software.affected_versions.values().any(|r| r.includes(&component.version)) {
                continue;
            }
            findings.push(build_finding(component, record, software));
        }
    }
    findings
}

/// Construct a single finding for a confirmed component/record match.
fn build_finding(component: &Component, record: &FeedRecord, software: &FeedSoftware) -> Finding {
    let ident = record_identifier(record);
    let title = record.title.as_deref().unwrap_or("known vulnerability");
    let fixed = software
        .patched_versions
        .iter()
        .find(|v| !v.is_empty())
        .map_or_else(|| "no fixed version available".to_owned(), |v| format!("fixed in {v}"));
    Finding {
        rule_id: format!("{ID}/{}/{ident}", component.slug),
        severity: severity_of(record.cvss.as_ref()),
        category: Category::SupplyChain,
        path: Some(component.path.clone()),
        line: None,
        message: format!(
            "{kind} {slug} {version} — {ident}: {title}; {fixed}",
            kind = component.kind.label(),
            slug = component.slug,
            version = component.version,
        ),
        // An installed version matched against a published, versioned advisory
        // is a fact, not a heuristic — like composer-audit against a lockfile.
        confidence: Confidence::Confirmed,
    }
}

impl Analyzer for WpVuln {
    fn id(&self) -> &str {
        ID
    }

    fn category(&self) -> Category {
        Category::SupplyChain
    }

    fn run(&self, ctx: &AnalysisCtx) -> Result<Vec<Finding>, AnalyzerError> {
        let Some(feed_ref) = ctx.config().analyzers.wp_vuln_feed.as_ref() else {
            return Err(AnalyzerError::Skipped(
                "no vulnerability feed configured (set analyzers.wp_vuln_feed)".to_owned(),
            ));
        };
        let feed_path =
            if feed_ref.is_absolute() { feed_ref.clone() } else { ctx.root().join(feed_ref) };
        if !feed_path.exists() {
            // A configured-but-missing feed is an operator error on a path that
            // should have worked — fail closed, don't skip (mirrors malware-yara).
            return Err(AnalyzerError::ToolFailed(format!(
                "configured vulnerability feed {} does not exist",
                feed_path.display()
            )));
        }

        let components = discover(ctx.root());
        if components.is_empty() {
            // Not a WordPress tree — nothing to check, not an error.
            return Ok(Vec::new());
        }

        // The feed is a large external document, deliberately not run through
        // the per-file content cache: a cache hit could hide a newly published
        // advisory against an unchanged install (the same reasoning that keeps
        // composer-audit uncached).
        let file = File::open(&feed_path)
            .map_err(|e| AnalyzerError::ToolFailed(format!("cannot open feed: {e}")))?;
        let feed: HashMap<String, FeedRecord> = serde_json::from_reader(BufReader::new(file))
            .map_err(|e| {
                AnalyzerError::Parse(format!("vulnerability feed is not valid JSON: {e}"))
            })?;

        Ok(match_components(&components, &feed))
    }
}

/// PHP-`version_compare`-compatible version ordering, ported from Wordfence's
/// comparator (`wordfence/util/versioning.py`). The feed's affected-version
/// ranges are authored against these exact semantics, so mirroring them is
/// what makes range containment agree with Wordfence's own scanner.
mod version {
    use std::cmp::Ordering;

    /// `TIER_NUMBER` in the reference: numeric components sort above `dev` /
    /// `alpha` / `beta` / `rc` (tiers 2–5) and above unknown strings (tier 1),
    /// but below `pl` (tier 7).
    const TIER_NUMBER: i64 = 6;

    /// One parsed version component.
    struct Component {
        /// Numeric value when the token is all digits.
        number: Option<i64>,
        /// The raw token (for equality and same-tier string ordering).
        text: String,
        /// Ordering tier (see [`evaluate_tier`]).
        tier: i64,
    }

    /// Lower-tier alpha token → its tier index (offset by 2, matching the
    /// reference's `LOWER_ALPHA_VERSIONS` + `TIER_OFFSET`).
    fn lower_alpha_tier(token: &str) -> Option<i64> {
        // Reference indices 0..=3 plus `TIER_OFFSET` (2): dev=2, alpha=3,
        // beta=4, RC=5 — all below the numeric tier (6).
        match token {
            "dev" => Some(2),
            "alpha" | "a" => Some(3),
            "beta" | "b" => Some(4),
            "RC" | "rc" => Some(5),
            _ => None,
        }
    }

    /// Higher-tier alpha token (`pl`/`p`) → its tier (above numbers).
    fn higher_alpha_tier(token: &str) -> Option<i64> {
        match token {
            "pl" | "p" => Some(TIER_NUMBER + 1),
            _ => None,
        }
    }

    /// Classify a token into its ordering tier, matching
    /// `PhpVersionComponent._evaluate_tier`.
    fn evaluate_tier(token: &str, is_number: bool) -> i64 {
        if let Some(t) = lower_alpha_tier(token) {
            return t;
        }
        if is_number {
            return TIER_NUMBER;
        }
        if let Some(t) = higher_alpha_tier(token) {
            return t;
        }
        // Any other (non-numeric, non-special) string.
        1
    }

    impl Component {
        fn new(token: &str) -> Self {
            let is_number = !token.is_empty() && token.bytes().all(|b| b.is_ascii_digit());
            let number = if is_number { token.parse::<i64>().ok() } else { None };
            let tier = evaluate_tier(token, is_number && number.is_some());
            Self { number, text: token.to_owned(), tier }
        }

        /// The `DefaultComponent` (`PhpVersionComponent('0')`) used to pad the
        /// shorter of two versions: numeric zero at the numeric tier.
        fn default() -> Self {
            Self { number: Some(0), text: "0".to_owned(), tier: TIER_NUMBER }
        }
    }

    /// Split a version string into ordered components, matching
    /// `PhpVersion.extract_components`: normalize `_`/`-`/`+` to `.`, insert
    /// `.` around every run of non-digit/non-`.` characters, then split on `.`
    /// dropping empties.
    fn components(version: &str) -> Vec<Component> {
        // Normalize alternate delimiters.
        let normalized: String =
            version.chars().map(|c| if matches!(c, '_' | '-' | '+') { '.' } else { c }).collect();
        // Insert dots around maximal runs of "special" characters.
        let chars: Vec<char> = normalized.chars().collect();
        let mut delimited = String::with_capacity(chars.len() + 4);
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c != '.' && !c.is_ascii_digit() {
                delimited.push('.');
                while i < chars.len() && chars[i] != '.' && !chars[i].is_ascii_digit() {
                    delimited.push(chars[i]);
                    i += 1;
                }
                delimited.push('.');
            } else {
                delimited.push(c);
                i += 1;
            }
        }
        delimited.split('.').filter(|s| !s.is_empty()).map(Component::new).collect()
    }

    /// Compare two components, matching `compare_version_components`.
    fn compare_components(a: &Component, b: &Component) -> Ordering {
        let equal = match (a.number, b.number) {
            (Some(x), Some(y)) => x == y,
            _ => a.text == b.text,
        };
        if equal {
            return Ordering::Equal;
        }
        if a.tier != b.tier {
            return a.tier.cmp(&b.tier);
        }
        if a.tier == 0 || a.tier == TIER_NUMBER {
            return match (a.number, b.number) {
                (Some(x), Some(y)) => x.cmp(&y),
                _ => a.text.cmp(&b.text),
            };
        }
        Ordering::Equal
    }

    /// Compare two version strings with PHP `version_compare` semantics.
    /// Returns [`Ordering::Less`] when `a < b`.
    #[must_use]
    pub fn compare(a: &str, b: &str) -> Ordering {
        let a = components(a);
        let b = components(b);
        let count = a.len().max(b.len());
        let default = Component::default();
        for i in 0..count {
            let ca = a.get(i).unwrap_or(&default);
            let cb = b.get(i).unwrap_or(&default);
            let ord = compare_components(ca, cb);
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    }

    #[cfg(test)]
    mod tests {
        use std::cmp::Ordering;

        use super::compare;

        #[test]
        fn plain_dotted_numbers() {
            assert_eq!(compare("1.2.3", "1.2.4"), Ordering::Less);
            assert_eq!(compare("1.2.10", "1.2.9"), Ordering::Greater);
            assert_eq!(compare("1.2.3", "1.2.3"), Ordering::Equal);
            assert_eq!(compare("2.0", "1.9.9"), Ordering::Greater);
        }

        #[test]
        fn missing_trailing_components_default_to_zero() {
            // PHP: 1.0 == 1, 1.2 == 1.2.0.
            assert_eq!(compare("1.0", "1"), Ordering::Equal);
            assert_eq!(compare("1.2", "1.2.0"), Ordering::Equal);
            assert_eq!(compare("1.2.0", "1.2"), Ordering::Equal);
            // But 1.2.1 > 1.2.
            assert_eq!(compare("1.2.1", "1.2"), Ordering::Greater);
        }

        #[test]
        fn prerelease_suffixes_sort_below_release() {
            // dev < alpha < beta < RC < release < pl.
            assert_eq!(compare("1.0.0-dev", "1.0.0"), Ordering::Less);
            assert_eq!(compare("1.0.0-alpha", "1.0.0-beta"), Ordering::Less);
            assert_eq!(compare("1.0.0-beta", "1.0.0-rc"), Ordering::Less);
            assert_eq!(compare("1.0.0-rc", "1.0.0"), Ordering::Less);
            assert_eq!(compare("1.0.0", "1.0.0-pl"), Ordering::Less);
            assert_eq!(compare("2.0RC1", "2.0"), Ordering::Less);
        }

        #[test]
        fn suffix_abbreviations_are_equivalent() {
            // a == alpha, b == beta, rc == RC.
            assert_eq!(compare("1.0.0-a1", "1.0.0-alpha1"), Ordering::Equal);
            assert_eq!(compare("1.0.0-b", "1.0.0-beta"), Ordering::Equal);
            assert_eq!(compare("1.0.0RC", "1.0.0rc"), Ordering::Equal);
        }

        #[test]
        fn separators_are_normalized() {
            // '_', '-', '+' all fold to '.'.
            assert_eq!(compare("1.2.3", "1-2-3"), Ordering::Equal);
            assert_eq!(compare("1.2.3", "1_2_3"), Ordering::Equal);
            assert_eq!(compare("1.2.3", "1+2+3"), Ordering::Equal);
        }

        #[test]
        fn numbers_outrank_prerelease_strings() {
            // A numeric component sorts above a prerelease string at the same
            // position (release beats beta).
            assert_eq!(compare("1.0.1", "1.0.0-beta5"), Ordering::Greater);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn range(from: &str, from_inc: bool, to: &str, to_inc: bool) -> FeedVersionRange {
        FeedVersionRange {
            from_version: from.to_owned(),
            from_inclusive: from_inc,
            to_version: to.to_owned(),
            to_inclusive: to_inc,
        }
    }

    #[test]
    fn range_containment_boundaries() {
        // "* - 1.2.3" inclusive upper: everything up to and including 1.2.3.
        let r = range("*", true, "1.2.3", true);
        assert!(r.includes("1.0.0"));
        assert!(r.includes("1.2.3"));
        assert!(!r.includes("1.2.4"));

        // Exclusive upper bound drops the boundary version itself.
        let excl = range("*", true, "1.2.3", false);
        assert!(excl.includes("1.2.2"));
        assert!(!excl.includes("1.2.3"));

        // A bounded window, inclusive both ends.
        let win = range("2.0.0", true, "2.5.0", true);
        assert!(!win.includes("1.9.9"));
        assert!(win.includes("2.0.0"));
        assert!(win.includes("2.5.0"));
        assert!(!win.includes("2.5.1"));

        // Exclusive lower bound.
        let low = range("2.0.0", false, "2.5.0", true);
        assert!(!low.includes("2.0.0"));
        assert!(low.includes("2.0.1"));
    }

    #[test]
    fn severity_from_rating_then_score_then_default() {
        assert_eq!(
            severity_of(Some(&FeedCvss { score: Some(9.8), rating: Some("Critical".into()) })),
            Severity::Critical
        );
        // Rating wins even if score would bucket differently.
        assert_eq!(
            severity_of(Some(&FeedCvss { score: Some(2.0), rating: Some("High".into()) })),
            Severity::High
        );
        // No rating -> score bucket.
        assert_eq!(severity_of(Some(&FeedCvss { score: Some(7.5), rating: None })), Severity::High);
        assert_eq!(
            severity_of(Some(&FeedCvss { score: Some(5.0), rating: None })),
            Severity::Medium
        );
        // No CVSS at all (scanner feed) -> medium floor.
        assert_eq!(severity_of(None), Severity::Medium);
    }

    /// A small feed with two records for `example-plugin`: one affecting
    /// `<= 1.2.3`, one already-patched window that our installed version is
    /// above.
    fn sample_feed() -> HashMap<String, FeedRecord> {
        serde_json::from_str(SAMPLE_FEED_JSON).expect("fixture feed parses")
    }

    const SAMPLE_FEED_JSON: &str = r#"{
      "11111111-1111-1111-1111-111111111111": {
        "id": "11111111-1111-1111-1111-111111111111",
        "title": "Example Plugin <= 1.2.3 - SQL Injection",
        "cve": "CVE-2024-1234",
        "cvss": { "vector": "CVSS:3.1/AV:N", "score": 9.8, "rating": "Critical" },
        "software": [{
          "type": "plugin",
          "name": "Example Plugin",
          "slug": "example-plugin",
          "affected_versions": {
            "* - 1.2.3": {
              "from_version": "*", "from_inclusive": true,
              "to_version": "1.2.3", "to_inclusive": true
            }
          },
          "patched": true,
          "patched_versions": ["1.2.4"]
        }]
      },
      "22222222-2222-2222-2222-222222222222": {
        "id": "22222222-2222-2222-2222-222222222222",
        "title": "Example Plugin 0.9 - 1.0 - XSS (old, already patched)",
        "cve": "CVE-2020-0001",
        "cvss": { "vector": "CVSS:3.1/AV:N", "score": 6.1, "rating": "Medium" },
        "software": [{
          "type": "plugin",
          "name": "Example Plugin",
          "slug": "example-plugin",
          "affected_versions": {
            "0.9 - 1.0": {
              "from_version": "0.9", "from_inclusive": true,
              "to_version": "1.0", "to_inclusive": true
            }
          },
          "patched": true,
          "patched_versions": ["1.0.1"]
        }]
      },
      "33333333-3333-3333-3333-333333333333": {
        "id": "33333333-3333-3333-3333-333333333333",
        "title": "Informational note",
        "informational": true,
        "software": [{
          "type": "plugin", "name": "Example Plugin", "slug": "example-plugin",
          "affected_versions": {
            "* - 9.9": { "from_version": "*", "from_inclusive": true,
                         "to_version": "9.9", "to_inclusive": true }
          },
          "patched": false, "patched_versions": []
        }]
      }
    }"#;

    #[test]
    fn feed_parses_to_expected_shape() {
        let feed = sample_feed();
        assert_eq!(feed.len(), 3);
        let rec = &feed["11111111-1111-1111-1111-111111111111"];
        assert_eq!(rec.cve.as_deref(), Some("CVE-2024-1234"));
        assert_eq!(rec.software[0].kind, "plugin");
        assert_eq!(rec.software[0].slug, "example-plugin");
        assert_eq!(rec.software[0].patched_versions, vec!["1.2.4".to_owned()]);
    }

    #[test]
    fn matches_vulnerable_version_not_patched_one() {
        let feed = sample_feed();
        // Installed 1.2.0: inside the "<= 1.2.3" range, above the old "0.9-1.0"
        // window, and the informational record must be excluded.
        let components = vec![Component {
            kind: ComponentKind::Plugin,
            slug: "example-plugin".to_owned(),
            version: "1.2.0".to_owned(),
            path: Path::new("wp-content/plugins/example-plugin/example-plugin.php").to_path_buf(),
        }];
        let findings = match_components(&components, &feed);
        assert_eq!(findings.len(), 1, "only the current, non-informational CVE should match");
        let f = &findings[0];
        assert_eq!(f.rule_id, "wp-vuln/example-plugin/CVE-2024-1234");
        assert_eq!(f.severity, Severity::Critical);
        assert_eq!(f.category, Category::SupplyChain);
        assert_eq!(f.confidence, Confidence::Confirmed);
        assert!(f.message.contains("plugin example-plugin 1.2.0"));
        assert!(f.message.contains("CVE-2024-1234"));
        assert!(f.message.contains("fixed in 1.2.4"));
    }

    #[test]
    fn patched_version_produces_no_finding() {
        let feed = sample_feed();
        // Installed 1.2.4: above every affected range -> clean.
        let components = vec![Component {
            kind: ComponentKind::Plugin,
            slug: "example-plugin".to_owned(),
            version: "1.2.4".to_owned(),
            path: Path::new("wp-content/plugins/example-plugin/example-plugin.php").to_path_buf(),
        }];
        assert!(match_components(&components, &feed).is_empty());
    }

    #[test]
    fn unrelated_slug_does_not_match() {
        let feed = sample_feed();
        let components = vec![Component {
            kind: ComponentKind::Plugin,
            slug: "some-other-plugin".to_owned(),
            version: "1.0.0".to_owned(),
            path: Path::new("wp-content/plugins/some-other-plugin/x.php").to_path_buf(),
        }];
        assert!(match_components(&components, &feed).is_empty());
    }

    #[test]
    fn theme_type_is_distinct_from_plugin() {
        let feed = sample_feed();
        // Same slug but type "theme" must not match a "plugin" record.
        let components = vec![Component {
            kind: ComponentKind::Theme,
            slug: "example-plugin".to_owned(),
            version: "1.2.0".to_owned(),
            path: Path::new("wp-content/themes/example-plugin/style.css").to_path_buf(),
        }];
        assert!(match_components(&components, &feed).is_empty());
    }

    // ---- discovery / header parsing ----

    #[test]
    fn parses_plugin_and_theme_and_core_headers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Plugin with a proper main-file header.
        let plugin = root.join("wp-content/plugins/example-plugin");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join("example-plugin.php"),
            "<?php\n/*\n * Plugin Name: Example Plugin\n * Version: 1.2.0\n */\n",
        )
        .unwrap();
        // A non-main php file in the same dir must be ignored.
        std::fs::write(plugin.join("helper.php"), "<?php // no header\n").unwrap();

        // Theme with a style.css header.
        let theme = root.join("wp-content/themes/example-theme");
        std::fs::create_dir_all(&theme).unwrap();
        std::fs::write(
            theme.join("style.css"),
            "/*\nTheme Name: Example Theme\nVersion: 3.4.5\n*/\n",
        )
        .unwrap();

        // Core version file.
        let inc = root.join("wp-includes");
        std::fs::create_dir_all(&inc).unwrap();
        std::fs::write(inc.join("version.php"), "<?php\n$wp_version = '6.4.1';\n").unwrap();

        let mut found = discover(root);
        found.sort_by(|a, b| a.slug.cmp(&b.slug));
        assert_eq!(found.len(), 3);

        let core = found.iter().find(|c| c.kind == ComponentKind::Core).unwrap();
        assert_eq!(core.slug, "wordpress");
        assert_eq!(core.version, "6.4.1");
        assert_eq!(core.path, Path::new("wp-includes/version.php"));

        let plugin = found.iter().find(|c| c.kind == ComponentKind::Plugin).unwrap();
        assert_eq!(plugin.slug, "example-plugin");
        assert_eq!(plugin.version, "1.2.0");

        let theme = found.iter().find(|c| c.kind == ComponentKind::Theme).unwrap();
        assert_eq!(theme.slug, "example-theme");
        assert_eq!(theme.version, "3.4.5");
    }

    #[test]
    fn non_wordpress_tree_discovers_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.php"), "<?php echo 1;\n").unwrap();
        assert!(discover(dir.path()).is_empty());
    }

    #[test]
    fn core_version_parses_both_quote_styles() {
        assert_eq!(core_version("$wp_version = '6.4.1';").as_deref(), Some("6.4.1"));
        assert_eq!(core_version("$wp_version = \"6.5\";").as_deref(), Some("6.5"));
        assert_eq!(core_version("no version here"), None);
    }

    #[test]
    fn header_value_tolerates_comment_leaders() {
        assert_eq!(header_value(" * Version: 1.2.3", "Version").as_deref(), Some("1.2.3"));
        assert_eq!(header_value("Version:1.0", "version").as_deref(), Some("1.0"));
        assert_eq!(header_value("// Plugin Name: Foo", "Plugin Name").as_deref(), Some("Foo"));
        assert_eq!(header_value("Description: x", "Version"), None);
    }

    // ---- full Analyzer::run path ----

    use crate::config::AnalyzeConfig;

    /// Build a config with `wp_vuln_feed` pointed at `feed` (relative to root).
    fn config_with_feed(feed: Option<&str>) -> AnalyzeConfig {
        let mut config = AnalyzeConfig::default();
        config.analyzers.wp_vuln_feed = feed.map(std::path::PathBuf::from);
        config
    }

    /// Write a minimal WordPress tree (one vulnerable plugin) under `root`.
    fn write_wp_tree(root: &Path, plugin_version: &str) {
        let plugin = root.join("wp-content/plugins/example-plugin");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join("example-plugin.php"),
            format!(
                "<?php\n/*\n * Plugin Name: Example Plugin\n * Version: {plugin_version}\n */\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn run_skips_when_feed_unset() {
        let dir = tempfile::tempdir().unwrap();
        write_wp_tree(dir.path(), "1.2.0");
        let ctx = AnalysisCtx::new(dir.path().to_path_buf(), config_with_feed(None));
        let err = WpVuln.run(&ctx).expect_err("no feed should skip");
        assert!(err.is_skip(), "expected Skipped, got {err:?}");
    }

    #[test]
    fn run_errors_when_feed_configured_but_missing() {
        let dir = tempfile::tempdir().unwrap();
        write_wp_tree(dir.path(), "1.2.0");
        let ctx = AnalysisCtx::new(dir.path().to_path_buf(), config_with_feed(Some("nope.json")));
        let err = WpVuln.run(&ctx).expect_err("missing feed should gate");
        assert!(!err.is_skip(), "a configured-but-missing feed must gate, not skip");
        assert!(format!("{err}").contains("does not exist"));
    }

    #[test]
    fn run_errors_on_garbage_feed() {
        let dir = tempfile::tempdir().unwrap();
        write_wp_tree(dir.path(), "1.2.0");
        std::fs::write(dir.path().join("feed.json"), "this is not json").unwrap();
        let ctx = AnalysisCtx::new(dir.path().to_path_buf(), config_with_feed(Some("feed.json")));
        let err = WpVuln.run(&ctx).expect_err("garbage feed should gate");
        assert!(matches!(err, AnalyzerError::Parse(_)), "expected Parse, got {err:?}");
    }

    #[test]
    fn run_on_non_wordpress_tree_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.php"), "<?php echo 1;\n").unwrap();
        // A valid feed is present, but there is no WP tree to scan.
        std::fs::write(dir.path().join("feed.json"), SAMPLE_FEED_JSON).unwrap();
        let ctx = AnalysisCtx::new(dir.path().to_path_buf(), config_with_feed(Some("feed.json")));
        let findings = WpVuln.run(&ctx).expect("non-WP tree is not an error");
        assert!(findings.is_empty());
    }

    #[test]
    fn run_end_to_end_flags_the_vulnerable_plugin() {
        let dir = tempfile::tempdir().unwrap();
        // Vulnerable: 1.2.0 is inside "* - 1.2.3".
        write_wp_tree(dir.path(), "1.2.0");
        // A second, patched plugin that must NOT produce a finding.
        let patched = dir.path().join("wp-content/plugins/patched-plugin");
        std::fs::create_dir_all(&patched).unwrap();
        std::fs::write(
            patched.join("patched-plugin.php"),
            "<?php\n/*\n * Plugin Name: Example Plugin\n * Version: 1.2.4\n */\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("feed.json"), SAMPLE_FEED_JSON).unwrap();

        let ctx = AnalysisCtx::new(dir.path().to_path_buf(), config_with_feed(Some("feed.json")));
        let findings = WpVuln.run(&ctx).expect("run succeeds");
        // The patched-plugin dir has slug "patched-plugin" (not in the feed),
        // so only the vulnerable example-plugin matches.
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.rule_id, "wp-vuln/example-plugin/CVE-2024-1234");
        assert_eq!(f.severity, Severity::Critical);
        assert_eq!(f.confidence, Confidence::Confirmed);
        assert_eq!(
            f.path.as_deref(),
            Some(Path::new("wp-content/plugins/example-plugin/example-plugin.php"))
        );
    }
}
