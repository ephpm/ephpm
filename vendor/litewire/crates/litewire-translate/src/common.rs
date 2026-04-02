//! Common SQL rewrites shared across all dialects.
//!
//! Handles expression-level transformations: function calls, boolean literals,
//! type casts, and parameter placeholders.

use sqlparser::ast::{
    Expr, Function, FunctionArg, FunctionArgExpr, FunctionArgumentList,
    FunctionArguments, Ident, ObjectName, Statement, Value, ValueWithSpan,
};

use crate::TranslateError;

/// Apply common rewrites to a statement in-place.
pub fn rewrite_statement(stmt: &mut Statement) -> Result<(), TranslateError> {
    rewrite_statement_exprs(stmt);
    Ok(())
}

/// Walk a statement and rewrite expressions.
fn rewrite_statement_exprs(stmt: &mut Statement) {
    match stmt {
        Statement::Query(query) => rewrite_query_exprs(query),
        Statement::Insert(insert) => {
            if let Some(source) = &mut insert.source {
                rewrite_query_exprs(source);
            }
        }
        Statement::Update {
            assignments,
            selection,
            ..
        } => {
            for assign in assignments {
                rewrite_expr(&mut assign.value);
            }
            if let Some(sel) = selection {
                rewrite_expr(sel);
            }
        }
        Statement::Delete(delete) => {
            if let Some(sel) = &mut delete.selection {
                rewrite_expr(sel);
            }
        }
        _ => {}
    }
}

/// Walk a query and rewrite expressions.
fn rewrite_query_exprs(query: &mut sqlparser::ast::Query) {
    // Handle VALUES clauses (e.g., INSERT ... VALUES (NOW())).
    if let sqlparser::ast::SetExpr::Values(values) = query.body.as_mut() {
        for row in &mut values.rows {
            for expr in row {
                rewrite_expr(expr);
            }
        }
        return;
    }

    if let sqlparser::ast::SetExpr::Select(select) = query.body.as_mut() {
        for item in &mut select.projection {
            if let sqlparser::ast::SelectItem::UnnamedExpr(expr)
            | sqlparser::ast::SelectItem::ExprWithAlias { expr, .. } = item
            {
                rewrite_expr(expr);
            }
        }
        if let Some(sel) = &mut select.selection {
            rewrite_expr(sel);
        }
        if let Some(having) = &mut select.having {
            rewrite_expr(having);
        }
    }
}

/// Rewrite a single expression to SQLite-compatible form.
fn rewrite_expr(expr: &mut Expr) {
    match expr {
        Expr::Function(func) => {
            rewrite_function(func);
            if let FunctionArguments::List(FunctionArgumentList { args, .. }) = &mut func.args {
                for arg in args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                        rewrite_expr(e);
                    }
                }
            }
        }
        Expr::Value(val_with_span) => {
            rewrite_value(&mut val_with_span.value);
        }
        Expr::BinaryOp { left, right, .. } => {
            rewrite_expr(left);
            rewrite_expr(right);
        }
        Expr::UnaryOp { expr: inner, .. } => {
            rewrite_expr(inner);
        }
        Expr::Nested(inner) => {
            rewrite_expr(inner);
        }
        Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            rewrite_expr(inner);
        }
        Expr::InList {
            expr: inner, list, ..
        } => {
            rewrite_expr(inner);
            for e in list {
                rewrite_expr(e);
            }
        }
        Expr::Between {
            expr: inner,
            low,
            high,
            ..
        } => {
            rewrite_expr(inner);
            rewrite_expr(low);
            rewrite_expr(high);
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(op) = operand {
                rewrite_expr(op);
            }
            for cw in conditions {
                rewrite_expr(&mut cw.condition);
                rewrite_expr(&mut cw.result);
            }
            if let Some(el) = else_result {
                rewrite_expr(el);
            }
        }
        Expr::Subquery(q) => {
            rewrite_query_exprs(q);
        }
        Expr::CompoundIdentifier(parts) => {
            let joined = parts
                .iter()
                .map(|p| p.value.to_ascii_uppercase())
                .collect::<Vec<_>>()
                .join(".");
            match joined.as_str() {
                "@@IDENTITY" => {
                    *expr = Expr::Function(Function {
                        name: func_name("last_insert_rowid"),
                        args: func_args(vec![]),
                        filter: None,
                        over: None,
                        within_group: vec![],
                        parameters: FunctionArguments::None,
                        null_treatment: None,
                        uses_odbc_syntax: false,
                    });
                }
                "@@ROWCOUNT" => {
                    *expr = Expr::Function(Function {
                        name: func_name("changes"),
                        args: func_args(vec![]),
                        filter: None,
                        over: None,
                        within_group: vec![],
                        parameters: FunctionArguments::None,
                        null_treatment: None,
                        uses_odbc_syntax: false,
                    });
                }
                _ => {}
            }
        }
        _ => {}
    }
}

/// Helper to create a `ValueWithSpan` from a `Value`.
fn value_expr(val: Value) -> Expr {
    Expr::Value(ValueWithSpan {
        value: val,
        span: sqlparser::tokenizer::Span::empty(),
    })
}

/// Helper to build a function name `ObjectName`.
fn func_name(name: &str) -> ObjectName {
    ObjectName(vec![sqlparser::ast::ObjectNamePart::Identifier(
        Ident::new(name),
    )])
}

/// Helper to build function args list.
fn func_args(args: Vec<Expr>) -> FunctionArguments {
    FunctionArguments::List(FunctionArgumentList {
        args: args
            .into_iter()
            .map(|e| FunctionArg::Unnamed(FunctionArgExpr::Expr(e)))
            .collect(),
        duplicate_treatment: None,
        clauses: vec![],
    })
}

/// Rewrite function calls to SQLite equivalents.
fn rewrite_function(func: &mut Function) {
    let name_upper = func.name.to_string().to_ascii_uppercase();

    match name_upper.as_str() {
        "NOW" | "CURRENT_TIMESTAMP" | "GETDATE" | "GETUTCDATE" => {
            func.name = func_name("datetime");
            func.args = func_args(vec![value_expr(Value::SingleQuotedString("now".into()))]);
        }
        "CURDATE" | "CURRENT_DATE" => {
            func.name = func_name("date");
            func.args = func_args(vec![value_expr(Value::SingleQuotedString("now".into()))]);
        }
        "UNIX_TIMESTAMP" => {
            func.name = func_name("strftime");
            func.args = func_args(vec![
                value_expr(Value::SingleQuotedString("%s".into())),
                value_expr(Value::SingleQuotedString("now".into())),
            ]);
        }
        "ISNULL" => {
            func.name = func_name("IFNULL");
        }
        "NEWID" => {
            // NEWID() -> lower(hex(randomblob(16)))
            func.name = func_name("lower");
            func.args = func_args(vec![Expr::Function(Function {
                name: func_name("hex"),
                args: func_args(vec![Expr::Function(Function {
                    name: func_name("randomblob"),
                    args: func_args(vec![value_expr(Value::Number("16".into(), false))]),
                    filter: None,
                    over: None,
                    within_group: vec![],
                    parameters: FunctionArguments::None,
                    null_treatment: None,
                    uses_odbc_syntax: false,
                })]),
                filter: None,
                over: None,
                within_group: vec![],
                parameters: FunctionArguments::None,
                null_treatment: None,
                uses_odbc_syntax: false,
            })]);
        }
        _ => {}
    }
}

/// Rewrite literal values.
fn rewrite_value(val: &mut Value) {
    match val {
        Value::Boolean(b) => {
            *val = Value::Number(if *b { "1".into() } else { "0".into() }, false);
        }
        Value::Placeholder(p) => {
            if let Some(rest) = p.strip_prefix('$') {
                if rest.chars().all(|c| c.is_ascii_digit()) {
                    *p = format!("?{rest}");
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{translate, Dialect, TranslateResult};

    #[test]
    fn boolean_rewrite() {
        let mut v = Value::Boolean(true);
        rewrite_value(&mut v);
        assert_eq!(v, Value::Number("1".into(), false));

        let mut v = Value::Boolean(false);
        rewrite_value(&mut v);
        assert_eq!(v, Value::Number("0".into(), false));
    }

    // ── Function call rewrites ──────────────────────────────────────────────

    #[test]
    fn now_rewrite() {
        let results = translate("SELECT NOW()", Dialect::MySQL).unwrap();
        let sql = extract_sql(&results[0]);
        assert!(sql.to_ascii_uppercase().contains("DATETIME"), "got: {sql}");
        assert!(sql.contains("'now'"), "got: {sql}");
    }

    #[test]
    fn curdate_rewrite() {
        let results = translate("SELECT CURDATE()", Dialect::MySQL).unwrap();
        let sql = extract_sql(&results[0]);
        assert!(sql.to_ascii_lowercase().contains("date"), "got: {sql}");
        assert!(sql.contains("'now'"), "got: {sql}");
    }

    #[test]
    fn unix_timestamp_rewrite() {
        let results = translate("SELECT UNIX_TIMESTAMP()", Dialect::MySQL).unwrap();
        let sql = extract_sql(&results[0]);
        assert!(sql.to_ascii_lowercase().contains("strftime"), "got: {sql}");
        assert!(sql.contains("'%s'"), "got: {sql}");
    }

    #[test]
    fn isnull_rewrite() {
        let results = translate("SELECT ISNULL(a, b) FROM t", Dialect::MySQL).unwrap();
        let sql = extract_sql(&results[0]);
        assert!(sql.to_ascii_uppercase().contains("IFNULL"), "got: {sql}");
    }

    #[test]
    fn getdate_rewrite() {
        let results = translate("SELECT GETDATE()", Dialect::TDS).unwrap();
        let sql = extract_sql(&results[0]);
        assert!(sql.to_ascii_uppercase().contains("DATETIME"), "got: {sql}");
    }

    // ── Expression tree walking ─────────────────────────────────────────────

    #[test]
    fn nested_function_in_binary_op() {
        let results = translate("SELECT NOW() > '2024-01-01'", Dialect::MySQL).unwrap();
        let sql = extract_sql(&results[0]);
        assert!(sql.to_ascii_lowercase().contains("datetime"), "got: {sql}");
    }

    #[test]
    fn boolean_in_where_clause() {
        let results =
            translate("SELECT * FROM t WHERE active = TRUE", Dialect::MySQL).unwrap();
        let sql = extract_sql(&results[0]);
        assert!(sql.contains('1'), "got: {sql}");
    }

    #[test]
    fn function_in_insert_values() {
        let results = translate(
            "INSERT INTO t (created) VALUES (NOW())",
            Dialect::MySQL,
        )
        .unwrap();
        let sql = extract_sql(&results[0]);
        assert!(sql.to_ascii_lowercase().contains("datetime"), "got: {sql}");
    }

    #[test]
    fn function_in_update_set() {
        let results = translate(
            "UPDATE t SET updated = NOW() WHERE id = 1",
            Dialect::MySQL,
        )
        .unwrap();
        let sql = extract_sql(&results[0]);
        assert!(sql.to_ascii_lowercase().contains("datetime"), "got: {sql}");
    }

    #[test]
    fn boolean_in_case_expression() {
        let results = translate(
            "SELECT CASE WHEN x = TRUE THEN 'yes' ELSE 'no' END FROM t",
            Dialect::MySQL,
        )
        .unwrap();
        let sql = extract_sql(&results[0]);
        assert!(sql.contains('1'), "got: {sql}");
    }

    // ── NEWID rewrite ────────────────────────────────────────────────────

    #[test]
    fn newid_rewrite() {
        let results = translate("SELECT NEWID()", Dialect::TDS).unwrap();
        let sql = extract_sql(&results[0]);
        let lower = sql.to_ascii_lowercase();
        assert!(lower.contains("lower"), "got: {sql}");
        assert!(lower.contains("hex"), "got: {sql}");
        assert!(lower.contains("randomblob"), "got: {sql}");
    }

    // ── Placeholder rewrite ────────────────────────────────────────────────

    #[test]
    fn dollar_placeholder_to_question_mark() {
        let results = translate("SELECT * FROM t WHERE id = $1", Dialect::PostgreSQL).unwrap();
        let sql = extract_sql(&results[0]);
        assert!(sql.contains("?1"), "got: {sql}");
        assert!(!sql.contains("$1"), "got: {sql}");
    }

    #[test]
    fn multiple_dollar_placeholders() {
        let results = translate(
            "SELECT * FROM t WHERE a = $1 AND b = $2",
            Dialect::PostgreSQL,
        )
        .unwrap();
        let sql = extract_sql(&results[0]);
        assert!(sql.contains("?1"), "got: {sql}");
        assert!(sql.contains("?2"), "got: {sql}");
    }

    // ── Helper ──────────────────────────────────────────────────────────────

    fn extract_sql(result: &TranslateResult) -> &str {
        match result {
            TranslateResult::Sql(s) => s.as_str(),
            other => panic!("expected Sql, got: {other:?}"),
        }
    }
}
