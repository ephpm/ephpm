//! TCP server accepting RESP protocol connections.
//!
//! Listens on a configurable TCP address and spawns a task per
//! connection. Each connection reads RESP frames, dispatches them to the
//! [`Store`] via [`command::dispatch`], and writes the response back.
//!
//! Connections are bounded three ways (all configurable): a global
//! connection cap (excess clients get `ERR max number of clients
//! reached`, like Redis), a per-connection input-buffer cap, and an idle
//! read timeout. Without these the lane's memory is proportional to
//! `in-flight frame bytes x connections`, entirely outside the store's
//! `memory_limit` accounting.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::signal;
use tracing::{debug, error, info, trace};

use crate::multi_tenant::MultiTenantStore;
use crate::resp::{self, Frame};
use crate::store::Store;
use crate::{auth, command};

/// Configuration for the KV TCP server.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// TCP listen address, e.g. `"127.0.0.1:6379"`.
    pub listen: String,
    /// Maximum input buffer size per connection (bytes). Protects against
    /// clients sending enormous payloads. Note this is per connection and
    /// is NOT counted against the store's `memory_limit`.
    pub max_input_buffer: usize,
    /// Maximum concurrent connections. Excess clients are refused with
    /// `ERR max number of clients reached`. `0` = unlimited.
    pub max_connections: usize,
    /// Idle read timeout in seconds. Connections that send nothing for
    /// this long are closed, freeing their buffers. `0` = no timeout.
    pub idle_timeout_secs: u64,
    /// Optional password for RESP AUTH. When set, clients must authenticate
    /// before any commands are accepted.
    pub password: Option<String>,
    /// Master secret for HMAC-derived per-site authentication. When set
    /// alongside a `MultiTenantStore`, clients must `AUTH <hostname>
    /// <derived_password>` and their connection is scoped to that site's
    /// store.
    pub secret: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:6379".into(),
            // 1 MiB, matching Redis' client-query-buffer-limit default.
            // The previous 64 MiB default let a handful of connections
            // with large in-flight frames OOM a memory-capped container.
            max_input_buffer: 1024 * 1024,
            max_connections: 1000,
            idle_timeout_secs: 300,
            password: None,
            secret: None,
        }
    }
}

/// Run the KV store RESP server.
///
/// Binds a TCP listener, spawns a background expiry task, and accepts
/// connections until a shutdown signal (Ctrl-C) is received.
///
/// When `multi_tenant` is provided alongside a `secret` in the config,
/// HMAC-derived per-site AUTH is required. Clients must send
/// `AUTH <hostname> <derived_password>` to scope their connection to
/// a specific site's store.
///
/// # Errors
///
/// Returns an error if the TCP listener fails to bind.
pub async fn run(
    store: Arc<Store>,
    config: ServerConfig,
    multi_tenant: Option<MultiTenantStore>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("failed to bind KV server to {}", config.listen))?;

    info!(listen = %config.listen, "KV store RESP server listening");

    let accept = serve_on(Arc::clone(&store), listener, config, multi_tenant);

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
/// When `secret` is `Some` and `multi_tenant` is `Some`, the server
/// requires HMAC-derived per-site authentication. The `password` field
/// is ignored in this mode.
///
/// # Errors
///
/// Returns an error if an unrecoverable accept failure occurs.
pub async fn serve_on(
    store: Arc<Store>,
    listener: TcpListener,
    config: ServerConfig,
    multi_tenant: Option<MultiTenantStore>,
) -> anyhow::Result<()> {
    let max_input_buffer = config.max_input_buffer;
    let idle_timeout_secs = config.idle_timeout_secs;
    let password: Option<Arc<str>> = config.password.map(|p| Arc::from(p.as_str()));
    let secret: Option<Arc<str>> = config.secret.map(|s| Arc::from(s.as_str()));
    let multi_tenant = multi_tenant.map(Arc::new);

    // Connection cap: excess clients get a Redis-style refusal instead of
    // an unbounded task + buffers each. `0` disables the cap.
    let conn_permits = (config.max_connections > 0)
        .then(|| Arc::new(tokio::sync::Semaphore::new(config.max_connections)));

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
            Ok((mut stream, addr)) => {
                let permit = match &conn_permits {
                    Some(sem) => match Arc::clone(sem).try_acquire_owned() {
                        Ok(permit) => Some(permit),
                        Err(_) => {
                            debug!(peer = %addr, "refusing KV connection: max clients reached");
                            // Bound the refusal write: a non-reading peer would
                            // otherwise pin a spawned refusal task indefinitely,
                            // and a flood of such peers would exhaust task
                            // memory even though the connection cap did its
                            // job on the accept path.
                            tokio::spawn(async move {
                                let _ = tokio::time::timeout(
                                    Duration::from_secs(5),
                                    stream.write_all(b"-ERR max number of clients reached\r\n"),
                                )
                                .await;
                            });
                            continue;
                        }
                    },
                    None => None,
                };
                debug!(peer = %addr, "new KV connection");
                let conn_store = Arc::clone(&store);
                let conn_password = password.clone();
                let conn_secret = secret.clone();
                let conn_mt = multi_tenant.clone();
                tokio::spawn(async move {
                    // Hold the connection permit for the task's lifetime.
                    let _permit = permit;
                    if let Err(e) = handle_connection(
                        stream,
                        &conn_store,
                        max_input_buffer,
                        idle_timeout_secs,
                        conn_password,
                        conn_secret,
                        conn_mt,
                    )
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
        Frame::Simple(s) => s.split_whitespace().next().map(str::to_ascii_uppercase),
        _ => None,
    }
}

/// Extract the first and optional second argument from an AUTH frame.
///
/// Redis supports both `AUTH <password>` and `AUTH <username> <password>`.
/// Returns `(first_arg, second_arg)`.
fn extract_auth_args(frame: &Frame) -> (Option<String>, Option<String>) {
    match frame {
        Frame::Array(a) => {
            let first = a.get(1).and_then(|f| match f {
                Frame::Bulk(b) => Some(String::from_utf8_lossy(b).into_owned()),
                Frame::Simple(s) => Some(s.clone()),
                _ => None,
            });
            let second = a.get(2).and_then(|f| match f {
                Frame::Bulk(b) => Some(String::from_utf8_lossy(b).into_owned()),
                Frame::Simple(s) => Some(s.clone()),
                _ => None,
            });
            (first, second)
        }
        Frame::Simple(s) => {
            let mut parts = s.split_whitespace().skip(1);
            let first = parts.next().map(String::from);
            let second = parts.next().map(String::from);
            (first, second)
        }
        _ => (None, None),
    }
}

/// Result of a successful HMAC-derived AUTH.
struct HmacAuthResult {
    /// The site store to use for this connection.
    store: Arc<Store>,
}

/// Handle the AUTH command, validating the password if one is configured.
///
/// Returns the response frame, whether authentication succeeded, and
/// optionally a site store override when HMAC multi-tenant auth is used.
fn handle_auth(
    frame: &Frame,
    required_password: Option<&Arc<str>>,
    secret: Option<&Arc<str>>,
    multi_tenant: Option<&Arc<MultiTenantStore>>,
) -> (Frame, bool, Option<HmacAuthResult>) {
    let (first_arg, second_arg) = extract_auth_args(frame);

    // HMAC multi-tenant mode: AUTH <hostname> <derived_password>
    if let (Some(secret_val), Some(mt)) = (secret, multi_tenant) {
        match (first_arg, second_arg) {
            (Some(hostname), Some(password)) => {
                if auth::validate_site_password(secret_val, &hostname, &password) {
                    let store = mt.get_site_store(&hostname);
                    (Frame::ok(), true, Some(HmacAuthResult { store }))
                } else {
                    (Frame::error("ERR invalid password"), false, None)
                }
            }
            // Missing hostname or password argument.
            (Some(_), None) | (None, _) => {
                (Frame::error("ERR wrong number of arguments for 'auth' command"), false, None)
            }
        }
    } else {
        // Legacy single-password mode.
        let password_arg = first_arg;
        match (required_password, password_arg) {
            // Password configured and client provided one — validate.
            (Some(expected), Some(ref provided)) if expected.as_ref() == provided.as_str() => {
                (Frame::ok(), true, None)
            }
            // Password configured but client provided wrong one.
            (Some(_), Some(_)) => (Frame::error("ERR invalid password"), false, None),
            // Password configured but client sent AUTH with no argument.
            (Some(_), None) => {
                (Frame::error("ERR wrong number of arguments for 'auth' command"), false, None)
            }
            // No password configured — AUTH is a no-op, always succeeds.
            (None, _) => (Frame::ok(), true, None),
        }
    }
}

/// Handle a single RESP connection.
#[allow(clippy::too_many_lines)]
async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    store: &Arc<Store>,
    max_buf: usize,
    idle_timeout_secs: u64,
    password: Option<Arc<str>>,
    secret: Option<Arc<str>>,
    multi_tenant: Option<Arc<MultiTenantStore>>,
) -> anyhow::Result<()> {
    let mut buf = BytesMut::with_capacity(4096);
    let mut write_buf = BytesMut::with_capacity(4096);

    // Determine whether auth is required:
    // - HMAC multi-tenant mode: secret + multi_tenant both set
    // - Legacy password mode: password is set
    // - No auth: neither secret nor password set
    let auth_required = secret.is_some() || password.is_some();
    let mut authenticated = !auth_required;

    // When HMAC auth succeeds, the connection is scoped to a specific site store.
    let mut active_store: Arc<Store> = Arc::clone(store);

    loop {
        // Try to parse frames already in the buffer before reading more.
        loop {
            match resp::parse_frame(&mut buf) {
                Ok(Some(frame)) => {
                    let cmd_name = extract_command_name(&frame);

                    // AUTH is always allowed, even before authentication.
                    if cmd_name.as_deref() == Some("AUTH") {
                        let (response, success, hmac_result) = handle_auth(
                            &frame,
                            password.as_ref(),
                            secret.as_ref(),
                            multi_tenant.as_ref(),
                        );
                        if success {
                            authenticated = true;
                            if let Some(result) = hmac_result {
                                active_store = result.store;
                            }
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

                    let response = command::dispatch(&active_store, &frame);
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

        // Idle timeout: a silent peer must not pin this task (and its
        // buffers) forever — half-open sockets otherwise linger until TCP
        // keepalive, hours later.
        let read = stream.read_buf(&mut buf);
        let n = if idle_timeout_secs > 0 {
            match tokio::time::timeout(Duration::from_secs(idle_timeout_secs), read).await {
                Ok(result) => result?,
                Err(_elapsed) => {
                    debug!("closing idle KV connection");
                    return Ok(());
                }
            }
        } else {
            read.await?
        };
        if n == 0 {
            // Connection closed by client.
            return Ok(());
        }
    }
}

/// Wait for Ctrl-C / SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    {
        let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        tokio::select! {
            () = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    ctrl_c.await;
}
