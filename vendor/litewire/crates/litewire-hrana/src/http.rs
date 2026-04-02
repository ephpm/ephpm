//! Hrana HTTP pipeline endpoint.
//!
//! Implements `POST /v2/pipeline` for sqld-compatible access.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use litewire_backend::SharedBackend;
use tracing::debug;

use crate::types::*;

/// Application state shared across requests.
#[derive(Clone)]
struct AppState {
    backend: SharedBackend,
}

/// Build the axum router for the Hrana HTTP frontend.
pub fn build_router(backend: SharedBackend) -> Router {
    let state = AppState { backend };

    Router::new()
        .route("/v2/pipeline", post(pipeline_handler))
        .route("/health", get(health_handler))
        .route("/version", get(version_handler))
        .with_state(state)
}

/// `POST /v2/pipeline` -- Hrana 3 pipeline endpoint.
async fn pipeline_handler(
    State(state): State<AppState>,
    Json(req): Json<PipelineRequest>,
) -> Result<Json<PipelineResponse>, (StatusCode, String)> {
    debug!(
        baton = ?req.baton,
        request_count = req.requests.len(),
        "Hrana pipeline request"
    );

    let mut results = Vec::with_capacity(req.requests.len());

    for stream_req in &req.requests {
        let result = match stream_req {
            StreamRequest::Execute(exec) => execute_stmt(&state.backend, &exec.stmt).await,
            StreamRequest::Close => Ok(StreamResult::Ok {
                response: StreamResponse::Close,
            }),
        };

        match result {
            Ok(r) => results.push(r),
            Err(e) => {
                results.push(StreamResult::Error {
                    error: ErrorResponse {
                        message: e.to_string(),
                        code: None,
                    },
                });
            }
        }
    }

    Ok(Json(PipelineResponse {
        baton: None, // Stateless for now (no transaction continuity).
        base_url: None,
        results,
    }))
}

/// Execute a single Hrana statement.
async fn execute_stmt(
    backend: &SharedBackend,
    stmt: &StmtRequest,
) -> Result<StreamResult, litewire_backend::BackendError> {
    let params: Vec<litewire_backend::Value> = stmt
        .args
        .iter()
        .map(|a| a.to_backend_value())
        .collect();

    // Hrana sends SQLite SQL natively -- no translation needed.
    let sql_upper = stmt.sql.trim().to_ascii_uppercase();
    let is_query = sql_upper.starts_with("SELECT")
        || sql_upper.starts_with("PRAGMA")
        || sql_upper.starts_with("EXPLAIN");

    if is_query {
        let rs = backend.query(&stmt.sql, &params).await?;

        let cols: Vec<ColResponse> = rs
            .columns
            .iter()
            .map(|c| ColResponse {
                name: c.name.clone(),
                decltype: c.decltype.clone(),
            })
            .collect();

        let rows: Vec<Vec<ResponseValue>> = rs
            .rows
            .iter()
            .map(|row| row.iter().map(ResponseValue::from_backend_value).collect())
            .collect();

        Ok(StreamResult::Ok {
            response: StreamResponse::Execute {
                result: ExecuteResponse {
                    cols,
                    rows,
                    affected_row_count: 0,
                    last_insert_rowid: None,
                },
            },
        })
    } else {
        let result = backend.execute(&stmt.sql, &params).await?;

        Ok(StreamResult::Ok {
            response: StreamResponse::Execute {
                result: ExecuteResponse {
                    cols: vec![],
                    rows: vec![],
                    affected_row_count: result.affected_rows,
                    last_insert_rowid: result.last_insert_rowid.map(|id| id.to_string()),
                },
            },
        })
    }
}

/// `GET /health` -- health check.
async fn health_handler() -> &'static str {
    "ok"
}

/// `GET /version` -- version info.
async fn version_handler() -> &'static str {
    concat!("litewire/", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use litewire_backend::Rusqlite;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn test_backend() -> SharedBackend {
        Arc::new(Rusqlite::memory().unwrap())
    }

    #[tokio::test]
    async fn health_check() {
        let app = build_router(test_backend());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"ok");
    }

    #[tokio::test]
    async fn version_endpoint() {
        let app = build_router(test_backend());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/version")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.starts_with("litewire/"), "got: {text}");
    }

    #[tokio::test]
    async fn pipeline_create_and_query() {
        let backend = test_backend();
        let app = build_router(backend);

        // Create table + insert + query in one pipeline.
        let body = serde_json::json!({
            "baton": null,
            "requests": [
                {
                    "type": "execute",
                    "stmt": { "sql": "CREATE TABLE t (id INTEGER, name TEXT)", "args": [] }
                },
                {
                    "type": "execute",
                    "stmt": {
                        "sql": "INSERT INTO t VALUES (1, 'Alice')",
                        "args": []
                    }
                },
                {
                    "type": "execute",
                    "stmt": { "sql": "SELECT * FROM t", "args": [] }
                }
            ]
        });

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/pipeline")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();

        let results = resp["results"].as_array().unwrap();
        assert_eq!(results.len(), 3);

        // All should be "ok".
        for r in results {
            assert_eq!(r["type"], "ok", "result: {r}");
        }

        // Third result should have the SELECT data.
        let select_result = &results[2]["response"]["result"];
        assert_eq!(select_result["rows"][0][0]["value"], "1");
        assert_eq!(select_result["rows"][0][1]["value"], "Alice");
    }

    #[tokio::test]
    async fn pipeline_with_params() {
        let backend = test_backend();

        // Pre-create table.
        backend
            .execute("CREATE TABLE t (id INTEGER, name TEXT)", &[])
            .await
            .unwrap();
        backend
            .execute(
                "INSERT INTO t VALUES (1, 'Alice')",
                &[],
            )
            .await
            .unwrap();

        let app = build_router(backend);

        let body = serde_json::json!({
            "baton": null,
            "requests": [
                {
                    "type": "execute",
                    "stmt": {
                        "sql": "SELECT * FROM t WHERE id = ?",
                        "args": [{"type": "integer", "value": "1"}]
                    }
                }
            ]
        });

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/pipeline")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let rows = &resp["results"][0]["response"]["result"]["rows"];
        assert_eq!(rows.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn pipeline_close_request() {
        let app = build_router(test_backend());

        let body = serde_json::json!({
            "baton": null,
            "requests": [{"type": "close"}]
        });

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/pipeline")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(resp["results"][0]["type"], "ok");
        assert_eq!(resp["results"][0]["response"]["type"], "close");
    }

    #[tokio::test]
    async fn pipeline_sql_error_returns_error_result() {
        let app = build_router(test_backend());

        let body = serde_json::json!({
            "baton": null,
            "requests": [
                {
                    "type": "execute",
                    "stmt": { "sql": "SELECT * FROM nonexistent_table", "args": [] }
                }
            ]
        });

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/pipeline")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(resp["results"][0]["type"], "error");
        assert!(resp["results"][0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("nonexistent_table"));
    }

    #[tokio::test]
    async fn pipeline_mutation_returns_affected_rows() {
        let backend = test_backend();
        backend
            .execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val TEXT)", &[])
            .await
            .unwrap();
        backend
            .execute("INSERT INTO t VALUES (1, 'a')", &[])
            .await
            .unwrap();
        backend
            .execute("INSERT INTO t VALUES (2, 'b')", &[])
            .await
            .unwrap();
        backend
            .execute("INSERT INTO t VALUES (3, 'c')", &[])
            .await
            .unwrap();

        let app = build_router(backend);

        let body = serde_json::json!({
            "baton": null,
            "requests": [
                {
                    "type": "execute",
                    "stmt": { "sql": "DELETE FROM t WHERE id >= 2", "args": [] }
                }
            ]
        });

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/pipeline")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let result = &resp["results"][0]["response"]["result"];
        assert_eq!(result["affected_row_count"], 2);
    }

    #[tokio::test]
    async fn pipeline_insert_returns_last_insert_rowid() {
        let backend = test_backend();
        backend
            .execute(
                "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, val TEXT)",
                &[],
            )
            .await
            .unwrap();

        let app = build_router(backend);

        let body = serde_json::json!({
            "baton": null,
            "requests": [
                {
                    "type": "execute",
                    "stmt": { "sql": "INSERT INTO t (val) VALUES ('hello')", "args": [] }
                }
            ]
        });

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/pipeline")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let result = &resp["results"][0]["response"]["result"];
        assert_eq!(result["affected_row_count"], 1);
        assert_eq!(result["last_insert_rowid"], "1");
    }

    #[tokio::test]
    async fn pipeline_empty_requests() {
        let app = build_router(test_backend());

        let body = serde_json::json!({
            "baton": null,
            "requests": []
        });

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/pipeline")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(resp["results"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn pipeline_pragma_is_query() {
        let backend = test_backend();
        backend
            .execute("CREATE TABLE t (id INTEGER, name TEXT)", &[])
            .await
            .unwrap();

        let app = build_router(backend);

        let body = serde_json::json!({
            "baton": null,
            "requests": [
                {
                    "type": "execute",
                    "stmt": { "sql": "PRAGMA table_info('t')", "args": [] }
                }
            ]
        });

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/pipeline")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let result = &resp["results"][0];
        assert_eq!(result["type"], "ok");
        // PRAGMA table_info returns rows with column metadata.
        let rows = result["response"]["result"]["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 2); // id and name columns
    }

    #[tokio::test]
    async fn pipeline_with_blob_param() {
        let backend = test_backend();
        backend
            .execute("CREATE TABLE t (data BLOB)", &[])
            .await
            .unwrap();

        let app = build_router(backend);

        // Insert a blob via base64-encoded param.
        let body = serde_json::json!({
            "baton": null,
            "requests": [
                {
                    "type": "execute",
                    "stmt": {
                        "sql": "INSERT INTO t VALUES (?)",
                        "args": [{"type": "blob", "base64": "3q0="}]
                    }
                },
                {
                    "type": "execute",
                    "stmt": { "sql": "SELECT * FROM t", "args": [] }
                }
            ]
        });

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/pipeline")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // The blob should come back base64-encoded.
        let val = &resp["results"][1]["response"]["result"]["rows"][0][0];
        assert_eq!(val["type"], "blob");
        assert_eq!(val["base64"], "3q0=");
    }
}
