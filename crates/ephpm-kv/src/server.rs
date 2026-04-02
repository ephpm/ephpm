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
    /// Optional password for RESP AUTH. When set, clients must authenticate
    /// before any commands are accepted.
    pub password: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:6379".into(),
            max_input_buffer: 64 * 1024 * 1024, // 64 MiB
            password: None,
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
    let password = config.password.clone();
    let accept = serve_on(Arc::clone(&store), listener, max_buf, password);

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
    password: Option<String>,
) -> anyhow::Result<()> {
    let password: Option<Arc<str>> = password.map(|p| Arc::from(p.as_str()));

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
                let conn_password = password.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        handle_connection(stream, &conn_store, max_input_buffer, conn_password)
                            .await
                    {
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

/// Extract the command name from a parsed RESP frame.
///
/// Returns the uppercased command name if the frame is a valid command
/// (either an array with a bulk/simple first element, or a simple string).
fn extract_command_name(frame: &Frame) -> Option<String> {
    match frame {
        Frame::Array(a) if !a.is_empty() => match &a[0] {
            Frame::Bulk(b) => Some(String::from_utf8_lossy(b).to_ascii_uppercase()),
            Frame::Simple(s) => Some(s.to_ascii_uppercase()),
            _ => None,
        },
        Frame::Simple(s) => s.split_whitespace().next().map(|c| c.to_ascii_uppercase()),
        _ => None,
    }
}

/// Handle the AUTH command, validating the password if one is configured.
///
/// Returns the response frame and whether authentication succeeded.
fn handle_auth(frame: &Frame, required_password: &Option<Arc<str>>) -> (Frame, bool) {
    let password_arg = match frame {
        Frame::Array(a) if a.len() >= 2 => match &a[1] {
            Frame::Bulk(b) => Some(String::from_utf8_lossy(b).into_owned()),
            Frame::Simple(s) => Some(s.clone()),
            _ => None,
        },
        Frame::Simple(s) => s.split_whitespace().nth(1).map(String::from),
        _ => None,
    };

    match (required_password, password_arg) {
        // Password configured and client provided one — validate.
        (Some(expected), Some(ref provided)) if expected.as_ref() == provided.as_str() => {
            (Frame::ok(), true)
        }
        // Password configured but client provided wrong one.
        (Some(_), Some(_)) => (Frame::error("ERR invalid password"), false),
        // Password configured but client sent AUTH with no argument.
        (Some(_), None) => (
            Frame::error("ERR wrong number of arguments for 'auth' command"),
            false,
        ),
        // No password configured — AUTH is a no-op, always succeeds.
        (None, _) => (Frame::ok(), true),
    }
}

/// Handle a single RESP connection.
async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    store: &Arc<Store>,
    max_buf: usize,
    password: Option<Arc<str>>,
) -> anyhow::Result<()> {
    let mut buf = BytesMut::with_capacity(4096);
    let mut write_buf = BytesMut::with_capacity(4096);
    // If no password is configured, connections start authenticated.
    let mut authenticated = password.is_none();

    loop {
        // Try to parse frames already in the buffer before reading more.
        loop {
            match resp::parse_frame(&mut buf) {
                Ok(Some(frame)) => {
                    let cmd_name = extract_command_name(&frame);

                    // AUTH is always allowed, even before authentication.
                    if cmd_name.as_deref() == Some("AUTH") {
                        let (response, success) = handle_auth(&frame, &password);
                        if success {
                            authenticated = true;
                        }
                        write_buf.clear();
                        response.write_to(&mut write_buf);
                        stream.write_all(&write_buf).await?;
                        continue;
                    }

                    // QUIT is always allowed.
                    if cmd_name.as_deref() == Some("QUIT") {
                        let response = Frame::ok();
                        write_buf.clear();
                        response.write_to(&mut write_buf);
                        stream.write_all(&write_buf).await?;
                        return Ok(());
                    }

                    // Block all other commands until authenticated.
                    if !authenticated {
                        let response = Frame::error("NOAUTH Authentication required");
                        write_buf.clear();
                        response.write_to(&mut write_buf);
                        stream.write_all(&write_buf).await?;
                        continue;
                    }

                    let response = command::dispatch(store, &frame);
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
