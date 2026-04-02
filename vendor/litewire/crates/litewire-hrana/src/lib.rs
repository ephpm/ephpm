//! Hrana HTTP frontend for litewire (sqld-compatible).
//!
//! Implements the Hrana 3 HTTP pipeline protocol (`POST /v2/pipeline`)
//! so that apps using the libsql client SDK can connect to litewire
//! as a lightweight drop-in replacement for sqld.

mod http;
mod types;

use std::net::SocketAddr;
use litewire_backend::SharedBackend;
use tracing::info;

/// Configuration for the Hrana HTTP frontend.
#[derive(Clone, Debug)]
pub struct HranaFrontendConfig {
    /// Address to listen on (e.g., `127.0.0.1:8080`).
    pub listen: SocketAddr,
}

/// Hrana HTTP frontend (sqld-compatible).
pub struct HranaFrontend {
    config: HranaFrontendConfig,
    backend: SharedBackend,
}

impl HranaFrontend {
    /// Create a new Hrana frontend.
    #[must_use]
    pub fn new(config: HranaFrontendConfig, backend: SharedBackend) -> Self {
        Self { config, backend }
    }

    /// Start serving Hrana HTTP requests.
    ///
    /// # Errors
    ///
    /// Returns an error if binding fails or the server encounters a fatal error.
    pub async fn serve(self) -> Result<(), std::io::Error> {
        let app = http::build_router(self.backend);
        let listener = tokio::net::TcpListener::bind(self.config.listen).await?;
        info!(listen = %self.config.listen, "Hrana HTTP frontend listening");
        axum::serve(listener, app).await
    }
}
