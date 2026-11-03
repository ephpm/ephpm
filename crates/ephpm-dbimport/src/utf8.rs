//! The invalid-UTF-8 ingestion contract.
//!
//! MySQL never validates the bytes in a `TEXT` column (a `latin1` or `binary`
//! column holds whatever was written — cp1252 smart quotes `0x92`/`0x93`/`0x94`,
//! an em-dash `0x97`, lone high bytes), and `mysqldump` writes those bytes
//! into the dump verbatim. Turso, on the other hand, validates `TEXT` at **read**
//! time: a value that is not valid UTF-8 imports "successfully" and then every
//! `SELECT` touching it fails with `TEXT value contains invalid UTF-8`. An
//! import must therefore never produce such a value, so what to do with an
//! invalid byte sequence is a decision the operator makes — a [`Utf8Policy`] —
//! and the default is to **refuse** the import rather than guess.
//!
//! The policy is applied by the [`DumpReader`](crate::DumpReader) as it
//! converts each input line to text, *per invalid sequence*: bytes that already
//! form valid UTF-8 are never re-decoded, so a `café` that was written as UTF-8
//! stays `café` even under `Transcode`, and only the byte runs that are not
//! UTF-8 are touched. Every touched run is reported as an [`InvalidRun`] on the
//! statement it landed in, which the import driver attributes to a table and
//! column for the compatibility report.

use std::borrow::Cow;
use std::fmt;

use encoding_rs::Encoding;

/// A legacy **single-byte** character set an invalid run is decoded from under
/// [`Utf8Policy::Transcode`].
///
/// Resolved by WHATWG label through `encoding_rs`, so `windows-1252`,
/// `cp1252`, `latin1` / `iso-8859-1` (both of which the WHATWG index — and
/// MySQL's own `latin1` — define as windows-1252), `windows-1251`, `koi8-r`,
/// `iso-8859-15`, `macintosh`, … all work. Multi-byte charsets (`gbk`,
/// `shift_jis`, `euc-kr`, …) are refused: their sequences include bytes that
/// are also valid UTF-8 (ASCII trail bytes), so decoding only the invalid runs
/// of a mixed stream could never be correct.
#[derive(Clone, Copy)]
pub struct Charset {
    encoding: &'static Encoding,
}

/// Why a `--source-charset` label was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CharsetError {
    /// No encoding is known by that label.
    #[error(
        "unknown charset {0:?} (expected a WHATWG label such as windows-1252, latin1, \
         iso-8859-15, windows-1251 or koi8-r)"
    )]
    Unknown(String),
    /// The label names a multi-byte or Unicode encoding, which cannot be
    /// applied per invalid sequence.
    #[error(
        "{0} is not a single-byte charset; --source-charset transcodes only the byte runs \
         that are not valid UTF-8 and leaves the rest of the stream untouched, which is \
         only well-defined for single-byte charsets (windows-1252, latin1, …)"
    )]
    NotSingleByte(&'static str),
}

impl Charset {
    /// Resolve a charset label. Case-insensitive; MySQL's own spellings of the
    /// common ones (`latin1`, `cp1252`, `koi8r`, `macroman`) are accepted.
    ///
    /// # Errors
    ///
    /// [`CharsetError::Unknown`] for a label no encoding is known by, and
    /// [`CharsetError::NotSingleByte`] for a multi-byte or Unicode encoding.
    pub fn parse(label: &str) -> Result<Self, CharsetError> {
        let trimmed = label.trim();
        // MySQL charset names that are not WHATWG labels.
        let whatwg = match trimmed.to_ascii_lowercase().as_str() {
            "koi8r" => "koi8-r",
            "koi8u" => "koi8-u",
            "macroman" => "macintosh",
            _ => trimmed,
        };
        let encoding = Encoding::for_label(whatwg.as_bytes())
            .ok_or_else(|| CharsetError::Unknown(label.to_string()))?;
        if !encoding.is_single_byte() {
            return Err(CharsetError::NotSingleByte(encoding.name()));
        }
        Ok(Self { encoding })
    }

    /// The canonical WHATWG name of the resolved encoding (`windows-1252` for
    /// `latin1`).
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.encoding.name()
    }

    /// Decode a run of bytes from this charset. Single-byte encodings are
    /// stateless, so a run can be decoded on its own; a byte the index leaves
    /// undefined decodes to U+FFFD.
    fn decode(self, bytes: &[u8]) -> Cow<'_, str> {
        self.encoding.decode_without_bom_handling(bytes).0
    }
}

/// What `--source-charset` names: the dump's own bytes are UTF-8 (the
/// `utf8mb4` default — valid multi-byte text passes through untouched and an
/// invalid run is handled by the policy without transcoding), or a legacy
/// single-byte [`Charset`] that invalid runs are decoded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceCharset {
    /// `utf8mb4` / `utf8` / `utf-8`: the stream is UTF-8; nothing is ever
    /// transcoded.
    #[default]
    Utf8,
    /// A legacy single-byte charset invalid runs are transcoded from.
    Legacy(Charset),
}

impl SourceCharset {
    /// Resolve a `--source-charset` label. `utf8mb4`, `utf8mb3`, `utf8` and
    /// `utf-8` (any case) are [`Utf8`](Self::Utf8); anything else goes
    /// through [`Charset::parse`].
    ///
    /// # Errors
    ///
    /// As [`Charset::parse`].
    pub fn parse(label: &str) -> Result<Self, CharsetError> {
        match label.trim().to_ascii_lowercase().as_str() {
            "utf8mb4" | "utf8mb3" | "utf8" | "utf-8" | "unicode-1-1-utf-8" => Ok(Self::Utf8),
            _ => Charset::parse(label).map(Self::Legacy),
        }
    }

    /// The legacy charset, if this is one.
    #[must_use]
    pub fn legacy(self) -> Option<Charset> {
        match self {
            Self::Utf8 => None,
            Self::Legacy(cs) => Some(cs),
        }
    }
}

impl fmt::Display for SourceCharset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Utf8 => f.write_str("utf8mb4"),
            Self::Legacy(cs) => fmt::Display::fmt(cs, f),
        }
    }
}

impl fmt::Debug for Charset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Charset").field(&self.name()).finish()
    }
}

impl fmt::Display for Charset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl PartialEq for Charset {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.encoding, other.encoding)
    }
}

impl Eq for Charset {}

/// What the importer does with a byte sequence that is not valid UTF-8.
///
/// Whatever the policy, every invalid run is counted and attributed in the
/// compatibility report; the policy only decides what text (if any) stands in
/// for it and whether the import may proceed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Utf8Policy {
    /// Refuse the import, naming the affected tables/columns and row counts.
    /// The default — a database that fails on `SELECT` is never produced
    /// silently. At the reader level the run is rendered as U+FFFD (so the
    /// statement can still be parsed for attribution); the driver refuses to
    /// execute any statement carrying one.
    #[default]
    Reject,
    /// Drop the invalid bytes.
    Strip,
    /// Replace each invalid sequence with U+FFFD (the Unicode replacement
    /// character).
    Replace,
    /// Decode each invalid run from a legacy single-byte charset. Lossless
    /// for a dump whose non-UTF-8 columns really are in that charset.
    Transcode(Charset),
}

impl Utf8Policy {
    /// The CLI spelling of the policy (`reject`, `strip`, `replace`,
    /// `transcode`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Reject => "reject",
            Self::Strip => "strip",
            Self::Replace => "replace",
            Self::Transcode(_) => "transcode",
        }
    }

    /// A past-tense description of what the policy did to a run, for the
    /// compatibility report.
    #[must_use]
    pub fn applied(self) -> String {
        match self {
            Self::Reject => "rejected (re-run with --on-invalid-utf8 strip|replace, or \
                             --source-charset <charset> to transcode)"
                .to_string(),
            Self::Strip => "invalid bytes stripped".to_string(),
            Self::Replace => "each invalid sequence replaced with U+FFFD".to_string(),
            Self::Transcode(cs) => format!("transcoded from {cs}"),
        }
    }

    /// Whether this policy refuses an import that meets an invalid run.
    #[must_use]
    pub fn is_reject(self) -> bool {
        matches!(self, Self::Reject)
    }
}

impl fmt::Display for Utf8Policy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transcode(cs) => write!(f, "transcode from {cs}"),
            other => f.write_str(other.name()),
        }
    }
}

/// One maximal run of bytes that were not valid UTF-8, as found in a
/// statement's text.
///
/// `offset` is a byte offset into the *decoded* statement text (the
/// [`RawStatement::sql`](crate::RawStatement::sql)): the position of the
/// replacement text under `Replace`/`Reject`/`Transcode`, or the position the
/// bytes were removed from under `Strip`. `bytes` are the original bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidRun {
    /// Byte offset into the decoded text.
    pub offset: usize,
    /// The original, invalid bytes.
    pub bytes: Vec<u8>,
}

/// Decode `bytes` into `out` under `policy`, appending an [`InvalidRun`] (with
/// an offset into `out`) for every maximal invalid run. Valid UTF-8 is copied
/// through untouched; consecutive invalid sequences coalesce into one run.
pub(crate) fn decode_into(
    mut bytes: &[u8],
    policy: Utf8Policy,
    out: &mut String,
    runs: &mut Vec<InvalidRun>,
) {
    // Index into `runs` of a run that ends exactly at `out.len()` — set only
    // while the previous iteration produced one with no valid text after it,
    // so an adjacent invalid sequence extends it rather than opening another.
    let mut open_run: Option<usize> = None;
    loop {
        match std::str::from_utf8(bytes) {
            Ok(valid) => {
                out.push_str(valid);
                return;
            }
            Err(e) => {
                let good = e.valid_up_to();
                if good > 0 {
                    // Validated by `valid_up_to`; re-checking the prefix keeps
                    // this free of `unsafe` at a cost paid once per run.
                    out.push_str(std::str::from_utf8(&bytes[..good]).unwrap_or_default());
                    open_run = None;
                }
                // `None` = the input ends mid-sequence: everything left is bad.
                let bad_len = e.error_len().unwrap_or(bytes.len() - good);
                let bad = &bytes[good..good + bad_len];

                let run = match open_run {
                    Some(idx) => idx,
                    None => {
                        runs.push(InvalidRun { offset: out.len(), bytes: Vec::new() });
                        runs.len() - 1
                    }
                };
                runs[run].bytes.extend_from_slice(bad);
                open_run = Some(run);

                match policy {
                    Utf8Policy::Strip => {}
                    Utf8Policy::Reject | Utf8Policy::Replace => out.push('\u{FFFD}'),
                    Utf8Policy::Transcode(cs) => out.push_str(&cs.decode(bad)),
                }
                bytes = &bytes[good + bad_len..];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8], policy: Utf8Policy) -> (String, Vec<InvalidRun>) {
        let mut out = String::new();
        let mut runs = Vec::new();
        decode_into(bytes, policy, &mut out, &mut runs);
        (out, runs)
    }

    #[test]
    fn valid_utf8_passes_through_under_every_policy() {
        let cp1252 = Charset::parse("windows-1252").unwrap();
        for policy in [
            Utf8Policy::Reject,
            Utf8Policy::Strip,
            Utf8Policy::Replace,
            Utf8Policy::Transcode(cp1252),
        ] {
            let (out, runs) = decode("café — naïve".as_bytes(), policy);
            assert_eq!(out, "café — naïve", "{policy:?}");
            assert!(runs.is_empty(), "{policy:?} reported runs in valid text: {runs:?}");
        }
    }

    #[test]
    fn a_lone_cp1252_smart_quote_is_transcoded_per_sequence() {
        // `it\x92s caf\xC3\xA9` — a cp1252 apostrophe next to a UTF-8 `é`.
        // Only the 0x92 run is decoded from windows-1252; the `é` must not be
        // double-encoded into `Ã©`.
        let cp1252 = Charset::parse("windows-1252").unwrap();
        let (out, runs) =
            decode(b"it\x92s caf\xC3\xA9 \x93ok\x94 \x97 end", Utf8Policy::Transcode(cp1252));
        assert_eq!(out, "it\u{2019}s café \u{201C}ok\u{201D} \u{2014} end");
        assert_eq!(runs.len(), 4);
        assert_eq!(runs[0], InvalidRun { offset: 2, bytes: vec![0x92] });
        assert_eq!(runs[0].offset, out.find('\u{2019}').unwrap());
    }

    #[test]
    fn latin1_is_windows_1252_like_mysql() {
        // MySQL's `latin1` is cp1252 (its docs say so), and so is the WHATWG
        // `latin1` label — so a 0x92 under `latin1` is a right single quote,
        // not the C1 control U+0092.
        let latin1 = Charset::parse("latin1").unwrap();
        assert_eq!(latin1.name(), "windows-1252");
        assert_eq!(decode(b"\x92", Utf8Policy::Transcode(latin1)).0, "\u{2019}");
        // Proper Latin-1 range bytes decode the same under both.
        assert_eq!(decode(b"\xE9", Utf8Policy::Transcode(latin1)).0, "é");
    }

    #[test]
    fn strip_and_replace_behave_per_spec() {
        let (out, runs) = decode(b"a\x92b", Utf8Policy::Strip);
        assert_eq!(out, "ab");
        assert_eq!(runs, vec![InvalidRun { offset: 1, bytes: vec![0x92] }]);

        let (out, runs) = decode(b"a\x92b", Utf8Policy::Replace);
        assert_eq!(out, "a\u{FFFD}b");
        assert_eq!(runs, vec![InvalidRun { offset: 1, bytes: vec![0x92] }]);

        // Reject renders like Replace at this level; the driver refuses.
        let (out, runs) = decode(b"a\x92b", Utf8Policy::Reject);
        assert_eq!(out, "a\u{FFFD}b");
        assert_eq!(runs.len(), 1);
    }

    #[test]
    fn adjacent_invalid_bytes_coalesce_into_one_run() {
        // Three consecutive cp1252 bytes are one run, not three.
        let (out, runs) = decode(b"x\x92\x93\x94y", Utf8Policy::Replace);
        assert_eq!(out, "x\u{FFFD}\u{FFFD}\u{FFFD}y");
        assert_eq!(runs, vec![InvalidRun { offset: 1, bytes: vec![0x92, 0x93, 0x94] }]);
        // A run at the very end (truncated lead byte, `error_len() == None`).
        let (out, runs) = decode(b"ab\xE2\x80", Utf8Policy::Strip);
        assert_eq!(out, "ab");
        assert_eq!(runs, vec![InvalidRun { offset: 2, bytes: vec![0xE2, 0x80] }]);
    }

    #[test]
    fn charset_labels_resolve_and_multibyte_ones_are_refused() {
        assert_eq!(Charset::parse("WINDOWS-1252").unwrap().name(), "windows-1252");
        assert_eq!(Charset::parse("cp1252").unwrap().name(), "windows-1252");
        assert_eq!(Charset::parse("iso-8859-1").unwrap().name(), "windows-1252");
        assert_eq!(Charset::parse("iso-8859-15").unwrap().name(), "ISO-8859-15");
        assert_eq!(Charset::parse("windows-1251").unwrap().name(), "windows-1251");
        assert_eq!(Charset::parse("koi8r").unwrap().name(), "KOI8-R");
        assert_eq!(Charset::parse("macroman").unwrap().name(), "macintosh");
        assert!(matches!(Charset::parse("klingon"), Err(CharsetError::Unknown(_))));
        assert!(matches!(Charset::parse("utf-8"), Err(CharsetError::NotSingleByte(_))));
        assert!(matches!(Charset::parse("gbk"), Err(CharsetError::NotSingleByte(_))));
        assert!(matches!(Charset::parse("shift_jis"), Err(CharsetError::NotSingleByte(_))));
        assert!(matches!(Charset::parse("utf-16le"), Err(CharsetError::NotSingleByte(_))));
    }

    #[test]
    fn source_charset_utf8mb4_is_the_native_passthrough() {
        for label in ["utf8mb4", "UTF8MB4", "utf8", "utf-8", "utf8mb3"] {
            assert_eq!(SourceCharset::parse(label).unwrap(), SourceCharset::Utf8, "{label}");
        }
        assert_eq!(SourceCharset::Utf8.to_string(), "utf8mb4");
        assert_eq!(SourceCharset::default(), SourceCharset::Utf8);
        let legacy = SourceCharset::parse("latin1").unwrap();
        assert_eq!(legacy.legacy().map(|c| c.name()), Some("windows-1252"));
        assert_eq!(legacy.to_string(), "windows-1252");
        assert!(matches!(SourceCharset::parse("gbk"), Err(CharsetError::NotSingleByte(_))));
    }

    /// Axis C (silent charset downgrade): valid multi-byte UTF-8 — a 4-byte
    /// emoji included — must pass through every policy byte-identically.
    /// There is no narrower charset anywhere on this path to turn `⏰` into
    /// `?`, and this pins that no policy introduces one.
    #[test]
    fn valid_multibyte_utf8_is_byte_identical_under_every_policy() {
        let bytes: &[u8] = b"\xE2\x8F\xB0 x \xF0\x9F\x98\x80 caf\xC3\xA9 \xE4\xB8\xAD";
        assert_eq!(std::str::from_utf8(bytes).unwrap(), "⏰ x 😀 café 中");
        let cp1252 = Charset::parse("windows-1252").unwrap();
        for policy in [
            Utf8Policy::Reject,
            Utf8Policy::Strip,
            Utf8Policy::Replace,
            Utf8Policy::Transcode(cp1252),
        ] {
            let (out, runs) = decode(bytes, policy);
            assert_eq!(out.as_bytes(), bytes, "{policy:?} altered valid UTF-8");
            assert!(runs.is_empty(), "{policy:?}: {runs:?}");
        }
    }

    #[test]
    fn policy_names_are_the_cli_spellings() {
        assert_eq!(Utf8Policy::Reject.name(), "reject");
        assert_eq!(Utf8Policy::Strip.name(), "strip");
        assert_eq!(Utf8Policy::Replace.name(), "replace");
        let cs = Charset::parse("windows-1252").unwrap();
        assert_eq!(Utf8Policy::Transcode(cs).name(), "transcode");
        assert_eq!(Utf8Policy::Transcode(cs).to_string(), "transcode from windows-1252");
        assert!(Utf8Policy::default().is_reject());
    }
}
