pub mod acme;
pub mod body;
pub mod file_cache;
mod idle;
pub mod metrics;
pub mod middleware;
pub mod opcache;
pub mod rate_limit;
pub mod router;
pub mod static_files;
pub mod tls;
pub mod tracked_backend;
pub mod worker_pool;

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

    // Wire the KV store into the middleware host table (a no-op when no
    // middleware is mounted), then load the chain — fail fast at startup on
    // any unresolvable library, missing symbol, or failing init.
    ephpm_middleware::host::set_kv_store(&kv_store);
    let middleware_chain = if config.middleware.is_empty() {
        None
    } else {
        let chain = middleware::MiddlewareChain::load(&config.middleware)
            .context("failed to load native middleware chain")?;
        tracing::info!(
            count = chain.len(),
            modules = ?chain.module_names(),
            "middleware chain loaded"
        );
        // In cluster mode, the built-in ratelimit middleware uses local KV
        // INCR to track the request count per window. SET/DEL and EXPIRE
        // now replicate across the cluster (KV replication v1.1), but
        // INCR is still local-only — read-modify-write ops need owner
        // routing to be cluster-correct (see
        // site/content/roadmap/clustered-kv-v2.md, "Replicated counters"),
        // so the rate limit is still enforced PER NODE, not across the
        // whole fleet. Surface the gap at startup instead of leaving
        // operators to find it in prod.
        if config.cluster.enabled && chain.module_names().contains(&"ratelimit") {
            tracing::warn!(
                "[middleware] ratelimit mounted with [cluster].enabled = true — KV INCR is \
                 not yet replicated across nodes (SET/DEL/EXPIRE now do), so rate limits are \
                 enforced PER NODE. A client hitting N nodes gets up to N × the configured \
                 allowance. See site/content/reference/middleware/ratelimit.md for the current \
                 status."
            );
        }
        Some(Arc::new(chain))
    };

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
        // When [cluster] secret is set, frames are sealed with a key
        // derived from it (nodes without the secret cannot read/inject).
        let data_port = config.cluster.kv.data_port;
        let data_plane_store = Arc::clone(&kv_store);
        let data_plane_cipher = if config.cluster.secret.is_empty() {
            None
        } else {
            Some(Arc::new(ephpm_cluster::ClusterCipher::for_kv_data_plane(&config.cluster.secret)))
        };
        tokio::spawn(async move {
            if let Err(e) =
                ephpm_cluster::data_plane::serve(data_plane_store, data_port, data_plane_cipher)
                    .await
            {
                tracing::error!(%e, "KV data plane error");
            }
        });

        let cluster_handle = Arc::new(handle);

        // Wire the local KV Store through the ClusteredStore replicator so
        // RESP + PHP native writes routed via `Store::set`/`remove`/`expire`
        // fan out to cluster peers (small values via chitchat gossip; large
        // values via the TCP data plane with `replication_factor` copies).
        //
        // This resolves the gap where a `SET foo bar` on node A would only
        // touch node A's local map — issue #143. Without this hook the
        // clustered KV knobs (`[cluster.kv].replication_factor` /
        // `.replication_mode`) are silent no-ops from the RESP + PHP lanes,
        // and cluster-wide features like OPcache invalidation cannot fan
        // out across nodes.
        let clustered = ephpm_cluster::ClusteredStore::new(
            Arc::clone(&kv_store),
            Arc::clone(&cluster_handle),
            config.cluster.kv.clone(),
            if config.cluster.secret.is_empty() {
                None
            } else {
                Some(Arc::new(ephpm_cluster::ClusterCipher::for_kv_data_plane(
                    &config.cluster.secret,
                )))
            },
        );
        // Wake the hot-key invalidation watcher (no-op when hot_key_cache
        // is disabled in config).
        clustered.init_hot_key_watcher().await;

        // Shared last-arrival-wins ordering map: threaded through both the
        // replicator (records origin writes) and the applier (records
        // remote applies), so a slow gossip echo of an older write can't
        // clobber a newer local overwrite.
        let applied = ephpm_cluster::clustered_store::new_applied_write_map();
        let replicator = ephpm_cluster::KvReplicator::new(
            Arc::clone(&clustered),
            tokio::runtime::Handle::current(),
            Arc::clone(&applied),
        );
        kv_store.set_replicator(Some(replicator as Arc<dyn ephpm_kv::store::Replicator>));

        // Materialize REMOTE gossip-tier writes into this node's local
        // Store so raw-store readers (RESP GET, PHP native functions, the
        // OPcache watcher) see cluster writes; the origin node materializes
        // synchronously inside the replicator.
        ephpm_cluster::clustered_store::start_gossip_applier(
            &cluster_handle,
            Arc::clone(&kv_store),
            applied,
        )
        .await;

        tracing::info!(
            small_key_threshold = config.cluster.kv.small_key_threshold,
            replication_factor = config.cluster.kv.replication_factor,
            replication_mode = %config.cluster.kv.replication_mode,
            "clustered KV replicator installed on local Store"
        );

        Some(cluster_handle)
    } else {
        None
    };

    // Create shared query stats collector. The label-series cap keeps
    // Prometheus cardinality bounded regardless of query template
    // explosion (see `StatsConfig::metric_label_series_max`).
    let query_stats = ephpm_query_stats::QueryStats::new(ephpm_query_stats::StatsConfig {
        enabled: config.db.analysis.query_stats,
        slow_query_threshold: parse_duration(&config.db.analysis.slow_query_threshold)
            .unwrap_or(Duration::from_secs(1)),
        max_digests: config.db.analysis.digest_store_max_entries,
        metric_label_series_max: config.db.analysis.metric_label_series_max,
    });

    let _db_handles = start_db_proxies(&config, cluster_handle.as_ref(), &query_stats).await?;

    let listeners = bind_listeners(&config, kv_store, metrics_handle, middleware_chain).await?;
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
    shutdown_timeout: Duration,
    router: Arc<Router>,
    limiter: Option<Arc<rate_limit::Limiter>>,
    file_cache: Option<Arc<file_cache::FileCache>>,
    /// Interval for file cache eviction sweeps (derived from `inactive_secs`).
    file_cache_eviction_interval: Duration,
    /// Persistent worker pool (worker mode), drained on shutdown.
    worker_pool: Option<Arc<worker_pool::WorkerPool>>,
}

/// Connection-level settings passed into spawned tasks.
#[derive(Clone, Copy)]
struct ConnSettings {
    header_read_timeout: Duration,
    max_header_size: usize,
    /// Close connections with no read/write activity for this long.
    /// Zero disables the idle watchdog.
    idle_timeout: Duration,
}

/// Parse config, build TLS, and bind all listeners.
async fn bind_listeners(
    config: &Config,
    kv_store: Arc<ephpm_kv::store::Store>,
    metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
    middleware_chain: Option<Arc<middleware::MiddlewareChain>>,
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
        idle_timeout: Duration::from_secs(config.server.timeouts.idle),
    };
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

    // Worker mode: wire the worker ops table and spawn the persistent worker
    // pool BEFORE the router so PHP requests can be dispatched to it. PHP is
    // already initialized (in main.rs, before the tokio runtime). fpm mode
    // leaves this None and uses the spawn_blocking path unchanged.
    let worker_pool = if config.php.is_worker_mode() {
        let script = config
            .resolve_worker_script()
            .context("worker mode: failed to resolve worker_script")?;

        if config.php.workers > 0 {
            tracing::warn!(
                "[php] workers = {} is ignored in worker mode — concurrency is \
                 bounded by worker_count and worker_backlog",
                config.php.workers
            );
        }

        // Windows / NTS: a single PHP context, so force one worker (design §6.1).
        let (mut worker_count, wc_source) = config.php.effective_worker_count_with_source();
        match wc_source {
            ephpm_config::WorkerCountSource::Explicit => {
                tracing::info!(
                    worker_count,
                    source = "explicit",
                    "worker_count from [php].worker_count"
                );
            }
            ephpm_config::WorkerCountSource::CgroupQuota { quota_cpus } => {
                tracing::info!(
                    worker_count,
                    source = "cgroup_quota",
                    quota_cpus,
                    "worker_count derived from cgroup CPU quota (ceil(quota))"
                );
            }
            ephpm_config::WorkerCountSource::HostParallelism { cpus } => {
                tracing::info!(
                    worker_count,
                    source = "host_parallelism",
                    detected_cpus = cpus,
                    "worker_count derived from host parallelism (clamped [2, 32])"
                );
            }
        }
        if cfg!(target_os = "windows") && worker_count > 1 {
            tracing::warn!(
                "worker mode on Windows (NTS) uses a single PHP context — \
                 forcing worker_count = 1 (requests serialize through one \
                 booted framework)"
            );
            worker_count = 1;
        }

        ephpm_php::PhpRuntime::install_worker_ops(config.php.worker_populate_superglobals);

        tracing::info!(
            worker_stream_threshold = config.php.worker_stream_threshold,
            "worker mode: request bodies at/above worker_stream_threshold stream \
             into the worker (flat memory); smaller bodies buffer"
        );

        let pool = worker_pool::WorkerPool::spawn(
            script,
            worker_count,
            config.php.worker_max_requests,
            config.php.effective_worker_backlog(),
            Duration::from_secs(config.php.worker_boot_timeout),
            // A client that stops reading a streamed download for longer than
            // the idle timeout aborts the stream (frees the worker thread) —
            // same idleness contract the connection layer applies.
            Duration::from_secs(config.server.timeouts.idle),
        );
        Some(pool)
    } else {
        // add-config-knob: worker_stream_threshold is worker-mode-only. Warn if
        // an fpm-mode operator set it to a non-default, so it is never a silent
        // no-op.
        if config.php.worker_stream_threshold != 1024 * 1024 {
            tracing::warn!(
                "[php] worker_stream_threshold is ignored in fpm mode (it only \
                 governs worker-mode request-body streaming)"
            );
        }
        None
    };

    let router = Arc::new(
        Router::new(
            config,
            kv_store,
            metrics_handle,
            limiter.clone(),
            file_cache.clone(),
            worker_pool.clone(),
        )
        .with_middleware_chain(middleware_chain),
    );

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
        shutdown_timeout,
        router,
        limiter,
        file_cache,
        file_cache_eviction_interval,
        worker_pool,
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
        shutdown_timeout,
        router,
        limiter,
        file_cache,
        file_cache_eviction_interval,
        worker_pool,
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

    // Worker mode: stop accepting new dispatch so in-flight worker iterations
    // finish and workers exit their loops cleanly (design §4.5).
    if let Some(pool) = &worker_pool {
        pool.drain();
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
                serve_connection(stream, router, remote_addr, false, conn).await;
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

    serve_connection(tls_stream, router, remote_addr, true, settings).await;
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
            serve_connection(tls_stream, router, remote_addr, true, settings).await;
        }
        Err(err) => {
            tracing::debug!(%remote_addr, %err, "TLS handshake failed");
        }
    }
}

// hyper's `max_buf_size` panics if given a value below its internal
// `MINIMUM_MAX_BUFFER_SIZE` (8192 in hyper 1.x). Our `max_header_size` config is
// allowed to be smaller — oversized headers above the configured limit are still
// rejected by hyper's buffer ceiling, which is at most this floor.
const HYPER_MIN_BUF_SIZE: usize = 8192;

fn hyper_max_buf_size(configured: usize) -> usize {
    configured.max(HYPER_MIN_BUF_SIZE)
}

/// Serve an HTTP connection over any transport (`TcpStream` or `TlsStream`).
///
/// Uses [`auto::Builder`] which negotiates HTTP/1.1 or HTTP/2 based on the
/// ALPN protocol agreed during the TLS handshake. Plain (non-TLS) connections
/// always use HTTP/1.1, since h2c (HTTP/2 cleartext) is not supported by
/// browsers.
///
/// When `settings.idle_timeout` is non-zero, the stream is wrapped in an
/// activity-tracking adapter and the connection future is raced against an
/// idle watchdog: after a full quiet window with no bytes read or written,
/// hyper's graceful shutdown is triggered so in-flight requests finish and
/// idle keep-alive connections close immediately.
async fn serve_connection<I>(
    stream: I,
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

    let tracker = idle::ActivityTracker::new();
    let io = TokioIo::new(idle::IdleIo::new(stream, tracker.clone()));

    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .keep_alive(true)
        .header_read_timeout(settings.header_read_timeout)
        .max_buf_size(hyper_max_buf_size(settings.max_header_size))
        .timer(hyper_util::rt::TokioTimer::new());

    let conn = builder.serve_connection_with_upgrades(io, service);
    let mut conn = std::pin::pin!(conn);

    let result = if settings.idle_timeout.is_zero() {
        conn.await
    } else {
        tokio::select! {
            result = conn.as_mut() => result,
            () = tracker.idle_expired(settings.idle_timeout) => {
                tracing::debug!(
                    %remote_addr,
                    idle_secs = settings.idle_timeout.as_secs(),
                    "closing idle connection"
                );
                conn.as_mut().graceful_shutdown();
                conn.await
            }
        }
    };

    if let Err(err) = result {
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
        .max_buf_size(hyper_max_buf_size(settings.max_header_size))
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
    if config.kv.redis_compat.socket.is_some() {
        tracing::warn!(
            "[kv.redis_compat].socket is set but Unix-socket listening is not yet \
             implemented — the RESP listener is TCP-only (listening on {}); remove \
             the socket key or point clients at the TCP address",
            config.kv.redis_compat.listen
        );
    }
    let listen = config.kv.redis_compat.listen.clone();
    let password = config.kv.redis_compat.password.clone();
    let secret = config.kv.secret.clone();
    let server_config = ephpm_kv::server::ServerConfig {
        listen,
        password,
        secret: secret.clone(),
        max_connections: config.kv.redis_compat.max_connections,
        max_input_buffer: config.kv.redis_compat.max_input_buffer,
        idle_timeout_secs: config.kv.redis_compat.idle_timeout_secs,
    };

    // Multi-tenant mode with the RESP listener enabled but no master secret:
    // per-site AUTH cannot be derived, so every tenant (and anything else that
    // can reach the listener) talks to the shared default store unauthenticated.
    if config.server.sites_dir.is_some() && secret.is_none() {
        tracing::warn!(
            "[kv].secret is not set while server.sites_dir (multi-tenant mode) and \
             kv.redis_compat are enabled — per-site RESP AUTH is disabled and any \
             client that can reach the RESP listener can access the default store; \
             set [kv].secret to enable per-site authentication"
        );
    }

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

        if let Some(socket) = &mysql_config.socket {
            tracing::warn!(
                socket = %socket.display(),
                listen = %listen,
                "[db.mysql].socket is configured but Unix socket listeners are not \
                 yet supported — only the TCP listener is active"
            );
        }

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

        if let Some(socket) = &pg_config.socket {
            tracing::warn!(
                socket = %socket.display(),
                listen = %listen,
                "[db.postgres].socket is configured but Unix socket listeners are not \
                 yet supported — only the TCP listener is active"
            );
        }

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
#[allow(unreachable_code, unused_variables)]
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

    // ── idle timeout ────────────────────────────────────────────────────────

    /// Minimal router serving `dir` with a static-only fallback (no PHP).
    fn idle_test_router(dir: &std::path::Path) -> Arc<Router> {
        let config = ephpm_config::Config {
            server: ephpm_config::ServerConfig {
                document_root: dir.to_path_buf(),
                fallback: vec!["$uri".to_string(), "=404".to_string()],
                ..ephpm_config::ServerConfig::default()
            },
            php: ephpm_config::PhpConfig::default(),
            db: ephpm_config::DbConfig::default(),
            kv: ephpm_config::KvConfig::default(),
            cluster: ephpm_config::ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        let store = ephpm_kv::store::Store::new(ephpm_kv::store::StoreConfig::default());
        Arc::new(Router::new(&config, store, None, None, None, None))
    }

    /// Bind a listener and serve exactly one connection with `settings`.
    async fn spawn_one_shot_server(settings: ConnSettings) -> SocketAddr {
        let dir = tempfile::tempdir().expect("tempdir");
        let router = idle_test_router(dir.path());
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            // Keep the docroot alive for the connection's lifetime.
            let _dir = dir;
            let (stream, remote) = listener.accept().await.expect("accept");
            serve_connection(stream, router, remote, false, settings).await;
        });
        addr
    }

    #[tokio::test]
    async fn idle_timeout_closes_silent_connection() {
        use tokio::io::AsyncReadExt as _;

        let addr = spawn_one_shot_server(ConnSettings {
            header_read_timeout: Duration::from_secs(30),
            max_header_size: 8192,
            idle_timeout: Duration::from_secs(1),
        })
        .await;

        let mut client = TcpStream::connect(addr).await.expect("connect");
        let mut buf = [0u8; 32];
        // Send nothing — the server must close the connection shortly after
        // the 1s idle window (well before the 30s header-read timeout).
        let n = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
            .await
            .expect("server did not close idle connection within 5s")
            .expect("read after server-side close");
        assert_eq!(n, 0, "expected EOF from server-side close");
    }

    #[tokio::test]
    async fn idle_timeout_closes_keep_alive_connection_after_response() {
        use tokio::io::AsyncReadExt as _;

        let addr = spawn_one_shot_server(ConnSettings {
            header_read_timeout: Duration::from_secs(30),
            max_header_size: 8192,
            idle_timeout: Duration::from_secs(1),
        })
        .await;

        let mut client = TcpStream::connect(addr).await.expect("connect");
        client
            .write_all(b"GET /missing.txt HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .expect("write request");

        // Read the response, then keep the connection open and silent — the
        // idle watchdog must re-arm after activity and close it afterwards.
        let mut saw_response = false;
        let mut buf = [0u8; 4096];
        loop {
            let n = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
                .await
                .expect("server did not respond/close within 5s")
                .expect("read");
            if n == 0 {
                break;
            }
            saw_response = true;
        }
        assert!(saw_response, "expected an HTTP response before the idle close");
    }

    #[tokio::test]
    async fn idle_timeout_zero_disables_watchdog() {
        use tokio::io::AsyncReadExt as _;

        let addr = spawn_one_shot_server(ConnSettings {
            header_read_timeout: Duration::from_secs(30),
            max_header_size: 8192,
            idle_timeout: Duration::ZERO,
        })
        .await;

        let mut client = TcpStream::connect(addr).await.expect("connect");
        // Stay silent past what would be a small idle window, then confirm
        // the connection still works.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        client
            .write_all(b"GET /missing.txt HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .expect("write request");
        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
            .await
            .expect("no response within 5s")
            .expect("read response");
        assert!(n > 0, "expected response bytes on a still-open connection");
        assert!(buf.starts_with(b"HTTP/1.1"), "expected an HTTP/1.1 response line");
    }
}
