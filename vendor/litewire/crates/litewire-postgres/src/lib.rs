//! PostgreSQL wire protocol frontend for litewire.
//!
//! Uses `pgwire` to accept PostgreSQL client connections, translates
//! incoming SQL from PostgreSQL dialect to SQLite, executes against the
//! backend, and returns results in PostgreSQL wire format.

mod handler;
mod types;

use std::net::SocketAddr;
use std::sync::Arc;

use litewire_backend::SharedBackend;
use pgwire::api::auth::noop::NoopStartupHandler;
use pgwire::api::copy::NoopCopyHandler;
use pgwire::api::PgWireServerHandlers;
use pgwire::api::NoopErrorHandler;
use pgwire::tokio::process_socket;
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

use handler::PostgresHandler;

/// Configuration for the PostgreSQL wire protocol frontend.
#[derive(Clone, Debug)]
pub struct PostgresFrontendConfig {
    /// Address to listen on (e.g., `127.0.0.1:5432`).
    pub listen: SocketAddr,
}

/// PostgreSQL wire protocol frontend.
pub struct PostgresFrontend {
    config: PostgresFrontendConfig,
    backend: SharedBackend,
}

impl PostgresFrontend {
    /// Create a new PostgreSQL frontend.
    #[must_use]
    pub fn new(config: PostgresFrontendConfig, backend: SharedBackend) -> Self {
        Self { config, backend }
    }

    /// Start accepting PostgreSQL client connections.
    ///
    /// Runs until the tokio runtime shuts down.
    ///
    /// # Errors
    ///
    /// Returns an error if binding the listen address fails.
    pub async fn serve(self) -> Result<(), std::io::Error> {
        let listener = TcpListener::bind(self.config.listen).await?;
        info!(listen = %self.config.listen, "PostgreSQL frontend listening");

        let factory = Arc::new(LiteWireHandlerFactory {
            handler: Arc::new(PostgresHandler::new(Arc::clone(&self.backend))),
        });

        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    warn!("PostgreSQL accept error: {e}");
                    continue;
                }
            };
            debug!(%peer, "PostgreSQL client connected");

            let factory = Arc::clone(&factory);
            tokio::spawn(async move {
                if let Err(e) = process_socket(stream, None, factory).await {
                    debug!(%peer, "PostgreSQL session ended: {e}");
                }
            });
        }
    }
}

/// No-op startup handler that accepts all connections without authentication.
struct LiteWireStartupHandler;

impl NoopStartupHandler for LiteWireStartupHandler {}

/// Factory that provides handler instances to pgwire's socket processor.
struct LiteWireHandlerFactory {
    handler: Arc<PostgresHandler>,
}

impl PgWireServerHandlers for LiteWireHandlerFactory {
    type StartupHandler = LiteWireStartupHandler;
    type SimpleQueryHandler = PostgresHandler;
    type ExtendedQueryHandler = PostgresHandler;
    type CopyHandler = NoopCopyHandler;
    type ErrorHandler = NoopErrorHandler;

    fn simple_query_handler(&self) -> Arc<Self::SimpleQueryHandler> {
        self.handler.clone()
    }

    fn extended_query_handler(&self) -> Arc<Self::ExtendedQueryHandler> {
        self.handler.clone()
    }

    fn startup_handler(&self) -> Arc<Self::StartupHandler> {
        Arc::new(LiteWireStartupHandler)
    }

    fn copy_handler(&self) -> Arc<Self::CopyHandler> {
        Arc::new(NoopCopyHandler)
    }

    fn error_handler(&self) -> Arc<Self::ErrorHandler> {
        Arc::new(NoopErrorHandler)
    }
}
