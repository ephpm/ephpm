//! `.ephpm-analyze.yml` — the golangci-lint-shaped configuration file.
//!
//! Every section is `#[serde(deny_unknown_fields)]`, matching the workspace's
//! strict-config discipline: an unknown key fails the run naming the key
//! instead of silently doing nothing (the #429 lesson — a knob an operator
//! set must never be a no-op). Unlike `ephpm.toml` there is no environment
//! layer feeding this file, so the **root** is strict too.
//!
//! Every struct here also derives `Serialize`: the incremental cache
//! (`crate::cache`) keys entries on a hash of the *effective* configuration,
//! so any config change — file or CLI override — invalidates cached results.
//!
//! ```yaml
//! profile: security          # security | none
//! level: 1                   # 0..3 strictness (see crate::policy)
//! fail_on: quarantine        # allow | quarantine | deny
//! output: text               # text | sarif
//! baseline: .ephpm-analyze-baseline.json   # suppress known findings
//! since: origin/main         # diff-aware: only files changed vs this ref
//! engine:                    # Phase 4 — parsed but inert today
//!   detonate: false
//!   timeout_ms: 30000
//! analyzers:
//!   enable: [composer-audit, semgrep-php, malware-yara, dangerous-sinks, suppression-scan]
//!   required: [composer-audit]
//!   deny_hard: [dangerous-sinks/eval]
//!   yara_rules: rules/malware.yar
//!   semgrep_config: p/php
//!   tool_timeout_ms: 300000
//! policy:
//!   quarantine_score: 10
//!   deny_score: 50
//! cache:
//!   enabled: true
//!   dir: /var/cache/ephpm-analyze
//! suppress:
//!   - rule: dangerous-sinks/assert
//!     path: legacy/compat.php
//!     reason: vetted 2026-09 — assert() behind a debug flag, not reachable
//! ```

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

/// A named preset that supplies the default analyzer set when
/// `analyzers.enable` is not given explicitly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// The default security set: `composer-audit`, `semgrep-php`,
    /// `malware-yara`, `dangerous-sinks`, `suppression-scan`.
    #[default]
    Security,
    /// No analyzers unless `analyzers.enable` lists them explicitly.
    None,
}

impl Profile {
    /// The analyzer ids this profile enables when `analyzers.enable` is not
    /// set.
    #[must_use]
    pub fn default_analyzers(self) -> Vec<String> {
        match self {
            Self::Security => vec![
                "composer-audit".to_owned(),
                "semgrep-php".to_owned(),
                "malware-yara".to_owned(),
                "dangerous-sinks".to_owned(),
                "suppression-scan".to_owned(),
            ],
            Self::None => Vec::new(),
        }
    }
}

impl FromStr for Profile {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "security" => Ok(Self::Security),
            "none" => Ok(Self::None),
            other => anyhow::bail!("unknown profile {other:?} (expected: security, none)"),
        }
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Security => "security",
            Self::None => "none",
        })
    }
}

/// How the final verdict maps to the process exit code (the CI gate).
///
/// | value        | behavior                                                  |
/// |--------------|-----------------------------------------------------------|
/// | `quarantine` | non-zero exit on `Quarantine` **or** `Deny` (the default) |
/// | `deny`       | non-zero exit only on `Deny`                              |
/// | `allow`      | report-only: the verdict never fails the exit code        |
///
/// `allow` is deliberately "the gate allows everything" (report-only) rather
/// than the literal "fail at allow-or-worse", which would fail every run and
/// mean nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FailOn {
    /// Report-only — never fail the exit code on the verdict.
    Allow,
    /// Fail on `Quarantine` or `Deny`. The default.
    #[default]
    Quarantine,
    /// Fail only on `Deny`.
    Deny,
}

impl FromStr for FailOn {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "allow" => Ok(Self::Allow),
            "quarantine" => Ok(Self::Quarantine),
            "deny" => Ok(Self::Deny),
            other => {
                anyhow::bail!("unknown fail_on {other:?} (expected: allow, quarantine, deny)")
            }
        }
    }
}

/// Output format for the analysis report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    /// Concise human-readable text. The default.
    #[default]
    Text,
    /// SARIF v2.1.0 JSON (one run).
    Sarif,
}

impl FromStr for OutputFormat {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "text" => Ok(Self::Text),
            "sarif" => Ok(Self::Sarif),
            other => anyhow::bail!("unknown output format {other:?} (expected: text, sarif)"),
        }
    }
}

/// `engine:` — the Phase-4 engine-in-the-loop (detonation) section.
///
/// **Planned: not yet implemented — parsed but not acted upon.** Both knobs
/// exist so Phase-4 configs are shaped now; setting either produces a startup
/// `tracing::warn!` (the workspace's no-silent-no-op rule).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    /// Planned: not yet implemented — parsed but not acted upon. Will opt
    /// suspicious inputs into sandboxed embedded-PHP evaluation.
    #[serde(default)]
    pub detonate: bool,
    /// Planned: not yet implemented — parsed but not acted upon. Will bound
    /// each detonation.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

impl EngineConfig {
    /// `true` when the operator set any Phase-4 knob (used to emit the
    /// "parsed but inert" warning).
    #[must_use]
    pub fn any_set(&self) -> bool {
        self.detonate || self.timeout_ms.is_some()
    }
}

fn default_semgrep_config() -> String {
    "p/php".to_owned()
}

/// Default per-tool timeout: 5 minutes.
fn default_tool_timeout_ms() -> u64 {
    300_000
}

/// `analyzers:` — which analyzers run and how strictly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalyzersConfig {
    /// Analyzer ids to run. When omitted, the `profile` supplies the set.
    /// An unknown id here is a hard configuration error (fail-closed), never
    /// silently ignored.
    #[serde(default)]
    pub enable: Option<Vec<String>>,
    /// Analyzer ids that must produce a result: a *skip* (tool absent) of a
    /// required analyzer is upgraded to a failure and floors the verdict at
    /// `Quarantine`, exactly like an error would. Must be a subset of the
    /// enabled set.
    #[serde(default)]
    pub required: Vec<String>,
    /// Hard-deny rules: a finding whose `rule_id` equals an entry, or falls
    /// under an entry as a `/`-separated prefix (e.g. `malware-yara` matches
    /// `malware-yara/AnyRule`), forces the verdict to `Deny` regardless of
    /// score.
    #[serde(default)]
    pub deny_hard: Vec<String>,
    /// Path to a YARA ruleset (file or directory index) for `malware-yara`.
    /// Relative paths resolve against the analyzed root. When unset,
    /// `malware-yara` is skipped with a diagnostic.
    #[serde(default)]
    pub yara_rules: Option<PathBuf>,
    /// Semgrep config/registry ref passed to `--config`. Default `p/php`.
    #[serde(default = "default_semgrep_config")]
    pub semgrep_config: String,
    /// Wall-clock budget per external tool invocation, in milliseconds. A
    /// tool exceeding it is killed and the analyzer fails (fail-closed).
    /// Default 300000 (5 minutes).
    #[serde(default = "default_tool_timeout_ms")]
    pub tool_timeout_ms: u64,
}

impl Default for AnalyzersConfig {
    fn default() -> Self {
        Self {
            enable: None,
            required: Vec::new(),
            deny_hard: Vec::new(),
            yara_rules: None,
            semgrep_config: default_semgrep_config(),
            tool_timeout_ms: default_tool_timeout_ms(),
        }
    }
}

fn default_quarantine_score() -> u32 {
    10
}

fn default_deny_score() -> u32 {
    50
}

/// `policy:` — the tunable thresholds of the weighted-score model.
///
/// See `crate::policy` for the full model. Weights per finding severity:
/// info 0, low 1, medium 4, high 10, critical 40 (critical also hard-denies
/// on its own, so its weight only matters for reporting).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    /// Total score at or above which the verdict is at least `Quarantine`.
    /// Default 10 (one high, or three mediums, ...).
    #[serde(default = "default_quarantine_score")]
    pub quarantine_score: u32,
    /// Total score at or above which the verdict is `Deny`. Default 50.
    #[serde(default = "default_deny_score")]
    pub deny_score: u32,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self { quarantine_score: default_quarantine_score(), deny_score: default_deny_score() }
    }
}

fn default_cache_enabled() -> bool {
    true
}

/// `cache:` — the incremental content-hash result cache.
///
/// Only the **native per-file analyzers** (`dangerous-sinks`,
/// `suppression-scan`) consult the cache; external-tool analyzers are never
/// cached because their results depend on state outside the analyzed tree
/// (advisory databases, registry rule packs, ruleset files) — a cache hit
/// there could hide a newly published advisory. See `crate::cache` for the
/// key model and the trust boundary of the cache directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    /// Whether the cache is consulted at all. Default `true` — the key
    /// includes the file content hash, the analyzer id, the effective config
    /// hash, and the crate version, so a hit is always current.
    #[serde(default = "default_cache_enabled")]
    pub enabled: bool,
    /// Cache directory. Default: `ephpm-analyze-cache` under the system temp
    /// directory. On shared hosts point this at a path only the operator can
    /// write — the cache is trusted state.
    #[serde(default)]
    pub dir: Option<PathBuf>,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self { enabled: default_cache_enabled(), dir: None }
    }
}

/// One operator-side suppression: waive findings matching `rule` (and
/// optionally `path`) with a documented `reason`.
///
/// Suppressions come **only** from this config — never from comments inside
/// the analyzed code. Tenant-authored suppression-shaped comments are
/// themselves flagged by the `suppression-scan` analyzer. `reason` is
/// mandatory and must be non-empty: a waiver without a recorded why is a
/// config error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuppressRule {
    /// Rule id to waive — exact, or a `/`-prefix (`dangerous-sinks` waives
    /// every `dangerous-sinks/<rule>`), same matching as
    /// `analyzers.deny_hard`.
    pub rule: String,
    /// When set, only findings at exactly this tree-relative path are waived
    /// (compared with normalized `/` separators). When unset, the rule
    /// applies at any path.
    #[serde(default)]
    pub path: Option<String>,
    /// Why this finding class is acceptable — recorded for the audit trail,
    /// required non-empty.
    pub reason: String,
}

fn default_level() -> u8 {
    1
}

/// The highest valid `level:` value — see `crate::policy` for the mapping.
pub const MAX_LEVEL: u8 = 3;

/// The root of `.ephpm-analyze.yml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeConfig {
    /// Named preset supplying the default analyzer set. Default `security`.
    #[serde(default)]
    pub profile: Profile,
    /// Strictness level `0..=3` scaling which findings gate — see
    /// `crate::policy` for the exact mapping. Default `1` (the weighted-score
    /// model). Composes with `profile`: the profile picks *which* analyzers
    /// run, the level picks *how strictly* their findings gate.
    #[serde(default = "default_level")]
    pub level: u8,
    /// Which verdicts fail the exit code. Default `quarantine`.
    #[serde(default)]
    pub fail_on: FailOn,
    /// Report format. Default `text`.
    #[serde(default)]
    pub output: OutputFormat,
    /// Baseline file to read: findings whose fingerprint appears in it are
    /// suppressed, so only *new* findings gate (the PHPStan model). Relative
    /// paths resolve against the analyzed root. A configured-but-missing
    /// baseline is a hard error (fail-closed), never a silent full scan.
    /// Generate the file with `ephpm analyze --baseline <file>`.
    #[serde(default)]
    pub baseline: Option<PathBuf>,
    /// Diff-aware scanning: restrict findings to files changed versus this
    /// git ref (`git diff --name-only <ref>` plus untracked files). A git
    /// failure is a hard error (fail-closed), never a silent full pass.
    #[serde(default)]
    pub since: Option<String>,
    /// Phase-4 engine section (parsed but inert — see [`EngineConfig`]).
    #[serde(default)]
    pub engine: EngineConfig,
    /// Analyzer selection and strictness.
    #[serde(default)]
    pub analyzers: AnalyzersConfig,
    /// Scoring thresholds.
    #[serde(default)]
    pub policy: PolicyConfig,
    /// Incremental result cache — see [`CacheConfig`].
    #[serde(default)]
    pub cache: CacheConfig,
    /// Operator-side finding waivers — see [`SuppressRule`]. The **only**
    /// suppression mechanism: inline comments in the analyzed code are never
    /// honored (and are flagged by `suppression-scan`).
    #[serde(default)]
    pub suppress: Vec<SuppressRule>,
}

impl Default for AnalyzeConfig {
    fn default() -> Self {
        Self {
            profile: Profile::default(),
            level: default_level(),
            fail_on: FailOn::default(),
            output: OutputFormat::default(),
            baseline: None,
            since: None,
            engine: EngineConfig::default(),
            analyzers: AnalyzersConfig::default(),
            policy: PolicyConfig::default(),
            cache: CacheConfig::default(),
            suppress: Vec::new(),
        }
    }
}

impl AnalyzeConfig {
    /// Parse a YAML document.
    ///
    /// # Errors
    ///
    /// On invalid YAML, any unknown key (all sections are strict), a `level`
    /// outside `0..=3`, or a `suppress` entry with an empty `rule` or
    /// `reason`.
    pub fn from_yaml(yaml: &str) -> anyhow::Result<Self> {
        let config: Self = serde_yaml_ng::from_str(yaml).context("invalid .ephpm-analyze.yml")?;
        config.validate()?;
        Ok(config)
    }

    /// Semantic checks past what serde can express.
    ///
    /// # Errors
    ///
    /// On an out-of-range `level` or an undocumented/empty suppression.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.level <= MAX_LEVEL,
            "level {} is out of range (expected 0..={MAX_LEVEL})",
            self.level
        );
        for (idx, rule) in self.suppress.iter().enumerate() {
            anyhow::ensure!(
                !rule.rule.trim().is_empty(),
                "suppress[{idx}] has an empty rule — name the rule id (or prefix) to waive"
            );
            anyhow::ensure!(
                !rule.reason.trim().is_empty(),
                "suppress[{idx}] ({}) has an empty reason — a waiver must document why",
                rule.rule
            );
        }
        Ok(())
    }

    /// Load from a file.
    ///
    /// # Errors
    ///
    /// On read failure or any parse error from [`AnalyzeConfig::from_yaml`].
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Self::from_yaml(&text).with_context(|| format!("in {}", path.display()))
    }

    /// The effective analyzer set: `analyzers.enable` when given, otherwise
    /// the profile's default set.
    #[must_use]
    pub fn enabled_analyzers(&self) -> Vec<String> {
        self.analyzers.enable.clone().unwrap_or_else(|| self.profile.default_analyzers())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_document_gets_all_defaults() {
        let cfg = AnalyzeConfig::from_yaml("{}").unwrap();
        assert_eq!(cfg.profile, Profile::Security);
        assert_eq!(cfg.level, 1);
        assert_eq!(cfg.fail_on, FailOn::Quarantine);
        assert_eq!(cfg.output, OutputFormat::Text);
        assert_eq!(cfg.baseline, None);
        assert_eq!(cfg.since, None);
        assert!(!cfg.engine.detonate);
        assert_eq!(cfg.policy.quarantine_score, 10);
        assert_eq!(cfg.policy.deny_score, 50);
        assert!(cfg.cache.enabled);
        assert_eq!(cfg.cache.dir, None);
        assert!(cfg.suppress.is_empty());
        assert_eq!(
            cfg.enabled_analyzers(),
            vec![
                "composer-audit",
                "semgrep-php",
                "malware-yara",
                "dangerous-sinks",
                "suppression-scan"
            ]
        );
    }

    #[test]
    fn parsed_and_derived_defaults_agree() {
        // `AnalyzeConfig::default()` is used when no config file exists;
        // parsing `{}` must produce the same effective configuration (this
        // is where a derived `Default` silently diverging from a serde
        // default would surface — e.g. `level` defaulting to 0 vs 1).
        let parsed = AnalyzeConfig::from_yaml("{}").unwrap();
        let derived = AnalyzeConfig::default();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), serde_json::to_value(&derived).unwrap());
    }

    #[test]
    fn full_document_parses() {
        let cfg = AnalyzeConfig::from_yaml(
            r"
profile: none
level: 2
fail_on: deny
output: sarif
baseline: .ephpm-analyze-baseline.json
since: origin/main
engine:
  detonate: true
  timeout_ms: 30000
analyzers:
  enable: [dangerous-sinks]
  required: [dangerous-sinks]
  deny_hard: [dangerous-sinks/eval]
  yara_rules: rules/malware.yar
  semgrep_config: p/security-audit
  tool_timeout_ms: 60000
policy:
  quarantine_score: 5
  deny_score: 20
cache:
  enabled: false
  dir: /var/cache/ephpm-analyze
suppress:
  - rule: dangerous-sinks/assert
    path: legacy/compat.php
    reason: vetted
",
        )
        .unwrap();
        assert_eq!(cfg.profile, Profile::None);
        assert_eq!(cfg.level, 2);
        assert_eq!(cfg.fail_on, FailOn::Deny);
        assert_eq!(cfg.baseline.as_deref(), Some(Path::new(".ephpm-analyze-baseline.json")));
        assert_eq!(cfg.since.as_deref(), Some("origin/main"));
        assert!(!cfg.cache.enabled);
        assert_eq!(cfg.cache.dir.as_deref(), Some(Path::new("/var/cache/ephpm-analyze")));
        assert_eq!(
            cfg.suppress,
            vec![SuppressRule {
                rule: "dangerous-sinks/assert".to_owned(),
                path: Some("legacy/compat.php".to_owned()),
                reason: "vetted".to_owned(),
            }]
        );
        assert_eq!(cfg.output, OutputFormat::Sarif);
        assert!(cfg.engine.detonate);
        assert_eq!(cfg.engine.timeout_ms, Some(30_000));
        assert_eq!(cfg.enabled_analyzers(), vec!["dangerous-sinks"]);
        assert_eq!(cfg.analyzers.required, vec!["dangerous-sinks"]);
        assert_eq!(cfg.analyzers.deny_hard, vec!["dangerous-sinks/eval"]);
        assert_eq!(cfg.analyzers.yara_rules.as_deref(), Some(Path::new("rules/malware.yar")));
        assert_eq!(cfg.analyzers.semgrep_config, "p/security-audit");
        assert_eq!(cfg.analyzers.tool_timeout_ms, 60_000);
        assert_eq!(cfg.policy.quarantine_score, 5);
        assert_eq!(cfg.policy.deny_score, 20);
    }

    #[test]
    fn unknown_keys_are_rejected_in_every_section() {
        // Root and every nested section are strict: a typo'd key must fail
        // the run naming the key, never silently no-op (#429 discipline).
        let cases = [
            ("root", "does_not_exist: 1"),
            ("engine", "engine:\n  detonate_all: true"),
            ("analyzers", "analyzers:\n  enabled: [x]"), // common typo of `enable`
            ("policy", "policy:\n  deny_treshold: 1"),
            ("cache", "cache:\n  directory: /tmp"), // common typo of `dir`
            ("suppress", "suppress:\n  - rule: a/b\n    reason: r\n    line: 3"),
        ];
        for (section, yaml) in cases {
            let err = AnalyzeConfig::from_yaml(yaml)
                .expect_err(&format!("unknown key in {section} must be rejected"));
            let msg = format!("{err:#}");
            assert!(msg.contains("unknown field"), "{section}: {msg}");
        }
    }

    #[test]
    fn level_out_of_range_is_rejected() {
        let err = AnalyzeConfig::from_yaml("level: 4").unwrap_err();
        assert!(format!("{err:#}").contains("out of range"));
        for level in 0..=3u8 {
            let cfg = AnalyzeConfig::from_yaml(&format!("level: {level}")).unwrap();
            assert_eq!(cfg.level, level);
        }
    }

    #[test]
    fn suppress_without_reason_is_rejected() {
        // A waiver must document why — an empty (or whitespace) reason is a
        // config error, not a silently accepted suppression.
        let err = AnalyzeConfig::from_yaml("suppress:\n  - rule: a/b\n    reason: ''").unwrap_err();
        assert!(format!("{err:#}").contains("empty reason"));
        let err =
            AnalyzeConfig::from_yaml("suppress:\n  - rule: ' '\n    reason: why").unwrap_err();
        assert!(format!("{err:#}").contains("empty rule"));
    }

    #[test]
    fn none_profile_enables_nothing() {
        let cfg = AnalyzeConfig::from_yaml("profile: none").unwrap();
        assert!(cfg.enabled_analyzers().is_empty());
    }

    #[test]
    fn explicit_enable_overrides_profile() {
        let cfg =
            AnalyzeConfig::from_yaml("profile: security\nanalyzers:\n  enable: [dangerous-sinks]")
                .unwrap();
        assert_eq!(cfg.enabled_analyzers(), vec!["dangerous-sinks"]);
    }

    #[test]
    fn from_str_parsers_cover_all_values() {
        assert_eq!("security".parse::<Profile>().unwrap(), Profile::Security);
        assert_eq!("none".parse::<Profile>().unwrap(), Profile::None);
        assert!("prod".parse::<Profile>().is_err());
        assert_eq!("allow".parse::<FailOn>().unwrap(), FailOn::Allow);
        assert_eq!("quarantine".parse::<FailOn>().unwrap(), FailOn::Quarantine);
        assert_eq!("deny".parse::<FailOn>().unwrap(), FailOn::Deny);
        assert!("block".parse::<FailOn>().is_err());
        assert_eq!("text".parse::<OutputFormat>().unwrap(), OutputFormat::Text);
        assert_eq!("sarif".parse::<OutputFormat>().unwrap(), OutputFormat::Sarif);
        assert!("json".parse::<OutputFormat>().is_err());
    }
}
