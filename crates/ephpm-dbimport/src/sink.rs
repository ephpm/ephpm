//! The one outbound edge of this crate: a [`StatementSink`] the driver pushes
//! translated-but-untranslated (dialect-shaped) statements to.
//!
//! Keeping execution behind a trait is what makes the whole pipeline pure and
//! unit-testable — the fixture tests drive it with an in-memory recording
//! sink, and the `ephpm` binary implements it over a litewire `Session`
//! (offline direct-open) or a wire connection to a running server.

/// An error from a [`StatementSink`] operation.
///
/// `message` is shown to the operator and recorded in the compatibility
/// report; `code` carries the backend error code when the concrete sink has
/// one (e.g. a MySQL errno mapped by litewire), purely for diagnostics.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct SinkError {
    /// Human-readable failure message.
    pub message: String,
    /// Backend error code, when the sink has one.
    pub code: Option<u16>,
}

impl SinkError {
    /// A sink error with just a message.
    #[must_use]
    pub fn message(msg: impl Into<String>) -> Self {
        Self { message: msg.into(), code: None }
    }

    /// A sink error carrying a backend error code.
    #[must_use]
    pub fn coded(code: u16, msg: impl Into<String>) -> Self {
        Self { message: msg.into(), code: Some(code) }
    }
}

/// A dynamically-typed cell value in a [`RowSet`].
///
/// This mirrors the shape of a backend value (NULL / integer / float / text /
/// blob) without pulling litewire into this pure crate: the `ephpm` binary maps
/// its litewire `Value`s onto these when answering [`StatementSink::query_rows`],
/// and the result formatters (`ephpm db query` / `ephpm db shell`) render them.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    /// SQL `NULL`.
    Null,
    /// A 64-bit signed integer.
    Integer(i64),
    /// A double-precision float.
    Float(f64),
    /// UTF-8 text.
    Text(String),
    /// Opaque bytes (a value that was not valid UTF-8 text).
    Blob(Vec<u8>),
}

/// A set of rows returned from [`StatementSink::query_rows`].
///
/// `columns` names the result columns in order; each row in `rows` has exactly
/// `columns.len()` cells. A statement that returns no result set (an
/// `INSERT`/`UPDATE`/DDL) yields an empty `RowSet` (no columns, no rows).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RowSet {
    /// Column names, in result order.
    pub columns: Vec<String>,
    /// The rows; each is `columns.len()` cells wide.
    pub rows: Vec<Vec<Cell>>,
}

impl RowSet {
    /// An empty result set — the shape a non-`SELECT` statement produces.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Whether this result set carries no columns (a non-query statement).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }
}

/// Where sanitized statements are executed.
///
/// The driver calls [`begin`](Self::begin) / [`commit`](Self::commit) around
/// each batch and [`exec`](Self::exec) for every statement; a failing batch is
/// [`rollback`](Self::rollback)ed. Transaction methods are no-ops for a sink
/// whose backend does not support them.
///
/// [`query_rows`](Self::query_rows) is the one read path, added for the
/// interactive `ephpm db query` / `ephpm db shell` consoles that run over the
/// same concrete sinks as `db import`. It defaults to "unsupported" so the
/// import driver's recording sink (which only ever executes) needs no change.
pub trait StatementSink {
    /// Execute one statement.
    ///
    /// # Errors
    ///
    /// Returns a [`SinkError`] if the backend rejects the statement.
    fn exec(&mut self, sql: &str) -> Result<(), SinkError>;

    /// Execute one statement and return its result set.
    ///
    /// A statement with no result set (a write / DDL) returns an empty
    /// [`RowSet`]. The default implementation reports that the sink has no
    /// query path — only the `ephpm db query` / `ephpm db shell` sinks
    /// override it; the import driver never calls it.
    ///
    /// # Errors
    ///
    /// Returns a [`SinkError`] if the backend rejects the statement, or if the
    /// concrete sink does not implement a query path.
    fn query_rows(&mut self, sql: &str) -> Result<RowSet, SinkError> {
        let _ = sql;
        Err(SinkError::message("this sink does not support queries"))
    }

    /// Begin a transaction (batch boundary).
    ///
    /// # Errors
    ///
    /// Returns a [`SinkError`] if the backend rejects `BEGIN`.
    fn begin(&mut self) -> Result<(), SinkError>;

    /// Commit the current transaction.
    ///
    /// # Errors
    ///
    /// Returns a [`SinkError`] if the backend rejects `COMMIT`.
    fn commit(&mut self) -> Result<(), SinkError>;

    /// Roll the current transaction back (best-effort cleanup on a fatal
    /// error).
    ///
    /// # Errors
    ///
    /// Returns a [`SinkError`] if the backend rejects `ROLLBACK`.
    fn rollback(&mut self) -> Result<(), SinkError>;
}
