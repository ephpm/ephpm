//! Hrana HTTP client backend.
//!
//! Implements the [`Backend`] trait by forwarding queries to a remote
//! server (typically sqld) via the Hrana 3 pipeline protocol over HTTP.
//!
//! The Hrana protocol types are defined inline here to avoid a cyclic
//! dependency between `litewire-backend` and `litewire-hrana`.

use serde::{Deserialize, Serialize};

use crate::{Backend, BackendError, Column, ExecuteResult, ResultSet, Value};

// ── Hrana 3 wire types (inline) ─────────────────────────────────────────────

#[derive(Serialize)]
struct PipelineRequest {
    baton: Option<String>,
    requests: Vec<StreamRequest>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamRequest {
    Execute(ExecuteRequest),
}

#[derive(Serialize)]
struct ExecuteRequest {
    stmt: StmtRequest,
}

#[derive(Serialize)]
struct StmtRequest {
    sql: String,
    args: Vec<HranaValue>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HranaValue {
    Null,
    Integer { value: String },
    Float { value: f64 },
    Text { value: String },
    Blob { base64: String },
}

#[derive(Deserialize)]
struct PipelineResponse {
    results: Vec<StreamResult>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamResult {
    Ok { response: StreamResponse },
    Error { error: ErrorResponse },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamResponse {
    Execute { result: ExecuteResponse },
    Close,
}

#[derive(Deserialize)]
struct ExecuteResponse {
    cols: Vec<ColResponse>,
    rows: Vec<Vec<ResponseValue>>,
    affected_row_count: u64,
    last_insert_rowid: Option<String>,
}

#[derive(Deserialize)]
struct ColResponse {
    name: String,
    decltype: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponseValue {
    Null,
    Integer { value: String },
    Float { value: f64 },
    Text { value: String },
    Blob { base64: String },
}

#[derive(Deserialize)]
struct ErrorResponse {
    message: String,
    code: Option<String>,
}

// ── Client ──────────────────────────────────────────────────────────────────

/// A backend that talks to sqld (or any Hrana-compatible server) over HTTP.
///
/// Uses `reqwest` with HTTP/2 connection reuse for minimal overhead on
/// localhost. Thread-safe and cheaply cloneable.
#[derive(Clone)]
pub struct HranaClient {
    client: reqwest::Client,
    pipeline_url: String,
    health_url: String,
}

impl HranaClient {
    /// Create a new Hrana client pointing at the given base URL.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use litewire_backend::HranaClient;
    /// let client = HranaClient::new("http://127.0.0.1:8081");
    /// ```
    #[must_use]
    pub fn new(base_url: &str) -> Self {
        let base = base_url.trim_end_matches('/');
        Self {
            client: reqwest::Client::new(),
            pipeline_url: format!("{base}/v2/pipeline"),
            health_url: format!("{base}/health"),
        }
    }

    /// Check if the remote server is healthy.
    ///
    /// # Errors
    ///
    /// Returns an error if the health endpoint is unreachable or returns
    /// a non-success status.
    pub async fn health_check(&self) -> Result<(), BackendError> {
        let resp = self
            .client
            .get(&self.health_url)
            .send()
            .await
            .map_err(|e| BackendError::Other(format!("health check failed: {e}")))?;

        if resp.status().is_success() {
            Ok(())
        } else {
            Err(BackendError::Other(format!(
                "health check returned {}",
                resp.status()
            )))
        }
    }

    /// Send a single statement via the Hrana pipeline and return the response.
    async fn execute_pipeline(
        &self,
        sql: &str,
        params: &[Value],
    ) -> Result<ExecuteResponse, BackendError> {
        let args: Vec<HranaValue> = params.iter().map(value_to_hrana).collect();

        let request = PipelineRequest {
            baton: None,
            requests: vec![StreamRequest::Execute(ExecuteRequest {
                stmt: StmtRequest {
                    sql: sql.to_string(),
                    args,
                },
            })],
        };

        let resp = self
            .client
            .post(&self.pipeline_url)
            .json(&request)
            .send()
            .await
            .map_err(|e| BackendError::Other(format!("HTTP request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(BackendError::Other(format!(
                "sqld returned {status}: {body}"
            )));
        }

        let pipeline: PipelineResponse = resp
            .json()
            .await
            .map_err(|e| BackendError::Other(format!("failed to parse response: {e}")))?;

        let result = pipeline
            .results
            .into_iter()
            .next()
            .ok_or_else(|| BackendError::Other("empty pipeline response".to_string()))?;

        match result {
            StreamResult::Ok { response } => match response {
                StreamResponse::Execute { result } => Ok(result),
                StreamResponse::Close => {
                    Err(BackendError::Other("unexpected close response".to_string()))
                }
            },
            StreamResult::Error { error } => Err(hrana_error_to_backend(error)),
        }
    }
}

#[async_trait::async_trait]
impl Backend for HranaClient {
    async fn query(&self, sql: &str, params: &[Value]) -> Result<ResultSet, BackendError> {
        let exec = self.execute_pipeline(sql, params).await?;
        Ok(execute_response_to_result_set(exec))
    }

    async fn execute(&self, sql: &str, params: &[Value]) -> Result<ExecuteResult, BackendError> {
        let exec = self.execute_pipeline(sql, params).await?;
        Ok(ExecuteResult {
            affected_rows: exec.affected_row_count,
            last_insert_rowid: exec
                .last_insert_rowid
                .as_deref()
                .and_then(|s| s.parse().ok()),
        })
    }
}

// ── Conversions ─────────────────────────────────────────────────────────────

/// Convert a backend [`Value`] to a Hrana wire value.
fn value_to_hrana(val: &Value) -> HranaValue {
    match val {
        Value::Null => HranaValue::Null,
        Value::Integer(i) => HranaValue::Integer {
            value: i.to_string(),
        },
        Value::Float(f) => HranaValue::Float { value: *f },
        Value::Text(s) => HranaValue::Text {
            value: s.clone(),
        },
        Value::Blob(b) => {
            use base64::Engine;
            HranaValue::Blob {
                base64: base64::engine::general_purpose::STANDARD.encode(b),
            }
        }
    }
}

/// Convert a Hrana [`ResponseValue`] back to a backend [`Value`].
fn response_value_to_backend(rv: &ResponseValue) -> Value {
    match rv {
        ResponseValue::Null => Value::Null,
        ResponseValue::Integer { value } => Value::Integer(value.parse().unwrap_or(0)),
        ResponseValue::Float { value } => Value::Float(*value),
        ResponseValue::Text { value } => Value::Text(value.clone()),
        ResponseValue::Blob { base64: b64 } => {
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .unwrap_or_default();
            Value::Blob(bytes)
        }
    }
}

/// Convert an [`ExecuteResponse`] to a [`ResultSet`].
fn execute_response_to_result_set(exec: ExecuteResponse) -> ResultSet {
    let columns = exec
        .cols
        .iter()
        .map(|c| Column {
            name: c.name.clone(),
            decltype: c.decltype.clone(),
        })
        .collect();

    let rows = exec
        .rows
        .iter()
        .map(|row| row.iter().map(response_value_to_backend).collect())
        .collect();

    ResultSet { columns, rows }
}

/// Convert a Hrana error to a [`BackendError`].
fn hrana_error_to_backend(err: ErrorResponse) -> BackendError {
    match err.code.as_deref() {
        Some(code) if code.starts_with("SQLITE") => {
            BackendError::Sqlite(format!("[{code}] {}", err.message))
        }
        _ => BackendError::Other(err.message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_to_hrana_null() {
        assert!(matches!(value_to_hrana(&Value::Null), HranaValue::Null));
    }

    #[test]
    fn value_to_hrana_integer() {
        match value_to_hrana(&Value::Integer(42)) {
            HranaValue::Integer { value } => assert_eq!(value, "42"),
            other => panic!("expected Integer, got: {other:?}"),
        }
    }

    #[test]
    fn value_to_hrana_text() {
        match value_to_hrana(&Value::Text("hello".into())) {
            HranaValue::Text { value } => assert_eq!(value, "hello"),
            other => panic!("expected Text, got: {other:?}"),
        }
    }

    #[test]
    fn value_to_hrana_blob_roundtrip() {
        let data = vec![0xDE, 0xAD, 0xBE, 0xEF];
        match value_to_hrana(&Value::Blob(data.clone())) {
            HranaValue::Blob { base64: b64 } => {
                use base64::Engine;
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(&b64)
                    .unwrap();
                assert_eq!(decoded, data);
            }
            other => panic!("expected Blob, got: {other:?}"),
        }
    }

    #[test]
    fn response_value_roundtrip() {
        let cases = vec![
            (Value::Null, ResponseValue::Null),
            (
                Value::Integer(123),
                ResponseValue::Integer {
                    value: "123".into(),
                },
            ),
            (Value::Float(3.14), ResponseValue::Float { value: 3.14 }),
            (
                Value::Text("test".into()),
                ResponseValue::Text {
                    value: "test".into(),
                },
            ),
        ];

        for (val, rv) in cases {
            let back = response_value_to_backend(&rv);
            assert_eq!(back, val, "roundtrip failed for {val:?}");
        }
    }

    #[test]
    fn execute_response_to_result_set_maps_correctly() {
        let exec = ExecuteResponse {
            cols: vec![
                ColResponse {
                    name: "id".into(),
                    decltype: Some("INTEGER".into()),
                },
                ColResponse {
                    name: "name".into(),
                    decltype: Some("TEXT".into()),
                },
            ],
            rows: vec![
                vec![
                    ResponseValue::Integer {
                        value: "1".into(),
                    },
                    ResponseValue::Text {
                        value: "alice".into(),
                    },
                ],
                vec![
                    ResponseValue::Integer {
                        value: "2".into(),
                    },
                    ResponseValue::Text {
                        value: "bob".into(),
                    },
                ],
            ],
            affected_row_count: 0,
            last_insert_rowid: None,
        };

        let rs = execute_response_to_result_set(exec);
        assert_eq!(rs.columns.len(), 2);
        assert_eq!(rs.columns[0].name, "id");
        assert_eq!(rs.columns[0].decltype.as_deref(), Some("INTEGER"));
        assert_eq!(rs.rows.len(), 2);
        assert_eq!(rs.rows[0][0], Value::Integer(1));
        assert_eq!(rs.rows[0][1], Value::Text("alice".into()));
        assert_eq!(rs.rows[1][0], Value::Integer(2));
    }

    #[test]
    fn hrana_sqlite_error() {
        let err = hrana_error_to_backend(ErrorResponse {
            message: "no such table: foo".into(),
            code: Some("SQLITE_ERROR".into()),
        });
        match err {
            BackendError::Sqlite(msg) => assert!(msg.contains("no such table")),
            other => panic!("expected Sqlite error, got: {other:?}"),
        }
    }

    #[test]
    fn hrana_generic_error() {
        let err = hrana_error_to_backend(ErrorResponse {
            message: "something broke".into(),
            code: None,
        });
        match err {
            BackendError::Other(msg) => assert_eq!(msg, "something broke"),
            other => panic!("expected Other error, got: {other:?}"),
        }
    }
}
