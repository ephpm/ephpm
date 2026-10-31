//! The compatibility report: what the importer changed, dropped, or could not
//! represent, as an ordered list of [`Finding`]s.
//!
//! Honesty is the point. The sanitizer and the execution driver push a
//! [`Finding`] for every statement they do not pass through verbatim, so the
//! operator sees exactly what Turso could not keep. [`FindingKind::Dropped`]
//! covers safe no-ops (a `LOCK TABLES`, a session `SET`) and is noise unless
//! `--verbose`; the other three — [`Downgraded`](FindingKind::Downgraded),
//! [`Unsupported`](FindingKind::Unsupported), [`Error`](FindingKind::Error) —
//! are always shown.

use serde::Serialize;

/// The severity/category of a [`Finding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FindingKind {
    /// A statement was dropped as a safe no-op (e.g. `LOCK TABLES`, a session
    /// `SET`, a `COMMENT ON`). Nothing is lost; shown only with `--verbose`.
    Dropped,
    /// A statement was kept but in a form that is not exactly equivalent —
    /// e.g. `ENUM`/`SET` columns stored as `TEXT`, or a sequence that becomes
    /// SQLite `AUTOINCREMENT` with different semantics.
    Downgraded,
    /// A construct SQLite/Turso cannot represent and the importer did not
    /// apply (stored procedures/functions, triggers, events, full-text or
    /// spatial indexes).
    Unsupported,
    /// A statement the backend rejected at execution time (recorded by the
    /// driver under `--on-error skip`, or the fatal one under `stop`).
    Error,
}

impl FindingKind {
    /// Lowercase label for human output.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Dropped => "dropped",
            Self::Downgraded => "downgraded",
            Self::Unsupported => "unsupported",
            Self::Error => "error",
        }
    }

    /// Whether a finding of this kind blocks a clean import — i.e. is an
    /// [`Unsupported`](Self::Unsupported) or [`Error`](Self::Error). Used by the
    /// CLI to decide the exit code under `--on-error stop`.
    #[must_use]
    pub fn is_blocking(self) -> bool {
        matches!(self, Self::Unsupported | Self::Error)
    }
}

/// One entry in a [`CompatReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// What kind of finding this is.
    pub kind: FindingKind,
    /// The SQL object or construct involved (a table/routine name, or the
    /// construct keyword when no name is available).
    pub object: String,
    /// A short human explanation of what happened and why.
    pub detail: String,
    /// 1-based line in the dump where the statement began.
    pub line: u64,
}

impl Finding {
    /// Build a finding.
    #[must_use]
    pub fn new(
        kind: FindingKind,
        object: impl Into<String>,
        detail: impl Into<String>,
        line: u64,
    ) -> Self {
        Self { kind, object: object.into(), detail: detail.into(), line }
    }
}

/// Counts of findings by kind. The non-`dropped` counts are what the exit code
/// and the headline summary line are built from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// Number of [`FindingKind::Dropped`] findings.
    pub dropped: usize,
    /// Number of [`FindingKind::Downgraded`] findings.
    pub downgraded: usize,
    /// Number of [`FindingKind::Unsupported`] findings.
    pub unsupported: usize,
    /// Number of [`FindingKind::Error`] findings.
    pub error: usize,
}

/// The ordered compatibility report built up over one import run.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CompatReport {
    findings: Vec<Finding>,
}

impl CompatReport {
    /// A fresh, empty report.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a finding (preserving order — findings read top-to-bottom like
    /// the dump).
    pub fn push(&mut self, finding: Finding) {
        self.findings.push(finding);
    }

    /// Every finding, in dump order.
    #[must_use]
    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    /// Findings an operator must see — everything that is not merely
    /// [`Dropped`](FindingKind::Dropped).
    pub fn significant(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|f| f.kind != FindingKind::Dropped)
    }

    /// Counts by kind.
    #[must_use]
    pub fn summary(&self) -> Summary {
        let mut s = Summary::default();
        for f in &self.findings {
            match f.kind {
                FindingKind::Dropped => s.dropped += 1,
                FindingKind::Downgraded => s.downgraded += 1,
                FindingKind::Unsupported => s.unsupported += 1,
                FindingKind::Error => s.error += 1,
            }
        }
        s
    }

    /// Whether the report carries any blocking finding (an
    /// [`Unsupported`](FindingKind::Unsupported) or
    /// [`Error`](FindingKind::Error)).
    #[must_use]
    pub fn has_blocking(&self) -> bool {
        self.findings.iter().any(|f| f.kind.is_blocking())
    }

    /// Serialize the report to pretty JSON — the `--report <path>` output.
    ///
    /// The shape is `{ "summary": {…}, "findings": [ {kind, object, detail,
    /// line}, … ] }`.
    ///
    /// # Panics
    ///
    /// Never in practice: the report is plain owned data (`Finding`s of
    /// strings and integers) with an infallible `Serialize`, so
    /// `serde_json` cannot fail. The `expect` documents that invariant.
    #[must_use]
    pub fn to_json(&self) -> String {
        let doc = serde_json::json!({
            "summary": self.summary(),
            "findings": self.findings,
        });
        serde_json::to_string_pretty(&doc).expect("CompatReport is always serializable")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> CompatReport {
        let mut r = CompatReport::new();
        r.push(Finding::new(FindingKind::Dropped, "LOCK TABLES", "no-op on SQLite", 5));
        r.push(Finding::new(FindingKind::Downgraded, "posts.status", "ENUM → TEXT", 12));
        r.push(Finding::new(FindingKind::Unsupported, "CREATE TRIGGER trg", "no triggers", 40));
        r
    }

    #[test]
    fn summary_counts_by_kind() {
        let s = sample().summary();
        assert_eq!(s, Summary { dropped: 1, downgraded: 1, unsupported: 1, error: 0 });
    }

    #[test]
    fn significant_excludes_dropped() {
        let r = sample();
        let kinds: Vec<_> = r.significant().map(|f| f.kind).collect();
        assert_eq!(kinds, vec![FindingKind::Downgraded, FindingKind::Unsupported]);
    }

    #[test]
    fn has_blocking_is_true_with_an_unsupported() {
        assert!(sample().has_blocking());
        let mut clean = CompatReport::new();
        clean.push(Finding::new(FindingKind::Downgraded, "x", "y", 1));
        assert!(!clean.has_blocking());
    }

    #[test]
    fn json_has_summary_and_findings() {
        let json = sample().to_json();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["summary"]["unsupported"], 1);
        assert_eq!(v["findings"][0]["kind"], "dropped");
        assert_eq!(v["findings"][1]["detail"], "ENUM → TEXT");
        assert_eq!(v["findings"][2]["line"], 40);
    }
}
