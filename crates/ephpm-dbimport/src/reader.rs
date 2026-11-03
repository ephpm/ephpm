//! Streaming SQL statement splitter for dump files.
//!
//! [`DumpReader`] yields one [`RawStatement`] at a time off any [`BufRead`]
//! without materializing the whole file. It is the part of the pipeline that
//! has to understand *lexical* structure — where one statement ends and the
//! next begins — so that the [`Sanitizer`](crate::Sanitizer) downstream can
//! reason about whole statements.
//!
//! # Memory
//!
//! The reader holds at most one statement at a time (plus one input line of
//! scratch). File size does not bound memory — statement size does. A
//! `mysqldump`'s extended `INSERT`s are already bounded by
//! `--net-buffer-length` (≈1 MiB by default), and a `COPY … FROM stdin` block
//! is accumulated as one statement because its rows must be converted
//! together; splitting an individual oversized `COPY`/`INSERT` into row chunks
//! at read time is a possible fast-follow, not needed for the ≥1 GiB streaming
//! guarantee (which is about not reading the file whole, verified by
//! `reads_a_large_input_without_buffering_it_all`).
//!
//! # What it respects
//!
//! * `'…'` and `"…"` string literals, with `\`-escapes (MySQL only) and
//!   doubled-quote escapes.
//! * Backtick identifiers (MySQL).
//! * `--` / `#` line comments and `/* … */` block comments (including the
//!   MySQL `/*! … */` executable form — kept verbatim, never split inside).
//! * `DELIMITER …` client directives — the delimiter changes and the routine
//!   body that follows (`CREATE PROCEDURE … BEGIN … ; … END $$`) stays one
//!   statement.
//! * The Postgres `COPY tbl (cols) FROM stdin;` … `\.` data block, read
//!   verbatim rather than split on the `;` or on semicolons in the data.
//!
//! # Bytes, not text
//!
//! The input is read as **bytes** and converted to text line by line under a
//! [`Utf8Policy`]: a dump from a `latin1`/`binary` column is not valid UTF-8,
//! and a `String`-based `read_line` would fail on the first such line — after
//! earlier batches had already been committed, with an error naming nothing.
//! Every invalid byte run is recorded on the statement it landed in
//! ([`RawStatement::invalid_utf8`]) so the driver can attribute it to a table
//! and column and apply the policy (refuse, or report what was changed).

use std::io::BufRead;

use crate::Dialect;
use crate::utf8::{InvalidRun, Utf8Policy, decode_into};

/// One raw, un-sanitized statement lifted out of a dump, with the 1-based
/// line number where it began (used to anchor [`Finding`](crate::Finding)s).
///
/// The text is verbatim — leading/trailing whitespace and interior comments
/// are preserved; the [`Sanitizer`](crate::Sanitizer) trims and interprets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawStatement {
    /// The statement text, with any trailing statement delimiter removed.
    pub sql: String,
    /// 1-based line number where the statement started.
    pub line: u64,
    /// Every byte run in this statement that was not valid UTF-8, with offsets
    /// into `sql`, in ascending offset order. Empty for a clean statement.
    pub invalid_utf8: Vec<InvalidRun>,
}

impl RawStatement {
    /// A clean statement (no invalid UTF-8) — the common case, and what the
    /// sanitizer tests build.
    #[must_use]
    pub fn new(sql: impl Into<String>, line: u64) -> Self {
        Self { sql: sql.into(), line, invalid_utf8: Vec::new() }
    }
}

/// An error reading or splitting a dump.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// The underlying reader failed.
    #[error("reading the dump: {0}")]
    Io(#[from] std::io::Error),
}

/// Lexical state carried across input lines while scanning for a statement
/// boundary. A string literal or block comment can span many lines, so this
/// must persist between reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scan {
    Normal,
    LineComment,
    BlockComment,
    Single,
    Double,
    Backtick,
}

/// A streaming statement splitter over `R`.
///
/// Construct with [`DumpReader::new`] and pull statements with
/// [`DumpReader::next_statement`], or iterate (`for stmt in reader` yields
/// `Result<RawStatement, ReadError>`).
pub struct DumpReader<R: BufRead> {
    reader: R,
    dialect: Dialect,
    /// What to do with input bytes that are not valid UTF-8.
    utf8: Utf8Policy,
    /// The active statement delimiter (`;`, or whatever `DELIMITER` set).
    delimiter: String,
    /// Text accumulated for the statement currently being assembled.
    pending: String,
    /// Invalid byte runs found in `pending`, offsets into `pending`.
    pending_runs: Vec<InvalidRun>,
    /// Byte offset in `pending` up to which [`Scan`] state is current.
    scan_pos: usize,
    /// Lexical state at `scan_pos`.
    state: Scan,
    /// 1-based number of the last line read.
    line_no: u64,
    /// Line where `pending`'s current statement began.
    stmt_line: u64,
    /// Reusable raw line buffer.
    line_buf: Vec<u8>,
    /// The decoded form of `line_buf`, plus the runs found in it.
    line_text: String,
    line_runs: Vec<InvalidRun>,
    /// Set once the underlying reader reports EOF.
    eof: bool,
    /// When `Some`, we are inside a `COPY … FROM stdin` data block: the buffer
    /// holds the header plus the data lines read so far.
    copy: Option<CopyBlock>,
}

/// In-progress `COPY … FROM stdin` capture.
struct CopyBlock {
    buf: String,
    runs: Vec<InvalidRun>,
    line: u64,
}

impl<R: BufRead> DumpReader<R> {
    /// Create a reader over `reader`, splitting according to `dialect`'s
    /// quoting rules. The initial statement delimiter is `;`, and the UTF-8
    /// policy is [`Utf8Policy::Reject`] (invalid runs rendered as U+FFFD and
    /// reported, for the driver to refuse) — see [`with_utf8_policy`].
    ///
    /// [`with_utf8_policy`]: Self::with_utf8_policy
    #[must_use]
    pub fn new(reader: R, dialect: Dialect) -> Self {
        Self {
            reader,
            dialect,
            utf8: Utf8Policy::Reject,
            delimiter: ";".to_string(),
            pending: String::new(),
            pending_runs: Vec::new(),
            scan_pos: 0,
            state: Scan::Normal,
            line_no: 0,
            stmt_line: 1,
            line_buf: Vec::new(),
            line_text: String::new(),
            line_runs: Vec::new(),
            eof: false,
            copy: None,
        }
    }

    /// Set the [`Utf8Policy`] applied to input bytes that are not valid UTF-8.
    #[must_use]
    pub fn with_utf8_policy(mut self, policy: Utf8Policy) -> Self {
        self.utf8 = policy;
        self
    }

    /// Read one raw line into `line_text` / `line_runs` (offsets relative to
    /// the line). Returns the number of raw bytes read (0 at EOF).
    fn read_line(&mut self) -> Result<usize, ReadError> {
        self.line_buf.clear();
        let n = self.reader.read_until(b'\n', &mut self.line_buf)?;
        self.line_text.clear();
        self.line_runs.clear();
        if n > 0 {
            decode_into(&self.line_buf, self.utf8, &mut self.line_text, &mut self.line_runs);
        }
        Ok(n)
    }

    /// Append the decoded line to `pending`, carrying its runs along (shifted
    /// to `pending` offsets).
    fn push_line_to_pending(&mut self) {
        let base = self.pending.len();
        self.pending.push_str(&self.line_text);
        self.pending_runs.extend(
            self.line_runs
                .drain(..)
                .map(|r| InvalidRun { offset: base + r.offset, bytes: r.bytes }),
        );
    }

    /// Return the next statement, or `None` at end of input.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError::Io`] if the underlying reader fails.
    pub fn next_statement(&mut self) -> Result<Option<RawStatement>, ReadError> {
        loop {
            // Inside a COPY data block: consume lines verbatim until `\.`.
            if self.copy.is_some() {
                if let Some(stmt) = self.pump_copy()? {
                    return Ok(Some(stmt));
                }
                // pump_copy only returns None at EOF without a terminator; it
                // still finalizes what it has, so falling through here means
                // the block is done being handled.
                continue;
            }

            // Try to carve a statement out of what we already hold.
            if let Some(stmt) = self.carve() {
                if stmt.sql.trim().is_empty() {
                    // A bare delimiter (`;;`) or comment-only run — not worth a
                    // Dropped finding; keep scanning.
                    continue;
                }
                return Ok(Some(stmt));
            }
            // `carve` returning None while a COPY block was just opened means
            // the data body follows: loop back so `pump_copy` reads it verbatim
            // rather than appending the rows to `pending`.
            if self.copy.is_some() {
                continue;
            }

            if self.eof {
                return Ok(self.flush_final());
            }

            // Need more input.
            let n = self.read_line()?;
            if n == 0 {
                self.eof = true;
                continue;
            }
            self.line_no += 1;

            // A `DELIMITER x` client directive only at a statement boundary.
            if self.state == Scan::Normal
                && self.pending.trim().is_empty()
                && let Some(delim) = parse_delimiter_directive(&self.line_text)
            {
                self.delimiter = delim;
                // The directive line is consumed, not part of any statement.
                self.pending.clear();
                self.pending_runs.clear();
                self.scan_pos = 0;
                continue;
            }

            if self.pending.trim().is_empty() {
                self.stmt_line = self.line_no;
            }
            self.push_line_to_pending();
        }
    }

    /// Split `pending` (and its runs) at a delimiter found at byte `at`, the
    /// delimiter being `delim_len` bytes long. Returns the statement text and
    /// its runs; `pending` keeps the remainder with its runs re-based.
    fn split_pending(&mut self, at: usize, delim_len: usize) -> (String, Vec<InvalidRun>) {
        let rest_start = at + delim_len;
        let stmt_text = self.pending[..at].to_string();
        let remainder = self.pending[rest_start..].to_string();
        let (stmt_runs, rest_runs): (Vec<InvalidRun>, Vec<InvalidRun>) =
            self.pending_runs.drain(..).partition(|r| r.offset < rest_start);
        self.pending = remainder;
        self.pending_runs = rest_runs
            .into_iter()
            .map(|r| InvalidRun { offset: r.offset - rest_start, bytes: r.bytes })
            .collect();
        self.scan_pos = 0;
        self.state = Scan::Normal;
        (stmt_text, stmt_runs)
    }

    /// Scan `pending` from `scan_pos` for an unquoted, un-commented delimiter.
    /// On a hit, split the statement off and reset for the remainder. Also
    /// detects a completed `COPY … FROM stdin` header and switches to copy
    /// mode instead of returning it.
    fn carve(&mut self) -> Option<RawStatement> {
        let bytes = self.pending.as_bytes();
        let delim = self.delimiter.as_bytes();
        let ms = matches!(self.dialect, Dialect::MySql);
        let mut i = self.scan_pos;

        while i < bytes.len() {
            match self.state {
                Scan::Normal => {
                    if bytes[i] == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
                        self.state = Scan::LineComment;
                        i += 2;
                    } else if ms && bytes[i] == b'#' {
                        self.state = Scan::LineComment;
                        i += 1;
                    } else if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                        self.state = Scan::BlockComment;
                        i += 2;
                    } else if bytes[i] == b'\'' {
                        self.state = Scan::Single;
                        i += 1;
                    } else if bytes[i] == b'"' {
                        self.state = Scan::Double;
                        i += 1;
                    } else if ms && bytes[i] == b'`' {
                        self.state = Scan::Backtick;
                        i += 1;
                    } else if bytes[i..].starts_with(delim) {
                        // Boundary at `i`, delimiter spans `delim.len()`.
                        let delim_len = delim.len();
                        let line = self.stmt_line;
                        let (stmt_text, stmt_runs) = self.split_pending(i, delim_len);

                        if ms || !is_copy_from_stdin(&stmt_text) {
                            return Some(RawStatement {
                                sql: stmt_text,
                                line,
                                invalid_utf8: stmt_runs,
                            });
                        }
                        // Postgres COPY … FROM stdin: begin capturing the data
                        // block; the header + rows + `\.` become one statement.
                        // Store from the COPY keyword (dropping any leading
                        // comment lines) so the bundled statement's first line
                        // is the header the sanitizer parses. The noise is a
                        // prefix, so a header run (vanishingly rare) is
                        // re-based by the length dropped.
                        let header = strip_leading_noise(&stmt_text);
                        let dropped = stmt_text.len() - header.len();
                        let runs = stmt_runs
                            .into_iter()
                            .filter(|r| r.offset >= dropped)
                            .map(|r| InvalidRun { offset: r.offset - dropped, bytes: r.bytes })
                            .collect();
                        self.copy = Some(CopyBlock { buf: format!("{header}\n"), runs, line });
                        return None;
                    } else {
                        i += 1;
                    }
                }
                Scan::LineComment => {
                    if bytes[i] == b'\n' {
                        self.state = Scan::Normal;
                    }
                    i += 1;
                }
                Scan::BlockComment => {
                    if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                        self.state = Scan::Normal;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                Scan::Single => {
                    let (ni, closed) = scan_quoted(bytes, i, b'\'', ms);
                    i = ni;
                    if closed {
                        self.state = Scan::Normal;
                    }
                }
                Scan::Double => {
                    let (ni, closed) = scan_quoted(bytes, i, b'"', ms);
                    i = ni;
                    if closed {
                        self.state = Scan::Normal;
                    }
                }
                Scan::Backtick => {
                    let (ni, closed) = scan_quoted(bytes, i, b'`', false);
                    i = ni;
                    if closed {
                        self.state = Scan::Normal;
                    }
                }
            }
        }

        self.scan_pos = bytes.len();
        None
    }

    /// Consume lines into the active [`CopyBlock`] until a `\.` terminator (or
    /// EOF), then return the whole block as one statement.
    fn pump_copy(&mut self) -> Result<Option<RawStatement>, ReadError> {
        loop {
            let n = self.read_line()?;
            if n == 0 {
                self.eof = true;
                return Ok(Some(self.finish_copy()));
            }
            self.line_no += 1;
            // A line consisting solely of `\.` (optionally CRLF) ends the block.
            if self.line_text.trim_end_matches(['\r', '\n']) == "\\." {
                return Ok(Some(self.finish_copy()));
            }
            let block = self.copy.as_mut().expect("in copy mode");
            let base = block.buf.len();
            block.buf.push_str(&self.line_text);
            block.runs.extend(
                self.line_runs
                    .drain(..)
                    .map(|r| InvalidRun { offset: base + r.offset, bytes: r.bytes }),
            );
        }
    }

    /// Close the active `COPY` block and hand it back as one statement.
    fn finish_copy(&mut self) -> RawStatement {
        let block = self.copy.take().expect("in copy mode");
        RawStatement { sql: block.buf, line: block.line, invalid_utf8: block.runs }
    }

    /// At EOF, hand back any trailing statement that had no final delimiter.
    fn flush_final(&mut self) -> Option<RawStatement> {
        let text = std::mem::take(&mut self.pending);
        let runs = std::mem::take(&mut self.pending_runs);
        self.scan_pos = 0;
        if text.trim().is_empty() {
            None
        } else {
            Some(RawStatement { sql: text, line: self.stmt_line, invalid_utf8: runs })
        }
    }
}

impl<R: BufRead> Iterator for DumpReader<R> {
    type Item = Result<RawStatement, ReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_statement().transpose()
    }
}

/// Scan forward through a quoted region that began just before `i` (the opening
/// quote has already been consumed). Returns the index just past the region
/// when it closes — with the caller's `state` left to be reset to `Normal` by
/// the return convention — or the end of the buffer when it is still open.
///
/// `mysql_escapes` enables the MySQL `\`-escape; both dialects honor the
/// doubled-quote escape (`''`, `""`, `` `` ``).
///
/// Returns `(new_index, closed)`: `closed` is true when the closing quote was
/// consumed (so the caller resets to [`Scan::Normal`]), false when the region
/// is still open at end of buffer (the caller stays in the quoted state and
/// resumes on the next line).
fn scan_quoted(bytes: &[u8], mut i: usize, quote: u8, mysql_escapes: bool) -> (usize, bool) {
    while i < bytes.len() {
        let b = bytes[i];
        if mysql_escapes && b == b'\\' {
            // Escape: skip the backslash and the escaped byte.
            i += 2;
            continue;
        }
        if b == quote {
            if i + 1 < bytes.len() && bytes[i + 1] == quote {
                // Doubled quote — an escaped literal quote, stay inside.
                i += 2;
                continue;
            }
            // Closing quote.
            return (i + 1, true);
        }
        i += 1;
    }
    (i, false)
}

/// Parse a `DELIMITER <token>` client directive from a whole input line,
/// returning the new delimiter token. Case-insensitive on the keyword; the
/// token is the first whitespace-delimited run after it.
fn parse_delimiter_directive(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let kw = parts.next()?;
    if !kw.eq_ignore_ascii_case("DELIMITER") {
        return None;
    }
    let rest = parts.next()?.trim();
    if rest.is_empty() { None } else { Some(rest.split_whitespace().next()?.to_string()) }
}

/// Whether `stmt` (a completed, delimiter-stripped statement) is a Postgres
/// `COPY … FROM stdin` whose data follows inline. Leading comments/whitespace
/// are skipped; the check is `COPY` … `FROM stdin` with no `(` data source.
fn is_copy_from_stdin(stmt: &str) -> bool {
    let s = strip_leading_noise(stmt);
    let upper = s.to_ascii_uppercase();
    upper.starts_with("COPY ") && upper.contains(" FROM STDIN")
}

/// Drop leading whitespace and `--`/`#` line comments and `/* */` blocks so a
/// statement's first real keyword can be matched. Used only for classification
/// peeks; the full text is preserved elsewhere.
fn strip_leading_noise(s: &str) -> &str {
    let mut rest = s.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("--").or_else(|| rest.strip_prefix('#')) {
            // Skip to end of line.
            match after.find('\n') {
                Some(nl) => rest = after[nl + 1..].trim_start(),
                None => return "",
            }
        } else if let Some(after) = rest.strip_prefix("/*") {
            match after.find("*/") {
                Some(end) => rest = after[end + 2..].trim_start(),
                None => return "",
            }
        } else {
            return rest;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::utf8::Charset;

    fn split(input: &str, dialect: Dialect) -> Vec<String> {
        let reader = DumpReader::new(Cursor::new(input.as_bytes().to_vec()), dialect);
        reader.map(|r| r.unwrap().sql.trim().to_string()).collect()
    }

    #[test]
    fn splits_simple_statements() {
        let out = split("SELECT 1;\nSELECT 2;\n", Dialect::MySql);
        assert_eq!(out, vec!["SELECT 1", "SELECT 2"]);
    }

    #[test]
    fn semicolon_inside_a_string_does_not_split() {
        let out = split("INSERT INTO t VALUES ('a;b', 'c');\n", Dialect::MySql);
        assert_eq!(out, vec!["INSERT INTO t VALUES ('a;b', 'c')"]);
    }

    #[test]
    fn mysql_backslash_escape_inside_string() {
        // The escaped quote must not end the string, so the `;` stays inside.
        let out = split(r"INSERT INTO t VALUES ('a\';b');", Dialect::MySql);
        assert_eq!(out, vec![r"INSERT INTO t VALUES ('a\';b')"]);
    }

    #[test]
    fn doubled_quote_escape_inside_string() {
        let out = split("INSERT INTO t VALUES ('it''s; fine');", Dialect::Pg);
        assert_eq!(out, vec!["INSERT INTO t VALUES ('it''s; fine')"]);
    }

    #[test]
    fn semicolon_inside_line_and_block_comments_does_not_split() {
        let out = split("SELECT 1; -- a; comment\nSELECT 2; /* b; c */\nSELECT 3;", Dialect::MySql);
        assert_eq!(out, vec!["SELECT 1", "-- a; comment\nSELECT 2", "/* b; c */\nSELECT 3"]);
    }

    #[test]
    fn backtick_identifier_with_semicolon() {
        let out = split("CREATE TABLE `we;ird` (id int);", Dialect::MySql);
        assert_eq!(out, vec!["CREATE TABLE `we;ird` (id int)"]);
    }

    #[test]
    fn delimiter_change_keeps_a_routine_body_whole() {
        let input = "DELIMITER $$\n\
            CREATE PROCEDURE p()\nBEGIN\n  SELECT 1;\n  SELECT 2;\nEND$$\n\
            DELIMITER ;\nSELECT 3;\n";
        let out = split(input, Dialect::MySql);
        assert_eq!(out.len(), 2, "routine body + trailing select, got {out:?}");
        assert!(out[0].contains("BEGIN"));
        assert!(out[0].contains("SELECT 1;"), "interior `;` preserved: {:?}", out[0]);
        assert!(out[0].contains("END"));
        assert_eq!(out[1], "SELECT 3");
    }

    #[test]
    fn copy_from_stdin_block_is_one_statement() {
        let input = "COPY public.t (id, v) FROM stdin;\n1\thello; world\n2\t\\N\n\\.\nSELECT 1;\n";
        let out = split(input, Dialect::Pg);
        assert_eq!(out.len(), 2, "copy block + select, got {out:?}");
        assert!(out[0].starts_with("COPY public.t"));
        assert!(
            out[0].contains("1\thello; world"),
            "semicolon in COPY data not split: {:?}",
            out[0]
        );
        assert!(out[0].contains("2\t\\N"));
        assert_eq!(out[1], "SELECT 1");
    }

    #[test]
    fn trailing_statement_without_a_delimiter_is_flushed() {
        let out = split("SELECT 1;\nSELECT 2", Dialect::MySql);
        assert_eq!(out, vec!["SELECT 1", "SELECT 2"]);
    }

    /// The v0.14.1 row-loss report: one extended `INSERT` whose rows carry
    /// `\'`, `''`, `);`, `,`, a `\\` before a closing quote, and HTML. The
    /// splitter must keep it one statement with every row — this was never
    /// the splitter's bug (the dry run was clean), but it is the invariant the
    /// rest of the fix rests on.
    #[test]
    fn extended_insert_with_escaped_quotes_and_html_stays_one_statement() {
        let rows: Vec<String> = (1..=200)
            .map(|i| match i % 5 {
                0 => format!(r"({i},'O\'Brien & co; it\'s fine')"),
                1 => format!(r"({i},'it''s doubled); not a boundary')"),
                2 => format!(r#"({i},'<a href="x">O\'Brien & co</a>, <b>bold</b>')"#),
                3 => format!(r"({i},'trailing backslash \\')"),
                _ => format!(r"({i},'plain, with comma)')"),
            })
            .collect();
        let dump = format!("INSERT INTO `t` VALUES {};\nSELECT 1;\n", rows.join(","));
        let out = split(&dump, Dialect::MySql);
        assert_eq!(out.len(), 2, "the INSERT and the SELECT, got {} statements", out.len());
        assert_eq!(out[0].matches("),(").count(), 199, "every row must stay in the INSERT");
        assert_eq!(out[1], "SELECT 1");
    }

    // ── UTF-8 policy ───────────────────────────────────────────────────────

    fn read_all(input: &[u8], policy: Utf8Policy) -> Vec<RawStatement> {
        let reader =
            DumpReader::new(Cursor::new(input.to_vec()), Dialect::MySql).with_utf8_policy(policy);
        reader.map(Result::unwrap).collect()
    }

    #[test]
    fn invalid_utf8_never_fails_the_read_and_is_reported_per_statement() {
        // Before v0.14.1 a `String` read_line failed here with a bare
        // "stream did not contain valid UTF-8" — after earlier statements had
        // already been handed out (and committed by the driver).
        let input =
            b"INSERT INTO t VALUES (1,'ok');\nINSERT INTO t VALUES (2,'it\x92s');\nSELECT 1;\n";
        let stmts = read_all(input, Utf8Policy::Replace);
        assert_eq!(stmts.len(), 3);
        assert_eq!(stmts[0].invalid_utf8, Vec::<InvalidRun>::new());
        assert_eq!(stmts[1].sql.trim(), "INSERT INTO t VALUES (2,'it\u{FFFD}s')");
        assert_eq!(stmts[1].invalid_utf8.len(), 1);
        assert_eq!(stmts[1].invalid_utf8[0].bytes, vec![0x92]);
        // The offset points at the replacement inside the statement text.
        let off = stmts[1].invalid_utf8[0].offset;
        assert!(stmts[1].sql[off..].starts_with('\u{FFFD}'), "offset {off} in {:?}", stmts[1].sql);
        assert_eq!(stmts[2].invalid_utf8, Vec::<InvalidRun>::new());
    }

    #[test]
    fn runs_are_rebased_when_several_statements_share_a_line() {
        let input = b"SELECT 'a\x92'; SELECT 'b\x93';\n";
        let stmts = read_all(input, Utf8Policy::Replace);
        assert_eq!(stmts.len(), 2);
        for s in &stmts {
            assert_eq!(s.invalid_utf8.len(), 1, "{s:?}");
            let off = s.invalid_utf8[0].offset;
            assert!(s.sql[off..].starts_with('\u{FFFD}'), "offset {off} in {:?}", s.sql);
        }
        assert_eq!(stmts[1].invalid_utf8[0].bytes, vec![0x93]);
    }

    #[test]
    fn transcode_turns_a_cp1252_quote_into_u2019_and_leaves_utf8_alone() {
        let cs = Charset::parse("windows-1252").unwrap();
        // A raw cp1252 apostrophe (0x92) next to a UTF-8 `é` (C3 A9).
        let raw = b"INSERT INTO t VALUES (1,'it\x92s caf\xC3\xA9');\n";
        let stmts = read_all(raw, Utf8Policy::Transcode(cs));
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0].sql.trim(), "INSERT INTO t VALUES (1,'it\u{2019}s café')");
        assert_eq!(stmts[0].invalid_utf8.len(), 1, "only the 0x92 run is an invalid run");
    }

    #[test]
    fn strip_drops_the_bytes_and_keeps_the_offset_where_they_were() {
        let stmts = read_all(b"INSERT INTO t VALUES (1,'ab\x92\x97cd');\n", Utf8Policy::Strip);
        assert_eq!(stmts[0].sql.trim(), "INSERT INTO t VALUES (1,'abcd')");
        assert_eq!(stmts[0].invalid_utf8.len(), 1);
        assert_eq!(stmts[0].invalid_utf8[0].bytes, vec![0x92, 0x97]);
        assert_eq!(&stmts[0].sql[stmts[0].invalid_utf8[0].offset..], "cd')");
    }

    #[test]
    fn copy_block_rows_carry_their_runs() {
        let input = b"COPY t (id, v) FROM stdin;\n1\tok\n2\tbad\x92\n\\.\n";
        let reader = DumpReader::new(Cursor::new(input.to_vec()), Dialect::Pg)
            .with_utf8_policy(Utf8Policy::Replace);
        let stmts: Vec<RawStatement> = reader.map(Result::unwrap).collect();
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0].invalid_utf8.len(), 1);
        let off = stmts[0].invalid_utf8[0].offset;
        assert!(stmts[0].sql[off..].starts_with('\u{FFFD}'));
        assert!(stmts[0].sql[..off].ends_with("2\tbad"));
    }

    #[test]
    fn line_numbers_track_statement_starts() {
        let input = "SELECT 1;\n\nSELECT 2;\n";
        let reader = DumpReader::new(Cursor::new(input.as_bytes().to_vec()), Dialect::MySql);
        let stmts: Vec<RawStatement> = reader.map(Result::unwrap).collect();
        assert_eq!(stmts[0].line, 1);
        assert_eq!(stmts[1].line, 3);
    }

    #[test]
    fn reads_a_large_input_without_buffering_it_all() {
        // A reader that yields many small statements but tracks the peak bytes
        // ever handed out, proving the splitter pulls incrementally. We feed
        // 200k tiny statements (~2.6 MB) and assert the reader's own live
        // buffer (`pending`) never approaches the input size.
        struct Counting {
            remaining: usize,
            chunk: Vec<u8>,
            pos: usize,
        }
        impl std::io::Read for Counting {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                if self.pos >= self.chunk.len() {
                    if self.remaining == 0 {
                        return Ok(0);
                    }
                    self.remaining -= 1;
                    self.chunk = b"INSERT INTO t VALUES (1);\n".to_vec();
                    self.pos = 0;
                }
                let n = (self.chunk.len() - self.pos).min(out.len());
                out[..n].copy_from_slice(&self.chunk[self.pos..self.pos + n]);
                self.pos += n;
                Ok(n)
            }
        }
        let src = Counting { remaining: 200_000, chunk: Vec::new(), pos: 0 };
        let mut reader = DumpReader::new(std::io::BufReader::new(src), Dialect::MySql);
        let mut count = 0u64;
        let mut peak = 0usize;
        while let Some(stmt) = reader.next_statement().unwrap() {
            peak = peak.max(reader.pending.len());
            assert_eq!(stmt.sql.trim(), "INSERT INTO t VALUES (1)");
            count += 1;
        }
        assert_eq!(count, 200_000);
        assert!(peak < 4096, "the reader held {peak} bytes — it should stay near one statement");
    }
}
