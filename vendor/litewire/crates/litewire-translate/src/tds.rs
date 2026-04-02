//! T-SQL (SQL Server) specific SQL rewrites.

use sqlparser::ast::{DataType, Expr, Statement};

use crate::TranslateError;

/// Apply T-SQL-specific rewrites to a statement in-place.
pub fn rewrite_statement(stmt: &mut Statement) -> Result<(), TranslateError> {
    match stmt {
        Statement::CreateTable(create) => {
            for col in &mut create.columns {
                col.data_type = rewrite_data_type(&col.data_type);
                // Remove IDENTITY column options (T-SQL auto-increment).
                col.options.retain(|opt| {
                    !matches!(&opt.option, sqlparser::ast::ColumnOption::Identity(..))
                });
            }
        }
        Statement::Query(query) => {
            rewrite_top_to_limit(query);
        }
        _ => {}
    }
    Ok(())
}

/// Rewrite `SELECT TOP n ...` to `SELECT ... LIMIT n`.
fn rewrite_top_to_limit(query: &mut sqlparser::ast::Query) {
    if let sqlparser::ast::SetExpr::Select(select) = query.body.as_mut() {
        if let Some(top) = select.top.take() {
            if let Some(quantity) = top.quantity {
                use sqlparser::ast::{LimitClause, TopQuantity};
                let limit_expr = match quantity {
                    TopQuantity::Expr(e) => e,
                    TopQuantity::Constant(n) => Expr::Value(sqlparser::ast::ValueWithSpan {
                        value: sqlparser::ast::Value::Number(n.to_string().into(), false),
                        span: sqlparser::tokenizer::Span::empty(),
                    }),
                };
                if query.limit_clause.is_none() {
                    query.limit_clause = Some(LimitClause::LimitOffset {
                        limit: Some(limit_expr),
                        offset: None,
                        limit_by: vec![],
                    });
                }
            }
        }
    }
}

/// Map T-SQL types to SQLite affinities.
fn rewrite_data_type(dt: &DataType) -> DataType {
    match dt {
        DataType::TinyInt(_)
        | DataType::SmallInt(_)
        | DataType::Int(_)
        | DataType::BigInt(_)
        | DataType::Integer(_) => DataType::Integer(None),

        DataType::Float(_)
        | DataType::Double(..)
        | DataType::Decimal(..)
        | DataType::Numeric(..) => DataType::Real,

        DataType::Varchar(_)
        | DataType::Char(_)
        | DataType::Nvarchar(_)
        | DataType::Text => DataType::Text,

        DataType::Binary(_) | DataType::Varbinary(_) => DataType::Blob(None),

        DataType::Boolean | DataType::Bit(_) => DataType::Integer(None),

        DataType::Date | DataType::Datetime(_) | DataType::Timestamp(_, _) | DataType::Time(_, _) => {
            DataType::Text
        }

        DataType::Custom(name, _) => {
            let upper = name.to_string().to_ascii_uppercase();
            match upper.as_str() {
                "MONEY" | "SMALLMONEY" => DataType::Real,
                "IMAGE" => DataType::Blob(None),
                "UNIQUEIDENTIFIER" => DataType::Text,
                _ => dt.clone(),
            }
        }

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

    #[test]
    fn int_types_to_integer() {
        let results = translate(
            "CREATE TABLE t (a TINYINT, b SMALLINT, c INT, d BIGINT)",
            Dialect::TDS,
        )
        .unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        assert!(!upper.contains("TINYINT"), "TINYINT not rewritten: {sql}");
        assert!(!upper.contains("SMALLINT"), "SMALLINT not rewritten: {sql}");
        assert!(!upper.contains("BIGINT"), "BIGINT not rewritten: {sql}");
    }

    #[test]
    fn nvarchar_to_text() {
        let results = translate(
            "CREATE TABLE t (name NVARCHAR(255))",
            Dialect::TDS,
        )
        .unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        assert!(!upper.contains("NVARCHAR"), "NVARCHAR not rewritten: {sql}");
        assert!(upper.contains("TEXT"), "no TEXT found: {sql}");
    }

    #[test]
    fn varbinary_to_blob() {
        let results = translate("CREATE TABLE t (data VARBINARY(MAX))", Dialect::TDS).unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        assert!(!upper.contains("VARBINARY"), "VARBINARY not rewritten: {sql}");
        assert!(upper.contains("BLOB"), "no BLOB found: {sql}");
    }

    #[test]
    fn datetime_to_text() {
        let results =
            translate("CREATE TABLE t (created DATETIME)", Dialect::TDS).unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        assert!(!upper.contains("DATETIME"), "DATETIME not rewritten: {sql}");
        assert!(upper.contains("TEXT"), "no TEXT found: {sql}");
    }

    #[test]
    fn float_types_to_real() {
        let results = translate(
            "CREATE TABLE t (a FLOAT, b DECIMAL(10,2))",
            Dialect::TDS,
        )
        .unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        assert!(upper.contains("REAL"), "no REAL found: {sql}");
    }

    #[test]
    fn set_nocount_is_noop() {
        let results = translate("SET NOCOUNT ON", Dialect::TDS).unwrap();
        assert!(matches!(results[0], TranslateResult::Noop));
    }

    #[test]
    fn simple_select_passthrough() {
        let results = translate("SELECT 1 + 2", Dialect::TDS).unwrap();
        let sql = extract_sql(&results[0]);
        assert!(sql.contains("1 + 2"), "got: {sql}");
    }

    #[test]
    fn top_to_limit() {
        let results = translate("SELECT TOP 10 * FROM t", Dialect::TDS).unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        assert!(!upper.contains("TOP"), "TOP not removed: {sql}");
        assert!(upper.contains("LIMIT"), "no LIMIT found: {sql}");
        assert!(upper.contains("10"), "no 10 found: {sql}");
    }

    #[test]
    fn top_with_parentheses() {
        let results = translate("SELECT TOP(5) name FROM t", Dialect::TDS).unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        assert!(upper.contains("LIMIT"), "no LIMIT found: {sql}");
    }

    #[test]
    fn bit_to_integer() {
        let results = translate("CREATE TABLE t (active BIT)", Dialect::TDS).unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        assert!(!upper.contains("BIT"), "BIT not rewritten: {sql}");
        assert!(upper.contains("INTEGER"), "no INTEGER found: {sql}");
    }

    #[test]
    fn money_to_real() {
        let results = translate("CREATE TABLE t (price MONEY)", Dialect::TDS).unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        assert!(!upper.contains("MONEY"), "MONEY not rewritten: {sql}");
        assert!(upper.contains("REAL"), "no REAL found: {sql}");
    }

    #[test]
    fn uniqueidentifier_to_text() {
        let results = translate(
            "CREATE TABLE t (id UNIQUEIDENTIFIER)",
            Dialect::TDS,
        )
        .unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        assert!(
            !upper.contains("UNIQUEIDENTIFIER"),
            "UNIQUEIDENTIFIER not rewritten: {sql}"
        );
        assert!(upper.contains("TEXT"), "no TEXT found: {sql}");
    }

    #[test]
    fn image_to_blob() {
        let results = translate("CREATE TABLE t (photo IMAGE)", Dialect::TDS).unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        assert!(!upper.contains("IMAGE"), "IMAGE not rewritten: {sql}");
        assert!(upper.contains("BLOB"), "no BLOB found: {sql}");
    }

    #[test]
    fn identity_column_option_removed() {
        let results = translate(
            "CREATE TABLE t (id INT IDENTITY(1,1) PRIMARY KEY)",
            Dialect::TDS,
        )
        .unwrap();
        let sql = extract_sql(&results[0]);
        let upper = sql.to_ascii_uppercase();
        assert!(!upper.contains("IDENTITY"), "IDENTITY not removed: {sql}");
        assert!(upper.contains("PRIMARY KEY"), "PRIMARY KEY missing: {sql}");
    }
}
