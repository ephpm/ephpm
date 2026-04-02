//! Hrana protocol request/response types.
//!
//! Implements the Hrana 3 pipeline protocol types as defined by Turso/sqld.

use litewire_backend::Value;
use serde::{Deserialize, Serialize};

// ── Request types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineRequest {
    pub baton: Option<String>,
    pub requests: Vec<StreamRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamRequest {
    Execute(ExecuteRequest),
    Close,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteRequest {
    pub stmt: StmtRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StmtRequest {
    pub sql: String,
    #[serde(default)]
    pub args: Vec<HranaValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HranaValue {
    Null,
    Integer { value: String },
    Float { value: f64 },
    Text { value: String },
    Blob { base64: String },
}

impl HranaValue {
    /// Convert to a litewire backend [`Value`].
    pub fn to_backend_value(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Integer { value } => {
                Value::Integer(value.parse().unwrap_or(0))
            }
            Self::Float { value } => Value::Float(*value),
            Self::Text { value } => Value::Text(value.clone()),
            Self::Blob { base64 } => {
                use base64::Engine;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(base64)
                    .unwrap_or_default();
                Value::Blob(bytes)
            }
        }
    }
}

// ── Response types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineResponse {
    pub baton: Option<String>,
    pub base_url: Option<String>,
    pub results: Vec<StreamResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamResult {
    Ok { response: StreamResponse },
    Error { error: ErrorResponse },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamResponse {
    Execute { result: ExecuteResponse },
    Close,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteResponse {
    pub cols: Vec<ColResponse>,
    pub rows: Vec<Vec<ResponseValue>>,
    pub affected_row_count: u64,
    pub last_insert_rowid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColResponse {
    pub name: String,
    pub decltype: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseValue {
    Null,
    Integer { value: String },
    Float { value: f64 },
    Text { value: String },
    Blob { base64: String },
}

impl ResponseValue {
    /// Convert from a litewire backend [`Value`].
    pub fn from_backend_value(val: &Value) -> Self {
        match val {
            Value::Null => Self::Null,
            Value::Integer(i) => Self::Integer {
                value: i.to_string(),
            },
            Value::Float(f) => Self::Float { value: *f },
            Value::Text(s) => Self::Text {
                value: s.clone(),
            },
            Value::Blob(b) => {
                use base64::Engine;
                Self::Blob {
                    base64: base64::engine::general_purpose::STANDARD.encode(b),
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub message: String,
    pub code: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use litewire_backend::Value;

    // ── HranaValue -> backend Value ─────────────────────────────────────────

    #[test]
    fn null_to_backend() {
        let v = HranaValue::Null;
        assert!(matches!(v.to_backend_value(), Value::Null));
    }

    #[test]
    fn integer_to_backend() {
        let v = HranaValue::Integer {
            value: "42".into(),
        };
        assert!(matches!(v.to_backend_value(), Value::Integer(42)));
    }

    #[test]
    fn integer_invalid_to_backend() {
        let v = HranaValue::Integer {
            value: "not_a_number".into(),
        };
        assert!(matches!(v.to_backend_value(), Value::Integer(0)));
    }

    #[test]
    fn float_to_backend() {
        let v = HranaValue::Float { value: 2.72 };
        assert!(matches!(v.to_backend_value(), Value::Float(f) if (f - 2.72).abs() < f64::EPSILON));
    }

    #[test]
    fn text_to_backend() {
        let v = HranaValue::Text {
            value: "hello".into(),
        };
        assert!(matches!(v.to_backend_value(), Value::Text(s) if s == "hello"));
    }

    #[test]
    fn blob_to_backend() {
        use base64::Engine;
        let data = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
        let v = HranaValue::Blob { base64: encoded };
        assert!(matches!(v.to_backend_value(), Value::Blob(b) if b == data));
    }

    #[test]
    fn blob_invalid_base64_to_backend() {
        let v = HranaValue::Blob {
            base64: "!!!invalid!!!".into(),
        };
        // Invalid base64 should return empty blob.
        assert!(matches!(v.to_backend_value(), Value::Blob(b) if b.is_empty()));
    }

    // ── backend Value -> ResponseValue ──────────────────────────────────────

    #[test]
    fn null_from_backend() {
        let rv = ResponseValue::from_backend_value(&Value::Null);
        assert!(matches!(rv, ResponseValue::Null));
    }

    #[test]
    fn integer_from_backend() {
        let rv = ResponseValue::from_backend_value(&Value::Integer(42));
        match rv {
            ResponseValue::Integer { value } => assert_eq!(value, "42"),
            other => panic!("expected Integer, got: {other:?}"),
        }
    }

    #[test]
    fn float_from_backend() {
        let rv = ResponseValue::from_backend_value(&Value::Float(2.72));
        match rv {
            ResponseValue::Float { value } => assert!((value - 2.72).abs() < f64::EPSILON),
            other => panic!("expected Float, got: {other:?}"),
        }
    }

    #[test]
    fn text_from_backend() {
        let rv = ResponseValue::from_backend_value(&Value::Text("hello".into()));
        match rv {
            ResponseValue::Text { value } => assert_eq!(value, "hello"),
            other => panic!("expected Text, got: {other:?}"),
        }
    }

    #[test]
    fn blob_from_backend_roundtrip() {
        let data = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let rv = ResponseValue::from_backend_value(&Value::Blob(data.clone()));
        match rv {
            ResponseValue::Blob { base64: encoded } => {
                use base64::Engine;
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(&encoded)
                    .unwrap();
                assert_eq!(decoded, data);
            }
            other => panic!("expected Blob, got: {other:?}"),
        }
    }

    // ── Serde roundtrip ─────────────────────────────────────────────────────

    #[test]
    fn pipeline_request_deserialize() {
        let json = r#"{
            "baton": null,
            "requests": [
                {
                    "type": "execute",
                    "stmt": {
                        "sql": "SELECT 1",
                        "args": [
                            {"type": "integer", "value": "42"},
                            {"type": "null"},
                            {"type": "text", "value": "hello"}
                        ]
                    }
                },
                {"type": "close"}
            ]
        }"#;
        let req: PipelineRequest = serde_json::from_str(json).unwrap();
        assert!(req.baton.is_none());
        assert_eq!(req.requests.len(), 2);
        match &req.requests[0] {
            StreamRequest::Execute(exec) => {
                assert_eq!(exec.stmt.sql, "SELECT 1");
                assert_eq!(exec.stmt.args.len(), 3);
            }
            StreamRequest::Close => panic!("expected Execute"),
        }
        assert!(matches!(req.requests[1], StreamRequest::Close));
    }

    #[test]
    fn pipeline_response_serialize() {
        let resp = PipelineResponse {
            baton: None,
            base_url: None,
            results: vec![
                StreamResult::Ok {
                    response: StreamResponse::Execute {
                        result: ExecuteResponse {
                            cols: vec![ColResponse {
                                name: "id".into(),
                                decltype: Some("INTEGER".into()),
                            }],
                            rows: vec![vec![ResponseValue::Integer {
                                value: "1".into(),
                            }]],
                            affected_row_count: 0,
                            last_insert_rowid: None,
                        },
                    },
                },
                StreamResult::Error {
                    error: ErrorResponse {
                        message: "test error".into(),
                        code: Some("SQLITE_ERROR".into()),
                    },
                },
            ],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"id\""));
        assert!(json.contains("test error"));
        assert!(json.contains("SQLITE_ERROR"));
    }
}
