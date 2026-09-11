//! `.ephpm-analyze.yml` — the golangci-lint-shaped configuration file.
//!
//! Every section is `#[serde(deny_unknown_fields)]`, matching the workspace's
//! strict-config discipline: an unknown key fails the run naming the key
//! instead of silently doing nothing (the #429 lesson — a knob an operator
//! set must never be a no-op). Unlike `ephpm.toml` there is no environment
//! layer feeding this file, so the **root** is strict too.
//!
//! ```yaml
//! profile: security          # security | none
//! fail_on: quarantine        # allow | quarantine | deny
//! output: text               # text | sarif
//! engine:                    # Phase 4 — parsed but inert today
//!   detonate: false
//!   timeout_ms: 30000
//! analyzers:
//!   enable: [composer-audit, semgrep-php, malware-yara, dangerous-sinks]
//!   required: [composer-audit]
//!   deny_hard: [dangerous-sinks/eval]
//!   yara_rules: rules/malware.yar
//!   semgrep_config: p/php
//!   tool_timeout_ms: 300000
//! policy:
//!   quarantine_score: 10
//!   deny_score: 50
//! ```

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::Context as _;
use serde::Deserialize;

/// A named preset that supplies the default analyzer set when
/// `analyzers.enable` is not given explicitly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// The Phase-1 security set: `composer-audit`, `semgrep-php`,
    /// `malware-yara`, `dangerous-sinks`. The default.
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
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
#[derive(Debug, Clone, Default, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
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

/// The root of `.ephpm-analyze.yml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeConfig {
    /// Named preset supplying the default analyzer set. Default `security`.
    #[serde(default)]
    pub profile: Profile,
    /// Which verdicts fail the exit code. Default `quarantine`.
    #[serde(default)]
    pub fail_on: FailOn,
    /// Report format. Default `text`.
    #[serde(default)]
    pub output: OutputFormat,
    /// Phase-4 engine section (parsed but inert — see [`EngineConfig`]).
    #[serde(default)]
    pub engine: EngineConfig,
    /// Analyzer selection and strictness.
    #[serde(default)]
    pub analyzers: AnalyzersConfig,
    /// Scoring thresholds.
    #[serde(default)]
    pub policy: PolicyConfig,
}

impl AnalyzeConfig {
    /// Parse a YAML document.
    ///
    /// # Errors
    ///
    /// On invalid YAML or any unknown key (all sections are strict).
    pub fn from_yaml(yaml: &str) -> anyhow::Result<Self> {
        serde_yaml_ng::from_str(yaml).context("invalid .ephpm-analyze.yml")
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
        assert_eq!(cfg.fail_on, FailOn::Quarantine);
        assert_eq!(cfg.output, OutputFormat::Text);
        assert!(!cfg.engine.detonate);
        assert_eq!(cfg.policy.quarantine_score, 10);
        assert_eq!(cfg.policy.deny_score, 50);
        assert_eq!(
            cfg.enabled_analyzers(),
            vec!["composer-audit", "semgrep-php", "malware-yara", "dangerous-sinks"]
        );
    }

    #[test]
    fn full_document_parses() {
        let cfg = AnalyzeConfig::from_yaml(
            r"
profile: none
fail_on: deny
output: sarif
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
",
        )
        .unwrap();
        assert_eq!(cfg.profile, Profile::None);
        assert_eq!(cfg.fail_on, FailOn::Deny);
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
        ];
        for (section, yaml) in cases {
            let err = AnalyzeConfig::from_yaml(yaml)
                .expect_err(&format!("unknown key in {section} must be rejected"));
            let msg = format!("{err:#}");
            assert!(msg.contains("unknown field"), "{section}: {msg}");
        }
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
