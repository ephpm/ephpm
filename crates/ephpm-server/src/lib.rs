pub mod acme;
pub mod body;
pub mod db_health;
pub mod dns01;
pub mod dns01_digitalocean;
pub mod dns01_google;
pub mod dns01_linode;
pub mod dns01_route53;
pub mod file_cache;
pub mod fpm_pool;
pub mod http3;
mod idle;
pub mod metrics;
pub mod middleware;
pub mod opcache;
#[cfg(feature = "otlp")]
pub mod otlp;
pub mod privdrop;
pub mod proxy;
pub mod rate_limit;
pub mod router;
pub mod screened_backend;
pub mod site_backends;
mod site_overrides;
pub mod site_wire_auth;
pub mod sql_forward;
pub mod static_files;
pub mod stream_compress;
pub mod tenant_ebpf;
mod timeline;
pub mod tls;
pub mod tracked_backend;
pub mod turso_cdc;
pub mod turso_cdc_metrics;
pub mod websocket;
pub mod worker_pool;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    // Install the process-wide rustls crypto provider before anything can
    // build a TLS config. ePHPm's own listeners name the provider explicitly
    // and do not need this, but third-party rustls users in the tree
    // (hyper-rustls behind the Prometheus push gateway, for one) call the
    // bare `builder()` and consult the process default. Installing it here,
    // deliberately and once, is what keeps every TLS config in the process on
    // the same provider. Idempotent — see `tls::install_default_crypto_provider`.
    tls::install_default_crypto_provider();

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
        // The two lanes are two PHASES, and saying so at startup is the only
        // way `order` is not a quiet lie: native modules run before the request
        // body is read and outside PHP; `php:` mounts run inside the PHP
        // request, after the body, immediately before the application script.
        // `order` sorts within each phase and cannot interleave them.
        if chain.has_php_mounts() {
            tracing::info!(
                php_mounts = ?chain.php_mount_names(),
                "PHP middleware mounted (EXPERIMENTAL) — these run INSIDE the PHP request, \
                 after the request body has been read and after every native module, so they \
                 cannot reject before the body transfer the way a native module can"
            );
            chain.check_php_mount_scripts(
                &config.server.document_root,
                config.server.sites_dir.is_some(),
            )?;
        }
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
        // In multi-tenant mode the data plane must route per-site keys into
        // the owning vhost's store (the large-value counterpart of the gossip
        // applier's routing); otherwise a tenant's large value would land in
        // the receiving node's GLOBAL keyspace. Single-keyspace nodes keep the
        // original listener exactly as before.
        let data_plane_sites = multi_tenant_kv.clone();
        tokio::spawn(async move {
            let result = match data_plane_sites {
                Some(sites) => {
                    ephpm_cluster::data_plane::serve_multi_tenant(
                        data_plane_store,
                        sites,
                        data_port,
                        data_plane_cipher,
                    )
                    .await
                }
                None => {
                    ephpm_cluster::data_plane::serve(data_plane_store, data_port, data_plane_cipher)
                        .await
                }
            };
            if let Err(e) = result {
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

        // Per-vhost keyspaces replicate too. Each site's Store is created
        // lazily (on a request, a RESP AUTH, or an inbound replicated write),
        // so the replicator is installed by a factory at creation time rather
        // than up-front — there is no window in which a vhost's writes are
        // silently node-local. A site replicator namespaces that vhost's keys
        // on the wire (`site_namespace`) and materializes into that vhost's own
        // store, so tenant keyspaces stay isolated on every node.
        if let Some(sites) = &multi_tenant_kv {
            let factory_clustered = Arc::clone(&clustered);
            let factory_applied = Arc::clone(&applied);
            // Captured HERE, in async context, and moved into the factory. The
            // factory itself runs on a request thread during a site's first
            // access, where `Handle::current()` would panic off a runtime
            // thread — and blocking there would stall dispatch.
            let factory_handle = tokio::runtime::Handle::current();
            let factory: ephpm_kv::multi_tenant::SiteReplicatorFactory =
                Arc::new(move |site: &str, store: &Arc<ephpm_kv::store::Store>| {
                    // Pure construction over the store handed in — the factory
                    // never resolves a store itself, so it cannot re-enter the
                    // registry that is mid-creation for this very site.
                    ephpm_cluster::SiteKvReplicator::new(
                        Arc::clone(&factory_clustered),
                        Arc::clone(store),
                        site,
                        factory_handle.clone(),
                        Arc::clone(&factory_applied),
                    ) as Arc<dyn ephpm_kv::store::Replicator>
                });
            sites.set_replicator_factory(Some(factory));
            tracing::info!("clustered KV replication enabled for per-vhost keyspaces");
        }

        // Materialize REMOTE gossip-tier writes into this node's local
        // Store so raw-store readers (RESP GET, PHP native functions, the
        // OPcache watcher) see cluster writes; the origin node materializes
        // synchronously inside the replicator. In multi-tenant mode an
        // enveloped key is routed into its own vhost's store instead.
        ephpm_cluster::clustered_store::start_gossip_applier_multi_tenant(
            &cluster_handle,
            Arc::clone(&kv_store),
            multi_tenant_kv.clone(),
            applied,
        )
        .await;

        tracing::info!(
            small_key_threshold = config.cluster.kv.small_key_threshold,
            replication_factor = config.cluster.kv.replication_factor,
            replication_mode = %config.cluster.kv.replication_mode,
            per_vhost = multi_tenant_kv.is_some(),
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

    // Per-site database registry (secure multi-tenancy): build and register it
    // with the PHP `ephpm_db_*` bridge BEFORE the HTTP listeners bind, so the
    // bridge is wired before any request can run. Fails closed if `[db.sqlite]
    // dir` is missing in multi-site mode.
    //
    // `Some` also carries the per-site MySQL credentials: the wire listener
    // verifies against them and the router injects them, so both are handed
    // this one value rather than deriving their own.
    let (per_site_wire_auth, per_site_cluster) = match wire_per_site_db(
        &config,
        &query_stats,
        cluster_handle.as_ref(),
        channel_handle.as_ref(),
    )? {
        Some((auth, cluster_wiring)) => (Some(auth), cluster_wiring),
        None => (None, None),
    };

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

    // Start the multi-tenant MySQL listener BEFORE the router, so the router
    // only ever injects `DB_*` credentials for an endpoint that is actually
    // bound. A bind failure here is fatal (same contract as the other DB
    // proxies) rather than leaving every tenant's `pdo_mysql` pointed at a
    // dead port with no indication why.
    let mut per_site_wire_handles = Vec::new();
    // The `Some`/`Some` arm is the only reachable one when an auth exists —
    // `wire_per_site_db` returns `Some` only with `[db.sqlite]` present — but
    // pairing them here avoids asserting that in a way that could panic.
    let per_site_db_wire = match (per_site_wire_auth, config.db.sqlite.as_ref()) {
        // `[db.sqlite.proxy] mysql_wire_enabled = false`: bridge-only mode. The
        // per-site registry and `ephpm_db_*` bridge were already wired up by
        // `wire_per_site_db` above; here we deliberately skip the wire FRONTEND
        // (no `:3306` bind) and hand the router `None` so it advertises no
        // `DB_HOST`/`DB_PORT`/`DB_USER`/`DB_PASSWORD` for an endpoint that does
        // not exist. In-process database access via `ephpm_db_*` is unaffected.
        (Some(_auth), Some(sqlite)) if !per_site_wire_enabled(sqlite) => {
            tracing::info!(
                "per-site MySQL wire listener DISABLED ([db.sqlite.proxy] mysql_wire_enabled = \
                 false): not binding {:?}. Per-site databases are reachable only through the \
                 in-process ephpm_db_* bridge (no pdo_mysql); no DB_* credentials are injected.",
                sqlite.proxy.mysql_listen
            );
            None
        }
        (Some(auth), Some(sqlite)) => {
            let listen = start_per_site_wire(sqlite, &auth, &mut per_site_wire_handles).await?;
            Some((auth, listen))
        }
        _ => None,
    };

    // Shared "am I the writable SQLite target?" view, exposed at
    // `/_ephpm/primary` for active-passive load-balancer routing. Starts `true`
    // (a standalone/non-clustered node is trivially writable); in
    // clustered-SQLite mode `start_db_proxies` hands this exact `Arc` to the
    // CDC election path, which flips it on every role change. Built before the
    // router so the same handle reaches both the request path (via
    // `with_primary_view`) and the election (via `start_db_proxies`).
    let primary_view = Arc::new(AtomicBool::new(true));

    let listeners = bind_listeners(
        &config,
        kv_store,
        multi_tenant_kv,
        metrics_handle,
        middleware_chain,
        effective_node_id,
        Arc::clone(&db_health),
        request_log,
        per_site_db_wire,
        Arc::clone(&primary_view),
    )
    .await?;

    let _db_handles = start_db_proxies(
        &config,
        cluster_handle.as_ref(),
        channel_handle.as_ref(),
        &query_stats,
        &db_health,
        primary_view,
        per_site_cluster,
    )
    .await?;
    let _per_site_wire_handles = per_site_wire_handles;

    // Everything root is needed for is now done: privileged ports are bound,
    // DB proxies and per-site wire listeners are up, the generated php.ini has
    // been read at MINIT, and ACME/sqlite/vhost directories exist. Drop the
    // whole process to the unprivileged uid (if configured) before we accept a
    // single request. No PHP has run yet, so no request-carrying thread can
    // race the process-wide credential change.
    privdrop::drop_privileges(&config).context("failed to drop privileges")?;

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
    // so there is no engine gate. Per-site clustered mode
    // (`is_per_site_clustered`) is a strict subset of `is_clustered_sqlite`,
    // so it is already covered here — its CDC/snapshot streams ride the same
    // channel — and needs no separate flag.
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
    // Per-site `pdo_mysql` credentials, with the address of the listener that
    // accepts them. `Some` only in per-site mode *and* only once that listener
    // is confirmed bindable — so the router never advertises a database
    // endpoint that does not exist.
    per_site_db_wire: Option<(site_wire_auth::SiteWireAuth, String)>,
    // Shared clustered-SQLite "am I the primary?" view for `/_ephpm/primary`
    // (active-passive LB routing). Constant `true` for a standalone node; the
    // CDC election path flips it in clustered mode.
    primary_view: Arc<AtomicBool>,
) -> anyhow::Result<Listeners> {
    let addr: SocketAddr = config.server.listen.parse().context("invalid listen address")?;

    // `[server.limits]` resolved against the `[server] preview` preset:
    // explicit operator values win; unset fields take the preview defaults
    // when preview mode is on, the regular all-off defaults otherwise.
    let limits = config.server.effective_limits();
    log_preview_preset(config, &limits);
    log_overload_policy(config);
    let limiter = {
        let l = rate_limit::Limiter::new(limits);
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
        // DNS-01 wildcard lane (opt-in via `challenge = "dns-01"`). Checked
        // before the general ACME arm because it is the more specific case.
        // It shares the TLS *serving* path with manual mode: the resolver
        // inside the acceptor is hot-swapped by the renewal task, so no ACME
        // ClientHello inspection is needed (the challenge is answered over DNS,
        // not on the TLS socket).
        Some(tls_config) if tls_config.is_dns01() => {
            let acme_store =
                if config.cluster.enabled { Some(Arc::clone(&kv_store)) } else { None };
            let setup =
                dns01::start_dns01_acme(tls_config, acme_store, Some(&config.cluster.node_id))?;
            TlsMode::Manual(setup.acceptor)
        }
        Some(tls_config) if tls_config.is_acme() => {
            let acme_store =
                if config.cluster.enabled { Some(Arc::clone(&kv_store)) } else { None };
            let setup = acme::start_acme(tls_config, acme_store, Some(&config.cluster.node_id))?;
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

        if config.php.fpm_engine == ephpm_config::FpmEngine::Pool {
            tracing::warn!(
                "[php] fpm_engine = \"pool\" is ignored in worker mode — the \
                 persistent worker pool already owns concurrency here"
            );
        }

        let (worker_count, wc_source) = config.php.effective_worker_count_with_source();
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
        // Historical note: worker_count used to be forced to 1 on Windows on
        // the belief that Windows builds were NTS (single PHP context). The
        // Windows php-sdk's `php8embed.lib` is in fact ZTS (#326) — same
        // TSRM-per-thread model as Linux/macOS — and multi-worker mode was
        // verified behaviorally on Windows (3 workers, overlapping wall-clock
        // sleeps + fatal/recycle), so the clamp was removed.

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
            config.php.admission,
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

    // Native WebSockets (experimental). `None` when `[server.websocket]` is
    // disabled, which also means the PHP bridge stays unwired: `ephpm_ws_*`
    // then throws "not enabled" instead of silently pretending to deliver.
    let websocket = crate::websocket::WsRuntime::new(&config.server.websocket).map(Arc::new);
    if let Some(ref runtime) = websocket {
        ephpm_php::PhpRuntime::set_ws_registry(Arc::clone(&runtime.registry));
    }

    // Per-vhost eBPF network policy ([server.tenant_network] ebpf_policy).
    // Load + attach BEFORE the router is built, then hand the Arc to it. Fail
    // closed: with ebpf_policy = true a load/attach, range, or overlap failure
    // aborts startup — ePHPm must never come up with the policy the operator
    // asked for silently absent (docs-must-match-code).
    let tenant_ebpf: Option<Arc<tenant_ebpf::TenantEbpf>> =
        if config.server.tenant_network.ebpf_policy {
            let port_of = |addr: &str| -> Option<u16> {
                addr.rsplit_once(':').and_then(|(_, p)| p.trim().parse().ok())
            };
            let range = config
                .server
                .tenant_network
                .parse_range()
                .map_err(|e| anyhow::anyhow!("[server.tenant_network] {e}"))?;
            // The sidecar range must not overlap the kernel ephemeral range, or an
            // outbound source port could collide with a handed-out real port.
            tenant_ebpf::TenantEbpf::assert_no_ephemeral_overlap(range)?;
            // ePHPm's own loopback infra ports every tagged vhost may reach: the
            // stock pdo_mysql wire listener and the KV RESP listener.
            let mut infra_ports: Vec<u16> = Vec::new();
            if let Some((_, listen)) = per_site_db_wire.as_ref()
                && let Some(p) = port_of(listen)
            {
                infra_ports.push(p);
            }
            if config.kv.redis_compat.enabled
                && let Some(p) = port_of(&config.kv.redis_compat.listen)
            {
                infra_ports.push(p);
            }
            let handle = tenant_ebpf::TenantEbpf::load_and_attach(
                config.server.tenant_network.cgroup_path.as_deref(),
                &infra_ports,
            )
            .context(
                "loading the eBPF per-vhost network policy ([server.tenant_network] ebpf_policy = \
             true). Requires Linux >= 5.10 with CONFIG_CGROUP_BPF + BTF and CAP_BPF + \
             CAP_NET_ADMIN. It also needs a raised RLIMIT_MEMLOCK (LimitMEMLOCK=infinity in the \
             systemd unit): BPF maps are charged against memlock and the loader cannot raise the \
             limit itself under NoNewPrivileges (no CAP_SYS_RESOURCE), so its absence surfaces as \
             'failed to create map ... Operation not permitted' even when the capabilities are \
             correct. Also confirm any external nft egress floor drops its blanket loopback-DROP \
             for the ePHPm cgroup, or every sidecar connect will fail.",
            )?;
            handle.fill_pool(range, config.server.tenant_network.max_sidecar_ports_per_vhost)?;
            tracing::info!(
                cgroup = ?config.server.tenant_network.cgroup_path,
                sidecar_port_range = %config.server.tenant_network.sidecar_port_range,
                max_per_vhost = config.server.tenant_network.max_sidecar_ports_per_vhost,
                ?infra_ports,
                "tenant_network: eBPF per-vhost policy loaded and attached"
            );
            Some(handle)
        } else {
            None
        };

    // `Router::share` rather than `Arc::new`: a WebSocket session outlives the
    // request that created it and keeps dispatching PHP events through this
    // router, so the Arc has to be reachable from the router itself.
    let router = {
        let router = Router::new(
            config,
            kv_store,
            multi_tenant_kv,
            metrics_handle,
            limiter.clone(),
            file_cache.clone(),
            worker_pool.clone(),
        )
        .with_tenant_ebpf(tenant_ebpf)
        .with_middleware_chain(middleware_chain)
        // Expose the effective gossip node id to PHP (EPHPM_NODE_ID). When
        // clustering is on this is the runtime id -- distinct per node even
        // when `[cluster] node_id` is left empty (auto-derived per pod in
        // Kind). In single-node mode this is None, so Router keeps whatever it
        // derived from `[cluster] node_id`.
        .with_node_id(node_id)
        .with_db_health(db_health)
        .with_primary_view(primary_view)
        .with_websocket(websocket)
        .with_request_log(request_log);

        let router = match per_site_db_wire {
            Some((auth, listen)) => router.with_per_site_db_wire(auth, listen),
            None => router,
        };

        // Only advertise HTTP/3 once its UDP socket is confirmed bound.
        match alt_svc {
            Some((port, max_age)) => router.with_alt_svc(port, max_age),
            None => router,
        }
        .share()
    };

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
                let guard = match acquire_connection(&limiter, &stream, remote_addr).await {
                    ConnAdmission::Admit(guard) => guard,
                    // Over the connection cap: the 503 was written; dropping
                    // the stream closes it instead of serving it anyway.
                    ConnAdmission::Shed => continue,
                };
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
                let guard = match acquire_connection(&limiter, &stream, remote_addr).await {
                    ConnAdmission::Admit(guard) => guard,
                    ConnAdmission::Shed => continue,
                };
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

    // FPM pool engine (`[php] fpm_engine = "pool"`): same contract — close the
    // dispatch queue so pool threads finish any in-flight request and exit.
    // Mutually exclusive with `worker_pool` (pool engine is fpm-mode only).
    let fpm_pool = router.fpm_pool();
    if let Some(pool) = &fpm_pool {
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

    // FPM pool engine: same #266 wait — `live` reaches 0 only after every pool
    // thread has released its TSRM slot (worker_thread_shutdown), so
    // php_embed_shutdown() is safe once it does.
    if let Some(pool) = &fpm_pool {
        let deadline = tokio::time::Instant::now() + shutdown_timeout;
        loop {
            let live = pool.live_count();
            if live == 0 {
                tracing::info!("all fpm pool threads retired");
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    live_threads = live,
                    "shutdown timeout reached with fpm pool threads still live — \
                     PHP teardown may be unsafe if a thread is wedged mid-request"
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

/// Log, at startup, exactly what the `[server] preview` preset did — which
/// `[server.limits]` fields it supplied and which the operator set — plus the
/// marker header. Never silent (the no-silent-knob rule, same pattern as the
/// multi-tenant hardening preset log).
fn log_preview_preset(config: &ephpm_config::Config, limits: &ephpm_config::ResolvedLimits) {
    let server = &config.server;
    if !server.preview {
        return;
    }
    let mut applied = server.preview_preset_applied();
    // The preset reaches outside `[server.limits]` for exactly one knob: the
    // request-granularity shed policy (issue #301). Reported in the same list so
    // "what did preview change?" has one answer. `log_overload_policy` then says
    // what the resulting policy actually does on the active engine.
    if config.overload_policy_from_preview_preset() {
        applied.push(("php.overload_policy", "shed".to_string()));
    }
    let preset_supplied =
        applied.iter().map(|(key, value)| format!("{key}={value}")).collect::<Vec<_>>().join(", ");
    tracing::info!(
        max_connections = limits.max_connections,
        per_ip_max_connections = limits.per_ip_max_connections,
        per_ip_rate = limits.per_ip_rate,
        per_ip_burst = limits.per_ip_burst,
        per_site_rate = limits.per_site_rate,
        per_site_burst = limits.per_site_burst,
        "preview mode ON: every response carries X-Ephpm-Preview: 1; \
         preset supplied [{}]; every other [server.limits] value above was \
         set explicitly by the operator (explicit values always win, \
         including explicit 0 = that limit off)",
        if preset_supplied.is_empty() {
            "nothing — all limits operator-set"
        } else {
            &preset_supplied
        },
    );
}

/// Log what `[php] overload_policy` resolved to, where that came from, and —
/// the part that actually matters — what it will do on the *active* execution
/// engine.
///
/// Shedding is not a property of the knob alone: it needs an admission queue to
/// reject from, and on the default `spawn_blocking` engine that queue exists
/// only when `[php] workers` is set. Setting the policy without it changes
/// nothing, which is exactly the silent-no-op this project forbids — so that
/// combination WARNs, naming the two ways out.
fn log_overload_policy(config: &ephpm_config::Config) {
    use ephpm_config::OverloadPolicy;

    let policy = config.effective_overload_policy();
    if policy == OverloadPolicy::Wait {
        // The historical behaviour and the default. Nothing to announce; an
        // operator who explicitly chose it under preview is opting out of the
        // preset, which the preview log already covers.
        return;
    }

    let source = if config.overload_policy_from_preview_preset() {
        "[server] preview preset"
    } else {
        "[php] overload_policy"
    };
    let shed_after_ms = config.php.shed_after_ms;

    if config.php.is_worker_mode() {
        tracing::warn!(
            source,
            "[php] overload_policy = \"shed\" is ignored in worker mode — the persistent \
             worker pool bounds concurrency with its own queue ([php] worker_backlog) and \
             answers 504 on a starved queue"
        );
        return;
    }

    if config.php.is_pool_engine() {
        tracing::info!(
            source,
            shed_after_ms,
            backlog = config.php.effective_worker_backlog(),
            pool_threads = config.php.effective_worker_count(),
            "load shedding ON: a PHP request that cannot get a pool slot within \
             shed_after_ms of a full dispatch backlog is answered 503 + Retry-After \
             instead of queueing"
        );
    } else if config.php.workers > 0 {
        tracing::info!(
            source,
            shed_after_ms,
            workers = config.php.workers,
            "load shedding ON: a PHP request that cannot get one of the [php] workers \
             slots within shed_after_ms is answered 503 + Retry-After instead of queueing"
        );
    } else {
        tracing::warn!(
            source,
            "load shedding is requested but INERT: the default [php] fpm_engine = \
             \"spawn_blocking\" sheds against the [php] workers semaphore, and workers = 0 \
             means there is no admission queue to reject from (tokio's blocking queue is \
             unbounded and its entries cannot be withdrawn). Set [php] workers to a \
             concurrency cap, or [php] fpm_engine = \"pool\", for shedding to take effect"
        );
    }
}

/// Outcome of the accept-time connection-limit check.
enum ConnAdmission {
    /// Serve the connection. The guard is `Some` when a limiter is active
    /// (it holds the connection slot until drop) and `None` when no limiter
    /// is configured.
    Admit(Option<rate_limit::ConnectionGuard>),
    /// Over the global or per-IP connection cap: a raw 503 was already
    /// written; the caller must drop the stream, not serve it.
    Shed,
}

/// Try to acquire a connection slot. On rejection, send a raw 503 and return
/// [`ConnAdmission::Shed`] so the accept loop drops the connection — the shed
/// must actually shed. (It previously returned the same `None` for "no
/// limiter" and "rejected", so rejected connections were served anyway after
/// the 503 bytes, and the cap protected nothing.)
async fn acquire_connection(
    limiter: &Option<Arc<rate_limit::Limiter>>,
    stream: &TcpStream,
    remote_addr: SocketAddr,
) -> ConnAdmission {
    let Some(l) = limiter else {
        return ConnAdmission::Admit(None);
    };
    match l.try_acquire_connection(remote_addr.ip()) {
        Some(guard) => ConnAdmission::Admit(Some(guard)),
        None => {
            tracing::debug!(%remote_addr, "connection rejected (limit reached)");
            // Best-effort raw HTTP response — the TLS handshake hasn't happened yet
            // for TLS connections, so this only works for plain HTTP.
            //
            // The stream was just accepted and has never been polled, so
            // tokio's cached readiness is empty and a bare `try_write` often
            // returns `WouldBlock` — silently skipping the 503 (the #299 E2E
            // pin flaked on exactly this: bare close, no bytes). Await
            // writability first; on a freshly accepted socket the send
            // buffer is empty, so this resolves on the next reactor tick.
            // The timeout is a belt-and-braces bound so a shed connection
            // can never stall the accept loop.
            let write_503 = async {
                loop {
                    if stream.writable().await.is_err() {
                        break;
                    }
                    match stream.try_write(
                        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    ) {
                        // Spurious readiness — re-arm and retry (bounded by
                        // the timeout below).
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        _ => break,
                    }
                }
            };
            let _ = tokio::time::timeout(Duration::from_millis(100), write_503).await;
            ConnAdmission::Shed
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
///
/// # The main listener is plain HTTP whenever a separate TLS listener exists
///
/// `[server.tls] listen` means, per its documented contract, *"`server.listen`
/// serves HTTP and this address serves HTTPS"*. So the moment a separate TLS
/// listener is bound, this listener speaks plain HTTP — whatever [`TlsMode`]
/// the server is in. `redirect_http` then chooses only what that plain-HTTP
/// listener *says*: a 301 to HTTPS, or the site itself.
///
/// This used to be gated on `has_tls_listener && redirect_http`, and
/// `redirect_http` defaults to `false`. A config with both `[server] listen`
/// and `[server.tls] listen` but no redirect therefore fell through to the
/// `tls_mode` match below and TLS-wrapped the *plain* listener: a plaintext
/// `GET` got a TLS alert record (`15 03 03 ...`) instead of a response, so
/// every plain-HTTP client — browsers, load-balancer health checks — failed to
/// connect. Serving HTTPS on both listeners is never a valid reading of the
/// contract, so there is no `tls_mode` arm here at all.
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

    if has_tls_listener {
        let router = Arc::clone(router);
        tokio::spawn(async move {
            let _guard = guard; // held until connection closes
            let _flight = flight_guard;
            if redirect_http {
                serve_http_redirect(stream, remote_addr, conn).await;
            } else {
                serve_connection(stream, router, remote_addr, false, conn).await;
            }
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
        && !err.is_incomplete_message()
    {
        tracing::debug!(%remote_addr, %err, "redirect connection error");
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

    // Fail closed: in multi-tenant mode a RESP listener with no master secret
    // cannot derive per-site AUTH, so every tenant (and anything else that can
    // reach the listener) would talk to the single shared default store
    // unauthenticated — cross-tenant KV read/write. `Config::validate` already
    // rejects this before `serve()`, but guard here too so any embedding or
    // test path that skips validation still refuses rather than silently
    // exposing every site's KV.
    if config.server.sites_dir.is_some() && !config.kv.secret_is_set() {
        anyhow::bail!(
            "refusing to start the KV RESP listener: [kv.redis_compat] enabled \
             with [server] sites_dir (multi-tenant vhosting) but no [kv] secret \
             — the listener would serve one shared KV store to every tenant \
             with no authentication. Set [kv] secret to enable per-site RESP \
             AUTH scoping, or disable the listener with [kv.redis_compat] \
             enabled = false."
        );
    }

    // Hand the RESP listener the *same* multi-tenant handle the router gets,
    // so `AUTH <hostname> <derived>` resolves to the identical `Arc<Store>`
    // that PHP's `ephpm_kv_*` functions write through for that vhost. In
    // single-site mode there is no per-vhost store, so this is `None` and the
    // listener serves the (correct) shared store; the multi-tenant + no-secret
    // case never reaches here (rejected just above).
    let resp_multi_tenant = if config.kv.secret_is_set() { multi_tenant.clone() } else { None };

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
    // Shared "am I the writable SQLite target?" view for `/_ephpm/primary`.
    // In clustered-SQLite mode this is handed to the CDC election path, which
    // flips it on every role change; in every other mode it is left at its
    // constant `true` (a standalone node is trivially writable).
    primary_view: Arc<AtomicBool>,
    // Per-site clustered replication wiring, built in `serve()` alongside the
    // registry so the resolver is registered before any request. `Some` only
    // in per-site clustered mode; consumed by the CDC path below.
    per_site_cluster: Option<PerSiteClusterWiring>,
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

        // One unambiguous line naming the resolved mode, emitted at the single
        // point where the branch is taken. Operators (and the benchmark gates)
        // assert on this rather than inferring the mode from the presence or
        // absence of other log lines: the modes differ in tenancy and in
        // durability, and "which one am I actually running?" must never be a
        // question you answer by elimination.
        tracing::info!(
            mode = sqlite_mode_label(config, cluster.is_some()),
            "embedded SQLite mode selected"
        );

        if is_per_site_clustered(config, cluster.is_some()) {
            // Per-site CLUSTERED mode: one replicated Turso database per
            // virtual host, HRW ownership. Tested BEFORE `is_clustered_sqlite`
            // because it is strictly more specific (it also satisfies
            // `is_clustered_sqlite`, which is what already enabled the `cdc`
            // channel feature for it). The registry + resolver + wire listener
            // were wired in `serve()`; here we start the replication plane.
            let wiring = per_site_cluster.context(
                "per-site clustered mode is active but its replication wiring is missing \
                 (startup ordering bug: wire_per_site_db should have produced it)",
            )?;
            turso_cdc::start_clustered_per_site_turso(
                sqlite_config,
                wiring.dir,
                cluster,
                channel_handle,
                wiring.site_events,
                wiring.registry,
                &mut handles,
                &primary_view,
            )
            .await?;
        } else if is_clustered_sqlite(sqlite_config, cluster.is_some()) {
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
                primary_view,
            )
            .await?;
        } else if is_per_site_sqlite(config, cluster.is_some()) {
            // Per-site (multi-tenant) mode. Nothing to do here: `serve()`
            // already started the one multi-tenant MySQL listener (before the
            // router, so credentials are never injected for an unbound
            // endpoint) via `start_per_site_wire`.
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

/// The resolved embedded-SQLite mode, as a stable label for the startup log.
///
/// Mirrors the branch order in `start_db_proxies` exactly — most specific
/// first — so the logged mode can never disagree with the mode that runs. The
/// four values are a committed interface: operators and benchmark gates assert
/// on them.
///
/// * `per-site-clustered` — one **replicated** database per virtual host.
/// * `clustered` — one replicated database shared by every virtual host.
/// * `per-site` — one local database per virtual host, no replication.
/// * `single-node` — one local database, no replication.
fn sqlite_mode_label(config: &Config, cluster_enabled: bool) -> &'static str {
    if is_per_site_clustered(config, cluster_enabled) {
        "per-site-clustered"
    } else if config.db.sqlite.as_ref().is_some_and(|s| is_clustered_sqlite(s, cluster_enabled)) {
        "clustered"
    } else if is_per_site_sqlite(config, cluster_enabled) {
        "per-site"
    } else {
        "single-node"
    }
}

/// Check if clustered SQLite mode should be used.
fn is_clustered_sqlite(sqlite_config: &ephpm_config::SqliteConfig, cluster_enabled: bool) -> bool {
    let role = sqlite_config.replication.role.as_str();
    role == "primary" || role == "replica" || (role == "auto" && cluster_enabled)
}

/// Whether the embedded database runs in **per-site** (secure multi-tenant)
/// mode: `[db.sqlite]` is configured, `[server] sites_dir` is set, and the
/// database is not clustered.
///
/// This is the single source of truth shared by `serve()` (which builds and
/// registers the per-site registry), `start_db_proxies` (which then skips the
/// shared wire listener), and `Router::new` (which pushes the per-request site
/// key to the bridge) — so all three agree on when isolation is active.
///
/// Clustered multi-site is intentionally excluded here: with
/// `[db.sqlite.replication] per_site = false` (the default) all tenants share
/// the one clustered database and `serve()` warns loudly. The opt-in
/// per-site *clustered* mode ([`is_per_site_clustered`]) is a separate path —
/// one replicated database per tenant — and this predicate stays `false` for
/// it, so the single-node per-site path is never taken for a cluster.
pub(crate) fn is_per_site_sqlite(config: &Config, cluster_enabled: bool) -> bool {
    config.db.sqlite.as_ref().is_some_and(|s| {
        config.server.sites_dir.is_some() && !is_clustered_sqlite(s, cluster_enabled)
    })
}

/// Whether the embedded database runs in **per-site clustered** mode:
/// `[db.sqlite]` is configured, `[server] sites_dir` is set, the database is
/// clustered, AND `[db.sqlite.replication] per_site = true`.
///
/// This is the opt-in that makes multi-tenant per-site isolation coexist with
/// clustering: each virtual host gets its own Turso database that *replicates*
/// across the cluster (HRW ownership), instead of every tenant sharing the one
/// clustered database (the `per_site = false` default, which warns).
///
/// Strictly more specific than [`is_clustered_sqlite`], so `start_db_proxies`
/// and `resolve_channel_features` must test it **before** the single-database
/// clustered case — a per-site-clustered config also satisfies
/// `is_clustered_sqlite` (which is what already enables the `cdc` channel
/// feature for it), and would otherwise wrongly take the single-DB CDC path.
pub(crate) fn is_per_site_clustered(config: &Config, cluster_enabled: bool) -> bool {
    config.db.sqlite.as_ref().is_some_and(|s| {
        config.server.sites_dir.is_some()
            && is_clustered_sqlite(s, cluster_enabled)
            && s.replication.per_site
    })
}

/// Build the per-site database registry and register it with the PHP bridge,
/// when per-site mode is active. Returns whether it is active (for the router
/// flag). The registry itself is kept alive by the resolver `Arc` handed to
/// the bridge, so it is dropped here intentionally.
///
/// Fails closed: in multi-site mode `[db.sqlite] dir` is **required** — a
/// single shared database would defeat tenant isolation, so refuse to start
/// rather than silently share one.
fn wire_per_site_db(
    config: &Config,
    query_stats: &ephpm_query_stats::QueryStats,
    cluster: Option<&Arc<ephpm_cluster::ClusterHandle>>,
    channel: Option<&ephpm_cluster::ChannelHandle>,
) -> anyhow::Result<Option<(site_wire_auth::SiteWireAuth, Option<PerSiteClusterWiring>)>> {
    let cluster_enabled = cluster.is_some();
    // Per-site CLUSTERED mode (opt-in via [db.sqlite.replication] per_site):
    // isolated per-tenant databases that ALSO replicate across the cluster.
    // Built here (before the listeners bind) like single-node per-site, plus
    // the replication wiring the CDC path consumes in `start_db_proxies`.
    if is_per_site_clustered(config, cluster_enabled) {
        return wire_per_site_clustered_db(config, query_stats, cluster, channel).map(Some);
    }

    // Reaching here means per-site clustered mode did NOT engage, so if the
    // knob is set it is inert. The repo forbids silent no-op config knobs:
    // say so rather than let an operator believe tenants are isolated.
    if let Some(sqlite) = &config.db.sqlite
        && sqlite.replication.per_site
    {
        tracing::warn!(
            sites_dir = config.server.sites_dir.is_some(),
            clustered = is_clustered_sqlite(sqlite, cluster_enabled),
            "[db.sqlite.replication] per_site = true has NO EFFECT in this configuration: it \
             requires BOTH [server] sites_dir (multi-tenant) AND clustered replication \
             ([cluster] enabled with replication.role auto/primary/replica). This deployment \
             is running unchanged without per-site clustered replication."
        );
    }

    // A multi-site + clustered SQLite config WITHOUT per_site cannot get
    // per-site isolation: warn rather than silently pretend it is isolated.
    // (When per_site is set the branch above took the isolated path instead.)
    if let Some(sqlite) = &config.db.sqlite
        && config.server.sites_dir.is_some()
        && is_clustered_sqlite(sqlite, cluster_enabled)
    {
        tracing::warn!(
            "[db.sqlite] multi-site mode ([server] sites_dir) combined with clustered \
                 replication does NOT get per-site database isolation — all virtual hosts share \
                 the clustered database. Set [db.sqlite.replication] per_site = true for isolated, \
                 per-tenant databases that replicate across the cluster (experimental; reads on \
                 any node, writes to each site's owner), or accept a shared database."
        );
    }

    if !is_per_site_sqlite(config, cluster_enabled) {
        return Ok(None);
    }

    let sqlite = config.db.sqlite.as_ref().expect("sqlite present in per-site mode");
    validate_sqlite_engine(&sqlite.engine)?;

    let dir = sqlite.dir.as_ref().map(std::path::PathBuf::from).ok_or_else(|| {
        anyhow::anyhow!(
            "[db.sqlite] dir is required in multi-site mode ([server] sites_dir is set): each \
             virtual host needs its own database file at <dir>/<site-key>.db. Set \
             `[db.sqlite] dir = \"...\"` to enable per-site isolation. Refusing to start with a \
             single shared database, which would let one tenant read and write another's data \
             (see issue #274)."
        )
    })?;

    let registry = site_backends::SiteBackends::new(
        dir,
        sqlite.max_open_dbs,
        query_stats.clone(),
        tokio::runtime::Handle::current(),
    )?;
    ephpm_php::PhpRuntime::set_db_backend_resolver(
        registry.as_resolver(),
        tokio::runtime::Handle::current(),
    );

    // Mint the per-site MySQL credentials over the SAME registry, so a site's
    // `pdo_mysql` connections and its `ephpm_db_*` bridge queries resolve to
    // one backend instance and one LRU entry — not two handles on one file.
    let auth = site_wire_auth::SiteWireAuth::new(registry)?;

    tracing::info!(
        max_open_dbs = sqlite.max_open_dbs,
        "per-site database isolation enabled (one Turso database per virtual host), reachable \
         both through the in-process ephpm_db_* bridge and through pdo_mysql via per-site \
         credentials on the MySQL wire listener"
    );
    Ok(Some((auth, None)))
}

/// Replication wiring for per-site clustered mode, handed from
/// [`wire_per_site_db`] (which builds the registry before the listeners bind)
/// to [`start_db_proxies`] (which starts the CDC replication plane).
struct PerSiteClusterWiring {
    /// `[db.sqlite] dir`, the per-site database directory.
    dir: std::path::PathBuf,
    /// Site keys that became active locally (a request opened them), fed by
    /// the registry's open hook. Drives the per-site replication working set.
    site_events: tokio::sync::mpsc::UnboundedReceiver<String>,
    /// The per-site registry, shared with the CDC path so the owner-side
    /// `sql/` forwarding handler runs statements against the same local
    /// backends the resolver and wire listener use.
    registry: site_backends::SiteBackends,
}

/// Build the per-site **clustered** registry: capture-on Turso backends (so an
/// owned site captures the writes it ships) plus an open hook that feeds newly
/// active sites to the replication driver. Registers the **forwarding**
/// `ephpm_db_*` resolver ([`sql_forward::ClusteredSiteResolver`]) — which
/// serves a site locally when this node is its HRW owner and forwards to the
/// owner otherwise — and mints the per-site MySQL credentials over the same
/// registry.
///
/// # Errors
///
/// Fails closed if `[db.sqlite] dir` is unset (a shared database would defeat
/// tenant isolation), if the cluster/channel context is missing (a startup
/// ordering bug — per-site clustered mode requires both), or if the registry
/// cannot be built.
fn wire_per_site_clustered_db(
    config: &Config,
    query_stats: &ephpm_query_stats::QueryStats,
    cluster: Option<&Arc<ephpm_cluster::ClusterHandle>>,
    channel: Option<&ephpm_cluster::ChannelHandle>,
) -> anyhow::Result<(site_wire_auth::SiteWireAuth, Option<PerSiteClusterWiring>)> {
    let sqlite = config.db.sqlite.as_ref().expect("sqlite present in per-site clustered mode");
    validate_sqlite_engine(&sqlite.engine)?;

    let cluster = cluster.context(
        "per-site clustered mode requires [cluster] enabled = true, but no cluster handle is \
         available (startup ordering bug)",
    )?;
    let channel = channel.context(
        "per-site clustered mode requires the cluster channel to be bound, but it is not \
         (startup ordering bug: resolve_channel_features should have enabled the cdc feature)",
    )?;

    let dir = sqlite.dir.as_ref().map(std::path::PathBuf::from).ok_or_else(|| {
        anyhow::anyhow!(
            "[db.sqlite] dir is required in per-site clustered mode ([server] sites_dir set with \
             [db.sqlite.replication] per_site = true): each virtual host needs its own database \
             file at <dir>/<site-key>.db. Refusing to start with a single shared database."
        )
    })?;

    // Announces a newly-active site to the per-site replication driver.
    //
    // Shared by BOTH activation paths, which is the point: a site becomes
    // active on this node either by having its database opened locally
    // (`SiteBackends`, the owner and stock-`pdo_mysql` routes) or by being
    // forwarded to its owner (`ClusteredSiteResolver`, the bridge route on a
    // non-owner). Wiring only the first left a bridge-only node forwarding a
    // site it never replicated — see `sql_forward::ClusteredSiteResolver`.
    let (site_tx, site_events) = tokio::sync::mpsc::unbounded_channel::<String>();
    let note_active: site_backends::SiteOpenHook = Arc::new(move |site: &str| {
        // The driver dedups; a full channel is impossible (unbounded), and a
        // dropped receiver only means the CDC path is not running.
        let _ = site_tx.send(site.to_string());
    });

    let registry = site_backends::SiteBackends::new_clustered(
        dir.clone(),
        sqlite.max_open_dbs,
        query_stats.clone(),
        tokio::runtime::Handle::current(),
        Arc::clone(&note_active),
    )?;

    // ONE resolver, shared by both tenant routes: local when this node is the
    // site's HRW owner, a remote proxy to the owner otherwise. This is what
    // makes writes work on any node.
    let resolver = Arc::new(sql_forward::ClusteredSiteResolver::new(
        registry.clone(),
        Arc::clone(cluster),
        channel.clone(),
        cluster.self_node().id,
        tokio::runtime::Handle::current(),
        note_active,
    ));

    // Route 1: the `ephpm_db_*` bridge.
    ephpm_php::PhpRuntime::set_db_backend_resolver(
        Arc::clone(&resolver) as Arc<dyn ephpm_php::db_bridge::SiteBackendResolver>,
        tokio::runtime::Handle::current(),
    );

    // Route 2: stock `pdo_mysql` on the multi-tenant wire listener. It resolves
    // through the SAME resolver, so a wire write on a non-owner is forwarded to
    // the owner and captured into CDC. Handing the wire listener the bare
    // registry instead (as it did before) made a non-owner's `pdo_mysql` write
    // commit to a local replica that nothing replicates and that is discarded
    // on the next re-bootstrap — silent divergence.
    let auth = site_wire_auth::SiteWireAuth::with_route(
        Arc::clone(&resolver) as Arc<dyn site_wire_auth::SiteWireRoute>
    )?;

    tracing::info!(
        max_open_dbs = sqlite.max_open_dbs,
        "per-site CLUSTERED database isolation enabled (one replicated Turso database per virtual \
         host). Owner-serves forwarding is active on BOTH tenant routes — the ephpm_db_* bridge \
         and stock pdo_mysql — so a site is served locally on its HRW owner and forwarded to the \
         owner from any other node, and reads and writes work on any node (experimental)."
    );

    Ok((auth, Some(PerSiteClusterWiring { dir, site_events, registry })))
}

/// Whether the per-site MySQL wire *frontend* should be bound.
///
/// Controlled by `[db.sqlite.proxy] mysql_wire_enabled` (default `true`).
/// Setting it `false` is for **bridge-only** deployments: every app reaches
/// its per-site database exclusively through the in-process `ephpm_db_*` SAPI
/// bridge and nothing uses stock `pdo_mysql`, so the `:3306` listener is pure
/// attack surface and stays unbound.
///
/// This gates ONLY the wire frontend. The per-site database registry and the
/// `ephpm_db_*` bridge are wired up independently in [`wire_per_site_db`], so
/// in-process database access is unaffected when this returns `false`.
fn per_site_wire_enabled(sqlite: &ephpm_config::SqliteConfig) -> bool {
    sqlite.proxy.mysql_wire_enabled
}

/// Start the multi-tenant MySQL wire listener for per-site mode.
///
/// One listener, many databases. The connection's tenant is the identity it
/// authenticates as, not anything it merely claims:
/// [`SiteWireAuth`](site_wire_auth::SiteWireAuth) verifies a per-site
/// `mysql_native_password` credential and only then resolves that site's
/// backend out of the same registry the `ephpm_db_*` bridge uses. A tenant that
/// cannot produce another tenant's password is refused at the handshake and
/// never reaches a backend at all.
///
/// # Why one listener and not one per site
///
/// A listener per site would cost N file descriptors and N ports and buy
/// **nothing**: every tenant's PHP runs in this process as this OS user, so
/// neither a port (enumerable) nor a unix socket (identical permissions) can
/// tell one tenant's connection from another's. The credential has to do that
/// work either way — so the listener count stays at one, the address stays the
/// stable, configured `[db.sqlite.proxy] mysql_listen`, and resource use is
/// O(1) in the number of sites. See `site_wire_auth`'s module docs for the full
/// argument.
///
/// # The other frontends stay off
///
/// Hrana, PostgreSQL, and TDS cannot resolve a backend per connection (litewire
/// refuses to start them under an authenticator, and would otherwise have to
/// bind one shared backend). They are skipped with a warning rather than served
/// a single tenant's database.
///
/// # Errors
///
/// Fails if `mysql_listen` is unparseable or cannot be bound. Both are fatal
/// on purpose: litewire's builder silently ignores an address it cannot parse
/// (leaving no frontend at all), and a port already in use would otherwise
/// surface only as every tenant's `pdo_mysql` getting connection-refused at
/// runtime. Refusing to start is the same contract the other DB proxies use.
///
/// Returns the address tenants should connect to, for injection into `DB_HOST`
/// / `DB_PORT`.
async fn start_per_site_wire(
    sqlite_config: &ephpm_config::SqliteConfig,
    auth: &site_wire_auth::SiteWireAuth,
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
) -> anyhow::Result<String> {
    let proxy = &sqlite_config.proxy;

    // Validate before spawning. `LiteWire::mysql` does `addr.parse().ok()`, so
    // a typo becomes a listener that never exists and a "no frontends
    // configured" error buried in a detached task.
    let addr: std::net::SocketAddr = proxy.mysql_listen.parse().with_context(|| {
        format!(
            "[db.sqlite.proxy] mysql_listen is not a valid socket address: {:?}. In multi-site \
             mode this is the endpoint every tenant's pdo_mysql connects to.",
            proxy.mysql_listen
        )
    })?;
    // Probe the bind so a busy port fails startup rather than at first query.
    // Dropped immediately; litewire binds it for real a moment later. The
    // window between the two is a race in theory, but the failure it catches
    // (a port already owned by another process for the whole run) is not.
    drop(tokio::net::TcpListener::bind(addr).await.with_context(|| {
        format!("failed to bind the multi-tenant MySQL wire listener on {addr}")
    })?);

    if proxy.hrana_listen.is_some() || proxy.postgres_listen.is_some() || proxy.tds_listen.is_some()
    {
        tracing::warn!(
            "[db.sqlite.proxy] hrana/postgres/tds listeners are configured but NOT started in \
             multi-site mode: only the MySQL frontend can bind a database per connection, so the \
             others would serve one shared database to every tenant. Only mysql_listen is served."
        );
    }

    let mut builder =
        litewire::LiteWire::with_authenticator(auth.as_authenticator()).mysql(&proxy.mysql_listen);

    if proxy.max_connections > 0 {
        builder = builder.max_connections(proxy.max_connections);
    }

    tracing::info!(
        listen = %proxy.mysql_listen,
        // A global cap, not a per-tenant one: see the `site_wire_auth` docs on
        // noisy neighbours.
        max_connections = proxy.max_connections,
        "per-site database mode: MySQL wire listener enabled with per-site credentials. Each \
         virtual host connects with DB_USER = its own hostname and the DB_PASSWORD injected into \
         its requests, and reaches ONLY its own database."
    );

    handles.push(tokio::spawn(async move {
        match builder.serve().await {
            Ok(()) => tracing::info!("litewire (per-site) stopped"),
            Err(e) => tracing::error!("litewire (per-site) error: {e:#}"),
        }
    }));

    Ok(proxy.mysql_listen.clone())
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

    // ── Main-listener role when a separate TLS listener is configured ───
    //
    // `[server.tls] listen` documents: "server.listen serves HTTP and this
    // address serves HTTPS". These tests pin that contract at
    // `dispatch_main_connection` itself, over a real socket, by inspecting
    // the first bytes the server writes back to a plaintext GET. The two
    // answers are unambiguous on the wire: an HTTP response begins
    // `HTTP/1.1`, whereas a TLS record begins with a content-type byte
    // (0x16 handshake, 0x15 alert). The live bug was diagnosed exactly this
    // way — `nc` to the plain port returned `15 03 03 00 02 02 32`.

    /// A router serving one static file out of a temp document root.
    fn dispatch_test_router(docroot: &std::path::Path) -> Arc<Router> {
        let config = ephpm_config::Config {
            server: ephpm_config::ServerConfig {
                listen: "127.0.0.1:0".to_owned(),
                document_root: docroot.to_path_buf(),
                index_files: vec!["index.html".to_owned()],
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

    /// Manual TLS over a freshly generated self-signed cert.
    fn dispatch_test_manual_tls(dir: &std::path::Path) -> TlsMode {
        tls::tests_support::init_crypto();
        let (cert, key) = tls::tests_support::generate_ec_cert(dir);
        TlsMode::Manual(tls::build_tls_acceptor(&cert, &key).expect("build acceptor"))
    }

    /// ACME TLS mode over the same cert material, so the tests cover the
    /// `TlsMode::Acme` arm too — the original bug hit both.
    fn dispatch_test_acme_tls(dir: &std::path::Path) -> TlsMode {
        tls::tests_support::init_crypto();
        let (cert, key) = tls::tests_support::generate_ec_cert(dir);
        let alpn = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let build =
            || Arc::new(tls::build_server_config(&cert, &key, &alpn).expect("build server config"));
        TlsMode::Acme { challenge_config: build(), default_config: build() }
    }

    /// Send a plaintext `GET /` through `dispatch_main_connection` and
    /// return the first bytes written back.
    async fn plaintext_probe(
        tls_mode: TlsMode,
        has_tls_listener: bool,
        redirect_http: bool,
    ) -> Vec<u8> {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("index.html"), "hello").expect("write index");
        let router = dispatch_test_router(dir.path());

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind probe listener");
        let addr = listener.local_addr().expect("probe local addr");
        let in_flight = Arc::new(AtomicUsize::new(0));

        let server = tokio::spawn(async move {
            let (stream, remote) = listener.accept().await.expect("accept probe connection");
            dispatch_main_connection(
                stream,
                remote,
                &tls_mode,
                has_tls_listener,
                redirect_http,
                ConnSettings {
                    header_read_timeout: Duration::from_secs(5),
                    max_header_size: 16 * 1024,
                    idle_timeout: Duration::from_secs(5),
                },
                &router,
                None,
                &in_flight,
            );
            // `dispatch_main_connection` spawns the connection task with its
            // own clones, so this task has nothing left to hold.
        });

        let mut client = tokio::net::TcpStream::connect(addr).await.expect("connect to probe");
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("write plaintext request");

        let mut buf = vec![0u8; 512];
        let read = tokio::time::timeout(Duration::from_secs(10), client.read(&mut buf))
            .await
            .expect("probe read timed out")
            .expect("probe read failed");
        buf.truncate(read);
        server.await.expect("probe server task");
        buf
    }

    /// Render the probe bytes for an assertion message: a TLS record is not
    /// printable, so show the leading bytes in hex as well.
    fn probe_debug(bytes: &[u8]) -> String {
        let head: Vec<String> = bytes.iter().take(8).map(|b| format!("{b:02x}")).collect();
        format!("{:?} (first bytes: {})", String::from_utf8_lossy(bytes), head.join(" "))
    }

    /// The regression: `[server] listen` plus `[server.tls] listen` with
    /// `redirect_http` unset (its default, `false`) must leave the main
    /// listener speaking plain HTTP. It used to fall through to the
    /// `tls_mode` match and TLS-wrap the plain listener, so every
    /// plain-HTTP client — including load-balancer health checks — got a
    /// TLS alert and a failed connection.
    #[tokio::test]
    async fn main_listener_serves_plain_http_when_tls_listener_is_configured() {
        let dir = tempfile::tempdir().expect("cert dir");
        let got = plaintext_probe(dispatch_test_manual_tls(dir.path()), true, false).await;
        assert!(
            got.starts_with(b"HTTP/1.1 "),
            "main listener must answer plain HTTP, got {}",
            probe_debug(&got)
        );
        assert_ne!(
            got.first(),
            Some(&0x15),
            "main listener wrote a TLS alert record instead of HTTP: {}",
            probe_debug(&got)
        );
        assert_ne!(
            got.first(),
            Some(&0x16),
            "main listener wrote a TLS handshake record instead of HTTP: {}",
            probe_debug(&got)
        );
    }

    /// Same contract in ACME mode: the bug was in the shared fall-through,
    /// so both `TlsMode` variants have to be pinned.
    #[tokio::test]
    async fn acme_main_listener_serves_plain_http_when_tls_listener_is_configured() {
        let dir = tempfile::tempdir().expect("cert dir");
        let got = plaintext_probe(dispatch_test_acme_tls(dir.path()), true, false).await;
        assert!(
            got.starts_with(b"HTTP/1.1 "),
            "ACME main listener must answer plain HTTP, got {}",
            probe_debug(&got)
        );
    }

    /// `redirect_http` keeps its only job: it decides what the plain-HTTP
    /// listener *says*, not whether it is plain HTTP.
    #[tokio::test]
    async fn main_listener_redirects_when_redirect_http_is_enabled() {
        let dir = tempfile::tempdir().expect("cert dir");
        let got = plaintext_probe(dispatch_test_manual_tls(dir.path()), true, true).await;
        assert!(
            got.starts_with(b"HTTP/1.1 301"),
            "redirect_http must 301 to HTTPS, got {}",
            probe_debug(&got)
        );
    }

    /// The other half of the contract, so the fix cannot regress into
    /// "never terminate TLS on the main listener": with no separate TLS
    /// listener, `server.listen` *is* the HTTPS listener, and a plaintext
    /// GET must not be answered with HTTP.
    #[tokio::test]
    async fn main_listener_terminates_tls_when_it_is_the_only_listener() {
        let dir = tempfile::tempdir().expect("cert dir");
        let got = plaintext_probe(dispatch_test_manual_tls(dir.path()), false, false).await;
        assert!(
            !got.starts_with(b"HTTP/"),
            "sole listener must terminate TLS, not answer plain HTTP: {}",
            probe_debug(&got)
        );
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
            dir: None,
            max_open_dbs: 256,
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

    /// Full `Config` for the mode matrix: `[db.sqlite]` always present,
    /// `[server] sites_dir` / `[db.sqlite.replication] per_site` /
    /// `[cluster] enabled` toggled.
    fn make_mode_config(
        sites_dir: Option<&std::path::Path>,
        role: &str,
        per_site: bool,
        cluster_enabled: bool,
    ) -> Config {
        let mut sqlite = make_sqlite_config(role);
        sqlite.replication.per_site = per_site;
        Config {
            server: ephpm_config::ServerConfig {
                sites_dir: sites_dir.map(std::path::Path::to_path_buf),
                ..ephpm_config::ServerConfig::default()
            },
            cluster: ephpm_config::ClusterConfig {
                enabled: cluster_enabled,
                ..Config::default().cluster
            },
            db: ephpm_config::DbConfig {
                sqlite: Some(sqlite),
                ..ephpm_config::DbConfig::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn sqlite_mode_matrix_is_exclusive_and_preserves_the_pre_existing_modes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sites = Some(dir.path());

        /// The database mode a config resolves to. Exactly one holds, which is
        /// the property this test exists to pin.
        #[derive(Debug, PartialEq, Eq, Clone, Copy)]
        enum Mode {
            /// One database, no vhost dimension, no cluster.
            SingleNode,
            /// One database per vhost, no cluster (pre-existing).
            PerSiteSingleNode,
            /// One clustered database shared by every vhost (pre-existing).
            ClusteredSingleDb,
            /// One replicated database per vhost (the new opt-in).
            PerSiteClustered,
        }

        /// Resolve the mode via the **production** label function rather than
        /// a copy of its branch order.
        ///
        /// `sqlite_mode_label` is what `start_db_proxies` logs as the effective
        /// mode, so routing the matrix through it pins two things at once: that
        /// exactly one mode holds per config, and that the label an operator
        /// (or a benchmark gate) reads at startup names the mode that actually
        /// runs. A re-implementation here could agree with the matrix while
        /// disagreeing with the log, which is the failure this guards.
        fn resolve_mode(config: &Config, cluster: bool) -> Mode {
            match sqlite_mode_label(config, cluster) {
                "per-site-clustered" => Mode::PerSiteClustered,
                "clustered" => Mode::ClusteredSingleDb,
                "per-site" => Mode::PerSiteSingleNode,
                "single-node" => Mode::SingleNode,
                other => panic!("unknown [db.sqlite] mode label: {other:?}"),
            }
        }

        // (sites_dir, role, per_site, cluster) => resolved mode
        let cases: &[(bool, &str, bool, bool, Mode)] = &[
            // --- Pre-existing modes, which must be unchanged. ---
            // Plain single-node: no sites_dir, no cluster.
            (false, "auto", false, false, Mode::SingleNode),
            // Single-node per-site: sites_dir, no cluster.
            (true, "auto", false, false, Mode::PerSiteSingleNode),
            // Single-DB clustered, single-site.
            (false, "auto", false, true, Mode::ClusteredSingleDb),
            // Single-DB clustered + multi-site WITHOUT the opt-in: the shared
            // database (warns). This is the row the new mode must not steal.
            (true, "auto", false, true, Mode::ClusteredSingleDb),
            // Forced single-node (role neither auto/primary/replica) even with
            // clustering on: still the single-node per-site path.
            (true, "single", false, true, Mode::PerSiteSingleNode),
            // --- The new opt-in mode. ---
            (true, "auto", true, true, Mode::PerSiteClustered),
            (true, "primary", true, false, Mode::PerSiteClustered),
            // --- The opt-in is inert wherever it does not apply. ---
            // per_site with no cluster => plain single-node per-site.
            (true, "auto", true, false, Mode::PerSiteSingleNode),
            // per_site with no sites_dir => single-DB clustered, unchanged.
            (false, "auto", true, true, Mode::ClusteredSingleDb),
        ];

        for &(has_sites, role, per_site, cluster, want) in cases {
            let sites_dir = if has_sites { sites } else { None };
            let config = make_mode_config(sites_dir, role, per_site, cluster);
            let label =
                format!("sites_dir={has_sites} role={role} per_site={per_site} cluster={cluster}");
            assert_eq!(resolve_mode(&config, cluster), want, "resolved mode: {label}");

            // The two per-site predicates are mutually exclusive, so the
            // single-node per-site path can never be taken for a cluster.
            assert!(
                !(is_per_site_sqlite(&config, cluster) && is_per_site_clustered(&config, cluster)),
                "per-site single-node and per-site clustered must never both hold: {label}"
            );
            // And per-site clustered is a strict subset of clustered, which is
            // why `start_db_proxies` must test it first.
            let sqlite = config.db.sqlite.as_ref().expect("sqlite present");
            assert!(
                !is_per_site_clustered(&config, cluster) || is_clustered_sqlite(sqlite, cluster),
                "per-site clustered implies clustered: {label}"
            );
        }
    }

    #[test]
    fn per_site_wire_enabled_by_default() {
        let config = make_sqlite_config("single");
        assert!(
            per_site_wire_enabled(&config),
            "the per-site MySQL wire listener is bound by default"
        );
    }

    #[test]
    fn per_site_wire_disabled_when_toggled_off() {
        let mut config = make_sqlite_config("single");
        config.proxy.mysql_wire_enabled = false;
        assert!(
            !per_site_wire_enabled(&config),
            "mysql_wire_enabled = false must skip binding the wire frontend (bridge-only mode)"
        );
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

    /// Multi-tenant config with the RESP listener enabled, secret optional.
    fn kv_resp_config(sites_dir: Option<&std::path::Path>, secret: Option<&str>) -> Config {
        Config {
            server: ephpm_config::ServerConfig {
                sites_dir: sites_dir.map(std::path::Path::to_path_buf),
                ..ephpm_config::ServerConfig::default()
            },
            kv: ephpm_config::KvConfig {
                secret: secret.map(str::to_string),
                redis_compat: ephpm_config::KvRedisCompatConfig {
                    enabled: true,
                    // Ephemeral port so the Ok path doesn't fight for 6379.
                    listen: "127.0.0.1:0".to_string(),
                    ..ephpm_config::KvRedisCompatConfig::default()
                },
                ..ephpm_config::KvConfig::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn start_kv_service_refuses_multi_tenant_resp_without_secret() {
        // Defense in depth: even if `Config::validate` is bypassed (embedding,
        // tests), startup itself must fail closed rather than serve a shared
        // global store to every tenant. The bail is before any `tokio::spawn`,
        // so no runtime is needed here.
        let dir = tempfile::tempdir().expect("tempdir");
        let config = kv_resp_config(Some(dir.path()), None);
        // `KvService` (the Ok type) isn't `Debug`, so match rather than
        // `expect_err`.
        let err = match start_kv_service(&config) {
            Ok(_) => panic!("must refuse to start a multi-tenant RESP listener without a secret"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("secret"), "error should point at [kv] secret: {msg}");
    }

    #[tokio::test]
    async fn start_kv_service_multi_tenant_resp_with_secret_starts() {
        // The secure config starts and returns a live RESP listener handle.
        let dir = tempfile::tempdir().expect("tempdir");
        let config = kv_resp_config(Some(dir.path()), Some("s3cret-value"));
        let (_store, multi_tenant, handle) =
            start_kv_service(&config).expect("secure multi-tenant config must start");
        assert!(multi_tenant.is_some(), "sites_dir set → per-vhost stores exist");
        let handle = handle.expect("RESP listener enabled → a listener task exists");
        handle.abort();
    }

    #[tokio::test]
    async fn start_kv_service_single_tenant_resp_without_secret_starts() {
        // Single-site is unaffected: the shared store is correct, no secret
        // required, RESP listener still starts.
        let config = kv_resp_config(None, None);
        let (_store, multi_tenant, handle) =
            start_kv_service(&config).expect("single-tenant RESP must keep working");
        assert!(multi_tenant.is_none(), "no sites_dir → no per-vhost stores");
        let handle = handle.expect("RESP listener enabled → a listener task exists");
        handle.abort();
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
