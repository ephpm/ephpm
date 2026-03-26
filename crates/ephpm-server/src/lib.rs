pub mod router;
pub mod static_files;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use ephpm_config::Config;
use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use router::Router;
use tokio::net::TcpListener;
use tokio::signal;

/// Start the HTTP server with the given configuration.
///
/// Listens on the configured address and routes requests to either
/// PHP execution or static file serving based on the request path.
///
/// # Errors
///
/// Returns an error if the listen address is invalid or binding fails.
pub async fn serve(config: Config) -> anyhow::Result<()> {
    let addr: SocketAddr = config.server.listen.parse().context("invalid listen address")?;

    let header_read_timeout = Duration::from_secs(config.server.timeouts.header_read);
    let idle_timeout = Duration::from_secs(config.server.timeouts.idle);
    let max_header_size = config.server.request.max_header_size;

    let router = Arc::new(Router::new(&config));
    let listener =
        TcpListener::bind(addr).await.with_context(|| format!("failed to bind to {addr}"))?;

    tracing::info!(%addr, "server listening");

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, remote_addr) = result.context("failed to accept connection")?;
                let router = Arc::clone(&router);

                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = hyper::service::service_fn(move |req| {
                        let router = Arc::clone(&router);
                        async move { router.handle(req, remote_addr).await }
                    });

                    if let Err(err) = http1::Builder::new()
                        .keep_alive(true)
                        .header_read_timeout(header_read_timeout)
                        .max_buf_size(max_header_size)
                        .timer(hyper_util::rt::TokioTimer::new())
                        .serve_connection(io, service)
                        .with_upgrades()
                        .await
                    {
                        // Filter out normal connection closures and timeouts
                        if !err.is_incomplete_message() {
                            tracing::debug!(%remote_addr, %err, "connection error");
                        }
                    }
                });
            }
            () = &mut shutdown => {
                tracing::info!("shutdown signal received, stopping server");
                break;
            }
        }
    }

    // Allow idle_timeout for in-flight connections to finish
    tracing::info!(
        timeout_secs = idle_timeout.as_secs(),
        "waiting for connections to drain"
    );
    tokio::time::sleep(Duration::from_secs(1)).await;

    Ok(())
}

/// Wait for a shutdown signal (Ctrl+C).
async fn shutdown_signal() {
    signal::ctrl_c().await.expect("failed to install ctrl+c handler");
}
