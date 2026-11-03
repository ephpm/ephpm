//! Attribute an invalid-UTF-8 run to the table, column and row it landed in.
//!
//! The compatibility report says `N TEXT values invalid UTF-8 across M rows:
//! posts.body …`, which needs more than "statement at line 1234": for a
//! `mysqldump` extended `INSERT` the run has to be placed in its tuple
//! (the row) and its position within the tuple (the column — named through
//! the `INSERT`'s own column list when it has one, else through the
//! `CREATE TABLE` the driver remembered). A `pg_dump` `COPY` block is simpler:
//! one data line per row, one tab per column.
//!
//! This is a *locating* pass over the raw statement text with the same
//! quote-aware rules the splitter uses, not a parse: an `INSERT … SELECT`, an
//! `UPDATE`, or a `CREATE TABLE` with a non-UTF-8 comment is attributed to the
//! statement's object only.

use crate::Dialect;
use crate::sanitizer::{is_copy_block, object_name, parse_copy_header, table_key, unquote_ident};
use crate::utf8::InvalidRun;

/// Where one [`InvalidRun`] sits inside a statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hit {
    /// 0-based row (tuple / `COPY` line) within the statement; `0` when the
    /// statement has no row structure.
    pub row: usize,
    /// 0-based column position within the row, when the run is inside one.
    pub column: Option<usize>,
}

/// The attribution of every run in one statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Located {
    /// The table the statement writes (unquoted, as written), if identifiable.
    pub table: Option<String>,
    /// The statement's explicit column list, when it has one.
    pub columns: Option<Vec<String>>,
    /// One entry per run, in run order.
    pub hits: Vec<Hit>,
}

impl Located {
    /// The schema-memory key of `table` (see
    /// [`table_key`](crate::sanitizer::table_key)).
    pub(crate) fn table_key(&self) -> Option<String> {
        self.table.as_deref().map(table_key)
    }
}

/// Attribute `runs` (ascending offsets into `sql`) within `sql`.
pub(crate) fn locate(sql: &str, dialect: Dialect, runs: &[InvalidRun]) -> Located {
    if matches!(dialect, Dialect::Pg) && is_copy_block(sql.trim_start()) {
        return locate_in_copy(sql, runs);
    }
    if let Some(located) = locate_in_insert(sql, dialect, runs) {
        return located;
    }
    // Anything else: name the object if we can, no row/column structure.
    let table = object_name(sql).or_else(|| first_table_after(sql, &["INTO", "UPDATE", "TABLE"]));
    Located {
        table,
        columns: None,
        hits: runs.iter().map(|_| Hit { row: 0, column: None }).collect(),
    }
}

/// `COPY tbl (cols) FROM stdin` + data lines: row = data line index, column =
/// number of tabs before the run on that line.
fn locate_in_copy(sql: &str, runs: &[InvalidRun]) -> Located {
    let sql = sql.trim_start();
    let header_end = sql.find('\n').unwrap_or(sql.len());
    let (table, cols) = parse_copy_header(sql[..header_end].trim().trim_end_matches(';'));
    let columns = cols.map(|c| c.split(',').map(unquote_ident).collect::<Vec<_>>());
    // Data starts after the header newline.
    let data_start = (header_end + 1).min(sql.len());
    let hits = runs
        .iter()
        .map(|run| {
            if run.offset < data_start {
                return Hit { row: 0, column: None };
            }
            let before = &sql[data_start..run.offset.min(sql.len())];
            let row = before.matches('\n').count();
            let line_start = before.rfind('\n').map_or(0, |p| p + 1);
            let column = before[line_start..].matches('\t').count();
            Hit { row, column: Some(column) }
        })
        .collect();
    Located { table: Some(unquote_ident(&table)), columns, hits }
}

/// `INSERT|REPLACE [modifiers] [INTO] tbl [(cols)] VALUES (…),(…)`: walk the
/// tuples quote-aware and place each run by tuple ordinal and top-level comma
/// count. `None` when `sql` is not a `VALUES` insert.
fn locate_in_insert(sql: &str, dialect: Dialect, runs: &[InvalidRun]) -> Option<Located> {
    let mysql = matches!(dialect, Dialect::MySql);
    let b = sql.as_bytes();
    let mut i = skip_noise(sql, 0, mysql);

    let (kw, next) = keyword_at(sql, i);
    if !matches!(kw.as_str(), "INSERT" | "REPLACE") {
        return None;
    }
    i = next;
    // Modifiers and the optional INTO.
    loop {
        i = skip_noise(sql, i, mysql);
        let (kw, next) = keyword_at(sql, i);
        match kw.as_str() {
            "INTO" | "IGNORE" | "LOW_PRIORITY" | "DELAYED" | "HIGH_PRIORITY" => i = next,
            _ => break,
        }
    }
    // Table name: quoted or bare, possibly schema-qualified.
    let (table, next) = identifier_at(sql, i)?;
    i = skip_noise(sql, next, mysql);

    // Optional column list.
    let mut columns = None;
    if b.get(i) == Some(&b'(') {
        let close = close_paren(sql, i, mysql)?;
        let inner = &sql[i + 1..close];
        // A `(` here could also open a `(SELECT …)` source; only a list of
        // identifiers is a column list.
        let names: Vec<String> = inner.split(',').map(unquote_ident).collect();
        if names.iter().all(|n| !n.is_empty() && !n.contains(char::is_whitespace)) {
            columns = Some(names);
            i = skip_noise(sql, close + 1, mysql);
        }
    }

    let (kw, next) = keyword_at(sql, i);
    if !matches!(kw.as_str(), "VALUES" | "VALUE") {
        return None;
    }
    i = next;

    // Walk the tuples.
    let mut hits = Vec::with_capacity(runs.len());
    let mut ri = 0usize;
    let mut depth = 0usize;
    let mut row: Option<usize> = None;
    let mut column = 0usize;
    let mut assign = |ri: &mut usize, upto: usize, row: Option<usize>, column: usize| {
        while *ri < runs.len() && runs[*ri].offset < upto {
            hits.push(Hit { row: row.unwrap_or(0), column: row.map(|_| column) });
            *ri += 1;
        }
    };
    while i < b.len() {
        let c = b[i];
        match c {
            b'\'' | b'"' | b'`' => {
                let end = quoted_end(b, i, c, mysql && c != b'`');
                // A run inside the literal belongs to the current cell; one
                // that `Strip` left exactly at the closing quote does too.
                assign(&mut ri, end, row, column);
                i = end;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                let end = sql[i..].find('\n').map_or(b.len(), |p| i + p);
                assign(&mut ri, end, row, column);
                i = end;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let end = sql[i + 2..].find("*/").map_or(b.len(), |p| i + 2 + p + 2);
                assign(&mut ri, end, row, column);
                i = end;
            }
            b'(' => {
                if depth == 0 {
                    row = Some(row.map_or(0, |r| r + 1));
                    column = 0;
                }
                depth += 1;
                assign(&mut ri, i + 1, row, column);
                i += 1;
            }
            b')' => {
                assign(&mut ri, i + 1, row, column);
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b',' if depth == 1 => {
                assign(&mut ri, i + 1, row, column);
                column += 1;
                i += 1;
            }
            _ => {
                assign(&mut ri, i + 1, row, column);
                i += 1;
            }
        }
    }
    assign(&mut ri, usize::MAX, row, column);

    Some(Located { table: Some(table), columns, hits })
}

/// Skip whitespace and comments from `from`.
fn skip_noise(sql: &str, mut from: usize, mysql: bool) -> usize {
    let b = sql.as_bytes();
    loop {
        while from < b.len() && b[from].is_ascii_whitespace() {
            from += 1;
        }
        if b.get(from) == Some(&b'-') && b.get(from + 1) == Some(&b'-')
            || (mysql && b.get(from) == Some(&b'#'))
        {
            from = sql[from..].find('\n').map_or(b.len(), |p| from + p + 1);
        } else if b.get(from) == Some(&b'/') && b.get(from + 1) == Some(&b'*') {
            from = sql[from + 2..].find("*/").map_or(b.len(), |p| from + 2 + p + 2);
        } else {
            return from;
        }
    }
}

/// The uppercased run of `[A-Za-z_]` at `at`, and the index past it.
fn keyword_at(sql: &str, at: usize) -> (String, usize) {
    let len = sql[at.min(sql.len())..]
        .bytes()
        .take_while(|c| c.is_ascii_alphabetic() || *c == b'_')
        .count();
    (sql[at..at + len].to_ascii_uppercase(), at + len)
}

/// A (possibly schema-qualified, possibly quoted) identifier at `at`,
/// unquoted, and the index past it.
fn identifier_at(sql: &str, at: usize) -> Option<(String, usize)> {
    let b = sql.as_bytes();
    let mut i = at;
    let mut parts: Vec<String> = Vec::new();
    loop {
        let c = *b.get(i)?;
        if c == b'`' || c == b'"' {
            let end = quoted_end(b, i, c, false);
            parts.push(unquote_ident(&sql[i..end]));
            i = end;
        } else {
            let len = sql[i..]
                .bytes()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'$'))
                .count();
            if len == 0 {
                return None;
            }
            parts.push(sql[i..i + len].to_string());
            i += len;
        }
        if b.get(i) == Some(&b'.') {
            i += 1;
            continue;
        }
        return Some((parts.join("."), i));
    }
}

/// Index past the quoted region opened at `open` (quote byte `q`).
fn quoted_end(b: &[u8], open: usize, q: u8, backslash: bool) -> usize {
    let mut i = open + 1;
    while i < b.len() {
        if backslash && b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] == q {
            if b.get(i + 1) == Some(&q) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

/// Index of the `)` matching the `(` at `open`, quote-aware.
fn close_paren(sql: &str, open: usize, mysql: bool) -> Option<usize> {
    let b = sql.as_bytes();
    let mut depth = 0usize;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            c @ (b'\'' | b'"' | b'`') => i = quoted_end(b, i, c, mysql && c != b'`'),
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// The identifier following the first of `kws` (uppercase) in `sql`, for
/// labelling non-INSERT statements.
fn first_table_after(sql: &str, kws: &[&str]) -> Option<String> {
    let mut it = sql.split_whitespace().peekable();
    while let Some(tok) = it.next() {
        if kws.contains(&tok.to_ascii_uppercase().as_str()) {
            let name = it.peek()?;
            return Some(unquote_ident(name.trim_end_matches(['(', ';'])));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs_at(sql: &str, needle: char) -> Vec<InvalidRun> {
        sql.match_indices(needle)
            .map(|(offset, _)| InvalidRun { offset, bytes: vec![0x92] })
            .collect()
    }

    #[test]
    fn mysql_extended_insert_runs_land_in_their_row_and_column() {
        let sql = "INSERT INTO `posts` VALUES (1,'a\u{FFFD}b','ok'),(2,'fine','x\u{FFFD}'),(3,'\u{FFFD}','\u{FFFD}')";
        let runs = runs_at(sql, '\u{FFFD}');
        let located = locate(sql, Dialect::MySql, &runs);
        assert_eq!(located.table.as_deref(), Some("posts"));
        assert_eq!(located.columns, None);
        assert_eq!(
            located.hits,
            vec![
                Hit { row: 0, column: Some(1) },
                Hit { row: 1, column: Some(2) },
                Hit { row: 2, column: Some(1) },
                Hit { row: 2, column: Some(2) },
            ]
        );
    }

    #[test]
    fn escapes_and_nested_parens_do_not_move_the_cursor() {
        // `\'` and `''` inside literals, a `,` and `)` inside a literal, a
        // nested function call in a cell, and a comment between rows.
        let sql = "INSERT IGNORE INTO db.t (a, b, c) VALUES \
                   (1,'O\\'Brien, (x))','\u{FFFD}'), /* c */ \
                   (2,'it''s',NOW(3, '\u{FFFD}')),\n-- row\n(3,\"q\\\"\",'\u{FFFD}')";
        let runs = runs_at(sql, '\u{FFFD}');
        let located = locate(sql, Dialect::MySql, &runs);
        assert_eq!(located.table.as_deref(), Some("db.t"));
        assert_eq!(
            located.columns.as_deref(),
            Some(&["a".to_string(), "b".into(), "c".into()][..])
        );
        assert_eq!(
            located.hits,
            vec![
                Hit { row: 0, column: Some(2) },
                Hit { row: 1, column: Some(2) },
                Hit { row: 2, column: Some(2) },
            ]
        );
    }

    #[test]
    fn copy_block_runs_land_by_line_and_tab() {
        let sql =
            "COPY public.t (id, v, w) FROM stdin;\n1\tok\tfine\n2\tb\u{FFFD}d\tz\u{FFFD}\n\\.";
        let runs = runs_at(sql, '\u{FFFD}');
        let located = locate(sql, Dialect::Pg, &runs);
        assert_eq!(located.table.as_deref(), Some("public.t"));
        assert_eq!(located.columns.as_deref().map(<[String]>::len), Some(3));
        assert_eq!(
            located.hits,
            vec![Hit { row: 1, column: Some(1) }, Hit { row: 1, column: Some(2) }]
        );
    }

    #[test]
    fn non_insert_statements_name_the_object_only() {
        let sql = "UPDATE `t` SET v = 'x\u{FFFD}' WHERE id = 1";
        let located = locate(sql, Dialect::MySql, &runs_at(sql, '\u{FFFD}'));
        assert_eq!(located.table.as_deref(), Some("t"));
        assert_eq!(located.hits, vec![Hit { row: 0, column: None }]);

        let sql = "CREATE TABLE `t` (`v` text COMMENT 'caf\u{FFFD}')";
        let located = locate(sql, Dialect::MySql, &runs_at(sql, '\u{FFFD}'));
        assert_eq!(located.table.as_deref(), Some("t"));
        assert_eq!(located.hits, vec![Hit { row: 0, column: None }]);

        // INSERT … SELECT has no tuples.
        let sql = "INSERT INTO t SELECT * FROM u WHERE v = '\u{FFFD}'";
        let located = locate(sql, Dialect::MySql, &runs_at(sql, '\u{FFFD}'));
        assert_eq!(located.table.as_deref(), Some("t"));
        assert_eq!(located.hits, vec![Hit { row: 0, column: None }]);
    }
}
