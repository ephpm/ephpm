//! Emit SQLite-compatible SQL from a rewritten AST.
//!
//! Uses sqlparser's `Display` implementation which produces standard SQL.
//! SQLite is largely standard-compliant, so this works for most cases.

use sqlparser::ast::Statement;

/// Emit a statement as a SQL string.
#[must_use]
pub fn emit_statement(stmt: &Statement) -> String {
    stmt.to_string()
}

#[cfg(test)]
mod tests {
    use sqlparser::dialect::{MySqlDialect, PostgreSqlDialect};
    use sqlparser::parser::Parser;

    use super::*;

    #[test]
    fn roundtrip_simple_select() {
        let sql = "SELECT 1 + 2";
        let stmts = Parser::parse_sql(&MySqlDialect {}, sql).unwrap();
        let emitted = emit_statement(&stmts[0]);
        assert!(emitted.contains("1 + 2"));
    }

    #[test]
    fn roundtrip_select_with_where() {
        let sql = "SELECT id, name FROM users WHERE id = 1";
        let stmts = Parser::parse_sql(&MySqlDialect {}, sql).unwrap();
        let emitted = emit_statement(&stmts[0]);
        assert!(emitted.contains("users"));
        assert!(emitted.contains("id = 1"));
    }

    #[test]
    fn roundtrip_insert() {
        let sql = "INSERT INTO users (name) VALUES ('Alice')";
        let stmts = Parser::parse_sql(&MySqlDialect {}, sql).unwrap();
        let emitted = emit_statement(&stmts[0]);
        assert!(emitted.contains("Alice"));
        assert!(emitted.to_ascii_uppercase().contains("INSERT"));
    }

    #[test]
    fn roundtrip_create_table() {
        let sql = "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)";
        let stmts = Parser::parse_sql(&MySqlDialect {}, sql).unwrap();
        let emitted = emit_statement(&stmts[0]);
        assert!(emitted.to_ascii_uppercase().contains("CREATE TABLE"));
        assert!(emitted.contains("id"));
        assert!(emitted.contains("name"));
    }

    #[test]
    fn roundtrip_update() {
        let sql = "UPDATE users SET name = 'Bob' WHERE id = 1";
        let stmts = Parser::parse_sql(&MySqlDialect {}, sql).unwrap();
        let emitted = emit_statement(&stmts[0]);
        assert!(emitted.contains("Bob"));
        assert!(emitted.contains("id = 1"));
    }

    #[test]
    fn roundtrip_delete() {
        let sql = "DELETE FROM users WHERE id = 1";
        let stmts = Parser::parse_sql(&MySqlDialect {}, sql).unwrap();
        let emitted = emit_statement(&stmts[0]);
        assert!(emitted.to_ascii_uppercase().contains("DELETE"));
    }

    #[test]
    fn roundtrip_select_with_join() {
        let sql = "SELECT u.name, o.total FROM users u JOIN orders o ON u.id = o.user_id";
        let stmts = Parser::parse_sql(&MySqlDialect {}, sql).unwrap();
        let emitted = emit_statement(&stmts[0]);
        assert!(emitted.contains("JOIN"));
        assert!(emitted.contains("u.id = o.user_id"));
    }

    #[test]
    fn roundtrip_pg_select() {
        let sql = "SELECT * FROM users WHERE active = true ORDER BY name LIMIT 10";
        let stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql).unwrap();
        let emitted = emit_statement(&stmts[0]);
        assert!(emitted.contains("LIMIT 10"));
    }

    #[test]
    fn roundtrip_subquery() {
        let sql = "SELECT * FROM users WHERE id IN (SELECT user_id FROM orders)";
        let stmts = Parser::parse_sql(&MySqlDialect {}, sql).unwrap();
        let emitted = emit_statement(&stmts[0]);
        assert!(emitted.contains("SELECT user_id FROM orders"));
    }
}
