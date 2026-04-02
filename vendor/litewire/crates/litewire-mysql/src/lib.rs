//! MySQL wire protocol frontend for litewire.
//!
//! Uses `opensrv-mysql` to accept MySQL client connections, translates
//! incoming SQL from MySQL dialect to SQLite, executes against the backend,
//! and returns results in MySQL wire format.

mod handler;
mod resultset;
mod types;

use std::net::SocketAddr;
use std::sync::Arc;

use litewire_backend::SharedBackend;
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

use handler::LiteWireHandler;

/// Configuration for the MySQL wire protocol frontend.
#[derive(Clone, Debug)]
pub struct MysqlFrontendConfig {
    /// Address to listen on (e.g., `127.0.0.1:3306`).
    pub listen: SocketAddr,
}

/// MySQL wire protocol frontend.
pub struct MysqlFrontend {
    config: MysqlFrontendConfig,
    backend: SharedBackend,
}

impl MysqlFrontend {
    /// Create a new MySQL frontend.
    #[must_use]
    pub fn new(config: MysqlFrontendConfig, backend: SharedBackend) -> Self {
        Self { config, backend }
    }

    /// Start accepting MySQL client connections.
    ///
    /// Runs until the tokio runtime shuts down.
    ///
    /// # Errors
    ///
    /// Returns an error if binding the listen address fails.
    pub async fn serve(self) -> Result<(), std::io::Error> {
        let listener = TcpListener::bind(self.config.listen).await?;
        info!(listen = %self.config.listen, "MySQL frontend listening");

        let backend = Arc::clone(&self.backend);

        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    warn!("MySQL accept error: {e}");
                    continue;
                }
            };
            debug!(%peer, "MySQL client connected");

            let be = Arc::clone(&backend);
            tokio::spawn(async move {
                let handler = LiteWireHandler::new(be);
                let (reader, writer) = stream.into_split();
                if let Err(e) =
                    opensrv_mysql::AsyncMysqlIntermediary::run_on(handler, reader, writer).await
                {
                    debug!(%peer, "MySQL session ended: {e}");
                }
            });
        }
    }
}
