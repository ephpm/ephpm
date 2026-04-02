//! TDS (SQL Server) wire protocol frontend for litewire.
//!
//! Custom TDS 7.4 implementation. Handles Pre-Login, Login7, and SQL Batch
//! messages, translating T-SQL to SQLite and returning results as TDS token
//! streams.

mod handler;
mod packet;
mod token;

use std::net::SocketAddr;
use std::sync::Arc;

use litewire_backend::SharedBackend;
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

/// Configuration for the TDS wire protocol frontend.
#[derive(Clone, Debug)]
pub struct TdsFrontendConfig {
    /// Address to listen on (e.g., `127.0.0.1:1433`).
    pub listen: SocketAddr,
}

/// TDS wire protocol frontend.
pub struct TdsFrontend {
    config: TdsFrontendConfig,
    backend: SharedBackend,
}

impl TdsFrontend {
    /// Create a new TDS frontend.
    #[must_use]
    pub fn new(config: TdsFrontendConfig, backend: SharedBackend) -> Self {
        Self { config, backend }
    }

    /// Start accepting TDS client connections.
    ///
    /// Runs until the tokio runtime shuts down.
    ///
    /// # Errors
    ///
    /// Returns an error if binding the listen address fails.
    pub async fn serve(self) -> Result<(), std::io::Error> {
        let listener = TcpListener::bind(self.config.listen).await?;
        info!(listen = %self.config.listen, "TDS frontend listening");

        let backend = Arc::clone(&self.backend);

        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    warn!("TDS accept error: {e}");
                    continue;
                }
            };
            debug!(%peer, "TDS client connected");

            let be = Arc::clone(&backend);
            tokio::spawn(async move {
                if let Err(e) = handler::handle_connection(stream, be).await {
                    debug!(%peer, "TDS session ended: {e}");
                }
            });
        }
    }
}
