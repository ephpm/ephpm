//! TCP server accepting RESP protocol connections.
//!
//! Listens on a configurable address (and optionally a Unix socket) and
//! spawns a task per connection. Each connection reads RESP frames,
//! dispatches them to the [`Store`] via [`command::dispatch`], and writes
//! the response back.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::signal;
use tracing::{debug, error, info, trace};

use crate::command;
use crate::resp::{self, Frame};
use crate::store::Store;

/// Configuration for the KV TCP server.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// TCP listen address, e.g. `"127.0.0.1:6379"`.
    pub listen: String,
    /// Maximum input buffer size per connection (bytes). Protects against
    /// clients sending enormous payloads.
    pub max_input_buffer: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:6379".into(),
            max_input_buffer: 64 * 1024 * 1024, // 64 MiB
        }
    }
}

/// Run the KV store RESP server.
///
/// Binds a TCP listener, spawns a background expiry task, and accepts
/// connections until a shutdown signal (Ctrl-C) is received.
///
/// # Errors
///
/// Returns an error if the TCP listener fails to bind.
pub async fn run(store: Arc<Store>, config: ServerConfig) -> anyhow::Result<()> {
    let listener = TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("failed to bind KV server to {}", config.listen))?;

    info!(listen = %config.listen, "KV store RESP server listening");

    let max_buf = config.max_input_buffer;
    let accept = serve_on(Arc::clone(&store), listener, max_buf);

    tokio::select! {
        result = accept => result,
        () = shutdown_signal() => {
            info!("KV server shutting down");
            // Brief drain period for in-flight connections.
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(())
        }
    }
}

/// Accept connections on an already-bound `listener` until the task is
/// cancelled.
///
/// Unlike [`run`], this does not install a shutdown signal handler — the
/// caller controls server lifetime by aborting the spawned task. Intended
/// for use in tests and embedding contexts where the caller manages the
/// listener socket.
///
/// # Errors
///
/// Returns an error if an unrecoverable accept failure occurs.
pub async fn serve_on(
    store: Arc<Store>,
    listener: TcpListener,
    max_input_buffer: usize,
) -> anyhow::Result<()> {
    // Background expiry reaper — runs every second.
    let expiry_store = Arc::clone(&store);
    let _expiry_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            expiry_store.expire_pass(100);
        }
    });

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                debug!(peer = %addr, "new KV connection");
                let conn_store = Arc::clone(&store);
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, &conn_store, max_input_buffer).await {
                        debug!(peer = %addr, error = %e, "connection closed with error");
                    }
                    trace!(peer = %addr, "connection closed");
                });
            }
            Err(e) => {
                error!(error = %e, "failed to accept connection");
            }
        }
    }

    #[allow(unreachable_code)]
    Ok(())
}

/// Handle a single RESP connection.
async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    store: &Arc<Store>,
    max_buf: usize,
) -> anyhow::Result<()> {
    let mut buf = BytesMut::with_capacity(4096);
    let mut write_buf = BytesMut::with_capacity(4096);

    loop {
        // Try to parse frames already in the buffer before reading more.
        loop {
            match resp::parse_frame(&mut buf) {
                Ok(Some(frame)) => {
                    let response = command::dispatch(store, &frame);

                    // Check for QUIT.
                    if matches!(&frame, Frame::Array(a) if !a.is_empty() && matches!(&a[0], Frame::Bulk(b) if b.eq_ignore_ascii_case(b"QUIT")))
                    {
                        write_buf.clear();
                        response.write_to(&mut write_buf);
                        stream.write_all(&write_buf).await?;
                        return Ok(());
                    }

                    write_buf.clear();
                    response.write_to(&mut write_buf);
                    stream.write_all(&write_buf).await?;
                }
                Ok(None) => break, // need more data
                Err(e) => {
                    let err_frame = Frame::error(format!("ERR {e}"));
                    write_buf.clear();
                    err_frame.write_to(&mut write_buf);
                    stream.write_all(&write_buf).await?;
                    // Clear the buffer on protocol error — we can't recover framing.
                    buf.clear();
                    break;
                }
            }
        }

        // Read more data from the socket.
        if buf.len() >= max_buf {
            let err_frame = Frame::error("ERR input buffer overflow");
            write_buf.clear();
            err_frame.write_to(&mut write_buf);
            stream.write_all(&write_buf).await?;
            return Ok(());
        }

        let n = stream.read_buf(&mut buf).await?;
        if n == 0 {
            // Connection closed by client.
            return Ok(());
        }
    }
}

/// Wait for Ctrl-C / SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    {
        let mut sigterm =
            signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler");
        tokio::select! {
            () = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    ctrl_c.await;
}
