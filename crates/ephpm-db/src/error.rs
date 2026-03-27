//! Error types for the database proxy.

use thiserror::Error;

/// Errors produced by the database proxy and connection pool.
#[derive(Debug, Error)]
pub enum DbError {
    /// TCP or I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Backend authentication or handshake failure.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// Wire protocol parsing or framing error.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// All pool connections are in use and `pool_timeout` elapsed.
    #[error("pool timeout: all {max} connections are busy")]
    PoolTimeout { max: u32 },

    /// The pool has been shut down.
    #[error("pool is closed")]
    PoolClosed,

    /// The database URL could not be parsed.
    #[error("invalid database URL: {0}")]
    InvalidUrl(String),

    /// A duration string in the config could not be parsed.
    #[error("invalid duration '{value}': {reason}")]
    InvalidDuration { value: String, reason: String },
}
