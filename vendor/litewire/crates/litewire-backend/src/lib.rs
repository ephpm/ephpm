//! Backend trait and implementations for litewire.
//!
//! The backend abstracts over how SQL gets executed. litewire doesn't care
//! whether SQLite is in-process or remote -- it only needs `query` and
//! `execute`.

#[cfg(feature = "rusqlite")]
pub mod rusqlite_backend;

#[cfg(feature = "hrana-client")]
pub mod hrana_client;

use std::fmt;
use std::sync::Arc;

/// A dynamically-typed SQL value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Float(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => write!(f, "NULL"),
            Self::Integer(v) => write!(f, "{v}"),
            Self::Float(v) => write!(f, "{v}"),
            Self::Text(v) => write!(f, "{v}"),
            Self::Blob(v) => write!(f, "<blob {} bytes>", v.len()),
        }
    }
}

/// Column metadata in a result set.
#[derive(Clone, Debug)]
pub struct Column {
    /// Column name (or alias).
    pub name: String,
    /// Declared type from the schema (e.g., `"INTEGER"`, `"TEXT"`).
    /// `None` for expressions without a declared type.
    pub decltype: Option<String>,
}

/// A set of rows returned from a query.
#[derive(Clone, Debug)]
pub struct ResultSet {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
}

/// Result of a non-query execution (INSERT, UPDATE, DELETE).
#[derive(Clone, Debug)]
pub struct ExecuteResult {
    pub affected_rows: u64,
    pub last_insert_rowid: Option<i64>,
}

/// Errors from backend operations.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("SQLite error: {0}")]
    Sqlite(String),

    #[error("backend error: {0}")]
    Other(String),
}

/// The core backend trait. Implementations execute SQL against a storage engine.
#[async_trait::async_trait]
pub trait Backend: Send + Sync + 'static {
    /// Execute a query that returns rows.
    async fn query(&self, sql: &str, params: &[Value]) -> Result<ResultSet, BackendError>;

    /// Execute a statement that modifies data.
    async fn execute(&self, sql: &str, params: &[Value]) -> Result<ExecuteResult, BackendError>;
}

/// Type alias for a shared backend reference.
pub type SharedBackend = Arc<dyn Backend>;

#[cfg(feature = "rusqlite")]
pub use rusqlite_backend::Rusqlite;

#[cfg(feature = "hrana-client")]
pub use hrana_client::HranaClient;

#[cfg(test)]
mod tests {
    use super::*;

    // ── Value Display ───────────────────────────────────────────────────────

    #[test]
    fn display_null() {
        assert_eq!(format!("{}", Value::Null), "NULL");
    }

    #[test]
    fn display_integer() {
        assert_eq!(format!("{}", Value::Integer(42)), "42");
        assert_eq!(format!("{}", Value::Integer(-1)), "-1");
        assert_eq!(format!("{}", Value::Integer(0)), "0");
    }

    #[test]
    fn display_float() {
        let s = format!("{}", Value::Float(2.72));
        assert!(s.starts_with("2.72"));
    }

    #[test]
    fn display_text() {
        assert_eq!(format!("{}", Value::Text("hello".into())), "hello");
        assert_eq!(format!("{}", Value::Text(String::new())), "");
    }

    #[test]
    fn display_blob() {
        assert_eq!(
            format!("{}", Value::Blob(vec![0xDE, 0xAD])),
            "<blob 2 bytes>"
        );
        assert_eq!(format!("{}", Value::Blob(vec![])), "<blob 0 bytes>");
    }

    // ── Value PartialEq ─────────────────────────────────────────────────────

    #[test]
    fn value_equality() {
        assert_eq!(Value::Null, Value::Null);
        assert_eq!(Value::Integer(1), Value::Integer(1));
        assert_ne!(Value::Integer(1), Value::Integer(2));
        assert_ne!(Value::Integer(1), Value::Text("1".into()));
        assert_eq!(Value::Text("a".into()), Value::Text("a".into()));
        assert_ne!(Value::Text("a".into()), Value::Text("b".into()));
        assert_eq!(Value::Blob(vec![1, 2]), Value::Blob(vec![1, 2]));
        assert_ne!(Value::Blob(vec![1]), Value::Blob(vec![2]));
    }

    // ── BackendError Display ────────────────────────────────────────────────

    #[test]
    fn backend_error_display() {
        let e = BackendError::Sqlite("table not found".into());
        assert!(e.to_string().contains("table not found"));

        let e = BackendError::Other("connection failed".into());
        assert!(e.to_string().contains("connection failed"));
    }

    // ── Column ──────────────────────────────────────────────────────────────

    #[test]
    fn column_with_decltype() {
        let c = Column {
            name: "id".into(),
            decltype: Some("INTEGER".into()),
        };
        assert_eq!(c.name, "id");
        assert_eq!(c.decltype.as_deref(), Some("INTEGER"));
    }

    #[test]
    fn column_without_decltype() {
        let c = Column {
            name: "expr".into(),
            decltype: None,
        };
        assert!(c.decltype.is_none());
    }

    // ── ExecuteResult ───────────────────────────────────────────────────────

    #[test]
    fn execute_result_no_insert() {
        let r = ExecuteResult {
            affected_rows: 3,
            last_insert_rowid: None,
        };
        assert_eq!(r.affected_rows, 3);
        assert!(r.last_insert_rowid.is_none());
    }

    // ── ResultSet ───────────────────────────────────────────────────────────

    #[test]
    fn empty_result_set() {
        let rs = ResultSet {
            columns: vec![
                Column { name: "a".into(), decltype: None },
                Column { name: "b".into(), decltype: None },
            ],
            rows: vec![],
        };
        assert_eq!(rs.columns.len(), 2);
        assert!(rs.rows.is_empty());
    }
}
