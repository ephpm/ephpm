pub mod acme;
pub mod body;
pub mod file_cache;
pub mod metrics;
pub mod rate_limit;
pub mod router;
pub mod static_files;
pub mod tls;
pub mod tracked_backend;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Context;
use ephpm_config::Config;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::{Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use router::Router;
use rustls::ServerConfig;
use rustls_acme::is_tls_alpn_challenge;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::signal;
use tokio_rustls::{LazyConfigAcceptor, TlsAcceptor};

/// Start the HTTP server with the given configuration.
///
/// Listens on the configured address and routes requests to either
/// PHP execution or static file serving based on the request path.
///
/// Also starts background services:
/// - KV store with optional RESP protocol server
/// - `MySQL` connection proxy (if configured)
/// - `PostgreSQL` connection proxy (if configured)
/// - `TDS` (SQL Server) connection proxy (if configured)
///
/// When `[server.tls]` is configured, the server terminates TLS using
/// either manual cert/key files or automatic ACME provisioning.
///
/// # Errors
///
/// Returns an error if the listen address is invalid or binding fails.
pub async fn serve(config: Config) -> anyhow::Result<()> {
    // Install Prometheus recorder if metrics are enabled.
    let metrics_handle = if config.server.metrics.enabled {
        Some(metrics::init().context("failed to initialize metrics")?)
    } else {
        None
    };

    // Start background services.
    let (kv_store, _kv_handle) = start_kv_service(&config)?;

    // Start cluster gossip before DB proxies — clustered SQLite needs the handle.
    let cluster_handle = if config.cluster.enabled {
        let handle = ephpm_cluster::start_gossip(&config.cluster)
            .await
            .context("failed to start cluster gossip")?;
        tracing::info!(
            node_id = %handle.self_node().id,
            cluster_id = %handle.cluster_id(),
            "cluster gossip started"
        );

        // Start the KV TCP data plane for large-value cross-node fetches.
        let data_port = config.cluster.kv.data_port;
        let data_plane_store = Arc::clone(&kv_store);
        tokio::spawn(async move {
            if let Err(e) = ephpm_cluster::data_plane::serve(data_plane_store, data_port).await {
                tracing::error!(%e, "KV data plane error");
            }
        });

        Some(Arc::new(handle))
    } else {
        None
    };

    // Create shared query stats collector.
    let query_stats = ephpm_query_stats::QueryStats::new(ephpm_query_stats::StatsConfig {
        enabled: config.db.analysis.query_stats,
        slow_query_threshold: parse_duration(&config.db.analysis.slow_query_threshold)
            .unwrap_or(Duration::from_secs(1)),
        max_digests: config.db.analysis.digest_store_max_entries,
    });

    let _db_handles = start_db_proxies(&config, cluster_handle.as_ref(), &query_stats).await?;

    let listeners = bind_listeners(&config, kv_store, metrics_handle).await?;
    accept_loop(listeners).await
}

/// Which TLS mode the server is operating in.
enum TlsMode {
    /// No TLS — plain HTTP only.
    None,
    /// Manual TLS with a static cert/key loaded at startup.
    Manual(TlsAcceptor),
    /// Automatic ACME certificate provisioning (Let's Encrypt).
    Acme { challenge_config: Arc<ServerConfig>, default_config: Arc<ServerConfig> },
}

/// Resolved listener state after binding.
struct Listeners {
    main: TcpListener,
    tls_listener: Option<TcpListener>,
    tls_mode: TlsMode,
    redirect_http: bool,
    conn: ConnSettings,
    idle_timeout: Duration,
    shutdown_timeout: Duration,
    router: Arc<Router>,
    limiter: Option<Arc<rate_limit::Limiter>>,
    file_cache: Option<Arc<file_cache::FileCache>>,
    /// Interval for file cache eviction sweeps (derived from `inactive_secs`).
    file_cache_eviction_interval: Duration,
}

/// Connection-level settings passed into spawned tasks.
#[derive(Clone, Copy)]
struct ConnSettings {
    header_read_timeout: Duration,
    max_header_size: usize,
}

/// Parse config, build TLS, and bind all listeners.
async fn bind_listeners(
    config: &Config,
    kv_store: Arc<ephpm_kv::store::Store>,
    metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
) -> anyhow::Result<Listeners> {
    let addr: SocketAddr = config.server.listen.parse().context("invalid listen address")?;

    let limiter = {
        let l = rate_limit::Limiter::new(config.server.limits.clone());
        if l.is_enabled() {
            tracing::info!("rate limiting enabled");
            Some(Arc::new(l))
        } else {
            None
        }
    };

    let conn = ConnSettings {
        header_read_timeout: Duration::from_secs(config.server.timeouts.header_read),
        max_header_size: config.server.request.max_header_size,
    };
    let idle_timeout = Duration::from_secs(config.server.timeouts.idle);
    let file_cache = if config.server.file_cache.enabled {
        tracing::info!(
            max_entries = config.server.file_cache.max_entries,
            "open file cache enabled"
        );
        Some(Arc::new(file_cache::FileCache::new(&config.server.file_cache)))
    } else {
        None
    };
    // Determine TLS mode (before router creation so we can share the kv_store).
    let tls_mode = match config.server.tls.as_ref() {
        Some(tls_config) if tls_config.is_manual() => {
            let cert = tls_config.cert.as_ref().expect("is_manual checks cert");
            let key = tls_config.key.as_ref().expect("is_manual checks key");
            tracing::info!(
                cert = %cert.display(),
                key = %key.display(),
                "TLS enabled (manual)"
            );
            let acceptor = tls::build_tls_acceptor(cert, key)?;
            TlsMode::Manual(acceptor)
        }
        Some(tls_config) if tls_config.is_acme() => {
            let acme_store =
                if config.cluster.enabled { Some(Arc::clone(&kv_store)) } else { None };
            let setup = acme::start_acme(tls_config, acme_store)?;
            TlsMode::Acme {
                challenge_config: setup.challenge_config,
                default_config: setup.default_config,
            }
        }
        Some(tls_config) if tls_config.cert.is_some() || tls_config.key.is_some() => {
            anyhow::bail!("TLS config must provide both cert and key, or neither (for ACME mode)");
        }
        _ => TlsMode::None,
    };

    let router = Arc::new(Router::new(
        config,
        kv_store,
        metrics_handle,
        limiter.clone(),
        file_cache.clone(),
    ));

    let has_tls = !matches!(tls_mode, TlsMode::None);

    // Determine if we need a separate TLS listener.
    let tls_listen_addr: Option<SocketAddr> = config
        .server
        .tls
        .as_ref()
        .and_then(|t| t.listen.as_ref())
        .map(|s| s.parse().context("invalid TLS listen address"))
        .transpose()?;

    let redirect_http = has_tls
        && config.server.tls.as_ref().is_some_and(|t| t.redirect_http && t.listen.is_some());

    if config.server.tls.as_ref().is_some_and(|t| t.redirect_http && t.listen.is_none()) {
        tracing::warn!(
            "tls.redirect_http is set but tls.listen is not — \
             redirect has no effect without a separate HTTP listener"
        );
    }

    let main =
        TcpListener::bind(addr).await.with_context(|| format!("failed to bind to {addr}"))?;

    let tls_listener = match tls_listen_addr {
        Some(tls_addr) if has_tls => {
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
        _ => None,
    };

    if has_tls && tls_listener.is_none() {
        tracing::info!(%addr, "HTTPS listening");
    } else if redirect_http {
        tracing::info!(%addr, "HTTP listening (redirecting to HTTPS)");
    } else {
        tracing::info!(%addr, "HTTP listening");
    }

    let shutdown_timeout = Duration::from_secs(config.server.timeouts.shutdown);

    // Eviction interval: half of inactive_secs, clamped to [1, 60].
    let inactive = config.server.file_cache.inactive_secs;
    let eviction_secs = (inactive / 2).max(1).min(60);
    let file_cache_eviction_interval = Duration::from_secs(eviction_secs);

    Ok(Listeners {
        main,
        tls_listener,
        tls_mode,
        redirect_http,
        conn,
        idle_timeout,
        shutdown_timeout,
        router,
        limiter,
        file_cache,
        file_cache_eviction_interval,
    })
}

/// Run the accept loop, dispatching connections to the appropriate handler.
async fn accept_loop(listeners: Listeners) -> anyhow::Result<()> {
    let Listeners {
        main,
        tls_listener,
        tls_mode,
        redirect_http,
        conn,
        idle_timeout: _idle_timeout,
        shutdown_timeout,
        router,
        limiter,
        file_cache,
        file_cache_eviction_interval,
    } = listeners;

    // Track in-flight connections for graceful shutdown.
    let in_flight = Arc::new(AtomicUsize::new(0));

    // Spawn background cleanup task for rate limiter state.
    if let Some(ref l) = limiter {
        let l = Arc::clone(l);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                l.cleanup_stale();
            }
        });
    }

    // Spawn background eviction task for file cache.
    if let Some(ref fc) = file_cache {
        let fc = Arc::clone(fc);
        let eviction_interval = file_cache_eviction_interval;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(eviction_interval);
            loop {
                interval.tick().await;
                fc.evict_inactive();
            }
        });
    }

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            result = main.accept() => {
                let (stream, remote_addr) = result.context("failed to accept connection")?;
                let guard = acquire_connection(&limiter, &stream, remote_addr).await;
                dispatch_main_connection(
                    stream, remote_addr, &tls_mode, tls_listener.is_some(),
                    redirect_http, conn, &router, guard, &in_flight,
                );
            }

            result = async {
                tls_listener.as_ref().expect("guarded by is_some").accept().await
            }, if tls_listener.is_some() => {
                let (stream, remote_addr) = result.context("failed to accept TLS connection")?;
                let guard = acquire_connection(&limiter, &stream, remote_addr).await;
                dispatch_tls_connection(stream, remote_addr, &tls_mode, conn, &router, guard, &in_flight);
            }

            () = &mut shutdown => {
                tracing::info!("shutdown signal received, stopping server");
                break;
            }
        }
    }

    // Graceful shutdown: wait for in-flight connections to drain.
    let active = in_flight.load(Ordering::Relaxed);
    if active > 0 {
        tracing::info!(
            active_connections = active,
            timeout_secs = shutdown_timeout.as_secs(),
            "waiting for in-flight connections to drain"
        );

        let deadline = tokio::time::Instant::now() + shutdown_timeout;
        loop {
            let remaining = in_flight.load(Ordering::Relaxed);
            if remaining == 0 {
                tracing::info!("all connections drained");
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    remaining_connections = remaining,
                    "shutdown timeout reached, force-closing remaining connections"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    Ok(())
}

/// Try to acquire a connection slot. On rejection, send a raw 503 and return `None`.
async fn acquire_connection(
    limiter: &Option<Arc<rate_limit::Limiter>>,
    stream: &TcpStream,
    remote_addr: SocketAddr,
) -> Option<rate_limit::ConnectionGuard> {
    let Some(l) = limiter else {
        return None;
    };
    match l.try_acquire_connection(remote_addr.ip()) {
        Some(guard) => Some(guard),
        None => {
            tracing::debug!(%remote_addr, "connection rejected (limit reached)");
            // Best-effort raw HTTP response — the TLS handshake hasn't happened yet
            // for TLS connections, so this only works for plain HTTP.
            let _ = stream.try_write(
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            None
        }
    }
}

/// RAII guard that decrements the in-flight connection counter on drop.
struct InFlightGuard(Arc<AtomicUsize>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Dispatch a connection from the main listener.
fn dispatch_main_connection(
    stream: TcpStream,
    remote_addr: SocketAddr,
    tls_mode: &TlsMode,
    has_tls_listener: bool,
    redirect_http: bool,
    conn: ConnSettings,
    router: &Arc<Router>,
    guard: Option<rate_limit::ConnectionGuard>,
    in_flight: &Arc<AtomicUsize>,
) {
    in_flight.fetch_add(1, Ordering::Relaxed);
    let flight_guard = InFlightGuard(Arc::clone(in_flight));

    if has_tls_listener && redirect_http {
        tokio::spawn(async move {
            let _flight = flight_guard;
            serve_http_redirect(stream, remote_addr, conn).await;
        });
        return;
    }

    match tls_mode {
        TlsMode::Manual(acceptor) => {
            let acceptor = acceptor.clone();
            let router = Arc::clone(router);
            tokio::spawn(async move {
                let _guard = guard; // held until connection closes
                let _flight = flight_guard;
                serve_manual_tls(stream, acceptor, router, remote_addr, conn).await;
            });
        }
        TlsMode::Acme { challenge_config, default_config } => {
            let challenge = Arc::clone(challenge_config);
            let default = Arc::clone(default_config);
            let router = Arc::clone(router);
            tokio::spawn(async move {
                let _guard = guard;
                let _flight = flight_guard;
                serve_acme_tls(stream, challenge, default, router, remote_addr, conn).await;
            });
        }
        TlsMode::None => {
            let router = Arc::clone(router);
            tokio::spawn(async move {
                let _guard = guard;
                let _flight = flight_guard;
                serve_connection(TokioIo::new(stream), router, remote_addr, false, conn).await;
            });
        }
    }
}

/// Dispatch a connection from the separate TLS listener.
fn dispatch_tls_connection(
    stream: TcpStream,
    remote_addr: SocketAddr,
    tls_mode: &TlsMode,
    conn: ConnSettings,
    router: &Arc<Router>,
    guard: Option<rate_limit::ConnectionGuard>,
    in_flight: &Arc<AtomicUsize>,
) {
    in_flight.fetch_add(1, Ordering::Relaxed);
    let flight_guard = InFlightGuard(Arc::clone(in_flight));

    match tls_mode {
        TlsMode::Manual(acceptor) => {
            let acceptor = acceptor.clone();
            let router = Arc::clone(router);
            tokio::spawn(async move {
                let _guard = guard;
                let _flight = flight_guard;
                serve_manual_tls(stream, acceptor, router, remote_addr, conn).await;
            });
        }
        TlsMode::Acme { challenge_config, default_config } => {
            let challenge = Arc::clone(challenge_config);
            let default = Arc::clone(default_config);
            let router = Arc::clone(router);
            tokio::spawn(async move {
                let _guard = guard;
                let _flight = flight_guard;
                serve_acme_tls(stream, challenge, default, router, remote_addr, conn).await;
            });
        }
        TlsMode::None => {
            unreachable!("tls_listener only exists when TLS is configured");
        }
    }
}

/// Perform a manual TLS handshake and then serve the connection.
async fn serve_manual_tls(
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

/// Handle an ACME-aware TLS connection using `LazyConfigAcceptor`.
///
/// Inspects the TLS `ClientHello` to distinguish ACME challenge connections
/// (TLS-ALPN-01) from normal HTTPS traffic. Challenge connections are handled
/// inline and closed; normal connections are passed through to hyper.
async fn serve_acme_tls(
    stream: TcpStream,
    challenge_config: Arc<ServerConfig>,
    default_config: Arc<ServerConfig>,
    router: Arc<Router>,
    remote_addr: SocketAddr,
    settings: ConnSettings,
) {
    let handshake = match tokio::time::timeout(
        settings.header_read_timeout,
        LazyConfigAcceptor::new(rustls::server::Acceptor::default(), stream),
    )
    .await
    {
        Ok(Ok(h)) => h,
        Ok(Err(err)) => {
            tracing::debug!(%remote_addr, %err, "TLS ClientHello failed");
            return;
        }
        Err(_) => {
            tracing::debug!(%remote_addr, "TLS ClientHello timed out");
            return;
        }
    };

    if is_tls_alpn_challenge(&handshake.client_hello()) {
        tracing::debug!(%remote_addr, "handling ACME TLS-ALPN-01 challenge");
        match handshake.into_stream(challenge_config).await {
            Ok(mut tls) => {
                let _ = tls.shutdown().await;
            }
            Err(err) => {
                tracing::debug!(%remote_addr, %err, "ACME challenge handshake failed");
            }
        }
        return;
    }

    match handshake.into_stream(default_config).await {
        Ok(tls_stream) => {
            serve_connection(TokioIo::new(tls_stream), router, remote_addr, true, settings).await;
        }
        Err(err) => {
            tracing::debug!(%remote_addr, %err, "TLS handshake failed");
        }
    }
}

/// Serve an HTTP connection over any transport (`TcpStream` or `TlsStream`).
///
/// Uses [`auto::Builder`] which negotiates HTTP/1.1 or HTTP/2 based on the
/// ALPN protocol agreed during the TLS handshake. Plain (non-TLS) connections
/// always use HTTP/1.1, since h2c (HTTP/2 cleartext) is not supported by
/// browsers.
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

    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .keep_alive(true)
        .header_read_timeout(settings.header_read_timeout)
        .max_buf_size(settings.max_header_size)
        .timer(hyper_util::rt::TokioTimer::new());

    if let Err(err) = builder.serve_connection_with_upgrades(io, service).await {
        // Downcast to hyper::Error to suppress noisy "connection closed before
        // message was completed" errors (clients disconnecting mid-request).
        let is_incomplete =
            err.downcast_ref::<hyper::Error>().is_some_and(hyper::Error::is_incomplete_message);
        if !is_incomplete {
            tracing::debug!(%remote_addr, %err, "connection error");
        }
    }
}

/// Serve a plain HTTP connection that redirects all requests to HTTPS.
async fn serve_http_redirect(stream: TcpStream, remote_addr: SocketAddr, settings: ConnSettings) {
    let io = TokioIo::new(stream);
    let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let host = req
            .headers()
            .get("host")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("localhost")
            .to_owned();
        let path_and_query =
            req.uri().path_and_query().map_or("/", http::uri::PathAndQuery::as_str).to_owned();

        async move {
            let location = format!("https://{host}{path_and_query}");
            Ok::<_, hyper::Error>(
                Response::builder()
                    .status(StatusCode::MOVED_PERMANENTLY)
                    .header("location", location)
                    .body(body::buffered(Full::new(Bytes::from("Redirecting to HTTPS\n"))))
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

/// Wait for a shutdown signal (Ctrl+C or SIGTERM).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = signal::ctrl_c() => {
                tracing::info!("received SIGINT (Ctrl+C), shutting down");
            }
            _ = sigterm.recv() => {
                tracing::info!("received SIGTERM, shutting down");
            }
        }
    }

    #[cfg(not(unix))]
    {
        signal::ctrl_c().await.expect("failed to install ctrl+c handler");
        tracing::info!("received Ctrl+C, shutting down");
    }
}

/// Start the KV store with optional RESP server.
fn start_kv_service(
    config: &Config,
) -> anyhow::Result<(Arc<ephpm_kv::store::Store>, Option<tokio::task::JoinHandle<()>>)> {
    // Create the KV store
    let store_config = ephpm_kv::store::StoreConfig {
        memory_limit: parse_memory_size(&config.kv.memory_limit)?,
        eviction_policy: ephpm_kv::store::EvictionPolicy::from_str_lossy(
            &config.kv.eviction_policy,
        ),
        compression: ephpm_kv::store::CompressionConfig {
            algo: ephpm_kv::store::CompressionAlgo::from_str_lossy(&config.kv.compression),
            level: config.kv.compression_level,
            min_size: config.kv.compression_min_size,
        },
    };
    let store = ephpm_kv::store::Store::new(store_config);

    // Wire the store into PHP native functions (ephpm_kv_get, etc.)
    ephpm_php::PhpRuntime::set_kv_store(&store);

    if !config.kv.redis_compat.enabled {
        tracing::debug!("KV store initialized (RESP server disabled)");
        return Ok((store, None));
    }

    // Start RESP server if enabled
    let listen = config.kv.redis_compat.listen.clone();
    let password = config.kv.redis_compat.password.clone();
    let secret = config.kv.secret.clone();
    let server_config = ephpm_kv::server::ServerConfig {
        listen,
        password,
        secret: secret.clone(),
        ..Default::default()
    };

    // Build multi-tenant store for HMAC auth if secret + sites_dir are both set.
    let multi_tenant = if secret.is_some() && config.server.sites_dir.is_some() {
        Some(ephpm_kv::multi_tenant::MultiTenantStore::new(
            Arc::clone(&store),
            ephpm_kv::store::StoreConfig::default(),
        ))
    } else {
        None
    };

    let store_for_resp = Arc::clone(&store);
    let handle = tokio::spawn(async move {
        match ephpm_kv::server::run(store_for_resp, server_config, multi_tenant).await {
            Ok(()) => tracing::info!("KV RESP server stopped"),
            Err(e) => tracing::error!("KV RESP server error: {e:#}"),
        }
    });

    Ok((store, Some(handle)))
}

/// Start database proxies (`MySQL`, `PostgreSQL`, `TDS`, embedded `SQLite`).
async fn start_db_proxies(
    config: &Config,
    cluster: Option<&Arc<ephpm_cluster::ClusterHandle>>,
    query_stats: &ephpm_query_stats::QueryStats,
) -> anyhow::Result<Vec<tokio::task::JoinHandle<()>>> {
    let mut handles = vec![];

    // MySQL proxy
    if let Some(mysql_config) = &config.db.mysql {
        let url = mysql_config.url.clone();
        let listen = mysql_config.listen.clone().unwrap_or_else(|| "127.0.0.1:3306".to_string());

        let pool_config = ephpm_db::pool::PoolConfig {
            min_connections: mysql_config.min_connections,
            max_connections: mysql_config.max_connections,
            idle_timeout: parse_duration(&mysql_config.idle_timeout)?,
            max_lifetime: parse_duration(&mysql_config.max_lifetime)?,
            pool_timeout: parse_duration(&mysql_config.pool_timeout)?,
            health_check_interval: parse_duration(&mysql_config.health_check_interval)?,
        };

        let reset_strategy = ephpm_db::ResetStrategy::from_str_lossy(&mysql_config.reset_strategy);

        let replica_urls =
            mysql_config.replicas.as_ref().map(|r| r.urls.clone()).unwrap_or_default();

        let rw_split = ephpm_db::mysql::RwSplitParams {
            enabled: config.db.read_write_split.enabled,
            sticky_duration: parse_duration(&config.db.read_write_split.sticky_duration)?,
        };

        match ephpm_db::mysql::build_proxy(
            &url,
            &listen,
            mysql_config.socket.clone(),
            pool_config,
            reset_strategy,
            replica_urls,
            rw_split,
        )
        .await
        {
            Ok(proxy) => {
                let maintenance_handle = proxy.start_maintenance();
                let proxy_handle = tokio::spawn(async move {
                    match proxy.run().await {
                        Ok(()) => tracing::info!("MySQL proxy stopped"),
                        Err(e) => tracing::error!("MySQL proxy error: {e:#}"),
                    }
                });
                handles.push(proxy_handle);
                // Drop the handle to detach the maintenance task — it runs independently.
                drop(maintenance_handle);
            }
            Err(e) => {
                tracing::error!("failed to start MySQL proxy: {e:#}");
            }
        }
    }

    // PostgreSQL proxy
    if let Some(pg_config) = &config.db.postgres {
        let url = pg_config.url.clone();
        let listen = pg_config.listen.clone().unwrap_or_else(|| "127.0.0.1:5432".to_string());

        let pool_config = ephpm_db::pool::PoolConfig {
            min_connections: pg_config.min_connections,
            max_connections: pg_config.max_connections,
            idle_timeout: parse_duration(&pg_config.idle_timeout)?,
            max_lifetime: parse_duration(&pg_config.max_lifetime)?,
            pool_timeout: parse_duration(&pg_config.pool_timeout)?,
            health_check_interval: parse_duration(&pg_config.health_check_interval)?,
        };

        let reset_strategy = ephpm_db::ResetStrategy::from_str_lossy(&pg_config.reset_strategy);

        let replica_urls = pg_config.replicas.as_ref().map(|r| r.urls.clone()).unwrap_or_default();

        let rw_split = ephpm_db::postgres::PgRwSplitParams {
            enabled: config.db.read_write_split.enabled,
            sticky_duration: parse_duration(&config.db.read_write_split.sticky_duration)?,
        };

        match ephpm_db::postgres::build_proxy(
            &url,
            &listen,
            pool_config,
            reset_strategy,
            replica_urls,
            rw_split,
        )
        .await
        {
            Ok(proxy) => {
                let maintenance_handle = proxy.start_maintenance();
                let proxy_handle = tokio::spawn(async move {
                    match proxy.run().await {
                        Ok(()) => tracing::info!("PostgreSQL proxy stopped"),
                        Err(e) => tracing::error!("PostgreSQL proxy error: {e:#}"),
                    }
                });
                handles.push(proxy_handle);
                drop(maintenance_handle);
            }
            Err(e) => {
                tracing::error!("failed to start PostgreSQL proxy: {e:#}");
            }
        }
    }

    // TDS (SQL Server) proxy — not yet implemented.
    // The TDS wire protocol is planned but not available. Log a clear
    // warning so users know to use the MySQL proxy instead.
    if config.db.tds.is_some() {
        tracing::warn!(
            "TDS (SQL Server) proxy is configured but not yet implemented. \
             The TDS wire protocol is planned for a future release. \
             Consider using the MySQL proxy ([db.mysql]) instead."
        );
    }

    // Embedded SQLite via litewire
    if let Some(sqlite_config) = &config.db.sqlite {
        let is_clustered = is_clustered_sqlite(sqlite_config, cluster.is_some());

        if is_clustered {
            start_clustered_sqlite(sqlite_config, cluster, query_stats, &mut handles).await?;
        } else {
            start_single_node_sqlite(sqlite_config, query_stats, &mut handles)?;
        }
    }

    Ok(handles)
}

/// Check if clustered SQLite mode should be used.
fn is_clustered_sqlite(sqlite_config: &ephpm_config::SqliteConfig, cluster_enabled: bool) -> bool {
    let role = sqlite_config.replication.role.as_str();
    role == "primary" || role == "replica" || (role == "auto" && cluster_enabled)
}

/// Start single-node SQLite (in-process rusqlite, no sqld).
fn start_single_node_sqlite(
    sqlite_config: &ephpm_config::SqliteConfig,
    query_stats: &ephpm_query_stats::QueryStats,
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
) -> anyhow::Result<()> {
    let db_path = &sqlite_config.path;
    let backend = litewire::backend::Rusqlite::open(db_path)
        .with_context(|| format!("failed to open SQLite database: {db_path}"))?;
    tracing::info!(path = %db_path, "opened embedded SQLite database (single-node)");

    let tracked = tracked_backend::TrackedBackend::new(backend, query_stats.clone());
    let mut builder = litewire::LiteWire::new(tracked);
    builder = builder.mysql(&sqlite_config.proxy.mysql_listen);
    tracing::info!(
        listen = %sqlite_config.proxy.mysql_listen,
        "SQLite MySQL wire protocol enabled"
    );

    if let Some(ref hrana_addr) = sqlite_config.proxy.hrana_listen {
        builder = builder.hrana(hrana_addr);
        tracing::info!(listen = %hrana_addr, "SQLite Hrana HTTP API enabled");
    }

    if let Some(ref pg_addr) = sqlite_config.proxy.postgres_listen {
        builder = builder.postgres(pg_addr);
        tracing::info!(listen = %pg_addr, "SQLite PostgreSQL wire protocol enabled");
    }

    if let Some(ref tds_addr) = sqlite_config.proxy.tds_listen {
        builder = builder.tds(tds_addr);
        tracing::info!(listen = %tds_addr, "SQLite TDS wire protocol enabled");
    }

    handles.push(tokio::spawn(async move {
        match builder.serve().await {
            Ok(()) => tracing::info!("litewire stopped"),
            Err(e) => tracing::error!("litewire error: {e:#}"),
        }
    }));
    Ok(())
}

/// Start clustered SQLite (sqld sidecar + litewire with Hrana client backend).
async fn start_clustered_sqlite(
    sqlite_config: &ephpm_config::SqliteConfig,
    cluster: Option<&Arc<ephpm_cluster::ClusterHandle>>,
    query_stats: &ephpm_query_stats::QueryStats,
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
) -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    {
        anyhow::bail!(
            "clustered SQLite (sqld sidecar) is not supported on Windows. \
             Use single-node mode (remove [db.sqlite.replication] or set role = \"auto\" \
             without clustering), or run on Linux/macOS/WSL."
        );
    }

    // Determine the initial sqld role and optional role-change receiver.
    let (sqld_role, role_rx) = match sqlite_config.replication.role.as_str() {
        "primary" => {
            tracing::info!("SQLite replication role forced to primary");
            (ephpm_sqld::SqldRole::Primary, None)
        }
        "replica" => {
            let url = &sqlite_config.replication.primary_grpc_url;
            anyhow::ensure!(
                !url.is_empty(),
                "replication.primary_grpc_url is required when role = \"replica\""
            );
            tracing::info!(primary = %url, "SQLite replication role forced to replica");
            (ephpm_sqld::SqldRole::Replica { primary_grpc_url: url.clone() }, None)
        }
        _ => {
            // "auto" — use gossip election
            let cluster_handle =
                cluster.context("cluster must be enabled for auto SQLite replication")?;
            let election = ephpm_cluster::SqliteElection::new(
                Arc::clone(cluster_handle),
                sqlite_config.sqld.grpc_listen.clone(),
            );
            let initial = election.determine_initial_role().await;
            let rx = election.watch_role();

            // Spawn the election loop.
            tokio::spawn(election.run());

            (elected_to_sqld_role(&initial), Some(rx))
        }
    };

    // Spawn sqld as a child process.
    let sqld_config = ephpm_sqld::SqldConfig {
        db_path: sqlite_config.path.clone(),
        http_listen: sqlite_config.sqld.http_listen.clone(),
        grpc_listen: sqlite_config.sqld.grpc_listen.clone(),
    };

    let sqld = ephpm_sqld::SqldProcess::spawn(sqld_config, sqld_role)
        .await
        .context("failed to start sqld")?;

    // Wait for sqld to become healthy.
    sqld.wait_healthy(Duration::from_secs(30)).await.context("sqld did not become healthy")?;

    let sqld_http_url = sqld.http_url();
    tracing::info!(url = %sqld_http_url, "sqld is healthy, starting litewire with Hrana backend");

    // Shared handle so the role-change watcher can restart sqld on failover.
    let sqld = Arc::new(tokio::sync::Mutex::new(sqld));

    // Spawn role-change watcher that restarts sqld when the election result changes.
    if let Some(mut watch_rx) = role_rx {
        let sqld_for_watcher = Arc::clone(&sqld);
        handles.push(tokio::spawn(async move {
            while watch_rx.changed().await.is_ok() {
                let new_elected = watch_rx.borrow().clone();
                let new_role = elected_to_sqld_role(&new_elected);
                tracing::info!(?new_role, "SQLite election: role changed, restarting sqld");

                let mut sqld = sqld_for_watcher.lock().await;
                if let Err(e) = sqld.restart(new_role).await {
                    tracing::error!("failed to restart sqld after role change: {e:#}");
                    continue;
                }

                // Wait for sqld to become healthy after restart.
                if let Err(e) = sqld.wait_healthy(Duration::from_secs(30)).await {
                    tracing::error!("sqld not healthy after restart: {e:#}");
                }
            }
        }));
    }

    // Create Hrana client backend pointing at local sqld, wrapped with stats tracking.
    let backend = litewire::backend::HranaClient::new(&sqld_http_url);
    let tracked = tracked_backend::TrackedBackend::new(backend, query_stats.clone());

    // Start litewire with the tracked backend.
    let mut builder = litewire::LiteWire::new(tracked);
    builder = builder.mysql(&sqlite_config.proxy.mysql_listen);
    tracing::info!(
        listen = %sqlite_config.proxy.mysql_listen,
        "SQLite MySQL wire protocol enabled (clustered)"
    );

    if let Some(ref hrana_addr) = sqlite_config.proxy.hrana_listen {
        builder = builder.hrana(hrana_addr);
        tracing::info!(listen = %hrana_addr, "SQLite Hrana HTTP API enabled (clustered)");
    }

    if let Some(ref pg_addr) = sqlite_config.proxy.postgres_listen {
        builder = builder.postgres(pg_addr);
        tracing::info!(listen = %pg_addr, "SQLite PostgreSQL wire protocol enabled (clustered)");
    }

    if let Some(ref tds_addr) = sqlite_config.proxy.tds_listen {
        builder = builder.tds(tds_addr);
        tracing::info!(listen = %tds_addr, "SQLite TDS wire protocol enabled (clustered)");
    }

    // Spawn litewire serve task. sqld is kept alive via the Arc.
    let sqld_guard = Arc::clone(&sqld);
    handles.push(tokio::spawn(async move {
        let _sqld_guard = sqld_guard;
        match builder.serve().await {
            Ok(()) => tracing::info!("litewire stopped (clustered)"),
            Err(e) => tracing::error!("litewire error (clustered): {e:#}"),
        }
    }));

    Ok(())
}

/// Convert an [`ElectedRole`] to an [`SqldRole`].
fn elected_to_sqld_role(elected: &ephpm_cluster::ElectedRole) -> ephpm_sqld::SqldRole {
    match elected {
        ephpm_cluster::ElectedRole::Primary => ephpm_sqld::SqldRole::Primary,
        ephpm_cluster::ElectedRole::Replica { primary_grpc_url } => {
            ephpm_sqld::SqldRole::Replica { primary_grpc_url: primary_grpc_url.clone() }
        }
    }
}

/// Parse a memory size string (e.g. "256MB", "1GB") to bytes.
fn parse_memory_size(s: &str) -> anyhow::Result<usize> {
    let s = s.trim().to_uppercase();

    let (num_str, multiplier) = if s.ends_with("MB") {
        (&s[..s.len() - 2], 1024 * 1024)
    } else if s.ends_with("GB") {
        (&s[..s.len() - 2], 1024 * 1024 * 1024)
    } else if s.ends_with("KB") {
        (&s[..s.len() - 2], 1024)
    } else {
        (s.as_str(), 1)
    };

    let num: usize = num_str.trim().parse().with_context(|| format!("invalid memory size: {s}"))?;
    Ok(num.saturating_mul(multiplier))
}

/// Parse a duration string (e.g. "30s", "5m", "1h") to `std::time::Duration`.
fn parse_duration(s: &str) -> anyhow::Result<std::time::Duration> {
    ephpm_db::duration::parse_duration(s).map_err(|e| anyhow::anyhow!("{e}"))
}

#[cfg(test)]
mod lib_tests {
    use super::*;

    #[test]
    fn parse_memory_size_megabytes() {
        assert_eq!(parse_memory_size("256MB").unwrap(), 256 * 1024 * 1024);
    }

    #[test]
    fn parse_memory_size_gigabytes() {
        assert_eq!(parse_memory_size("1GB").unwrap(), 1024 * 1024 * 1024);
    }

    #[test]
    fn parse_memory_size_kilobytes() {
        assert_eq!(parse_memory_size("512KB").unwrap(), 512 * 1024);
    }

    #[test]
    fn parse_memory_size_bytes_no_suffix() {
        assert_eq!(parse_memory_size("1024").unwrap(), 1024);
    }

    #[test]
    fn parse_memory_size_lowercase() {
        assert_eq!(parse_memory_size("256mb").unwrap(), 256 * 1024 * 1024);
    }

    #[test]
    fn parse_memory_size_with_whitespace() {
        assert_eq!(parse_memory_size(" 256MB ").unwrap(), 256 * 1024 * 1024);
    }

    #[test]
    fn parse_memory_size_invalid() {
        assert!(parse_memory_size("notanumber").is_err());
    }

    #[test]
    fn parse_memory_size_zero() {
        assert_eq!(parse_memory_size("0").unwrap(), 0);
    }

    fn make_sqlite_config(role: &str) -> ephpm_config::SqliteConfig {
        ephpm_config::SqliteConfig {
            path: "test.db".into(),
            proxy: ephpm_config::SqliteProxyConfig::default(),
            sqld: ephpm_config::SqldConfig::default(),
            replication: ephpm_config::ReplicationConfig {
                role: role.into(),
                primary_grpc_url: String::new(),
            },
        }
    }

    #[test]
    fn clustered_sqlite_auto_without_cluster() {
        let config = make_sqlite_config("auto");
        assert!(!is_clustered_sqlite(&config, false));
    }

    #[test]
    fn clustered_sqlite_auto_with_cluster() {
        let config = make_sqlite_config("auto");
        assert!(is_clustered_sqlite(&config, true));
    }

    #[test]
    fn clustered_sqlite_explicit_primary() {
        let config = make_sqlite_config("primary");
        assert!(is_clustered_sqlite(&config, false));
        assert!(is_clustered_sqlite(&config, true));
    }

    #[test]
    fn clustered_sqlite_explicit_replica() {
        let config = make_sqlite_config("replica");
        assert!(is_clustered_sqlite(&config, false));
        assert!(is_clustered_sqlite(&config, true));
    }
}
