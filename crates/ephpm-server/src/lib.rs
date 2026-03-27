pub mod router;
pub mod static_files;
pub mod tls;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use ephpm_config::Config;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::{Response, StatusCode};
use hyper_util::rt::TokioIo;
use router::Router;
use tokio::net::{TcpListener, TcpStream};
use tokio::signal;
use tokio_rustls::TlsAcceptor;

/// Start the HTTP server with the given configuration.
///
/// Listens on the configured address and routes requests to either
/// PHP execution or static file serving based on the request path.
///
/// When `[server.tls]` is configured, the server terminates TLS using
/// the provided certificate and key. If `tls.listen` is set, a separate
/// HTTPS listener is created and the main listener serves HTTP (with
/// optional redirect).
///
/// # Errors
///
/// Returns an error if the listen address is invalid or binding fails.
pub async fn serve(config: Config) -> anyhow::Result<()> {
    let listeners = bind_listeners(&config).await?;
    accept_loop(listeners, &config).await
}

/// Resolved listener state after binding.
struct Listeners {
    main: TcpListener,
    tls_listener: Option<TcpListener>,
    tls_acceptor: Option<TlsAcceptor>,
    redirect_http: bool,
    conn: ConnSettings,
    idle_timeout: Duration,
    router: Arc<Router>,
}

/// Connection-level settings passed into spawned tasks.
#[derive(Clone, Copy)]
struct ConnSettings {
    header_read_timeout: Duration,
    max_header_size: usize,
}

/// Parse config, build TLS, and bind all listeners.
async fn bind_listeners(config: &Config) -> anyhow::Result<Listeners> {
    let addr: SocketAddr = config.server.listen.parse().context("invalid listen address")?;

    let conn = ConnSettings {
        header_read_timeout: Duration::from_secs(config.server.timeouts.header_read),
        max_header_size: config.server.request.max_header_size,
    };
    let idle_timeout = Duration::from_secs(config.server.timeouts.idle);
    let router = Arc::new(Router::new(config));

    // Build TLS acceptor if configured.
    let tls_acceptor = config
        .server
        .tls
        .as_ref()
        .map(|tls_config| {
            tracing::info!(
                cert = %tls_config.cert.display(),
                key = %tls_config.key.display(),
                "TLS enabled"
            );
            tls::build_tls_acceptor(&tls_config.cert, &tls_config.key)
        })
        .transpose()?;

    // Determine if we need a separate TLS listener.
    let tls_listen_addr: Option<SocketAddr> = config
        .server
        .tls
        .as_ref()
        .and_then(|t| t.listen.as_ref())
        .map(|s| s.parse().context("invalid TLS listen address"))
        .transpose()?;

    let redirect_http = config
        .server
        .tls
        .as_ref()
        .is_some_and(|t| t.redirect_http && t.listen.is_some());

    if config
        .server
        .tls
        .as_ref()
        .is_some_and(|t| t.redirect_http && t.listen.is_none())
    {
        tracing::warn!(
            "tls.redirect_http is set but tls.listen is not — \
             redirect has no effect without a separate HTTP listener"
        );
    }

    let main = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind to {addr}"))?;

    let tls_listener = match tls_listen_addr {
        Some(tls_addr) => {
            if tls_addr == addr {
                anyhow::bail!(
                    "server.listen ({addr}) and server.tls.listen ({tls_addr}) \
                     must be different addresses"
                );
            }
            let listener = TcpListener::bind(tls_addr)
                .await
                .with_context(|| format!("failed to bind TLS to {tls_addr}"))?;
            tracing::info!(%tls_addr, "HTTPS listening");
            Some(listener)
        }
        None => None,
    };

    if tls_acceptor.is_some() && tls_listener.is_none() {
        tracing::info!(%addr, "HTTPS listening");
    } else if redirect_http {
        tracing::info!(%addr, "HTTP listening (redirecting to HTTPS)");
    } else {
        tracing::info!(%addr, "HTTP listening");
    }

    Ok(Listeners {
        main,
        tls_listener,
        tls_acceptor,
        redirect_http,
        conn,
        idle_timeout,
        router,
    })
}

/// Run the accept loop, dispatching connections to the appropriate handler.
async fn accept_loop(listeners: Listeners, _config: &Config) -> anyhow::Result<()> {
    let Listeners {
        main,
        tls_listener,
        tls_acceptor,
        redirect_http,
        conn,
        idle_timeout,
        router,
    } = listeners;

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            result = main.accept() => {
                let (stream, remote_addr) = result.context("failed to accept connection")?;

                if tls_listener.is_some() && redirect_http {
                    tokio::spawn(serve_http_redirect(stream, remote_addr, conn));
                } else if let Some(ref acceptor) = tls_acceptor {
                    let acceptor = acceptor.clone();
                    let router = Arc::clone(&router);
                    tokio::spawn(async move {
                        serve_tls_connection(stream, acceptor, router, remote_addr, conn).await;
                    });
                } else {
                    let router = Arc::clone(&router);
                    tokio::spawn(async move {
                        serve_connection(
                            TokioIo::new(stream), router, remote_addr, false, conn,
                        ).await;
                    });
                }
            }

            result = async {
                // SAFETY: guarded by `if tls_listener.is_some()` below.
                tls_listener.as_ref().expect("guarded by is_some").accept().await
            }, if tls_listener.is_some() => {
                let (stream, remote_addr) = result.context("failed to accept TLS connection")?;
                // tls_listener is only set when tls_acceptor is set.
                let acceptor = tls_acceptor.clone().expect("tls_listener requires tls_acceptor");
                let router = Arc::clone(&router);
                tokio::spawn(async move {
                    serve_tls_connection(stream, acceptor, router, remote_addr, conn).await;
                });
            }

            () = &mut shutdown => {
                tracing::info!("shutdown signal received, stopping server");
                break;
            }
        }
    }

    tracing::info!(
        timeout_secs = idle_timeout.as_secs(),
        "waiting for connections to drain"
    );
    tokio::time::sleep(Duration::from_secs(1)).await;

    Ok(())
}

/// Perform a TLS handshake and then serve the connection.
async fn serve_tls_connection(
    stream: TcpStream,
    acceptor: TlsAcceptor,
    router: Arc<Router>,
    remote_addr: SocketAddr,
    settings: ConnSettings,
) {
    let tls_stream =
        match tokio::time::timeout(settings.header_read_timeout, acceptor.accept(stream)).await {
            Ok(Ok(s)) => s,
            Ok(Err(err)) => {
                tracing::debug!(%remote_addr, %err, "TLS handshake failed");
                return;
            }
            Err(_) => {
                tracing::debug!(%remote_addr, "TLS handshake timed out");
                return;
            }
        };

    serve_connection(TokioIo::new(tls_stream), router, remote_addr, true, settings).await;
}

/// Serve an HTTP connection over any transport (`TcpStream` or `TlsStream`).
async fn serve_connection<I>(
    io: TokioIo<I>,
    router: Arc<Router>,
    remote_addr: SocketAddr,
    is_tls: bool,
    settings: ConnSettings,
) where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let service = hyper::service::service_fn(move |req| {
        let router = Arc::clone(&router);
        async move { router.handle(req, remote_addr, is_tls).await }
    });

    if let Err(err) = http1::Builder::new()
        .keep_alive(true)
        .header_read_timeout(settings.header_read_timeout)
        .max_buf_size(settings.max_header_size)
        .timer(hyper_util::rt::TokioTimer::new())
        .serve_connection(io, service)
        .with_upgrades()
        .await
    {
        if !err.is_incomplete_message() {
            tracing::debug!(%remote_addr, %err, "connection error");
        }
    }
}

/// Serve a plain HTTP connection that redirects all requests to HTTPS.
async fn serve_http_redirect(
    stream: TcpStream,
    remote_addr: SocketAddr,
    settings: ConnSettings,
) {
    let io = TokioIo::new(stream);
    let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let host = req
            .headers()
            .get("host")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("localhost")
            .to_owned();
        let path_and_query = req
            .uri()
            .path_and_query()
            .map_or("/", http::uri::PathAndQuery::as_str)
            .to_owned();

        async move {
            let location = format!("https://{host}{path_and_query}");
            Ok::<_, hyper::Error>(
                Response::builder()
                    .status(StatusCode::MOVED_PERMANENTLY)
                    .header("location", location)
                    .body(Full::new(Bytes::from("Redirecting to HTTPS\n")))
                    .expect("valid redirect response"),
            )
        }
    });

    if let Err(err) = http1::Builder::new()
        .keep_alive(false)
        .header_read_timeout(settings.header_read_timeout)
        .max_buf_size(settings.max_header_size)
        .timer(hyper_util::rt::TokioTimer::new())
        .serve_connection(io, service)
        .await
    {
        if !err.is_incomplete_message() {
            tracing::debug!(%remote_addr, %err, "redirect connection error");
        }
    }
}

/// Wait for a shutdown signal (Ctrl+C).
async fn shutdown_signal() {
    signal::ctrl_c().await.expect("failed to install ctrl+c handler");
}
