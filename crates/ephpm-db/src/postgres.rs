//! `PostgreSQL` transparent proxy with connection pooling.
//!
//! Similar to `MySQL` proxy but for `PostgreSQL`'s native wire protocol.
//! Currently a placeholder — full implementation pending.

use crate::pool::PoolConfig;
use crate::error::DbError;

/// `PostgreSQL` proxy builder (placeholder).
///
/// TODO: Implement `PostgreSQL` wire protocol proxy with SASL authentication.
///
/// # Errors
///
/// Always returns an error — not yet implemented.
#[allow(clippy::unused_async)]
pub async fn build_proxy(
    _url: &str,
    _listen: &str,
    _pool_config: PoolConfig,
) -> Result<(), DbError> {
    Err(DbError::Auth(
        "PostgreSQL proxy not yet implemented".to_string(),
    ))
}
