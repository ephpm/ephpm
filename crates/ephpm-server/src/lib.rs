pub mod acme;
pub mod body;
pub mod db_health;
pub mod file_cache;
pub mod http3;
mod idle;
pub mod metrics;
pub mod middleware;
pub mod opcache;
#[cfg(feature = "otlp")]
pub mod otlp;
pub mod rate_limit;
pub mod router;
pub mod static_files;
pub mod stream_compress;
mod timeline;
pub mod tls;
pub mod tracked_backend;
pub mod turso_cdc;
pub mod turso_cdc_metrics;
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

/// `tracing` target of the request spans (`http.request`,
/// `worker.queue_wait`, `php.execute`) emitted by the router at DEBUG level.
///
/// A dedicated target so the OTLP layer (see the `otlp` feature) can enable
/// exactly these spans with a `Targets` filter without also exporting every
/// debug log line — and so the default `info`-level stack leaves the span
/// callsites disabled (a single static check per request).
pub const OTEL_TRACE_TARGET: &str = "ephpm_otel";

/// Start the HTTP server with the given configuration.
///
/// `dev_mode` is `true` under `ephpm dev` / bare `ephpm` and `false` under
/// `ephpm serve`. It selects the default for the request-timeline ring
/// buffer (`/_ephpm/requests`): on in dev, off in serve, unless
/// `[server.diagnostics] request_log` says otherwise.
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
pub async fn serve(config: Config, dev_mode: bool) -> anyhow::Result<()> {
    // Install Prometheus recorder if metrics are enabled.
    let metrics_handle = if config.server.metrics.enabled {
        Some(metrics::init().context("failed to initialize metrics")?)
    } else {
        None
    };

    // Start background services.
    let (kv_store, multi_tenant_kv, _kv_handle) = start_kv_service(&config)?;

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
        // Fail closed on an unauthenticated cluster: an empty secret means
        // gossip and the KV data plane run as plaintext with no auth, so any
        // host on the cluster network can forge KV writes. Require a secret
        // unless the operator explicitly opts into insecure mode.
        config
            .cluster
            .ensure_secure()
            .map_err(|msg| anyhow::anyhow!(msg))
            .context("refusing to start clustering without authentication")?;
        if config.cluster.allow_insecure_no_auth && config.cluster.secret.is_empty() {
            tracing::warn!(
                "[cluster] allow_insecure_no_auth = true with an empty secret: gossip and the \
                 KV data plane are running as UNAUTHENTICATED PLAINTEXT. Any host on the cluster \
                 network can read and forge KV writes. Only use this on a fully trusted private \
                 network with the gossip and data-plane ports firewalled from untrusted hosts."
            );
        }

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

    // Cluster channel — lazy-bound. Bound only if a channel feature
    // (today: Turso CDC replication) is enabled on this node. When no
    // feature asks, `maybe_start_cluster_channel` returns `Ok(None)`
    // and no socket is bound — preserving byte-identical startup for
    // any config that doesn't opt in to a channel feature.
    let channel_handle = if let Some(ref cluster_handle) = cluster_handle {
        let features = resolve_channel_features(&config);
        ephpm_cluster::maybe_start_cluster_channel(
            &config.cluster.channel,
            &config.cluster.secret,
            cluster_handle,
            features,
        )
        .await
        .context("failed to start cluster channel")?
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

    // Upstream health for every configured SQL proxy, built BEFORE the HTTP
    // listeners so `/_ephpm/ready` can never report ready in the window
    // between the router existing and the proxies starting. Fails startup on
    // a malformed proxy URL.
    let db_health = db_health::DbProxyHealth::from_config(&config)?;

    // Bind the HTTP listeners BEFORE constructing DB proxies: proxy startup
    // now retries a down backend with backoff (up to ~40s per proxy), and if
    // that ran first the listen sockets wouldn't exist yet — long enough for
    // orchestrator TCP readiness probes to kill the pod. With the sockets
    // bound, probes pass and connections queue in the accept backlog while
    // the proxies come up. Hard proxy errors still fail startup.
    // Effective node id for PHP's EPHPM_NODE_ID: the running gossip node's id
    // when clustered (distinct per node even if `[cluster] node_id` was left
    // empty and auto-derived), else None (Router falls back to config).
    let effective_node_id = cluster_handle.as_ref().map(|h| h.self_node().id.clone());

    // Request timeline ring buffer (`/_ephpm/requests`): on by default in
    // dev mode, opt-in via [server.diagnostics] request_log in serve mode.
    let request_log = config
        .server
        .diagnostics
        .effective_request_log(dev_mode)
        .then(|| Arc::new(timeline::RequestLog::new(timeline::REQUEST_LOG_CAPACITY)));
    if request_log.is_some() {
        tracing::info!(
            capacity = timeline::REQUEST_LOG_CAPACITY,
            "request timeline enabled — last requests served at /_ephpm/requests"
        );
    }

    let listeners = bind_listeners(
        &config,
        kv_store,
        multi_tenant_kv,
        metrics_handle,
        middleware_chain,
        effective_node_id,
        Arc::clone(&db_health),
        request_log,
    )
    .await?;

    let _db_handles = start_db_proxies(
        &config,
        cluster_handle.as_ref(),
        channel_handle.as_ref(),
        &query_stats,
        &db_health,
    )
    .await?;

    accept_loop(listeners).await
}

/// Resolve which cluster-channel features are enabled on this node.
///
/// The single source of truth for the "if nothing needs it, don't bind
/// it" contract — extend this when adding a new channel feature.
///
/// The conditions must match the ones `start_db_proxies` uses to
/// actually take the CDC path, `is_clustered_sqlite()` included:
/// without that last check a config with `replication.role = "single"`
/// would open a channel listener and only *then* log that
/// `cdc_experimental` is being ignored.
fn resolve_channel_features(config: &Config) -> ephpm_cluster::ChannelFeatureFlags {
    // Clustered SQLite always replicates over the Turso CDC path as of
    // v0.7.0 (sqld removed), so the channel is needed exactly when the
    // SQLite config resolves to clustered mode. Turso is the only engine,
    // so there is no engine gate.
    let cdc =
        config.db.sqlite.as_ref().is_some_and(|s| is_clustered_sqlite(s, config.cluster.enabled));
    ephpm_cluster::ChannelFeatureFlags { cdc }
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
    /// Bound QUIC endpoint serving HTTP/3, when `[server.http3]` is enabled.
    /// `None` means HTTP/3 is off — the TCP listeners are unaffected either
    /// way, since HTTP/3 is additive (UDP) rather than a replacement.
    http3_endpoint: Option<quinn::Endpoint>,
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
    // The one `MultiTenantStore` built by `start_kv_service`. `Some` exactly
    // when `[server] sites_dir` is set; shared with the RESP listener so PHP
    // and RESP clients see one keyspace per vhost.
    multi_tenant_kv: Option<ephpm_kv::multi_tenant::MultiTenantStore>,
    metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
    middleware_chain: Option<Arc<middleware::MiddlewareChain>>,
    // Effective cluster node id (from the running gossip handle), injected into
    // PHP `$_SERVER` as `EPHPM_NODE_ID`. `None` in single-node mode, where the
    // Router falls back to `[cluster] node_id` from config.
    node_id: Option<String>,
    // Upstream health of the configured SQL proxies, so the readiness probe
    // reports 503 until each one has reached its upstream at least once.
    // (`request_log` follows below: the dev-mode request timeline buffer,
    // `None` when disabled — resolved in `serve` from
    // `[server.diagnostics] request_log` + the mode default.)
    db_health: Arc<db_health::DbProxyHealth>,
    request_log: Option<Arc<timeline::RequestLog>>,
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

    // HTTP/3 (UDP) is bound before the router is built so the `Alt-Svc`
    // advertisement can be attached only if the QUIC socket really came up.
    //
    // The address it defaults to is whatever terminates TLS over TCP:
    // `[server.tls] listen` when a separate HTTPS listener is configured,
    // otherwise `[server] listen`. Same authority and port as HTTPS, different
    // transport — which is exactly what an `Alt-Svc: h3=":<port>"` promises.
    let https_addr: SocketAddr = match config.server.tls.as_ref().and_then(|t| t.listen.as_ref()) {
        Some(raw) => raw.parse().context("invalid TLS listen address")?,
        None => addr,
    };
    let (http3_endpoint, alt_svc) = match http3::Http3Params::resolve(config, https_addr)? {
        Some(params) => {
            let endpoint = http3::build_endpoint(params.listen, &params.cert, &params.key)?;
            // Read the port back off the socket rather than trusting config:
            // with `listen = "…:0"` the OS picks it, and advertising `:0`
            // would send every client to a port that is not there.
            let bound = endpoint
                .local_addr()
                .context("HTTP/3 endpoint has no local address after binding")?;
            let max_age = config.server.http3.alt_svc_max_age;
            tracing::info!(
                listen = %bound,
                alt_svc_max_age = max_age,
                "HTTP/3 (QUIC) listening on UDP"
            );
            if max_age == 0 {
                tracing::warn!(
                    "[server.http3] alt_svc_max_age = 0 suppresses the Alt-Svc header — \
                     browsers will never discover HTTP/3 and will stay on TCP"
                );
            }
            (Some(endpoint), Some((bound.port(), max_age)))
        }
        None => {
            // add-config-knob: the two supporting knobs only mean anything when
            // `enabled = true`. Warn rather than let them read as effective.
            if config.server.http3.listen.is_some() {
                tracing::warn!(
                    "[server.http3] listen is ignored while enabled = false — \
                     no QUIC socket is bound"
                );
            }
            if config.server.http3.alt_svc_max_age != 86400 {
                tracing::warn!(
                    "[server.http3] alt_svc_max_age is ignored while enabled = false — \
                     no Alt-Svc header is advertised"
                );
            }
            (None, None)
        }
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

    let router = Arc::new({
        let router = Router::new(
            config,
            kv_store,
            multi_tenant_kv,
            metrics_handle,
            limiter.clone(),
            file_cache.clone(),
            worker_pool.clone(),
        )
        .with_middleware_chain(middleware_chain)
        // Expose the effective gossip node id to PHP (EPHPM_NODE_ID). When
        // clustering is on this is the runtime id -- distinct per node even
        // when `[cluster] node_id` is left empty (auto-derived per pod in
        // Kind). In single-node mode this is None, so Router keeps whatever it
        // derived from `[cluster] node_id`.
        .with_node_id(node_id)
        .with_db_health(db_health)
        .with_request_log(request_log);

        // Only advertise HTTP/3 once its UDP socket is confirmed bound.
        match alt_svc {
            Some((port, max_age)) => router.with_alt_svc(port, max_age),
            None => router,
        }
    });

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
        http3_endpoint,
    })
}

/// Pause after an `accept()` failure that is likely to repeat immediately.
///
/// Long enough that a wedged listener cannot spin a core, short enough that a
/// transient descriptor shortage clears without a visible stall.
const ACCEPT_BACKOFF_MS: u64 = 50;

/// Decide what to do about a failed `accept()`.
///
/// An accept error is never fatal to the server. Descriptor exhaustion
/// (`EMFILE`/`ENFILE`) and an aborted handshake (`ECONNABORTED`) are
/// transient, per-connection conditions; propagating either out of
/// [`accept_loop`] used to terminate the loop and shut the whole process
/// down over one bad connection.
///
/// Returns `Some(backoff)` when the caller should pause before accepting
/// again. Errors that clearly concern a single connection retry immediately;
/// everything else — including descriptor exhaustion, which has no stable
/// [`std::io::ErrorKind`] and would otherwise busy-loop — backs off first.
fn handle_accept_error(err: &std::io::Error, listener: &str) -> Option<Duration> {
    match err.kind() {
        std::io::ErrorKind::ConnectionAborted
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::Interrupted => {
            tracing::debug!(
                error = %err,
                "{listener} accept failed for one connection; continuing"
            );
            None
        }
        _ => {
            tracing::warn!(
                error = %err,
                backoff_ms = ACCEPT_BACKOFF_MS,
                "{listener} accept failed; backing off and continuing"
            );
            Some(Duration::from_millis(ACCEPT_BACKOFF_MS))
        }
    }
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
        http3_endpoint,
    } = listeners;

    // Track in-flight connections for graceful shutdown.
    let in_flight = Arc::new(AtomicUsize::new(0));

    // HTTP/3 accepts on its own task: QUIC connections arrive on a UDP socket
    // that has nothing to do with the TCP `accept()` calls below. It shares
    // `in_flight` so the drain loop at the end of this function waits for
    // in-progress HTTP/3 requests exactly as it does for TCP connections.
    let http3_shutdown = tokio::sync::watch::channel(false);
    let http3_task = http3_endpoint.map(|endpoint| {
        let router = Arc::clone(&router);
        let in_flight = Arc::clone(&in_flight);
        let mut rx = http3_shutdown.0.subscribe();
        tokio::spawn(async move {
            http3::accept_loop(endpoint, router, in_flight, async move {
                // `changed()` only errors if every sender is gone, which also
                // means shutdown; either way, stop accepting.
                let _ = rx.changed().await;
            })
            .await;
        })
    });

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
                let (stream, remote_addr) = match result {
                    Ok(conn) => conn,
                    Err(e) => {
                        if let Some(backoff) = handle_accept_error(&e, "HTTP") {
                            tokio::time::sleep(backoff).await;
                        }
                        continue;
                    }
                };
                // HTTP responses (JSON APIs, small pages, 304s) are frequently
                // sub-MSS; without nodelay, Nagle holds the response tail until
                // the client ACKs, which delayed-ACK defers ~40ms — surfacing as
                // p50 latency under keep-alive/concurrency. hyper's server does
                // not set this; the KV RESP listener already does for the same
                // reason (ephpm-kv server.rs).
                let _ = stream.set_nodelay(true);
                let guard = acquire_connection(&limiter, &stream, remote_addr).await;
                dispatch_main_connection(
                    stream, remote_addr, &tls_mode, tls_listener.is_some(),
                    redirect_http, conn, &router, guard, &in_flight,
                );
            }

            result = async {
                tls_listener.as_ref().expect("guarded by is_some").accept().await
            }, if tls_listener.is_some() => {
                let (stream, remote_addr) = match result {
                    Ok(conn) => conn,
                    Err(e) => {
                        if let Some(backoff) = handle_accept_error(&e, "HTTPS") {
                            tokio::time::sleep(backoff).await;
                        }
                        continue;
                    }
                };
                // Same rationale as the plain-HTTP accept above; set on the raw
                // TCP stream before the TLS handshake wraps it.
                let _ = stream.set_nodelay(true);
                let guard = acquire_connection(&limiter, &stream, remote_addr).await;
                dispatch_tls_connection(stream, remote_addr, &tls_mode, conn, &router, guard, &in_flight);
            }

            () = &mut shutdown => {
                tracing::info!("shutdown signal received, stopping server");
                break;
            }
        }
    }

    // Stop accepting new QUIC connections. In-flight HTTP/3 requests keep
    // running and are covered by the `in_flight` drain below.
    let _ = http3_shutdown.0.send(true);

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

    // Worker mode: wait (bounded) for the worker OS threads themselves to
    // finish retiring. They are detached, so nothing else joins them — and
    // `live` only reaches 0 after every worker has run
    // `PhpRuntime::worker_thread_shutdown()` (php_request_shutdown +
    // ts_free_thread on its own thread). php_embed_shutdown() must not run
    // while any of those TSRM entries are still live (issue #266), so give
    // the drain a real window before the caller proceeds to PHP teardown.
    if let Some(pool) = &worker_pool {
        let deadline = tokio::time::Instant::now() + shutdown_timeout;
        loop {
            let live = pool.live_count();
            if live == 0 {
                tracing::info!("all worker threads retired");
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    live_workers = live,
                    "shutdown timeout reached with worker threads still live — \
                     PHP teardown may be unsafe if a worker is wedged mid-request"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    // The QUIC endpoint sends CONNECTION_CLOSE to its peers as the accept loop
    // exits; give that task a bounded moment to finish so clients see a clean
    // close instead of a timeout.
    if let Some(task) = http3_task {
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
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

/// What [`start_kv_service`] hands back: the process-wide default store, the
/// per-vhost store when `sites_dir` is configured, and the RESP listener's
/// join handle.
///
/// Named rather than written inline so the signature stays under
/// `clippy::type_complexity`.
type KvService = (
    Arc<ephpm_kv::store::Store>,
    Option<ephpm_kv::multi_tenant::MultiTenantStore>,
    Option<tokio::task::JoinHandle<()>>,
);

/// Start the KV store with optional RESP server.
///
/// Returns the process-wide default [`Store`](ephpm_kv::store::Store), the
/// per-vhost [`MultiTenantStore`](ephpm_kv::multi_tenant::MultiTenantStore)
/// when `sites_dir` is configured, and the RESP listener's join handle.
///
/// The multi-tenant store is built **here and only here**. Its `sites` map is
/// what makes two vhost keyspaces distinct, so every consumer — the PHP path
/// (`Router` → `kv_bridge::set_site_store`) and the RESP listener alike — has
/// to be handed this same instance. Constructing a second one gives each
/// consumer its own lazily-populated map, and `ephpm_kv_set()` from PHP then
/// lands in a different `Store` than a Predis `SET` on the same vhost.
fn start_kv_service(config: &Config) -> anyhow::Result<KvService> {
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
    let store = ephpm_kv::store::Store::new(store_config.clone());

    // Wire the store into PHP native functions (ephpm_kv_get, etc.)
    ephpm_php::PhpRuntime::set_kv_store(&store);

    // Per-vhost stores inherit the `[kv]` block (memory_limit, eviction_policy,
    // compression) rather than `StoreConfig::default()` — otherwise every vhost
    // silently ran on the hardcoded 256 MiB / allkeys-lru / no-compression
    // defaults no matter what the operator configured.
    let multi_tenant = if config.server.sites_dir.is_some() {
        Some(ephpm_kv::multi_tenant::MultiTenantStore::new(Arc::clone(&store), store_config))
    } else {
        None
    };

    if !config.kv.redis_compat.enabled {
        tracing::debug!("KV store initialized (RESP server disabled)");
        return Ok((store, multi_tenant, None));
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

    // Hand the RESP listener the *same* multi-tenant handle the router gets,
    // so `AUTH <hostname> <derived>` resolves to the identical `Arc<Store>`
    // that PHP's `ephpm_kv_*` functions write through for that vhost. Only
    // wired when a secret exists: without one the listener has no way to
    // derive per-site credentials and stays on the shared default store
    // (warned about just above).
    let resp_multi_tenant = if secret.is_some() { multi_tenant.clone() } else { None };

    let store_for_resp = Arc::clone(&store);
    let handle = tokio::spawn(async move {
        match ephpm_kv::server::run(store_for_resp, server_config, resp_multi_tenant).await {
            Ok(()) => tracing::info!("KV RESP server stopped"),
            Err(e) => tracing::error!("KV RESP server error: {e:#}"),
        }
    });

    Ok((store, multi_tenant, Some(handle)))
}

/// Warn about every database listener bound to a non-loopback address.
///
/// None of the SQL frontends authenticate anyone. The `[db.mysql]` proxy
/// reads the client handshake and discards it without validating
/// credentials; the `[db.postgres]` proxy answers any startup message
/// with `AuthenticationOk`; litewire's `MySQL`/Hrana/`PostgreSQL`/TDS
/// frontends in front of embedded `SQLite` do no authentication either.
/// That is by design — the whole model assumes the listener is reachable
/// only by PHP running in this same process. A routable bind address
/// therefore publishes unauthenticated read/write access to the database
/// to anyone who can reach the port.
///
/// This warns rather than refusing to start. Unlike `[cluster] secret`
/// there is no credential an operator could configure to make the
/// listener safe, and binding `0.0.0.0` inside a container that is
/// firewalled by a network policy is a legitimate deployment.
fn warn_on_exposed_db_listeners(config: &Config) {
    // Defaults mirror the ones applied where each proxy is started below.
    if let Some(mysql) = &config.db.mysql {
        warn_if_db_listener_exposed(
            "[db.mysql] listen",
            mysql.listen.as_deref().unwrap_or("127.0.0.1:3306"),
        );
    }
    if let Some(pg) = &config.db.postgres {
        warn_if_db_listener_exposed(
            "[db.postgres] listen",
            pg.listen.as_deref().unwrap_or("127.0.0.1:5432"),
        );
    }
    if let Some(sqlite) = &config.db.sqlite {
        let proxy = &sqlite.proxy;
        warn_if_db_listener_exposed("[db.sqlite.proxy] mysql_listen", &proxy.mysql_listen);
        let optional = [
            ("[db.sqlite.proxy] hrana_listen", proxy.hrana_listen.as_deref()),
            ("[db.sqlite.proxy] postgres_listen", proxy.postgres_listen.as_deref()),
            ("[db.sqlite.proxy] tds_listen", proxy.tds_listen.as_deref()),
        ];
        for (key, addr) in optional {
            if let Some(addr) = addr {
                warn_if_db_listener_exposed(key, addr);
            }
        }
    }
}

/// Emit the exposure warning for one listener, if it is in fact exposed.
fn warn_if_db_listener_exposed(key: &str, addr: &str) {
    if let Some(exposure) = db_listen_exposure(addr) {
        tracing::warn!(
            "{key} = \"{addr}\" listens on {exposure}. The SQL wire frontends do NOT \
             authenticate clients — any host that can reach this port gets full read/write \
             access to the database. Bind a loopback address unless this port is firewalled \
             from untrusted networks."
        );
    }
}

/// Classify a listen address for the exposure warning.
///
/// Returns a description of how the address is exposed, or `None` when it
/// is loopback-only. Addresses that are not IP literals (`localhost:3306`,
/// `db.internal:3306`) also return `None`: classifying them would require
/// a DNS lookup at startup, and a wrong warning is worse than no warning.
fn db_listen_exposure(addr: &str) -> Option<&'static str> {
    let socket: SocketAddr = addr.trim().parse().ok()?;
    if socket.ip().is_loopback() {
        None
    } else if socket.ip().is_unspecified() {
        Some("all network interfaces")
    } else {
        Some("a non-loopback address")
    }
}

/// Start database proxies (`MySQL`, `PostgreSQL`, `TDS`, embedded `SQLite`).
///
/// # Startup ordering
///
/// The SQL proxies bind their listen sockets here and reach their upstreams
/// from background tasks, so nothing in this function blocks on a database
/// being reachable. That is load-bearing, not tidiness: the embedded-SQLite
/// branch below runs *after* the proxy branches, and a `[db.mysql]` proxy
/// pointed at `[db.sqlite]`'s own litewire listener used to exhaust its
/// entire connect budget against a listener this function had not bound yet,
/// then give up permanently (issue #226). The same inversion fixes every
/// deployment whose database is simply slower to start than ePHPm.
///
/// Errors from proxy startup — a malformed URL, a listen address that cannot
/// be bound — are now fatal rather than logged. A proxy that cannot bind its
/// port is a configuration error, and the previous behavior (log, continue,
/// leave the port dead) is precisely the failure mode this function is being
/// fixed for.
async fn start_db_proxies(
    config: &Config,
    cluster: Option<&Arc<ephpm_cluster::ClusterHandle>>,
    channel_handle: Option<&ephpm_cluster::ChannelHandle>,
    query_stats: &ephpm_query_stats::QueryStats,
    db_health: &db_health::DbProxyHealth,
) -> anyhow::Result<Vec<tokio::task::JoinHandle<()>>> {
    let mut handles = vec![];

    warn_on_exposed_db_listeners(config);

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

        let health = db_health
            .mysql()
            .cloned()
            .context("[db.mysql] is configured but no health handle was registered")?;

        handles.push(
            ephpm_db::mysql::spawn_deferred(
                &url,
                &listen,
                mysql_config.socket.clone(),
                pool_config,
                reset_strategy,
                replica_urls,
                rw_split,
                // Same collector the litewire paths hand to `TrackedBackend`, so
                // proxied and embedded queries land on one metrics surface.
                query_stats.clone(),
                health,
            )
            .await
            .context("failed to start MySQL proxy")?,
        );
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

        let health = db_health
            .postgres()
            .cloned()
            .context("[db.postgres] is configured but no health handle was registered")?;

        handles.push(
            ephpm_db::postgres::spawn_deferred(
                &url,
                &listen,
                pool_config,
                reset_strategy,
                replica_urls,
                rw_split,
                query_stats.clone(),
                health,
            )
            .await
            .context("failed to start PostgreSQL proxy")?,
        );
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

    // Embedded SQLite via litewire (Turso engine only as of v0.7.0).
    if let Some(sqlite_config) = &config.db.sqlite {
        validate_sqlite_engine(&sqlite_config.engine)?;
        warn_on_removed_sqlite_knobs(sqlite_config);

        if is_clustered_sqlite(sqlite_config, cluster.is_some()) {
            // Clustered SQLite replicates through the in-process Turso CDC
            // path over the cluster channel — no sqld sidecar. The channel
            // handle is guaranteed Some by `resolve_channel_features` (which
            // enables the `cdc` channel feature for exactly this
            // configuration); if it is None here, something reordered
            // startup wrong and `start_clustered_turso_cdc` fails loudly.
            turso_cdc::start_clustered_turso_cdc(
                sqlite_config,
                cluster,
                channel_handle,
                query_stats,
                &mut handles,
            )
            .await?;
        } else {
            start_single_node_sqlite(sqlite_config, query_stats, &mut handles).await?;
        }
    }

    Ok(handles)
}

/// Validate the `[db.sqlite].engine` knob.
///
/// As of v0.7.0 the only embedded SQLite-family engine is Turso: the
/// rusqlite backend and the sqld sidecar were removed. `"turso"` (the
/// default) is the only accepted value. The legacy `"sqlite"` / `"rusqlite"`
/// values are rejected with a migration message rather than silently
/// falling back to a now-absent backend (fail closed).
fn validate_sqlite_engine(engine: &str) -> anyhow::Result<()> {
    match engine {
        "turso" => Ok(()),
        "sqlite" | "rusqlite" => anyhow::bail!(
            "[db.sqlite] engine = \"{engine}\" was removed in v0.7.0. The embedded \
             SQLite-family engine is now Turso only — the rusqlite backend and the \
             sqld clustered sidecar were dropped. Remove the `engine` key (it now \
             defaults to \"turso\") or set engine = \"turso\". See the v0.7.0 upgrade \
             notes for database file-format details before upgrading production data."
        ),
        other => anyhow::bail!(
            "[db.sqlite] engine = \"{other}\" is not a valid engine; \
             the only supported value is \"turso\" (the default)"
        ),
    }
}

/// Warn on `[db.sqlite]` knobs that were removed in v0.7.0 but are still
/// parsed so upgrading configs do not hard-fail.
///
/// Honors the "no silent no-op config knob" rule: a knob that is set but no
/// longer does anything must be surfaced, not silently ignored.
fn warn_on_removed_sqlite_knobs(sqlite_config: &ephpm_config::SqliteConfig) {
    if let Some(sqld) = &sqlite_config.sqld {
        tracing::warn!(
            "[db.sqlite.sqld] was removed in v0.7.0 (the sqld sidecar and the rusqlite \
             backend were dropped). Clustered SQLite now replicates through the in-process \
             Turso CDC path — no sqld process. This section is ignored; delete it."
        );
        if sqld.write_permits.is_some() {
            tracing::warn!(
                "[db.sqlite.sqld] write_permits was removed in v0.7.0: it gated sqld's \
                 single writer, and Turso is MVCC with no single writer to admit against. \
                 Ignored."
            );
        }
    }
    if sqlite_config.replication.cdc_experimental {
        tracing::warn!(
            "[db.sqlite.replication] cdc_experimental was removed in v0.7.0: CDC is now the \
             only clustered SQLite replication path and is always active in clustered mode. \
             This knob is ignored; delete it."
        );
    }
}

/// Check if clustered SQLite mode should be used.
fn is_clustered_sqlite(sqlite_config: &ephpm_config::SqliteConfig, cluster_enabled: bool) -> bool {
    let role = sqlite_config.replication.role.as_str();
    role == "primary" || role == "replica" || (role == "auto" && cluster_enabled)
}

/// Start single-node SQLite via the in-process Turso engine.
///
/// As of v0.7.0 Turso is the only embedded SQLite-family engine
/// (`validate_sqlite_engine` rejects any other value upstream). Turso is a
/// factory — one `turso::Database` per file, a `turso::Connection` per
/// session — so there is no `sqlite3_open`-per-connect cost and no
/// handle-reuse pool to tune.
async fn start_single_node_sqlite(
    sqlite_config: &ephpm_config::SqliteConfig,
    query_stats: &ephpm_query_stats::QueryStats,
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
) -> anyhow::Result<()> {
    let db_path = &sqlite_config.path;
    let backend = litewire::Turso::open(db_path)
        .await
        .with_context(|| format!("failed to open database with Turso engine: {db_path}"))?;
    tracing::info!(
        path = %db_path,
        engine = "turso",
        "opened embedded database (single-node, Turso engine)"
    );
    spawn_single_node_litewire(
        sqlite_config,
        share_backend_with_php(tracked_backend::TrackedBackend::new(backend, query_stats.clone())),
        handles,
    );
    Ok(())
}

/// Erase `backend` behind an `Arc`, register that same instance as the
/// target of the PHP native `ephpm_db_*` functions, and return a
/// forwarding wrapper for the litewire builder.
///
/// This is how wire clients and in-process PHP bridge sessions come to
/// share ONE backend object — including the [`tracked_backend::TrackedBackend`]
/// wrapper passed in by every SQLite startup path, so bridge queries land
/// in query-stats exactly like wire queries. Called only from the
/// `[db.sqlite]` startup paths; when none of them runs, the bridge stays
/// unregistered and `ephpm_db_*` throws a clean PHP exception.
///
/// Must be called from within the server's tokio runtime (it pins
/// [`tokio::runtime::Handle::current`] for the bridge's sync-to-async
/// boundary).
pub(crate) fn share_backend_with_php(backend: impl litewire::backend::Backend) -> PhpSharedBackend {
    let shared: litewire::backend::SharedBackend = std::sync::Arc::new(backend);
    ephpm_php::PhpRuntime::set_db_backend(
        std::sync::Arc::clone(&shared),
        tokio::runtime::Handle::current(),
    );
    PhpSharedBackend(shared)
}

/// Forwarding wrapper handing an already-erased [`litewire::backend::SharedBackend`]
/// back to `LiteWire::new`, which insists on taking `impl Backend` by value.
/// The default `query`/`execute` convenience methods route through
/// `connect()`, whose returned connections carry the stats wrapper.
pub(crate) struct PhpSharedBackend(litewire::backend::SharedBackend);

#[async_trait::async_trait]
impl litewire::backend::Backend for PhpSharedBackend {
    async fn connect(
        &self,
    ) -> Result<Box<dyn litewire::backend::BackendConn>, litewire::backend::BackendError> {
        self.0.connect().await
    }
}

/// Wire the configured frontends onto a litewire builder and spawn it.
fn spawn_single_node_litewire(
    sqlite_config: &ephpm_config::SqliteConfig,
    backend: impl litewire::backend::Backend,
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    let mut builder = litewire::LiteWire::new(backend);
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

    if sqlite_config.proxy.max_connections > 0 {
        builder = builder.max_connections(sqlite_config.proxy.max_connections);
        tracing::info!(
            max_connections = sqlite_config.proxy.max_connections,
            "SQLite wire frontends: connection cap enabled"
        );
    }

    handles.push(tokio::spawn(async move {
        match builder.serve().await {
            Ok(()) => tracing::info!("litewire stopped"),
            Err(e) => tracing::error!("litewire error: {e:#}"),
        }
    }));
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

    /// `share_backend_with_php` must register the erased backend with the
    /// PHP bridge AND hand back a wrapper whose `connect()` reaches the
    /// same instance. Runs in stub mode — the bridge core is stub-safe.
    #[tokio::test]
    async fn share_backend_with_php_registers_and_forwards() {
        // Turso is the only embedded engine as of v0.7.0; open one on a
        // temp file (rusqlite's in-memory backend is no longer linked).
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("share.db");
        let backend = litewire::Turso::open(db_path.to_str().expect("utf-8 path"))
            .await
            .expect("open turso backend");
        let wrapper = share_backend_with_php(backend);
        assert!(ephpm_php::db_bridge::is_configured());

        // The wrapper still opens working sessions for the wire frontends.
        use litewire::backend::Backend as _;
        let conn = wrapper.connect().await.expect("connect through wrapper");
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", &[])
            .await
            .expect("execute through wrapper");
    }

    #[test]
    fn validate_sqlite_engine_accepts_turso() {
        assert!(validate_sqlite_engine("turso").is_ok());
    }

    #[test]
    fn validate_sqlite_engine_rejects_legacy_values_with_migration_message() {
        for legacy in ["sqlite", "rusqlite"] {
            let err = validate_sqlite_engine(legacy).expect_err("legacy engine must be rejected");
            let msg = err.to_string();
            assert!(msg.contains("removed in v0.7.0"), "message should flag removal: {msg}");
            assert!(msg.contains("turso"), "message should point at turso: {msg}");
        }
    }

    #[test]
    fn validate_sqlite_engine_rejects_unknown_value() {
        assert!(validate_sqlite_engine("postgres").is_err());
    }

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

    #[test]
    fn db_listen_exposure_allows_loopback() {
        assert!(db_listen_exposure("127.0.0.1:3306").is_none());
        assert!(db_listen_exposure("127.0.0.2:3306").is_none());
        assert!(db_listen_exposure("[::1]:3306").is_none());
        assert!(db_listen_exposure(" 127.0.0.1:3306 ").is_none());
    }

    #[test]
    fn db_listen_exposure_flags_wildcard_and_routable() {
        assert_eq!(db_listen_exposure("0.0.0.0:3306"), Some("all network interfaces"));
        assert_eq!(db_listen_exposure("[::]:3306"), Some("all network interfaces"));
        assert_eq!(db_listen_exposure("10.0.0.5:3306"), Some("a non-loopback address"));
        assert_eq!(db_listen_exposure("203.0.113.7:5432"), Some("a non-loopback address"));
    }

    #[test]
    fn db_listen_exposure_skips_addresses_it_cannot_classify() {
        // Hostnames would need a DNS lookup to classify; warning on a
        // guess would be worse than staying quiet.
        assert!(db_listen_exposure("localhost:3306").is_none());
        assert!(db_listen_exposure("db.internal:3306").is_none());
        assert!(db_listen_exposure("").is_none());
    }

    fn make_sqlite_config(role: &str) -> ephpm_config::SqliteConfig {
        ephpm_config::SqliteConfig {
            path: "test.db".into(),
            engine: "turso".into(),
            proxy: ephpm_config::SqliteProxyConfig::default(),
            sqld: None,
            replication: ephpm_config::ReplicationConfig {
                role: role.into(),
                primary_grpc_url: String::new(),
                ..ephpm_config::ReplicationConfig::default()
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

    // ── KV service wiring ───────────────────────────────────────────────────

    #[test]
    fn kv_service_has_no_multi_tenant_store_without_sites_dir() {
        let config = Config::default();
        let (_store, multi_tenant, handle) = start_kv_service(&config).expect("start kv service");
        assert!(multi_tenant.is_none(), "single-site mode needs no per-vhost stores");
        assert!(handle.is_none(), "[kv.redis_compat] is off by default");
    }

    #[test]
    fn kv_service_site_stores_inherit_the_kv_config() {
        // Per-vhost stores are templated from the `[kv]` block, not
        // `StoreConfig::default()`. They previously ran on the hardcoded
        // 256 MiB / allkeys-lru / no-compression defaults no matter what the
        // operator configured.
        let dir = tempfile::tempdir().expect("tempdir");
        let config = Config {
            server: ephpm_config::ServerConfig {
                sites_dir: Some(dir.path().to_path_buf()),
                ..ephpm_config::ServerConfig::default()
            },
            kv: ephpm_config::KvConfig {
                memory_limit: "8MB".to_string(),
                ..ephpm_config::KvConfig::default()
            },
            ..Config::default()
        };

        let (default_store, multi_tenant, _handle) =
            start_kv_service(&config).expect("start kv service");
        let multi_tenant = multi_tenant.expect("sites_dir set, so a per-vhost store exists");

        assert_eq!(default_store.config().memory_limit, 8 * 1024 * 1024);
        assert_eq!(
            multi_tenant.get_site_store("blog.example.com").config().memory_limit,
            8 * 1024 * 1024,
            "site stores must inherit [kv] memory_limit"
        );
    }

    #[test]
    fn kv_service_multi_tenant_clone_shares_site_stores() {
        // The handle handed to the router and the one handed to the RESP
        // listener are clones of one instance, so a vhost resolves to the same
        // `Arc<Store>` on both paths. Two `MultiTenantStore::new` calls would
        // not — that was the split-keyspace bug.
        let dir = tempfile::tempdir().expect("tempdir");
        let config = Config {
            server: ephpm_config::ServerConfig {
                sites_dir: Some(dir.path().to_path_buf()),
                ..ephpm_config::ServerConfig::default()
            },
            ..Config::default()
        };

        let (_store, multi_tenant, _handle) = start_kv_service(&config).expect("start kv service");
        let router_side = multi_tenant.expect("sites_dir set, so a per-vhost store exists");
        let resp_side = router_side.clone();

        let a = router_side.get_site_store("blog.example.com");
        let b = resp_side.get_site_store("blog.example.com");
        assert!(Arc::ptr_eq(&a, &b), "PHP and RESP must share one store per vhost");
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
        Arc::new(Router::new(&config, store, None, None, None, None, None))
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

    // ── accept() error handling ──────────────────────────────────────

    /// An accept error must never be fatal. Both of these used to be
    /// `result.context(..)?` out of `accept_loop`, so one aborted handshake
    /// or a momentary descriptor shortage took the whole process down.
    #[test]
    fn aborted_handshake_retries_without_backoff() {
        let err = std::io::Error::from(std::io::ErrorKind::ConnectionAborted);
        assert_eq!(handle_accept_error(&err, "HTTP"), None);
    }

    #[test]
    fn unrecognised_accept_error_backs_off_instead_of_spinning() {
        // Descriptor exhaustion (EMFILE/ENFILE) has no stable `ErrorKind`, so
        // it lands in the catch-all arm. Retrying instantly would busy-loop a
        // core while the process is already degraded.
        let err = std::io::Error::other("simulated descriptor exhaustion");
        assert_eq!(
            handle_accept_error(&err, "HTTP"),
            Some(Duration::from_millis(ACCEPT_BACKOFF_MS))
        );
    }
}
