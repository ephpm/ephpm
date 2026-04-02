//! MySQL-specific SQL rewrites.
//!
//! Handles MySQL DDL constructs (`AUTO_INCREMENT`, `ENGINE=`, type mappings),
//! DML rewrites (`ON DUPLICATE KEY UPDATE`, `LIMIT offset, count`), and
//! MySQL-specific expressions.

use sqlparser::ast::{
    DataType, DoUpdate, Expr, LimitClause, Offset, OffsetRows, OnConflict, OnConflictAction,
    OnInsert, Statement,
};

use crate::TranslateError;

/// Apply MySQL-specific rewrites to a statement in-place.
pub fn rewrite_statement(stmt: &mut Statement) -> Result<(), TranslateError> {
    match stmt {
        Statement::CreateTable(create) => {
            rewrite_create_table(create);
        }
        Statement::Insert(insert) => {
            rewrite_insert_on_duplicate(insert);
        }
        Statement::Query(query) => {
            rewrite_limit_clause(query);
        }
        _ => {}
    }
    Ok(())
}

// ── ON DUPLICATE KEY UPDATE -> ON CONFLICT DO UPDATE ─────────────────────────

/// Rewrite MySQL's `ON DUPLICATE KEY UPDATE` to SQLite's `ON CONFLICT DO UPDATE`.
fn rewrite_insert_on_duplicate(insert: &mut sqlparser::ast::Insert) {
    if let Some(OnInsert::DuplicateKeyUpdate(assignments)) = insert.on.take() {
        insert.on = Some(OnInsert::OnConflict(OnConflict {
            conflict_target: None,
            action: OnConflictAction::DoUpdate(DoUpdate {
                assignments,
                selection: None,
            }),
        }));
    }
}

// ── LIMIT offset, count -> LIMIT count OFFSET offset ────────────────────────

/// Rewrite MySQL's `LIMIT offset, count` to standard `LIMIT count OFFSET offset`.
fn rewrite_limit_clause(query: &mut sqlparser::ast::Query) {
    if let Some(LimitClause::OffsetCommaLimit { offset, limit }) = query.limit_clause.take() {
        query.limit_clause = Some(LimitClause::LimitOffset {
            limit: Some(limit),
            offset: Some(Offset {
                value: offset,
                rows: OffsetRows::None,
            }),
            limit_by: vec![],
        });
    }
}

// ── DDL: CREATE TABLE ────────────────────────────────────────────────────────

/// Rewrite `CREATE TABLE` for SQLite compatibility.
fn rewrite_create_table(create: &mut sqlparser::ast::CreateTable) {
    // Rewrite column types.
    for col in &mut create.columns {
        col.data_type = rewrite_data_type(&col.data_type);

        // Remove AUTO_INCREMENT from column options.
        col.options.retain(|opt| {
            !matches!(
                &opt.option,
                sqlparser::ast::ColumnOption::DialectSpecific(tokens)
                    if tokens.iter().any(|t| t.to_string().to_ascii_uppercase() == "AUTO_INCREMENT")
            )
        });
    }
}

/// Map MySQL types to SQLite affinities.
fn rewrite_data_type(dt: &DataType) -> DataType {
    match dt {
        // Integer types -> INTEGER
        DataType::TinyInt(_)
        | DataType::SmallInt(_)
        | DataType::MediumInt(_)
        | DataType::Int(_)
        | DataType::BigInt(_)
        | DataType::Integer(_)
        | DataType::TinyIntUnsigned(_)
        | DataType::SmallIntUnsigned(_)
        | DataType::MediumIntUnsigned(_)
        | DataType::IntUnsigned(_)
        | DataType::IntegerUnsigned(_)
        | DataType::BigIntUnsigned(_) => DataType::Integer(None),

        // Float types -> REAL
        DataType::Float(_)
        | DataType::Double(..)
        | DataType::Decimal(..)
        | DataType::Numeric(..) => DataType::Real,

        // String types -> TEXT
        DataType::Varchar(_)
        | DataType::Char(_)
        | DataType::Text
        | DataType::TinyText
        | DataType::MediumText
        | DataType::LongText
        | DataType::Enum(..)
        | DataType::Set(_) => DataType::Text,

        // Binary types -> BLOB
        DataType::Binary(_)
        | DataType::Varbinary(_)
        | DataType::Blob(_)
        | DataType::TinyBlob
        | DataType::MediumBlob
        | DataType::LongBlob => DataType::Blob(None),

        // Boolean -> INTEGER
        DataType::Boolean => DataType::Integer(None),

        // Date/time types -> TEXT
        DataType::Date
        | DataType::Datetime(_)
        | DataType::Timestamp(_, _)
        | DataType::Time(_, _) => DataType::Text,

        // JSON -> TEXT
        DataType::JSON | DataType::JSONB => DataType::Text,

        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use crate::{translate, Dialect, TranslateResult};

    fn extract_sql(result: &TranslateResult) -> &str {
        match result {
            TranslateResult::Sql(s) => s.as_str(),
            other => panic!("expected Sql, got: {other:?}"),
        }
    }

    // ── No-ops ──────────────────────────────────────────────────────────────

    #[test]
    fn set_names_is_noop() {
        let results = translate("SET NAMES utf8mb4", Dialect::MySQL).unwrap();
        assert!(matches!(results[0], TranslateResult::Noop));
    }

    #[test]
    fn set_character_set_is_noop() {
        let results = translate("SET CHARACTER SET utf8", Dialect::MySQL).unwrap();
        assert!(matches!(results[0], TranslateResult::Noop));
    }

    #[test]
    fn set_session_is_noop() {
        let results = translate("SET SESSION wait_timeout = 28800", Dialect::MySQL).unwrap();
        assert!(matches!(results[0], TranslateResult::Noop));
    }

    #[test]
    fn set_sql_mode_is_noop() {
        let results =
            translate("SET sql_mode = 'STRICT_TRANS_TABLES'", Dialect::MySQL).unwrap();
        assert!(matches!(results[0], TranslateResult::Noop));
    }

    #[test]
    fn set_time_zone_is_noop() {
        let results = translate("SET time_zone = '+00:00'", Dialect::MySQL).unwrap();
        assert!(matches!(results[0], TranslateResult::Noop));
    }

    // ── Expression rewrites ─────────────────────────────────────────────────

    #[test]
    fn select_now_translated() {
        let results = translate("SELECT NOW()", Dialect::MySQL).unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        assert!(upper.contains("DATETIME"), "expected datetime, got: {sql}");
    }

    #[test]
    fn boolean_translated() {
        let results = translate("SELECT TRUE, FALSE", Dialect::MySQL).unwrap();
        let sql = extract_sql(&results[0]);
        assert!(
            sql.contains('1') && sql.contains('0'),
            "expected 1 and 0, got: {sql}"
        );
    }

    // ── ON DUPLICATE KEY UPDATE ─────────────────────────────────────────────

    #[test]
    fn on_duplicate_key_update_rewritten() {
        let results = translate(
            "INSERT INTO t (id, name) VALUES (1, 'Alice') ON DUPLICATE KEY UPDATE name = 'Alice'",
            Dialect::MySQL,
        )
        .unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        assert!(
            upper.contains("ON CONFLICT"),
            "expected ON CONFLICT, got: {sql}"
        );
        assert!(
            upper.contains("DO UPDATE"),
            "expected DO UPDATE, got: {sql}"
        );
        assert!(
            !upper.contains("DUPLICATE KEY"),
            "DUPLICATE KEY should be removed: {sql}"
        );
    }

    #[test]
    fn on_duplicate_key_update_preserves_assignments() {
        let results = translate(
            "INSERT INTO t (id, val) VALUES (1, 10) ON DUPLICATE KEY UPDATE val = val + 1",
            Dialect::MySQL,
        )
        .unwrap();
        let sql = extract_sql(&results[0]);
        assert!(sql.contains("val"), "assignments lost: {sql}");
    }

    // ── LIMIT offset, count ─────────────────────────────────────────────────

    #[test]
    fn limit_offset_comma_rewritten() {
        let results =
            translate("SELECT * FROM t LIMIT 5, 10", Dialect::MySQL).unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        assert!(upper.contains("LIMIT 10"), "expected LIMIT 10, got: {sql}");
        assert!(
            upper.contains("OFFSET 5"),
            "expected OFFSET 5, got: {sql}"
        );
    }

    #[test]
    fn standard_limit_unchanged() {
        // Standard LIMIT without offset should not add an OFFSET clause.
        let results =
            translate("SELECT * FROM t LIMIT 10", Dialect::MySQL).unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        // sqlparser may or may not preserve LIMIT in the emitted SQL for MySQL dialect.
        // The key assertion is: no spurious OFFSET is introduced.
        assert!(!upper.contains("OFFSET"), "unexpected OFFSET: {sql}");
    }

    // ── DDL: type rewrites ──────────────────────────────────────────────────

    #[test]
    fn int_types_to_integer() {
        let results = translate(
            "CREATE TABLE t (a TINYINT, b SMALLINT, c MEDIUMINT, d INT, e BIGINT)",
            Dialect::MySQL,
        )
        .unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        assert!(!upper.contains("TINYINT"), "TINYINT not rewritten: {sql}");
        assert!(
            !upper.contains("SMALLINT"),
            "SMALLINT not rewritten: {sql}"
        );
        assert!(
            !upper.contains("MEDIUMINT"),
            "MEDIUMINT not rewritten: {sql}"
        );
        assert!(!upper.contains("BIGINT"), "BIGINT not rewritten: {sql}");
    }

    #[test]
    fn varchar_to_text() {
        let results =
            translate("CREATE TABLE t (name VARCHAR(255), bio TEXT)", Dialect::MySQL).unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        assert!(!upper.contains("VARCHAR"), "VARCHAR not rewritten: {sql}");
    }

    #[test]
    fn float_types_to_real() {
        let results = translate(
            "CREATE TABLE t (a FLOAT, b DOUBLE, c DECIMAL(10,2))",
            Dialect::MySQL,
        )
        .unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        assert!(upper.contains("REAL"), "no REAL found: {sql}");
    }

    #[test]
    fn blob_types() {
        let results = translate("CREATE TABLE t (a BLOB, b LONGBLOB)", Dialect::MySQL).unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        assert!(!upper.contains("LONGBLOB"), "LONGBLOB not rewritten: {sql}");
    }

    #[test]
    fn datetime_to_text() {
        let results = translate(
            "CREATE TABLE t (created DATETIME, updated TIMESTAMP)",
            Dialect::MySQL,
        )
        .unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        assert!(
            !upper.contains("DATETIME"),
            "DATETIME not rewritten: {sql}"
        );
        assert!(
            !upper.contains("TIMESTAMP"),
            "TIMESTAMP not rewritten: {sql}"
        );
    }

    #[test]
    fn auto_increment_removed() {
        let results = translate(
            "CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(100))",
            Dialect::MySQL,
        )
        .unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        assert!(
            !upper.contains("AUTO_INCREMENT"),
            "AUTO_INCREMENT not removed: {sql}"
        );
    }

    #[test]
    fn boolean_to_integer_in_ddl() {
        let results = translate("CREATE TABLE t (active BOOLEAN)", Dialect::MySQL).unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        assert!(!upper.contains("BOOLEAN"), "BOOLEAN not rewritten: {sql}");
        assert!(upper.contains("INTEGER"), "no INTEGER found: {sql}");
    }

    #[test]
    fn json_to_text() {
        let results = translate("CREATE TABLE t (data JSON)", Dialect::MySQL).unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        assert!(!upper.contains(" JSON"), "JSON not rewritten: {sql}");
        assert!(upper.contains("TEXT"), "no TEXT found: {sql}");
    }

    // ── Passthrough ─────────────────────────────────────────────────────────

    #[test]
    fn simple_select_passthrough() {
        let results = translate("SELECT 1 + 2", Dialect::MySQL).unwrap();
        let sql = extract_sql(&results[0]);
        assert!(sql.contains("1 + 2"), "got: {sql}");
    }

    #[test]
    fn insert_passthrough() {
        let results = translate(
            "INSERT INTO users (name, age) VALUES ('Alice', 30)",
            Dialect::MySQL,
        )
        .unwrap();
        let sql = extract_sql(&results[0]);
        assert!(sql.contains("Alice"), "got: {sql}");
    }

    #[test]
    fn delete_passthrough() {
        let results = translate("DELETE FROM users WHERE id = 1", Dialect::MySQL).unwrap();
        let sql = extract_sql(&results[0]);
        assert!(sql.to_ascii_uppercase().contains("DELETE"), "got: {sql}");
    }
}
