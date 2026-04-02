//! Metadata query detection and translation.
//!
//! Handles `SHOW TABLES`, `SHOW COLUMNS`, `DESCRIBE`, `INFORMATION_SCHEMA`
//! queries, and their T-SQL equivalents (`sys.tables`, `sp_tables`).

use crate::Dialect;

/// A detected metadata query that requires special handling.
#[derive(Debug, Clone)]
pub enum MetadataQuery {
    /// `SHOW TABLES` or equivalent.
    ShowTables,
    /// `SHOW DATABASES` or equivalent.
    ShowDatabases,
    /// `SHOW COLUMNS FROM <table>` or `DESCRIBE <table>`.
    ShowColumns { table: String },
    /// `SHOW CREATE TABLE <table>`.
    ShowCreateTable { table: String },
    /// `SHOW INDEX FROM <table>`.
    ShowIndex { table: String },
    /// `SELECT @@variable` queries — MySQL system variables.
    SystemVariables { variables: Vec<String> },
    /// `SELECT ... FROM information_schema.tables` — table listing.
    InformationSchemaTables {
        schema_filter: Option<String>,
    },
    /// `SELECT ... FROM information_schema.columns` — column listing.
    InformationSchemaColumns {
        table_filter: Option<String>,
    },
    /// `SELECT ... FROM information_schema.schemata` — schema listing.
    InformationSchemata,
    /// `SELECT ... FROM pg_catalog.pg_tables` or similar.
    PgCatalogTables,
    /// `SELECT ... FROM pg_catalog.pg_columns` or `pg_attribute` for a table.
    PgCatalogColumns { table: String },
    /// `SELECT ... FROM sys.tables` (T-SQL).
    SysTables,
    /// `SELECT ... FROM sys.columns` (T-SQL) for a table.
    SysColumns { table: String },
}

impl MetadataQuery {
    /// Convert this metadata query to the SQLite SQL that produces equivalent results.
    #[must_use]
    pub fn to_sqlite_sql(&self) -> String {
        match self {
            Self::ShowTables => {
                "SELECT name AS Tables_in_database FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name".into()
            }
            Self::ShowDatabases => {
                "SELECT 'main' AS Database".into()
            }
            Self::ShowColumns { table } => {
                format!("PRAGMA table_info('{table}')")
            }
            Self::ShowCreateTable { table } => {
                format!(
                    "SELECT sql AS 'Create Table' FROM sqlite_master WHERE type='table' AND name='{table}'"
                )
            }
            Self::ShowIndex { table } => {
                format!("PRAGMA index_list('{table}')")
            }
            Self::InformationSchemaTables { schema_filter } => {
                // Map INFORMATION_SCHEMA.TABLES to sqlite_master.
                let base = "SELECT name AS TABLE_NAME, 'BASE TABLE' AS TABLE_TYPE, 'main' AS TABLE_SCHEMA FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'";
                if let Some(schema) = schema_filter {
                    // TABLE_SCHEMA filter -- in SQLite there's only 'main'.
                    if schema.eq_ignore_ascii_case("main") || schema.eq_ignore_ascii_case("def") {
                        format!("{base} ORDER BY name")
                    } else {
                        // Schema doesn't match -- return empty.
                        format!("{base} AND 0 ORDER BY name")
                    }
                } else {
                    format!("{base} ORDER BY name")
                }
            }
            Self::InformationSchemaColumns { table_filter } => {
                // Map INFORMATION_SCHEMA.COLUMNS to PRAGMA table_info.
                // This is tricky because we need per-table info. If a table
                // filter is given, we can use PRAGMA directly.
                if let Some(table) = table_filter {
                    format!(
                        "SELECT '{table}' AS TABLE_NAME, name AS COLUMN_NAME, \
                         CASE WHEN \"notnull\" = 1 THEN 'NO' ELSE 'YES' END AS IS_NULLABLE, \
                         type AS DATA_TYPE, dflt_value AS COLUMN_DEFAULT, \
                         cid AS ORDINAL_POSITION \
                         FROM pragma_table_info('{table}') ORDER BY cid"
                    )
                } else {
                    // Without a table filter, list columns for all tables.
                    // This requires a query per table; return tables list as fallback.
                    "SELECT name AS TABLE_NAME FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name".into()
                }
            }
            Self::InformationSchemata => {
                "SELECT 'main' AS SCHEMA_NAME, 'main' AS CATALOG_NAME".into()
            }
            Self::SystemVariables { variables } => {
                // Build a SELECT with synthetic values for each variable.
                let cols: Vec<String> = variables
                    .iter()
                    .map(|v| {
                        let val = system_variable_value(v);
                        format!("{val} AS `@@{v}`")
                    })
                    .collect();
                format!("SELECT {}", cols.join(", "))
            }
            Self::PgCatalogTables => {
                "SELECT name AS tablename, 'main' AS schemaname FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name".into()
            }
            Self::PgCatalogColumns { table } => {
                format!(
                    "SELECT '{table}' AS table_name, name AS column_name, \
                     type AS data_type, \
                     CASE WHEN \"notnull\" = 1 THEN 'NO' ELSE 'YES' END AS is_nullable, \
                     dflt_value AS column_default \
                     FROM pragma_table_info('{table}') ORDER BY cid"
                )
            }
            Self::SysTables => {
                "SELECT name, 'U' AS type FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name".into()
            }
            Self::SysColumns { table } => {
                format!(
                    "SELECT name AS COLUMN_NAME, type AS DATA_TYPE, \
                     CASE WHEN \"notnull\" = 1 THEN 0 ELSE 1 END AS is_nullable \
                     FROM pragma_table_info('{table}') ORDER BY cid"
                )
            }
        }
    }
}

/// Return a synthetic value for a MySQL system variable.
fn system_variable_value(name: &str) -> &'static str {
    match name.to_ascii_lowercase().as_str() {
        "max_allowed_packet" => "67108864",  // 64 MiB
        "wait_timeout" => "28800",           // 8 hours
        "interactive_timeout" => "28800",
        "net_write_timeout" => "60",
        "net_read_timeout" => "30",
        "socket" => "'/tmp/litewire.sock'",
        "character_set_client" => "'utf8mb4'",
        "character_set_connection" => "'utf8mb4'",
        "character_set_results" => "'utf8mb4'",
        "character_set_server" => "'utf8mb4'",
        "collation_connection" => "'utf8mb4_general_ci'",
        "collation_server" => "'utf8mb4_general_ci'",
        "version" => "'8.0.0-litewire'",
        "version_comment" => "'litewire'",
        "sql_mode" => "''",
        "lower_case_table_names" => "0",
        "autocommit" => "1",
        "transaction_isolation" | "tx_isolation" => "'SERIALIZABLE'",
        "identity" => "last_insert_rowid()",
        "rowcount" => "changes()",
        _ => "NULL",
    }
}

/// Detect if a SQL string is a metadata query that needs special handling.
///
/// Returns `None` if it's a regular query that should go through the parser.
pub fn detect_metadata_query(sql: &str, _dialect: Dialect) -> Option<MetadataQuery> {
    let trimmed = sql.trim();
    let upper = trimmed.to_ascii_uppercase();

    // SHOW TABLES
    if upper == "SHOW TABLES" || upper.starts_with("SHOW TABLES ") {
        return Some(MetadataQuery::ShowTables);
    }

    // SHOW DATABASES
    if upper == "SHOW DATABASES" || upper.starts_with("SHOW DATABASES ") {
        return Some(MetadataQuery::ShowDatabases);
    }

    // SHOW COLUMNS FROM <table> / SHOW FIELDS FROM <table>
    if let Some(rest) = upper
        .strip_prefix("SHOW COLUMNS FROM ")
        .or_else(|| upper.strip_prefix("SHOW FIELDS FROM "))
    {
        let table = extract_table_name(rest, trimmed);
        return Some(MetadataQuery::ShowColumns { table });
    }

    // DESCRIBE <table> / DESC <table>
    if let Some(rest) = upper
        .strip_prefix("DESCRIBE ")
        .or_else(|| upper.strip_prefix("DESC "))
    {
        let table = extract_table_name(rest, trimmed);
        return Some(MetadataQuery::ShowColumns { table });
    }

    // SHOW CREATE TABLE <table>
    if let Some(rest) = upper.strip_prefix("SHOW CREATE TABLE ") {
        let table = extract_table_name(rest, trimmed);
        return Some(MetadataQuery::ShowCreateTable { table });
    }

    // SHOW INDEX FROM <table> / SHOW INDEXES FROM <table> / SHOW KEYS FROM <table>
    if let Some(rest) = upper
        .strip_prefix("SHOW INDEX FROM ")
        .or_else(|| upper.strip_prefix("SHOW INDEXES FROM "))
        .or_else(|| upper.strip_prefix("SHOW KEYS FROM "))
    {
        let table = extract_table_name(rest, trimmed);
        return Some(MetadataQuery::ShowIndex { table });
    }

    // INFORMATION_SCHEMA queries.
    if upper.contains("INFORMATION_SCHEMA") {
        if upper.contains("INFORMATION_SCHEMA.TABLES")
            || upper.contains("INFORMATION_SCHEMA.`TABLES`")
        {
            let schema_filter = extract_where_value_original(trimmed, "TABLE_SCHEMA");
            return Some(MetadataQuery::InformationSchemaTables { schema_filter });
        }
        if upper.contains("INFORMATION_SCHEMA.COLUMNS")
            || upper.contains("INFORMATION_SCHEMA.`COLUMNS`")
        {
            let table_filter = extract_where_value_original(trimmed, "TABLE_NAME");
            return Some(MetadataQuery::InformationSchemaColumns { table_filter });
        }
        if upper.contains("INFORMATION_SCHEMA.SCHEMATA")
            || upper.contains("INFORMATION_SCHEMA.`SCHEMATA`")
        {
            return Some(MetadataQuery::InformationSchemata);
        }
    }

    // SELECT @@variable queries (MySQL system variables).
    if upper.starts_with("SELECT ") && trimmed.contains("@@") {
        // Check that all projected columns are @@variables (no FROM clause).
        let after_select = &trimmed["SELECT ".len()..];
        if !upper.contains(" FROM ") {
            let variables: Vec<String> = after_select
                .split(',')
                .filter_map(|part| {
                    let part = part.trim();
                    // Handle @@global.var, @@session.var, @@var
                    if let Some(pos) = part.find("@@") {
                        let var_part = &part[pos + 2..];
                        let var_name = var_part
                            .trim_start_matches("global.")
                            .trim_start_matches("session.")
                            .split(|c: char| !c.is_alphanumeric() && c != '_')
                            .next()
                            .unwrap_or("");
                        if !var_name.is_empty() {
                            return Some(var_name.to_string());
                        }
                    }
                    None
                })
                .collect();

            if !variables.is_empty() {
                return Some(MetadataQuery::SystemVariables { variables });
            }
        }
    }

    // pg_catalog queries.
    if upper.contains("PG_CATALOG.PG_TABLES") || upper.contains("PG_CATALOG.PG_CLASS") {
        return Some(MetadataQuery::PgCatalogTables);
    }
    if upper.contains("PG_CATALOG.PG_ATTRIBUTE") {
        let table_filter = extract_where_value_original(trimmed, "TABLE_NAME")
            .or_else(|| extract_where_value_original(trimmed, "ATTRELID"));
        return Some(MetadataQuery::PgCatalogColumns {
            table: table_filter.unwrap_or_default(),
        });
    }

    // T-SQL sys.tables / sys.columns.
    if upper.contains("SYS.TABLES") || upper.contains("SYSOBJECTS") {
        return Some(MetadataQuery::SysTables);
    }
    if upper.contains("SYS.COLUMNS") || upper.contains("SYSCOLUMNS") {
        let table_filter = extract_where_value_original(trimmed, "TABLE_NAME");
        return Some(MetadataQuery::SysColumns {
            table: table_filter.unwrap_or_default(),
        });
    }

    // T-SQL stored procedure metadata: sp_tables, sp_columns.
    if upper.starts_with("EXEC SP_TABLES") || upper.starts_with("SP_TABLES") {
        return Some(MetadataQuery::SysTables);
    }
    if upper.starts_with("EXEC SP_COLUMNS") || upper.starts_with("SP_COLUMNS") {
        // Try to extract the table name argument.
        let table = trimmed
            .split_ascii_whitespace()
            .nth(1)
            .map(|s| {
                s.trim_matches('\'')
                    .trim_matches('"')
                    .trim_end_matches(';')
                    .to_string()
            })
            .unwrap_or_default();
        return Some(MetadataQuery::SysColumns { table });
    }

    None
}

/// Extract a simple `column = 'value'` filter from a WHERE clause.
/// Searches on the uppercased SQL for the column name, but extracts the
/// value from the original SQL to preserve case.
fn extract_where_value(upper_sql: &str, column: &str) -> Option<String> {
    let pattern = format!("{column} = ");
    if let Some(pos) = upper_sql.find(&pattern) {
        let after = &upper_sql[pos + pattern.len()..];
        let after = after.trim_start();
        if after.starts_with('\'') || after.starts_with('"') {
            let quote = after.as_bytes()[0] as char;
            let rest = &after[1..];
            if let Some(end) = rest.find(quote) {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

/// Like [`extract_where_value`] but preserves original case by indexing into
/// the original (non-uppercased) SQL.
fn extract_where_value_original(original_sql: &str, column: &str) -> Option<String> {
    let upper = original_sql.to_ascii_uppercase();
    let pattern = format!("{column} = ");
    if let Some(pos) = upper.find(&pattern) {
        let after = &original_sql[pos + pattern.len()..];
        let after_trimmed = after.trim_start();
        let trim_offset = after.len() - after_trimmed.len();
        let after = after_trimmed;
        if after.starts_with('\'') || after.starts_with('"') {
            let quote = after.as_bytes()[0] as char;
            let rest = &after[1..];
            if let Some(end) = rest.find(quote) {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

/// Extract a table name from the remaining SQL after the keyword.
/// Uses the original (non-uppercased) string for proper casing.
fn extract_table_name(upper_rest: &str, original: &str) -> String {
    let word = upper_rest.split_ascii_whitespace().next().unwrap_or("");
    // Find the same position in the original string.
    let offset = original.len() - (upper_rest.len().min(original.len()));
    let orig_rest = &original[offset..];
    let orig_word = orig_rest.split_ascii_whitespace().next().unwrap_or(word);

    // Strip backticks, double quotes, brackets.
    orig_word
        .trim_matches('`')
        .trim_matches('"')
        .trim_matches('[')
        .trim_end_matches(']')
        .trim_end_matches(';')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Dialect;

    // ── Detection ───────────────────────────────────────────────────────────

    #[test]
    fn detect_show_tables() {
        let q = detect_metadata_query("SHOW TABLES", Dialect::MySQL);
        assert!(matches!(q, Some(MetadataQuery::ShowTables)));
    }

    #[test]
    fn detect_show_tables_trailing_space() {
        let q = detect_metadata_query("SHOW TABLES ", Dialect::MySQL);
        assert!(matches!(q, Some(MetadataQuery::ShowTables)));
    }

    #[test]
    fn detect_show_databases() {
        let q = detect_metadata_query("SHOW DATABASES", Dialect::MySQL);
        assert!(matches!(q, Some(MetadataQuery::ShowDatabases)));
    }

    #[test]
    fn detect_describe() {
        let q = detect_metadata_query("DESCRIBE users", Dialect::MySQL);
        assert!(matches!(q, Some(MetadataQuery::ShowColumns { table }) if table == "users"));
    }

    #[test]
    fn detect_desc_shorthand() {
        let q = detect_metadata_query("DESC users", Dialect::MySQL);
        assert!(matches!(q, Some(MetadataQuery::ShowColumns { table }) if table == "users"));
    }

    #[test]
    fn detect_show_columns_from() {
        let q = detect_metadata_query("SHOW COLUMNS FROM users", Dialect::MySQL);
        assert!(matches!(q, Some(MetadataQuery::ShowColumns { table }) if table == "users"));
    }

    #[test]
    fn detect_show_fields_from() {
        let q = detect_metadata_query("SHOW FIELDS FROM users", Dialect::MySQL);
        assert!(matches!(q, Some(MetadataQuery::ShowColumns { table }) if table == "users"));
    }

    #[test]
    fn detect_show_create_table() {
        let q = detect_metadata_query("SHOW CREATE TABLE `posts`", Dialect::MySQL);
        assert!(matches!(q, Some(MetadataQuery::ShowCreateTable { table }) if table == "posts"));
    }

    #[test]
    fn detect_show_index_from() {
        let q = detect_metadata_query("SHOW INDEX FROM users", Dialect::MySQL);
        assert!(matches!(q, Some(MetadataQuery::ShowIndex { table }) if table == "users"));
    }

    #[test]
    fn detect_show_indexes_from() {
        let q = detect_metadata_query("SHOW INDEXES FROM users", Dialect::MySQL);
        assert!(matches!(q, Some(MetadataQuery::ShowIndex { table }) if table == "users"));
    }

    #[test]
    fn detect_show_keys_from() {
        let q = detect_metadata_query("SHOW KEYS FROM users", Dialect::MySQL);
        assert!(matches!(q, Some(MetadataQuery::ShowIndex { table }) if table == "users"));
    }

    #[test]
    fn regular_select_not_metadata() {
        let q = detect_metadata_query("SELECT * FROM users", Dialect::MySQL);
        assert!(q.is_none());
    }

    #[test]
    fn insert_not_metadata() {
        let q = detect_metadata_query("INSERT INTO users VALUES (1)", Dialect::MySQL);
        assert!(q.is_none());
    }

    // ── Table name extraction ───────────────────────────────────────────────

    #[test]
    fn backtick_stripped() {
        let q = detect_metadata_query("DESCRIBE `my_table`", Dialect::MySQL);
        assert!(matches!(q, Some(MetadataQuery::ShowColumns { table }) if table == "my_table"));
    }

    #[test]
    fn double_quote_stripped() {
        let q = detect_metadata_query("DESCRIBE \"my_table\"", Dialect::MySQL);
        assert!(matches!(q, Some(MetadataQuery::ShowColumns { table }) if table == "my_table"));
    }

    #[test]
    fn semicolon_stripped() {
        let q = detect_metadata_query("DESCRIBE users;", Dialect::MySQL);
        assert!(matches!(q, Some(MetadataQuery::ShowColumns { table }) if table == "users"));
    }

    // ── SQL generation ──────────────────────────────────────────────────────

    #[test]
    fn show_tables_sql() {
        let sql = MetadataQuery::ShowTables.to_sqlite_sql();
        assert!(sql.contains("sqlite_master"));
        assert!(sql.contains("type='table'"));
        assert!(sql.contains("NOT LIKE 'sqlite_%'"));
    }

    #[test]
    fn show_databases_sql() {
        let sql = MetadataQuery::ShowDatabases.to_sqlite_sql();
        assert!(sql.contains("'main'"));
    }

    #[test]
    fn show_columns_sql() {
        let sql = MetadataQuery::ShowColumns {
            table: "users".into(),
        }
        .to_sqlite_sql();
        assert!(sql.contains("PRAGMA table_info"));
        assert!(sql.contains("users"));
    }

    #[test]
    fn show_create_table_sql() {
        let sql = MetadataQuery::ShowCreateTable {
            table: "posts".into(),
        }
        .to_sqlite_sql();
        assert!(sql.contains("sqlite_master"));
        assert!(sql.contains("posts"));
    }

    #[test]
    fn show_index_sql() {
        let sql = MetadataQuery::ShowIndex {
            table: "users".into(),
        }
        .to_sqlite_sql();
        assert!(sql.contains("PRAGMA index_list"));
        assert!(sql.contains("users"));
    }

    #[test]
    fn system_variables_sql() {
        let sql = MetadataQuery::SystemVariables {
            variables: vec!["version".into(), "max_allowed_packet".into()],
        }
        .to_sqlite_sql();
        assert!(sql.contains("@@version"), "got: {sql}");
        assert!(sql.contains("@@max_allowed_packet"), "got: {sql}");
        assert!(sql.contains("8.0.0-litewire"), "got: {sql}");
        assert!(sql.contains("67108864"), "got: {sql}");
    }

    #[test]
    fn unknown_system_variable_returns_null() {
        let sql = MetadataQuery::SystemVariables {
            variables: vec!["nonexistent_var".into()],
        }
        .to_sqlite_sql();
        assert!(sql.contains("NULL"), "got: {sql}");
    }

    // ── Case insensitivity ──────────────────────────────────────────────────

    #[test]
    fn case_insensitive_show_tables() {
        let q = detect_metadata_query("show tables", Dialect::MySQL);
        assert!(matches!(q, Some(MetadataQuery::ShowTables)));
    }

    #[test]
    fn case_insensitive_describe() {
        let q = detect_metadata_query("describe Users", Dialect::MySQL);
        assert!(matches!(q, Some(MetadataQuery::ShowColumns { .. })));
    }

    // ── System variable detection (SELECT @@...) ────────────────────────────

    #[test]
    fn detect_single_system_variable() {
        let q = detect_metadata_query("SELECT @@version", Dialect::MySQL);
        match q {
            Some(MetadataQuery::SystemVariables { variables }) => {
                assert_eq!(variables, vec!["version"]);
            }
            other => panic!("expected SystemVariables, got: {other:?}"),
        }
    }

    #[test]
    fn detect_multiple_system_variables() {
        let q = detect_metadata_query(
            "SELECT @@version, @@max_allowed_packet, @@wait_timeout",
            Dialect::MySQL,
        );
        match q {
            Some(MetadataQuery::SystemVariables { variables }) => {
                assert_eq!(variables.len(), 3);
                assert!(variables.contains(&"version".to_string()));
                assert!(variables.contains(&"max_allowed_packet".to_string()));
                assert!(variables.contains(&"wait_timeout".to_string()));
            }
            other => panic!("expected SystemVariables, got: {other:?}"),
        }
    }

    #[test]
    fn detect_global_scoped_variable() {
        let q = detect_metadata_query("SELECT @@global.max_connections", Dialect::MySQL);
        match q {
            Some(MetadataQuery::SystemVariables { variables }) => {
                assert_eq!(variables, vec!["max_connections"]);
            }
            other => panic!("expected SystemVariables, got: {other:?}"),
        }
    }

    #[test]
    fn detect_session_scoped_variable() {
        let q = detect_metadata_query("SELECT @@session.wait_timeout", Dialect::MySQL);
        match q {
            Some(MetadataQuery::SystemVariables { variables }) => {
                assert_eq!(variables, vec!["wait_timeout"]);
            }
            other => panic!("expected SystemVariables, got: {other:?}"),
        }
    }

    #[test]
    fn select_with_from_not_system_variable() {
        // SELECT @@var FROM table should NOT match -- it has a FROM clause.
        let q = detect_metadata_query("SELECT @@version FROM dual", Dialect::MySQL);
        assert!(q.is_none());
    }

    #[test]
    fn regular_select_with_at_sign_not_system_variable() {
        // A normal select without @@ should not match.
        let q = detect_metadata_query("SELECT email FROM users", Dialect::MySQL);
        assert!(!matches!(q, Some(MetadataQuery::SystemVariables { .. })));
    }

    // ── system_variable_value coverage ──────────────────────────────────────

    #[test]
    fn known_system_variables_produce_expected_values() {
        let cases = vec![
            ("version", "8.0.0-litewire"),
            ("max_allowed_packet", "67108864"),
            ("autocommit", "1"),
            ("character_set_client", "utf8mb4"),
            ("transaction_isolation", "SERIALIZABLE"),
            ("tx_isolation", "SERIALIZABLE"),
        ];
        for (var, expected_fragment) in cases {
            let sql = MetadataQuery::SystemVariables {
                variables: vec![var.into()],
            }
            .to_sqlite_sql();
            assert!(
                sql.contains(expected_fragment),
                "@@{var}: expected {expected_fragment} in: {sql}"
            );
        }
    }

    // ── INFORMATION_SCHEMA detection ────────────────────────────────────────

    #[test]
    fn detect_information_schema_tables() {
        let q = detect_metadata_query(
            "SELECT * FROM information_schema.tables",
            Dialect::MySQL,
        );
        assert!(matches!(
            q,
            Some(MetadataQuery::InformationSchemaTables { schema_filter: None })
        ));
    }

    #[test]
    fn detect_information_schema_tables_with_backticks() {
        let q = detect_metadata_query(
            "SELECT * FROM INFORMATION_SCHEMA.`TABLES`",
            Dialect::MySQL,
        );
        assert!(matches!(
            q,
            Some(MetadataQuery::InformationSchemaTables { .. })
        ));
    }

    #[test]
    fn detect_information_schema_tables_with_schema_filter() {
        let q = detect_metadata_query(
            "SELECT TABLE_NAME FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_SCHEMA = 'mydb'",
            Dialect::MySQL,
        );
        match q {
            Some(MetadataQuery::InformationSchemaTables {
                schema_filter: Some(schema),
            }) => assert_eq!(schema, "mydb"),
            other => panic!("expected InformationSchemaTables with filter, got: {other:?}"),
        }
    }

    #[test]
    fn detect_information_schema_columns() {
        let q = detect_metadata_query(
            "SELECT * FROM information_schema.columns WHERE TABLE_NAME = 'users'",
            Dialect::MySQL,
        );
        match q {
            Some(MetadataQuery::InformationSchemaColumns {
                table_filter: Some(table),
            }) => assert_eq!(table, "users"),
            other => panic!("expected InformationSchemaColumns with filter, got: {other:?}"),
        }
    }

    #[test]
    fn detect_information_schema_columns_no_filter() {
        let q = detect_metadata_query(
            "SELECT * FROM INFORMATION_SCHEMA.COLUMNS",
            Dialect::MySQL,
        );
        assert!(matches!(
            q,
            Some(MetadataQuery::InformationSchemaColumns {
                table_filter: None
            })
        ));
    }

    #[test]
    fn detect_information_schema_schemata() {
        let q = detect_metadata_query(
            "SELECT * FROM INFORMATION_SCHEMA.SCHEMATA",
            Dialect::MySQL,
        );
        assert!(matches!(q, Some(MetadataQuery::InformationSchemata)));
    }

    // ── INFORMATION_SCHEMA SQL generation ───────────────────────────────────

    #[test]
    fn information_schema_tables_sql() {
        let sql = MetadataQuery::InformationSchemaTables {
            schema_filter: None,
        }
        .to_sqlite_sql();
        assert!(sql.contains("sqlite_master"), "got: {sql}");
        assert!(sql.contains("TABLE_NAME"), "got: {sql}");
        assert!(sql.contains("TABLE_TYPE"), "got: {sql}");
    }

    #[test]
    fn information_schema_tables_with_main_filter() {
        let sql = MetadataQuery::InformationSchemaTables {
            schema_filter: Some("main".into()),
        }
        .to_sqlite_sql();
        assert!(sql.contains("sqlite_master"), "got: {sql}");
        // Should NOT contain "AND 0" (main is a valid schema).
        assert!(!sql.contains("AND 0"), "got: {sql}");
    }

    #[test]
    fn information_schema_tables_with_unknown_schema() {
        let sql = MetadataQuery::InformationSchemaTables {
            schema_filter: Some("nonexistent".into()),
        }
        .to_sqlite_sql();
        // Should return empty (AND 0).
        assert!(sql.contains("AND 0"), "got: {sql}");
    }

    #[test]
    fn information_schema_columns_with_table_sql() {
        let sql = MetadataQuery::InformationSchemaColumns {
            table_filter: Some("users".into()),
        }
        .to_sqlite_sql();
        assert!(sql.contains("pragma_table_info"), "got: {sql}");
        assert!(sql.contains("users"), "got: {sql}");
        assert!(sql.contains("COLUMN_NAME"), "got: {sql}");
    }

    #[test]
    fn information_schema_columns_no_table_fallback() {
        let sql = MetadataQuery::InformationSchemaColumns {
            table_filter: None,
        }
        .to_sqlite_sql();
        // Falls back to listing tables.
        assert!(sql.contains("sqlite_master"), "got: {sql}");
    }

    #[test]
    fn information_schema_schemata_sql() {
        let sql = MetadataQuery::InformationSchemata.to_sqlite_sql();
        assert!(sql.contains("SCHEMA_NAME"), "got: {sql}");
        assert!(sql.contains("'main'"), "got: {sql}");
    }

    // ── @@IDENTITY / @@ROWCOUNT ────────────────────────────────────────────

    #[test]
    fn identity_system_variable() {
        let sql = MetadataQuery::SystemVariables {
            variables: vec!["identity".into()],
        }
        .to_sqlite_sql();
        assert!(sql.contains("last_insert_rowid()"), "got: {sql}");
    }

    #[test]
    fn rowcount_system_variable() {
        let sql = MetadataQuery::SystemVariables {
            variables: vec!["rowcount".into()],
        }
        .to_sqlite_sql();
        assert!(sql.contains("changes()"), "got: {sql}");
    }

    #[test]
    fn detect_select_at_identity() {
        let q = detect_metadata_query("SELECT @@IDENTITY", Dialect::TDS);
        assert!(
            matches!(q, Some(MetadataQuery::SystemVariables { .. })),
            "got: {q:?}"
        );
    }

    #[test]
    fn detect_select_at_rowcount() {
        let q = detect_metadata_query("SELECT @@ROWCOUNT", Dialect::TDS);
        assert!(
            matches!(q, Some(MetadataQuery::SystemVariables { .. })),
            "got: {q:?}"
        );
    }

    // ── pg_catalog detection ───────────────────────────────────────────────

    #[test]
    fn detect_pg_catalog_tables() {
        let q = detect_metadata_query(
            "SELECT * FROM pg_catalog.pg_tables",
            Dialect::PostgreSQL,
        );
        assert!(matches!(q, Some(MetadataQuery::PgCatalogTables)), "got: {q:?}");
    }

    #[test]
    fn detect_pg_catalog_pg_class() {
        let q = detect_metadata_query(
            "SELECT * FROM pg_catalog.pg_class WHERE relkind = 'r'",
            Dialect::PostgreSQL,
        );
        assert!(matches!(q, Some(MetadataQuery::PgCatalogTables)), "got: {q:?}");
    }

    #[test]
    fn pg_catalog_tables_sql() {
        let sql = MetadataQuery::PgCatalogTables.to_sqlite_sql();
        assert!(sql.contains("sqlite_master"), "got: {sql}");
        assert!(sql.contains("tablename"), "got: {sql}");
    }

    #[test]
    fn pg_catalog_columns_sql() {
        let sql = MetadataQuery::PgCatalogColumns {
            table: "users".into(),
        }
        .to_sqlite_sql();
        assert!(sql.contains("pragma_table_info"), "got: {sql}");
        assert!(sql.contains("users"), "got: {sql}");
    }

    // ── sys.tables / sys.columns detection ─────────────────────────────────

    #[test]
    fn detect_sys_tables() {
        let q = detect_metadata_query("SELECT * FROM sys.tables", Dialect::TDS);
        assert!(matches!(q, Some(MetadataQuery::SysTables)), "got: {q:?}");
    }

    #[test]
    fn detect_sys_columns() {
        let q = detect_metadata_query(
            "SELECT * FROM sys.columns WHERE TABLE_NAME = 'users'",
            Dialect::TDS,
        );
        assert!(
            matches!(q, Some(MetadataQuery::SysColumns { ref table }) if table == "users"),
            "got: {q:?}"
        );
    }

    #[test]
    fn sys_tables_sql() {
        let sql = MetadataQuery::SysTables.to_sqlite_sql();
        assert!(sql.contains("sqlite_master"), "got: {sql}");
    }

    #[test]
    fn sys_columns_sql() {
        let sql = MetadataQuery::SysColumns {
            table: "orders".into(),
        }
        .to_sqlite_sql();
        assert!(sql.contains("pragma_table_info"), "got: {sql}");
        assert!(sql.contains("orders"), "got: {sql}");
    }

    // ── sp_tables / sp_columns detection ───────────────────────────────────

    #[test]
    fn detect_sp_tables() {
        let q = detect_metadata_query("EXEC sp_tables", Dialect::TDS);
        assert!(matches!(q, Some(MetadataQuery::SysTables)), "got: {q:?}");
    }

    #[test]
    fn detect_sp_columns() {
        let q = detect_metadata_query("EXEC sp_columns 'users'", Dialect::TDS);
        assert!(
            matches!(q, Some(MetadataQuery::SysColumns { .. })),
            "got: {q:?}"
        );
    }
}
