//! `opensrv-mysql` shim implementation.
//!
//! Implements `AsyncMysqlShim` to handle MySQL protocol commands including
//! prepared statement prepare/execute/close.

use std::collections::HashMap;

use litewire_backend::{SharedBackend, Value};
use litewire_translate::{self, Dialect, StatementKind, TranslateResult, classify};
use opensrv_mysql::*;
use tokio::io::AsyncWrite;
use tracing::{debug, warn};

/// Build an `OkResponse` with the correct transaction status flag.
fn ok_response(affected_rows: u64, last_insert_id: u64, in_transaction: bool) -> OkResponse {
    let status_flags = if in_transaction {
        StatusFlags::SERVER_STATUS_IN_TRANS
    } else {
        StatusFlags::empty()
    };
    OkResponse {
        affected_rows,
        last_insert_id,
        status_flags,
        ..OkResponse::default()
    }
}

use crate::types::sqlite_to_mysql_column_type;

/// A cached prepared statement.
struct PreparedStmt {
    /// The translated SQLite SQL (with `?` placeholders).
    sqlite_sql: String,
    /// Whether this is a query (SELECT) or mutation (INSERT/UPDATE/DELETE).
    kind: StatementKind,
    /// Number of `?` parameters.
    param_count: usize,
}

/// Handler for a single MySQL client connection.
pub struct LiteWireHandler {
    backend: SharedBackend,
    /// Prepared statements keyed by the statement ID assigned during `on_prepare`.
    stmts: HashMap<u32, PreparedStmt>,
    /// Next statement ID to assign.
    next_stmt_id: u32,
    /// Whether the connection is inside an explicit transaction.
    in_transaction: bool,
}

impl LiteWireHandler {
    pub fn new(backend: SharedBackend) -> Self {
        Self {
            backend,
            stmts: HashMap::new(),
            next_stmt_id: 1,
            in_transaction: false,
        }
    }

    /// Execute a query and write result set.
    async fn do_query<W: AsyncWrite + Send + Unpin>(
        &self,
        sql: &str,
        params: &[Value],
        results: QueryResultWriter<'_, W>,
    ) -> Result<(), std::io::Error> {
        match self.backend.query(sql, params).await {
            Ok(rs) => {
                let columns: Vec<Column> = rs
                    .columns
                    .iter()
                    .map(|c| Column {
                        table: String::new(),
                        column: c.name.clone(),
                        coltype: sqlite_to_mysql_column_type(c.decltype.as_deref()),
                        colflags: ColumnFlags::empty(),
                    })
                    .collect();

                let mut rw: RowWriter<'_, W> = results.start(&columns).await?;

                for row in &rs.rows {
                    for val in row {
                        match val {
                            Value::Null => rw.write_col(None::<&str>)?,
                            Value::Integer(i) => rw.write_col(i.to_string())?,
                            Value::Float(f) => rw.write_col(f.to_string())?,
                            Value::Text(s) => rw.write_col(s.as_str())?,
                            Value::Blob(b) => rw.write_col(b.as_slice())?,
                        }
                    }
                    rw.end_row().await?;
                }

                rw.finish().await
            }
            Err(e) => {
                results
                    .error(ErrorKind::ER_UNKNOWN_ERROR, e.to_string().as_bytes())
                    .await
            }
        }
    }

    /// Execute a mutation and write OK response.
    async fn do_execute<W: AsyncWrite + Send + Unpin>(
        &self,
        sql: &str,
        params: &[Value],
        results: QueryResultWriter<'_, W>,
    ) -> Result<(), std::io::Error> {
        match self.backend.execute(sql, params).await {
            Ok(r) => {
                let resp = ok_response(
                    r.affected_rows,
                    r.last_insert_rowid.unwrap_or(0) as u64,
                    self.in_transaction,
                );
                results.completed(resp).await
            }
            Err(e) => {
                results
                    .error(ErrorKind::ER_UNKNOWN_ERROR, e.to_string().as_bytes())
                    .await
            }
        }
    }

    /// Execute a transaction command (BEGIN/COMMIT/ROLLBACK) and update state.
    async fn do_transaction<W: AsyncWrite + Send + Unpin>(
        &mut self,
        sql: &str,
        results: QueryResultWriter<'_, W>,
    ) -> Result<(), std::io::Error> {
        match self.backend.execute(sql, &[]).await {
            Ok(_) => {
                let upper = sql.trim().to_ascii_uppercase();
                if upper.starts_with("BEGIN") || upper.starts_with("START") {
                    self.in_transaction = true;
                } else if upper.starts_with("COMMIT") || upper.starts_with("ROLLBACK") {
                    self.in_transaction = false;
                }
                let resp = ok_response(0, 0, self.in_transaction);
                results.completed(resp).await
            }
            Err(e) => {
                results
                    .error(ErrorKind::ER_UNKNOWN_ERROR, e.to_string().as_bytes())
                    .await
            }
        }
    }

    /// Translate SQL and return the first translated result, or an error string.
    fn translate_sql(&self, query: &str) -> Result<(String, StatementKind), String> {
        let translated = litewire_translate::translate(query, Dialect::MySQL)
            .map_err(|e| e.to_string())?;

        let Some(result) = translated.into_iter().next() else {
            return Ok((String::new(), StatementKind::Other));
        };

        match result {
            TranslateResult::Noop => Ok((String::new(), StatementKind::Other)),
            TranslateResult::Metadata(meta) => {
                let sql = meta.to_sqlite_sql();
                Ok((sql, StatementKind::Query))
            }
            TranslateResult::Sql(sql) => {
                let kind = classify(&sql);
                Ok((sql, kind))
            }
        }
    }
}

/// Convert an opensrv-mysql parameter value to our backend Value type.
fn param_to_value(param: ParamValue<'_>) -> Value {
    match param.value.into_inner() {
        ValueInner::NULL => Value::Null,
        ValueInner::Int(i) => Value::Integer(i),
        ValueInner::UInt(u) => Value::Integer(u as i64),
        ValueInner::Double(f) => Value::Float(f),
        ValueInner::Bytes(b) => match std::str::from_utf8(b) {
            Ok(s) => Value::Text(s.to_string()),
            Err(_) => Value::Blob(b.to_vec()),
        },
        ValueInner::Date(b) | ValueInner::Time(b) | ValueInner::Datetime(b) => {
            // Date/time binary encodings -- convert to text for SQLite.
            Value::Text(String::from_utf8_lossy(b).into_owned())
        }
    }
}

#[async_trait::async_trait]
impl<W: AsyncWrite + Send + Unpin> AsyncMysqlShim<W> for LiteWireHandler {
    type Error = std::io::Error;

    async fn on_prepare<'a>(
        &'a mut self,
        query: &'a str,
        info: StatementMetaWriter<'a, W>,
    ) -> Result<(), Self::Error> {
        debug!(sql = %query, "COM_STMT_PREPARE");

        let (sqlite_sql, kind) = match self.translate_sql(query) {
            Ok(r) => r,
            Err(e) => {
                return info
                    .error(ErrorKind::ER_PARSE_ERROR, e.as_bytes())
                    .await;
            }
        };

        // Count `?` placeholders in the translated SQL.
        let param_count = sqlite_sql.chars().filter(|&c| c == '?').count();

        let params: Vec<Column> = (0..param_count)
            .map(|_| Column {
                table: String::new(),
                column: "?".into(),
                coltype: ColumnType::MYSQL_TYPE_VAR_STRING,
                colflags: ColumnFlags::empty(),
            })
            .collect();

        // Try to determine output columns for queries.
        let columns = if kind == StatementKind::Query && !sqlite_sql.is_empty() {
            let probe_sql = format!("{sqlite_sql} LIMIT 0");
            match self.backend.query(&probe_sql, &[]).await {
                Ok(rs) => rs
                    .columns
                    .iter()
                    .map(|c| Column {
                        table: String::new(),
                        column: c.name.clone(),
                        coltype: sqlite_to_mysql_column_type(c.decltype.as_deref()),
                        colflags: ColumnFlags::empty(),
                    })
                    .collect(),
                Err(_) => vec![],
            }
        } else {
            vec![]
        };

        // Assign a statement ID and cache it.
        let stmt_id = self.next_stmt_id;
        self.next_stmt_id += 1;

        self.stmts.insert(
            stmt_id,
            PreparedStmt {
                sqlite_sql,
                kind,
                param_count,
            },
        );

        info.reply(stmt_id, &params, &columns).await
    }

    async fn on_execute<'a>(
        &'a mut self,
        id: u32,
        params: ParamParser<'a>,
        results: QueryResultWriter<'a, W>,
    ) -> Result<(), Self::Error> {
        debug!(stmt_id = id, "COM_STMT_EXECUTE");

        let Some(stmt) = self.stmts.get(&id) else {
            return results
                .error(
                    ErrorKind::ER_UNKNOWN_STMT_HANDLER,
                    format!("unknown statement id: {id}").as_bytes(),
                )
                .await;
        };

        let sql = stmt.sqlite_sql.clone();
        let kind = stmt.kind;

        // Extract parameter values.
        let values: Vec<Value> = params.into_iter().map(param_to_value).collect();

        // Noop statements (empty SQL from SET NAMES etc.)
        if sql.is_empty() {
            return results
                .completed(ok_response(0, 0, self.in_transaction))
                .await;
        }

        match kind {
            StatementKind::Query => self.do_query(&sql, &values, results).await,
            StatementKind::Transaction => self.do_transaction(&sql, results).await,
            _ => self.do_execute(&sql, &values, results).await,
        }
    }

    async fn on_close(&mut self, id: u32) {
        debug!(stmt_id = id, "COM_STMT_CLOSE");
        self.stmts.remove(&id);
    }

    async fn on_query<'a>(
        &'a mut self,
        query: &'a str,
        results: QueryResultWriter<'a, W>,
    ) -> Result<(), Self::Error> {
        debug!(sql = %query, "COM_QUERY");

        let translated = match litewire_translate::translate(query, Dialect::MySQL) {
            Ok(r) => r,
            Err(e) => {
                warn!("SQL translation error: {e}");
                return results
                    .error(ErrorKind::ER_PARSE_ERROR, e.to_string().as_bytes())
                    .await;
            }
        };

        let Some(result) = translated.into_iter().next() else {
            return results.completed(OkResponse::default()).await;
        };

        match result {
            TranslateResult::Noop => results.completed(OkResponse::default()).await,
            TranslateResult::Metadata(meta) => {
                let sqlite_sql = meta.to_sqlite_sql();
                self.do_query(&sqlite_sql, &[], results).await
            }
            TranslateResult::Sql(sqlite_sql) => {
                let kind = classify(&sqlite_sql);
                match kind {
                    StatementKind::Query => self.do_query(&sqlite_sql, &[], results).await,
                    StatementKind::Transaction => {
                        self.do_transaction(&sqlite_sql, results).await
                    }
                    _ => self.do_execute(&sqlite_sql, &[], results).await,
                }
            }
        }
    }

    async fn on_init<'a>(
        &'a mut self,
        schema: &'a str,
        writer: InitWriter<'a, W>,
    ) -> Result<(), Self::Error> {
        debug!(schema = %schema, "COM_INIT_DB (USE)");
        writer.ok().await
    }
}
