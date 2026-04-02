//! SQL dialect translation for litewire.
//!
//! Translates MySQL, PostgreSQL, and T-SQL dialects to SQLite-compatible SQL
//! using `sqlparser-rs` for parsing and AST manipulation.

pub mod common;
pub mod emit;
pub mod metadata;
pub mod mysql;
pub mod postgres;
pub mod tds;

use sqlparser::ast::Statement;
use sqlparser::dialect::{MySqlDialect, PostgreSqlDialect, MsSqlDialect};
use sqlparser::parser::Parser;

/// Source SQL dialect for translation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    MySQL,
    PostgreSQL,
    TDS,
}

/// Errors during SQL translation.
#[derive(Debug, thiserror::Error)]
pub enum TranslateError {
    #[error("SQL parse error: {0}")]
    Parse(String),

    #[error("unsupported SQL construct: {0}")]
    Unsupported(String),
}

/// Result of translating a SQL statement.
#[derive(Debug)]
pub enum TranslateResult {
    /// Translated SQL string to execute against SQLite.
    Sql(String),
    /// A no-op statement (e.g., `SET NAMES utf8mb4`) -- should return OK without executing.
    Noop,
    /// A metadata query that requires special handling.
    Metadata(metadata::MetadataQuery),
}

/// Translate a SQL string from the given dialect to SQLite.
///
/// Returns one or more translated results (one per statement in the input).
///
/// # Errors
///
/// Returns a [`TranslateError`] if the SQL cannot be parsed or contains
/// unsupported constructs.
pub fn translate(sql: &str, dialect: Dialect) -> Result<Vec<TranslateResult>, TranslateError> {
    // Check for metadata queries first (before parsing -- some are not valid SQL).
    if let Some(meta) = metadata::detect_metadata_query(sql, dialect) {
        return Ok(vec![TranslateResult::Metadata(meta)]);
    }

    // Check for no-op statements.
    if is_noop(sql, dialect) {
        return Ok(vec![TranslateResult::Noop]);
    }

    let parser_dialect: Box<dyn sqlparser::dialect::Dialect> = match dialect {
        Dialect::MySQL => Box::new(MySqlDialect {}),
        Dialect::PostgreSQL => Box::new(PostgreSqlDialect {}),
        Dialect::TDS => Box::new(MsSqlDialect {}),
    };

    let statements =
        Parser::parse_sql(parser_dialect.as_ref(), sql).map_err(|e| TranslateError::Parse(e.to_string()))?;

    let mut results = Vec::with_capacity(statements.len());
    for stmt in statements {
        let rewritten = rewrite_statement(stmt, dialect)?;
        let sqlite_sql = emit::emit_statement(&rewritten);
        results.push(TranslateResult::Sql(sqlite_sql));
    }

    Ok(results)
}

/// Check if a SQL statement is a no-op for SQLite.
fn is_noop(sql: &str, _dialect: Dialect) -> bool {
    let upper = sql.trim().to_ascii_uppercase();

    // SET statements that have no SQLite equivalent.
    if upper.starts_with("SET ") {
        let rest = upper["SET ".len()..].trim_start();
        // SET NAMES, SET CHARACTER SET, SET SESSION, SET GLOBAL, SET time_zone, SET sql_mode
        if rest.starts_with("NAMES")
            || rest.starts_with("CHARACTER SET")
            || rest.starts_with("SESSION")
            || rest.starts_with("GLOBAL")
            || rest.starts_with("TIME_ZONE")
            || rest.starts_with("SQL_MODE")
            || rest.starts_with("NOCOUNT")
            || rest.starts_with("ANSI_NULLS")
            || rest.starts_with("QUOTED_IDENTIFIER")
            || rest.starts_with("XACT_ABORT")
        {
            return true;
        }
    }

    false
}

/// Rewrite a parsed statement from the source dialect to SQLite-compatible form.
fn rewrite_statement(
    mut stmt: Statement,
    dialect: Dialect,
) -> Result<Statement, TranslateError> {
    // Apply common rewrites (expressions, types).
    common::rewrite_statement(&mut stmt)?;

    // Apply dialect-specific rewrites.
    match dialect {
        Dialect::MySQL => mysql::rewrite_statement(&mut stmt)?,
        Dialect::PostgreSQL => postgres::rewrite_statement(&mut stmt)?,
        Dialect::TDS => tds::rewrite_statement(&mut stmt)?,
    }

    Ok(stmt)
}

/// Classify a SQL statement for routing decisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatementKind {
    Query,
    Mutation,
    Ddl,
    Transaction,
    Other,
}

/// Quickly classify a SQL string without full parsing.
#[must_use]
pub fn classify(sql: &str) -> StatementKind {
    let upper = sql.trim().to_ascii_uppercase();
    let first_word = upper.split_ascii_whitespace().next().unwrap_or("");

    match first_word {
        "SELECT" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" | "PRAGMA" => StatementKind::Query,
        "INSERT" | "UPDATE" | "DELETE" | "REPLACE" => StatementKind::Mutation,
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" => StatementKind::Ddl,
        "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" => {
            StatementKind::Transaction
        }
        _ => StatementKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── classify ────────────────────────────────────────────────────────────

    #[test]
    fn classify_select() {
        assert_eq!(classify("SELECT * FROM users"), StatementKind::Query);
    }

    #[test]
    fn classify_show() {
        assert_eq!(classify("SHOW TABLES"), StatementKind::Query);
    }

    #[test]
    fn classify_describe() {
        assert_eq!(classify("DESCRIBE users"), StatementKind::Query);
    }

    #[test]
    fn classify_explain() {
        assert_eq!(classify("EXPLAIN SELECT 1"), StatementKind::Query);
    }

    #[test]
    fn classify_pragma() {
        assert_eq!(classify("PRAGMA table_info('users')"), StatementKind::Query);
    }

    #[test]
    fn classify_insert() {
        assert_eq!(
            classify("INSERT INTO users VALUES (1)"),
            StatementKind::Mutation
        );
    }

    #[test]
    fn classify_update() {
        assert_eq!(
            classify("UPDATE users SET name = 'x'"),
            StatementKind::Mutation
        );
    }

    #[test]
    fn classify_delete() {
        assert_eq!(
            classify("DELETE FROM users WHERE id = 1"),
            StatementKind::Mutation
        );
    }

    #[test]
    fn classify_replace() {
        assert_eq!(
            classify("REPLACE INTO users VALUES (1, 'x')"),
            StatementKind::Mutation
        );
    }

    #[test]
    fn classify_create() {
        assert_eq!(
            classify("CREATE TABLE users (id INT)"),
            StatementKind::Ddl
        );
    }

    #[test]
    fn classify_drop() {
        assert_eq!(classify("DROP TABLE users"), StatementKind::Ddl);
    }

    #[test]
    fn classify_alter() {
        assert_eq!(
            classify("ALTER TABLE users ADD col TEXT"),
            StatementKind::Ddl
        );
    }

    #[test]
    fn classify_begin() {
        assert_eq!(classify("BEGIN"), StatementKind::Transaction);
    }

    #[test]
    fn classify_commit() {
        assert_eq!(classify("COMMIT"), StatementKind::Transaction);
    }

    #[test]
    fn classify_rollback() {
        assert_eq!(classify("ROLLBACK"), StatementKind::Transaction);
    }

    #[test]
    fn classify_savepoint() {
        assert_eq!(classify("SAVEPOINT sp1"), StatementKind::Transaction);
    }

    #[test]
    fn classify_unknown() {
        assert_eq!(classify("VACUUM"), StatementKind::Other);
    }

    #[test]
    fn classify_case_insensitive() {
        assert_eq!(classify("select * from t"), StatementKind::Query);
    }

    #[test]
    fn classify_leading_whitespace() {
        assert_eq!(classify("  SELECT 1"), StatementKind::Query);
    }

    // ── translate integration ───────────────────────────────────────────────

    #[test]
    fn translate_empty_returns_empty() {
        // An empty input should still parse (to zero statements).
        let results = translate("", Dialect::MySQL);
        // sqlparser may return an error or empty vec — either is fine.
        match results {
            Ok(v) => assert!(v.is_empty()),
            Err(_) => {} // parse error on empty string is acceptable
        }
    }

    #[test]
    fn translate_error_on_garbage() {
        let result = translate("NOT VALID SQL !!! @@@ {{{}}", Dialect::MySQL);
        assert!(result.is_err());
    }
}
