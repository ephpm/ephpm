//! Statement-level sanitizer: turn one [`RawStatement`] into a
//! [`SanitizeOutcome`] an ePHPm Turso database can accept, recording a
//! [`Finding`] whenever the result is not a verbatim pass-through.
//!
//! This is file-format / dialect-*shape* work only. It never translates SQL
//! *types* or *expressions* to SQLite — that is litewire's job, downstream of
//! the [`StatementSink`](crate::StatementSink). What it does:
//!
//! * MySQL — unwrap `/*! … */` / `/*M! … */` version gates (dropping the
//!   session-`SET` family, keeping structural inner SQL); drop
//!   `LOCK`/`UNLOCK TABLES` and `ALTER TABLE … {DISABLE|ENABLE} KEYS`; strip
//!   trailing `CREATE TABLE` storage options; rewrite `ENUM(…)`/`SET(…)`
//!   columns to `TEXT`; drop `FULLTEXT`/`SPATIAL` inline indexes; pass
//!   multi-row `INSERT`s through unchanged.
//! * PostgreSQL — convert a `COPY … FROM stdin` block to batched `INSERT`s;
//!   drop `SET`, `SELECT pg_catalog.set_config`, `\connect`, `COMMENT ON`,
//!   `GRANT`/`REVOKE`, `… OWNER TO`, `CREATE EXTENSION`; downgrade
//!   `CREATE SEQUENCE` (→ AUTOINCREMENT, different semantics).
//! * Both — classify stored routines / triggers / events / full-text / spatial
//!   as [`Unsupported`](crate::FindingKind::Unsupported).

use crate::dialect::Dialect;
use crate::reader::RawStatement;
use crate::report::{Finding, FindingKind};

/// What to do with one statement.
pub enum SanitizeOutcome {
    /// Emit these statement(s) verbatim (no compatibility concern).
    Emit(Vec<String>),
    /// Drop the statement as a safe no-op; the string is the reason (surfaced
    /// only as a [`Dropped`](FindingKind::Dropped) finding under `--verbose`).
    Drop(String),
    /// Do not emit anything, and record this finding (typically
    /// [`Unsupported`](FindingKind::Unsupported)).
    Skip(Finding),
    /// Emit these statement(s) *and* record these finding(s) (typically
    /// [`Downgraded`](FindingKind::Downgraded)).
    ///
    /// The finding list is a `Vec` rather than the single `Finding` of the
    /// original design sketch: one `CREATE TABLE` can carry several distinct
    /// compatibility concerns at once (e.g. an `ENUM` column *downgraded* to
    /// `TEXT` alongside a `FULLTEXT` index that is *unsupported*), and each
    /// deserves its own correctly-kinded report entry rather than being folded
    /// into one.
    Convert(Vec<String>, Vec<Finding>),
}

/// How many `COPY`-derived rows go into one multi-row `INSERT`.
const COPY_ROWS_PER_INSERT: usize = 256;

/// The dialect-aware statement sanitizer.
pub struct Sanitizer {
    dialect: Dialect,
}

impl Sanitizer {
    /// Create a sanitizer for `dialect`.
    #[must_use]
    pub fn new(dialect: Dialect) -> Self {
        Self { dialect }
    }

    /// Classify and rewrite one statement.
    #[must_use]
    pub fn sanitize(&self, stmt: &RawStatement) -> SanitizeOutcome {
        let line = stmt.line;
        let raw = stmt.sql.trim();
        if raw.is_empty() {
            return SanitizeOutcome::Drop("blank or comment-only".to_string());
        }

        // A COPY block arrives bundled (header + rows + `\.`) from the reader;
        // detect it before flattening, which would mangle the tab-delimited
        // data body.
        if matches!(self.dialect, Dialect::Pg) && is_copy_block(raw) {
            return convert_copy_block(raw, line);
        }

        // Flatten comments and MySQL gate markers (quote-aware) so the first
        // real keyword is visible and gate-wrapped `SET`s become plain `SET`s.
        let flat = flatten(raw, self.dialect);
        let flat = flat.trim();
        if flat.is_empty() {
            return SanitizeOutcome::Drop("comment-only after gate stripping".to_string());
        }

        match self.dialect {
            Dialect::MySql => self.sanitize_mysql(flat, line),
            Dialect::Pg => self.sanitize_pg(flat, line),
            // `db import` never constructs a `Sqlite` sanitizer (only `sniff`
            // or an explicit `--dialect mysql|pg` feeds this crate); `Sqlite`
            // exists purely for the `db query`/`db shell` consoles, which run
            // SQL raw and never touch the sanitizer. If one ever reaches here,
            // the statement is already the engine's own dialect — emit it as-is
            // rather than misapplying MySQL/PG rewrites.
            Dialect::Sqlite => SanitizeOutcome::Emit(vec![flat.to_string()]),
        }
    }

    fn sanitize_mysql(&self, flat: &str, line: u64) -> SanitizeOutcome {
        let head = head_upper(flat, 3);

        // Session setters and locking — safe no-ops on SQLite.
        if head.starts_with("SET ") || head == "SET" {
            return SanitizeOutcome::Drop(format!("session setter: {}", snippet(flat)));
        }
        if head.starts_with("LOCK TABLES") || head.starts_with("UNLOCK TABLES") {
            return SanitizeOutcome::Drop("table locking (no-op on SQLite)".to_string());
        }
        if head.starts_with("USE ") {
            return SanitizeOutcome::Drop(
                "database selection (single-database on SQLite)".to_string(),
            );
        }
        if head.starts_with("ALTER TABLE") && contains_kw(flat, &["DISABLE KEYS", "ENABLE KEYS"]) {
            return SanitizeOutcome::Drop("ALTER TABLE … KEYS (no-op on SQLite)".to_string());
        }

        // Stored routines / triggers / events / full-text — unsupported.
        if let Some(kind) = unsupported_object(flat) {
            return SanitizeOutcome::Skip(Finding::new(
                FindingKind::Unsupported,
                object_name(flat).unwrap_or_else(|| kind.to_string()),
                format!("{kind} is not supported by SQLite/Turso; skipped"),
                line,
            ));
        }

        if head.starts_with("CREATE TABLE") {
            return rewrite_create_table(flat, line);
        }

        // Everything else — INSERT/REPLACE, CREATE INDEX, CREATE VIEW (after a
        // DEFINER strip), plain ALTER/DELETE/UPDATE — passes through for
        // litewire to translate.
        if let Some(view) = strip_view_definer(flat) {
            return SanitizeOutcome::Emit(vec![view]);
        }
        SanitizeOutcome::Emit(vec![flat.to_string()])
    }

    fn sanitize_pg(&self, flat: &str, line: u64) -> SanitizeOutcome {
        let head = head_upper(flat, 3);

        if flat.starts_with('\\') {
            return SanitizeOutcome::Drop(format!("psql meta-command: {}", snippet(flat)));
        }
        if head.starts_with("SET ") || head == "SET" {
            return SanitizeOutcome::Drop(format!("session setter: {}", snippet(flat)));
        }
        if head.starts_with("SELECT PG_CATALOG.SET_CONFIG") || head.starts_with("SELECT SET_CONFIG")
        {
            return SanitizeOutcome::Drop("pg_catalog.set_config (session setup)".to_string());
        }
        if head.starts_with("COMMENT ON") {
            return SanitizeOutcome::Drop("COMMENT ON (no column/object comments)".to_string());
        }
        if head.starts_with("GRANT ") || head.starts_with("REVOKE ") {
            return SanitizeOutcome::Drop("GRANT/REVOKE (no SQLite role model)".to_string());
        }
        if head.starts_with("CREATE EXTENSION") {
            return SanitizeOutcome::Drop("CREATE EXTENSION (no extensions on SQLite)".to_string());
        }
        if contains_kw(flat, &["OWNER TO"]) && head.starts_with("ALTER ") {
            return SanitizeOutcome::Drop("ownership change (no SQLite owner model)".to_string());
        }
        if head.starts_with("ALTER SEQUENCE") || contains_kw(flat, &["SETVAL("]) {
            return SanitizeOutcome::Drop(
                "sequence value adjustment (see CREATE SEQUENCE)".to_string(),
            );
        }

        if head.starts_with("CREATE SEQUENCE") {
            return SanitizeOutcome::Skip(Finding::new(
                FindingKind::Downgraded,
                object_name(flat).unwrap_or_else(|| "sequence".to_string()),
                "sequence → SQLite AUTOINCREMENT differs (no standalone sequence object); dropped"
                    .to_string(),
                line,
            ));
        }

        if let Some(kind) = unsupported_object(flat) {
            return SanitizeOutcome::Skip(Finding::new(
                FindingKind::Unsupported,
                object_name(flat).unwrap_or_else(|| kind.to_string()),
                format!("{kind} is not supported by SQLite/Turso; skipped"),
                line,
            ));
        }

        SanitizeOutcome::Emit(vec![flat.to_string()])
    }
}

// ── Classification helpers ──────────────────────────────────────────────────

/// Uppercase the first `words` whitespace-separated tokens, joined by single
/// spaces — a cheap keyword-prefix view for matching.
fn head_upper(s: &str, words: usize) -> String {
    s.split_whitespace().take(words).collect::<Vec<_>>().join(" ").to_ascii_uppercase()
}

/// Whether the uppercased statement contains any of `kws` (already uppercase).
fn contains_kw(s: &str, kws: &[&str]) -> bool {
    let upper = s.to_ascii_uppercase();
    kws.iter().any(|k| upper.contains(k))
}

/// A short single-line snippet of a statement for diagnostics.
fn snippet(s: &str) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.len() > 60 { format!("{}…", &one[..60]) } else { one }
}

/// If `flat` is a `CREATE` of an object SQLite cannot represent, return the
/// object kind keyword; else `None`. Covers stored procedures, functions,
/// triggers, events, and standalone full-text / spatial index creation. The
/// `CREATE` may be prefixed by `OR REPLACE`, `DEFINER=…`, `ALGORITHM=…`, or
/// `SQL SECURITY …` (the mysqldump trigger spelling, flattened).
fn unsupported_object(flat: &str) -> Option<&'static str> {
    let upper = flat.to_ascii_uppercase();
    if !upper.starts_with("CREATE") {
        return None;
    }
    // Scan the token stream, skipping the known CREATE modifiers, to find the
    // object kind.
    let mut tokens = upper.split_whitespace().peekable();
    tokens.next(); // CREATE
    while let Some(tok) = tokens.peek().copied() {
        match tok {
            "OR" | "REPLACE" | "SQL" | "SECURITY" | "DEFINER" | "INVOKER" => {
                tokens.next();
            }
            t if t.starts_with("DEFINER=") || t.starts_with("ALGORITHM=") => {
                tokens.next();
            }
            _ => break,
        }
    }
    match tokens.next() {
        Some("PROCEDURE") => Some("stored procedure"),
        Some("FUNCTION") => Some("stored function"),
        Some("TRIGGER") => Some("trigger"),
        Some("EVENT") => Some("event"),
        Some("FULLTEXT") => Some("full-text index"),
        Some("SPATIAL") => Some("spatial index"),
        _ => None,
    }
}

/// Best-effort name of the object a `CREATE …`/`ALTER …` statement acts on —
/// the first identifier-looking token after the object keyword. Used only to
/// label findings, so an imperfect answer is harmless.
fn object_name(flat: &str) -> Option<String> {
    let mut it = flat.split_whitespace();
    // Walk to the keyword that introduces a name, then take the next token.
    let mut prev_was_kw = false;
    for tok in it.by_ref() {
        let up = tok.to_ascii_uppercase();
        if matches!(
            up.as_str(),
            "TABLE"
                | "TRIGGER"
                | "PROCEDURE"
                | "FUNCTION"
                | "EVENT"
                | "SEQUENCE"
                | "INDEX"
                | "VIEW"
        ) {
            prev_was_kw = true;
            continue;
        }
        if prev_was_kw {
            // Strip common prefixes/suffixes and quoting.
            let name = tok.trim_matches(|c| c == '`' || c == '"' || c == '(' || c == ';');
            if name.eq_ignore_ascii_case("IF")
                || name.eq_ignore_ascii_case("UNIQUE")
                || name.is_empty()
            {
                continue;
            }
            return Some(name.to_string());
        }
    }
    None
}

/// Strip a MySQL `CREATE … VIEW` of its `DEFINER=…`, `ALGORITHM=…`, and
/// `SQL SECURITY …` clauses (which SQLite has no analogue for), returning the
/// cleaned statement — or `None` if `flat` is not a `CREATE VIEW`.
fn strip_view_definer(flat: &str) -> Option<String> {
    let upper = flat.to_ascii_uppercase();
    if !upper.starts_with("CREATE") || !upper.contains(" VIEW ") {
        return None;
    }
    // Only act when there is something to strip, so a plain CREATE VIEW falls
    // through to the verbatim path.
    if !(upper.contains("DEFINER=")
        || upper.contains("ALGORITHM=")
        || upper.contains("SQL SECURITY"))
    {
        return None;
    }
    // Rebuild from the VIEW keyword, keeping `CREATE` + `OR REPLACE`.
    let view_pos = upper.find(" VIEW ")?;
    let prefix = if upper.contains("OR REPLACE") { "CREATE OR REPLACE" } else { "CREATE" };
    Some(format!("{prefix}{}", &flat[view_pos..]))
}

// ── CREATE TABLE rewriting ──────────────────────────────────────────────────

/// Rewrite a (flattened) MySQL `CREATE TABLE`: strip trailing storage options,
/// rewrite `ENUM`/`SET` columns to `TEXT`, and drop inline `FULLTEXT`/`SPATIAL`
/// indexes — recording a [`Finding`] when anything was downgraded or dropped.
fn rewrite_create_table(flat: &str, line: u64) -> SanitizeOutcome {
    let table = object_name(flat).unwrap_or_else(|| "table".to_string());
    let Some(open) = flat.find('(') else {
        return SanitizeOutcome::Emit(vec![flat.to_string()]);
    };
    let Some(close) = matching_paren(flat, open) else {
        return SanitizeOutcome::Emit(vec![flat.to_string()]);
    };

    let header = flat[..open].trim_end();
    let body = &flat[open + 1..close];
    // Everything after the matching ')' is table options (ENGINE=, DEFAULT
    // CHARSET=, COLLATE=, ROW_FORMAT=, AUTO_INCREMENT=, COMMENT=, PARTITION
    // BY …) — none representable on SQLite, all dropped.

    let mut findings: Vec<Finding> = Vec::new();
    let mut kept: Vec<String> = Vec::new();
    for def in split_top_level_commas(body) {
        let def = def.trim();
        if def.is_empty() {
            continue;
        }
        let up = def.to_ascii_uppercase();
        if up.starts_with("FULLTEXT") || up.starts_with("SPATIAL") {
            findings.push(Finding::new(
                FindingKind::Unsupported,
                table.clone(),
                format!(
                    "inline {} index dropped",
                    if up.starts_with("FULLTEXT") { "full-text" } else { "spatial" }
                ),
                line,
            ));
            continue;
        }
        if let Some((rewritten, col)) = rewrite_enum_set_column(def) {
            findings.push(Finding::new(
                FindingKind::Downgraded,
                format!("{table}.{col}"),
                "ENUM/SET column stored as TEXT (no value constraint enforced)".to_string(),
                line,
            ));
            kept.push(rewritten);
        } else {
            kept.push(def.to_string());
        }
    }

    let sql = format!("{header} (\n  {}\n)", kept.join(",\n  "));
    if findings.is_empty() {
        SanitizeOutcome::Emit(vec![sql])
    } else {
        SanitizeOutcome::Convert(vec![sql], findings)
    }
}

/// If a column definition's type is `ENUM(…)` or `SET(…)`, rewrite it to
/// `TEXT` (keeping the column name and the trailing attributes), returning
/// `(rewritten_def, column_name)`. Otherwise `None`.
fn rewrite_enum_set_column(def: &str) -> Option<(String, String)> {
    // The column name is the first token (possibly backtick-quoted).
    let mut it = def.splitn(2, char::is_whitespace);
    let name_tok = it.next()?;
    let rest_trim = it.next()?.trim_start();
    let up = rest_trim.to_ascii_uppercase();
    let is_type = up.starts_with("ENUM(")
        || up.starts_with("ENUM (")
        || up.starts_with("SET(")
        || up.starts_with("SET (");
    if !is_type {
        return None;
    }
    // Find the matching close paren of the type's value list.
    let open_abs = rest_trim.find('(')?;
    let close = matching_paren(rest_trim, open_abs)?;
    let after = rest_trim[close + 1..].trim_start();
    let col = name_tok.trim_matches('`').trim_matches('"').to_string();
    let rewritten = if after.is_empty() {
        format!("{name_tok} TEXT")
    } else {
        format!("{name_tok} TEXT {after}")
    };
    Some((rewritten, col))
}

// ── COPY FROM stdin → INSERT ─────────────────────────────────────────────────

/// Whether `raw` is a bundled `COPY … FROM stdin` block (header + data rows).
fn is_copy_block(raw: &str) -> bool {
    let first = raw.lines().next().unwrap_or("");
    let up = first.to_ascii_uppercase();
    up.trim_start().starts_with("COPY ") && up.contains("FROM STDIN")
}

/// Convert a `COPY tbl (cols) FROM stdin` block into batched multi-row
/// `INSERT`s. `\N` becomes `NULL`; `\t \n \r \\` are unescaped; every other
/// field becomes a single-quoted literal (SQLite applies column affinity).
fn convert_copy_block(raw: &str, line: u64) -> SanitizeOutcome {
    let mut lines = raw.split('\n');
    let header = lines.next().unwrap_or("").trim().trim_end_matches(';');

    // Parse `COPY <table> [(cols)] FROM stdin`.
    let (table, cols) = parse_copy_header(header);
    let col_clause = cols.map_or_else(String::new, |c| format!(" ({c})"));

    let mut rows: Vec<String> = Vec::new();
    for data in lines {
        if data == "\\." {
            break;
        }
        // A genuinely empty trailing line (from the split) is not a row.
        if data.is_empty() {
            continue;
        }
        let fields = data.split('\t').map(copy_field_to_sql).collect::<Vec<_>>();
        rows.push(format!("({})", fields.join(", ")));
    }

    if rows.is_empty() {
        return SanitizeOutcome::Drop(format!("empty COPY block for {table}"));
    }

    let inserts = rows
        .chunks(COPY_ROWS_PER_INSERT)
        .map(|chunk| format!("INSERT INTO {table}{col_clause} VALUES {}", chunk.join(", ")))
        .collect::<Vec<_>>();

    let _ = line;
    SanitizeOutcome::Emit(inserts)
}

/// Parse the table name and optional column list out of a `COPY … FROM stdin`
/// header. Returns `(table, Some(cols))` or `(table, None)`.
fn parse_copy_header(header: &str) -> (String, Option<String>) {
    // Drop the leading `COPY `.
    let rest = header.trim_start();
    let rest = rest[rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len())..].trim_start();
    if let Some(open) = rest.find('(') {
        let table = rest[..open].trim().to_string();
        let close = rest[open..].find(')').map_or(rest.len(), |p| open + p);
        let cols = rest[open + 1..close].trim().to_string();
        (table, Some(cols))
    } else {
        // `COPY tbl FROM stdin` — table is the token before FROM.
        let up = rest.to_ascii_uppercase();
        let end = up.find(" FROM").unwrap_or(rest.len());
        (rest[..end].trim().to_string(), None)
    }
}

/// Convert one COPY field to a SQL value literal.
fn copy_field_to_sql(field: &str) -> String {
    if field == "\\N" {
        return "NULL".to_string();
    }
    let mut out = String::with_capacity(field.len() + 2);
    out.push('\'');
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('\\') | None => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
            }
        } else if c == '\'' {
            out.push('\'');
            out.push('\'');
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

// ── Lexical helpers ──────────────────────────────────────────────────────────

/// Index of the `)` matching the `(` at `open`, quote/backtick aware. `None`
/// if unbalanced.
fn matching_paren(s: &str, open: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut i = open;
    let mut quote: Option<u8> = None;
    while i < b.len() {
        let c = b[i];
        if let Some(q) = quote {
            if c == b'\\' && q != b'`' {
                i += 2;
                continue;
            }
            if c == q {
                if i + 1 < b.len() && b[i + 1] == q {
                    i += 2;
                    continue;
                }
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' | b'`' => quote = Some(c),
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Split `s` on top-level commas (not inside parens, quotes, or backticks).
fn split_top_level_commas(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut i = 0usize;
    let mut quote: Option<u8> = None;
    while i < b.len() {
        let c = b[i];
        if let Some(q) = quote {
            if c == b'\\' && q != b'`' {
                i += 2;
                continue;
            }
            if c == q {
                if i + 1 < b.len() && b[i + 1] == q {
                    i += 2;
                    continue;
                }
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' | b'`' => quote = Some(c),
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                parts.push(s[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(s[start..].to_string());
    parts
}

/// Remove comments from `s`, quote-aware, and for MySQL unwrap `/*! … */` /
/// `/*M! … */` executable comments to their inner SQL (dropping the gate
/// markers and the version number). String/identifier literals are preserved
/// byte-for-byte.
fn flatten(s: &str, dialect: Dialect) -> String {
    let ms = matches!(dialect, Dialect::MySql);
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let byte = bytes[i];
        match byte {
            b'\'' | b'"' => {
                let (next, text) = copy_quoted(s, i, byte, ms);
                out.push_str(text);
                i = next;
            }
            b'`' if ms => {
                let (next, text) = copy_quoted(s, i, b'`', false);
                out.push_str(text);
                i = next;
            }
            b'-' if i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                i = skip_to_eol(bytes, i);
                out.push(' ');
            }
            b'#' if ms => {
                i = skip_to_eol(bytes, i);
                out.push(' ');
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                let is_exec = ms
                    && i + 2 < bytes.len()
                    && (bytes[i + 2] == b'!'
                        || (bytes[i + 2] == b'M' && i + 3 < bytes.len() && bytes[i + 3] == b'!'));
                if is_exec {
                    // Skip `/*!` or `/*M!` and the version digits.
                    let mut cur = i + 2;
                    if bytes[cur] == b'M' {
                        cur += 1;
                    }
                    cur += 1; // the '!'
                    while cur < bytes.len() && bytes[cur].is_ascii_digit() {
                        cur += 1;
                    }
                    let end = find_block_end(bytes, cur);
                    out.push(' ');
                    out.push_str(&s[cur..end]);
                    out.push(' ');
                    i = if end + 2 <= bytes.len() { end + 2 } else { bytes.len() };
                } else {
                    let end = find_block_end(bytes, i + 2);
                    out.push(' ');
                    i = if end + 2 <= bytes.len() { end + 2 } else { bytes.len() };
                }
            }
            _ => {
                out.push(byte as char);
                i += 1;
            }
        }
    }
    out
}

/// Copy a quoted region starting at the opening quote `open` (byte `quote`),
/// returning `(index_past_region, slice)`.
fn copy_quoted(s: &str, open: usize, quote: u8, mysql_escapes: bool) -> (usize, &str) {
    let b = s.as_bytes();
    let mut i = open + 1;
    while i < b.len() {
        let c = b[i];
        if mysql_escapes && c == b'\\' {
            i += 2;
            continue;
        }
        if c == quote {
            if i + 1 < b.len() && b[i + 1] == quote {
                i += 2;
                continue;
            }
            return (i + 1, &s[open..=i]);
        }
        i += 1;
    }
    (b.len(), &s[open..])
}

/// Advance past a `--`/`#` line comment: return the index of the newline (kept
/// so the caller's newline bookkeeping is unaffected) or end of input.
fn skip_to_eol(b: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < b.len() && b[i] != b'\n' {
        i += 1;
    }
    i
}

/// Index of the `*` in the `*/` that ends a block comment opened at `from`
/// (i.e. just past `/*`). Returns the end-of-input index if unterminated.
fn find_block_end(b: &[u8], from: usize) -> usize {
    let mut i = from;
    while i + 1 < b.len() {
        if b[i] == b'*' && b[i + 1] == b'/' {
            return i;
        }
        i += 1;
    }
    b.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stmt(sql: &str) -> RawStatement {
        RawStatement { sql: sql.to_string(), line: 1 }
    }

    fn emit(outcome: SanitizeOutcome) -> Vec<String> {
        match outcome {
            SanitizeOutcome::Emit(v) | SanitizeOutcome::Convert(v, _) => v,
            other => panic!("expected Emit/Convert, got {}", outcome_kind(&other)),
        }
    }

    fn outcome_kind(o: &SanitizeOutcome) -> &'static str {
        match o {
            SanitizeOutcome::Emit(_) => "Emit",
            SanitizeOutcome::Drop(_) => "Drop",
            SanitizeOutcome::Skip(_) => "Skip",
            SanitizeOutcome::Convert(_, _) => "Convert",
        }
    }

    #[test]
    fn drops_mysql_session_set_gate() {
        let s = Sanitizer::new(Dialect::MySql);
        let o = s.sanitize(&stmt("/*!40101 SET @saved_cs_client = @@character_set_client */"));
        assert!(matches!(o, SanitizeOutcome::Drop(_)));
    }

    #[test]
    fn keeps_structural_inner_sql_from_a_gate() {
        let s = Sanitizer::new(Dialect::MySql);
        // A gate wrapping real DDL keeps the inner statement.
        let o = s.sanitize(&stmt("/*!40000 ALTER TABLE `t` DISABLE KEYS */"));
        // ALTER … DISABLE KEYS is itself a no-op → dropped.
        assert!(matches!(o, SanitizeOutcome::Drop(_)));
    }

    #[test]
    fn drops_lock_tables() {
        let s = Sanitizer::new(Dialect::MySql);
        assert!(matches!(s.sanitize(&stmt("LOCK TABLES `t` WRITE")), SanitizeOutcome::Drop(_)));
        assert!(matches!(s.sanitize(&stmt("UNLOCK TABLES")), SanitizeOutcome::Drop(_)));
    }

    #[test]
    fn multi_row_insert_passes_through() {
        let s = Sanitizer::new(Dialect::MySql);
        let sql = "INSERT INTO `t` VALUES (1,'a'),(2,'b'),(3,'c')";
        let out = emit(s.sanitize(&stmt(sql)));
        assert_eq!(out, vec![sql.to_string()]);
    }

    #[test]
    fn create_table_strips_options_and_rewrites_enum() {
        let s = Sanitizer::new(Dialect::MySql);
        let sql = "CREATE TABLE `posts` (\n  `id` int NOT NULL,\n  `status` enum('draft','pub') NOT NULL DEFAULT 'draft',\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci AUTO_INCREMENT=5";
        let o = s.sanitize(&stmt(sql));
        let SanitizeOutcome::Convert(out, findings) = o else { panic!("expected Convert") };
        let joined = out.join("\n");
        assert!(!joined.to_ascii_uppercase().contains("ENGINE="), "options not stripped: {joined}");
        assert!(!joined.to_ascii_uppercase().contains("ENUM("), "enum not rewritten: {joined}");
        assert!(joined.contains("`status` TEXT"), "enum→TEXT missing: {joined}");
        assert!(joined.contains("DEFAULT 'draft'"), "trailing attrs lost: {joined}");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FindingKind::Downgraded);
        assert_eq!(findings[0].object, "posts.status");
    }

    #[test]
    fn create_table_drops_fulltext_index() {
        let s = Sanitizer::new(Dialect::MySql);
        let sql = "CREATE TABLE `a` (\n  `id` int,\n  `body` text,\n  FULLTEXT KEY `ft` (`body`)\n) ENGINE=InnoDB";
        let o = s.sanitize(&stmt(sql));
        let SanitizeOutcome::Convert(out, findings) = o else { panic!("expected Convert") };
        assert!(!out.join("\n").to_ascii_uppercase().contains("FULLTEXT"));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FindingKind::Unsupported);
    }

    #[test]
    fn create_trigger_is_unsupported() {
        let s = Sanitizer::new(Dialect::MySql);
        // mysqldump's gate-split spelling.
        let sql = "/*!50003 CREATE*/ /*!50017 DEFINER=`root`@`localhost`*/ /*!50003 TRIGGER `trg` BEFORE INSERT ON `t` FOR EACH ROW BEGIN SET NEW.x = 1; END */";
        let o = s.sanitize(&stmt(sql));
        let SanitizeOutcome::Skip(f) = o else { panic!("expected Skip, got {}", outcome_kind(&o)) };
        assert_eq!(f.kind, FindingKind::Unsupported);
        assert!(f.detail.contains("trigger"));
    }

    #[test]
    fn create_procedure_is_unsupported() {
        let s = Sanitizer::new(Dialect::MySql);
        let sql = "CREATE DEFINER=`root`@`localhost` PROCEDURE `p`() BEGIN SELECT 1; END";
        let o = s.sanitize(&stmt(sql));
        assert!(matches!(o, SanitizeOutcome::Skip(f) if f.kind == FindingKind::Unsupported));
    }

    #[test]
    fn pg_copy_block_converts_to_inserts() {
        let s = Sanitizer::new(Dialect::Pg);
        let block =
            "COPY public.users (id, name, note) FROM stdin;\n1\tAlice\thi\n2\tBob\t\\N\n\\.";
        let out = emit(s.sanitize(&stmt(block)));
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("INSERT INTO public.users (id, name, note) VALUES "));
        assert!(out[0].contains("('1', 'Alice', 'hi')"));
        assert!(out[0].contains("('2', 'Bob', NULL)"), "\\N → NULL: {}", out[0]);
    }

    #[test]
    fn pg_copy_unescapes_tabs_and_newlines() {
        let s = Sanitizer::new(Dialect::Pg);
        let block = "COPY t (v) FROM stdin;\na\\tb\\nc\n\\.";
        let out = emit(s.sanitize(&stmt(block)));
        assert!(out[0].contains("('a\tb\nc')"), "escapes not decoded: {:?}", out[0]);
    }

    #[test]
    fn pg_drops_session_and_ownership() {
        let s = Sanitizer::new(Dialect::Pg);
        assert!(matches!(s.sanitize(&stmt("SET statement_timeout = 0")), SanitizeOutcome::Drop(_)));
        assert!(matches!(
            s.sanitize(&stmt("ALTER TABLE public.t OWNER TO postgres")),
            SanitizeOutcome::Drop(_)
        ));
        assert!(matches!(
            s.sanitize(&stmt("SELECT pg_catalog.set_config('search_path', '', false)")),
            SanitizeOutcome::Drop(_)
        ));
        assert!(matches!(
            s.sanitize(&stmt("CREATE EXTENSION IF NOT EXISTS pgcrypto WITH SCHEMA public")),
            SanitizeOutcome::Drop(_)
        ));
    }

    #[test]
    fn pg_sequence_is_downgraded() {
        let s = Sanitizer::new(Dialect::Pg);
        let o =
            s.sanitize(&stmt("CREATE SEQUENCE public.users_id_seq START WITH 1 INCREMENT BY 1"));
        let SanitizeOutcome::Skip(f) = o else { panic!("expected Skip") };
        assert_eq!(f.kind, FindingKind::Downgraded);
        assert!(f.detail.to_ascii_lowercase().contains("sequence"));
    }

    #[test]
    fn pg_insert_passes_through() {
        let s = Sanitizer::new(Dialect::Pg);
        let sql = "INSERT INTO public.t (id, v) VALUES (1, 'x')";
        assert_eq!(emit(s.sanitize(&stmt(sql))), vec![sql.to_string()]);
    }

    #[test]
    fn mysql_view_definer_is_stripped() {
        let s = Sanitizer::new(Dialect::MySql);
        let sql = "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`localhost` SQL SECURITY DEFINER VIEW `v` AS SELECT 1";
        let out = emit(s.sanitize(&stmt(sql)));
        assert!(!out[0].to_ascii_uppercase().contains("DEFINER="), "definer kept: {}", out[0]);
        assert!(out[0].to_ascii_uppercase().starts_with("CREATE VIEW"), "got {}", out[0]);
    }
}
