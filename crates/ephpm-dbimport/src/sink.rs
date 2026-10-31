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

/// Where sanitized statements are executed.
///
/// The driver calls [`begin`](Self::begin) / [`commit`](Self::commit) around
/// each batch and [`exec`](Self::exec) for every statement; a failing batch is
/// [`rollback`](Self::rollback)ed. Transaction methods are no-ops for a sink
/// whose backend does not support them.
pub trait StatementSink {
    /// Execute one statement.
    ///
    /// # Errors
    ///
    /// Returns a [`SinkError`] if the backend rejects the statement.
    fn exec(&mut self, sql: &str) -> Result<(), SinkError>;

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
