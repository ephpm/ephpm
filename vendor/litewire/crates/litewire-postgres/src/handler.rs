//! PostgreSQL wire protocol handler.
//!
//! Implements `SimpleQueryHandler` and `ExtendedQueryHandler` from the `pgwire`
//! crate, translating incoming PostgreSQL SQL to SQLite via `litewire-translate`
//! and executing against the backend.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream;
use pgwire::api::portal::{Format, Portal};
use pgwire::api::query::{ExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{
    DataRowEncoder, DescribePortalResponse, DescribeStatementResponse, FieldInfo,
    QueryResponse, Response, Tag,
};
use pgwire::api::stmt::{NoopQueryParser, StoredStatement};
use pgwire::api::{ClientInfo, Type};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::data::DataRow;
use tracing::{debug, warn};

use litewire_backend::{SharedBackend, Value};
use litewire_translate::{self, Dialect, StatementKind, TranslateResult, classify};

use crate::types::sqlite_to_pg_type;

/// Handler for a single PostgreSQL client connection.
pub struct PostgresHandler {
    backend: SharedBackend,
    query_parser: Arc<NoopQueryParser>,
}

impl PostgresHandler {
    pub fn new(backend: SharedBackend) -> Self {
        Self {
            backend,
            query_parser: Arc::new(NoopQueryParser::new()),
        }
    }

    /// Translate SQL from PostgreSQL dialect to SQLite and classify it.
    fn translate_sql(&self, query: &str) -> Result<(String, StatementKind), String> {
        let translated = litewire_translate::translate(query, Dialect::PostgreSQL)
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

    /// Execute a query and return a pgwire `Response`.
    async fn exec_query<'a>(
        &self,
        sql: &str,
        params: &[Value],
        format: &Format,
    ) -> PgWireResult<Response<'a>> {
        let kind = classify(sql);
        match kind {
            StatementKind::Query => self.do_select(sql, params, format).await,
            _ => self.do_mutation(sql, params, &kind).await,
        }
    }

    /// Execute a SELECT and build a query response with rows.
    async fn do_select<'a>(
        &self,
        sql: &str,
        params: &[Value],
        format: &Format,
    ) -> PgWireResult<Response<'a>> {
        let rs = self
            .backend
            .query(sql, params)
            .await
            .map_err(|e| PgWireError::ApiError(Box::new(e)))?;

        // Build field info, inferring types from declared type first,
        // then falling back to the first row's actual values for expressions
        // without a declared type (e.g. `SELECT 1 + 2`).
        let fields: Vec<FieldInfo> = rs
            .columns
            .iter()
            .enumerate()
            .map(|(idx, col)| {
                let pg_type = if col.decltype.is_some() {
                    sqlite_to_pg_type(col.decltype.as_deref())
                } else {
                    // Infer from the first row's actual value.
                    rs.rows
                        .first()
                        .and_then(|row| row.get(idx))
                        .map(value_to_pg_type)
                        .unwrap_or(Type::TEXT)
                };
                FieldInfo::new(
                    col.name.clone(),
                    None,
                    None,
                    pg_type,
                    format.format_for(idx),
                )
            })
            .collect();

        let schema = Arc::new(fields);
        let mut rows: Vec<PgWireResult<DataRow>> = Vec::with_capacity(rs.rows.len());

        for row in &rs.rows {
            let mut encoder = DataRowEncoder::new(schema.clone());
            for (idx, val) in row.iter().enumerate() {
                encode_value(&mut encoder, val, &schema[idx])?;
            }
            rows.push(encoder.finish());
        }

        Ok(Response::Query(QueryResponse::new(
            schema,
            stream::iter(rows),
        )))
    }

    /// Execute a mutation (INSERT/UPDATE/DELETE/DDL) and return an execution response.
    async fn do_mutation<'a>(
        &self,
        sql: &str,
        params: &[Value],
        kind: &StatementKind,
    ) -> PgWireResult<Response<'a>> {
        // Transaction commands need special Response variants, handle before
        // the generic execute path to avoid double-execution.
        if *kind == StatementKind::Transaction {
            self.backend
                .execute(sql, params)
                .await
                .map_err(|e| PgWireError::ApiError(Box::new(e)))?;

            let upper = sql.trim().to_ascii_uppercase();
            if upper.starts_with("BEGIN") || upper.starts_with("START") {
                return Ok(Response::TransactionStart(Tag::new("BEGIN")));
            }
            if upper.starts_with("COMMIT") {
                return Ok(Response::TransactionEnd(Tag::new("COMMIT")));
            }
            if upper.starts_with("ROLLBACK") {
                return Ok(Response::TransactionEnd(Tag::new("ROLLBACK")));
            }
            return Ok(Response::Execution(Tag::new("OK")));
        }

        let result = self
            .backend
            .execute(sql, params)
            .await
            .map_err(|e| PgWireError::ApiError(Box::new(e)))?;

        let tag_name = match kind {
            StatementKind::Mutation => {
                let upper = sql.trim().to_ascii_uppercase();
                if upper.starts_with("INSERT") {
                    "INSERT"
                } else if upper.starts_with("UPDATE") {
                    "UPDATE"
                } else if upper.starts_with("DELETE") {
                    "DELETE"
                } else {
                    "OK"
                }
            }
            StatementKind::Ddl => {
                let upper = sql.trim().to_ascii_uppercase();
                if upper.starts_with("CREATE") {
                    "CREATE"
                } else if upper.starts_with("DROP") {
                    "DROP"
                } else if upper.starts_with("ALTER") {
                    "ALTER"
                } else {
                    "OK"
                }
            }
            _ => "OK",
        };

        let mut tag = Tag::new(tag_name);
        if *kind == StatementKind::Mutation {
            tag = tag.with_rows(result.affected_rows as usize);
        }

        Ok(Response::Execution(tag))
    }

    /// Build column metadata for a query by probing with LIMIT 1.
    ///
    /// Uses LIMIT 1 (not LIMIT 0) so we can infer types from actual data
    /// when columns lack declared types (e.g. `SELECT 1 + 2`).
    async fn probe_columns(
        &self,
        sql: &str,
        format: &Format,
    ) -> PgWireResult<Vec<FieldInfo>> {
        let probe = format!("{sql} LIMIT 1");
        match self.backend.query(&probe, &[]).await {
            Ok(rs) => Ok(rs
                .columns
                .iter()
                .enumerate()
                .map(|(idx, col)| {
                    let pg_type = if col.decltype.is_some() {
                        sqlite_to_pg_type(col.decltype.as_deref())
                    } else {
                        rs.rows
                            .first()
                            .and_then(|row| row.get(idx))
                            .map(value_to_pg_type)
                            .unwrap_or(Type::TEXT)
                    };
                    FieldInfo::new(
                        col.name.clone(),
                        None,
                        None,
                        pg_type,
                        format.format_for(idx),
                    )
                })
                .collect()),
            Err(_) => Ok(vec![]),
        }
    }
}

/// Infer the PostgreSQL type from an actual runtime value.
///
/// Used when a column has no declared type (e.g. expression results).
fn value_to_pg_type(val: &Value) -> Type {
    match val {
        Value::Null => Type::TEXT,
        Value::Integer(_) => Type::INT8,
        Value::Float(_) => Type::FLOAT8,
        Value::Text(_) => Type::TEXT,
        Value::Blob(_) => Type::BYTEA,
    }
}

/// Encode a single backend `Value` into a pgwire `DataRowEncoder`.
fn encode_value(
    encoder: &mut DataRowEncoder,
    val: &Value,
    field: &FieldInfo,
) -> PgWireResult<()> {
    match val {
        Value::Null => encoder.encode_field(&None::<i8>),
        Value::Integer(i) => {
            // Encode based on the declared PG type.
            if *field.datatype() == Type::BOOL {
                encoder.encode_field(&(*i != 0))
            } else {
                encoder.encode_field(i)
            }
        }
        Value::Float(f) => encoder.encode_field(f),
        Value::Text(s) => encoder.encode_field(&s.as_str()),
        Value::Blob(b) => encoder.encode_field(&b.as_slice()),
    }
}

/// Extract parameter values from a portal, converting from PG binary format
/// to backend `Value` types.
fn extract_params(portal: &Portal<String>) -> Vec<Value> {
    let mut values = Vec::with_capacity(portal.parameter_len());
    for i in 0..portal.parameter_len() {
        let param_type = portal
            .statement
            .parameter_types
            .get(i)
            .cloned()
            .unwrap_or(Type::TEXT);

        let val = match &param_type {
            t if *t == Type::BOOL => portal
                .parameter::<bool>(i, &param_type)
                .ok()
                .flatten()
                .map_or(Value::Null, |v| Value::Integer(i64::from(v))),

            t if *t == Type::INT2 => portal
                .parameter::<i16>(i, &param_type)
                .ok()
                .flatten()
                .map_or(Value::Null, |v| Value::Integer(i64::from(v))),

            t if *t == Type::INT4 => portal
                .parameter::<i32>(i, &param_type)
                .ok()
                .flatten()
                .map_or(Value::Null, |v| Value::Integer(i64::from(v))),

            t if *t == Type::INT8 => portal
                .parameter::<i64>(i, &param_type)
                .ok()
                .flatten()
                .map_or(Value::Null, |v| Value::Integer(v)),

            t if *t == Type::FLOAT4 => portal
                .parameter::<f32>(i, &param_type)
                .ok()
                .flatten()
                .map_or(Value::Null, |v| Value::Float(f64::from(v))),

            t if *t == Type::FLOAT8 => portal
                .parameter::<f64>(i, &param_type)
                .ok()
                .flatten()
                .map_or(Value::Null, |v| Value::Float(v)),

            t if *t == Type::BYTEA => portal
                .parameter::<Vec<u8>>(i, &param_type)
                .ok()
                .flatten()
                .map_or(Value::Null, Value::Blob),

            // Default: treat as text
            _ => portal
                .parameter::<String>(i, &Type::TEXT)
                .ok()
                .flatten()
                .map_or(Value::Null, Value::Text),
        };
        values.push(val);
    }
    values
}

/// Build a PgWireError from an error string.
fn pg_error(msg: &str) -> PgWireError {
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".to_owned(),
        "XX000".to_owned(),
        msg.to_owned(),
    )))
}

#[async_trait]
impl SimpleQueryHandler for PostgresHandler {
    async fn do_query<'a, C>(
        &self,
        _client: &mut C,
        query: &'a str,
    ) -> PgWireResult<Vec<Response<'a>>>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        debug!(sql = %query, "PG simple query");

        let translated = litewire_translate::translate(query, Dialect::PostgreSQL)
            .map_err(|e| {
                warn!("SQL translation error: {e}");
                pg_error(&e.to_string())
            })?;

        let mut responses = Vec::new();

        for result in translated {
            let resp = match result {
                TranslateResult::Noop => {
                    Response::Execution(Tag::new("SET"))
                }
                TranslateResult::Metadata(meta) => {
                    let sqlite_sql = meta.to_sqlite_sql();
                    self.exec_query(&sqlite_sql, &[], &Format::UnifiedText)
                        .await?
                }
                TranslateResult::Sql(sqlite_sql) => {
                    if sqlite_sql.is_empty() {
                        Response::Execution(Tag::new("OK"))
                    } else {
                        self.exec_query(&sqlite_sql, &[], &Format::UnifiedText)
                            .await?
                    }
                }
            };
            responses.push(resp);
        }

        if responses.is_empty() {
            responses.push(Response::Execution(Tag::new("OK")));
        }

        Ok(responses)
    }
}

#[async_trait]
impl ExtendedQueryHandler for PostgresHandler {
    type Statement = String;
    type QueryParser = NoopQueryParser;

    fn query_parser(&self) -> Arc<Self::QueryParser> {
        self.query_parser.clone()
    }

    async fn do_query<'a, 'b: 'a, C>(
        &'b self,
        _client: &mut C,
        portal: &'a Portal<Self::Statement>,
        _max_rows: usize,
    ) -> PgWireResult<Response<'a>>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let query = &portal.statement.statement;
        debug!(sql = %query, "PG extended query execute");

        let (sqlite_sql, _kind) = self.translate_sql(query).map_err(|e| pg_error(&e))?;

        if sqlite_sql.is_empty() {
            return Ok(Response::Execution(Tag::new("OK")));
        }

        let params = extract_params(portal);
        self.exec_query(&sqlite_sql, &params, &portal.result_column_format)
            .await
    }

    async fn do_describe_statement<C>(
        &self,
        _client: &mut C,
        stmt: &StoredStatement<Self::Statement>,
    ) -> PgWireResult<DescribeStatementResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let (sqlite_sql, kind) = self
            .translate_sql(&stmt.statement)
            .map_err(|e| pg_error(&e))?;

        let param_types = stmt.parameter_types.clone();

        if kind == StatementKind::Query && !sqlite_sql.is_empty() {
            let fields = self
                .probe_columns(&sqlite_sql, &Format::UnifiedBinary)
                .await?;
            Ok(DescribeStatementResponse::new(param_types, fields))
        } else {
            Ok(DescribeStatementResponse::new(param_types, vec![]))
        }
    }

    async fn do_describe_portal<C>(
        &self,
        _client: &mut C,
        portal: &Portal<Self::Statement>,
    ) -> PgWireResult<DescribePortalResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let (sqlite_sql, kind) = self
            .translate_sql(&portal.statement.statement)
            .map_err(|e| pg_error(&e))?;

        if kind == StatementKind::Query && !sqlite_sql.is_empty() {
            let fields = self
                .probe_columns(&sqlite_sql, &portal.result_column_format)
                .await?;
            Ok(DescribePortalResponse::new(fields))
        } else {
            Ok(DescribePortalResponse::new(vec![]))
        }
    }
}
