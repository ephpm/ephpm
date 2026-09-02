//! Request router.
//!
//! Routes incoming HTTP requests using configurable `fallback` resolution:
//! each entry is checked in order, and the first match that exists on disk
//! is served. The last entry is the fallback (an internal rewrite or status
//! code like `=404`).

use std::collections::HashMap;
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

#[allow(unused_imports)]
use ::metrics::{counter, gauge, histogram};
use ephpm_config::{Config, MiddlewareMount};
use ephpm_kv::store::Store;
use ephpm_php::PhpRuntime;
use ephpm_php::request::PhpRequest;
use flate2::Compression;
use flate2::write::GzEncoder;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Request, Response, StatusCode};
use ipnet::IpNet;

use crate::body::{self, ServerBody};
use crate::{metrics, static_files};

/// Outcome of running the middleware **request** phase ahead of a static-file
/// response (issue #395, security half): either short-circuit with a module
/// response, or continue serving the file with any appended response headers.
enum StaticGate {
    /// A request-phase module answered (e.g. a 401/403 auth denial); serve
    /// this instead of the file — the file is never read.
    Respond(Response<ServerBody>),
    /// Serve the file; append these `CONTINUE`/`REWRITE` response headers.
    Continue(Vec<(String, String)>),
}

/// Result of resolving a request through `fallback`.
enum Resolved {
    /// A file on disk (static or PHP).
    File(PathBuf),
    /// A status code fallback (e.g. `=404`).
    Status(u16),
}

/// Compression settings extracted from config.
#[derive(Clone, Copy)]
pub struct CompressionSettings {
    /// Whether compression is enabled.
    pub enabled: bool,
    /// Gzip compression level (1–9).
    pub level: u32,
    /// Minimum response size in bytes to compress.
    pub min_size: usize,
    /// Streamed worker-response compression mode.
    pub streaming: StreamingCompression,
}

/// Streamed worker-response compression mode
/// (`[server.response] compression_streaming`).
///
/// Applies only to worker-mode `send_response_stream` bodies. Buffered
/// responses keep the whole-body brotli/gzip path regardless of this
/// setting.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum StreamingCompression {
    /// Streamed responses pass through identity-encoded. The code path is
    /// identical to releases before this knob existed: no encoder, task,
    /// or extra channel is created.
    #[default]
    Off,
    /// Brotli-compress streamed responses whose Content-Type is
    /// `text/event-stream`, flushing per chunk so every SSE event decodes
    /// as it arrives (see [`crate::stream_compress`]).
    Sse,
    /// Brotli-compress every streamed worker response.
    All,
}

impl StreamingCompression {
    /// Parse the config string (`"off"` / `"sse"` / `"all"`,
    /// case-insensitive). Returns `None` for unknown values so the caller
    /// can warn — a typo must never become a silent no-op.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "sse" => Some(Self::Sse),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

/// How long a "no site directory for this host" answer is cached
/// before we retry the on-disk lookup. Short enough that lazy vhost
/// discovery stays near-immediate for legitimate deploys — 60s here
/// shipped in 0.4.0 and made freshly created site directories 404 for
/// up to a minute after any prior probe (caught by the vhosts e2e once
/// the suite was un-broken). 2s still collapses bot-probe bursts for
/// the same `Host: <random>` into one stat per window, which is all
/// the cache is for; a stat every 2s per unique host is noise.
const UNKNOWN_SITE_TTL: Duration = Duration::from_secs(2);

/// A virtual host's two roots, which are **not** the same directory once an
/// operator-supplied override declares a `document_root`
/// (`[server] site_overrides_dir`, see [`crate::site_overrides`]).
///
/// # Why two
///
/// A vhost directory under `sites_dir` is the site *container*: the whole
/// checkout, including `vendor/`, `composer.json`, `config/` and
/// `storage/logs/`. Modern PHP frameworks put their front controller in a
/// subdirectory (`public/`, `web/`, `htdocs/`) exactly so those files sit above
/// the web root. Before this struct existed the two collapsed into one
/// `document_root`, and a Laravel vhost served its own logs — which routinely
/// carry stack traces containing env values and database credentials — over HTTP.
///
/// # The invariant
///
/// * `document_root` is the **web root**: what URLs resolve against, what static
///   files and PHP entrypoints are contained within, and `$_SERVER['DOCUMENT_ROOT']`.
/// * `container` is the **`open_basedir` boundary** and the identity the
///   per-vhost temp/session state root is derived from.
///
/// `container` must never follow `document_root` into the web root, for two
/// separate reasons:
///
/// 1. **It would break every framework.** PHP served from `public/index.php`
///    does `require __DIR__.'/../vendor/autoload.php'` on its first line; a
///    sandbox narrowed to the web root fails on request one.
/// 2. **It would make the sandbox override-controlled.** `open_basedir` is the
///    primary cross-tenant boundary in a model where all tenants share one
///    process and one uid, so it is derived solely from the container ePHPm
///    placed the tenant in. An override may narrow what is *served*; it can
///    never widen what PHP may *read*.
///
/// Pinned by [`tests::site_override_open_basedir_stays_the_container`] and
/// [`tests::site_override_cannot_widen_open_basedir`].
///
/// When no declaration applies — no override file, a rejected declaration, the
/// mechanism disabled, an unmatched host, or single-site mode — both fields hold
/// the same path and behaviour is byte-identical to before.
#[derive(Clone, Debug)]
pub(crate) struct SiteRoots {
    /// The web root: URL resolution, static-file and PHP-script containment,
    /// and `$_SERVER['DOCUMENT_ROOT']`.
    pub(crate) document_root: PathBuf,
    /// The site container: the `open_basedir` entry, the OPcache invalidation
    /// scope, and the input to [`vhost_state_root`]. Equal to `document_root`
    /// whenever no per-site declaration applies. **Never override-controlled.**
    pub(crate) container: PathBuf,
}

impl SiteRoots {
    /// Both roots are the same directory — the shape that predates per-site
    /// overrides, used for the default document root and wherever no container
    /// is distinguishable.
    fn flat(root: PathBuf) -> Self {
        Self { container: root.clone(), document_root: root }
    }

    /// `true` when a per-site override actually moved this site's web root.
    fn declared(&self) -> bool {
        self.document_root != self.container
    }
}

/// Per-site configuration resolved at startup from `sites_dir`.
struct SiteConfig {
    /// The vhost directory itself. Always the `open_basedir` boundary.
    container: PathBuf,
    index_files: Vec<String>,
    /// WebSocket entrypoint names for this vhost, in try-order — the
    /// `index_files` of the upgrade path. Carried per-site for exactly the same
    /// reason `index_files` is: the resolution has to happen against the
    /// vhost's own document root.
    websocket_files: Vec<String>,
    fallback: Vec<String>,
}

/// The per-request identities derived from a [`ResolvedSite`]'s key: which
/// database this request may reach, which KV keyspace it sees, and which OPcache
/// vhost it invalidates against.
///
/// One struct, one derivation site ([`Router::site_identities`]), consumed by
/// both PHP dispatch paths — so "the database, the credential and the keyspace
/// name the same tenant as the document root" is a property of one function
/// rather than of four call sites that happen to agree today.
struct SiteIdentities {
    /// Site key for the per-site database and the `pdo_mysql` credential.
    /// `None` outside per-site mode **and** for any host that matched no known
    /// vhost — such a request gets no database context rather than a fresh
    /// `<host>.db` (issue #291).
    db: Option<String>,
    /// Key for this request's KV keyspace and its RESP credential. Falls back to
    /// the normalized host for an unknown site, which keeps a catch-all document
    /// root's per-hostname keyspaces working.
    kv: String,
    /// Key for cluster-wide OPcache invalidation (`opcache:version:<key>`).
    opcache: String,
    /// Scope for this request's WebSocket capability — which connections and
    /// channels `ephpm_ws_*` may reach.
    ///
    /// `None` on a multi-tenant node for a host that matched no vhost: such a
    /// request gets no WebSocket capability at all, the same way it gets no
    /// database context (issue #291). On a single-site node it is the
    /// [`ephpm_php::ws_bridge::SINGLE_SITE_SCOPE`] sentinel, because there is
    /// exactly one tenant and nothing to isolate it from.
    ws: Option<String>,
}

/// A request's virtual host, resolved once: the tenant's **canonical site key**
/// together with everything that key selects.
///
/// # Why the key is carried rather than re-derived
///
/// ePHPm derives four separate per-tenant things from the `Host` header, and
/// they must name the *same* tenant:
///
/// 1. the **document root** (which code runs),
/// 2. the **per-site database** file, `<[db.sqlite] dir>/<key>.db`,
/// 3. the **per-vhost state root** — this tenant's private `tmp/` and
///    `sessions/` (derived from the document root, so it follows 1), and
/// 4. the **per-site wire credential** (`DB_USER` / `DB_PASSWORD` for the
///    multi-tenant MySQL listener), plus this vhost's KV keyspace and RESP
///    credential.
///
/// Each of those used to normalize the `Host` header for itself, and a pentest
/// (issue #290) found the seam: with `[server] sites_domain_suffix = ".local"`,
/// `Host: shop.local` and `Host: shop` selected the *same* document root and the
/// *same* session directory but *different* databases (`shop.local.db` vs
/// `shop.db`) — one tenant, bifurcated data. A disagreement between any two of
/// these is an isolation or integrity bug; that instance happened to be
/// integrity.
///
/// So there is now exactly one derivation — [`Router::resolve_site`] — and it
/// hands back the key it actually matched. Everything downstream consumes this
/// value instead of looking at the `Host` header again.
///
/// `key` is `None` when the host matched no known virtual host (see
/// [`Router::default_site`]): the request still gets the default document root,
/// but it has no tenant identity, so no per-tenant resource may be minted from
/// it.
struct ResolvedSite<'a> {
    /// The canonical site key — suffix-stripped, port-stripped, lowercased,
    /// trailing-dot-stripped, and [`is_valid_site_key`]-clean — or `None` when
    /// no known site matched.
    key: Option<String>,
    /// This request's web root and site container. The web root is what the
    /// request resolves against; the container is the `open_basedir` boundary
    /// and stays the vhost directory even when the web root moved into
    /// `public/`. See [`SiteRoots`].
    roots: SiteRoots,
    /// Index files for this site.
    index_files: &'a [String],
    /// WebSocket entrypoint names for this site, in try-order. Travels with the
    /// resolved site so the upgrade path never re-reads config or re-derives a
    /// document root.
    websocket_files: &'a [String],
    /// Fallback chain for this site.
    fallback: &'a [String],
}

/// A URI-path glob pattern pre-split into segments at Router
/// construction. The router's hot path evaluates blocked-path and
/// allowed-PHP-path lists on every request; the old `glob_match`
/// re-split every pattern into `Vec<&str>` per request, allocating a
/// short-lived Vec per pattern-check pair (2 patterns × N requests ×
/// M segments). Pre-splitting turns that back into borrows.
///
/// The `raw` field is kept because the exact/prefix short-circuit
/// still uses the whole-pattern string comparison.
#[derive(Clone)]
struct CompiledGlob {
    /// Original pattern string, used for exact and prefix matches
    /// (`ends_with('/')` and non-wildcard patterns).
    raw: String,
    /// Pattern split by `/` at Router construction; each segment may
    /// itself contain `*` wildcards (handled by [`segment_match`]).
    segments: Vec<String>,
    /// `true` if this pattern is `<prefix>/*` — matches the prefix
    /// directory and every child path underneath.
    prefix_wildcard: bool,
    /// `true` if the pattern contains any `*`; when `false` we can
    /// take the exact-match / prefix-match short-circuit without
    /// scanning segments.
    has_wildcard: bool,
}

impl CompiledGlob {
    /// Pre-split a raw pattern string.
    fn compile(raw: &str) -> Self {
        Self {
            raw: raw.to_string(),
            segments: raw.split('/').map(str::to_string).collect(),
            prefix_wildcard: raw.ends_with("/*"),
            has_wildcard: raw.contains('*'),
        }
    }

    /// Match a URI path against the pre-split pattern. Semantics are
    /// identical to the previous per-request `glob_match(raw, path)`.
    fn matches(&self, path: &str) -> bool {
        if !self.has_wildcard {
            // Exact match, or directory-prefix match if the pattern
            // ends in `/`.
            return path == self.raw || (self.raw.ends_with('/') && path.starts_with(&self.raw));
        }

        // Fast-path split of `path` into segments; the pattern
        // segments are already split. Both are short slices of `&str`
        // (borrowed from the input `path` and from `self.segments`).
        // No Vec allocation per call.
        let uri_segs: Vec<&str> = path.split('/').collect();

        // `<prefix>/*` matches the directory and everything below it.
        if self.prefix_wildcard {
            let prefix = &self.segments[..self.segments.len() - 1];
            let uri_prefix = &uri_segs[..prefix.len().min(uri_segs.len())];
            if prefix.len() <= uri_segs.len()
                && prefix.iter().zip(uri_prefix.iter()).all(|(p, s)| segment_match(p, s))
            {
                return true;
            }
        }

        if self.segments.len() != uri_segs.len() {
            return false;
        }

        self.segments.iter().zip(uri_segs.iter()).all(|(p, s)| segment_match(p, s))
    }
}

/// What the request pipeline needs from a request body, whatever transport
/// produced it.
///
/// The TCP path supplies hyper's [`hyper::body::Incoming`]; HTTP/3 supplies
/// [`crate::http3::H3RequestBody`]. Naming the bounds once — rather than
/// hard-coding `Incoming` — is what lets HTTP/3 reuse [`Router::handle`]
/// instead of growing a second request pipeline. The TCP path monomorphizes
/// to exactly the code it had before, so this costs nothing at runtime.
pub trait RequestBody:
    hyper::body::Body<Data = Bytes, Error = Self::BodyError> + Send + Unpin + 'static
{
    /// The body's error type, re-stated so it can carry the bounds the
    /// pipeline needs: `Display` for logging and the blanket
    /// `Into<Box<dyn Error>>` that `http_body_util::Limited` requires.
    type BodyError: std::error::Error + Send + Sync + 'static;
}

impl<B, E> RequestBody for B
where
    B: hyper::body::Body<Data = Bytes, Error = E> + Send + Unpin + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    type BodyError = E;
}

pub struct Router {
    document_root: PathBuf,
    sites: HashMap<String, SiteConfig>,
    /// Optional path to the sites directory for lazy vhost discovery.
    /// When set, unknown hosts are checked against the filesystem.
    sites_dir: Option<PathBuf>,
    /// Directory of operator-supplied per-site overrides
    /// (`[server] site_overrides_dir`), or `None` when the mechanism is off or
    /// this is single-site mode.
    ///
    /// Applied on **both** discovery paths — the startup scan and the lazy
    /// unknown-host lookup — because they share one resolver
    /// ([`Router::site_roots`]). Fixing only one would present as "works after a
    /// restart, ignores the override when the preview is created", which is
    /// precisely the provisioning flow this exists for.
    site_overrides_dir: Option<PathBuf>,
    /// Resolved per-site roots, keyed by **canonical site key**, expiring after
    /// [`SITE_CONFIG_TTL`].
    ///
    /// Keyed by site key rather than container path because the key *is* the
    /// tenant identity — the same string that names the override file, the vhost
    /// directory and `<dir>/<key>.db`. This is what makes an override take
    /// effect without a restart on an *already-discovered* site, and what keeps
    /// the file from being re-read and re-canonicalized on every request. Seeded
    /// at startup by [`Router::seed_site_roots`].
    site_roots_cache: dashmap::DashMap<String, CachedSiteRoots>,
    /// Lowercased domain suffix (e.g. `.localhost`) stripped from incoming
    /// `Host` headers before vhost resolution. Lets dev-mode users keep
    /// short directory names while their browser uses `*.localhost`.
    sites_domain_suffix: Option<String>,
    index_files: Vec<String>,
    /// Default WebSocket entrypoint names (`[server] websocket_files`), used
    /// for the default document root and for lazily-discovered vhosts.
    websocket_files: Vec<String>,
    /// Whether this node serves more than one tenant — `sites_dir` is set, or
    /// static vhosts were configured.
    ///
    /// Decides the WebSocket registry scope: multi-tenant nodes scope by the
    /// canonical site key (and a host that matched no vhost gets **no** scope),
    /// single-site nodes share one sentinel scope because there is exactly one
    /// tenant to isolate.
    multi_site: bool,
    /// Native WebSocket runtime, or `None` when `[server.websocket]` is
    /// disabled — in which case an upgrade request is routed like any other
    /// GET, exactly as before the feature existed.
    websocket: Option<Arc<crate::websocket::WsRuntime>>,
    /// Built-in reverse-proxy engine (`[[server.proxy]]`), or `None` when no
    /// rules are configured — in which case the proxy check on the request path
    /// is one `Option::is_none()`. A matched rule short-circuits all local
    /// serving.
    proxy: Option<crate::proxy::ProxyEngine>,
    /// Weak self-reference, installed by [`Router::share`].
    ///
    /// A WebSocket session outlives the request that created it, and every
    /// event it dispatches goes back through this router. `Weak` rather than a
    /// clone of the `Arc` so the router is still dropped at shutdown even with
    /// sessions live, and `Arc::new_cyclic` rather than a post-construction
    /// setter so it cannot be forgotten. A `Router` that was never shared (unit
    /// tests build one on the stack) simply cannot serve an upgrade — see
    /// [`Router::handle_websocket_upgrade`].
    self_weak: std::sync::Weak<Router>,
    fallback: Vec<String>,
    server_port: u16,
    max_body_size: u64,
    /// `[server.request] middleware_body_limit` — max body bytes buffered and
    /// exposed to request-phase middleware via `request_body`. `0` = disabled
    /// (the chain runs before the body is read).
    middleware_body_limit: u64,
    compression: CompressionSettings,
    hidden_files: String,
    cache_control: String,
    etag: bool,
    request_timeout: Duration,
    trusted_proxies: Vec<IpNet>,
    /// Blocked-path patterns, pre-split at Router construction so the
    /// per-request path check is allocation-free.
    blocked_paths: Vec<CompiledGlob>,
    /// PHP-allowlist patterns, pre-split at Router construction so
    /// [`Router::is_php_allowed`] avoids per-request splitting.
    allowed_php_paths: Vec<CompiledGlob>,
    /// Trusted `Host` values, pre-lowercased at construction so
    /// [`Router::check_trusted_host`] lowercases the incoming host once and
    /// does a single-pass ASCII compare per entry — no per-request
    /// `eq_ignore_ascii_case` (which re-lowercases both sides every call).
    trusted_hosts: Box<[Box<str>]>,
    /// Config response headers precomputed as valid
    /// (HeaderName, HeaderValue) at Router construction. Entries with
    /// invalid names or values are dropped at startup with a warning
    /// so we don't repeat the parse per response.
    response_headers: Vec<(hyper::header::HeaderName, hyper::header::HeaderValue)>,
    /// Precomputed `Alt-Svc` value advertising the HTTP/3 endpoint, e.g.
    /// `h3=":443"; ma=86400`.
    ///
    /// `None` when HTTP/3 is disabled or `alt_svc_max_age = 0`. This header is
    /// the *only* way a browser learns HTTP/3 exists, so it goes on every
    /// TLS-terminated response — see [`Router::apply_alt_svc`].
    alt_svc: Option<hyper::header::HeaderValue>,
    store: Arc<Store>,
    /// Per-vhost KV stores. Cloned from the single instance `start_kv_service`
    /// builds and shares with the RESP listener, so a vhost's keyspace is the
    /// same `Arc<Store>` whether it is reached from PHP or over RESP.
    /// `None` outside multi-tenant mode.
    multi_tenant_kv: Option<ephpm_kv::multi_tenant::MultiTenantStore>,
    open_basedir: bool,
    /// Whether per-site database isolation is active (multi-site + embedded
    /// Turso, single-node). When true, the request's validated site key is
    /// pushed to the `ephpm_db_*` bridge before PHP runs, so each tenant's
    /// queries hit its own database. Derived from config in [`Router::new`] —
    /// the same condition ephpm-server's `is_per_site_sqlite` uses to register
    /// the resolver, so the two never disagree.
    per_site_db: bool,
    php_etag_cache_config: ephpm_config::PhpETagCacheConfig,
    metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
    metrics_path: String,
    limiter: Option<Arc<crate::rate_limit::Limiter>>,
    /// Preview-host mode (`[server] preview = true`). When set, every
    /// response — success, error, timeout, rate-limited — carries
    /// `X-Ephpm-Preview: 1` so a preview instance can never be mistaken for
    /// production. The limit side of the preset is resolved into the
    /// [`Router::limiter`] config before the router is built
    /// (`ServerConfig::effective_limits`); this flag only drives the marker
    /// header.
    preview: bool,
    file_cache: Option<Arc<crate::file_cache::FileCache>>,
    /// KV secret for deriving per-site RESP passwords. When set alongside
    /// `multi_tenant_kv`, `EPHPM_REDIS_*` env vars are injected into PHP.
    kv_secret: Option<String>,
    /// RESP listen address (used for `EPHPM_REDIS_HOST` / `EPHPM_REDIS_PORT`).
    kv_listen: String,
    /// Whether the RESP protocol listener is enabled.
    kv_redis_compat_enabled: bool,
    /// Database environment variables to inject into PHP `$_SERVER`.
    /// Populated from `[db.mysql]` or `[db.postgres]` when `inject_env = true`.
    ///
    /// Config-derived and process-global. In per-site mode the DB vars are
    /// per-request instead — see [`Router::per_site_db_wire`].
    db_env_vars: Vec<(String, String)>,
    /// Per-site `pdo_mysql` credentials, when the multi-tenant wire listener is
    /// running (`[server] sites_dir` + `[db.sqlite]`, single-node).
    ///
    /// Unlike [`Router::db_env_vars`] this cannot be computed once: each
    /// virtual host gets its **own** MySQL account, so the DB vars are built
    /// per request from the site key. Holds the same [`SiteWireAuth`] the
    /// listener verifies against, so what the router injects and what the
    /// listener accepts cannot drift.
    per_site_db_wire: Option<PerSiteDbWire>,
    /// This node's stable cluster identity, injected into PHP `$_SERVER` as
    /// `EPHPM_NODE_ID`. Set from the running gossip node's id (or the
    /// configured `[cluster] node_id` in single-node mode). `None` when no
    /// identity is available; PHP then sees no `EPHPM_NODE_ID` key.
    ///
    /// Environment-agnostic on purpose: it is a distinct value per node in
    /// BOTH the bare-process harness (config sets `node_id = "cluster-node-N"`)
    /// and the Kind StatefulSet (auto-derived `<pod-name>-<rand>` per pod),
    /// so cluster e2e tests can assert on it instead of the OS hostname (which
    /// collapses to one value when every node is a process on the same host).
    node_id: Option<String>,
    /// Caps concurrent PHP executions when `[php] workers > 0` (php-fpm
    /// `max_children` semantics). `None` = unlimited. This deliberately does
    /// NOT cap tokio's blocking pool — static file I/O and other blocking
    /// work must never be starved by slow PHP scripts.
    php_semaphore: Option<Arc<tokio::sync::Semaphore>>,
    /// Persistent worker pool when `[php] mode = "worker"`. `None` in fpm mode.
    /// When set, PHP requests are dispatched to the pool instead of running on
    /// the `spawn_blocking` path.
    worker_pool: Option<Arc<crate::worker_pool::WorkerPool>>,
    /// Dedicated FPM execution pool when `[php] fpm_engine = "pool"` (fpm mode
    /// only). `None` on the default `spawn_blocking` engine and in worker mode.
    /// When set, per-request PHP execution is dispatched to this pool instead of
    /// tokio's `spawn_blocking`, and the `php_semaphore` cap is bypassed (the
    /// pool size is the cap). Built in [`Router::new`]; drained on shutdown via
    /// [`Router::fpm_pool`].
    fpm_pool: Option<Arc<crate::fpm_pool::FpmPool>>,
    /// Per-vhost eBPF network policy handle. `None` unless
    /// `[server.tenant_network] ebpf_policy = true`. When `None`, no tag is
    /// written on the request path and the whole feature is zero-cost. When
    /// `Some`, the top of `run_php` tags the executing thread with the request's
    /// vhost id (cleared on the same thread when the closure returns) so the
    /// kernel `bind`/`connect` hooks can scope the tenant's loopback network.
    tenant_ebpf: Option<Arc<crate::tenant_ebpf::TenantEbpf>>,
    /// What a PHP-bound request does when no execution slot is available
    /// (`[php] overload_policy`, resolved against the `[server] preview`
    /// preset). [`OverloadPolicy::Wait`] is the historical behaviour — queue and
    /// wait; [`OverloadPolicy::Shed`] answers 503 + `Retry-After` instead
    /// (issue #301).
    overload_policy: ephpm_config::OverloadPolicy,
    /// Grace window a request may spend waiting for an execution slot before
    /// [`OverloadPolicy::Shed`] rejects it (`[php] shed_after_ms`). Zero = do not
    /// wait at all. Inert under [`OverloadPolicy::Wait`].
    shed_after: Duration,
    /// Request-body size (bytes) at/above which worker mode streams the body
    /// instead of buffering it (Phase 3). See `[php] worker_stream_threshold`.
    worker_stream_threshold: u64,
    /// Native middleware chain (`[[middleware]]`), evaluated on the PHP-bound
    /// path before the request body is read. `None` = no middleware mounted.
    middleware_chain: Option<Arc<crate::middleware::MiddlewareChain>>,
    /// Cluster-wide OPcache invalidation watcher (Phase 1). Consulted on
    /// every PHP request in fpm mode. Currently not wired into the worker-mode
    /// dispatch path (see `[opcache].cluster_invalidation` docs).
    opcache_watcher: crate::opcache::OpcacheWatcher,
    /// Negative-lookup cache for [`resolve_site`]: hosts that neither
    /// match a scanned site nor an on-disk site directory. Bot probes
    /// hit unknown Host headers constantly; without this cache each
    /// probe triggers a `sites_dir.join(host).is_dir()` syscall and
    /// (previously) a `tracing::info!` line per unknown hostname.
    ///
    /// Entries expire so lazy vhost discovery still works — a site
    /// deployed after the first probe becomes visible within
    /// [`UNKNOWN_SITE_TTL`].
    unknown_site_cache: dashmap::DashMap<String, std::time::Instant>,
    /// Set of per-vhost private state roots whose `tmp/` and `sessions/`
    /// subdirectories have already been created on disk. Populated by
    /// [`Router::ensure_vhost_private_dirs`] so the filesystem work
    /// (`create_dir_all` + permission tightening) happens once per site
    /// rather than on every request. Membership is keyed by the state root
    /// [`vhost_state_root`] derives from the resolved document root, so two
    /// requests to the same vhost coalesce onto one entry.
    ensured_vhost_dirs: dashmap::DashSet<PathBuf>,
    /// Cache of per-hostname derived KV site passwords (HMAC-SHA256 of
    /// `secret + hostname`). The HMAC is deterministic — computed once
    /// per host per process, then served from the DashMap for the rest
    /// of that process's lifetime.
    ///
    /// Keyed by the request's canonical site key when one matched, and by the
    /// normalized host otherwise (an unknown host keeps its own keyspace on the
    /// default document root). The second case is client-controlled, so the map
    /// is capped at [`SITE_PASSWORD_CACHE_MAX`] and past that point the HMAC is
    /// recomputed per request rather than stored.
    kv_site_password_cache: dashmap::DashMap<String, String>,
    /// Cache of per-site derived MySQL passwords, same shape and rationale as
    /// [`Router::kv_site_password_cache`].
    ///
    /// Bounded by the number of *validated* site keys, not by anything a client
    /// can invent: entries are only inserted for a request that resolved to a
    /// served vhost (an unknown host gets no per-site database identity at all —
    /// issue #291), so a flood of junk `Host` headers cannot grow this map.
    per_site_db_password_cache: dashmap::DashMap<String, String>,
    /// Canonicalized document roots, keyed by the as-configured root path,
    /// with the instant they were resolved. Caching removes a
    /// `canonicalize()` syscall from every static-file hit (issue #132),
    /// but entries are revalidated after a short TTL: `canonicalize()`
    /// resolves symlinks, and atomic-deploy layouts flip a symlinked
    /// docroot to a new release directory — an immortal cache would pin
    /// the OLD release forever.
    canonical_roots: dashmap::DashMap<PathBuf, (PathBuf, Instant)>,
    /// Canonicalized PHP *script* paths, keyed by the joined-but-unresolved
    /// path [`Router::resolve_fallback`] produced, with the instant they were
    /// resolved. Same shape and same reasoning as [`Router::canonical_roots`],
    /// applied to the other half of [`Router::php_script_contained`] so a PHP
    /// request no longer pays an O(path-depth) `realpath()` walk per hit.
    ///
    /// Only the canonicalized path is cached — never the containment verdict.
    /// The `starts_with(canonical_root)` test is recomputed on every request
    /// against the root resolved for *that* request, so one vhost's cached
    /// entry can never authorize another's, and a docroot that moves inside
    /// [`CANONICAL_ROOT_TTL`] re-decides immediately.
    ///
    /// Bounded by [`CANONICAL_SCRIPT_CACHE_MAX`]; see
    /// [`Router::remember_canonical_script`] for the behavior at the cap.
    canonical_scripts: dashmap::DashMap<PathBuf, (PathBuf, Instant)>,
    /// When [`Router::canonical_scripts`] was last swept of expired entries.
    /// The sweep is O(cache size), so it is throttled to at most once per
    /// [`CANONICAL_SCRIPT_TTL`] — otherwise a scanner that keeps the cache at
    /// its cap would turn every miss into a full scan, which is worse than the
    /// syscall this cache exists to remove.
    canonical_scripts_swept: std::sync::Mutex<Instant>,
    /// Inbound request-header names (lowercased) stripped UNCONDITIONALLY at
    /// ingest, before the middleware chain runs and before any header crosses
    /// to PHP. This closes two spoofing vectors:
    ///
    /// * `proxy` (httpoxy) — a forged `Proxy:` request header must never
    ///   surface as `$_SERVER['HTTP_PROXY']`.
    /// * every configured JWT `claims_header` — the `jwt` middleware only
    ///   `override_header`-sanitizes its claims header when it actually runs,
    ///   and in fpm mode it only runs when the request path matches the
    ///   module's `match` glob. A request to a non-matching path would
    ///   otherwise pass a client-forged claims header straight through to PHP.
    ///   Stripping at ingest makes a `match`-skipped (or bypassed) module
    ///   unable to leave a forged value behind.
    ///
    /// Always contains `"proxy"`; JWT claims-header names are appended at
    /// construction from `[[middleware]]` config (see
    /// [`ingest_strip_headers`]).
    ingest_strip_headers: Vec<String>,
    /// Upstream health of the configured SQL proxies, consulted by
    /// [`Router::readiness_check`]. `None` when the router was built without
    /// one (tests); readiness then ignores the database entirely.
    db_health: Option<Arc<crate::db_health::DbProxyHealth>>,
    /// Per-request timeline ring buffer served at `/_ephpm/requests`.
    /// `None` = disabled: nothing is recorded and the endpoint path falls
    /// through to normal routing like any other unknown `/_ephpm/` path.
    /// Enabled by default in dev mode, opt-in via
    /// `[server.diagnostics] request_log` in serve mode.
    request_log: Option<Arc<crate::timeline::RequestLog>>,
    /// Whether this node is currently the writable SQLite target, exposed at
    /// `/_ephpm/primary` so an external load balancer can route
    /// active-passive to the elected cluster primary.
    ///
    /// `true` means this node accepts writes: it is the elected
    /// clustered-SQLite primary, a non-clustered/standalone node (trivially
    /// writable), or **any node in per-site clustered mode** (ownership there
    /// is per tenant, and a non-owner forwards its writes to the site's owner,
    /// so every healthy node accepts writes for every site — see
    /// [`turso_cdc::start_clustered_per_site_turso`](crate::turso_cdc::start_clustered_per_site_turso)).
    /// `false` means this node is a **single-database** clustered-SQLite
    /// replica, whose writes would silently diverge — the LB must steer writes
    /// away from it.
    ///
    /// A lock-free `AtomicBool` so the `/_ephpm/primary` handler is a single
    /// relaxed load with no lock and no await on the request hot path. Defaults
    /// to a constant `true` (`Router::new`); in clustered-SQLite mode the CDC
    /// election path (`turso_cdc::start_clustered_turso_cdc`) shares this exact
    /// `Arc` and flips it on every role change (issue: primary-aware health
    /// endpoint).
    primary_view: Arc<AtomicBool>,
}

/// Header names always stripped from inbound requests at ingest, in addition
/// to any configured JWT `claims_header`. `proxy` defends against httpoxy
/// (CVE-2016-5385 and friends): a forged `Proxy:` request header must never
/// become PHP's `HTTP_PROXY`.
const ALWAYS_STRIPPED_INGEST_HEADERS: &[&str] = &["proxy"];

/// Build the lowercased list of inbound headers to strip at ingest.
///
/// Combines [`ALWAYS_STRIPPED_INGEST_HEADERS`] with the `claims_header` of
/// every configured `jwt` middleware mount. Because the claims header is only
/// sanitized by the jwt module when that module actually runs (and in fpm
/// mode it only runs on `match`-glob paths), stripping it up-front guarantees
/// a client can never forge it on a path the module skips. Names are
/// deduplicated and lowercased for the case-insensitive compare in
/// [`extract_headers`].
fn build_ingest_strip_headers(mounts: &[MiddlewareMount]) -> Vec<String> {
    let mut names: Vec<String> =
        ALWAYS_STRIPPED_INGEST_HEADERS.iter().map(|s| (*s).to_owned()).collect();
    for mount in mounts {
        // Only the builtin `jwt` module (and its long-form spellings) forwards
        // a claims header; match the same canonicalization the middleware
        // registry uses.
        let canonical = mount.library.replace('_', "-");
        if !matches!(canonical.as_str(), "jwt" | "ephpm-middleware-jwt") {
            continue;
        }
        if let Some(name) = mount
            .config
            .as_ref()
            .and_then(|c| c.get("claims_header"))
            .and_then(serde_json::Value::as_str)
            && !name.is_empty()
        {
            names.push(name.to_ascii_lowercase());
        }
    }
    names.sort_unstable();
    names.dedup();
    names
}

/// How long a cached canonicalized docroot stays valid before the next
/// request re-resolves it. Long enough to amortize the syscall away at any
/// realistic request rate, short enough that a symlink-flip deploy is
/// picked up almost immediately.
const CANONICAL_ROOT_TTL: Duration = Duration::from_secs(2);

/// How long a cached canonicalized PHP script path stays valid.
///
/// Deliberately *derived from* [`CANONICAL_ROOT_TTL`] rather than written as
/// its own number: both halves of [`Router::php_script_contained`] are cached,
/// and the containment guarantee is only as fresh as the staler of the two, so
/// the two windows must not be allowed to drift apart.
///
/// What the window costs, stated plainly: the check exists to stop a PHP file
/// that resolves outside its document root from executing. If a path is
/// resolved while contained, then swapped for a symlink pointing out of the
/// docroot, requests keep executing against the cached (contained) resolution
/// for up to this long. Doing that requires write access inside the document
/// root — the same capability that `[server] allowed_php_paths` exists to
/// contain — and two seconds is the same exposure the docroot side has shipped
/// with since the canonical-root cache landed. It is bounded and small; before
/// the containment check existed at all the window was unbounded.
const CANONICAL_SCRIPT_TTL: Duration = CANONICAL_ROOT_TTL;

/// Hard cap on [`Router::canonical_scripts`] entries.
///
/// The key is a filesystem path built from the request path, so its growth is
/// client-driven. `..` segments are rejected at ingest by
/// [`percent_decode_path`], which keeps the worst case from being arbitrary —
/// but it does not make the key space small. Every `.php` file under a
/// document root is a distinct key (thousands per WordPress install, times the
/// vhost count), and on the case-insensitive filesystems ePHPm also targets
/// (macOS, Windows) `Path` compares case-sensitively while the filesystem does
/// not, so `/InDeX.php` and `/index.php` are two keys for one file and the key
/// space is genuinely unbounded. An unbounded map on a client-controlled key
/// is a memory-exhaustion vector; a cap is not optional.
///
/// 4096 entries is far more than any real site's set of PHP entry points and
/// costs on the order of a megabyte of paths.
const CANONICAL_SCRIPT_CACHE_MAX: usize = 4096;

/// Hard cap on [`Router::kv_site_password_cache`] entries.
///
/// The derived-password caches are keyed by site identity. For a request that
/// resolved to a known vhost that is the canonical site key, so the map is
/// bounded by the fleet; but the KV cache is also consulted for hosts that
/// matched no site (they keep their own keyspace on the default document root),
/// and that key is whatever the client sent. Past the cap the HMAC is recomputed
/// per request instead of cached — cheap, and it keeps an unauthenticated
/// caller from growing the map by varying `Host`.
const SITE_PASSWORD_CACHE_MAX: usize = 4096;

/// How long a resolved per-site override is cached before the file is read again.
///
/// # Why a TTL rather than an mtime check or a watcher
///
/// The requirement is that an override takes effect without a server restart, on
/// an *already-discovered* site as well as a new one, and without a filesystem
/// watcher.
///
/// An mtime check does not actually cover it: it still costs a stat per request,
/// and it only detects changes to the *file*. The expensive and
/// security-relevant half of resolution is the canonicalization that proves
/// containment, and a symlink swap under the declared directory changes nothing
/// about the override file's mtime. A TTL re-does both, uniformly.
///
/// Deliberately **derived from** [`CANONICAL_ROOT_TTL`] rather than written as
/// its own number, for the same reason [`CANONICAL_SCRIPT_TTL`] is: this window
/// and the canonical-docroot window bound the same containment guarantee from
/// two sides, and letting them drift apart would make the weaker one invisible.
///
/// What the window costs, stated plainly: an operator who writes or fixes an
/// override sees it take effect within two seconds, not instantly. Requests in
/// that window use the previous (already validated, already contained)
/// resolution. Nothing unvalidated is ever served.
const SITE_CONFIG_TTL: Duration = CANONICAL_ROOT_TTL;

/// Hard cap on [`Router::site_roots_cache`] entries.
///
/// Unlike [`CANONICAL_SCRIPT_CACHE_MAX`], the key here is not client-driven: an
/// entry is only ever created for a canonical site key that resolved to a real
/// directory under `sites_dir`, so the map is bounded by the fleet the operator
/// deployed. The cap is defence in depth against a `sites_dir` with a pathological
/// number of entries; past it, resolution simply happens per request.
const SITE_ROOTS_CACHE_MAX: usize = 4096;

/// A resolved [`SiteRoots`] plus when it was resolved, for [`SITE_CONFIG_TTL`].
struct CachedSiteRoots {
    roots: SiteRoots,
    resolved_at: Instant,
}

/// Resolve one vhost's [`SiteRoots`] from its operator-supplied override file.
///
/// `overrides_dir` is `[server] site_overrides_dir` — a directory outside
/// `sites_dir`, so no tenant can write it (see [`crate::site_overrides`] for why
/// that placement is the whole design). Every failure mode (absent, unreadable,
/// malformed, or declaring a `document_root` that escapes its container)
/// collapses to "serve the container", which is the behaviour that predates this
/// mechanism.
///
/// Note what this function structurally cannot do: it returns a [`SiteRoots`]
/// whose `container` is the argument it was given. There is no path by which an
/// override file influences the container, and therefore none by which it
/// influences `open_basedir`.
fn resolve_site_roots(container: PathBuf, overrides_dir: &Path, site_key: &str) -> SiteRoots {
    match crate::site_overrides::load(overrides_dir, site_key, &container).document_root {
        Some(document_root) => SiteRoots { document_root, container },
        None => SiteRoots::flat(container),
    }
}

/// Scan `sites_dir` for virtual host subdirectories.
///
/// Each subdirectory becomes a virtual host keyed by its name (lowercased).
/// Returns an empty map if `sites_dir` is `None`.
///
/// Per-site overrides are **not** read here — [`Router::site_roots`] owns that,
/// so the startup path and the lazy path share one implementation and one
/// freshness window. `Router::new` seeds the cache immediately afterwards, which
/// is also what produces the startup log naming every site serving from an
/// overridden root.
fn scan_sites_dir(
    sites_dir: Option<&Path>,
    default_index_files: &[String],
    default_websocket_files: &[String],
    default_fallback: &[String],
) -> HashMap<String, SiteConfig> {
    let Some(dir) = sites_dir else {
        return HashMap::new();
    };

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(path = %dir.display(), %e, "failed to read sites_dir");
            return HashMap::new();
        }
    };

    let mut sites = HashMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let host = name.to_ascii_lowercase();
        tracing::info!(host = %host, path = %path.display(), "discovered virtual host");
        sites.insert(
            host,
            SiteConfig {
                container: path,
                index_files: default_index_files.to_vec(),
                websocket_files: default_websocket_files.to_vec(),
                fallback: default_fallback.to_vec(),
            },
        );
    }

    if sites.is_empty() {
        tracing::warn!(path = %dir.display(), "sites_dir is empty — no virtual hosts configured");
    } else {
        tracing::info!(count = sites.len(), "virtual hosts loaded");
    }

    sites
}

impl Router {
    /// Build the router.
    ///
    /// `multi_tenant_kv` must be the process-wide instance created by
    /// `start_kv_service` — the same handle the RESP listener is given. The
    /// router does **not** create its own: a `MultiTenantStore` owns the
    /// hostname → `Arc<Store>` map that defines a vhost's keyspace, so a
    /// second instance would give the PHP path a different set of stores than
    /// RESP clients get for the same hostnames. Pass `None` when
    /// `[server] sites_dir` is unset (single-site mode, where every request
    /// uses `store` directly) or in tests that never exercise per-vhost KV.
    #[must_use]
    pub fn new(
        config: &Config,
        store: Arc<Store>,
        multi_tenant_kv: Option<ephpm_kv::multi_tenant::MultiTenantStore>,
        metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
        limiter: Option<Arc<crate::rate_limit::Limiter>>,
        file_cache: Option<Arc<crate::file_cache::FileCache>>,
        worker_pool: Option<Arc<crate::worker_pool::WorkerPool>>,
    ) -> Self {
        let port =
            config.server.listen.rsplit_once(':').and_then(|(_, p)| p.parse().ok()).unwrap_or(8080);

        let security = config.server.security.as_ref();

        let trusted_proxies: Vec<IpNet> = security
            .map(|s| s.trusted_proxies.as_slice())
            .unwrap_or_default()
            .iter()
            .filter_map(|cidr| {
                cidr.parse::<IpNet>()
                    .map_err(|e| tracing::warn!(cidr, %e, "ignoring invalid trusted_proxy"))
                    .ok()
            })
            .collect();

        let open_basedir = config.server.effective_open_basedir();
        if config.server.sites_dir.is_some() {
            // Multi-tenant deployments get isolation by default; surface the
            // resolved values so an explicit opt-out is never silent.
            tracing::info!(
                open_basedir,
                disable_shell_exec = config.server.effective_disable_shell_exec(),
                "multi-tenant security defaults resolved"
            );
            if !open_basedir {
                tracing::warn!(
                    "open_basedir explicitly disabled in multi-tenant mode — \
                     sites can read each other's files"
                );
            }
        }

        // Operator-supplied per-site overrides. `None` in single-site mode and
        // when `site_overrides_dir` is unset.
        let site_overrides_dir =
            config.server.effective_site_overrides_dir().map(Path::to_path_buf);
        // Never a silent no-op: the knob only acts in multi-tenant mode, and
        // `ephpm-config` has no `tracing` dependency to say so itself.
        if config.server.site_overrides_dir.is_some() && config.server.sites_dir.is_none() {
            tracing::warn!(
                "[server] site_overrides_dir is set but [server] sites_dir is not — per-site \
                 overrides only apply to virtual hosts, so this is ignored. In single-site \
                 mode point [server] document_root at the web root directly."
            );
        }

        // Scan sites_dir for virtual host directories.
        let sites = scan_sites_dir(
            config.server.sites_dir.as_deref(),
            &config.server.index_files,
            &config.server.websocket_files,
            &config.server.fallback,
        );

        // Experimental dedicated FPM thread pool (`[php] fpm_engine = "pool"`,
        // fpm mode only). The pool size IS the concurrency cap for this engine,
        // so the `workers` semaphore below is redundant and deliberately
        // bypassed. `is_pool_engine()` is already false in worker mode, so the
        // worker pool and this pool can never both be active.
        let fpm_pool = if config.php.is_pool_engine() {
            let thread_count = config.php.effective_worker_count();
            let backlog = config.php.effective_worker_backlog();
            if config.php.workers > 0 {
                tracing::warn!(
                    workers = config.php.workers,
                    "[php] workers is ignored when [php] fpm_engine = \"pool\" — the pool \
                     size ({thread_count}) is the concurrency cap",
                    thread_count = thread_count,
                );
            }
            Some(crate::fpm_pool::FpmPool::spawn(thread_count, backlog))
        } else {
            None
        };

        // The `workers` semaphore caps the default `spawn_blocking` engine only.
        // In pool mode the pool itself is the cap, so leave it `None`.
        let php_semaphore = (fpm_pool.is_none() && config.php.workers > 0)
            .then(|| Arc::new(tokio::sync::Semaphore::new(config.php.workers)));

        // Multi-tenant iff a sites_dir is configured or vhost directories were
        // found. Resolved once here so the WebSocket scope derivation in
        // `site_identities` is a field read, not a re-scan.
        let multi_site = config.server.sites_dir.is_some() || !sites.is_empty();

        let router = Self {
            document_root: config.server.document_root.clone(),
            sites,
            multi_site,
            websocket: None,
            proxy: crate::proxy::ProxyEngine::new(&config.server.proxy),
            self_weak: std::sync::Weak::new(),
            websocket_files: config.server.websocket_files.clone(),
            sites_dir: config.server.sites_dir.clone(),
            site_overrides_dir,
            site_roots_cache: dashmap::DashMap::new(),
            sites_domain_suffix: config
                .server
                .sites_domain_suffix
                .as_ref()
                .map(|s| s.to_ascii_lowercase()),
            index_files: config.server.index_files.clone(),
            fallback: config.server.fallback.clone(),
            server_port: port,
            max_body_size: config.server.request.max_body_size,
            middleware_body_limit: config.server.request.middleware_body_limit,
            compression: CompressionSettings {
                enabled: config.server.response.compression,
                level: config.server.response.compression_level,
                min_size: config.server.response.compression_min_size,
                streaming: StreamingCompression::parse(
                    &config.server.response.compression_streaming,
                )
                .unwrap_or_else(|| {
                    tracing::warn!(
                        value = %config.server.response.compression_streaming,
                        "unknown [server.response] compression_streaming value \
                         (expected \"off\", \"sse\", or \"all\") — falling back to \"off\""
                    );
                    StreamingCompression::Off
                }),
            },
            hidden_files: config.server.static_files.hidden_files.clone(),
            cache_control: config.server.static_files.cache_control.clone(),
            etag: config.server.static_files.etag,
            request_timeout: Duration::from_secs(config.server.timeouts.request),
            trusted_proxies,
            blocked_paths: security
                .map(|s| s.blocked_paths.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|p| CompiledGlob::compile(p))
                .collect(),
            allowed_php_paths: security
                .map(|s| s.allowed_php_paths.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|p| CompiledGlob::compile(p))
                .collect(),
            trusted_hosts: config
                .server
                .request
                .trusted_hosts
                .iter()
                .map(|h| h.to_ascii_lowercase().into_boxed_str())
                .collect(),
            response_headers: config
                .server
                .response
                .headers
                .iter()
                .filter_map(|[k, v]| {
                    match (
                        hyper::header::HeaderName::from_bytes(k.as_bytes()),
                        hyper::header::HeaderValue::from_str(v),
                    ) {
                        (Ok(name), Ok(value)) => Some((name, value)),
                        _ => {
                            tracing::warn!(
                                header = %k,
                                "[server.response] headers entry is not a valid HTTP header — skipped at startup"
                            );
                            None
                        }
                    }
                })
                .collect(),
            // Filled in by `with_alt_svc` once the QUIC endpoint has bound.
            alt_svc: None,
            open_basedir,
            // Per-request tenant-key gate must be true whenever a per-site DB
            // context exists — the single-node per-site path OR the per-site
            // *clustered* path (`is_per_site_sqlite` excludes clustered configs
            // by construction, so it alone would leave every clustered request
            // with `db = None` → "no per-site database context").
            per_site_db: crate::is_per_site_sqlite(config, config.cluster.enabled)
                || crate::is_per_site_clustered(config, config.cluster.enabled),
            multi_tenant_kv,
            store,
            php_etag_cache_config: config.server.php_etag_cache.clone(),
            metrics_handle,
            metrics_path: config.server.metrics.path.clone(),
            limiter,
            preview: config.server.preview,
            file_cache,
            kv_secret: config.kv.secret.clone(),
            kv_listen: config.kv.redis_compat.listen.clone(),
            kv_redis_compat_enabled: config.kv.redis_compat.enabled,
            db_env_vars: build_db_env_vars(config),
            // Wired by `serve()` via `with_per_site_db_wire` only if the
            // multi-tenant wire listener actually starts. Left `None` here so
            // a router built without it injects no DB credentials at all
            // rather than credentials nothing is listening for.
            per_site_db_wire: None,
            // Prefer the explicit `[cluster] node_id`; `serve()` overrides this
            // via `with_node_id` with the effective gossip id once clustering
            // is up (that id is auto-derived per node when config leaves it
            // empty). Empty config value => None so PHP sees no key here.
            node_id: {
                let id = config.cluster.node_id.trim();
                if id.is_empty() { None } else { Some(id.to_string()) }
            },
            php_semaphore,
            worker_pool,
            fpm_pool,
            // Wired by `serve()` via `with_tenant_ebpf` only when
            // `[server.tenant_network] ebpf_policy = true` (Linux multi-tenant).
            // `None` here => no per-request tagging, zero cost.
            tenant_ebpf: None,
            overload_policy: config.effective_overload_policy(),
            shed_after: Duration::from_millis(config.php.shed_after_ms),
            worker_stream_threshold: config.php.worker_stream_threshold,
            middleware_chain: None,
            opcache_watcher: {
                let enabled = config.opcache.effective_cluster_invalidation(config.cluster.enabled);
                if enabled && config.php.is_worker_mode() {
                    // Never a silent no-op: worker-mode wiring is a future
                    // phase, so surface it at startup instead.
                    tracing::warn!(
                        "[opcache] cluster_invalidation is enabled but [php] mode = \
                         \"worker\" — Phase 1 of OPcache clustering wires the watcher on \
                         the fpm dispatch path only. Worker-mode invalidation is planned; \
                         see site/content/roadmap/opcache-clustering.md."
                    );
                } else if enabled {
                    tracing::info!(
                        cluster_enabled = config.cluster.enabled,
                        "[opcache] cluster_invalidation enabled — watching opcache:version:<vhost>"
                    );
                }
                crate::opcache::OpcacheWatcher::new(enabled)
            },
            unknown_site_cache: dashmap::DashMap::new(),
            ensured_vhost_dirs: dashmap::DashSet::new(),
            kv_site_password_cache: dashmap::DashMap::new(),
            per_site_db_password_cache: dashmap::DashMap::new(),
            canonical_roots: dashmap::DashMap::new(),
            canonical_scripts: dashmap::DashMap::new(),
            canonical_scripts_swept: std::sync::Mutex::new(Instant::now()),
            ingest_strip_headers: build_ingest_strip_headers(&config.middleware),
            db_health: None,
            request_log: None,
            // Default: this node is a writable SQLite target (standalone /
            // non-clustered). Clustered-SQLite mode replaces this with the
            // election's shared view via `with_primary_view`.
            primary_view: Arc::new(AtomicBool::new(true)),
        };

        // Read every known site's override once, now: it seeds the cache (so the
        // first request to each vhost does not pay the read) and it produces the
        // startup log naming the sites that serve from an overridden root.
        // Lazily discovered sites log the same line on first resolution.
        router.seed_site_roots();
        router
    }

    /// Resolve and cache the override for every startup-scanned site, then name
    /// the ones that moved in a single log line.
    ///
    /// Operators need to see, without making a request, which overrides are in
    /// effect and where they point. An override that names a directory which
    /// does not exist presents as a 404 with no other symptom, and "check
    /// whether the provisioning daemon wrote the override" is not an obvious
    /// first hypothesis.
    fn seed_site_roots(&self) {
        if self.site_overrides_dir.is_none() || self.sites.is_empty() {
            return;
        }
        let mut declared: Vec<String> = Vec::new();
        // Collect keys first: `site_roots` takes its own map guards, and holding
        // an iteration guard over `self.sites` across that is fine (different
        // map), but the borrow of `site.container` is not — clone the pairs.
        let scanned: Vec<(String, PathBuf)> =
            self.sites.iter().map(|(host, site)| (host.clone(), site.container.clone())).collect();
        for (host, container) in scanned {
            let roots = self.site_roots(&host, container);
            if roots.declared() {
                declared.push(format!("{host} -> {}", roots.document_root.display()));
            }
        }
        if !declared.is_empty() {
            declared.sort_unstable();
            tracing::info!(
                overrides_dir = %self.site_overrides_dir.as_deref().unwrap_or(Path::new("")).display(),
                count = declared.len(),
                sites = %declared.join("; "),
                "per-site document root overrides applied — these vhosts serve from a \
                 subdirectory of their site container; open_basedir still resolves to the \
                 container, so PHP can require from above the web root"
            );
        }
    }

    /// This vhost's [`SiteRoots`], reading its override file at most once per
    /// [`SITE_CONFIG_TTL`].
    ///
    /// The single resolution point for both discovery paths. `container` is
    /// always a directory ePHPm chose (a child of `sites_dir`); an override can
    /// only narrow `document_root` within it, never touch `container`.
    fn site_roots(&self, site_key: &str, container: PathBuf) -> SiteRoots {
        let Some(overrides_dir) = self.site_overrides_dir.as_deref() else {
            return SiteRoots::flat(container);
        };

        if let Some(hit) = self.site_roots_cache.get(site_key)
            && hit.resolved_at.elapsed() < SITE_CONFIG_TTL
            // Guard against a container that changed under a stable key (a
            // reconfigured `sites_dir`): the cached roots must still describe
            // the directory we were asked about.
            && hit.roots.container == container
        {
            return hit.roots.clone();
        }

        let roots = resolve_site_roots(container, overrides_dir, site_key);

        // Log a transition rather than every re-read: once when a site first
        // resolves to an overridden root, and again whenever it changes. A 2s
        // TTL means the unconditional version would emit a line every two
        // seconds per site, forever.
        let previous =
            self.site_roots_cache.get(site_key).map(|hit| hit.roots.document_root.clone());
        if previous.as_ref() != Some(&roots.document_root) {
            if roots.declared() {
                tracing::info!(
                    site = site_key,
                    container = %roots.container.display(),
                    document_root = %roots.document_root.display(),
                    "site serves from an overridden document root"
                );
            } else if previous.is_some() {
                tracing::info!(
                    site = site_key,
                    container = %roots.container.display(),
                    "per-site document root override removed — serving the site container"
                );
            }
        }

        // Only grow the map up to the cap; past it, resolve per request rather
        // than let a pathological `sites_dir` grow it without bound. Existing
        // keys are always refreshed so a cached entry cannot go stale forever.
        if self.site_roots_cache.len() < SITE_ROOTS_CACHE_MAX
            || self.site_roots_cache.contains_key(site_key)
        {
            self.site_roots_cache.insert(
                site_key.to_string(),
                CachedSiteRoots { roots: roots.clone(), resolved_at: Instant::now() },
            );
        }
        roots
    }

    /// Attach (or leave off) the request-timeline ring buffer resolved in
    /// `serve()`. Kept out of `new()`'s signature so its existing call sites
    /// stay unchanged; `None` keeps the timeline disabled.
    #[must_use]
    pub(crate) fn with_request_log(
        mut self,
        request_log: Option<Arc<crate::timeline::RequestLog>>,
    ) -> Self {
        self.request_log = request_log;
        self
    }

    /// Canonicalized form of a site's document root, cached with a short
    /// TTL ([`CANONICAL_ROOT_TTL`]). The TTL matters: `canonicalize()`
    /// resolves symlinks, and atomic-deploy layouts (docroot → symlink →
    /// `releases/N`) flip that symlink on deploy — a permanent cache would
    /// keep serving the old release. Returns `None` when the root does not
    /// exist — the caller treats that as 404, matching the previous
    /// per-request `canonicalize()` behavior.
    fn canonical_root(&self, root: &Path) -> Option<PathBuf> {
        if let Some(hit) = self.canonical_roots.get(root) {
            let (canon, resolved_at) = hit.value();
            if resolved_at.elapsed() < CANONICAL_ROOT_TTL {
                return Some(canon.clone());
            }
        }
        let canon = root.canonicalize().ok()?;
        self.canonical_roots.insert(root.to_path_buf(), (canon.clone(), Instant::now()));
        Some(canon)
    }

    /// Canonicalize a site's document root and re-assert, on **every** resolve,
    /// that it still lies within the site container.
    ///
    /// This is the containment re-check for #394. [`crate::site_overrides`]'s
    /// `validate_declared_root` checks `starts_with(container)` exactly once,
    /// when the override file is read, but the resolved `document_root` is
    /// dereferenced live and cached by value: a tenant can pass validation with
    /// a real `web/` directory and then, post-boot, replace it with a symlink
    /// out of its container (`rmdir web; symlink('/', 'web')` — PHP's
    /// `symlink()` only `open_basedir`-checks the link, never its target). The
    /// next `canonicalize()` follows the symlink out, and the **static path has
    /// no `open_basedir` backstop** — [`static_files::serve_file`]'s only
    /// boundary is the root this returns. So containment is recomputed here,
    /// against the container (which is never override-controlled), every time —
    /// not trusted from load time and not cached as a verdict.
    ///
    /// Fails **closed**: returns `None` when the document root cannot be
    /// resolved, or when it has escaped its container. The static caller turns
    /// `None` into a 404 and the PHP caller into a 403 — the escaped path is
    /// never served. When no override moved the web root
    /// (`document_root == container`) containment holds by construction and the
    /// second `canonicalize` is skipped, so every non-override site and
    /// single-site mode pays nothing new.
    ///
    /// Both sides come from [`Router::canonical_root`], i.e. both from
    /// `canonicalize()`, so on Windows they share the same `\\?\` verbatim form
    /// and `starts_with` compares like with like — a verbatim/non-verbatim
    /// mismatch would itself be a bypass.
    fn contained_canonical_root(&self, roots: &SiteRoots) -> Option<PathBuf> {
        let canonical_root = self.canonical_root(&roots.document_root)?;

        // No override in effect: the document root IS the container, so
        // containment is definitional. The hot path — every site without an
        // override file, and single-site mode.
        if roots.document_root == roots.container {
            return Some(canonical_root);
        }

        // An override moved the web root. The container is operator-chosen and
        // cannot be swapped by the tenant, so its canonical form is the
        // trustworthy boundary to re-check against.
        let Some(canonical_container) = self.canonical_root(&roots.container) else {
            tracing::warn!(
                document_root = %roots.document_root.display(),
                container = %roots.container.display(),
                "per-site container did not resolve — refusing the overridden document root"
            );
            return None;
        };
        if !canonical_root.starts_with(&canonical_container) {
            tracing::warn!(
                document_root = %roots.document_root.display(),
                resolved = %canonical_root.display(),
                container = %canonical_container.display(),
                "per-site document root escaped its container after validation \
                 (symlink swap) — refusing (404/403), not serving the escaped path"
            );
            return None;
        }
        Some(canonical_root)
    }

    /// Require a resolved PHP script to live inside its site's document root.
    ///
    /// The static branch gets this for free from `static_files::serve_file`,
    /// which canonicalizes the file and checks
    /// `starts_with(canonical_root)`. The PHP branch had only
    /// [`Router::is_php_allowed`], a URI *prefix* test that cannot see
    /// symlinks or dot segments — so a script reachable through a symlink
    /// pointing out of the docroot executed anyway, which under `sites_dir`
    /// vhosting is cross-tenant code execution.
    ///
    /// Both sides are cached: the root through
    /// [`Router::contained_canonical_root`], the script through
    /// [`Router::canonical_scripts`]. `canonicalize()` is a `realpath()` walk —
    /// one `lstat` per path component, so an O(path-depth) burst of syscalls —
    /// and paying it uncached on every PHP request would undo the same syscall
    /// the docroot cache was added to remove. What is *not* cached is the
    /// decision: `contained_canonical_root` re-asserts the root is inside the
    /// container (fix for #394), and the `starts_with(canonical_root)`
    /// comparison below runs on every request against the root resolved for
    /// that request, so a cache entry can only ever save work, never grant
    /// reach.
    fn php_script_contained(&self, fs_path: &Path, roots: &SiteRoots) -> bool {
        let Some(canonical_root) = self.contained_canonical_root(roots) else {
            return false;
        };

        if let Some(cached) = self.cached_script_contained(fs_path, &canonical_root) {
            if !cached {
                Self::warn_traversal(fs_path, &canonical_root);
            }
            return cached;
        }

        let Ok(canonical_script) = fs_path.canonicalize() else {
            return false;
        };
        if !canonical_script.starts_with(&canonical_root) {
            Self::warn_traversal(fs_path, &canonical_root);
            return false;
        }
        // Only in-docroot resolutions are worth remembering. A script that
        // resolved outside is an attack or a misconfiguration: it gets a 403
        // either way, it does not need to be fast, and keeping it out means
        // attacker-shaped paths cannot crowd real entry points out of a
        // bounded cache quite as easily.
        self.remember_canonical_script(fs_path, canonical_script);
        true
    }

    /// Test helper: containment against a flat site (`document_root ==
    /// container`, the no-override shape), so the script-containment suite reads
    /// `(script, root)` without constructing a [`SiteRoots`] each time.
    #[cfg(test)]
    fn php_script_contained_flat(&self, fs_path: &Path, root: &Path) -> bool {
        self.php_script_contained(fs_path, &SiteRoots::flat(root.to_path_buf()))
    }

    /// Single place the containment failure is logged, so the cached and
    /// uncached branches of [`Router::php_script_contained`] cannot drift.
    fn warn_traversal(fs_path: &Path, canonical_root: &Path) {
        tracing::warn!(
            path = %fs_path.display(),
            root = %canonical_root.display(),
            "PHP path traversal attempt blocked"
        );
    }

    /// Containment answer from [`Router::canonical_scripts`] alone, or `None`
    /// when there is no entry or the entry is older than
    /// [`CANONICAL_SCRIPT_TTL`].
    ///
    /// The comparison happens here, under the map guard, for two reasons: it
    /// avoids cloning the cached `PathBuf` onto the hot path, and it makes it
    /// structurally impossible to cache the verdict — `canonical_root` is an
    /// argument, so the answer is always recomputed for the caller's root.
    fn cached_script_contained(&self, fs_path: &Path, canonical_root: &Path) -> Option<bool> {
        let hit = self.canonical_scripts.get(fs_path)?;
        let (canonical_script, resolved_at) = hit.value();
        if resolved_at.elapsed() >= CANONICAL_SCRIPT_TTL {
            return None;
        }
        Some(canonical_script.starts_with(canonical_root))
    }

    /// Record a freshly resolved script path, keeping the cache under
    /// [`CANONICAL_SCRIPT_CACHE_MAX`].
    ///
    /// At the cap the cache first drops everything already past its TTL (at
    /// most once per [`CANONICAL_SCRIPT_TTL`], since that sweep is O(cache
    /// size)); if it is still full of live entries the new path is simply not
    /// remembered and the request pays the `canonicalize()` it already paid.
    /// That is deliberate: the degraded state is exactly the pre-cache
    /// behavior, so a client that floods the cache can cost the server back
    /// the optimization but can never cost it correctness or unbounded memory.
    /// No LRU, no eviction bookkeeping on the hot path.
    fn remember_canonical_script(&self, fs_path: &Path, canonical_script: PathBuf) {
        if self.canonical_scripts.len() >= CANONICAL_SCRIPT_CACHE_MAX {
            // `try_lock` rather than `lock`: if another thread is already
            // sweeping there is nothing useful to wait for, and a request
            // thread must never block here.
            let Ok(mut last_sweep) = self.canonical_scripts_swept.try_lock() else {
                return;
            };
            if last_sweep.elapsed() < CANONICAL_SCRIPT_TTL {
                return;
            }
            self.canonical_scripts.retain(|_, entry| entry.1.elapsed() < CANONICAL_SCRIPT_TTL);
            *last_sweep = Instant::now();
            if self.canonical_scripts.len() >= CANONICAL_SCRIPT_CACHE_MAX {
                return;
            }
        }
        self.canonical_scripts.insert(fs_path.to_path_buf(), (canonical_script, Instant::now()));
    }

    /// Attach the native middleware chain loaded in `serve()` at startup.
    /// Kept out of `new()`'s signature so its many existing call sites (all
    /// middleware-free) stay unchanged.
    #[must_use]
    pub fn with_middleware_chain(
        mut self,
        chain: Option<Arc<crate::middleware::MiddlewareChain>>,
    ) -> Self {
        self.middleware_chain = chain;
        self
    }

    /// The dedicated FPM execution pool, when `[php] fpm_engine = "pool"` built
    /// one. `serve()` uses this to drain the pool on shutdown (close dispatch,
    /// wait for threads to release their TSRM slots) before PHP teardown, the
    /// same way it drains the worker pool. `None` on the default engine.
    #[must_use]
    pub fn fpm_pool(&self) -> Option<Arc<crate::fpm_pool::FpmPool>> {
        self.fpm_pool.clone()
    }

    /// Advertise an HTTP/3 endpoint via `Alt-Svc` on TLS responses.
    ///
    /// Called by `serve()` **after** the QUIC endpoint has actually bound, so
    /// ePHPm never advertises an HTTP/3 port that isn't listening — a stale
    /// `Alt-Svc` costs clients a failed connection attempt and a fallback on
    /// every request until `ma` expires.
    ///
    /// `port` is the UDP port the QUIC endpoint bound; `max_age` of 0
    /// suppresses the header (see `[server.http3] alt_svc_max_age`).
    #[must_use]
    pub fn with_alt_svc(mut self, port: u16, max_age: u64) -> Self {
        self.alt_svc = crate::http3::alt_svc_value(port, max_age).and_then(|value| {
            match hyper::header::HeaderValue::from_str(&value) {
                Ok(header) => Some(header),
                Err(err) => {
                    tracing::warn!(%err, %value, "computed Alt-Svc value is not a valid header");
                    None
                }
            }
        });
        self
    }

    /// Add the `Alt-Svc` HTTP/3 advertisement to a response.
    ///
    /// Only TLS-terminated responses get it: an `http://` origin cannot
    /// upgrade to HTTP/3 (QUIC mandates TLS), so advertising there would just
    /// be noise. Responses served *over* HTTP/3 carry it too, matching nginx
    /// and Caddy — it keeps the advertisement fresh for clients that later
    /// fall back to TCP.
    fn apply_alt_svc(&self, response: &mut Response<ServerBody>, is_tls: bool) {
        if !is_tls {
            return;
        }
        if let Some(value) = &self.alt_svc {
            response.headers_mut().insert(hyper::header::ALT_SVC, value.clone());
        }
    }

    /// Add the `X-Ephpm-Preview: 1` marker when `[server] preview = true`.
    ///
    /// Applied to EVERY response the router produces — success, 4xx/5xx,
    /// timeout 504, and both rate-limit 429s — so no path out of a preview
    /// instance can be mistaken for production. (The accept-time raw 503
    /// shed in `lib.rs` is the one exception: it is written before HTTP
    /// parsing exists.)
    fn apply_preview_marker(&self, response: &mut Response<ServerBody>) {
        if self.preview {
            response.headers_mut().insert(
                hyper::header::HeaderName::from_static("x-ephpm-preview"),
                hyper::header::HeaderValue::from_static("1"),
            );
        }
    }

    /// Wrap the finished router in the `Arc` the server shares across
    /// connections, recording the weak self-reference WebSocket sessions need.
    ///
    /// Use this instead of `Arc::new(router)`: a session task keeps dispatching
    /// PHP events long after the request that created it returned, so it needs
    /// a handle on the router that does not borrow from a request.
    /// `Arc::new_cyclic` makes that handle a construction-time guarantee rather
    /// than a step someone can forget.
    #[must_use]
    pub fn share(self) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self { self_weak: weak.clone(), ..self })
    }

    /// Attach the native WebSocket runtime.
    ///
    /// `None` (the default) leaves the feature off: an upgrade request is then
    /// routed exactly like any other GET, so enabling `[server.websocket]` is
    /// the only thing that changes upgrade routing.
    #[must_use]
    pub fn with_websocket(mut self, websocket: Option<Arc<crate::websocket::WsRuntime>>) -> Self {
        self.websocket = websocket;
        self
    }

    /// Attach the per-vhost eBPF network policy handle
    /// (`[server.tenant_network] ebpf_policy`). `None` (the default) leaves the
    /// feature off and the request path untagged. `serve()` loads and attaches
    /// the programs before calling this, so a `Some` here means the kernel hooks
    /// are already live.
    #[must_use]
    pub fn with_tenant_ebpf(
        mut self,
        tenant_ebpf: Option<Arc<crate::tenant_ebpf::TenantEbpf>>,
    ) -> Self {
        self.tenant_ebpf = tenant_ebpf;
        self
    }

    /// Override this node's `EPHPM_NODE_ID` (injected into PHP `$_SERVER`)
    /// with the effective runtime cluster identity.
    ///
    /// `serve()` passes the running gossip node's id here so PHP sees the same
    /// distinct-per-node value the cluster uses internally -- including the
    /// auto-derived id when `[cluster] node_id` is left empty (e.g. the Kind
    /// StatefulSet, where each pod gets `<pod-name>-<rand>`). A `None` or empty
    /// argument leaves whatever `new()` derived from config untouched.
    #[must_use]
    pub fn with_node_id(mut self, node_id: Option<String>) -> Self {
        if let Some(id) = node_id {
            let id = id.trim();
            if !id.is_empty() {
                self.node_id = Some(id.to_string());
            }
        }
        self
    }

    /// Attach the SQL proxies' upstream health to the readiness probe.
    ///
    /// `serve()` builds one [`DbProxyHealth`](crate::db_health::DbProxyHealth)
    /// from config before the listeners are bound and hands the same handle
    /// to proxy startup, so `/_ephpm/ready` reports 503 until every
    /// configured proxy has reached its upstream once.
    #[must_use]
    pub fn with_db_health(mut self, db_health: Arc<crate::db_health::DbProxyHealth>) -> Self {
        self.db_health = Some(db_health);
        self
    }

    /// Share the clustered-SQLite election's "am I the primary?" view, exposed
    /// at `/_ephpm/primary` for active-passive load-balancer routing.
    ///
    /// `serve()` passes the *same* `Arc<AtomicBool>` that
    /// [`turso_cdc::start_clustered_turso_cdc`](crate::turso_cdc::start_clustered_turso_cdc)
    /// flips on every election role change, so `/_ephpm/primary` tracks the
    /// live role with a single relaxed atomic load — no lock, no await on the
    /// request path. Left unset (single-node / non-clustered), the router keeps
    /// the constant-`true` view `new()` installed, and `/_ephpm/primary`
    /// reports 200 because a standalone node is trivially writable.
    #[must_use]
    pub fn with_primary_view(mut self, primary_view: Arc<AtomicBool>) -> Self {
        self.primary_view = primary_view;
        self
    }

    /// Inject per-site `pdo_mysql` credentials for the multi-tenant wire
    /// listener.
    ///
    /// `serve()` calls this **only after** `start_per_site_wire` has validated
    /// and bound that address, and it passes the *same*
    /// [`SiteWireAuth`](crate::site_wire_auth::SiteWireAuth) instance the
    /// listener verifies against. That ordering is what keeps the two halves
    /// honest: the router never advertises a `DB_HOST`/`DB_PASSWORD` for an
    /// endpoint that could not be bound (that case is a fatal startup error),
    /// and the listener never sees a credential the router did not mint.
    ///
    /// Left unset, a tenant's requests carry no `DB_*` at all — a visibly
    /// absent database config, never one silently pointed at something shared.
    ///
    /// `listen` is the bound MySQL address (`host:port`) tenants connect to.
    #[must_use]
    pub fn with_per_site_db_wire(
        mut self,
        auth: crate::site_wire_auth::SiteWireAuth,
        listen: String,
    ) -> Self {
        self.per_site_db_wire = Some(PerSiteDbWire { auth, listen });
        self
    }

    /// Resolve the site configuration from the `Host` header.
    ///
    /// Returns the document root, index files, and fallback chain for the
    /// matched site. Falls back to global defaults if no site matches or
    /// vhosting is disabled.
    ///
    /// Uses lazy discovery: if a host isn't in the startup-scanned registry
    /// but a matching directory exists in `sites_dir`, it is served immediately.
    /// This means new sites can be deployed without restarting ephpm.
    /// Build the per-site `DB_*` variables that point this request's PHP at
    /// its **own** database over `pdo_mysql`.
    ///
    /// # Why these are per request and not per process
    ///
    /// Single-site mode has one database and one account, so `DB_*` is
    /// config-derived once ([`build_db_env_vars`]). Multi-tenant mode gives
    /// every virtual host its own database, and therefore its own MySQL
    /// account: `DB_USER` is the site key and `DB_PASSWORD` is that site's
    /// derived password. The listener resolves a connection's database from
    /// the credential it authenticates, so injecting the wrong site's password
    /// would not cross tenants — it would simply be refused.
    ///
    /// # Why this does not leak across tenants
    ///
    /// These land in `$_SERVER`, which ePHPm rebuilds per request from a
    /// thread-local table (`ephpm_request_clear` zeroes it at the top of every
    /// request). There is no process-global `setenv`, so site B's PHP never
    /// observes the credential injected for site A.
    ///
    /// Note `getenv()` does **not** see these — ePHPm installs no
    /// `sapi_module.getenv`, deliberately: a process-global environment is
    /// shared by every worker thread and would be exactly the cross-tenant leak
    /// this design avoids. Tenants must read `$_SERVER['DB_PASSWORD']`.
    fn build_per_site_db_env_vars(&self, site_key: &str) -> Vec<(String, String)> {
        let Some(wire) = &self.per_site_db_wire else {
            return Vec::new();
        };
        // Fail closed: no key means no credential. A site that cannot be named
        // gets no DB_* variables rather than some default account.
        if site_key.is_empty() {
            return Vec::new();
        }

        // Deterministic per site, so cache it exactly as the KV path does
        // rather than recomputing an HMAC on every request.
        let password = if let Some(cached) = self.per_site_db_password_cache.get(site_key) {
            cached.clone()
        } else {
            let derived = wire.auth.password_for(site_key);
            self.per_site_db_password_cache.insert(site_key.to_string(), derived.clone());
            derived
        };

        let (host, port) = wire.listen.rsplit_once(':').unwrap_or(("127.0.0.1", "3306"));
        // The database name is cosmetic here — the connection's database is
        // fixed by the credential, and litewire answers `USE <db>` without
        // switching anything. Set it to the site key so framework config and
        // logs read sensibly.
        vec![
            ("DB_CONNECTION".into(), "mysql".into()),
            ("DB_HOST".into(), host.into()),
            ("DB_PORT".into(), port.into()),
            ("DB_DATABASE".into(), site_key.into()),
            ("DB_NAME".into(), site_key.into()),
            ("DB_USER".into(), site_key.into()),
            ("DB_USERNAME".into(), site_key.into()),
            ("DB_PASSWORD".into(), password.clone()),
            (
                "DATABASE_URL".into(),
                format!("mysql://{site_key}:{password}@{host}:{port}/{site_key}"),
            ),
        ]
    }

    /// Build `EPHPM_REDIS_*` environment variables for PHP injection.
    ///
    /// Only produces variables when all conditions are met:
    /// - `kv.redis_compat.enabled` is true
    /// - `kv.secret` is set
    /// - Multi-tenant mode is active (a site hostname is available)
    fn build_kv_env_vars(&self, hostname: &str) -> Vec<(String, String)> {
        let is_multi_tenant = self.multi_tenant_kv.is_some();
        let Some(ref secret) = self.kv_secret else {
            return Vec::new();
        };
        if !self.kv_redis_compat_enabled || !is_multi_tenant || hostname.is_empty() {
            return Vec::new();
        }

        // Per-hostname HMAC-SHA256 password is deterministic — cache
        // it in a DashMap keyed by hostname so we compute the HMAC
        // exactly once per host per process instead of on every
        // request.
        //
        // The cap matters: for a *known* vhost this key is the canonical site
        // key (bounded by the site fleet), but an unknown host that falls
        // through to the default document root is keyed by its own name, and
        // that is client-controlled. Past the cap the HMAC is simply recomputed
        // per request — a few microseconds — rather than growing a map an
        // unauthenticated caller can drive (the memory-growth half of #291).
        let password = if let Some(cached) = self.kv_site_password_cache.get(hostname) {
            cached.clone()
        } else {
            let derived = ephpm_kv::auth::derive_site_password(secret, hostname);
            if self.kv_site_password_cache.len() < SITE_PASSWORD_CACHE_MAX {
                self.kv_site_password_cache.insert(hostname.to_string(), derived.clone());
            }
            derived
        };

        // Parse host:port from the listen address.
        let (host, port) = self.kv_listen.rsplit_once(':').unwrap_or(("127.0.0.1", "6379"));

        vec![
            ("EPHPM_REDIS_HOST".into(), host.into()),
            ("EPHPM_REDIS_PORT".into(), port.into()),
            ("EPHPM_REDIS_USERNAME".into(), hostname.into()),
            ("EPHPM_REDIS_PASSWORD".into(), password),
        ]
    }

    /// Resolve a request's `Host` to its virtual host: the canonical site key
    /// and everything that key selects.
    ///
    /// This is **the** host→tenant derivation. Every other per-tenant value is
    /// taken from the [`ResolvedSite`] it returns rather than re-derived from
    /// the `Host` header — see the [`ResolvedSite`] docs for why.
    fn resolve_site(&self, host: &str) -> ResolvedSite<'_> {
        if self.sites_dir.is_none() && self.sites.is_empty() {
            return self.default_site();
        }

        // Strip port and trailing dot, lowercase (shared normalization).
        let clean = normalize_host_key(host);

        // Defense-in-depth: never join a host that isn't a valid vhost key
        // onto `sites_dir`. `handle` already rejects malformed hosts with 404
        // before routing (see `reject_malformed_host`), but resolving to the
        // default document root here guarantees the traversal join cannot
        // happen even if this method is ever reached by another path.
        if !is_valid_site_key(&clean) {
            return self.default_site();
        }

        // Negative-lookup fast path: a host we've already determined
        // has no site directory does not need to re-syscall the
        // filesystem. Bot probes against `Host: <random>.example.com`
        // hit this constantly; every one used to trigger an `is_dir`
        // syscall and (worse) an `info!` line per unknown hostname.
        //
        // Entries TTL out (`UNKNOWN_SITE_TTL`) so lazy vhost
        // discovery — the feature that lets an operator drop a new
        // directory into `sites_dir` without restart — still works
        // within about a minute of the site coming online.
        if let Some(cached_at) = self.unknown_site_cache.get(&clean) {
            if cached_at.elapsed() < UNKNOWN_SITE_TTL {
                return self.default_site();
            }
            // Cache entry is stale — drop it and fall through to the
            // real lookup so a freshly-deployed site is found.
            drop(cached_at);
            self.unknown_site_cache.remove(&clean);
        }

        // If a domain suffix is configured (e.g. `.localhost`), peel it off
        // first so `blog.localhost` looks up the `blog/` directory. Falls
        // back to the literal name if the host doesn't end with the suffix.
        //
        // Security (issue #397): the stripped result is a fresh candidate vhost
        // key and MUST pass `is_valid_site_key` in its own right — the check at
        // the top of this function validated `clean`, NOT what remains after the
        // suffix is removed. A misconfigured (dotless) `sites_domain_suffix`
        // lets `Host: <suffix>` strip to the empty string, and `sites_dir.join("")`
        // is `sites_dir` itself — one vhost whose document root and `open_basedir`
        // become the entire fleet (cross-tenant read AND write). Dropping any
        // stripped candidate that fails the allowlist closes that: the empty (or
        // otherwise invalid) key is discarded and the request falls through to
        // the literal `clean` lookup, then to `default_site()` — never to a
        // `sites_dir`-rooted document root. `sites_domain_suffix` is additionally
        // rejected at config load when it lacks a leading dot, but this is the
        // load-bearing backstop and holds for any suffix shape.
        let stripped = self
            .sites_domain_suffix
            .as_deref()
            .and_then(|suffix| clean.strip_suffix(suffix))
            .filter(|s| is_valid_site_key(s))
            .map(str::to_owned);
        let lookup_keys: &[&str] = match stripped.as_deref() {
            Some(s) => &[s, clean.as_str()][..],
            None => &[clean.as_str()][..],
        };

        // Check the startup-scanned registry first for each candidate key.
        // Verify the directory still exists — it may have been removed (teardown).
        for key in lookup_keys {
            if let Some(site) = self.sites.get(*key)
                && site.container.is_dir()
            {
                return ResolvedSite {
                    key: Some((*key).to_string()),
                    // Through the same TTL-cached resolver the lazy path uses,
                    // so a newly written override is picked up without a restart
                    // on an already-scanned site too.
                    roots: self.site_roots(key, site.container.clone()),
                    index_files: &site.index_files,
                    websocket_files: &site.websocket_files,
                    fallback: &site.fallback,
                };
            }
        }

        // Lazy filesystem check: if sites_dir is set and the directory exists,
        // serve from it. No restart needed — new sites are discovered on demand.
        if let Some(ref sites_dir) = self.sites_dir {
            for key in lookup_keys {
                let candidate = sites_dir.join(key);
                if candidate.is_dir() {
                    // The override applies here too, and it must: a preview
                    // created while the server is running never goes through
                    // `scan_sites_dir`. Same resolver, same TTL as the registry
                    // path above.
                    let roots = self.site_roots(key, candidate);
                    // Discovery of a real vhost is worth an info line
                    // (once per host). Bot-probe misses go to `debug`
                    // via the fall-through below.
                    tracing::debug!(
                        host = %clean,
                        key = %key,
                        path = %roots.container.display(),
                        document_root = %roots.document_root.display(),
                        "discovered new virtual host (lazy)"
                    );
                    return ResolvedSite {
                        key: Some((*key).to_string()),
                        roots,
                        index_files: &self.index_files,
                        websocket_files: &self.websocket_files,
                        fallback: &self.fallback,
                    };
                }
            }
        }

        // Nothing matched. Remember this host so we don't re-syscall
        // for the next probe from the same bot. Cap at a
        // configuration-agnostic ceiling to avoid unbounded growth
        // under a very determined attacker; 10_000 covers realistic
        // long-tail probing without becoming a memory concern.
        if self.unknown_site_cache.len() < 10_000 {
            self.unknown_site_cache.insert(clean, std::time::Instant::now());
        }

        self.default_site()
    }

    /// The "no known site matched" outcome: the default document root, and —
    /// deliberately — **no** site key.
    ///
    /// A well-formed but unknown `Host` lands here (as does every request in
    /// single-site mode). It gets the default document root, exactly as before,
    /// but it names no tenant: nothing per-tenant may be minted from it. That
    /// is what stops an unauthenticated caller from creating an arbitrary
    /// `<key>.db` by varying the `Host` header (issue #291).
    fn default_site(&self) -> ResolvedSite<'_> {
        ResolvedSite {
            key: None,
            // The default document root is a web root, not a site container —
            // there is no directory above it that ePHPm owns — so the two roots
            // are the same path and the web-root convention does not apply. Its
            // `open_basedir` (when `sites_dir` is set and a host matched no
            // vhost) is unchanged from before this convention existed.
            roots: SiteRoots::flat(self.document_root.clone()),
            index_files: &self.index_files,
            websocket_files: &self.websocket_files,
            fallback: &self.fallback,
        }
    }

    /// The canonical site key for `host`, or `None` when it names no known
    /// virtual host.
    ///
    /// Thin wrapper over [`Router::resolve_site`] — the point is that there is
    /// exactly one derivation, so anything that needs "which tenant is this?"
    /// without needing the document root asks the same function that picked the
    /// document root.
    #[cfg(test)]
    fn canonical_site_key(&self, host: &str) -> Option<String> {
        self.resolve_site(host).key
    }

    /// The per-request tenant identities, all derived from the one canonical
    /// site key [`Router::resolve_site`] matched.
    ///
    /// Both PHP dispatch paths (fpm and worker) go through this, so they cannot
    /// disagree with each other and neither can re-derive a tenant from the
    /// `Host` header behind the other's back. See [`ResolvedSite`].
    fn site_identities(&self, site_key: Option<&str>, server_name: &str) -> SiteIdentities {
        SiteIdentities {
            // Fail closed twice over: no per-site mode, or no known site, means
            // no database identity at all (issues #290, #291).
            db: if self.per_site_db { site_key.map(str::to_owned) } else { None },
            kv: site_key.map_or_else(|| normalize_host_key(server_name), str::to_owned),
            opcache: opcache_vhost_key(site_key),
            // Unlike `kv`, this does NOT fall back to the request host. A
            // client-controlled scope would let anyone mint a fresh WebSocket
            // namespace by varying `Host`, and — worse — two requests that
            // *should* be one tenant could end up in different scopes and lose
            // sight of each other's connections. Multi-tenant: the canonical
            // key or nothing. Single-site: one sentinel scope for everything.
            ws: if self.multi_site {
                site_key.map(str::to_owned)
            } else {
                Some(ephpm_php::ws_bridge::SINGLE_SITE_SCOPE.to_string())
            },
        }
    }

    /// Resolve, and lazily create, this vhost's private temp + session
    /// directories, returning the paths to inject into the per-request PHP
    /// sandbox.
    ///
    /// Each tenant gets `<state_root>/tmp` and `<state_root>/sessions` where
    /// `state_root` is [`vhost_state_root`] of the resolved document root.
    /// The directories are created once per site (guarded by
    /// [`Router::ensured_vhost_dirs`]) and, on Unix, tightened to `0700` so
    /// they are not group/other-readable. Even without OS-level per-tenant
    /// uids, the security boundary is `open_basedir`: `state_root` is the only
    /// temp entry in this vhost's basedir, and no other vhost's basedir
    /// contains it, so cross-tenant temp/session access is denied by PHP.
    ///
    /// Creation failures are logged and swallowed — the paths are returned
    /// regardless so the caller still narrows `open_basedir` away from the
    /// shared system temp. A missing session/temp dir degrades that tenant's
    /// own temp/session writes; it never widens another tenant's access.
    fn ensure_vhost_private_dirs(&self, document_root: &Path) -> VhostPrivateDirs {
        let state_root = vhost_state_root(document_root);
        let temp = state_root.join("tmp");
        let sessions = state_root.join("sessions");

        if !self.ensured_vhost_dirs.contains(&state_root) {
            for dir in [&temp, &sessions] {
                if let Err(e) = create_private_dir(dir) {
                    tracing::warn!(
                        path = %dir.display(),
                        error = %e,
                        "failed to create per-vhost private directory; \
                         this tenant's temp/session writes may fail"
                    );
                }
            }
            self.ensured_vhost_dirs.insert(state_root.clone());
        }

        VhostPrivateDirs { state_root, temp, sessions }
    }

    /// Handle an incoming HTTP request.
    ///
    /// # Errors
    ///
    /// Returns `hyper::Error` if the response cannot be constructed.
    ///
    /// # Panics
    ///
    /// Panics if a static HTTP response builder fails (should never happen).
    pub async fn handle<B>(
        &self,
        req: Request<B>,
        remote_addr: SocketAddr,
        is_tls: bool,
    ) -> Result<Response<ServerBody>, hyper::Error>
    where
        B: RequestBody,
    {
        // HTTP/2 and HTTP/3 carry the host in the `:authority` pseudo-header,
        // NOT a `Host` header (hyper surfaces it as the request-URI authority).
        // Synthesize a `Host` header from that authority when the client sent
        // none, so every downstream host consumer sees one consistent value:
        // vhost/document-root resolution (`extract_server_name`), the
        // trusted-host gate, and — critically — the `HTTP_HOST` `$_SERVER`
        // variable handed to PHP. Without this, an HTTP/2 browser request
        // reached the right document root (via `extract_server_name`'s own
        // authority fallback) but PHP saw an empty `HTTP_HOST`, so apps like
        // WordPress computed `localhost` URLs and 404'd every page over HTTPS
        // while HTTP/1.1 worked.
        let mut req = req;
        ensure_host_header(&mut req);

        // Metrics label for the request method. Standard HTTP methods map to
        // a `&'static str` so the two metric sites below allocate nothing per
        // request (issue #136); non-standard methods collapse to `"OTHER"`,
        // which also caps `method` label cardinality (an attacker sending
        // random verbs can't explode the Prometheus series count).
        let method_label: &'static str = method_metric_label(req.method());

        // Per-IP rate limiting (uses effective IP after proxy resolution).
        if let Some(ref limiter) = self.limiter {
            let (effective_addr, _) = self.resolve_proxy_info(&req, remote_addr, is_tls);
            if !limiter.check_rate(effective_addr.ip()) {
                counter!("ephpm_rate_limited_total").increment(1);
                let mut resp =
                    error_response(StatusCode::TOO_MANY_REQUESTS, "429 Too Many Requests");
                // This early return bypasses the marker application below.
                self.apply_preview_marker(&mut resp);
                return Ok(resp);
            }
        }

        gauge!("ephpm_http_requests_in_flight").increment(1.0);
        let start = std::time::Instant::now();

        // Timeline capture (dev mode / [server.diagnostics] request_log):
        // method + path have to be cloned out before `req` is consumed below.
        // Internal endpoints (`/_ephpm/*`, the metrics path) are excluded so
        // a dashboard polling `/_ephpm/requests` doesn't fill the buffer with
        // its own polls. `None` when the timeline is disabled — the shared
        // (serve-mode) hot path allocates nothing here.
        let timeline_capture = self.request_log.as_ref().and_then(|_| {
            let path = req.uri().path();
            if path.starts_with("/_ephpm/") || path == self.metrics_path {
                None
            } else {
                Some((req.method().as_str().to_owned(), path.to_owned()))
            }
        });

        // Request span for OTLP export. DEBUG level under a dedicated target
        // (`crate::OTEL_TRACE_TARGET`) so the default info-level stack leaves
        // the callsite disabled — the span only materializes when a layer
        // opts in (the OTLP layer's Targets filter, or RUST_LOG=debug).
        // Timing-wise it brackets the same region as `start`/`elapsed`.
        let span = tracing::debug_span!(
            target: crate::OTEL_TRACE_TARGET,
            "http.request",
            http.request.method = %req.method(),
            url.path = req.uri().path(),
            http.response.status_code = tracing::field::Empty,
        );
        // W3C trace-context propagation: parent the request span to an
        // incoming `traceparent` header. Only compiled with the `otlp`
        // feature (the propagator lives in the opentelemetry crates) and
        // only paid when the span is enabled and the header is present.
        #[cfg(feature = "otlp")]
        if !span.is_disabled() {
            crate::otlp::set_span_parent_from_headers(&span, req.headers());
        }

        // A `request` timeout of 0 disables the per-request deadline
        // (`[server.timeouts] request = 0`). In that mode we run the inner
        // handler directly rather than paying to arm and disarm a tokio
        // timer on every request (issue #135) - the timer registration is
        // ~0.02ms of pure overhead when the deadline never fires.
        let inner = tracing::Instrument::instrument(
            self.handle_inner(req, remote_addr, is_tls),
            span.clone(),
        );
        let (result, handler) = if self.request_timeout.is_zero() {
            let result = inner.await;
            let handler = result.as_ref().map_or("error", |(_, h)| *h);
            (result.map(|(resp, _)| resp), handler)
        } else if let Ok(result) = tokio::time::timeout(self.request_timeout, inner).await {
            let handler = result.as_ref().map_or("error", |(_, h)| *h);
            (result.map(|(resp, _)| resp), handler)
        } else {
            counter!("ephpm_http_timeouts_total", "stage" => "request").increment(1);
            (Ok(error_response(StatusCode::GATEWAY_TIMEOUT, "504 Gateway Timeout")), "error")
        };

        let elapsed = start.elapsed().as_secs_f64();
        gauge!("ephpm_http_requests_in_flight").decrement(1.0);

        // Advertise HTTP/3 on every TLS response, including error and timeout
        // paths — a client that only ever sees 404s should still discover h3.
        let mut result = result;
        if let Ok(ref mut resp) = result {
            self.apply_alt_svc(resp, is_tls);
            self.apply_preview_marker(resp);

            span.record("http.response.status_code", resp.status().as_u16());

            // Timeline entry: reuse the values already measured — `elapsed`
            // (the histogram measurement below) plus the PHP-path timings the
            // dispatch paths stashed in the response extensions. Nothing is
            // re-measured here.
            if let (Some(log), Some((method, path))) =
                (self.request_log.as_deref(), timeline_capture)
            {
                let timings = resp.extensions_mut().remove::<crate::timeline::PhpTimings>();
                let response_bytes = hyper::body::Body::size_hint(resp.body()).exact();
                log.record(crate::timeline::TimelineEntry {
                    timestamp_ms: crate::timeline::unix_now_ms(),
                    method,
                    path,
                    status: resp.status().as_u16(),
                    total_ms: elapsed * 1000.0,
                    queue_wait_ms: timings
                        .and_then(|t| t.queue_wait)
                        .map(|d| d.as_secs_f64() * 1000.0),
                    php_ms: timings.and_then(|t| t.execute).map(|d| d.as_secs_f64() * 1000.0),
                    response_bytes,
                });
            }
        }

        if let Ok(ref resp) = result {
            // Map the status to a `&'static str` label. The `metrics` macros
            // require label values to be `'static` (they intern into
            // `Cow<'static, str>`); the previous code satisfied that by
            // allocating `status.as_u16().to_string()` per request. The
            // helper returns a static literal for the codes this server emits
            // and collapses anything else to "other", so the hot path
            // allocates nothing and status-label cardinality stays bounded
            // (issue #136).
            let status_label = status_metric_label(resp.status());
            counter!("ephpm_http_requests_total",
                "method" => method_label,
                "status" => status_label,
                "handler" => handler
            )
            .increment(1);
            histogram!("ephpm_http_request_duration_seconds",
                "method" => method_label,
                "handler" => handler
            )
            .record(elapsed);
        }

        result
    }

    /// Inner request handler (wrapped by timeout in `handle`).
    ///
    /// Returns the response paired with a handler label for metrics.
    #[allow(clippy::too_many_lines)]
    async fn handle_inner<B>(
        &self,
        req: Request<B>,
        remote_addr: SocketAddr,
        is_tls: bool,
    ) -> Result<(Response<ServerBody>, &'static str), hyper::Error>
    where
        B: RequestBody,
    {
        // Use the percent-decoded path for routing and static-file lookup.
        // hyper hands us the raw URI, so `/test%2Ehtml` would otherwise be
        // looked up as the literal name `test%2Ehtml`. percent_decode_path
        // also rejects encoded slashes so the decoding can't be used to
        // sneak past path-traversal or prefix-block checks.
        let uri_path = match percent_decode_path(req.uri().path()) {
            Some(path) => path,
            None => {
                return Ok((error_response(StatusCode::BAD_REQUEST, "400 Bad Request"), "error"));
            }
        };
        let query_string = req.uri().query().unwrap_or("").to_string();
        // hyper's `Method::as_str()` returns the canonical uppercase
        // form for standard methods (`GET`, `POST`, ...); no
        // `to_ascii_uppercase` alloc needed. For custom methods it
        // returns the client-supplied bytes verbatim (already
        // uppercase-normalised by hyper's parser). We use `method_ref`
        // for the fast-path equality comparisons and hand the
        // downstream PHP path a `String` only where the signature
        // still requires one.
        let method_ref = req.method().clone();
        let method = method_ref.as_str();

        // Internal ePHPm endpoints — served before the trusted-host check
        // (and every other security check) since they are not user-supplied
        // content. Kubernetes probes and Prometheus scrapes address pods by
        // raw IP, so a `Host`-gated probe would 421 and the pod would never
        // become ready.
        if method_ref == hyper::Method::GET {
            if let Some(ref handle) = self.metrics_handle
                && uri_path == self.metrics_path
            {
                return Ok((metrics::render(handle), "metrics"));
            }

            // Liveness probe — always 200 if the server is running.
            if uri_path == "/_ephpm/health" {
                return Ok((json_response(StatusCode::OK, r#"{"status":"ok"}"#), "health"));
            }

            // Readiness probe — checks PHP initialization and DB proxy.
            if uri_path == "/_ephpm/ready" {
                return Ok((self.readiness_check(), "health"));
            }

            // Primary probe — the load-balancer target for active-passive
            // routing to the writable SQLite node. 200 when this node accepts
            // writes (the elected clustered-SQLite primary, any
            // non-clustered/standalone node, or any node in per-site clustered
            // mode, where ownership is per tenant and writes are forwarded to
            // the owner), 503 when it is a single-database clustered replica
            // whose writes would silently diverge. Safe to health-check in any
            // topology, so it never 404s. A single relaxed atomic load — no
            // lock, no await.
            if uri_path == "/_ephpm/primary" {
                return Ok((self.primary_check(), "health"));
            }

            // Request timeline (dev mode / [server.diagnostics] request_log):
            // the ring buffer as JSON, newest first. Only mounted when the
            // timeline is enabled — when disabled, the path deliberately
            // falls through and behaves like any other unknown /_ephpm/ path.
            if uri_path == "/_ephpm/requests"
                && let Some(ref log) = self.request_log
            {
                return Ok((json_response_owned(StatusCode::OK, log.to_json()), "diagnostics"));
            }
        }

        // Validate Host header against trusted hosts list.
        if let Some(resp) = self.check_trusted_host(&req) {
            return Ok((resp, "error"));
        }

        // Reject a Host that cannot be a safe `sites_dir` vhost key (path
        // traversal, separators, NUL, non-DNS characters) before it is used to
        // resolve a document root. Independent of `trusted_hosts`. See #275.
        if let Some(resp) = self.reject_malformed_host(&req) {
            return Ok((resp, "error"));
        }

        // ACME HTTP-01 challenge responder — serves challenge tokens from the
        // KV store so any cluster node can respond to Let's Encrypt challenges.
        if let Some(token) = uri_path.strip_prefix("/.well-known/acme-challenge/") {
            if let Some(authorization) = crate::acme::get_acme_challenge(&self.store, token) {
                let resp = Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/plain")
                    .body(body::buffered(Full::new(Bytes::from(authorization))))
                    .expect("acme challenge response");
                return Ok((resp, "acme"));
            }
            return Ok((error_response(StatusCode::NOT_FOUND, ""), "acme"));
        }

        // Block hidden files (dot-prefixed path segments like .env, .git)
        if let Some(resp) = self.check_hidden_file(&uri_path) {
            return Ok((resp, "error"));
        }

        // Block explicitly forbidden paths (patterns pre-split at
        // Router construction — no per-request Vec<&str> allocation).
        if self.blocked_paths.iter().any(|p| p.matches(&uri_path)) {
            return Ok((error_response(StatusCode::FORBIDDEN, "403 Forbidden"), "error"));
        }

        // Resolve real client IP and HTTPS status from trusted proxy headers
        let (effective_addr, is_https) = self.resolve_proxy_info(&req, remote_addr, is_tls);

        // Built-in reverse proxy (`[[server.proxy]]`). Positioned deliberately:
        //
        // * AFTER the host-validation gates (trusted host, malformed host) and
        //   the hidden-file / blocked-path gates above — an operator's global
        //   blocks still apply to proxied paths, so a proxy rule can never be a
        //   way around them (documented precedence: gates outrank proxy).
        // * AFTER `resolve_proxy_info`, so the `X-Forwarded-*` headers carry the
        //   resolved client identity.
        // * BEFORE `resolve_site` and the native WebSocket block, so a matched
        //   rule short-circuits ALL local serving (static, PHP, native WS
        //   termination) — a proxied host/path belongs to the backend.
        //
        // With no rules configured this is one `Option::is_none()`.
        if let Some(ref proxy) = self.proxy {
            let proxy_host = extract_server_name(&req).trim_end_matches('.').to_ascii_lowercase();
            if let Some(idx) = proxy.match_index(&proxy_host, &uri_path) {
                let original_host = req
                    .headers()
                    .get(http::header::HOST)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let mut resp =
                    proxy.forward(idx, req, &original_host, effective_addr, is_https).await;
                // Configured response headers belong on error/normal responses
                // but not on a 101 — that hands the socket to the WS tunnel and
                // anything appended would be read as frame bytes.
                if resp.status() != StatusCode::SWITCHING_PROTOCOLS {
                    self.apply_response_headers(&mut resp);
                }
                return Ok((resp, "proxy"));
            }
        }

        let accepts_br = self.compression.enabled && accepts_encoding(&req, "br");
        let accepts_gzip = self.compression.enabled && accepts_encoding(&req, "gzip");

        // Resolve virtual host — determines the canonical site key, document
        // root, index files and fallback. This is the ONLY host→tenant
        // derivation; the key travels with the request from here (see
        // `ResolvedSite`).
        let host = extract_server_name(&req);
        let ResolvedSite {
            key: site_key,
            roots: site_roots,
            index_files: site_index,
            fallback: site_fallback,
            websocket_files: site_websocket_files,
        } = self.resolve_site(&host);
        // Everything below routes against the WEB root: index files, the
        // fallback chain, static-file containment and PHP-script containment.
        // The container travels separately inside `site_roots` and is what
        // `handle_php` turns into `open_basedir` — see `SiteRoots`.
        let site_root = site_roots.document_root.clone();

        // Snapshot the request headers for the middleware phases before `req`
        // is consumed downstream: the static-path request phase (issue #395,
        // security half) and the response phase (the choke point below) both
        // build a `RequestCtx` from this. Only taken when a chain exists.
        let mw_req_headers: Option<Vec<(String, String)>> = self
            .middleware_chain
            .as_ref()
            .map(|_| extract_headers(req.headers(), &self.ingest_strip_headers));

        // WebSocket upgrade. Positioned deliberately:
        //
        // * AFTER the security gates above (trusted host, malformed host,
        //   hidden files, blocked paths) — an upgrade is not a way around them.
        // * AFTER `resolve_site`, because the entrypoint is resolved against
        //   THIS vhost's document root and the connection's tenant identity is
        //   the canonical key that resolution produced.
        // * BEFORE `resolve_fallback`, because an upgrade request must never
        //   fall through to a static file, `index.php`, or the fallback chain.
        //   A vhost with no entrypoint gets a 404, full stop.
        //
        // With `[server.websocket]` disabled this is one `Option::is_none()`
        // and an upgrade request routes exactly as it did before the feature
        // existed.
        if let Some(ref runtime) = self.websocket
            && crate::websocket::is_upgrade_request(&req)
        {
            let mut resp = self
                .handle_websocket_upgrade(
                    req,
                    runtime,
                    effective_addr,
                    is_https,
                    &site_roots,
                    site_websocket_files,
                    site_key,
                )
                .await;
            // Configured response headers belong on the error paths (404 /
            // 400 / 503) but not on a 101 — a Switching Protocols response
            // hands the socket over, and anything appended to it is bytes
            // the client will read as WebSocket framing.
            if resp.status() != StatusCode::SWITCHING_PROTOCOLS {
                self.apply_response_headers(&mut resp);
            }
            return Ok((resp, "websocket"));
        }

        // Extract If-None-Match for ETag support before consuming the request.
        let if_none_match = if self.etag {
            req.headers().get("if-none-match").and_then(|v| v.to_str().ok()).map(String::from)
        } else {
            None
        };

        let (mut response, handler) = match self.resolve_fallback(
            &uri_path,
            &query_string,
            &site_root,
            site_index,
            site_fallback,
        ) {
            Resolved::File(fs_path) => {
                if is_php_file(&fs_path) {
                    if self.is_php_allowed(&uri_path)
                        && self.php_script_contained(&fs_path, &site_roots)
                    {
                        let is_cacheable = (method_ref == hyper::Method::GET
                            || method_ref == hyper::Method::HEAD)
                            && self.php_etag_cache_config.enabled;

                        // Pre-check: bypass PHP if client's ETag matches stored value.
                        if is_cacheable && let Some(client_tag) = &if_none_match {
                            let key = php_etag_cache_key(
                                &self.php_etag_cache_config.key_prefix,
                                method,
                                &uri_path,
                                &query_string,
                            );
                            if let Some(stored) = self.store.get(&key) {
                                let stored_etag = String::from_utf8_lossy(&stored);
                                if etag_matches_value(&stored_etag, client_tag) {
                                    return Ok((
                                        Response::builder()
                                            .status(StatusCode::NOT_MODIFIED)
                                            .header("etag", stored_etag.as_ref())
                                            .body(body::buffered(Full::new(Bytes::new())))
                                            .expect("304 builder"),
                                        "php",
                                    ));
                                }
                            }
                        }

                        // Execute PHP
                        let resp = self
                            .handle_php(
                                req,
                                effective_addr,
                                is_https,
                                fs_path,
                                accepts_gzip,
                                accepts_br,
                                site_roots.clone(),
                                site_key.clone(),
                                None,
                            )
                            .await;

                        // Post-store: cache any ETag PHP set in the response.
                        if is_cacheable
                            && let Some(etag_val) =
                                resp.headers().get("etag").and_then(|v| v.to_str().ok())
                        {
                            let key = php_etag_cache_key(
                                &self.php_etag_cache_config.key_prefix,
                                method,
                                &uri_path,
                                &query_string,
                            );
                            #[allow(clippy::cast_sign_loss)]
                            let ttl = if self.php_etag_cache_config.ttl_secs > 0 {
                                Some(Duration::from_secs(
                                    self.php_etag_cache_config.ttl_secs as u64,
                                ))
                            } else {
                                None
                            };
                            self.store.set(key, etag_val.as_bytes().to_vec(), ttl);
                        }

                        (resp, "php")
                    } else {
                        (error_response(StatusCode::FORBIDDEN, "403 Forbidden"), "error")
                    }
                } else if let Some(canonical_root) = self.contained_canonical_root(&site_roots) {
                    // #395 (security half): run the request phase (fail-closed)
                    // BEFORE the file is read, so an auth/gate module can deny a
                    // static asset — the sensitive bytes never leave disk. On
                    // the PHP path this happens inside `handle_php`; the static
                    // path had no request phase at all before this. A REWRITE's
                    // path/header overrides are ignored here (the file is
                    // already resolved and no PHP runs); only RESPOND and
                    // appended response headers apply.
                    match self.static_request_phase(
                        mw_req_headers.as_deref(),
                        method,
                        &uri_path,
                        &query_string,
                        effective_addr,
                        &host,
                        is_https,
                    ) {
                        StaticGate::Respond(resp) => (resp, "middleware"),
                        StaticGate::Continue(extra_headers) => {
                            let resp = static_files::serve_file(
                                &canonical_root,
                                &fs_path,
                                accepts_gzip,
                                accepts_br,
                                &self.cache_control,
                                self.compression,
                                self.etag,
                                if_none_match.as_deref(),
                                self.file_cache.as_deref(),
                            )
                            .await;
                            (apply_response_headers(resp, &extra_headers), "static")
                        }
                    }
                } else {
                    (error_response(StatusCode::NOT_FOUND, "404 Not Found"), "error")
                }
            }
            Resolved::Status(code) => {
                // Worker mode: the booted framework owns routing (Octane/
                // RoadRunner model), so every request that isn't a static asset
                // goes to the worker entrypoint rather than 404ing on a missing
                // file. The framework decides the real status (incl. its own
                // 404). fpm mode keeps the literal fallback status.
                if self.worker_pool.is_some() {
                    // SCRIPT_FILENAME is nominal in worker mode (the worker
                    // script is the entrypoint); use the conventional front
                    // controller path so $_SERVER looks familiar to frameworks.
                    let script = site_root.join("index.php");
                    (
                        self.handle_php(
                            req,
                            effective_addr,
                            is_https,
                            script,
                            accepts_gzip,
                            accepts_br,
                            site_roots.clone(),
                            site_key.clone(),
                            None,
                        )
                        .await,
                        "php",
                    )
                } else {
                    let status = StatusCode::from_u16(code).unwrap_or(StatusCode::NOT_FOUND);
                    (
                        error_response_owned(
                            status,
                            format!("{code} {}", status.canonical_reason().unwrap_or("Error")),
                        ),
                        "error",
                    )
                }
            }
        };

        // Apply custom response headers to all responses.
        self.apply_response_headers(&mut response);

        // Response phase: let response-capable middleware transform the
        // generated response (compression, ETag, header injection) in reverse
        // chain order. Runs on PHP, static, worker-buffered, and error
        // responses alike — this is what makes those transforms (and #395's
        // transform half) apply to static files, not just PHP. Buffered
        // responses only; a streamed body bypasses it (see the method).
        // Applied AFTER the server's configured headers so a module can see
        // and override them.
        if self.middleware_chain.as_ref().is_some_and(|c| c.has_response_phase()) {
            response = self
                .run_response_phase(
                    response,
                    mw_req_headers.as_deref(),
                    method,
                    &uri_path,
                    &query_string,
                    effective_addr,
                    &host,
                    is_https,
                )
                .await;
        }

        Ok((response, handler))
    }

    /// Resolve a request through the `fallback` chain.
    ///
    /// Each entry except the last is tested against the filesystem.
    /// The last entry is the fallback — either a rewrite target or `=NNN`
    /// status code.
    fn resolve_fallback(
        &self,
        uri_path: &str,
        query_string: &str,
        doc_root: &Path,
        index_files: &[String],
        fallback_chain: &[String],
    ) -> Resolved {
        if fallback_chain.is_empty() {
            return Resolved::Status(404);
        }

        let (probes, fallback) = fallback_chain.split_at(fallback_chain.len() - 1);

        for entry in probes {
            let expanded = expand_variables(entry, uri_path, query_string);
            if let Some(path) = self.probe_path(&expanded, doc_root, index_files) {
                return Resolved::File(path);
            }
        }

        let last = &fallback[0];
        if let Some(code) = last.strip_prefix('=') {
            let code = code.parse().unwrap_or(404);
            Resolved::Status(code)
        } else {
            let expanded = expand_variables(last, uri_path, query_string);
            let (rewrite_path, _) = split_path_query(&expanded);
            let fs_path = doc_root.join(rewrite_path.trim_start_matches('/'));
            if fs_path.exists() && fs_path.is_file() {
                Resolved::File(fs_path)
            } else {
                Resolved::Status(404)
            }
        }
    }

    /// Probe a single `fallback` entry against the filesystem.
    fn probe_path(
        &self,
        expanded: &str,
        doc_root: &Path,
        index_files: &[String],
    ) -> Option<PathBuf> {
        let (path_part, _) = split_path_query(expanded);

        if path_part.ends_with('/') {
            let dir = doc_root.join(path_part.trim_start_matches('/'));
            if dir.is_dir() {
                for index in index_files {
                    let candidate = dir.join(index);
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
            None
        } else {
            let fs_path = doc_root.join(path_part.trim_start_matches('/'));
            if fs_path.is_file() { Some(fs_path) } else { None }
        }
    }

    /// Handle a PHP request by executing it in a blocking task.
    ///
    /// `site_key` is the canonical site key [`Router::resolve_site`] matched for
    /// this request (`None` when the host names no known vhost). Every
    /// per-tenant value below is derived from it rather than from the `Host`
    /// header, so the database, the wire credential, the KV keyspace and the
    /// document root cannot name different tenants — see [`ResolvedSite`].
    /// Resolve this vhost's WebSocket entrypoint — `index_files` semantics for
    /// the upgrade path.
    ///
    /// Tries `[server] websocket_files` in order against the **resolved**
    /// document root and returns the first that exists. `None` means this vhost
    /// has not opted into WebSockets, which the caller turns into a 404.
    ///
    /// Each candidate is containment-checked even though the names are
    /// operator-supplied rather than client-supplied: an entry may legitimately
    /// contain a separator (`"public/ws.php"`), so `..` or an absolute path
    /// must not be able to escape the vhost. A name that resolves outside the
    /// document root is skipped with a warning rather than silently served.
    fn resolve_websocket_script(&self, roots: &SiteRoots, names: &[String]) -> Option<PathBuf> {
        let site_root = roots.document_root.as_path();
        for name in names {
            let candidate = site_root.join(name);
            if !candidate.is_file() {
                continue;
            }
            if !self.php_script_contained(&candidate, roots) {
                tracing::warn!(
                    entry = %name,
                    root = %site_root.display(),
                    "ignoring a [server] websocket_files entry that resolves outside the \
                     document root"
                );
                continue;
            }
            return Some(candidate);
        }
        None
    }

    /// Answer a WebSocket upgrade request.
    ///
    /// Order matters, and every step before the handshake is a refusal that
    /// costs no socket:
    ///
    /// 1. **Entrypoint** — no `websocket_files` entry on disk ⇒ `404`, never a
    ///    fallthrough.
    /// 2. **Handshake** — a missing key or a `Sec-WebSocket-Version` other than
    ///    13 ⇒ `400` advertising version 13.
    /// 3. **Transport** — no `OnUpgrade` extension (HTTP/2, HTTP/3) ⇒ `400`.
    /// 4. **Tenant** — no canonical site key on a multi-vhost node ⇒ `404`. A
    ///    connection with no tenant identity would have no registry scope, so
    ///    it is refused rather than created unreachable.
    /// 5. **Capacity** — registry caps ⇒ `503`.
    /// 6. **`connect`** — the entrypoint runs with the real request. Non-2xx is
    ///    returned to the client verbatim and the reserved connection is
    ///    released. This is the only point at which an upgrade can be refused
    ///    by application code, which is why authentication belongs here.
    /// 7. **`101`** — the socket is handed to a session task.
    #[allow(clippy::too_many_arguments)]
    async fn handle_websocket_upgrade<B>(
        &self,
        mut req: Request<B>,
        runtime: &Arc<crate::websocket::WsRuntime>,
        remote_addr: SocketAddr,
        is_https: bool,
        site_roots: &SiteRoots,
        websocket_files: &[String],
        site_key: Option<String>,
    ) -> Response<ServerBody>
    where
        B: RequestBody,
    {
        // The entrypoint is resolved against the WEB root, same as an HTTP
        // request's index files — a `websocket.php` above the web root is not
        // publicly routable and must not become one over an upgrade.
        let site_root = site_roots.document_root.as_path();
        let Some(script) = self.resolve_websocket_script(site_roots, websocket_files) else {
            tracing::debug!(
                root = %site_root.display(),
                "websocket upgrade refused: this vhost has no [server] websocket_files entrypoint"
            );
            return error_response(StatusCode::NOT_FOUND, "404 Not Found");
        };

        let Some(handshake_key) = crate::websocket::handshake_key(req.headers()) else {
            return crate::websocket::bad_handshake("handshake");
        };

        // Only the HTTP/1.1 path carries an upgrade handle. HTTP/2 and HTTP/3
        // requests never reach here (`is_upgrade_request` requires HTTP/1.1),
        // but the check is what makes that a fact rather than an assumption.
        let Some(upgrade) = crate::websocket::take_upgrade(&mut req) else {
            return crate::websocket::bad_handshake("transport");
        };

        // The router must be shared (`Router::share`) for a session to outlive
        // this request. Unreachable in a real server; a stack-allocated router
        // in a unit test would land here.
        let Some(router) = self.self_weak.upgrade() else {
            tracing::error!(
                "websocket upgrade refused: this router was not created with Router::share()"
            );
            return error_response(StatusCode::SERVICE_UNAVAILABLE, "503 Service Unavailable");
        };

        let server_name = extract_server_name(&req);
        // The registry scope comes from the SAME derivation as the database,
        // KV and OPcache identities — one canonical site key (issue #293),
        // pinned here and carried by the connection for its whole life.
        let Some(site_scope) = self.site_identities(site_key.as_deref(), &server_name).ws else {
            tracing::debug!(
                host = %server_name,
                "websocket upgrade refused: host matched no virtual host, so the connection \
                 would have no tenant identity"
            );
            return error_response(StatusCode::NOT_FOUND, "404 Not Found");
        };

        let registered = match runtime.registry.register(&site_scope) {
            Ok(registered) => registered,
            Err(e) => {
                tracing::warn!(site = %site_scope, %e, "websocket upgrade refused");
                return crate::websocket::registry_full(e);
            }
        };

        // Captured before `handle_php` consumes the request, and replayed on
        // every later event so `$_SERVER` stays stable for the connection.
        let uri = req.uri().clone();
        let headers = req.headers().clone();

        let session = crate::websocket::WsSession {
            router,
            site_key: site_key.clone(),
            connection_id: registered.id.clone(),
            script: script.clone(),
            roots: site_roots.clone(),
            remote_addr,
            is_https,
            uri,
            headers,
        };

        // `connect` runs BEFORE the handshake completes, on the real request —
        // so it sees the cookies, query string and headers that authentication
        // needs. Compression is off: this response is either a refusal (small)
        // or discarded.
        let connect = self
            .handle_php(
                req,
                remote_addr,
                is_https,
                script,
                false,
                false,
                site_roots.clone(),
                site_key,
                Some(crate::websocket::WsEvent {
                    kind: crate::websocket::WsEventKind::Connect,
                    connection_id: registered.id.clone(),
                    opcode: None,
                }),
            )
            .await;
        counter!("ephpm_ws_events_total", "event" => "connect").increment(1);

        if !connect.status().is_success() {
            // Application-level refusal (API-Gateway `$connect` semantics).
            // Release the reservation and hand the script's own response back
            // to the client — status, headers and body — so a 401 with a
            // WWW-Authenticate header works exactly as it does over HTTP.
            runtime.registry.unregister(&registered.id);
            counter!("ephpm_ws_connect_rejected_total").increment(1);
            tracing::debug!(
                status = connect.status().as_u16(),
                "websocket upgrade refused by the connect handler"
            );
            return connect;
        }

        tracing::debug!(conn = %registered.id, "websocket connection established");
        tokio::spawn(crate::websocket::run_session(
            session,
            upgrade,
            Arc::clone(runtime),
            registered.rx,
            registered.control,
            self.request_timeout,
        ));

        crate::websocket::switching_protocols(&handshake_key)
    }

    /// Dispatch one WebSocket lifecycle event through the ordinary PHP path.
    ///
    /// The entry point session tasks use. Compression is disabled (nothing
    /// reads the response body) and the event's `$_SERVER` context rides along
    /// in `ws_event`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn handle_php_event(
        &self,
        req: Request<http_body_util::Full<Bytes>>,
        remote_addr: SocketAddr,
        is_https: bool,
        script: PathBuf,
        roots: SiteRoots,
        site_key: Option<String>,
        event: crate::websocket::WsEvent,
    ) -> Response<ServerBody> {
        self.handle_php(
            req,
            remote_addr,
            is_https,
            script,
            false,
            false,
            roots,
            site_key,
            Some(event),
        )
        .await
    }

    ///
    /// `ws_event` marks this execution as a WebSocket lifecycle event rather
    /// than an HTTP request. It changes exactly two things and nothing else —
    /// the `$_SERVER` entries it contributes (`WS_EVENT`, `WS_CONNECTION_ID`,
    /// `WS_OPCODE`) and which limit bounds the body — so a WebSocket event and
    /// an HTTP request are the same execution in every way that affects
    /// isolation: same per-site database session, same KV keyspace, same
    /// `open_basedir` / temp / session sandbox, same OPcache vhost, same crash
    /// guard, same engine.
    #[allow(clippy::too_many_arguments)]
    async fn handle_php<B>(
        &self,
        req: Request<B>,
        remote_addr: SocketAddr,
        is_https: bool,
        script_filename: PathBuf,
        accepts_gzip: bool,
        accepts_br: bool,
        roots: SiteRoots,
        site_key: Option<String>,
        ws_event: Option<crate::websocket::WsEvent>,
    ) -> Response<ServerBody>
    where
        B: RequestBody,
    {
        // Split the two roots apart exactly once. `document_root` is what PHP
        // sees as `$_SERVER['DOCUMENT_ROOT']` (the web root — frameworks resolve
        // asset URLs against it); `site_container` is the isolation boundary. The
        // two are the same path unless the per-site web-root convention moved
        // this vhost's root — see `SiteRoots`.
        let SiteRoots { document_root, container: site_container } = roots;
        // Per-site PHP rate cap (`[server.limits] per_site_rate`, enabled by
        // default under `[server] preview`). Enforced HERE — the single point
        // every PHP dispatch converges on (fpm spawn_blocking, fpm pool, and
        // worker mode via `handle_php_worker`) — so it runs before any PHP
        // work but after the cheap non-PHP exits (static files, PHP-ETag-cache
        // 304s), which are deliberately not counted: PHP CPU is what a viral
        // preview eats, not sendfile.
        //
        // Keyed by the canonical site key `resolve_site` returned, NEVER
        // re-derived from the Host header (issues #290/#291). No key
        // (unmatched host, or single-site mode) = no per-site cap.
        if let (Some(limiter), Some(key)) = (self.limiter.as_deref(), site_key.as_deref())
            && !limiter.check_site_rate(key)
        {
            counter!("ephpm_site_rate_limited_total").increment(1);
            let retry_after = limiter.site_retry_after_secs();
            let mut resp = error_response(StatusCode::TOO_MANY_REQUESTS, "429 Too Many Requests");
            if let Ok(value) = hyper::header::HeaderValue::from_str(&retry_after.to_string()) {
                resp.headers_mut().insert(hyper::header::RETRY_AFTER, value);
            }
            return resp;
        }

        let method = req.method().to_string();
        // `method` is the client's raw verb and belongs in PHP's `$_SERVER`,
        // never in a Prometheus label — that is exactly what
        // `method_metric_label` exists to prevent (see `handle`): standard
        // verbs map to a `&'static str`, everything else collapses to
        // `"OTHER"`, so random verbs can't explode the series count or cost
        // a `String` allocation per request on the hot path.
        let method_label: &'static str = method_metric_label(req.method());
        let mut uri = request_uri_origin_form(&req);
        let mut path = req.uri().path().to_string();
        let query_string = req.uri().query().unwrap_or("").to_string();
        let protocol = format!("{:?}", req.version());
        let mut headers = extract_headers(req.headers(), &self.ingest_strip_headers);
        let content_type =
            req.headers().get("content-type").and_then(|v| v.to_str().ok()).map(String::from);
        let server_name = extract_server_name(&req);

        // Reject oversized request bodies before reading.
        //
        // Skipped for WebSocket events: their payload is a frame the codec
        // already bounded by `[server.websocket] max_message_size`, and it is
        // in an in-memory body we built ourselves. Applying
        // `[server.request] max_body_size` on top would silently cap WebSocket
        // messages at the HTTP body limit — two unrelated knobs, one of which
        // the operator did not think they were setting.
        if ws_event.is_none()
            && let Some(resp) = self.check_body_size(&req)
        {
            return resp;
        }

        let server_port = self.server_port;

        // Content-Length (if declared) drives the worker-mode buffer-vs-stream
        // decision below. Read here — before the body may be consumed for
        // middleware buffering.
        let content_length: Option<u64> = req
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok());

        // Optional request-body buffering for middleware (`[server.request]
        // middleware_body_limit`). When enabled AND a chain is mounted, buffer
        // the body up front so the chain's `request_body` accessor can inspect
        // it (webhook/HMAC, CSRF-with-body); the SAME bytes are then handed to
        // PHP, so the body still arrives intact. When disabled (the default)
        // the chain runs before the body is read — a RESPOND never pays for the
        // transfer — and `req` flows unbuffered to the dispatch below.
        //
        // `req` becomes an `Option` so it can be consumed conditionally: taken
        // for buffering, or left intact for the streaming/buffered dispatch.
        // WebSocket events (synthetic, codec-bounded bodies) skip this.
        let mut req = Some(req);
        let buffer_body =
            self.middleware_body_limit > 0 && self.middleware_chain.is_some() && ws_event.is_none();
        let mut prebuffered: Option<Vec<u8>> = if buffer_body {
            let taken = req.take().expect("request present before body buffering");
            match self.collect_body_capped(taken).await {
                Ok(bytes) => {
                    #[allow(clippy::cast_precision_loss)]
                    histogram!("ephpm_http_request_body_bytes", "method" => method_label)
                        .record(bytes.len() as f64);
                    Some(bytes)
                }
                Err(()) => {
                    counter!("ephpm_http_body_overflow_total").increment(1);
                    return error_response(StatusCode::PAYLOAD_TOO_LARGE, "413 Payload Too Large");
                }
            }
        } else {
            None
        };

        // Native middleware chain. v1: every module sees the original request;
        // accumulated REWRITE overrides are applied here, after the chain.
        // CONTINUE/REWRITE response headers are appended to whatever response
        // this request ultimately produces (PHP output or an error page). The
        // `request_body` accessor exposes up to `middleware_body_limit` bytes of
        // the buffered body (empty when buffering is off).
        let mut mw_response_headers: Vec<(String, String)> = Vec::new();
        if let Some(ref chain) = self.middleware_chain {
            let body_view: &[u8] = prebuffered.as_deref().map_or(&[], |b| {
                let cap = usize::try_from(self.middleware_body_limit).unwrap_or(usize::MAX);
                &b[..b.len().min(cap)]
            });
            let ctx = ephpm_middleware::host::RequestCtx::new(
                &method,
                &path,
                &query_string,
                &remote_addr.ip().to_string(),
                &server_name,
                &headers,
            )
            .with_scheme(is_https)
            .with_host(&normalize_host_key(&server_name))
            .with_body(body_view);
            match chain.evaluate(&ctx, &path) {
                crate::middleware::ChainVerdict::Respond { status, body, headers } => {
                    return middleware_response(status, body, &headers);
                }
                crate::middleware::ChainVerdict::Continue {
                    rewrite_path,
                    header_overrides,
                    response_headers,
                } => {
                    mw_response_headers = response_headers;
                    for (name, value) in header_overrides {
                        override_header(&mut headers, name, value);
                    }
                    if let Some(new_path) = rewrite_path {
                        // A path rewrite affects REQUEST_URI (and PATH) only —
                        // documented v1 behavior. Script resolution already
                        // happened in handle_inner, so the fpm path keeps
                        // executing the originally resolved script. Worker
                        // mode is fully rewritten: the booted framework routes
                        // on REQUEST_URI, which we rebuild here.
                        uri = if query_string.is_empty() {
                            new_path.clone()
                        } else {
                            format!("{new_path}?{query_string}")
                        };
                        path = new_path;
                    }
                }
            }
        }

        // Worker mode: dispatch to the persistent worker pool instead of the
        // spawn_blocking fpm path. The whole handler is already wrapped in the
        // outer request timeout, so a starved queue becomes a 504.
        //
        // Large / unknown-length bodies STREAM into the worker (Phase 3): the
        // hyper `Incoming` body is read frame-by-frame by a task feeding a
        // bounded channel the worker drains, so ePHPm never holds the whole
        // body in memory. Small bodies keep the cheaper buffered path. A body
        // already buffered for middleware skips streaming and is handed over
        // directly (its histogram was recorded when it was buffered).
        if let Some(pool) = self.worker_pool.clone() {
            let (worker_body, body_overflow) = if let Some(bytes) = prebuffered.take() {
                (ephpm_php::worker_bridge::WorkerBody::Buffered(bytes), None)
            } else {
                let should_stream = self.worker_stream_threshold > 0
                    && content_length.is_none_or(|len| len >= self.worker_stream_threshold);
                if should_stream {
                    let (body, overflow) = stream_request_body(
                        req.take().expect("request present for streaming"),
                        content_length,
                        self.max_body_size,
                    );
                    (body, Some(overflow))
                } else {
                    // The Content-Length pre-check already 413'd declared-large
                    // bodies; `Limited` catches chunked / lying clients on the
                    // buffered path (`Err` on exceeding the cap).
                    let bytes = if self.max_body_size > 0 {
                        let cap = usize::try_from(self.max_body_size).unwrap_or(usize::MAX);
                        let taken = req.take().expect("request present for buffered worker body");
                        match http_body_util::Limited::new(taken, cap).collect().await {
                            Ok(collected) => collected.to_bytes().to_vec(),
                            Err(_) => {
                                counter!("ephpm_http_body_overflow_total").increment(1);
                                return apply_response_headers(
                                    error_response(
                                        StatusCode::PAYLOAD_TOO_LARGE,
                                        "413 Payload Too Large",
                                    ),
                                    &mw_response_headers,
                                );
                            }
                        }
                    } else {
                        let taken = req.take().expect("request present for buffered worker body");
                        match taken.collect().await {
                            Ok(collected) => collected.to_bytes().to_vec(),
                            Err(_) => Vec::new(),
                        }
                    };
                    #[allow(clippy::cast_precision_loss)]
                    histogram!("ephpm_http_request_body_bytes", "method" => method_label)
                        .record(bytes.len() as f64);
                    (ephpm_php::worker_bridge::WorkerBody::Buffered(bytes), None)
                }
            };

            let resp = self
                .handle_php_worker(
                    &pool,
                    method,
                    uri,
                    path,
                    query_string,
                    &script_filename,
                    document_root,
                    headers,
                    worker_body,
                    content_type,
                    remote_addr,
                    server_name,
                    site_key.as_deref(),
                    server_port,
                    is_https,
                    protocol,
                    accepts_gzip,
                    accepts_br,
                )
                .await;

            // The streaming cap tripped mid-body: whatever the worker made of
            // the truncated body, the request as sent was over the limit — the
            // client gets a 413, exactly as the Content-Length pre-check would
            // have produced.
            if let Some(flag) = body_overflow
                && flag.load(std::sync::atomic::Ordering::Acquire)
            {
                return apply_response_headers(
                    error_response(StatusCode::PAYLOAD_TOO_LARGE, "413 Payload Too Large"),
                    &mw_response_headers,
                );
            }
            return apply_response_headers(resp, &mw_response_headers);
        }

        // fpm path buffers the whole body; cap chunked / lying clients the same
        // way the Content-Length pre-check caps declared bodies. A WebSocket
        // event's body was bounded by the codec, not by this knob (see the
        // `check_body_size` skip above), so it takes the uncapped branch. A body
        // already buffered for middleware is reused as-is (its histogram was
        // recorded when it was buffered).
        let body = if let Some(bytes) = prebuffered.take() {
            bytes
        } else {
            let bytes = if self.max_body_size > 0 && ws_event.is_none() {
                let cap = usize::try_from(self.max_body_size).unwrap_or(usize::MAX);
                let taken = req.take().expect("request present for fpm body");
                match http_body_util::Limited::new(taken, cap).collect().await {
                    Ok(collected) => collected.to_bytes().to_vec(),
                    Err(_) => {
                        counter!("ephpm_http_body_overflow_total").increment(1);
                        return apply_response_headers(
                            error_response(StatusCode::PAYLOAD_TOO_LARGE, "413 Payload Too Large"),
                            &mw_response_headers,
                        );
                    }
                }
            } else {
                let taken = req.take().expect("request present for fpm body");
                match taken.collect().await {
                    Ok(collected) => collected.to_bytes().to_vec(),
                    Err(_) => Vec::new(),
                }
            };
            #[allow(clippy::cast_precision_loss)]
            histogram!("ephpm_http_request_body_bytes", "method" => method_label)
                .record(bytes.len() as f64);
            bytes
        };

        let multi_tenant_kv = self.multi_tenant_kv.clone();
        let vhost_open_basedir = self.sites_dir.is_some() && self.open_basedir;
        // Per-request tenant identities, all from the one canonical site key
        // that also selected `document_root` — so `<dir>/<key>.db`, the injected
        // `pdo_mysql` credential and the KV keyspace all belong to the vhost
        // whose code is about to run (issue #290). `db` is `None` for a host
        // that matched no vhost, so `ephpm_db_*` reports "no per-site database
        // context" instead of minting `<that host>.db` (issue #291).
        let SiteIdentities {
            db: db_site_key,
            kv: kv_site_key,
            opcache: vhost_name,
            ws: ws_site_scope,
        } = self.site_identities(site_key.as_deref(), &server_name);

        // Per-vhost eBPF tag input: `Some` only when the feature is on AND this
        // request matched a vhost (site_key is `None` for an unmatched Host,
        // which gets no tag — exactly like it gets no per-site DB/KV identity).
        // Cloned before the closure alongside the other `*_site_key` captures so
        // the `move` closure owns it. Consumed inside `run_php`, where a guard
        // tags the executing thread and clears it on that same thread on return.
        let ebpf_tag: Option<(Arc<crate::tenant_ebpf::TenantEbpf>, String)> =
            match (self.tenant_ebpf.as_ref(), site_key.as_deref()) {
                (Some(e), Some(k)) => Some((Arc::clone(e), k.to_owned())),
                _ => None,
            };

        // In multi-tenant mode, give this vhost its OWN temp + session
        // directories (issue #276). Resolved and created here, in the async
        // context, off the resolved (traversal-safe) document root; the paths
        // are moved into the blocking closure and applied as per-request INI
        // (open_basedir temp component, sys_temp_dir, upload_tmp_dir,
        // session.save_path) so no two tenants share `/tmp`.
        //
        // Derived from the site **container**, not the web root: the state root
        // must name the tenant, and it must keep naming the same tenant when a
        // site gains or loses a `public/` directory — otherwise adding a web
        // root would silently orphan that site's existing sessions and uploads.
        let vhost_dirs = if vhost_open_basedir {
            Some(self.ensure_vhost_private_dirs(&site_container))
        } else {
            None
        };
        // disable_shell_exec is applied globally via the generated php.ini
        // (zend_disable_functions runs once at MINIT and removes the
        // functions from the function table; runtime ini changes don't
        // re-disable them). Wiring lives in `crates/ephpm/src/main.rs`.

        // Build EPHPM_REDIS_* env vars for multi-tenant RESP auth injection,
        // plus DB_* env vars for framework auto-discovery.
        let mut env_vars = self.build_kv_env_vars(&kv_site_key);
        env_vars.extend_from_slice(&self.db_env_vars);
        // Per-site DB credentials, when the multi-tenant wire listener is up.
        // Reuses the site key already derived for the bridge, so `pdo_mysql`
        // and `ephpm_db_*` are guaranteed to name the same tenant.
        if let Some(key) = db_site_key.as_deref() {
            env_vars.extend(self.build_per_site_db_env_vars(key));
        }
        if let Some(ref id) = self.node_id {
            env_vars.push(("EPHPM_NODE_ID".to_string(), id.clone()));
        }
        // WebSocket lifecycle context. Present only for a WebSocket event, so
        // an ordinary request can distinguish the two with
        // `isset($_SERVER['WS_EVENT'])`.
        if let Some(ref event) = ws_event {
            env_vars.extend(event.server_vars());
        }
        // The connection the implicit `ephpm_ws_send($payload)` form acts on.
        // `None` for an ordinary HTTP request — and it is set either way, so
        // an event's id can never survive onto the next request that lands on
        // this blocking thread.
        let ws_connection_id = ws_event.as_ref().map(|event| event.connection_id.clone());

        // Phase-1 OPcache clustered invalidation: fast-path check outside the
        // spawn_blocking hop so a no-op costs one atomic load + one KV get and
        // never touches the blocking pool. When Decision::Invalidate fires, we
        // pass the version + vhost name down so the blocking closure can call
        // the FFI invalidator inside the PHP request lifecycle (must run on a
        // TSRM-registered thread with an active request).
        let invalidate_version = match self.opcache_watcher.check(&self.store, &vhost_name) {
            crate::opcache::Decision::NoOp => None,
            crate::opcache::Decision::Invalidate { version } => Some(version),
        };
        let opcache_watcher = invalidate_version.map(|_| self.opcache_watcher.clone());

        // PHP middleware lane (`library = "php:<path>"`, EXPERIMENTAL).
        //
        // Resolved against THIS request's `document_root` — the one
        // `resolve_site` returned — so in multi-tenant mode the mount names the
        // tenant's own file, executes in the tenant's own PHP context, and has
        // exactly the reach the tenant's `index.php` already has. There is no
        // path here by which a mount could read or run another tenant's code.
        //
        // The glob is matched against the post-native-chain `path`: PHP mounts
        // are the later phase, so they see the request as the native modules
        // left it.
        // Walked ONCE: `php_mounts` allocates, and the metric labels ride along
        // in the same pass so the closure can name a mount without reaching
        // back into the router. The `has_php_mounts` guard keeps all of it off
        // the hot path for every server that mounts no PHP middleware — the
        // default, and for now the overwhelming majority.
        let (php_middleware, php_middleware_names): (
            Vec<ephpm_php::request::PhpMiddleware>,
            Vec<String>,
        ) = match self.middleware_chain.as_ref() {
            Some(chain) if chain.has_php_mounts() => chain
                .php_mounts(&path)
                .into_iter()
                .map(|mount| {
                    (
                        ephpm_php::request::PhpMiddleware {
                            script: document_root.join(&mount.script),
                            config_json: mount.config_json.clone(),
                        },
                        mount.name.clone(),
                    )
                })
                .unzip(),
            _ => (Vec::new(), Vec::new()),
        };

        // The per-request execution body — IDENTICAL for both fpm engines.
        // Both the default `spawn_blocking` path and the dedicated pool run this
        // exact closure, so per-request parity (per-site DB session, KV
        // keyspace, `open_basedir`/temp/session INI, OPcache invalidation, and
        // the `PhpRuntime::execute` bailout crash guard) is guaranteed by
        // construction — the same code runs; only the thread it lands on
        // differs.
        let run_php = move || -> Result<ephpm_php::response::PhpResponse, ephpm_php::PhpError> {
            // Tag this thread with its vhost id for the kernel bind/connect
            // hooks, BEFORE any tenant code runs. The guard clears tag[tid] when
            // this closure returns — on THIS blocking/pool thread, before it is
            // reused. The Drop is a pure bpf `delete_elem` syscall, not a PHP
            // call, so it does not cross PHP's setjmp/longjmp boundary
            // (`PhpRuntime::execute` catches its own bailout and returns a
            // `Result`, so the closure always unwinds normally). A stale tag on a
            // reused ZTS thread would be a cross-tenant leak — this guard is the
            // one hard invariant. `None` (feature off, or a host that matched no
            // vhost) writes nothing, so there is nothing to clear. The shed / 503
            // / hung-thread paths return before `run_php` runs at all, so they
            // never write a tag.
            let _ebpf_tag_guard = ebpf_tag.as_ref().map(|(e, key)| e.tag_current_thread(key));

            // Scope KV store to this virtual host for multi-tenant isolation.
            // Keyed on the same identity the injected `EPHPM_REDIS_USERNAME`
            // names, so the in-process bridge and a RESP client reach one
            // keyspace.
            ephpm_php::kv_bridge::set_site_store(
                multi_tenant_kv.as_ref().map(|mt| mt.get_site_store(&kv_site_key)),
            );

            // Scope the embedded database to this virtual host: the bridge
            // resolves the tenant's own database from this key (per-site DB
            // isolation). `None` leaves the bridge on its single global
            // backend (single-site) — and clears any stale key so per-site
            // mode never silently reuses a previous request's tenant.
            ephpm_php::db_bridge::set_current_site(db_site_key.as_deref());

            // Scope `ephpm_ws_*` to this virtual host. Derived from the SAME
            // canonical site key as the database and KV identities above, so a
            // script cannot reach a socket belonging to a tenant whose code it
            // is not running. `None` — a host that matched no vhost — clears
            // the scope, leaving this thread with no WebSocket capability at
            // all rather than the previous request's.
            ephpm_php::ws_bridge::set_current_site(ws_site_scope.as_deref());

            // And which connection the implicit `ephpm_ws_*` forms mean.
            // Cleared (`None`) for every non-WebSocket execution, so a
            // reused blocking thread cannot carry a previous event's
            // connection into an unrelated request.
            ephpm_php::ws_bridge::set_current_connection(ws_connection_id.as_deref());

            // Apply per-request PHP sandbox for multi-tenant isolation.
            // open_basedir varies per vhost (each site only sees its own
            // directory), so it has to be set per request. The C wrapper
            // uses STAGE_ACTIVATE to bypass OnUpdateBaseDir's
            // "must-be-tighter-than-current" check, since each site's path
            // is a peer rather than a subset of the previous one.
            //
            // Alongside open_basedir we point PHP's temp + session storage at
            // this vhost's OWN directories (issue #276). open_basedir is the
            // enforcement boundary: its only temp entry is this vhost's
            // state_root, so a tenant cannot read or write another tenant's
            // temp/session files even by absolute path. sys_temp_dir /
            // upload_tmp_dir / session.save_path then make PHP's own temp
            // writes (uploads, session files, tmpfile fallbacks) land inside
            // that permitted directory rather than tripping the basedir check.
            // session.save_path and upload_tmp_dir are re-read per request, so
            // each tenant's sessions/uploads are physically separated; the
            // files session handler keeps working, just per-site.
            //
            // THE invariant of the per-site web-root convention: the basedir
            // entry is the site **container**, never the web root. A Laravel
            // front controller's first statement is
            // `require __DIR__.'/../vendor/autoload.php'`; a sandbox narrowed to
            // `public/` fails it on request one. What the convention moves is the
            // HTTP surface (`document_root`, above) — not the sandbox.
            if let Some(dirs) = &vhost_dirs {
                let basedir = vhost_open_basedir_value(&site_container, &dirs.state_root);
                PhpRuntime::set_request_ini("open_basedir", &basedir);
                PhpRuntime::set_request_ini("sys_temp_dir", &dirs.temp.to_string_lossy());
                PhpRuntime::set_request_ini("upload_tmp_dir", &dirs.temp.to_string_lossy());
                PhpRuntime::set_request_ini("session.save_path", &dirs.sessions.to_string_lossy());
            }

            // OPcache clustered invalidation (Phase 1): if the watcher told us
            // to invalidate, drop bytecode under this vhost's docroot BEFORE
            // executing the script. Runs on the TSRM-registered blocking
            // thread inside the thread's still-active previous/initial request
            // (ephpm_thread_init leaves one open; execute() cycles it AFTER
            // this point) — OPcache SHM effects survive that cycle.
            // mark_invalidated deduplicates so concurrent requests coalesce on
            // the per-vhost mutex.
            if let (Some(watcher), Some(version)) = (opcache_watcher, invalidate_version) {
                // Scoped to the container, not the web root: a framework's
                // compiled bytecode overwhelmingly lives in `vendor/` and `app/`
                // *above* `public/`, so invalidating only under the web root
                // would leave the stale code cached and defeat the purpose.
                watcher.mark_invalidated(
                    &vhost_name,
                    &site_container,
                    version,
                    crate::opcache::InvalidationTrigger::Kv,
                    PhpRuntime::opcache_invalidate_under,
                );
            }

            // JIT buffer gauges: piggyback on the request path (same thread
            // state the invalidator above relies on — TSRM-registered, inside
            // the thread's still-active request). One relaxed atomic load per
            // request; the FFI status call runs at most once per sampling
            // interval process-wide. No-op in stub mode.
            ephpm_php::jit_metrics::maybe_sample();

            let had_php_middleware = !php_middleware.is_empty();
            let result = PhpRuntime::execute(PhpRequest {
                method,
                uri,
                path,
                query_string,
                script_filename,
                document_root,
                headers,
                body,
                content_type,
                remote_addr,
                server_name,
                server_port,
                is_https,
                protocol,
                env_vars,
                middleware: php_middleware,
            });

            // The chain outcome lives in a `__thread` int in the C wrapper, so
            // it has to be read here — on the blocking thread that just ran the
            // request — not back in the async handler.
            //
            // Mounts that ran and fell through are counted `continue`; the one
            // that ended the chain gets `respond` (it called `exit()`) or
            // `error` (it fataled); mounts that never ran are not counted at
            // all. There is deliberately no `rewrite` action for this lane —
            // PHP expresses a rewrite by assigning to `$_SERVER`, and detecting
            // that would mean diffing the superglobal on the hot path, so a
            // `rewrite` label here would be invented rather than observed.
            if had_php_middleware {
                let (outcome, ran) = PhpRuntime::middleware_outcome();
                for (i, name) in php_middleware_names.iter().take(ran).enumerate() {
                    let terminal = i + 1 == ran;
                    let action = if terminal {
                        outcome.label()
                    } else {
                        ephpm_php::request::MiddlewareOutcome::Continue.label()
                    };
                    counter!(
                        "ephpm_middleware_invocations_total",
                        "module" => name.clone(),
                        "action" => action
                    )
                    .increment(1);
                }
            }
            result
        };

        // `php.execute` span: brackets exactly the region `php_start` /
        // `php_elapsed` measure (execution plus, on the pool engine, the
        // dispatch-queue wait). Created inside the `http.request`-instrumented
        // future so it parents correctly; dropped right after the timer is read.
        let php_span = tracing::debug_span!(target: crate::OTEL_TRACE_TARGET, "php.execute");
        let php_start = std::time::Instant::now();

        // Engine selection. DEFAULT (`fpm_engine = "spawn_blocking"`): the
        // `else` arm is byte-identical to the historical path (workers
        // semaphore + spawn_blocking). POOL (`fpm_engine = "pool"`): dispatch
        // the same closure to ePHPm's dedicated thread pool, whose size is the
        // concurrency cap (the semaphore is bypassed). Backpressure → 504 via
        // the outer timeout; a closed pool → 503; a wedged thread → 504 + a
        // replacement, mirroring the worker-mode dispatch path.
        let (result, queue_wait): (
            Result<
                Result<ephpm_php::response::PhpResponse, ephpm_php::PhpError>,
                tokio::task::JoinError,
            >,
            Option<Duration>,
        ) = if let Some(pool) = self.fpm_pool.clone() {
            // `wait` (default) queues behind a full backlog; `shed` refuses to,
            // after an optional `shed_after` grace, so overload becomes a fast
            // 503 instead of a client timeout (issue #301). Both share every
            // downstream arm — only admission differs.
            let recv = match self.overload_policy {
                ephpm_config::OverloadPolicy::Wait => {
                    pool.dispatch(Box::new(run_php)).await.map_err(ShedReason::from)
                }
                ephpm_config::OverloadPolicy::Shed => pool
                    .try_dispatch(Box::new(run_php), self.shed_after)
                    .await
                    .map_err(ShedReason::from),
            };
            let queue_wait = php_start.elapsed();
            let rx = match recv {
                Ok(rx) => rx,
                // Backlog full (shed) or pool draining / all threads gone.
                // Both answer 503; only the shed arm advertises Retry-After and
                // counts as shedding.
                Err(reason) => {
                    drop(php_span);
                    let mut resp =
                        apply_response_headers(shed_response(reason, "pool"), &mw_response_headers);
                    resp.extensions_mut().insert(crate::timeline::PhpTimings {
                        queue_wait: Some(queue_wait),
                        execute: None,
                    });
                    return resp;
                }
            };
            // Bound the wait so a wedged thread becomes a 504 AND signals the
            // pool to replace it. A `request` timeout of 0 disables the deadline
            // (issue #135). `rx` awaited bare yields `Result<_, RecvError>`;
            // wrap it as `Ok(_)` to match the `timeout` arm's shape.
            let awaited = if self.request_timeout.is_zero() {
                Ok(rx.await)
            } else {
                tokio::time::timeout(self.request_timeout, rx).await
            };
            match awaited {
                Ok(Ok(exec_output)) => (Ok(exec_output), Some(queue_wait)),
                // Thread dropped its sender (retired without replying) — 500,
                // via the same `build_php_response` error arm as a bailout.
                Ok(Err(_)) => (
                    Ok(Err(ephpm_php::PhpError::ExecutionFailed(
                        "fpm pool thread dropped response".into(),
                    ))),
                    Some(queue_wait),
                ),
                // No reply within the request timeout — replace the thread, 504.
                Err(_) => {
                    pool.note_hung();
                    counter!("ephpm_http_timeouts_total", "stage" => "fpm_pool").increment(1);
                    drop(php_span);
                    let mut resp = apply_response_headers(
                        error_response(StatusCode::GATEWAY_TIMEOUT, "504 Gateway Timeout"),
                        &mw_response_headers,
                    );
                    resp.extensions_mut().insert(crate::timeline::PhpTimings {
                        queue_wait: Some(queue_wait),
                        execute: None,
                    });
                    return resp;
                }
            }
        } else {
            // Cap concurrent PHP executions when [php].workers is set. The
            // permit is held for the whole execution (php-fpm max_children
            // semantics): requests past the cap queue here until a worker frees
            // up, still subject to the outer request timeout. Acquire never
            // fails — the semaphore is never closed.
            //
            // Under `overload_policy = "shed"` the acquire is bounded by
            // `shed_after` and a request that does not get a permit in time is
            // answered 503 rather than queued (issue #301). This is the ONLY
            // shed point available on this engine: once the closure reaches
            // `spawn_blocking` it is committed to tokio's unbounded,
            // uncancellable blocking queue, where ePHPm can neither reject nor
            // withdraw it. Cancelling the *acquire* is safe — tokio's
            // `acquire_owned` future removes itself from the wait list when
            // dropped and no permit is leaked — and no PHP has run yet.
            //
            // With `workers = 0` (the default) there is no semaphore, so there
            // is nothing to bound and nothing is shed. `serve()` WARNs about
            // that combination at startup so the inertness is never silent.
            let _php_permit = match &self.php_semaphore {
                Some(sem) => {
                    let acquire = Arc::clone(sem).acquire_owned();
                    let permit = match self.overload_policy {
                        ephpm_config::OverloadPolicy::Wait => {
                            Some(acquire.await.expect("PHP semaphore never closed"))
                        }
                        ephpm_config::OverloadPolicy::Shed => {
                            match tokio::time::timeout(self.shed_after, acquire).await {
                                Ok(permit) => Some(permit.expect("PHP semaphore never closed")),
                                Err(_elapsed) => None,
                            }
                        }
                    };
                    let Some(permit) = permit else {
                        drop(php_span);
                        let mut resp = apply_response_headers(
                            shed_response(ShedReason::Overloaded, "spawn_blocking"),
                            &mw_response_headers,
                        );
                        resp.extensions_mut().insert(crate::timeline::PhpTimings {
                            queue_wait: Some(php_start.elapsed()),
                            execute: None,
                        });
                        return resp;
                    };
                    Some(permit)
                }
                None => None,
            };
            (tokio::task::spawn_blocking(run_php).await, None)
        };

        let php_elapsed = php_start.elapsed();
        drop(php_span);

        histogram!("ephpm_php_execution_duration_seconds").record(php_elapsed.as_secs_f64());
        let exec_status = match &result {
            Ok(Ok(_)) => "ok",
            Ok(Err(_)) | Err(_) => "error",
        };
        counter!("ephpm_php_executions_total", "status" => exec_status).increment(1);

        let mut response = apply_response_headers(
            build_php_response(result, accepts_gzip, accepts_br, self.compression),
            &mw_response_headers,
        );
        // Hand the measurement up to `handle` for the request timeline. On the
        // default `spawn_blocking` engine there is no dispatch queue, so
        // `queue_wait` is `None` (absent, not zero); on the pool engine it holds
        // the dispatch wait. (The one `spawn_blocking` request that does report
        // a wait is a shed one — it never reaches here, and its admission wait
        // on the `workers` semaphore is exactly the "how long before we gave up"
        // number worth showing in the timeline.)
        response
            .extensions_mut()
            .insert(crate::timeline::PhpTimings { queue_wait, execute: Some(php_elapsed) });
        response
    }

    /// Dispatch a PHP request to the persistent worker pool (worker mode).
    ///
    /// Builds an owned request from the same `$_SERVER`/cookie derivation the
    /// fpm path uses, hands it to the pool, and awaits the `oneshot`. The outer
    /// request timeout (in `handle`) turns a starved queue into a 504; a
    /// dropped sender (worker bailout with no stashed sender) becomes a 500.
    /// The response reuses `build_php_response` unchanged.
    #[allow(clippy::too_many_arguments)]
    async fn handle_php_worker(
        &self,
        pool: &Arc<crate::worker_pool::WorkerPool>,
        method: String,
        uri: String,
        path: String,
        query_string: String,
        script_filename: &Path,
        document_root: PathBuf,
        headers: Vec<(String, String)>,
        body: ephpm_php::worker_bridge::WorkerBody,
        content_type: Option<String>,
        remote_addr: SocketAddr,
        server_name: String,
        site_key: Option<&str>,
        server_port: u16,
        is_https: bool,
        protocol: String,
        accepts_gzip: bool,
        accepts_br: bool,
    ) -> Response<ServerBody> {
        // Build $_SERVER and the cookie string with the *same* derivation the
        // fpm path uses (via the shared free functions in ephpm_php::request),
        // so worker mode and fpm mode present PHP with byte-identical request
        // metadata. We build directly from the owned/borrowed locals here
        // rather than constructing a throwaway `PhpRequest` — that intermediate
        // built $_SERVER a second time and cloned method/uri/query/headers/
        // content_type on the hot path for no reason.
        // Same identity derivation as the fpm path — one function, so the two
        // dispatch modes cannot name different tenants for one request.
        let identities = self.site_identities(site_key, &server_name);
        let mut env_vars = self.build_kv_env_vars(&identities.kv);
        env_vars.extend_from_slice(&self.db_env_vars);
        // Per-site DB credentials. Worth noting these work here even though the
        // `ephpm_db_*` bridge does not: the bridge needs a thread-local site key
        // that only the fpm path sets, whereas a wire connection carries its
        // tenant in its own credential and needs nothing from the request
        // thread.
        if let Some(key) = identities.db.as_deref() {
            env_vars.extend(self.build_per_site_db_env_vars(key));
        }
        if let Some(ref id) = self.node_id {
            env_vars.push(("EPHPM_NODE_ID".to_string(), id.clone()));
        }

        let cookie_data = ephpm_php::request::cookie_string_from_headers(&headers);
        let server_vars = ephpm_php::request::build_server_variables(
            &method,
            &uri,
            &query_string,
            script_filename,
            &document_root,
            &path,
            &server_name,
            server_port,
            &protocol,
            remote_addr,
            is_https,
            &headers,
            &env_vars,
        );

        // `method`, `uri`, `query_string`, and `headers` are owned locals that
        // are no longer read below — move them into the owned request instead
        // of cloning.
        let owned = ephpm_php::worker_bridge::WorkerRequestOwned {
            method,
            uri,
            query_string,
            cookie_data,
            content_type,
            body,
            server_vars,
            headers,
        };

        // `worker.queue_wait` and `php.execute` spans, siblings under
        // `http.request`. Both open at dispatch time — the same instant the
        // existing timers start. queue_wait closes when the pool hands back
        // its oneshot receiver; php.execute closes when the response arrives
        // (or on the early error returns), mirroring the two histograms —
        // note the worker-mode execution timer deliberately includes the
        // queue wait, exactly like `ephpm_php_execution_duration_seconds`.
        let queue_span =
            tracing::debug_span!(target: crate::OTEL_TRACE_TARGET, "worker.queue_wait");
        let php_span = tracing::debug_span!(target: crate::OTEL_TRACE_TARGET, "php.execute");
        let php_start = std::time::Instant::now();
        let queue_wait_start = php_start;
        let recv = pool.dispatch(owned).await;
        let queue_wait = queue_wait_start.elapsed();
        drop(queue_span);
        #[allow(clippy::cast_precision_loss)]
        histogram!("ephpm_worker_request_wait_seconds").record(queue_wait.as_secs_f64());

        // Timings handed up to `handle` for the request timeline. The queue
        // wait is known from here on; `execute` is filled in only once the
        // worker actually delivers a response.
        let queued_only =
            crate::timeline::PhpTimings { queue_wait: Some(queue_wait), execute: None };

        // Dispatch channel closed (pool draining / all workers gone) — 503.
        let Ok(rx) = recv else {
            return with_php_timings(
                error_response(StatusCode::SERVICE_UNAVAILABLE, "503 Service Unavailable"),
                queued_only,
            );
        };

        gauge!("ephpm_worker_busy").increment(1.0);
        // Bound the wait so a wedged worker becomes a 504 AND signals the pool
        // to replace it (design §5.4). This inner timeout fires at or before
        // the outer request timeout.
        //
        // NOTE: for a streaming response this awaits only the HEADERS (the
        // `send_response_stream` -> response_begin delivers status+headers
        // immediately, before the body is produced), so a long streamed
        // download is NOT cut off by this timeout — the body flows afterward.
        //
        // A `request` timeout of 0 disables the deadline (issue #135): await
        // the receiver directly so no inner timer is armed. `rx` awaited bare
        // yields `Result<_, RecvError>`; wrap it as `Ok(_)` to match the
        // `timeout` arm's `Result<Result<_, _>, Elapsed>` shape.
        let awaited = if self.request_timeout.is_zero() {
            Ok(rx.await)
        } else {
            tokio::time::timeout(self.request_timeout, rx).await
        };
        gauge!("ephpm_worker_busy").decrement(1.0);

        let worker_resp = match awaited {
            Ok(Ok(resp)) => resp,
            // Sender dropped (worker unwound with no stashed sender) — 500.
            Ok(Err(_)) => {
                counter!("ephpm_php_executions_total", "status" => "error").increment(1);
                return with_php_timings(
                    build_php_response(
                        Ok(Err(ephpm_php::PhpError::ExecutionFailed(
                            "worker dropped response (bailout)".into(),
                        ))),
                        accepts_gzip,
                        accepts_br,
                        self.compression,
                    ),
                    queued_only,
                );
            }
            // Worker never responded in time — replace it, return 504.
            Err(_) => {
                pool.note_hung();
                counter!("ephpm_http_timeouts_total", "stage" => "worker").increment(1);
                return with_php_timings(
                    error_response(StatusCode::GATEWAY_TIMEOUT, "504 Gateway Timeout"),
                    queued_only,
                );
            }
        };

        let php_elapsed = php_start.elapsed();
        drop(php_span);
        histogram!("ephpm_php_execution_duration_seconds").record(php_elapsed.as_secs_f64());
        counter!("ephpm_php_executions_total", "status" => "ok").increment(1);

        let timings = crate::timeline::PhpTimings {
            queue_wait: Some(queue_wait),
            execute: Some(php_elapsed),
        };
        match worker_resp {
            ephpm_php::worker_bridge::WorkerResponse::Buffered { status, headers, body } => {
                with_php_timings(
                    build_php_response(
                        Ok(Ok(ephpm_php::response::PhpResponse { status, headers, body })),
                        accepts_gzip,
                        accepts_br,
                        self.compression,
                    ),
                    timings,
                )
            }
            // Streamed response (Phase 3): flush chunks to the client as PHP
            // produces them. No content-length (unknown up front) — chunked
            // transfer. Optionally wrapped in a flush-per-chunk brotli
            // encoder when `[server.response] compression_streaming` says so.
            ephpm_php::worker_bridge::WorkerResponse::Streaming {
                status,
                headers,
                body_rx,
                aborted,
            } => with_php_timings(
                build_streamed_worker_response(
                    status,
                    headers,
                    body_rx,
                    aborted,
                    accepts_br,
                    self.compression,
                ),
                timings,
            ),
        }
    }

    /// Collect the full request body into a `Vec<u8>`, enforcing
    /// `max_body_size` (the same cap the fpm/worker buffered paths apply):
    /// `Err(())` when the cap is exceeded (the caller turns it into a 413),
    /// `Ok` otherwise. Used to buffer the body up front for middleware
    /// (`[server.request] middleware_body_limit`); the same bytes are then
    /// handed to PHP, so the body still arrives intact.
    async fn collect_body_capped<B>(&self, req: Request<B>) -> Result<Vec<u8>, ()>
    where
        B: RequestBody,
    {
        if self.max_body_size > 0 {
            let cap = usize::try_from(self.max_body_size).unwrap_or(usize::MAX);
            http_body_util::Limited::new(req, cap)
                .collect()
                .await
                .map(|c| c.to_bytes().to_vec())
                .map_err(|_| ())
        } else {
            req.collect().await.map(|c| c.to_bytes().to_vec()).map_err(|_| ())
        }
    }

    /// Return 413 if Content-Length exceeds the limit.
    fn check_body_size<B>(&self, req: &Request<B>) -> Option<Response<ServerBody>> {
        if self.max_body_size == 0 {
            return None;
        }
        let len: u64 = req
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if len > self.max_body_size {
            Some(error_response(StatusCode::PAYLOAD_TOO_LARGE, "413 Payload Too Large"))
        } else {
            None
        }
    }

    /// Block requests for hidden files (dot-prefixed path segments).
    fn check_hidden_file(&self, uri_path: &str) -> Option<Response<ServerBody>> {
        if self.hidden_files == "allow" {
            return None;
        }
        if has_hidden_segment(uri_path) {
            let status = if self.hidden_files == "ignore" {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::FORBIDDEN
            };
            // Two-outcome dispatch: hidden-file blocking is either
            // "deny" (403) or "ignore" (404). Handing back a canned
            // `&'static` body keeps [`error_response`] on the
            // no-alloc fast path.
            let body: &'static str =
                if status == StatusCode::NOT_FOUND { "404 Not Found" } else { "403 Forbidden" };
            Some(error_response(status, body))
        } else {
            None
        }
    }

    /// Check if a PHP path is allowed to execute.
    ///
    /// When `allowed_php_paths` is empty, all PHP files are allowed.
    /// Otherwise the URI path must match at least one pattern.
    fn is_php_allowed(&self, uri_path: &str) -> bool {
        if self.allowed_php_paths.is_empty() {
            return true;
        }
        // Patterns are pre-split at Router construction — the per-
        // request check does not split into `Vec<&str>`.
        self.allowed_php_paths.iter().any(|p| p.matches(uri_path))
    }

    /// Resolve real client address and HTTPS status from proxy headers.
    ///
    /// When the request comes from a trusted proxy, reads `X-Forwarded-For`
    /// (rightmost untrusted IP) and `X-Forwarded-Proto` for HTTPS detection.
    fn resolve_proxy_info<B>(
        &self,
        req: &Request<B>,
        remote_addr: SocketAddr,
        is_tls: bool,
    ) -> (SocketAddr, bool) {
        if self.trusted_proxies.is_empty() || !self.is_trusted_proxy(remote_addr.ip()) {
            return (remote_addr, is_tls);
        }

        let real_ip = req
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|xff| self.resolve_xff(xff))
            .unwrap_or(remote_addr.ip());

        let is_https = req
            .headers()
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .map_or(is_tls, |proto| proto.eq_ignore_ascii_case("https"));

        (SocketAddr::new(real_ip, remote_addr.port()), is_https)
    }

    /// Validate the `Host` header against the trusted hosts list.
    ///
    /// Returns a 421 Misdirected Request if the host is not trusted.
    fn check_trusted_host<B>(&self, req: &Request<B>) -> Option<Response<ServerBody>> {
        if self.trusted_hosts.is_empty() {
            return None;
        }
        let host = req.headers().get("host").and_then(|v| v.to_str().ok()).unwrap_or("");
        // Lowercase the incoming host ONCE; the trusted list is already
        // lowercased at construction, so each entry compare is a plain byte
        // equality rather than a per-entry `eq_ignore_ascii_case`.
        let host_lc = host.to_ascii_lowercase();
        let host_no_port = host_lc.split(':').next().unwrap_or(&host_lc);
        let is_trusted = self
            .trusted_hosts
            .iter()
            .any(|trusted| host_lc == **trusted || host_no_port == &**trusted);
        if is_trusted {
            None
        } else {
            tracing::debug!(host, "rejected untrusted host");
            Some(error_response(StatusCode::MISDIRECTED_REQUEST, "421 Misdirected Request"))
        }
    }

    /// Reject a `Host` header that cannot be a valid virtual-host directory
    /// name **before** it is ever joined onto `sites_dir`.
    ///
    /// This closes the unauthenticated Host-header path traversal (issue
    /// #275). It runs independently of `trusted_hosts` (empty by default), so a
    /// multi-site deployment is protected out of the box: `Host:
    /// ../../../../../etc`, `Host: ../single`, encoded/backslash variants, and
    /// any other non-DNS host resolve to a 404 rather than escaping the sites
    /// directory. Well-formed but unknown hosts (`random.example.com`) are
    /// *not* rejected here — they fall through to the normal registry/lazy
    /// lookup and its `document_root` fallback, preserving existing behavior.
    ///
    /// Single-site mode (`sites_dir` unset and no scanned sites) never joins
    /// the host onto the filesystem, so the header is left untouched.
    fn reject_malformed_host<B>(&self, req: &Request<B>) -> Option<Response<ServerBody>> {
        if self.sites_dir.is_none() && self.sites.is_empty() {
            return None;
        }
        // Use the exact value `resolve_site` will normalize, so the gate and
        // the join can never disagree about what the key is.
        let host = extract_server_name(req);
        let key = normalize_host_key(&host);
        if is_valid_site_key(&key) {
            None
        } else {
            tracing::warn!(host = %host, "rejected malformed Host header — invalid vhost key");
            Some(error_response(StatusCode::NOT_FOUND, "404 Not Found"))
        }
    }

    /// Check server readiness for the `/ready` probe.
    ///
    /// Returns 200 if PHP is initialized. Returns 503 with a reason
    /// string otherwise.
    fn readiness_check(&self) -> Response<ServerBody> {
        if !PhpRuntime::is_ready() {
            return json_response(
                StatusCode::SERVICE_UNAVAILABLE,
                r#"{"status":"not_ready","reason":"PHP runtime not initialized"}"#,
            );
        }
        // Worker mode: not ready until at least one worker has booted its
        // framework and reached take_request() — prevents load balancers from
        // routing before the framework is up (design §4.5).
        if let Some(pool) = &self.worker_pool
            && pool.ready_count() == 0
        {
            return json_response(
                StatusCode::SERVICE_UNAVAILABLE,
                r#"{"status":"not_ready","reason":"no worker has finished booting"}"#,
            );
        }
        // A configured SQL proxy that has never reached its upstream cannot
        // serve a single query — the process must stay out of rotation and a
        // rollout containing it must stall. Deliberately a *first-connect*
        // gate, not a live database probe: see `crate::db_health` for why a
        // post-startup outage must not evict every replica at once.
        if let Some(resp) = Self::db_not_ready(self.db_health.as_ref()) {
            return resp;
        }
        json_response(StatusCode::OK, r#"{"status":"ready"}"#)
    }

    /// Primary probe served at `/_ephpm/primary` — the active-passive
    /// load-balancer target for the writable clustered-SQLite node.
    ///
    /// `200 {"primary":true}` when this node accepts writes: the elected
    /// clustered-SQLite primary, or any non-clustered/standalone node (whose
    /// `primary_view` stays the constant `true` from `new()`). `503
    /// {"primary":false}` when this node is a clustered-SQLite replica —
    /// steering a write here would silently diverge and be lost, so the LB
    /// must route it away until it wins an election.
    ///
    /// One relaxed atomic load: no lock and no await on the request path.
    fn primary_check(&self) -> Response<ServerBody> {
        if self.primary_view.load(std::sync::atomic::Ordering::Relaxed) {
            json_response(StatusCode::OK, r#"{"primary":true}"#)
        } else {
            json_response(StatusCode::SERVICE_UNAVAILABLE, r#"{"primary":false}"#)
        }
    }

    /// The database half of [`Router::readiness_check`]: `Some(503)` when a
    /// configured SQL proxy has never reached its upstream, `None` otherwise.
    ///
    /// Split out so the behavior can be tested without a live PHP runtime.
    fn db_not_ready(
        db_health: Option<&Arc<crate::db_health::DbProxyHealth>>,
    ) -> Option<Response<ServerBody>> {
        let pending = db_health?.first_never_connected()?;
        Some(json_response_owned(
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                r#"{{"status":"not_ready","reason":"database proxy has not reached its upstream: {}"}}"#,
                json_escape(&pending.describe())
            ),
        ))
    }

    /// Apply custom response headers from config.
    ///
    /// The header pairs are already validated to `(HeaderName,
    /// HeaderValue)` at Router construction time, so this hot-path
    /// method is one clone per pair — no per-response
    /// `HeaderName::from_bytes` / `HeaderValue::from_str` parse.
    fn apply_response_headers(&self, response: &mut Response<ServerBody>) {
        let headers = response.headers_mut();
        for (name, value) in &self.response_headers {
            headers.insert(name.clone(), value.clone());
        }
    }

    /// Run the middleware **request** phase for a static-file request, before
    /// the file is read (issue #395, security half). Fail-closed, exactly like
    /// the PHP path's request phase: a `RESPOND` verdict (e.g. an auth denial)
    /// short-circuits and the file is never served. A `REWRITE`'s path/header
    /// overrides are ignored on this branch (the file is already resolved and
    /// no PHP runs); only its appended response headers are carried through.
    #[allow(clippy::too_many_arguments)]
    fn static_request_phase(
        &self,
        req_headers: Option<&[(String, String)]>,
        method: &str,
        path: &str,
        query: &str,
        remote_addr: SocketAddr,
        server_name: &str,
        is_https: bool,
    ) -> StaticGate {
        let Some(chain) = self.middleware_chain.as_ref() else {
            return StaticGate::Continue(Vec::new());
        };
        // Static requests carry no buffered body, but scheme/host are still
        // authoritative from the connection (a `force_https` gate on static
        // assets needs the real scheme).
        let ctx = ephpm_middleware::host::RequestCtx::new(
            method,
            path,
            query,
            &remote_addr.ip().to_string(),
            server_name,
            req_headers.unwrap_or(&[]),
        )
        .with_scheme(is_https)
        .with_host(&normalize_host_key(server_name));
        match chain.evaluate(&ctx, path) {
            crate::middleware::ChainVerdict::Respond { status, body, headers } => {
                StaticGate::Respond(middleware_response(status, body, &headers))
            }
            crate::middleware::ChainVerdict::Continue { response_headers, .. } => {
                StaticGate::Continue(response_headers)
            }
        }
    }

    /// Run the middleware **response** phase over a generated response, letting
    /// response-capable modules transform it (compression, ETag, header
    /// injection) in reverse chain order.
    ///
    /// Only **buffered** responses are transformed: a streamed body
    /// (worker-mode `send_response_stream`, large files streamed from disk)
    /// has no exact `size_hint` — only a fully-in-memory `Full<Bytes>` reports
    /// one — and bypasses the phase untouched, so a stream is never buffered
    /// or corrupted. Header values that are not valid UTF-8 are invisible to
    /// modules and pass through unchanged. When a module replaced the body,
    /// `Content-Length` is recomputed here.
    #[allow(clippy::too_many_arguments)]
    async fn run_response_phase(
        &self,
        response: Response<ServerBody>,
        req_headers: Option<&[(String, String)]>,
        method: &str,
        path: &str,
        query: &str,
        remote_addr: SocketAddr,
        server_name: &str,
        is_https: bool,
    ) -> Response<ServerBody> {
        use hyper::body::Body as _;

        let Some(chain) = self.middleware_chain.as_ref() else {
            return response;
        };
        let (mut parts, body) = response.into_parts();

        // Buffered-only: a streamed body has no exact size_hint. Bypass rather
        // than risk buffering (or corrupting) a stream.
        if body.size_hint().exact().is_none() {
            static STREAM_BYPASS_LOG: std::sync::Once = std::sync::Once::new();
            STREAM_BYPASS_LOG.call_once(|| {
                tracing::debug!(
                    "response-phase middleware skipped for streamed responses (v1: buffered only)"
                );
            });
            return Response::from_parts(parts, body);
        }

        // The body's exact size_hint guarantees it is already in memory, so
        // this collect resolves immediately without blocking.
        let collected = match body.collect().await {
            Ok(buf) => buf.to_bytes(),
            Err(err) => {
                // Unreachable for a buffered `Full<Bytes>` (uninhabited error
                // type); handle defensively rather than panic.
                tracing::error!(%err, "response-phase body collect failed; returning 500");
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "500 Internal Server Error",
                );
            }
        };

        // Split headers: UTF-8-decodable ones are visible to (and mutable by)
        // modules; any non-decodable value passes through untouched so a rare
        // binary header is never lost in the round-trip.
        let mut decodable: Vec<(String, String)> = Vec::new();
        let mut opaque: Vec<(hyper::header::HeaderName, hyper::header::HeaderValue)> = Vec::new();
        for (name, value) in &parts.headers {
            match value.to_str() {
                Ok(text) => decodable.push((name.as_str().to_owned(), text.to_owned())),
                Err(_) => opaque.push((name.clone(), value.clone())),
            }
        }

        let ctx = ephpm_middleware::host::RequestCtx::new(
            method,
            path,
            query,
            &remote_addr.ip().to_string(),
            server_name,
            req_headers.unwrap_or(&[]),
        )
        .with_scheme(is_https)
        .with_host(&normalize_host_key(server_name));
        let outcome = chain.run_response_phase(
            &ctx,
            path,
            parts.status.as_u16(),
            decodable,
            collected.to_vec(),
        );

        parts.status = StatusCode::from_u16(outcome.status).unwrap_or(parts.status);
        parts.headers.clear();
        for (name, value) in &outcome.headers {
            match (
                hyper::header::HeaderName::from_bytes(name.as_bytes()),
                hyper::header::HeaderValue::from_str(value),
            ) {
                (Ok(name), Ok(value)) => {
                    parts.headers.append(name, value);
                }
                _ => tracing::warn!(
                    header = %name,
                    "response-phase header is not valid HTTP — skipped"
                ),
            }
        }
        for (name, value) in opaque {
            parts.headers.append(name, value);
        }
        if outcome.body_replaced {
            // The body changed — make Content-Length authoritative so a
            // transform (compression, rewrite) can't desync framing.
            parts.headers.remove(hyper::header::CONTENT_LENGTH);
            if let Ok(value) = hyper::header::HeaderValue::from_str(&outcome.body.len().to_string())
            {
                parts.headers.insert(hyper::header::CONTENT_LENGTH, value);
            }
        }

        Response::from_parts(parts, body::buffered(Full::new(Bytes::from(outcome.body))))
    }

    /// Check if an IP address matches any trusted proxy CIDR.
    fn is_trusted_proxy(&self, ip: IpAddr) -> bool {
        self.trusted_proxies.iter().any(|net| net.contains(&ip))
    }

    /// Walk X-Forwarded-For from right to left, return the first untrusted IP.
    ///
    /// Uses `rsplit` to scan right-to-left in place — no `Vec` allocation for
    /// the (typically 1-3 element) proxy chain on this per-request path.
    fn resolve_xff(&self, xff: &str) -> Option<IpAddr> {
        for ip_str in xff.rsplit(',') {
            if let Ok(ip) = ip_str.trim().parse::<IpAddr>()
                && !self.is_trusted_proxy(ip)
            {
                return Some(ip);
            }
        }
        // All IPs in the chain are trusted (or unparseable) — use the leftmost.
        xff.split(',').next().and_then(|s| s.trim().parse().ok())
    }
}

/// Check if a URI path contains a hidden (dot-prefixed) segment.
fn has_hidden_segment(uri_path: &str) -> bool {
    uri_path.split('/').any(|segment| {
        segment.starts_with('.') && !segment.is_empty() && segment != "." && segment != ".."
    })
}

/// Returns `true` if any segment of `path` is exactly `..`.
///
/// Both `/` and `\` count as separators: once the URI path is joined onto a
/// document root, Windows treats the backslash as a real separator, so
/// `/a\..\b` traverses exactly like `/a/../b`.
fn has_dot_dot_segment(path: &str) -> bool {
    path.split(['/', '\\']).any(|segment| segment == "..")
}

/// Percent-decode a URI path so static-file lookup and routing work
/// against the literal characters the client meant.
///
/// Returns `None` if the input is malformed (truncated `%`, non-hex
/// digits), contains an encoded `/` / `\`, or contains a `..` path
/// segment — all three would let the request address a file the URI-level
/// checks never saw. Nothing downstream normalizes dot segments: the joined
/// path goes straight to the filesystem, so `/../b/index.php` on a raw
/// socket (browsers normalize it away, `curl --path-as-is` does not) would
/// otherwise escape the document root and bypass prefix-based blocks like
/// `/vendor/*`. Callers should treat `None` as a 400.
///
/// The output is validated as UTF-8; an invalid sequence also yields
/// `None`. ASCII paths (the overwhelming majority) round-trip exactly.
fn percent_decode_path(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    // Fast path: the overwhelming majority of request paths contain no `%`.
    // `raw` is already a valid `&str` (UTF-8), so with nothing to decode we
    // can hand back an owned copy without the byte-by-byte scan, the
    // `Vec<u8>` build, or the trailing `from_utf8` re-validation.
    if !bytes.contains(&b'%') {
        return if has_dot_dot_segment(raw) { None } else { Some(raw.to_owned()) };
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = hex_nibble(bytes[i + 1])?;
            let lo = hex_nibble(bytes[i + 2])?;
            let byte = (hi << 4) | lo;
            if byte == b'/' || byte == b'\\' {
                return None;
            }
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let decoded = String::from_utf8(out).ok()?;
    // `%2e%2e` decodes to `..`, so the dot-segment check has to run on the
    // decoded form, not the raw one.
    if has_dot_dot_segment(&decoded) {
        return None;
    }
    Some(decoded)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Test-only reference implementation kept for the legacy glob unit
/// tests; production routing uses [`CompiledGlob::matches`] which is
/// pre-split at Router construction.
#[cfg(test)]
fn is_path_blocked(uri_path: &str, blocked: &[String]) -> bool {
    blocked.iter().any(|pattern| glob_match(pattern, uri_path))
}

/// Simple glob matching for URI paths.
///
/// Kept for the unit-test corpus that pins the historical match
/// semantics; production paths go through [`CompiledGlob::matches`],
/// which uses this same segment-matching logic but on pre-split
/// segments so the hot path is allocation-free.
#[cfg_attr(not(test), allow(dead_code))]
///
/// Supports `*` as a wildcard matching any sequence of characters within
/// a single path segment (no `/`), and exact prefix matching for patterns
/// ending with `/*` (matches the directory and all children).
fn glob_match(pattern: &str, path: &str) -> bool {
    if !pattern.contains('*') {
        // Exact match or prefix match for directories
        return path == pattern || (pattern.ends_with('/') && path.starts_with(pattern));
    }

    // Split into segments and match segment-by-segment
    let pat_segs: Vec<&str> = pattern.split('/').collect();
    let uri_segs: Vec<&str> = path.split('/').collect();

    // Pattern ending with /* matches directory and all children
    if pattern.ends_with("/*") && pat_segs.len() == uri_segs.len().min(pat_segs.len()) {
        let prefix = &pat_segs[..pat_segs.len() - 1];
        let uri_prefix = &uri_segs[..prefix.len().min(uri_segs.len())];
        if prefix.len() <= uri_segs.len()
            && prefix.iter().zip(uri_prefix.iter()).all(|(p, s)| segment_match(p, s))
        {
            return true;
        }
    }

    if pat_segs.len() != uri_segs.len() {
        return false;
    }

    pat_segs.iter().zip(uri_segs.iter()).all(|(p, s)| segment_match(p, s))
}

/// Match a single path segment against a pattern segment.
/// `*` matches any non-empty sequence of characters.
fn segment_match(pattern: &str, segment: &str) -> bool {
    if pattern == "*" {
        return !segment.is_empty();
    }
    if !pattern.contains('*') {
        return pattern == segment;
    }
    // Simple *.ext or prefix* matching
    if let Some(suffix) = pattern.strip_prefix('*') {
        return segment.ends_with(suffix);
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return segment.starts_with(prefix);
    }
    // prefix*suffix
    if let Some((prefix, suffix)) = pattern.split_once('*') {
        return segment.starts_with(prefix)
            && segment.ends_with(suffix)
            && segment.len() >= prefix.len() + suffix.len();
    }
    pattern == segment
}

/// Collect inbound request headers into owned `(name, value)` pairs, dropping
/// any header whose name (case-insensitively) appears in `strip` before it can
/// reach the middleware chain or PHP.
///
/// `strip` is the router's [`Router::ingest_strip_headers`] list — always
/// `proxy` (httpoxy) plus every configured JWT `claims_header`. Stripping here,
/// at the single ingest point shared by the fpm and worker dispatch paths,
/// guarantees a client-forged value can never surface as `$_SERVER['HTTP_*']`
/// even when the JWT middleware is skipped by its `match` glob (or absent).
fn extract_headers(headers: &hyper::HeaderMap, strip: &[String]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| !strip.iter().any(|s| name.as_str().eq_ignore_ascii_case(s)))
        .map(|(name, value)| (name.to_string(), value.to_str().unwrap_or("").to_string()))
        .collect()
}

/// Map an HTTP method to a `&'static str` metrics label.
///
/// Standard methods return their canonical spelling as a `&'static str`
/// (no allocation on the metrics hot path, issue #136). Any non-standard
/// verb collapses to `"OTHER"` so a client sending random custom methods
/// cannot explode Prometheus `method`-label cardinality.
fn method_metric_label(method: &hyper::Method) -> &'static str {
    match *method {
        hyper::Method::GET => "GET",
        hyper::Method::POST => "POST",
        hyper::Method::PUT => "PUT",
        hyper::Method::DELETE => "DELETE",
        hyper::Method::HEAD => "HEAD",
        hyper::Method::OPTIONS => "OPTIONS",
        hyper::Method::PATCH => "PATCH",
        hyper::Method::TRACE => "TRACE",
        hyper::Method::CONNECT => "CONNECT",
        _ => "OTHER",
    }
}

/// Map an HTTP status code to a `&'static str` metrics label.
///
/// The `metrics` macros require label values to be `'static`; returning a
/// static literal keeps the metrics hot path allocation-free (issue #136)
/// where the previous code allocated `status.as_u16().to_string()` per
/// request. Covers the status codes this server and typical PHP
/// applications emit; anything outside the set collapses to `"other"` so
/// an app returning arbitrary codes cannot explode `status`-label
/// cardinality.
fn status_metric_label(status: StatusCode) -> &'static str {
    match status.as_u16() {
        200 => "200",
        201 => "201",
        202 => "202",
        204 => "204",
        206 => "206",
        301 => "301",
        302 => "302",
        303 => "303",
        304 => "304",
        307 => "307",
        308 => "308",
        400 => "400",
        401 => "401",
        403 => "403",
        404 => "404",
        405 => "405",
        406 => "406",
        409 => "409",
        410 => "410",
        413 => "413",
        415 => "415",
        421 => "421",
        422 => "422",
        429 => "429",
        500 => "500",
        501 => "501",
        502 => "502",
        503 => "503",
        504 => "504",
        _ => "other",
    }
}

/// Synthesize a `Host` header from the URI `:authority` when the request has
/// none.
///
/// HTTP/2 and HTTP/3 carry the host in the `:authority` pseudo-header, which
/// hyper exposes as the request-URI authority rather than a `Host` header. So
/// that every downstream host consumer agrees — vhost/document-root resolution
/// (`extract_server_name`), the trusted-host gate, and the `HTTP_HOST`
/// `$_SERVER` variable PHP receives (built from the `Host` header) — this copies
/// the authority into a real `Host` header at ingress. Idempotent: a request
/// that already carries a `Host` header (HTTP/1.1) is left untouched.
fn ensure_host_header<B>(req: &mut Request<B>) {
    if !req.headers().contains_key(http::header::HOST)
        && let Some(authority) = req.uri().authority().cloned()
        && let Ok(value) = hyper::header::HeaderValue::from_str(authority.as_str())
    {
        req.headers_mut().insert(http::header::HOST, value);
    }
}

/// Build the CGI `REQUEST_URI` as the origin-form target: the path plus an
/// optional `?query`, never a scheme or authority.
///
/// Over HTTP/1.1 hyper's request URI is already origin-form (`/path?q`), but
/// over HTTP/2 and HTTP/3 it is the absolute form (`https://host/path?q`).
/// Handing that absolute form to PHP as `REQUEST_URI` makes apps build
/// canonical redirects to a mangled URL — e.g. WordPress redirected `/` to
/// `https://demo.preview.ephpm.devhttps/demo.preview.ephpm.dev/`. Taking
/// `path_and_query` yields the identical `/path?q` for every protocol version.
fn request_uri_origin_form<B>(req: &Request<B>) -> String {
    req.uri()
        .path_and_query()
        .map_or_else(|| req.uri().path().to_string(), |pq| pq.as_str().to_string())
}

fn extract_server_name<B>(req: &Request<B>) -> String {
    // HTTP/1.1 carries the host in the `Host` header; HTTP/2 and HTTP/3 carry it
    // in the `:authority` pseudo-header, which hyper exposes as the request-URI
    // authority, NOT as a synthesized `Host` header. Reading only `Host` made
    // every browser request over TLS (which negotiates HTTP/2 via ALPN) resolve
    // to `localhost` — the default document root — so multi-tenant vhosts 404'd
    // over HTTPS while working over HTTP/1.1. Prefer `Host`, then fall back to
    // the URI authority, and only then to `localhost`.
    if let Some(host) = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(|h| h.split(':').next().unwrap_or(h))
        .filter(|h| !h.is_empty())
    {
        return host.to_string();
    }
    if let Some(authority) = req.uri().authority() {
        // `Authority::host()` already excludes any userinfo and port.
        return authority.host().to_string();
    }
    "localhost".to_string()
}

/// Normalize a raw `Host` value into the canonical vhost-lookup key: strip the
/// port, strip a single trailing FQDN-root dot, and lowercase.
///
/// This is the one normalization shared by every host-keyed lookup —
/// [`Router::resolve_site`] (which turns it into the canonical site key), the
/// [`is_valid_site_key`] gate that guards the `sites_dir` join, and
/// [`crate::site_wire_auth::SiteWireAuth`] (which applies it to a client-
/// asserted MySQL username). Keeping them on a single function is what stops the
/// normalizations from drifting apart (a pentest found `resolve_site` and the
/// SERVER_NAME path lowercasing/suffix-stripping differently).
///
/// Note this is only *half* of a tenant's identity: it does not strip
/// `[server] sites_domain_suffix`, so it is not on its own a site key. The
/// canonical key is what [`Router::resolve_site`] returns — a fixed point of
/// this function, by construction (already lowercase, port-free, and with no
/// trailing dot), which is what lets the wire path apply this to a username and
/// land on the same key the router injected.
pub(crate) fn normalize_host_key(host: &str) -> String {
    host.split(':').next().unwrap_or("").trim_end_matches('.').to_ascii_lowercase()
}

/// Vhost key used for OPcache clustered invalidation (`opcache:version:<key>`).
///
/// Takes the request's already-resolved canonical site key (see
/// [`ResolvedSite`]) rather than re-normalizing the `Host` header, so a request
/// served from `<sites_dir>/blog/` invalidates against `opcache:version:blog` —
/// matching exactly what `ephpm deploy --site blog` writes — no matter which of
/// that vhost's names (`blog`, `blog.localhost`, `BLOG.`) the client used.
///
/// A host that names no known site has no deployable identity, so it maps to
/// [`crate::opcache::DEFAULT_VHOST`] (`_default`, the default document root)
/// rather than to a key invented from the header. That also keeps the
/// invalidation key space bounded by the site fleet instead of by what a client
/// can type.
fn opcache_vhost_key(site_key: Option<&str>) -> String {
    site_key.map_or_else(|| crate::opcache::DEFAULT_VHOST.to_string(), str::to_owned)
}

/// Whether a **normalized** host key (see [`normalize_host_key`]) is safe to
/// join onto `sites_dir` as a virtual-host directory name.
///
/// This is the fix for the unauthenticated `Host`-header path traversal
/// (issue #275): `sites_dir.join(host)` with an unsanitized `Host` let
/// `Host: ../../../../../etc` + `GET /passwd` escape `sites_dir` and serve
/// `/etc/passwd`, and `Host: ../single` point the document root — and PHP
/// execution — at an arbitrary directory. The downstream containment checks
/// (`serve_file`'s `starts_with(canonical_root)` and `php_script_contained`)
/// could not catch it because they are anchored to *this* attacker-chosen
/// root, which canonicalizes to the escaped directory.
///
/// The rule is a strict allowlist rather than a `..` denylist: a key is valid
/// only if it is a non-empty sequence of DNS-style labels drawn from
/// `[a-z0-9._-]`, with no empty label. That single "no empty label" test
/// rejects every dangerous dot combination at once — a bare `.`/`..`, an
/// embedded `a..b`, and a leading/trailing dot — while `/`, `\`, NUL, and any
/// other separator or control byte are simply outside the charset. Because no
/// `..` segment and no path separator can survive, `sites_dir.join(key)` can
/// only ever name a direct child of `sites_dir` — the escape is closed
/// lexically, independent of `trusted_hosts` (empty by default).
///
/// Note this is a *lexical* guarantee, deliberately not a
/// `canonicalize().starts_with(sites_dir)` check: the router intentionally
/// supports a site directory that is a symlink to a release tree outside
/// `sites_dir` (atomic-deploy layout — see [`Router::canonical_root`]), and a
/// canonical-containment gate would break that supported pattern. Confining
/// the join to a direct child is enough, since the label can no longer contain
/// traversal primitives.
pub(crate) fn is_valid_site_key(key: &str) -> bool {
    // Cap length defensively (DNS names max 253; allow a little slack). An
    // empty key would `join("")` back to `sites_dir` itself — reject it.
    if key.is_empty() || key.len() > 255 {
        return false;
    }
    // Allowlist charset. `/`, `\`, NUL, `:`, `%`, whitespace, and every other
    // separator/control byte fall outside it and are rejected here.
    if !key
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.'))
    {
        return false;
    }
    // With `.` allowed for subdomains, an empty label is the one traversal the
    // charset check can't see: it means a leading dot, a trailing dot, or a
    // `..` segment. Any of those would traverse when joined.
    if key.split('.').any(str::is_empty) {
        return false;
    }
    true
}

/// Build a JSON response with the given status and body.
///
/// Takes a `&'static str` and constructs the body with
/// [`Bytes::from_static`], so canned responses (`/_ephpm/health`,
/// `/_ephpm/ready`) do not allocate a `Vec<u8>` per request.
fn json_response(status: StatusCode, body: &'static str) -> Response<ServerBody> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(body::buffered(Full::new(Bytes::from_static(body.as_bytes()))))
        .expect("static json response")
}

/// Stash PHP-path timings in the response extensions so `Router::handle`
/// can hand the already-taken measurements to the request timeline without
/// re-measuring anything.
fn with_php_timings(
    mut resp: Response<ServerBody>,
    timings: crate::timeline::PhpTimings,
) -> Response<ServerBody> {
    resp.extensions_mut().insert(timings);
    resp
}

/// A JSON response whose body is built at runtime.
///
/// Only used off the hot path (the readiness probe, which needs to name the
/// proxy that is holding the pod down).
fn json_response_owned(status: StatusCode, body: String) -> Response<ServerBody> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(body::buffered(Full::new(Bytes::from(body))))
        .expect("owned json response")
}

/// Escape a string for embedding in a JSON string literal.
///
/// The readiness reason interpolates config-derived text (a listen address
/// and an upstream `host:port`). Those are operator-controlled rather than
/// attacker-controlled, but a stray quote would still emit a body that no
/// probe could parse, so escape rather than trust.
fn json_escape(s: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                // Writing to a String is infallible.
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// What the router needs to hand each tenant its own `pdo_mysql` credentials.
///
/// Cheap to hold: [`SiteWireAuth`](crate::site_wire_auth::SiteWireAuth) is an
/// `Arc` shared with the listener, so this is a pointer plus an address string.
struct PerSiteDbWire {
    /// Mints each site's password. The *same* instance the listener verifies
    /// against — sharing it, rather than deriving twice from a copied secret,
    /// is what makes "the router injects what the listener accepts" a
    /// structural property instead of a convention.
    auth: crate::site_wire_auth::SiteWireAuth,
    /// The bound MySQL address (`host:port`) tenants connect to.
    listen: String,
}

/// Build database environment variables from config for PHP injection.
///
/// When a DB backend has `inject_env = true`, produces `DB_HOST`, `DB_PORT`,
/// `DB_NAME`, `DB_USER`, `DB_PASSWORD`, `DB_CONNECTION`, and `DATABASE_URL`
/// pointing at the proxy listener. PHP frameworks auto-discover these.
fn build_db_env_vars(config: &Config) -> Vec<(String, String)> {
    // MySQL takes precedence (most common for PHP).
    if let Some(ref mysql) = config.db.mysql
        && mysql.inject_env
    {
        let listen = mysql.listen.as_deref().unwrap_or("127.0.0.1:3306");
        return db_env_from_url(listen, &mysql.url, "mysql");
    }
    if let Some(ref pg) = config.db.postgres
        && pg.inject_env
    {
        let listen = pg.listen.as_deref().unwrap_or("127.0.0.1:5432");
        return db_env_from_url(listen, &pg.url, "pgsql");
    }
    Vec::new()
}

/// Parse a database URL and proxy listen address into env var pairs.
fn db_env_from_url(listen: &str, backend_url: &str, driver: &str) -> Vec<(String, String)> {
    let (host, port) = listen.rsplit_once(':').unwrap_or((listen, "3306"));

    // Parse: scheme://user:password@host:port/dbname
    let rest = backend_url.find("://").map_or(backend_url, |i| &backend_url[i + 3..]);
    let (creds, host_db) = rest.split_once('@').unwrap_or(("", rest));
    let (user, password) = creds.split_once(':').unwrap_or((creds, ""));
    let db_name = host_db.split_once('/').map_or("", |(_, db)| db).split('?').next().unwrap_or("");

    vec![
        ("DB_HOST".into(), host.into()),
        ("DB_PORT".into(), port.into()),
        ("DB_NAME".into(), db_name.into()),
        ("DB_USER".into(), user.into()),
        ("DB_PASSWORD".into(), password.into()),
        ("DB_CONNECTION".into(), driver.into()),
        ("DATABASE_URL".into(), format!("{driver}://{user}:{password}@{host}:{port}/{db_name}")),
    ]
}

/// Build the HTTP response for a middleware `RESPOND` verdict.
///
/// Defaults `content-type` to `text/plain` when the module set none, and
/// degrades to a plain 500 if the module produced an invalid status code or
/// header (a native module is trusted but not infallible).
fn middleware_response(
    status: u16,
    body: Vec<u8>,
    headers: &[(String, String)],
) -> Response<ServerBody> {
    let Ok(status) = StatusCode::from_u16(status) else {
        tracing::error!(status, "middleware RESPOND returned an invalid status — returning 500");
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "500 Internal Server Error");
    };
    let mut builder = Response::builder().status(status);
    let mut has_content_type = false;
    for (name, value) in headers {
        has_content_type |= name.eq_ignore_ascii_case("content-type");
        builder = builder.header(name.as_str(), value.as_str());
    }
    if !has_content_type {
        builder = builder.header("content-type", "text/plain");
    }
    builder.body(body::buffered(Full::new(Bytes::from(body)))).unwrap_or_else(|e| {
        tracing::error!(%e, "middleware RESPOND produced an invalid response — returning 500");
        error_response(StatusCode::INTERNAL_SERVER_ERROR, "500 Internal Server Error")
    })
}

/// Append middleware-supplied response headers (`ChainVerdict::Continue`) to
/// the response this request produced. Appends rather than replaces so
/// duplicates like `Set-Cookie` survive; entries that are not valid HTTP
/// header names/values are skipped with a warning.
fn apply_response_headers(
    mut resp: Response<ServerBody>,
    headers: &[(String, String)],
) -> Response<ServerBody> {
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            hyper::header::HeaderName::from_bytes(name.as_bytes()),
            hyper::header::HeaderValue::from_str(value),
        ) {
            resp.headers_mut().append(name, value);
        } else {
            tracing::warn!(header = %name, "middleware response header is not valid HTTP — skipped");
        }
    }
    resp
}

/// Apply one middleware request-header override: replace the value
/// case-insensitively when the header exists (removing any duplicate
/// occurrences so the override wins outright), append otherwise.
fn override_header(headers: &mut Vec<(String, String)>, name: String, value: String) {
    let mut replaced = false;
    headers.retain_mut(|(n, v)| {
        if n.eq_ignore_ascii_case(&name) {
            if replaced {
                return false;
            }
            replaced = true;
            v.clone_from(&value);
        }
        true
    });
    if !replaced {
        headers.push((name, value));
    }
}

/// Build a simple error response with a text body.
///
/// Takes a `&'static str` and constructs the body with
/// [`Bytes::from_static`]; every call site passes a compile-time
/// string literal (`"400 Bad Request"`, `"403 Forbidden"`, ...), so
/// error responses do not allocate a body `Vec<u8>` per request.
fn error_response(status: StatusCode, body: &'static str) -> Response<ServerBody> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(body::buffered(Full::new(Bytes::from_static(body.as_bytes()))))
        .expect("static error response")
}

/// Why a PHP-bound request was answered 503 without ever running PHP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShedReason {
    /// No execution slot within the shed budget (`[php] overload_policy =
    /// "shed"`). The server is up and healthy — it is *saturated*, and saying so
    /// immediately is the point.
    Overloaded,
    /// The execution pool is draining or has no live threads. Pre-existing
    /// behaviour, unchanged; kept distinct so an overload 503 and a
    /// shutting-down 503 are not conflated in metrics or logs.
    Closed,
}

impl From<crate::fpm_pool::DispatchClosed> for ShedReason {
    fn from(_: crate::fpm_pool::DispatchClosed) -> Self {
        Self::Closed
    }
}

impl From<crate::fpm_pool::DispatchRejected> for ShedReason {
    fn from(rejected: crate::fpm_pool::DispatchRejected) -> Self {
        match rejected {
            crate::fpm_pool::DispatchRejected::Full => Self::Overloaded,
            crate::fpm_pool::DispatchRejected::Closed => Self::Closed,
        }
    }
}

/// The 503 for a request that never reached PHP.
///
/// Built and returned through the ordinary response path — the response hyper
/// is already waiting on for this request — never a bare `try_write` on the
/// socket. That distinction is the #299 lesson: a 503 written behind hyper's
/// back races the connection state machine and can be dropped, so the shed
/// would be invisible to the client it was meant to protect.
///
/// An overload shed carries `Retry-After: 1`: the condition is transient by
/// construction (a slot frees up as soon as an in-flight request finishes), and
/// a concrete value is what makes a proxy or client back off instead of
/// hot-looping the retry.
fn shed_response(reason: ShedReason, engine: &'static str) -> Response<ServerBody> {
    match reason {
        ShedReason::Overloaded => {
            counter!("ephpm_php_shed_total", "engine" => engine).increment(1);
            let mut resp = error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "503 Service Unavailable (overloaded)",
            );
            resp.headers_mut()
                .insert(hyper::header::RETRY_AFTER, hyper::header::HeaderValue::from_static("1"));
            resp
        }
        ShedReason::Closed => {
            error_response(StatusCode::SERVICE_UNAVAILABLE, "503 Service Unavailable")
        }
    }
}

/// Build a simple error response with a dynamically-owned text body.
///
/// Companion to [`error_response`] for the (few) cold paths that
/// need to embed a runtime-formatted body (fallback status codes,
/// PHP execution error messages). The hot path uses `error_response`
/// with a `&'static str` and pays no allocation.
fn error_response_owned(status: StatusCode, body: String) -> Response<ServerBody> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(body::buffered(Full::new(Bytes::from(body))))
        .expect("owned-body error response")
}

/// Build an HTTP response from a PHP execution result, optionally compressing.
///
/// Prefers Brotli (`br`) over gzip when the client supports it.
/// Stream a hyper request body into a [`WorkerBody::Streaming`] (Phase 3).
///
/// Spawns a task that reads the `Incoming` body frame-by-frame and forwards
/// each data frame into a bounded channel the worker drains via `body_read`.
/// The bounded channel is the backpressure point: a slow PHP reader stalls the
/// hyper read, so ePHPm never buffers more than a few chunks regardless of the
/// upload size. The task ends (closing the channel = EOF) on the last frame, a
/// read error, or when the worker drops the receiver (request done early).
fn stream_request_body<B: RequestBody>(
    req: Request<B>,
    content_length: Option<u64>,
    max_body_size: u64,
) -> (ephpm_php::worker_bridge::WorkerBody, Arc<std::sync::atomic::AtomicBool>) {
    use http_body_util::BodyExt;

    let (tx, rx) =
        tokio::sync::mpsc::channel::<Bytes>(ephpm_php::worker_bridge::BODY_CHANNEL_DEPTH);
    let mut body = req.into_body();

    // Set when the cumulative body size exceeds `max_body_size`. The
    // Content-Length pre-check can't see chunked / lying clients, so the cap
    // is enforced on the actual bytes; the router turns the flag into a 413
    // regardless of what the worker produced from the truncated body.
    let overflow = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let overflow_task = Arc::clone(&overflow);

    tokio::spawn(async move {
        let mut total: u64 = 0;
        loop {
            match body.frame().await {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        if data.is_empty() {
                            continue;
                        }
                        total = total.saturating_add(data.len() as u64);
                        if max_body_size > 0 && total > max_body_size {
                            overflow_task.store(true, std::sync::atomic::Ordering::Release);
                            counter!("ephpm_http_body_overflow_total").increment(1);
                            tracing::warn!(
                                total,
                                max_body_size,
                                "request body exceeded max_body_size mid-stream — \
                                 truncating body and answering 413"
                            );
                            break;
                        }
                        // send().await suspends when the channel is full,
                        // applying backpressure without blocking a thread. Err
                        // means the worker finished/dropped the receiver.
                        if tx.send(data).await.is_err() {
                            break;
                        }
                    }
                    // Non-data frames (trailers) are ignored for the body.
                }
                Some(Err(e)) => {
                    tracing::debug!(%e, "request body stream error — ending body");
                    break;
                }
                None => break, // clean EOF
            }
        }
        // Dropping `tx` here closes the channel => worker sees EOF.
    });

    // `content_length` is advisory (declared length); 0 for chunked/unknown.
    let declared_len = usize::try_from(content_length.unwrap_or(0)).unwrap_or(usize::MAX);
    (ephpm_php::worker_bridge::WorkerBody::Streaming { rx, declared_len }, overflow)
}

/// Build a chunked, streamed HTTP response from a worker-mode streaming
/// response (Phase 3). Status + headers are known now; the body flows from the
/// channel as PHP produces it. No content-length (the length is unknown up
/// front) — hyper uses chunked transfer encoding.
///
/// Compression: gated by `[server.response] compression_streaming`. With the
/// default `Off` (or a client without `Accept-Encoding: br`, or a body PHP
/// already encoded) the body channel is passed to hyper untouched — the exact
/// pre-knob code path, no wrapper allocated. Otherwise the channel is wrapped
/// in a flush-per-chunk brotli encoder task ([`crate::stream_compress`]) and
/// `Content-Encoding: br` + `Vary: Accept-Encoding` are added.
///
/// `aborted` travels with the body all the way to hyper: if the worker dies
/// before finishing, the body ends in an error rather than a clean EOF, so the
/// client cannot read the truncation as a successful `200`. It is threaded past
/// the brotli wrapper unchanged — the wrapper's own channel closes only after
/// the worker's does, so the flag is already set by the time the outer stream
/// observes the end.
fn build_streamed_worker_response(
    status: u16,
    headers: Vec<(String, String)>,
    body_rx: tokio::sync::mpsc::Receiver<Bytes>,
    aborted: ephpm_php::worker_bridge::StreamAbortFlag,
    accepts_br: bool,
    compression: CompressionSettings,
) -> Response<ServerBody> {
    let compress = streamed_response_wants_brotli(&headers, accepts_br, compression.streaming);

    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let mut resp = Response::builder().status(status);
    for (name, value) in &headers {
        // Skip content-length: the streamed length is not known in advance, and
        // a stale/incorrect one would corrupt framing.
        if name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        resp = resp.header(name.as_str(), value.as_str());
    }

    let body = if compress {
        resp = resp.header("content-encoding", "br").header("vary", "Accept-Encoding");
        body::channel_body(crate::stream_compress::brotli_stream_body(body_rx), aborted)
    } else {
        body::channel_body(body_rx, aborted)
    };
    resp.body(body).unwrap_or_else(|_| {
        error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
    })
}

/// Decide whether a streamed worker response gets the brotli wrapper.
///
/// Requires all of:
/// - mode != `Off`;
/// - the client advertised brotli support (`accepts_br` — which already
///   folds in the master `[server.response] compression` switch);
/// - PHP did not set its own `Content-Encoding` (never double-encode);
/// - for `Sse` mode, a `text/event-stream` Content-Type.
fn streamed_response_wants_brotli(
    headers: &[(String, String)],
    accepts_br: bool,
    mode: StreamingCompression,
) -> bool {
    if mode == StreamingCompression::Off || !accepts_br {
        return false;
    }
    if headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-encoding")) {
        return false;
    }
    match mode {
        StreamingCompression::Off => false,
        StreamingCompression::All => true,
        StreamingCompression::Sse => headers.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case("content-type")
                && v.trim_start().to_ascii_lowercase().starts_with("text/event-stream")
        }),
    }
}

fn build_php_response(
    result: Result<
        Result<ephpm_php::response::PhpResponse, ephpm_php::PhpError>,
        tokio::task::JoinError,
    >,
    accepts_gzip: bool,
    accepts_br: bool,
    compression: CompressionSettings,
) -> Response<ServerBody> {
    match result {
        Ok(Ok(php_response)) => {
            let status = StatusCode::from_u16(php_response.status).unwrap_or(StatusCode::OK);
            let ct = php_response
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                .map_or("", |(_, v)| v.as_str());

            let original_len = php_response.body.len();
            #[allow(clippy::cast_precision_loss)]
            {
                histogram!("ephpm_http_response_body_bytes", "handler" => "php")
                    .record(original_len as f64);
                histogram!("ephpm_php_output_bytes").record(original_len as f64);
            }

            // Try Brotli first (better ratio), then fall back to gzip.
            let (body_bytes, encoding) = if accepts_br {
                brotli_compress(&php_response.body, ct, compression)
                    .map_or_else(|| (php_response.body, None), |c| (c, Some("br")))
            } else if accepts_gzip {
                gzip_compress(&php_response.body, ct, compression)
                    .map_or_else(|| (php_response.body, None), |c| (c, Some("gzip")))
            } else {
                (php_response.body, None)
            };

            if encoding.is_some() && original_len > 0 {
                #[allow(clippy::cast_precision_loss)]
                histogram!("ephpm_http_compression_ratio")
                    .record(body_bytes.len() as f64 / original_len as f64);
            }

            let mut resp = Response::builder().status(status);
            for (name, value) in &php_response.headers {
                resp = resp.header(name.as_str(), value.as_str());
            }
            if let Some(enc) = encoding {
                resp = resp.header("content-encoding", enc).header("vary", "Accept-Encoding");
            }
            resp = resp.header("content-length", body_bytes.len());

            resp.body(body::buffered(Full::new(Bytes::from(body_bytes)))).unwrap_or_else(|_| {
                error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
            })
        }
        Ok(Err(err)) => {
            tracing::error!(%err, "PHP execution failed");
            error_response_owned(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("PHP execution error: {err}"),
            )
        }
        Err(err) => {
            tracing::error!(%err, "spawn_blocking task failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
        }
    }
}

/// Build the per-vhost `open_basedir` value: the site's document root plus
/// that vhost's **private** state root (`state_root`).
///
/// Historically the second entry was the shared `std::env::temp_dir()`, which
/// put the *same* `/tmp` inside every tenant's `open_basedir` — so one tenant
/// could read and overwrite another tenant's temp files and PHP session files
/// (issue #276, pentest finding C3: cross-tenant temp read/write and session
/// hijack). Each vhost now gets its own [`vhost_state_root`] instead, so a
/// tenant's basedir never overlaps another tenant's temp/session storage.
///
/// PHP splits `open_basedir` on the platform's `PATH_SEPARATOR` — `:` on
/// Unix, `;` on Windows. Hardcoding `:` produced a single bogus Windows
/// entry (`C:\sites\blog:/tmp`) that matches no path, so every file access
/// under a vhost was denied. Deriving the separator from the platform keeps
/// the value valid on both.
///
/// `ServerConfig::effective_open_basedir` defaults to `true` whenever
/// `sites_dir` is set, so this value is what a default vhost deployment runs
/// with.
fn vhost_open_basedir_value(document_root: &Path, state_root: &Path) -> String {
    let separator = if cfg!(windows) { ';' } else { ':' };
    format!("{}{separator}{}", document_root.display(), state_root.display())
}

/// Per-vhost private state root — the parent directory that holds this
/// tenant's `tmp/` and `sessions/` subdirectories.
///
/// Derived from the already-resolved, traversal-safe `document_root` (issue
/// #280 hardened `resolve_site` so the root can never point outside
/// `sites_dir`), NOT from the raw `Host` header — so it inherits that
/// safety and needs no separate sanitization. The name combines a readable
/// label (the document root's final component) with a deterministic 64-bit
/// digest of the full canonical-ish path, so:
///   * two sites that happen to share a leaf directory name never collide, and
///   * the same site maps to the same directory across restarts, so its
///     sessions and temp files persist (the digest uses fixed hasher keys).
///
/// The base lives under the system temp dir (honouring `TMPDIR`) — the same
/// writable location the shared temp used to point at — but namespaced under
/// `ephpm-vhosts/` and split per site, so no two tenants share a parent that
/// would appear in each other's `open_basedir`.
fn vhost_state_root(document_root: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};

    // DefaultHasher uses fixed keys, so the digest is stable across processes
    // and restarts (unlike a randomly-seeded HashMap hasher). This is a
    // uniqueness/dedup key, not a security primitive, so SipHash's collision
    // resistance is more than sufficient.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    document_root.hash(&mut hasher);
    let digest = hasher.finish();

    let label = document_root
        .file_name()
        .and_then(|s| s.to_str())
        .map_or_else(|| "site".to_string(), sanitize_path_label);

    std::env::temp_dir().join("ephpm-vhosts").join(format!("{label}-{digest:016x}"))
}

/// Reduce a directory-name label to a conservative `[A-Za-z0-9._-]` set so it
/// is always a single, benign path component. The digest suffix in
/// [`vhost_state_root`] guarantees uniqueness, so this only needs to keep the
/// human-readable prefix from introducing separators or other surprises.
fn sanitize_path_label(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .take(64)
        .collect();
    if cleaned.is_empty() { "site".to_string() } else { cleaned }
}

/// Create `dir` (and any missing parents), then, on Unix, tighten it to
/// `0700` so a tenant's temp/session files are not readable by other OS
/// users. On Windows the default ACLs are left in place (there is no cheap
/// portable equivalent, and the tenant boundary is `open_basedir`, not OS
/// permissions).
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// The three per-vhost private paths injected into a request's PHP sandbox.
#[derive(Clone, Debug)]
struct VhostPrivateDirs {
    /// Parent of `temp`/`sessions`; the entry added to `open_basedir`.
    state_root: PathBuf,
    /// `sys_temp_dir` / `upload_tmp_dir` target for this vhost.
    temp: PathBuf,
    /// `session.save_path` target for this vhost (files handler).
    sessions: PathBuf,
}

/// Check if a filesystem path is a PHP file.
fn is_php_file(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("php"))
}

/// Expand `$uri` and `$query_string` variables in a `fallback` entry.
///
/// Borrows when the entry has no `$` placeholders — the common case for
/// literal fallback entries, probed on every request.
fn expand_variables<'a>(
    entry: &'a str,
    uri_path: &str,
    query_string: &str,
) -> std::borrow::Cow<'a, str> {
    if !entry.contains('$') {
        return std::borrow::Cow::Borrowed(entry);
    }
    std::borrow::Cow::Owned(entry.replace("$uri", uri_path).replace("$query_string", query_string))
}

/// Split an expanded path into the path component and optional query string.
fn split_path_query(expanded: &str) -> (&str, &str) {
    expanded.split_once('?').unwrap_or((expanded, ""))
}

/// Check if the request's Accept-Encoding header contains the given encoding.
fn accepts_encoding<B>(req: &Request<B>, encoding: &str) -> bool {
    req.headers()
        .get("accept-encoding")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains(encoding))
}

/// Content types eligible for gzip compression.
fn is_compressible(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || content_type.contains("javascript")
        || content_type.contains("json")
        || content_type.contains("xml")
        || content_type.contains("svg")
}

/// Try to gzip-compress a body. Returns `None` if not worth compressing.
#[must_use]
pub fn gzip_compress(
    data: &[u8],
    content_type: &str,
    settings: CompressionSettings,
) -> Option<Vec<u8>> {
    if data.len() < settings.min_size || !is_compressible(content_type) {
        return None;
    }
    let level = Compression::new(settings.level);
    let mut encoder = GzEncoder::new(Vec::new(), level);
    encoder.write_all(data).ok()?;
    let compressed = encoder.finish().ok()?;
    if compressed.len() < data.len() { Some(compressed) } else { None }
}

/// Try to Brotli-compress a body. Returns `None` if not worth compressing.
///
/// Brotli typically achieves 15-25% better compression than gzip on text
/// content, making it the preferred choice when the client supports it.
#[must_use]
pub fn brotli_compress(
    data: &[u8],
    content_type: &str,
    settings: CompressionSettings,
) -> Option<Vec<u8>> {
    if data.len() < settings.min_size || !is_compressible(content_type) {
        return None;
    }
    // Map gzip level (1-9) to Brotli quality (0-11). Brotli 4-6 is a
    // good balance of speed and ratio for on-the-fly compression.
    let quality = settings.level.min(9);
    let mut compressed = Vec::new();
    {
        let mut encoder = brotli::CompressorWriter::new(
            &mut compressed,
            4096, // buffer size
            quality,
            22, // lgwin (default window size)
        );
        encoder.write_all(data).ok()?;
        // CompressorWriter flushes on drop, but we need to handle errors.
        // Drop triggers the final flush.
    }
    if compressed.len() < data.len() { Some(compressed) } else { None }
}

/// Build the KV store key for caching a PHP response's `ETag`.
///
/// Format: `{prefix}{method}:{path}` or `{prefix}{method}:{path}?{query}` if query string is present.
fn php_etag_cache_key(prefix: &str, method: &str, path: &str, query: &str) -> String {
    if query.is_empty() {
        format!("{prefix}{method}:{path}")
    } else {
        format!("{prefix}{method}:{path}?{query}")
    }
}

/// Check if a stored `ETag` value matches the client's `If-None-Match` header.
///
/// Implements RFC 7232 semantics:
/// - Handles `*` (matches any `ETag`)
/// - Handles comma-separated lists of `ETag`s
/// - Trims whitespace correctly
fn etag_matches_value(etag: &str, if_none_match: &str) -> bool {
    let trimmed = if_none_match.trim();
    if trimmed == "*" {
        return true;
    }
    trimmed.split(',').any(|tag| tag.trim() == etag)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use ephpm_config::{ClusterConfig, Config, DbConfig, KvConfig, PhpConfig, ServerConfig};
    use ephpm_kv::store::StoreConfig;
    use http_body_util::Empty;

    use super::*;

    fn test_store() -> Arc<Store> {
        Store::new(StoreConfig::default())
    }

    fn test_router(dir: &Path) -> Router {
        let config = Config {
            server: ServerConfig {
                listen: "0.0.0.0:8080".to_string(),
                document_root: dir.to_path_buf(),
                index_files: vec!["index.php".to_string(), "index.html".to_string()],
                fallback: vec![
                    "$uri".to_string(),
                    "$uri/".to_string(),
                    "/index.php?$query_string".to_string(),
                ],
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        Router::new(&config, test_store(), None, None, None, None, None)
    }

    /// A browser over TLS negotiates HTTP/2, which carries the host in the
    /// `:authority` pseudo-header (surfaced as the request-URI authority), not a
    /// `Host` header. `extract_server_name` must read it, or every HTTPS request
    /// resolves to `localhost` — the default document root — and multi-tenant
    /// vhosts 404 while HTTP/1.1 works. Regression guard for that.
    #[test]
    fn extract_server_name_prefers_host_then_authority() {
        // HTTP/1.1: `Host` header, port stripped.
        let req = Request::builder()
            .method("GET")
            .uri("/x")
            .header("host", "demo.preview.ephpm.dev:443")
            .body(Empty::<Bytes>::new())
            .unwrap();
        assert_eq!(extract_server_name(&req), "demo.preview.ephpm.dev");

        // HTTP/2: no `Host` header; host is in the URI `:authority`.
        let req = Request::builder()
            .method("GET")
            .uri("https://demo.preview.ephpm.dev/x")
            .body(Empty::<Bytes>::new())
            .unwrap();
        assert!(req.headers().get("host").is_none());
        assert_eq!(extract_server_name(&req), "demo.preview.ephpm.dev");

        // `:authority` with an explicit port is stripped by `Authority::host()`.
        let req = Request::builder()
            .method("GET")
            .uri("https://demo.preview.ephpm.dev:8443/x")
            .body(Empty::<Bytes>::new())
            .unwrap();
        assert_eq!(extract_server_name(&req), "demo.preview.ephpm.dev");

        // Neither present → `localhost` fallback (unchanged behaviour).
        let req = Request::builder().method("GET").uri("/x").body(Empty::<Bytes>::new()).unwrap();
        assert_eq!(extract_server_name(&req), "localhost");
    }

    /// `ensure_host_header` must copy the HTTP/2 `:authority` into a real `Host`
    /// header, because PHP's `HTTP_HOST` `$_SERVER` var is built from the `Host`
    /// header. Without it an HTTP/2 request reaches the right document root but
    /// PHP sees an empty `HTTP_HOST` — WordPress then computes `localhost` URLs
    /// and 404s. Regression guard for the `$_SERVER['HTTP_HOST']` half of the
    /// HTTP/2 vhost fix.
    #[test]
    fn ensure_host_header_synthesizes_from_authority() {
        // HTTP/2: no `Host` header; authority present → header synthesized.
        let mut req = Request::builder()
            .method("GET")
            .uri("https://demo.preview.ephpm.dev/x")
            .body(Empty::<Bytes>::new())
            .unwrap();
        assert!(req.headers().get("host").is_none());
        ensure_host_header(&mut req);
        assert_eq!(
            req.headers().get("host").and_then(|v| v.to_str().ok()),
            Some("demo.preview.ephpm.dev")
        );

        // `:authority` with an explicit port is preserved verbatim (a real
        // `Host` header keeps the port too).
        let mut req = Request::builder()
            .method("GET")
            .uri("https://demo.preview.ephpm.dev:8443/x")
            .body(Empty::<Bytes>::new())
            .unwrap();
        ensure_host_header(&mut req);
        assert_eq!(
            req.headers().get("host").and_then(|v| v.to_str().ok()),
            Some("demo.preview.ephpm.dev:8443")
        );

        // HTTP/1.1: an existing `Host` header is left untouched (idempotent).
        let mut req = Request::builder()
            .method("GET")
            .uri("/x")
            .header("host", "explicit.example.com")
            .body(Empty::<Bytes>::new())
            .unwrap();
        ensure_host_header(&mut req);
        assert_eq!(
            req.headers().get("host").and_then(|v| v.to_str().ok()),
            Some("explicit.example.com")
        );

        // Neither present → nothing to synthesize; no `Host` header added.
        let mut req =
            Request::builder().method("GET").uri("/x").body(Empty::<Bytes>::new()).unwrap();
        ensure_host_header(&mut req);
        assert!(req.headers().get("host").is_none());
    }

    /// `REQUEST_URI` must be origin-form for every protocol. Over HTTP/2 the
    /// request URI is absolute (`https://host/path?q`); emitted verbatim it
    /// makes WordPress build canonical redirects to a mangled URL. Regression
    /// guard for the `REQUEST_URI` third of the HTTP/2 vhost fix.
    #[test]
    fn request_uri_origin_form_strips_scheme_and_authority() {
        // HTTP/2 absolute form → path + query only.
        let req = Request::builder()
            .method("GET")
            .uri("https://demo.preview.ephpm.dev/wp-admin/?foo=bar")
            .body(Empty::<Bytes>::new())
            .unwrap();
        assert_eq!(request_uri_origin_form(&req), "/wp-admin/?foo=bar");

        // HTTP/2 absolute form, root path.
        let req = Request::builder()
            .method("GET")
            .uri("https://demo.preview.ephpm.dev/")
            .body(Empty::<Bytes>::new())
            .unwrap();
        assert_eq!(request_uri_origin_form(&req), "/");

        // HTTP/1.1 origin form is already correct and unchanged.
        let req = Request::builder()
            .method("GET")
            .uri("/wp-admin/?foo=bar")
            .body(Empty::<Bytes>::new())
            .unwrap();
        assert_eq!(request_uri_origin_form(&req), "/wp-admin/?foo=bar");
    }

    // ── Middleware response phase + static-path request phase (#395) ──────

    fn chain_with(mounts: Vec<MiddlewareMount>) -> Arc<crate::middleware::MiddlewareChain> {
        Arc::new(crate::middleware::MiddlewareChain::load(&mounts).expect("chain loads"))
    }

    /// The response phase MUST run on the **static-file** path, not just PHP —
    /// that is half of #395's value. A header-injection module mounted on a
    /// static site stamps its header onto a served `.html` file.
    ///
    /// The module is the compiled-in `header-transform` **builtin**, not the
    /// `mw_response_header` cdylib example this test used to `dlopen`. Nothing
    /// in a test build puts that example on disk — `cargo test` and `cargo
    /// nextest` do not emit example artifacts (which is why CI runs an explicit
    /// `cargo build --workspace --lib --examples` first, see `ci.yml`) — so the
    /// artifact assertion turned a bare `cargo test` on a clean checkout into a
    /// hard failure on every platform (issue #435, reported from Windows). What
    /// this test is *about* is the router calling the response phase on the
    /// static path; which lane the module was loaded through is irrelevant
    /// here, and the dlopen lane keeps its own coverage in
    /// `tests/middleware_dlopen.rs` and `tests/middleware_response_phase.rs`.
    #[tokio::test]
    async fn response_phase_runs_on_static_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("page.html"), b"<h1>hi</h1>").unwrap();
        let mount = MiddlewareMount {
            library: "header-transform".to_string(),
            match_pattern: None,
            order: 10,
            config: Some(serde_json::json!({
                "response": { "set": { "X-Resp-Phase": "static" } }
            })),
        };
        let router = test_router(dir.path()).with_middleware_chain(Some(chain_with(vec![mount])));
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        let req =
            Request::builder().method("GET").uri("/page.html").body(Empty::<Bytes>::new()).unwrap();
        let resp = router.handle(req, addr, false).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("x-resp-phase").and_then(|v| v.to_str().ok()),
            Some("static"),
            "the response phase must run on the static-file path (#395 transform half)"
        );
    }

    /// The **request** phase (fail-closed) MUST run on the static path too, so
    /// an auth module can deny a static asset *before the file is read* — the
    /// security half of #395. A gated file is answered 401 and its bytes never
    /// reach the client; an ungated sibling is served normally.
    #[tokio::test]
    async fn request_phase_gates_static_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("secret.txt"), b"TOPSECRET").unwrap();
        std::fs::write(dir.path().join("public.txt"), b"public").unwrap();
        // jwt (builtin) scoped to the secret asset; no response phase.
        let jwt = MiddlewareMount {
            library: "jwt".to_string(),
            match_pattern: Some("/secret.txt".to_string()),
            order: 10,
            config: Some(serde_json::json!({ "secret": "s3cret" })),
        };
        let router = test_router(dir.path()).with_middleware_chain(Some(chain_with(vec![jwt])));
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        // Gated asset, no bearer token -> denied; the file is never served.
        let req = Request::builder()
            .method("GET")
            .uri("/secret.txt")
            .body(Empty::<Bytes>::new())
            .unwrap();
        let resp = router.handle(req, addr, false).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_ne!(&body[..], b"TOPSECRET", "the gated file's bytes must never leave disk");

        // Ungated sibling: served normally (glob does not match it).
        let req = Request::builder()
            .method("GET")
            .uri("/public.txt")
            .body(Empty::<Bytes>::new())
            .unwrap();
        let resp = router.handle(req, addr, false).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"public");
    }

    // ── Ingest header hygiene (Finding 3) ────────────────────────────────

    /// A JWT middleware mount scoped to `/api/*` with an explicit claims
    /// header. On a non-API path this module never runs, so the strip at
    /// ingest is the only thing standing between a forged claims header and
    /// PHP's `$_SERVER`.
    fn jwt_mount(claims_header: &str) -> MiddlewareMount {
        MiddlewareMount {
            library: "jwt".to_string(),
            match_pattern: Some("/api/*".to_string()),
            order: 10,
            config: Some(serde_json::json!({
                "secret": "s3cret",
                "claims_header": claims_header,
            })),
        }
    }

    #[test]
    fn ingest_strip_list_always_contains_proxy() {
        let strip = build_ingest_strip_headers(&[]);
        assert!(
            strip.iter().any(|h| h == "proxy"),
            "Proxy (httpoxy) must always be stripped even with no middleware: {strip:?}"
        );
    }

    // ── Alt-Svc (HTTP/3 discovery) ───────────────────────────────────

    /// Browsers only try HTTP/3 after seeing `Alt-Svc` on a TCP response, so
    /// this asserts it lands on the ordinary HTTP/1.1+2 path — not just on
    /// HTTP/3 responses, where it would be useless for discovery.
    #[tokio::test]
    async fn alt_svc_is_emitted_on_tls_responses() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        let router = test_router(dir.path()).with_alt_svc(443, 86400);
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        let req =
            Request::builder().method("GET").uri("/a.txt").body(Empty::<Bytes>::new()).unwrap();
        let resp = router.handle(req, addr, true).await.unwrap();

        assert_eq!(
            resp.headers().get(hyper::header::ALT_SVC).and_then(|v| v.to_str().ok()),
            Some("h3=\":443\"; ma=86400")
        );
    }

    /// An `http://` origin cannot upgrade to HTTP/3 (QUIC mandates TLS), so
    /// advertising there would only cost bytes.
    #[tokio::test]
    async fn alt_svc_is_absent_on_plaintext_responses() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        let router = test_router(dir.path()).with_alt_svc(443, 86400);
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        let req =
            Request::builder().method("GET").uri("/a.txt").body(Empty::<Bytes>::new()).unwrap();
        let resp = router.handle(req, addr, false).await.unwrap();

        assert!(resp.headers().get(hyper::header::ALT_SVC).is_none());
    }

    /// `alt_svc_max_age = 0` must suppress the header outright.
    #[tokio::test]
    async fn alt_svc_absent_when_max_age_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        let router = test_router(dir.path()).with_alt_svc(443, 0);
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        let req =
            Request::builder().method("GET").uri("/a.txt").body(Empty::<Bytes>::new()).unwrap();
        let resp = router.handle(req, addr, true).await.unwrap();

        assert!(resp.headers().get(hyper::header::ALT_SVC).is_none());
    }

    /// A router that was never told about an HTTP/3 endpoint must not
    /// advertise one — this is what keeps `Alt-Svc` from pointing at a port
    /// that never bound.
    #[tokio::test]
    async fn alt_svc_absent_when_http3_is_off() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        let router = test_router(dir.path());
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        let req =
            Request::builder().method("GET").uri("/a.txt").body(Empty::<Bytes>::new()).unwrap();
        let resp = router.handle(req, addr, true).await.unwrap();

        assert!(resp.headers().get(hyper::header::ALT_SVC).is_none());
    }

    // ── Preview marker + per-site rate cap ───────────────────────────

    fn preview_router(dir: &Path, limiter: Option<Arc<crate::rate_limit::Limiter>>) -> Router {
        let config = Config {
            server: ServerConfig {
                listen: "0.0.0.0:8080".to_string(),
                document_root: dir.to_path_buf(),
                preview: true,
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        Router::new(&config, test_store(), None, None, limiter, None, None)
    }

    /// A multi-site router with a per-site limiter: `sites_dir` holds the
    /// given site directories (each with an `index.php`), the default docroot
    /// gets one too, and `.localhost` is the domain suffix so one tenant is
    /// addressable under two hosts.
    fn per_site_router(
        root: &Path,
        sites: &[&str],
        limits: ephpm_config::ResolvedLimits,
    ) -> Router {
        let sites_dir = root.join("sites");
        fs::create_dir_all(&sites_dir).unwrap();
        for site in sites {
            let dir = sites_dir.join(site);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("index.php"), "<?php echo 'hi';").unwrap();
        }
        let docroot = root.join("docroot");
        fs::create_dir_all(&docroot).unwrap();
        fs::write(docroot.join("index.php"), "<?php echo 'default';").unwrap();

        let config = Config {
            server: ServerConfig {
                listen: "0.0.0.0:8080".to_string(),
                document_root: docroot,
                sites_dir: Some(sites_dir),
                sites_domain_suffix: Some(".localhost".to_string()),
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        let limiter = Some(Arc::new(crate::rate_limit::Limiter::new(limits)));
        Router::new(&config, test_store(), None, None, limiter, None, None)
    }

    fn get_with_host(uri: &str, host: &str) -> Request<Empty<Bytes>> {
        Request::builder()
            .method("GET")
            .uri(uri)
            .header("host", host)
            .body(Empty::<Bytes>::new())
            .unwrap()
    }

    /// `[server] preview = true` stamps `X-Ephpm-Preview: 1` on every
    /// response — success, 404, and the per-IP 429 early-return path.
    #[tokio::test]
    async fn preview_marker_on_every_response_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        // Success and 404, no limiter involved.
        let router = preview_router(dir.path(), None);
        for (uri, expect_status) in
            [("/a.txt", StatusCode::OK), ("/nope.txt", StatusCode::NOT_FOUND)]
        {
            let req =
                Request::builder().method("GET").uri(uri).body(Empty::<Bytes>::new()).unwrap();
            let resp = router.handle(req, addr, false).await.unwrap();
            assert_eq!(resp.status(), expect_status);
            assert_eq!(
                resp.headers().get("x-ephpm-preview").and_then(|v| v.to_str().ok()),
                Some("1"),
                "marker missing on {uri} ({expect_status})"
            );
        }

        // The per-IP 429 early return bypasses the main marker application
        // and must carry the marker via its own path.
        let limiter = Arc::new(crate::rate_limit::Limiter::new(ephpm_config::ResolvedLimits {
            per_ip_rate: 0.001,
            per_ip_burst: 1,
            ..ephpm_config::ResolvedLimits::default()
        }));
        let router = preview_router(dir.path(), Some(limiter));
        let req =
            Request::builder().method("GET").uri("/a.txt").body(Empty::<Bytes>::new()).unwrap();
        let first = router.handle(req, addr, false).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let req =
            Request::builder().method("GET").uri("/a.txt").body(Empty::<Bytes>::new()).unwrap();
        let denied = router.handle(req, addr, false).await.unwrap();
        assert_eq!(denied.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            denied.headers().get("x-ephpm-preview").and_then(|v| v.to_str().ok()),
            Some("1"),
            "marker missing on the per-IP 429 early return"
        );
    }

    /// Without `[server] preview` no marker is emitted anywhere.
    #[tokio::test]
    async fn no_preview_marker_when_preview_off() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        let router = test_router(dir.path());
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        let req =
            Request::builder().method("GET").uri("/a.txt").body(Empty::<Bytes>::new()).unwrap();
        let resp = router.handle(req, addr, false).await.unwrap();
        assert!(resp.headers().get("x-ephpm-preview").is_none());
    }

    /// The per-site cap is keyed by the CANONICAL site key from
    /// `resolve_site`, so one tenant addressed two ways (`blog.localhost`
    /// and `blog` under the domain suffix) drains ONE bucket — the
    /// #290/#291 invariant applied to rate limiting. A sibling site keeps
    /// its own budget, and the 429 carries `Retry-After`.
    #[tokio::test]
    async fn per_site_cap_uses_canonical_key_and_sets_retry_after() {
        let root = tempfile::tempdir().unwrap();
        // rate 0.5/s: no refill on a test timescale; Retry-After = 2s.
        let limits = ephpm_config::ResolvedLimits {
            per_site_rate: 0.5,
            per_site_burst: 2,
            ..ephpm_config::ResolvedLimits::default()
        };
        let router = per_site_router(root.path(), &["blog", "other"], limits);
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        // Two PHP requests within the burst — not rate-limited (stub mode has
        // no PHP engine, so "allowed" is any status but 429).
        for host in ["blog.localhost", "blog"] {
            let resp = router.handle(get_with_host("/index.php", host), addr, false).await.unwrap();
            assert_ne!(
                resp.status(),
                StatusCode::TOO_MANY_REQUESTS,
                "burst request via {host} must not be limited"
            );
        }

        // Third hit on the SAME tenant — via either host form — is over
        // budget: both forms drained the one canonical bucket.
        let resp = router
            .handle(get_with_host("/index.php", "blog.localhost"), addr, false)
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            resp.headers().get(hyper::header::RETRY_AFTER).and_then(|v| v.to_str().ok()),
            Some("2"),
            "429 must tell the client when a token refills (1/0.5 = 2s)"
        );

        // A different tenant still has its own full budget.
        let resp = router
            .handle(get_with_host("/index.php", "other.localhost"), addr, false)
            .await
            .unwrap();
        assert_ne!(resp.status(), StatusCode::TOO_MANY_REQUESTS, "sibling site must be unaffected");
    }

    /// A host that names no site (key = `None`) is never per-site-capped —
    /// nothing per-tenant may be minted from an unmatched host (issue #291),
    /// and that includes a rate-limit bucket.
    #[tokio::test]
    async fn unmatched_host_is_not_per_site_capped() {
        let root = tempfile::tempdir().unwrap();
        let limits = ephpm_config::ResolvedLimits {
            per_site_rate: 0.5,
            per_site_burst: 1,
            ..ephpm_config::ResolvedLimits::default()
        };
        let router = per_site_router(root.path(), &["blog"], limits);
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        for _ in 0..5 {
            let resp = router
                .handle(get_with_host("/index.php", "unknown.example.com"), addr, false)
                .await
                .unwrap();
            assert_ne!(
                resp.status(),
                StatusCode::TOO_MANY_REQUESTS,
                "unmatched host has no site key and must not be per-site-capped"
            );
        }
    }

    /// Static files are not counted against the per-site PHP cap: the cap
    /// protects PHP CPU, and a page's asset fan-out must not eat the budget.
    #[tokio::test]
    async fn static_files_do_not_consume_per_site_budget() {
        let root = tempfile::tempdir().unwrap();
        let limits = ephpm_config::ResolvedLimits {
            per_site_rate: 0.5,
            per_site_burst: 1,
            ..ephpm_config::ResolvedLimits::default()
        };
        let router = per_site_router(root.path(), &["blog"], limits);
        std::fs::write(root.path().join("sites").join("blog").join("style.css"), b"body{}")
            .unwrap();
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        // Many static hits — none consume the (burst = 1) PHP budget.
        for _ in 0..5 {
            let resp = router
                .handle(get_with_host("/style.css", "blog.localhost"), addr, false)
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }

        // The single PHP token is still available.
        let resp = router
            .handle(get_with_host("/index.php", "blog.localhost"), addr, false)
            .await
            .unwrap();
        assert_ne!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        // ... and now it is spent.
        let resp = router
            .handle(get_with_host("/index.php", "blog.localhost"), addr, false)
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn ingest_strip_list_includes_configured_jwt_claims_header() {
        let strip = build_ingest_strip_headers(&[jwt_mount("X-Jwt-Claims")]);
        // Lowercased for the case-insensitive ingest compare.
        assert!(strip.iter().any(|h| h == "x-jwt-claims"), "{strip:?}");
        assert!(strip.iter().any(|h| h == "proxy"), "{strip:?}");
    }

    #[test]
    fn ingest_strip_list_ignores_non_jwt_mounts() {
        // A cors mount carrying an incidental `claims_header` key must not
        // contribute — only the jwt module forwards claims.
        let cors = MiddlewareMount {
            library: "cors".to_string(),
            match_pattern: None,
            order: 10,
            config: Some(serde_json::json!({ "claims_header": "X-Not-Jwt" })),
        };
        let strip = build_ingest_strip_headers(&[cors]);
        assert!(!strip.iter().any(|h| h == "x-not-jwt"), "{strip:?}");
    }

    /// The core Finding 3 proof: with a `/api/*`-scoped jwt module, a request
    /// to a NON-matching path (`/index.php`) carrying a client-forged
    /// `Proxy` and a forged claims header must have BOTH stripped before the
    /// header list is handed to PHP — even though the middleware never ran.
    #[test]
    fn forged_proxy_and_claims_headers_absent_from_php_on_non_matching_path() {
        let strip = build_ingest_strip_headers(&[jwt_mount("X-Jwt-Claims")]);

        let mut incoming = hyper::HeaderMap::new();
        incoming.insert("Proxy", "http://evil.example:8080".parse().unwrap());
        incoming.insert("X-Jwt-Claims", "{\"sub\":\"admin\"}".parse().unwrap());
        incoming.insert("Host", "victim.example".parse().unwrap());
        incoming.insert("User-Agent", "curl/8".parse().unwrap());

        let handed_to_php = extract_headers(&incoming, &strip);

        assert!(
            !handed_to_php.iter().any(|(n, _)| n.eq_ignore_ascii_case("proxy")),
            "forged Proxy must not reach PHP: {handed_to_php:?}"
        );
        assert!(
            !handed_to_php.iter().any(|(n, _)| n.eq_ignore_ascii_case("x-jwt-claims")),
            "forged claims header must not reach PHP on a match-skipped path: {handed_to_php:?}"
        );
        // Legitimate headers are preserved untouched.
        assert!(handed_to_php.iter().any(|(n, _)| n.eq_ignore_ascii_case("host")));
        assert!(handed_to_php.iter().any(|(n, _)| n.eq_ignore_ascii_case("user-agent")));
    }

    // ── Request timeline (/_ephpm/requests) ──────────────────────────

    use tracing_subscriber::layer::SubscriberExt as _;

    /// `(span name, parent span name)` as collected by [`SpanTree`].
    type SpanRecord = (String, Option<String>);

    /// Test layer collecting `(span name, parent span name)` pairs for the
    /// router's request spans (target [`crate::OTEL_TRACE_TARGET`]).
    #[derive(Clone, Default)]
    struct SpanTree(Arc<std::sync::Mutex<Vec<SpanRecord>>>);

    impl SpanTree {
        fn snapshot(&self) -> Vec<SpanRecord> {
            self.0.lock().unwrap().clone()
        }
    }

    /// Makes the router's span callsites enabled **process-wide**, so a span
    /// test's thread-local collector can actually see them.
    ///
    /// Without this, the span tests are order-dependent flakes. `tracing`
    /// caches each callsite's `Interest` **globally and once**, at the callsite's
    /// first hit: `DefaultCallsite::register` asks `DISPATCHERS.rebuilder()`,
    /// which — with no global dispatcher installed — takes the `JustOne` path
    /// and consults `dispatcher::get_default`, i.e. *whatever subscriber was
    /// current on the thread that happened to hit the callsite first*. In this
    /// crate's test binary that is almost always a plain router test with no
    /// subscriber at all, whose `NoSubscriber` answers `Interest::never()` — and
    /// that verdict is then cached for every thread, forever. A span test that
    /// later installs a collector with `set_default` records nothing, and its
    /// `spans` snapshot comes back empty.
    ///
    /// `set_default` alone does not fix it. It does trigger a full interest
    /// rebuild (via `Dispatch::new` → `callsite::register_dispatch`), which
    /// repairs callsites already registered by then — which is why the common
    /// `http.request` callsite usually survives. But `php.execute` and
    /// `worker.queue_wait` are hit by only a handful of tests, so they are
    /// frequently *still unregistered* at that moment, and a concurrent
    /// no-subscriber thread can register them — latching `never` — in the window
    /// between the rebuild and the span test's own request.
    ///
    /// Installing a global default closes the window from both ends: the
    /// `set_global_default` call rebuilds the cache (repairing anything already
    /// latched `never`), and from then on the `JustOne` path resolves to this
    /// `Registry` — which enables everything — no matter which thread registers
    /// a callsite first. Collection stays per-test and thread-local; this
    /// subscriber only exists to keep the callsites alive.
    fn enable_span_callsites() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            // A plain `Registry` enables every callsite and collects nothing.
            // The error case (a global default already set) is not reachable
            // here, and would be harmless anyway.
            let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry());
        });
    }

    impl<S> tracing_subscriber::Layer<S> for SpanTree
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _id: &tracing::span::Id,
            ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if attrs.metadata().target() != crate::OTEL_TRACE_TARGET {
                return;
            }
            let parent = attrs
                .parent()
                .cloned()
                .or_else(|| {
                    if attrs.is_contextual() { ctx.current_span().id().cloned() } else { None }
                })
                .and_then(|pid| ctx.span(&pid).map(|s| s.name().to_string()));
            self.0.lock().unwrap().push((attrs.metadata().name().to_string(), parent));
        }
    }

    async fn fetch_timeline(router: &Router) -> Vec<serde_json::Value> {
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let req = Request::builder()
            .method("GET")
            .uri("/_ephpm/requests")
            .body(Empty::<Bytes>::new())
            .unwrap();
        let resp = router.handle(req, addr, false).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&body).unwrap()
    }

    /// A completed request lands in `/_ephpm/requests` with the shared
    /// measurements filled in — and the endpoint's own polls (and other
    /// `/_ephpm/` internals) are excluded from the buffer.
    #[tokio::test]
    async fn requests_endpoint_returns_entries_after_request() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let router = test_router(dir.path())
            .with_request_log(Some(Arc::new(crate::timeline::RequestLog::new(8))));
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        // One health poll (must NOT be recorded) and one static hit (must be).
        let req = Request::builder()
            .method("GET")
            .uri("/_ephpm/health")
            .body(Empty::<Bytes>::new())
            .unwrap();
        router.handle(req, addr, false).await.unwrap();
        let req =
            Request::builder().method("GET").uri("/a.txt").body(Empty::<Bytes>::new()).unwrap();
        let resp = router.handle(req, addr, false).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let entries = fetch_timeline(&router).await;
        assert_eq!(entries.len(), 1, "only /a.txt should be recorded: {entries:?}");
        assert_eq!(entries[0]["method"], "GET");
        assert_eq!(entries[0]["path"], "/a.txt");
        assert_eq!(entries[0]["status"], 200);
        assert!(entries[0]["total_ms"].as_f64().unwrap() >= 0.0);
        assert!(entries[0]["timestamp_ms"].as_u64().unwrap() > 0);
        assert!(entries[0]["queue_wait_ms"].is_null(), "static file has no queue wait");
        assert!(entries[0]["php_ms"].is_null(), "static file has no PHP time");
        assert_eq!(entries[0]["response_bytes"], 5);
    }

    /// With the timeline disabled (the serve-mode default), the endpoint
    /// must behave like any other unknown `/_ephpm/` path — here a plain
    /// 404 via the static fallback chain.
    #[tokio::test]
    async fn requests_endpoint_is_404_when_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let router = test_router_with_404(dir.path());
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        let req = Request::builder()
            .method("GET")
            .uri("/_ephpm/requests")
            .body(Empty::<Bytes>::new())
            .unwrap();
        let resp = router.handle(req, addr, false).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// Worker-mode PHP dispatch: the timeline entry carries the queue wait
    /// (from the existing `ephpm_worker_request_wait_seconds` measurement)
    /// with no PHP execution value when the worker never responds, and the
    /// request produces the 3-span tree `http.request` →
    /// {`worker.queue_wait`, `php.execute`}.
    ///
    /// A zero-worker pool (stub-mode-safe: spawns no PHP threads) accepts
    /// the dispatch but never answers, so the inner worker timeout fires.
    #[tokio::test(start_paused = true)]
    async fn worker_mode_records_queue_wait_and_emits_span_tree() {
        enable_span_callsites();
        let tree = SpanTree::default();
        let subscriber = tracing_subscriber::registry().with(tree.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.php"), b"<?php echo 1;").unwrap();
        let mut config = Config {
            server: ServerConfig {
                listen: "0.0.0.0:8080".to_string(),
                document_root: dir.path().to_path_buf(),
                index_files: vec!["index.php".to_string()],
                fallback: vec!["$uri".to_string(), "=404".to_string()],
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        // Short request deadline so the never-answering pool becomes a 504
        // without waiting out the 300s default (paused clock auto-advances).
        config.server.timeouts.request = 1;
        let pool = crate::worker_pool::WorkerPool::spawn(
            dir.path().join("worker.php"),
            0, // zero workers: dispatch queues, nobody answers
            500,
            4,
            Duration::from_secs(30),
            Duration::from_secs(60),
        );
        let router = Router::new(&config, test_store(), None, None, None, None, Some(pool))
            .with_request_log(Some(Arc::new(crate::timeline::RequestLog::new(8))));
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        let req =
            Request::builder().method("GET").uri("/index.php").body(Empty::<Bytes>::new()).unwrap();
        let resp = router.handle(req, addr, false).await.unwrap();
        assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);

        let entries = fetch_timeline(&router).await;
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0]["path"], "/index.php");
        assert_eq!(entries[0]["status"], 504);
        assert!(
            entries[0]["queue_wait_ms"].as_f64().unwrap() >= 0.0,
            "worker mode must record the dispatch-queue wait: {entries:?}"
        );
        assert!(
            entries[0]["php_ms"].is_null(),
            "a worker that never responded produced no execution measurement: {entries:?}"
        );

        let spans = tree.snapshot();
        let find = |name: &str| spans.iter().find(|(n, _)| n == name);
        let http = find("http.request").expect("http.request span must exist");
        assert_eq!(http.1, None, "http.request is the root span");
        assert_eq!(
            find("worker.queue_wait").expect("worker.queue_wait span must exist").1.as_deref(),
            Some("http.request")
        );
        assert_eq!(
            find("php.execute").expect("php.execute span must exist").1.as_deref(),
            Some("http.request")
        );
    }

    /// fpm-path PHP dispatch emits `http.request` → `php.execute` (no queue
    /// span — there is no dispatch queue on the spawn_blocking path) and the
    /// timeline entry records the execution time with an absent queue wait.
    /// Runs against the stub PHP runtime, where execution fails with a 500 —
    /// the measurement points are identical.
    #[tokio::test]
    async fn fpm_mode_spans_and_timeline_have_no_queue_wait() {
        enable_span_callsites();
        let tree = SpanTree::default();
        let subscriber = tracing_subscriber::registry().with(tree.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.php"), b"<?php echo 1;").unwrap();
        let router = test_router(dir.path())
            .with_request_log(Some(Arc::new(crate::timeline::RequestLog::new(8))));
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        let req =
            Request::builder().method("GET").uri("/index.php").body(Empty::<Bytes>::new()).unwrap();
        let resp = router.handle(req, addr, false).await.unwrap();
        // Stub mode: 200 (stub page) when another test already initialized
        // the process-global stub runtime, 500 (NotInitialized) otherwise.
        // The timing/span instrumentation sits on the same code path either
        // way, so accept both rather than depend on test order.
        assert!(
            resp.status() == StatusCode::OK || resp.status() == StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected stub-mode PHP status: {}",
            resp.status()
        );

        let entries = fetch_timeline(&router).await;
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(entries[0]["queue_wait_ms"].is_null(), "fpm mode has no worker queue");
        assert!(
            entries[0]["php_ms"].as_f64().unwrap() >= 0.0,
            "fpm mode records the spawn_blocking execution time: {entries:?}"
        );

        let spans = tree.snapshot();
        assert!(spans.iter().any(|(n, p)| n == "http.request" && p.is_none()), "{spans:?}");
        assert!(
            spans.iter().any(|(n, p)| n == "php.execute" && p.as_deref() == Some("http.request")),
            "{spans:?}"
        );
        assert!(
            !spans.iter().any(|(n, _)| n == "worker.queue_wait"),
            "no queue span on the fpm path: {spans:?}"
        );
    }

    /// The experimental pool engine (`[php] fpm_engine = "pool"`) dispatches a
    /// PHP request through ePHPm's dedicated thread pool and returns its
    /// response — the same stub 200 the spawn_blocking path yields — proving the
    /// dispatch → execute → oneshot round-trip and the router wiring. Runs in
    /// stub mode: `PhpRuntime::init()` sets the stub-runtime flag so pool
    /// threads register and `execute()` returns the stub page. The pool path
    /// records a (>= 0) dispatch-queue wait, unlike the spawn_blocking path.
    #[tokio::test]
    async fn fpm_pool_engine_dispatches_php_request() {
        ephpm_php::PhpRuntime::init().expect("stub runtime init");

        enable_span_callsites();
        let tree = SpanTree::default();
        let subscriber = tracing_subscriber::registry().with(tree.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.php"), b"<?php echo 1;").unwrap();
        let mut config = Config {
            server: ServerConfig {
                listen: "0.0.0.0:8080".to_string(),
                document_root: dir.path().to_path_buf(),
                index_files: vec!["index.php".to_string()],
                fallback: vec!["$uri".to_string(), "=404".to_string()],
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        // Opt into the pool engine with a small, deterministic pool size.
        config.php.fpm_engine = ephpm_config::FpmEngine::Pool;
        config.php.worker_count = 2;
        // Also set the (now-bypassed) semaphore cap to confirm it is ignored.
        config.php.workers = 4;

        let router = Router::new(&config, test_store(), None, None, None, None, None)
            .with_request_log(Some(Arc::new(crate::timeline::RequestLog::new(8))));
        assert!(router.fpm_pool().is_some(), "pool engine must build the fpm pool");
        assert!(router.php_semaphore.is_none(), "the workers semaphore is bypassed in pool mode");

        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let req =
            Request::builder().method("GET").uri("/index.php").body(Empty::<Bytes>::new()).unwrap();
        let resp = router.handle(req, addr, false).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "pool must return the stub PHP response");

        let entries = fetch_timeline(&router).await;
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0]["status"], 200);
        assert!(
            entries[0]["queue_wait_ms"].as_f64().unwrap() >= 0.0,
            "pool mode records the dispatch-queue wait: {entries:?}"
        );
        assert!(
            entries[0]["php_ms"].as_f64().unwrap() >= 0.0,
            "pool mode records the execution time: {entries:?}"
        );

        // Drain retires the pool threads so the test leaves no live PHP context.
        if let Some(pool) = router.fpm_pool() {
            pool.drain();
        }
    }

    // ── Load shedding (`[php] overload_policy`, issue #301) ─────────────

    /// A router on the default `spawn_blocking` engine with a `[php] workers`
    /// cap and the requested shed policy.
    fn shed_router(dir: &Path, policy: ephpm_config::OverloadPolicy, shed_after_ms: u64) -> Router {
        let mut config = Config {
            server: ServerConfig {
                listen: "0.0.0.0:8080".to_string(),
                document_root: dir.to_path_buf(),
                index_files: vec!["index.php".to_string()],
                fallback: vec!["$uri".to_string(), "=404".to_string()],
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        // One slot, so "the semaphore is held" == "the server is saturated".
        config.php.workers = 1;
        config.php.overload_policy = Some(policy);
        config.php.shed_after_ms = shed_after_ms;
        Router::new(&config, test_store(), None, None, None, None, None)
    }

    fn php_request() -> Request<Empty<Bytes>> {
        Request::builder().method("GET").uri("/index.php").body(Empty::<Bytes>::new()).unwrap()
    }

    /// The #301 fix on the default engine: with every `[php] workers` slot
    /// taken, an arriving PHP request is answered `503` + `Retry-After`
    /// immediately instead of queueing until the client gives up.
    ///
    /// Saturation is produced by holding the only permit, so the assertion does
    /// not depend on PHP timing (stub execution is instantaneous).
    #[tokio::test]
    async fn shed_policy_rejects_when_every_worker_slot_is_taken() {
        ephpm_php::PhpRuntime::init().expect("stub runtime init");
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.php"), b"<?php echo 1;").unwrap();
        let router = shed_router(dir.path(), ephpm_config::OverloadPolicy::Shed, 0);
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        let sem =
            Arc::clone(router.php_semaphore.as_ref().expect("workers cap builds a semaphore"));
        let held = sem.acquire_owned().await.expect("permit");

        let resp = router.handle(php_request(), addr, false).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "a saturated server must answer, not queue"
        );
        assert_eq!(
            resp.headers().get(hyper::header::RETRY_AFTER).and_then(|v| v.to_str().ok()),
            Some("1"),
            "an overload shed must tell the client when to come back"
        );

        // Releasing the slot restores normal service — shedding is transient
        // admission control, not a latched failure state.
        drop(held);
        let resp = router.handle(php_request(), addr, false).await.unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "the server must recover the moment a slot frees up"
        );
    }

    /// `shed_after_ms` is a grace window: a slot that frees up inside it is used
    /// rather than shed, so a microburst does not turn into 503s.
    #[tokio::test]
    async fn shed_after_ms_grace_admits_a_slot_that_frees_up() {
        ephpm_php::PhpRuntime::init().expect("stub runtime init");
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.php"), b"<?php echo 1;").unwrap();
        let router = shed_router(dir.path(), ephpm_config::OverloadPolicy::Shed, 2_000);
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        let sem =
            Arc::clone(router.php_semaphore.as_ref().expect("workers cap builds a semaphore"));
        let held = sem.acquire_owned().await.expect("permit");
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(held);
        });

        let resp = router.handle(php_request(), addr, false).await.unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "a slot freed inside the grace window must be used, not shed"
        );
    }

    /// The default policy is unchanged: `wait` still queues. Proven by the
    /// request completing only *after* the held permit is released — under
    /// `shed` the same setup returns 503 immediately (test above).
    #[tokio::test]
    async fn wait_policy_queues_instead_of_shedding() {
        ephpm_php::PhpRuntime::init().expect("stub runtime init");
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.php"), b"<?php echo 1;").unwrap();
        let router = shed_router(dir.path(), ephpm_config::OverloadPolicy::Wait, 0);
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        let sem =
            Arc::clone(router.php_semaphore.as_ref().expect("workers cap builds a semaphore"));
        let held = sem.acquire_owned().await.expect("permit");

        let handle = tokio::spawn(async move { router.handle(php_request(), addr, false).await });
        // Still queued after a beat — `wait` does not answer a saturated server.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!handle.is_finished(), "the wait policy must queue, not shed");

        drop(held);
        let resp = handle.await.unwrap().unwrap();
        assert_ne!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "the queued request is served");
    }

    fn test_router_with_404(dir: &Path) -> Router {
        let config = Config {
            server: ServerConfig {
                listen: "0.0.0.0:8080".to_string(),
                document_root: dir.to_path_buf(),
                index_files: vec!["index.php".to_string(), "index.html".to_string()],
                fallback: vec!["$uri".to_string(), "$uri/".to_string(), "=404".to_string()],
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        Router::new(&config, test_store(), None, None, None, None, None)
    }

    #[allow(dead_code)]
    fn test_router_with_store(dir: &Path, store: Arc<Store>) -> Router {
        let mut config = Config {
            server: ServerConfig {
                listen: "0.0.0.0:8080".to_string(),
                document_root: dir.to_path_buf(),
                index_files: vec!["index.php".to_string(), "index.html".to_string()],
                fallback: vec![
                    "$uri".to_string(),
                    "$uri/".to_string(),
                    "/index.php?$query_string".to_string(),
                ],
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        config.server.static_files.etag = true;
        Router::new(&config, store, None, None, None, None, None)
    }

    fn default_compression() -> CompressionSettings {
        CompressionSettings {
            enabled: true,
            level: 1,
            min_size: 1024,
            streaming: StreamingCompression::Off,
        }
    }

    /// Test helper: call resolve_fallback with the router's own defaults.
    fn resolve_fb(router: &Router, uri: &str, qs: &str) -> Resolved {
        router.resolve_fallback(
            uri,
            qs,
            &router.document_root,
            &router.index_files,
            &router.fallback,
        )
    }

    // ── fallback resolution ─────────────────────────────────────────

    #[test]
    fn test_existing_file_matches_uri() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("style.css"), "body{}").unwrap();

        let router = test_router(dir.path());
        let resolved = resolve_fb(&router, "/style.css", "");
        assert!(matches!(resolved, Resolved::File(p) if p == dir.path().join("style.css")));
    }

    #[test]
    fn test_existing_php_file_matches_uri() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("info.php"), "<?php phpinfo();").unwrap();

        let router = test_router(dir.path());
        let resolved = resolve_fb(&router, "/info.php", "");
        assert!(matches!(resolved, Resolved::File(p) if p == dir.path().join("info.php")));
    }

    #[test]
    fn test_directory_with_index_matches_uri_slash() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("index.php"), "<?php").unwrap();

        let router = test_router(dir.path());
        let resolved = resolve_fb(&router, "/", "");
        assert!(matches!(resolved, Resolved::File(p) if p == dir.path().join("index.php")));
    }

    #[test]
    fn test_directory_falls_to_index_html() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("index.html"), "<html>").unwrap();

        let router = test_router(dir.path());
        let resolved = resolve_fb(&router, "/", "");
        assert!(matches!(resolved, Resolved::File(p) if p == dir.path().join("index.html")));
    }

    #[test]
    fn test_permalink_falls_to_index_php() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("index.php"), "<?php").unwrap();

        let router = test_router(dir.path());
        let resolved = resolve_fb(&router, "/2024/hello-world", "p=123");
        assert!(matches!(resolved, Resolved::File(p) if p == dir.path().join("index.php")));
    }

    #[test]
    fn test_missing_file_with_404_fallback() {
        let dir = tempfile::tempdir().unwrap();

        let router = test_router_with_404(dir.path());
        let resolved = resolve_fb(&router, "/nope.css", "");
        assert!(matches!(resolved, Resolved::Status(404)));
    }

    #[test]
    fn test_missing_php_with_404_fallback() {
        let dir = tempfile::tempdir().unwrap();

        let router = test_router_with_404(dir.path());
        let resolved = resolve_fb(&router, "/nope.php", "");
        assert!(matches!(resolved, Resolved::Status(404)));
    }

    #[test]
    fn test_missing_with_no_index_falls_to_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let router = test_router(dir.path());
        let resolved = resolve_fb(&router, "/anything", "");
        assert!(matches!(resolved, Resolved::Status(404)));
    }

    #[test]
    fn test_subdirectory_with_index() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("blog")).unwrap();
        fs::write(dir.path().join("blog/index.php"), "<?php").unwrap();

        let router = test_router(dir.path());
        let resolved = resolve_fb(&router, "/blog/", "");
        assert!(matches!(resolved, Resolved::File(p) if p == dir.path().join("blog/index.php")));
    }

    // ── helper functions ─────────────────────────────────────────────

    #[test]
    fn test_expand_variables() {
        assert_eq!(expand_variables("$uri", "/hello", "foo=bar"), "/hello");
        assert_eq!(
            expand_variables("/index.php?$query_string", "/hello", "foo=bar"),
            "/index.php?foo=bar"
        );
        assert_eq!(expand_variables("$uri/", "/blog", ""), "/blog/");
    }

    #[test]
    fn test_split_path_query() {
        assert_eq!(split_path_query("/index.php?foo=bar"), ("/index.php", "foo=bar"));
        assert_eq!(split_path_query("/style.css"), ("/style.css", ""));
    }

    #[test]
    fn test_is_php_file_check() {
        assert!(is_php_file(Path::new("/var/www/index.php")));
        assert!(is_php_file(Path::new("test.PHP")));
        assert!(!is_php_file(Path::new("style.css")));
        assert!(!is_php_file(Path::new("README")));
    }

    // ── hidden files ──────────────────────────────────────────────────

    #[test]
    fn test_has_hidden_segment() {
        assert!(has_hidden_segment("/.env"));
        assert!(has_hidden_segment("/.git/config"));
        assert!(has_hidden_segment("/wp-content/.htaccess"));
        assert!(has_hidden_segment("/.hidden/file.txt"));
        assert!(!has_hidden_segment("/index.php"));
        assert!(!has_hidden_segment("/wp-content/uploads/file.jpg"));
        assert!(!has_hidden_segment("/"));
    }

    // ── compression ────────────────────────────────────────────────

    #[test]
    fn test_gzip_compress_small_body() {
        let data = b"too small";
        assert!(gzip_compress(data, "text/html", default_compression()).is_none());
    }

    #[test]
    fn test_gzip_compress_non_compressible() {
        let data = vec![0u8; 2048];
        assert!(gzip_compress(&data, "image/png", default_compression()).is_none());
    }

    #[test]
    fn test_gzip_compress_html() {
        let data = "<html><body>Hello World!</body></html>\n".repeat(100);
        let compressed = gzip_compress(data.as_bytes(), "text/html", default_compression());
        assert!(compressed.is_some());
        assert!(compressed.unwrap().len() < data.len());
    }

    #[test]
    fn test_gzip_compress_custom_min_size() {
        let settings = CompressionSettings {
            enabled: true,
            level: 1,
            min_size: 4096,
            streaming: StreamingCompression::Off,
        };
        let data = "a".repeat(2048);
        // 2048 bytes < 4096 min_size — should not compress
        assert!(gzip_compress(data.as_bytes(), "text/html", settings).is_none());
    }

    // ── trusted proxies ────────────────────────────────────────────

    #[test]
    fn test_resolve_xff_rightmost_untrusted() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config {
            server: ServerConfig {
                document_root: dir.path().to_path_buf(),
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        config.server.security.get_or_insert_default().trusted_proxies =
            vec!["10.0.0.0/8".to_string()];
        let router = Router::new(&config, test_store(), None, None, None, None, None);

        // 203.0.113.50 is the real client, 10.0.0.1 is the proxy
        let xff = "203.0.113.50, 10.0.0.1";
        let ip = router.resolve_xff(xff);
        assert_eq!(ip, Some("203.0.113.50".parse().unwrap()));
    }

    #[test]
    fn test_resolve_xff_all_trusted_uses_leftmost() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config {
            server: ServerConfig {
                document_root: dir.path().to_path_buf(),
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        config.server.security.get_or_insert_default().trusted_proxies =
            vec!["10.0.0.0/8".to_string()];
        let router = Router::new(&config, test_store(), None, None, None, None, None);

        let xff = "10.0.0.2, 10.0.0.1";
        let ip = router.resolve_xff(xff);
        assert_eq!(ip, Some("10.0.0.2".parse().unwrap()));
    }

    // ── percent decoding (fast-path parity) ─────────────────────────

    #[test]
    fn percent_decode_free_of_percent_roundtrips() {
        // The `%`-free fast path must return the exact input unchanged.
        for p in ["/", "/index.php", "/a/b/c.txt", "/wp-admin/", "/x?y=z"] {
            assert_eq!(percent_decode_path(p).as_deref(), Some(p), "fast path changed {p}");
        }
    }

    #[test]
    fn percent_decode_still_decodes_escapes() {
        // `%2E` -> '.', so `test%2Ehtml` decodes to `test.html`.
        assert_eq!(percent_decode_path("/test%2Ehtml").as_deref(), Some("/test.html"));
        assert_eq!(percent_decode_path("/a%20b").as_deref(), Some("/a b"));
    }

    #[test]
    fn percent_decode_rejects_encoded_slash_and_malformed() {
        // Encoded '/' (%2F) and '\' (%5C) must be rejected (traversal bypass).
        assert_eq!(percent_decode_path("/a%2Fb"), None);
        assert_eq!(percent_decode_path("/a%5Cb"), None);
        // Truncated / non-hex escapes are malformed.
        assert_eq!(percent_decode_path("/a%"), None);
        assert_eq!(percent_decode_path("/a%2"), None);
        assert_eq!(percent_decode_path("/a%zz"), None);
    }

    #[test]
    fn percent_decode_rejects_dot_dot_segments() {
        // Nothing downstream normalizes dot segments, and `has_hidden_segment`
        // explicitly exempts `..`, so a literal `../` used to be joined
        // straight onto the document root. Browsers normalize it away; a raw
        // socket or `curl --path-as-is` does not.
        assert_eq!(percent_decode_path("/../b/index.php"), None);
        assert_eq!(percent_decode_path("/a/../../etc/passwd"), None);
        assert_eq!(percent_decode_path("/a/.."), None);
        // `%2e%2e` decodes to `..` — the check must run after decoding.
        assert_eq!(percent_decode_path("/%2e%2e/b/index.php"), None);
        // Backslash is a separator once the path is joined on Windows.
        assert_eq!(percent_decode_path("/a\\..\\b"), None);
        // `..` inside a segment is an ordinary filename, not a traversal.
        assert_eq!(percent_decode_path("/a..b/c").as_deref(), Some("/a..b/c"));
        assert_eq!(percent_decode_path("/...").as_deref(), Some("/..."));
    }

    // ── PHP document-root containment ────────────────────────────────

    /// The PHP branch used to run `handle_php` on whatever
    /// `doc_root.join(path)` produced, guarded only by `is_php_allowed`
    /// (a URI prefix test). Under `sites_dir` vhosting that is cross-tenant
    /// code execution.
    #[test]
    fn php_script_outside_document_root_is_rejected() {
        let outer = tempfile::tempdir().unwrap();
        let docroot = outer.path().join("site-a");
        let sibling = outer.path().join("site-b");
        std::fs::create_dir_all(&docroot).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(docroot.join("index.php"), "<?php echo 'a';").unwrap();
        std::fs::write(sibling.join("index.php"), "<?php echo 'b';").unwrap();

        let router = test_router(&docroot);

        // Exactly what `probe_path` builds for `GET /../site-b/index.php`.
        let escaped = docroot.join("../site-b/index.php");
        assert!(escaped.is_file(), "traversal target must exist for this test to mean anything");
        assert!(
            !router.php_script_contained_flat(&escaped, &docroot),
            "a PHP script resolving outside the document root must not execute"
        );

        assert!(
            router.php_script_contained_flat(&docroot.join("index.php"), &docroot),
            "a PHP script inside the document root must still execute"
        );
    }

    #[test]
    fn php_script_missing_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let router = test_router(dir.path());
        // `canonicalize()` fails on a nonexistent path; treat that as
        // "not contained" rather than trusting the unresolved join.
        assert!(!router.php_script_contained_flat(&dir.path().join("nope.php"), dir.path()));
    }

    /// The regression that would actually hurt: `php_script_contained` now
    /// caches the canonicalized script path, so the traversal rejection has to
    /// survive a warm cache. A first call that populates the cache must not
    /// make a second identical call succeed, and a rejected path must not be
    /// remembered at all.
    #[test]
    fn php_script_traversal_stays_rejected_with_a_warm_cache() {
        let outer = tempfile::tempdir().unwrap();
        let docroot = outer.path().join("site-a");
        let sibling = outer.path().join("site-b");
        std::fs::create_dir_all(&docroot).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(docroot.join("index.php"), "<?php echo 'a';").unwrap();
        std::fs::write(sibling.join("index.php"), "<?php echo 'b';").unwrap();

        let router = test_router(&docroot);
        let escaped = docroot.join("../site-b/index.php");
        let legit = docroot.join("index.php");

        // Warm every cache the check touches (docroot and script) with a
        // legitimate hit first, so the traversal below runs against a
        // populated cache rather than a cold one.
        assert!(router.php_script_contained_flat(&legit, &docroot));
        assert!(
            router.php_script_contained_flat(&legit, &docroot),
            "cached hit must still be allowed"
        );

        for attempt in 0..3 {
            assert!(
                !router.php_script_contained_flat(&escaped, &docroot),
                "traversal must stay rejected on attempt {attempt} with a warm cache"
            );
        }
        // A rejected path is never remembered, so it cannot occupy a slot in
        // the bounded cache or be served from it later.
        assert!(
            !router.canonical_scripts.contains_key(&escaped),
            "a script that resolved outside the document root must not be cached"
        );

        // And the legitimate path is still allowed after the rejections.
        assert!(router.php_script_contained_flat(&legit, &docroot));
    }

    /// The cache stores the canonicalized path, never the verdict. Proving it:
    /// the *same* cached script path checked against a different document root
    /// must be rejected, even though the entry was inserted while contained.
    #[test]
    fn cached_script_is_rechecked_against_the_requesting_root() {
        let outer = tempfile::tempdir().unwrap();
        let root_a = outer.path().join("site-a");
        let root_b = outer.path().join("site-b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        std::fs::write(root_a.join("index.php"), "<?php echo 'a';").unwrap();

        let router = test_router(&root_a);
        let script = root_a.join("index.php");

        assert!(router.php_script_contained_flat(&script, &root_a));
        assert!(
            router.canonical_scripts.contains_key(&script),
            "the contained resolution should have been cached"
        );
        assert!(
            !router.php_script_contained_flat(&script, &root_b),
            "a cache entry from one vhost must never authorize a script under another"
        );
    }

    /// The vector the containment check actually exists for, now that `..` is
    /// rejected at ingest by `percent_decode_path`: a symlink inside the
    /// document root pointing at a sibling tenant. Unix-only — creating a
    /// symlink on Windows needs privileges CI does not grant.
    #[cfg(unix)]
    #[test]
    fn php_script_symlink_escape_is_rejected_hot_and_cold() {
        let outer = tempfile::tempdir().unwrap();
        let docroot = outer.path().join("site-a");
        let sibling = outer.path().join("site-b");
        std::fs::create_dir_all(&docroot).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("secrets.php"), "<?php echo 'b';").unwrap();
        std::os::unix::fs::symlink(sibling.join("secrets.php"), docroot.join("link.php")).unwrap();

        let router = test_router(&docroot);
        let link = docroot.join("link.php");
        assert!(link.is_file(), "the symlink must resolve for this test to mean anything");

        // Cold and warm: `canonicalize()` follows the symlink out of the
        // docroot both times, and the rejection is never cached as an allow.
        assert!(!router.php_script_contained_flat(&link, &docroot));
        assert!(!router.php_script_contained_flat(&link, &docroot));
        assert!(!router.canonical_scripts.contains_key(&link));
    }

    /// The cache must not grow without bound on client-controlled keys. This
    /// pins the cap enforcement without writing 4096 files: it pre-fills the
    /// map to the cap with synthetic live entries, then checks that a genuine
    /// resolution neither exceeds the cap nor changes the answer.
    #[test]
    fn canonical_script_cache_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.php"), "<?php echo 'a';").unwrap();
        let router = test_router(dir.path());

        let canon = dir.path().canonicalize().map(ephpm_config::strip_verbatim_prefix).unwrap();
        for i in 0..CANONICAL_SCRIPT_CACHE_MAX {
            router
                .canonical_scripts
                .insert(canon.join(format!("filler-{i}.php")), (canon.clone(), Instant::now()));
        }
        assert_eq!(router.canonical_scripts.len(), CANONICAL_SCRIPT_CACHE_MAX);

        // At the cap the entries are all live and the sweep is throttled, so
        // the new path is simply not remembered — the check still answers
        // correctly, it just pays the `canonicalize()` it already paid.
        assert!(router.php_script_contained_flat(&dir.path().join("index.php"), dir.path()));
        assert!(
            router.canonical_scripts.len() <= CANONICAL_SCRIPT_CACHE_MAX,
            "the cache must never exceed its cap"
        );
    }

    // ── port parsing ─────────────────────────────────────────────────

    #[test]
    fn test_new_parses_port() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            server: ServerConfig {
                listen: "0.0.0.0:3000".to_string(),
                document_root: dir.path().to_path_buf(),
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        let router = Router::new(&config, test_store(), None, None, None, None, None);
        assert_eq!(router.server_port, 3000);
    }

    #[test]
    fn test_new_defaults_port_when_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            server: ServerConfig {
                listen: "localhost:notaport".to_string(),
                document_root: dir.path().to_path_buf(),
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        let router = Router::new(&config, test_store(), None, None, None, None, None);
        assert_eq!(router.server_port, 8080);
    }

    // ── security default resolution ──────────────────────────────────

    #[test]
    fn test_open_basedir_defaults_on_when_sites_dir_set_without_section() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            server: ServerConfig {
                document_root: dir.path().to_path_buf(),
                sites_dir: Some(dir.path().to_path_buf()),
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        let router = Router::new(&config, test_store(), None, None, None, None, None);
        assert!(router.open_basedir, "multi-tenant mode must default open_basedir on");
    }

    #[test]
    fn test_open_basedir_defaults_off_without_section_or_sites_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            server: ServerConfig {
                document_root: dir.path().to_path_buf(),
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        let router = Router::new(&config, test_store(), None, None, None, None, None);
        assert!(!router.open_basedir);
    }

    // ── blocked paths ─────────────────────────────────────────────────

    #[test]
    fn test_blocked_exact_path() {
        let blocked = vec!["/wp-config.php".to_string()];
        assert!(is_path_blocked("/wp-config.php", &blocked));
        assert!(!is_path_blocked("/index.php", &blocked));
    }

    #[test]
    fn test_blocked_wildcard_directory() {
        let blocked = vec!["/vendor/*".to_string()];
        assert!(is_path_blocked("/vendor/autoload.php", &blocked));
        assert!(is_path_blocked("/vendor/anything", &blocked));
        assert!(!is_path_blocked("/index.php", &blocked));
    }

    #[test]
    fn test_blocked_extension_wildcard() {
        let blocked = vec!["/wp-content/uploads/*.php".to_string()];
        assert!(is_path_blocked("/wp-content/uploads/evil.php", &blocked));
        assert!(!is_path_blocked("/wp-content/uploads/photo.jpg", &blocked));
    }

    // ── allowed PHP paths ─────────────────────────────────────────────

    #[test]
    fn test_php_allowed_empty_allows_all() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            server: ServerConfig {
                document_root: dir.path().to_path_buf(),
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        let router = Router::new(&config, test_store(), None, None, None, None, None);
        assert!(router.is_php_allowed("/anything.php"));
    }

    #[test]
    fn test_php_allowed_exact_match() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config {
            server: ServerConfig {
                document_root: dir.path().to_path_buf(),
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        config.server.security.get_or_insert_default().allowed_php_paths =
            vec!["/index.php".to_string(), "/wp-login.php".to_string()];
        let router = Router::new(&config, test_store(), None, None, None, None, None);
        assert!(router.is_php_allowed("/index.php"));
        assert!(router.is_php_allowed("/wp-login.php"));
        assert!(!router.is_php_allowed("/evil.php"));
    }

    #[test]
    fn test_php_allowed_wildcard_directory() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config {
            server: ServerConfig {
                document_root: dir.path().to_path_buf(),
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        config.server.security.get_or_insert_default().allowed_php_paths =
            vec!["/index.php".to_string(), "/wp-admin/*.php".to_string()];
        let router = Router::new(&config, test_store(), None, None, None, None, None);
        assert!(router.is_php_allowed("/index.php"));
        assert!(router.is_php_allowed("/wp-admin/admin.php"));
        assert!(router.is_php_allowed("/wp-admin/options.php"));
        assert!(!router.is_php_allowed("/wp-content/uploads/shell.php"));
    }

    // ── glob matching ─────────────────────────────────────────────────

    #[test]
    fn test_glob_match_exact() {
        assert!(glob_match("/index.php", "/index.php"));
        assert!(!glob_match("/index.php", "/other.php"));
    }

    #[test]
    fn test_glob_match_star_segment() {
        assert!(glob_match("/wp-admin/*.php", "/wp-admin/admin.php"));
        assert!(!glob_match("/wp-admin/*.php", "/wp-admin/sub/deep.php"));
        assert!(!glob_match("/wp-admin/*.php", "/index.php"));
    }

    #[test]
    fn test_glob_match_star_catches_directory() {
        assert!(glob_match("/vendor/*", "/vendor/autoload.php"));
        assert!(glob_match("/vendor/*", "/vendor/anything"));
        // nested paths beyond the /* also match (/* means "directory and children")
        assert!(glob_match("/vendor/*", "/vendor/foo/bar"));
    }

    // ── ETag caching tests ──────────────────────────────────────────

    #[test]
    fn test_php_etag_cache_key_without_query() {
        let key = php_etag_cache_key("etag:", "GET", "/api/data", "");
        assert_eq!(key, "etag:GET:/api/data");
    }

    #[test]
    fn test_php_etag_cache_key_with_query() {
        let key = php_etag_cache_key("etag:", "POST", "/api/users", "id=42");
        assert_eq!(key, "etag:POST:/api/users?id=42");
    }

    #[test]
    fn test_etag_matches_value_exact() {
        assert!(etag_matches_value("W/\"abc123\"", "W/\"abc123\""));
        assert!(!etag_matches_value("W/\"abc123\"", "W/\"xyz789\""));
    }

    #[test]
    fn test_etag_matches_value_wildcard() {
        assert!(etag_matches_value("W/\"anything\"", "*"));
        assert!(etag_matches_value("W/\"123\"", "*"));
    }

    #[test]
    fn test_etag_matches_value_comma_separated() {
        assert!(etag_matches_value("W/\"v1\"", "W/\"v1\", W/\"v2\""));
        assert!(etag_matches_value("W/\"v2\"", "W/\"v1\", W/\"v2\""));
        assert!(!etag_matches_value("W/\"v3\"", "W/\"v1\", W/\"v2\""));
    }

    #[test]
    fn test_etag_matches_value_with_whitespace() {
        assert!(etag_matches_value("W/\"v1\"", "  W/\"v1\"  "));
        assert!(etag_matches_value("W/\"v1\"", "W/\"v1\" , W/\"v2\" "));
    }

    // ── is_compressible ─────────────────────────────────────────────

    #[test]
    fn is_compressible_text_types() {
        assert!(is_compressible("text/html"));
        assert!(is_compressible("text/css"));
        assert!(is_compressible("text/plain"));
        assert!(is_compressible("text/xml"));
    }

    #[test]
    fn is_compressible_application_types() {
        assert!(is_compressible("application/javascript"));
        assert!(is_compressible("application/json"));
        assert!(is_compressible("application/xml"));
        assert!(is_compressible("image/svg+xml"));
    }

    #[test]
    fn is_not_compressible_binary() {
        assert!(!is_compressible("image/png"));
        assert!(!is_compressible("image/jpeg"));
        assert!(!is_compressible("application/octet-stream"));
        assert!(!is_compressible("video/mp4"));
    }

    // ── segment_match edge cases ────────────────────────────────────

    #[test]
    fn segment_match_exact() {
        assert!(segment_match("index.php", "index.php"));
        assert!(!segment_match("index.php", "other.php"));
    }

    #[test]
    fn segment_match_star_matches_any() {
        assert!(segment_match("*", "anything"));
        assert!(segment_match("*", "index.php"));
    }

    #[test]
    fn segment_match_prefix_star() {
        assert!(segment_match("*.php", "index.php"));
        assert!(segment_match("*.php", "admin.php"));
        assert!(!segment_match("*.php", "index.html"));
    }

    #[test]
    fn segment_match_suffix_star() {
        assert!(segment_match("index*", "index.php"));
        assert!(segment_match("index*", "index.html"));
        assert!(!segment_match("index*", "other.php"));
    }

    #[test]
    fn segment_match_prefix_star_suffix() {
        assert!(segment_match("wp-*.php", "wp-admin.php"));
        assert!(segment_match("wp-*.php", "wp-login.php"));
        assert!(!segment_match("wp-*.php", "index.php"));
        assert!(!segment_match("wp-*.php", "wp-admin.html"));
    }

    // ── has_hidden_segment edge cases ───────────────────────────────

    #[test]
    fn has_hidden_segment_dot_only_not_hidden() {
        assert!(!has_hidden_segment("/./file.txt"));
        assert!(!has_hidden_segment("/../file.txt"));
    }

    #[test]
    fn has_hidden_segment_deep_nesting() {
        assert!(has_hidden_segment("/a/b/c/.secret/d"));
        assert!(!has_hidden_segment("/a/b/c/d/e"));
    }

    // ── per-vhost open_basedir ──────────────────────────────────────

    /// PHP splits `open_basedir` on the platform `PATH_SEPARATOR`. The value
    /// used to be `format!("{}:/tmp", document_root.display())`, so on
    /// Windows — where the separator is `;` and the docroot itself contains
    /// a drive-letter colon — a vhost got one bogus entry matching nothing
    /// and every file access was denied.
    #[test]
    fn vhost_open_basedir_uses_platform_separator_and_per_site_state_root() {
        let root = std::env::temp_dir().join("ephpm-basedir-test");
        let state_root = vhost_state_root(&root);
        let value = vhost_open_basedir_value(&root, &state_root);

        let separator = if cfg!(windows) { ';' } else { ':' };
        let parts: Vec<&str> = value.split(separator).collect();
        assert_eq!(parts.len(), 2, "expected exactly docroot + state root, got {value}");
        assert_eq!(
            parts[0],
            root.display().to_string(),
            "the document root must survive verbatim (drive-letter colon included)"
        );
        assert_eq!(
            parts[1],
            state_root.display().to_string(),
            "the second entry must be this vhost's private state root, not the shared temp dir"
        );
    }

    #[test]
    fn vhost_state_root_is_deterministic_and_per_site() {
        let base = std::env::temp_dir().join("ephpm-sites");
        let site_a = base.join("site-a.test");
        let site_b = base.join("site-b.test");

        // Deterministic: same document root → same state root across calls
        // (so a site's sessions/temp persist across restarts).
        assert_eq!(vhost_state_root(&site_a), vhost_state_root(&site_a));

        // Distinct sites get distinct state roots — the core of the fix. The
        // shared system temp dir must NOT be a prefix either would resolve to.
        let a = vhost_state_root(&site_a);
        let b = vhost_state_root(&site_b);
        assert_ne!(a, b, "two tenants must never share a private state root");
        assert_ne!(a, std::env::temp_dir(), "state root must not be the shared system temp dir");

        // Neither state root is contained in the other, so neither appears in
        // the other's open_basedir (which would re-open the cross-tenant hole).
        assert!(!a.starts_with(&b) && !b.starts_with(&a), "state roots must not nest");
    }

    #[test]
    fn vhost_state_root_leaf_names_that_collide_still_separate() {
        // Two sites under different parents sharing a leaf name must not map
        // to the same state root — the digest suffix disambiguates.
        let a = vhost_state_root(Path::new("/srv/tenant-a/public"));
        let b = vhost_state_root(Path::new("/srv/tenant-b/public"));
        assert_ne!(a, b);
    }

    #[test]
    fn sanitize_path_label_strips_separators_and_bounds_length() {
        assert_eq!(sanitize_path_label("blog.localhost"), "blog.localhost");
        assert_eq!(sanitize_path_label("a/b\\c:d"), "a_b_c_d");
        assert_eq!(sanitize_path_label(""), "site");
        assert_eq!(sanitize_path_label("..").len(), 2); // dots are allowed but the digest suffix keeps it unique+bounded
        assert_eq!(sanitize_path_label(&"x".repeat(200)).len(), 64);
    }

    #[test]
    fn ensure_vhost_private_dirs_creates_isolated_tmp_and_sessions() {
        let sites =
            std::env::temp_dir().join(format!("ephpm-evd-{}", std::process::id())).join("sites");
        let site_a = sites.join("site-a.test");
        let site_b = sites.join("site-b.test");
        std::fs::create_dir_all(&site_a).unwrap();
        std::fs::create_dir_all(&site_b).unwrap();

        let config = Config {
            server: ServerConfig {
                document_root: sites.clone(),
                sites_dir: Some(sites.clone()),
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        let router = Router::new(&config, test_store(), None, None, None, None, None);

        let a = router.ensure_vhost_private_dirs(&site_a);
        let b = router.ensure_vhost_private_dirs(&site_b);

        assert!(a.temp.is_dir(), "site-a temp dir must be created");
        assert!(a.sessions.is_dir(), "site-a sessions dir must be created");
        assert!(b.temp.is_dir() && b.sessions.is_dir());

        // The whole point: no overlap between tenants.
        assert_ne!(a.state_root, b.state_root);
        assert!(a.temp.starts_with(&a.state_root) && a.sessions.starts_with(&a.state_root));
        assert!(!a.temp.starts_with(&b.state_root), "site-a temp must be outside site-b's basedir");
        assert!(
            !a.sessions.starts_with(&b.state_root),
            "site-a sessions must be outside site-b's basedir"
        );

        // Idempotent + cached: a second call returns the same paths.
        let a2 = router.ensure_vhost_private_dirs(&site_a);
        assert_eq!(a.state_root, a2.state_root);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&a.sessions).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "session dir must be private (0700)");
        }

        let _ = std::fs::remove_dir_all(
            std::env::temp_dir().join(format!("ephpm-evd-{}", std::process::id())),
        );
    }

    // ── is_php_file edge cases ──────────────────────────────────────

    #[test]
    fn is_php_file_case_insensitive() {
        assert!(is_php_file(Path::new("test.PHP")));
        assert!(is_php_file(Path::new("test.Php")));
    }

    #[test]
    fn is_php_file_false_for_non_php() {
        assert!(!is_php_file(Path::new("test.html")));
        assert!(!is_php_file(Path::new("test.js")));
        assert!(!is_php_file(Path::new("no-extension")));
    }

    // ── gzip_compress edge cases ────────────────────────────────────

    #[test]
    fn gzip_compress_json() {
        let data = r#"{"key": "value", "list": [1,2,3]}"#.repeat(100);
        let compressed = gzip_compress(data.as_bytes(), "application/json", default_compression());
        assert!(compressed.is_some(), "JSON should be compressible");
        assert!(compressed.unwrap().len() < data.len());
    }

    #[test]
    fn gzip_compress_svg() {
        let data = r#"<svg xmlns="http://www.w3.org/2000/svg"><circle r="50"/></svg>"#.repeat(50);
        let compressed = gzip_compress(data.as_bytes(), "image/svg+xml", default_compression());
        assert!(compressed.is_some(), "SVG should be compressible");
    }

    #[test]
    fn gzip_compress_binary_not_compressed() {
        let data = vec![0x89, 0x50, 0x4e, 0x47]; // PNG header
        assert!(gzip_compress(&data, "image/png", default_compression()).is_none());
    }

    // ── etag_matches_value edge cases ───────────────────────────────

    #[test]
    fn etag_matches_empty_if_none_match() {
        assert!(!etag_matches_value("\"v1\"", ""));
    }

    #[test]
    fn etag_matches_strong_etag() {
        assert!(etag_matches_value("\"abc\"", "\"abc\""));
        assert!(!etag_matches_value("\"abc\"", "\"def\""));
    }

    // ── blocked paths edge cases ────────────────────────────────────

    #[test]
    fn blocked_empty_list_blocks_nothing() {
        let blocked: Vec<String> = vec![];
        assert!(!is_path_blocked("/anything", &blocked));
    }

    #[test]
    fn blocked_multiple_patterns() {
        let blocked =
            vec!["/wp-config.php".to_string(), "/vendor/*".to_string(), "/.env".to_string()];
        assert!(is_path_blocked("/wp-config.php", &blocked));
        assert!(is_path_blocked("/vendor/autoload.php", &blocked));
        assert!(is_path_blocked("/.env", &blocked));
        assert!(!is_path_blocked("/index.php", &blocked));
    }

    // ── port parsing edge cases ─────────────────────────────────────

    #[test]
    fn port_from_ipv6_listen_address() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            server: ServerConfig {
                listen: "[::]:9090".to_string(),
                document_root: dir.path().to_path_buf(),
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        let router = Router::new(&config, test_store(), None, None, None, None, None);
        assert_eq!(router.server_port, 9090);
    }

    // ── glob_match edge cases ───────────────────────────────────────

    #[test]
    fn glob_match_directory_prefix() {
        assert!(glob_match("/admin/", "/admin/settings"));
        assert!(!glob_match("/admin/", "/other/page"));
    }

    #[test]
    fn glob_match_no_wildcard_exact_only() {
        assert!(glob_match("/index.php", "/index.php"));
        assert!(!glob_match("/index.php", "/index.phps"));
        assert!(!glob_match("/index.php", "/index.ph"));
    }

    // ── ETag cache unit tests (no PHP required) ───────────────────────
    //
    // These tests verify the ETag cache logic in isolation — key
    // generation, store/retrieve, matching, and TTL behavior — without
    // needing a PHP runtime.

    #[test]
    fn etag_cache_key_without_query() {
        let key = php_etag_cache_key("etag:", "GET", "/index.php", "");
        assert_eq!(key, "etag:GET:/index.php");
    }

    #[test]
    fn etag_cache_key_with_query() {
        let key = php_etag_cache_key("etag:", "GET", "/api/data", "page=1&sort=name");
        assert_eq!(key, "etag:GET:/api/data?page=1&sort=name");
    }

    #[test]
    fn etag_cache_key_head_method() {
        let key = php_etag_cache_key("etag:", "HEAD", "/status", "");
        assert_eq!(key, "etag:HEAD:/status");
    }

    #[test]
    fn etag_cache_key_custom_prefix() {
        let key = php_etag_cache_key("cache:", "GET", "/page", "");
        assert_eq!(key, "cache:GET:/page");
    }

    #[test]
    fn etag_store_and_retrieve() {
        let store = test_store();
        let key = php_etag_cache_key("etag:", "GET", "/test.php", "");

        // Store an ETag.
        store.set(key.clone(), b"\"v1\"".to_vec(), None);

        // Retrieve it.
        let stored = store.get(&key);
        assert!(stored.is_some());
        assert_eq!(stored.unwrap().as_ref(), b"\"v1\"");
    }

    #[test]
    fn etag_store_overwrites_previous() {
        let store = test_store();
        let key = php_etag_cache_key("etag:", "GET", "/test.php", "");

        store.set(key.clone(), b"\"v1\"".to_vec(), None);
        store.set(key.clone(), b"\"v2\"".to_vec(), None);

        let stored = store.get(&key);
        assert_eq!(stored.unwrap().as_ref(), b"\"v2\"");
    }

    #[test]
    fn etag_matches_wildcard() {
        assert!(etag_matches_value("\"any\"", "*"));
    }

    #[test]
    fn etag_matches_comma_separated_list() {
        assert!(etag_matches_value("\"v2\"", "\"v1\", \"v2\", \"v3\""));
        assert!(!etag_matches_value("\"v4\"", "\"v1\", \"v2\", \"v3\""));
    }

    #[test]
    fn etag_matches_with_whitespace() {
        assert!(etag_matches_value("\"abc\"", "  \"abc\"  "));
        assert!(etag_matches_value("\"abc\"", "\"def\" , \"abc\""));
    }

    #[test]
    fn etag_no_match_different_values() {
        assert!(!etag_matches_value("\"abc\"", "\"xyz\""));
    }

    #[test]
    fn etag_cache_respects_ttl_zero_as_indefinite() {
        let store = test_store();
        let key = php_etag_cache_key("etag:", "GET", "/page", "");

        // TTL of None means indefinite storage.
        store.set(key.clone(), b"\"forever\"".to_vec(), None);

        // Should be retrievable.
        let stored = store.get(&key);
        assert_eq!(stored.unwrap().as_ref(), b"\"forever\"");
    }

    #[test]
    fn etag_cache_different_methods_different_keys() {
        let store = test_store();
        let get_key = php_etag_cache_key("etag:", "GET", "/page", "");
        let head_key = php_etag_cache_key("etag:", "HEAD", "/page", "");

        store.set(get_key.clone(), b"\"get-v1\"".to_vec(), None);
        store.set(head_key.clone(), b"\"head-v1\"".to_vec(), None);

        assert_eq!(store.get(&get_key).unwrap().as_ref(), b"\"get-v1\"");
        assert_eq!(store.get(&head_key).unwrap().as_ref(), b"\"head-v1\"");
    }

    #[test]
    fn etag_cache_different_paths_different_keys() {
        let store = test_store();
        let key_a = php_etag_cache_key("etag:", "GET", "/page-a", "");
        let key_b = php_etag_cache_key("etag:", "GET", "/page-b", "");

        store.set(key_a.clone(), b"\"a-v1\"".to_vec(), None);
        store.set(key_b.clone(), b"\"b-v1\"".to_vec(), None);

        assert_eq!(store.get(&key_a).unwrap().as_ref(), b"\"a-v1\"");
        assert_eq!(store.get(&key_b).unwrap().as_ref(), b"\"b-v1\"");
    }

    #[test]
    fn etag_cache_query_string_differentiates() {
        let store = test_store();
        let key_no_qs = php_etag_cache_key("etag:", "GET", "/api", "");
        let key_with_qs = php_etag_cache_key("etag:", "GET", "/api", "v=2");

        store.set(key_no_qs.clone(), b"\"no-qs\"".to_vec(), None);
        store.set(key_with_qs.clone(), b"\"with-qs\"".to_vec(), None);

        assert_eq!(store.get(&key_no_qs).unwrap().as_ref(), b"\"no-qs\"");
        assert_eq!(store.get(&key_with_qs).unwrap().as_ref(), b"\"with-qs\"");
    }

    #[test]
    fn etag_cache_304_logic_matches_stored() {
        let store = test_store();
        let key = php_etag_cache_key("etag:", "GET", "/index.php", "");
        store.set(key.clone(), b"\"cached-v1\"".to_vec(), None);

        // Simulate the cache lookup that happens in handle().
        let stored = store.get(&key);
        assert!(stored.is_some());
        let stored_bytes = stored.unwrap();
        let stored_etag = String::from_utf8_lossy(&stored_bytes);
        let client_tag = "\"cached-v1\"";

        // This should match → 304.
        assert!(etag_matches_value(&stored_etag, client_tag));
    }

    #[test]
    fn etag_cache_304_logic_no_match() {
        let store = test_store();
        let key = php_etag_cache_key("etag:", "GET", "/index.php", "");
        store.set(key.clone(), b"\"cached-v1\"".to_vec(), None);

        let stored_bytes = store.get(&key).unwrap();
        let stored_etag = String::from_utf8_lossy(&stored_bytes);
        let client_tag = "\"old-version\"";

        // Different ETag → should not match → execute PHP.
        assert!(!etag_matches_value(&stored_etag, client_tag));
    }

    #[test]
    fn etag_cache_miss_returns_none() {
        let store = test_store();
        let key = php_etag_cache_key("etag:", "GET", "/nonexistent.php", "");

        // No entry → cache miss → execute PHP.
        assert!(store.get(&key).is_none());
    }

    #[test]
    fn etag_cache_with_short_ttl() {
        use std::time::Duration;

        let store = test_store();
        let key = php_etag_cache_key("etag:", "GET", "/page.php", "");

        // Store with 1-second TTL.
        store.set(key.clone(), b"\"ttl-v1\"".to_vec(), Some(Duration::from_secs(1)));

        // Should be retrievable immediately.
        assert!(store.get(&key).is_some());
    }

    // ── PHP-linked ETag integration tests ────────────────────────────
    //
    // These tests require PHP to be linked. They verify that PHP-set
    // ETags are properly cached in the KV store and matched on
    // subsequent requests.
    //
    // Run with: cargo nextest run -p ephpm-server --run-ignored all

    #[allow(unexpected_cfgs)]
    #[cfg(all(test, php_linked))]
    mod php_etag_tests {
        use ephpm_php::PhpRuntime;
        // `Empty` lives in `http_body_util`, not `hyper::body` — this module
        // is `cfg(php_linked)`, so the wrong path went unnoticed until
        // `Router::handle` became generic over the body type and made
        // `Request<Empty<Bytes>>` a legal argument at all.
        use http_body_util::{BodyExt, Empty};
        use serial_test::serial;

        use super::*;

        /// Helper to read response body bytes
        async fn body_bytes(resp: Response<ServerBody>) -> Vec<u8> {
            resp.into_body().collect().await.unwrap().to_bytes().to_vec()
        }

        /// Helper to create a test request
        fn make_request(
            method: &str,
            path: &str,
            if_none_match: Option<&str>,
        ) -> Request<Empty<Bytes>> {
            let mut builder = Request::builder().method(method).uri(path);
            if let Some(tag) = if_none_match {
                builder = builder.header("if-none-match", tag);
            }
            builder.body(Empty::new()).unwrap()
        }

        #[tokio::test]
        #[serial]
        async fn php_etag_stored_on_first_request() {
            let dir = tempfile::tempdir().unwrap();
            let php_code = r#"<?php
header('ETag: "test-v1"');
echo "content here";
"#;
            fs::write(dir.path().join("index.php"), php_code).unwrap();

            let store = test_store();
            let router = test_router_with_store(dir.path(), Arc::clone(&store));

            let req = make_request("GET", "/index.php", None);
            let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
            let resp = router.handle(req, addr, false).await.unwrap();

            // Should be 200 with ETag header
            assert_eq!(resp.status(), StatusCode::OK);
            let etag = resp.headers().get("etag").and_then(|v| v.to_str().ok());
            assert_eq!(etag, Some("\"test-v1\""));

            // ETag should be stored in the KV store
            let key = php_etag_cache_key("etag:", "GET", "/index.php", "");
            let stored = store.get(&key);
            assert!(stored.is_some());
            assert_eq!(stored.unwrap(), b"\"test-v1\"");
        }

        #[tokio::test]
        #[serial]
        async fn php_etag_returns_304_on_match() {
            let dir = tempfile::tempdir().unwrap();
            let php_code = r#"<?php
header('ETag: "test-v2"');
// This should NOT execute on the second request
file_put_contents('/tmp/php_executed', 'yes');
echo "should not see this";
"#;
            fs::write(dir.path().join("index.php"), php_code).unwrap();

            let store = test_store();
            let router = test_router_with_store(dir.path(), Arc::clone(&store));

            // Pre-seed the store with an ETag
            let key = php_etag_cache_key("etag:", "GET", "/index.php", "");
            store.set(key, b"\"test-v2\"".to_vec(), None);

            // Make request with matching If-None-Match
            let req = make_request("GET", "/index.php", Some("\"test-v2\""));
            let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
            let resp = router.handle(req, addr, false).await.unwrap();

            // Should be 304 with no body
            assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
            let body = body_bytes(resp).await;
            assert!(body.is_empty());
        }

        #[tokio::test]
        #[serial]
        async fn php_etag_executes_php_on_mismatch() {
            let dir = tempfile::tempdir().unwrap();
            let php_code = r#"<?php
header('ETag: "new-version"');
echo "new content";
"#;
            fs::write(dir.path().join("index.php"), php_code).unwrap();

            let store = test_store();
            let router = test_router_with_store(dir.path(), Arc::clone(&store));

            // Pre-seed the store with a different ETag
            let key = php_etag_cache_key("etag:", "GET", "/index.php", "");
            store.set(key.clone(), b"\"old-version\"".to_vec(), None);

            // Make request with different If-None-Match
            let req = make_request("GET", "/index.php", Some("\"old-version\""));
            let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
            let resp = router.handle(req, addr, false).await.unwrap();

            // Should be 200 with new ETag
            assert_eq!(resp.status(), StatusCode::OK);
            let etag = resp.headers().get("etag").and_then(|v| v.to_str().ok());
            assert_eq!(etag, Some("\"new-version\""));

            // Store should be updated
            let stored = store.get(&key);
            assert_eq!(stored.unwrap(), b"\"new-version\"");
        }

        #[tokio::test]
        #[serial]
        async fn php_no_etag_header_not_stored() {
            let dir = tempfile::tempdir().unwrap();
            let php_code = r#"<?php
// No ETag header
echo "no etag";
"#;
            fs::write(dir.path().join("index.php"), php_code).unwrap();

            let store = test_store();
            let router = test_router_with_store(dir.path(), Arc::clone(&store));

            let req = make_request("GET", "/index.php", None);
            let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
            let resp = router.handle(req, addr, false).await.unwrap();

            // Should be 200 with no ETag header
            assert_eq!(resp.status(), StatusCode::OK);
            assert!(resp.headers().get("etag").is_none());

            // KV store should not have an entry for this path
            let key = php_etag_cache_key("etag:", "GET", "/index.php", "");
            assert!(store.get(&key).is_none());
        }

        #[tokio::test]
        #[serial]
        async fn php_etag_not_cached_for_post() {
            let dir = tempfile::tempdir().unwrap();
            let php_code = r#"<?php
header('ETag: "post-etag"');
echo "post response";
"#;
            fs::write(dir.path().join("index.php"), php_code).unwrap();

            let store = test_store();
            let router = test_router_with_store(dir.path(), Arc::clone(&store));

            let req = make_request("POST", "/index.php", None);
            let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
            let resp = router.handle(req, addr, false).await.unwrap();

            // POST should execute normally and return 200
            assert_eq!(resp.status(), StatusCode::OK);

            // POST responses should NOT be cached in KV store (only GET/HEAD)
            let key = php_etag_cache_key("etag:", "POST", "/index.php", "");
            assert!(store.get(&key).is_none());
        }
    }

    // ── virtual host resolution ──────────────────────────────────────

    #[test]
    fn vhost_resolves_to_site_directory() {
        let dir = tempfile::tempdir().unwrap();
        let sites = dir.path().join("sites");
        let site_dir = sites.join("example.com");
        fs::create_dir_all(&site_dir).unwrap();
        fs::write(site_dir.join("index.html"), "<html>hi</html>").unwrap();

        let config = Config {
            server: ServerConfig {
                listen: "0.0.0.0:8080".to_string(),
                document_root: dir.path().to_path_buf(),
                sites_dir: Some(sites),
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        let router = Router::new(&config, test_store(), None, None, None, None, None);

        let doc_root = router.resolve_site("example.com").roots.document_root;
        assert_eq!(doc_root, site_dir);
    }

    #[test]
    fn vhost_fallback_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let sites = dir.path().join("sites");
        fs::create_dir_all(&sites).unwrap();

        let config = Config {
            server: ServerConfig {
                listen: "0.0.0.0:8080".to_string(),
                document_root: dir.path().to_path_buf(),
                sites_dir: Some(sites),
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        let router = Router::new(&config, test_store(), None, None, None, None, None);

        let doc_root = router.resolve_site("unknown.com").roots.document_root;
        assert_eq!(doc_root, dir.path());
    }

    #[test]
    fn vhost_strips_port() {
        let dir = tempfile::tempdir().unwrap();
        let sites = dir.path().join("sites");
        let site_dir = sites.join("example.com");
        fs::create_dir_all(&site_dir).unwrap();

        let config = Config {
            server: ServerConfig {
                listen: "0.0.0.0:8080".to_string(),
                document_root: dir.path().to_path_buf(),
                sites_dir: Some(sites),
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        let router = Router::new(&config, test_store(), None, None, None, None, None);

        let doc_root = router.resolve_site("example.com:8080").roots.document_root;
        assert_eq!(doc_root, site_dir);
    }

    #[test]
    fn vhost_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let sites = dir.path().join("sites");
        let site_dir = sites.join("example.com");
        fs::create_dir_all(&site_dir).unwrap();

        let config = Config {
            server: ServerConfig {
                listen: "0.0.0.0:8080".to_string(),
                document_root: dir.path().to_path_buf(),
                sites_dir: Some(sites),
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        let router = Router::new(&config, test_store(), None, None, None, None, None);

        let doc_root = router.resolve_site("Example.COM").roots.document_root;
        assert_eq!(doc_root, site_dir);
    }

    #[test]
    fn vhost_empty_sites_dir_uses_default() {
        let dir = tempfile::tempdir().unwrap();

        let config = Config {
            server: ServerConfig {
                listen: "0.0.0.0:8080".to_string(),
                document_root: dir.path().to_path_buf(),
                sites_dir: None,
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        let router = Router::new(&config, test_store(), None, None, None, None, None);

        let doc_root = router.resolve_site("anything.com").roots.document_root;
        assert_eq!(doc_root, dir.path());
    }

    #[test]
    fn vhost_fallback_resolves_files_from_site_root() {
        let dir = tempfile::tempdir().unwrap();
        let sites = dir.path().join("sites");
        let site_dir = sites.join("myblog.com");
        fs::create_dir_all(&site_dir).unwrap();
        fs::write(site_dir.join("index.php"), "<?php echo 'hi';").unwrap();

        let config = Config {
            server: ServerConfig {
                listen: "0.0.0.0:8080".to_string(),
                document_root: dir.path().to_path_buf(),
                sites_dir: Some(sites),
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        let router = Router::new(&config, test_store(), None, None, None, None, None);

        let ResolvedSite { roots, index_files, fallback, .. } = router.resolve_site("myblog.com");
        let doc_root = roots.document_root;
        let resolved = router.resolve_fallback("/", "", &doc_root, index_files, fallback);
        assert!(
            matches!(resolved, Resolved::File(p) if p == site_dir.join("index.php")),
            "fallback should resolve index.php from site directory"
        );
    }

    #[test]
    fn vhost_lazy_discovery_finds_new_directory() {
        let dir = tempfile::tempdir().unwrap();
        let sites = dir.path().join("sites");
        fs::create_dir_all(&sites).unwrap();

        // Create router with empty sites_dir — no sites at startup.
        let config = Config {
            server: ServerConfig {
                listen: "0.0.0.0:8080".to_string(),
                document_root: dir.path().to_path_buf(),
                sites_dir: Some(sites.clone()),
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        let router = Router::new(&config, test_store(), None, None, None, None, None);

        // Host doesn't exist yet — should fall back to default.
        let doc_root = router.resolve_site("new-site.com").roots.document_root;
        assert_eq!(doc_root, dir.path());

        // Create the directory AFTER router startup (simulates switchboard deploying).
        let new_site = sites.join("new-site.com");
        fs::create_dir_all(&new_site).unwrap();
        fs::write(new_site.join("index.html"), "<html>live!</html>").unwrap();

        // A negative-lookup cache entry with an `UNKNOWN_SITE_TTL`
        // window would keep serving the fall-back for up to a
        // minute; drop it directly here so the test stays fast
        // without weakening the TTL for prod. Bot probes are the
        // reason the cache exists; a legitimate switchboard deploy
        // in prod sees the site come alive on the next miss after
        // the TTL elapses.
        router.unknown_site_cache.clear();

        // Now it should be discovered lazily.
        let doc_root = router.resolve_site("new-site.com").roots.document_root;
        assert_eq!(doc_root, new_site);
    }

    #[test]
    fn vhost_lazy_discovery_teardown() {
        let dir = tempfile::tempdir().unwrap();
        let sites = dir.path().join("sites");
        let site_dir = sites.join("temp-site.com");
        fs::create_dir_all(&site_dir).unwrap();

        let config = Config {
            server: ServerConfig {
                listen: "0.0.0.0:8080".to_string(),
                document_root: dir.path().to_path_buf(),
                sites_dir: Some(sites),
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        let router = Router::new(&config, test_store(), None, None, None, None, None);

        // Site exists — should resolve.
        let doc_root = router.resolve_site("temp-site.com").roots.document_root;
        assert_eq!(doc_root, site_dir);

        // Delete the directory (simulates switchboard tearing down).
        fs::remove_dir_all(&site_dir).unwrap();

        // Should fall back to default now.
        let doc_root = router.resolve_site("temp-site.com").roots.document_root;
        assert_eq!(doc_root, dir.path());
    }

    // ── Per-site overrides (`[server] site_overrides_dir`) ─────────

    /// A multi-tenant fleet: `sites/` for tenant checkouts and `overrides/`
    /// **beside** it (never inside), which is the placement the whole design
    /// rests on.
    struct Fleet {
        dir: tempfile::TempDir,
        sites: PathBuf,
        overrides: PathBuf,
    }

    fn fleet() -> Fleet {
        let dir = tempfile::tempdir().unwrap();
        let sites = dir.path().join("sites");
        let overrides = dir.path().join("overrides");
        fs::create_dir_all(&sites).unwrap();
        fs::create_dir_all(&overrides).unwrap();
        Fleet { dir, sites, overrides }
    }

    impl Fleet {
        /// Create a vhost directory with the given subdirectories.
        fn site(&self, key: &str, subdirs: &[&str]) -> PathBuf {
            let container = self.sites.join(key);
            fs::create_dir_all(&container).unwrap();
            for sub in subdirs {
                fs::create_dir_all(container.join(sub)).unwrap();
            }
            container
        }

        /// Write the operator-owned override for `key`.
        fn override_for(&self, key: &str, text: &str) {
            fs::write(self.overrides.join(format!("{key}.toml")), text).unwrap();
        }

        fn remove_override(&self, key: &str) {
            fs::remove_file(self.overrides.join(format!("{key}.toml"))).unwrap();
        }

        /// Router with overrides enabled.
        fn router(&self) -> Router {
            self.build(Some(self.overrides.clone()))
        }

        /// Router with the mechanism switched off.
        fn router_without_overrides(&self) -> Router {
            self.build(None)
        }

        fn build(&self, overrides_dir: Option<PathBuf>) -> Router {
            let config = Config {
                server: ServerConfig {
                    listen: "0.0.0.0:8080".to_string(),
                    document_root: self.dir.path().to_path_buf(),
                    sites_dir: Some(self.sites.clone()),
                    site_overrides_dir: overrides_dir,
                    ..ServerConfig::default()
                },
                php: PhpConfig::default(),
                db: DbConfig::default(),
                kv: KvConfig::default(),
                cluster: ClusterConfig::default(),
                middleware: Vec::new(),
                opcache: ephpm_config::OpcacheConfig::default(),
            };
            Router::new(&config, test_store(), None, None, None, None, None)
        }
    }

    /// A site whose override declares `document_root = "web"` serves from it —
    /// and its container is still the vhost directory.
    #[test]
    fn site_override_moves_document_root_and_keeps_container() {
        let f = fleet();
        let site = f.site("laravel.test", &["web", "vendor"]);
        f.override_for("laravel.test", "document_root = \"web\"\n");

        let router = f.router();
        let resolved = router.resolve_site("laravel.test");

        assert_eq!(
            resolved.roots.document_root,
            site.join("web").canonicalize().map(ephpm_config::strip_verbatim_prefix).unwrap(),
            "web root must be the declared subdirectory"
        );
        assert_eq!(
            resolved.roots.container, site,
            "the container must stay the vhost directory — it is the open_basedir boundary"
        );
        assert_eq!(resolved.key.as_deref(), Some("laravel.test"));
    }

    /// THE invariant: `open_basedir` follows the container, so PHP served from
    /// the overridden root can still `require '../vendor/autoload.php'`. If this
    /// regresses, every framework vhost fails on its front controller's first
    /// statement.
    #[test]
    fn site_override_open_basedir_stays_the_container() {
        let f = fleet();
        let site = f.site("laravel.test", &["web", "vendor"]);
        f.override_for("laravel.test", "document_root = \"web\"\n");

        let roots = f.router().resolve_site("laravel.test").roots;

        // Exactly the derivation `handle_php` performs.
        let state_root = vhost_state_root(&roots.container);
        let basedir = vhost_open_basedir_value(&roots.container, &state_root);

        let separator = if cfg!(windows) { ';' } else { ':' };
        let entries: Vec<&str> = basedir.split(separator).collect();
        assert_eq!(entries[0], site.display().to_string(), "basedir entry must be the container");
        assert_ne!(
            entries[0],
            roots.document_root.display().to_string(),
            "basedir must NOT follow document_root into the overridden web root"
        );
        // vendor/ lives above the web root and must be inside the sandbox.
        assert!(site.join("vendor").starts_with(entries[0]));
    }

    /// An override may NARROW what is served; it can never WIDEN what PHP may
    /// read. Whatever the file says — including keys naming the sandbox itself —
    /// the `open_basedir` entry is the container ePHPm chose, byte for byte.
    #[test]
    fn site_override_cannot_widen_open_basedir() {
        let f = fleet();
        let site = f.site("hostile.test", &["web"]);

        for declaration in [
            "document_root = \"web\"\n",
            "document_root = \"/\"\n",
            "document_root = \"../../\"\n",
            "open_basedir = \"/\"\n",
            "container = \"/\"\n",
            "",
        ] {
            f.override_for("hostile.test", declaration);
            let roots = f.router().resolve_site("hostile.test").roots;
            assert_eq!(
                roots.container, site,
                "no override may change the container: {declaration:?}"
            );
            let basedir =
                vhost_open_basedir_value(&roots.container, &vhost_state_root(&roots.container));
            assert!(
                basedir.starts_with(&site.display().to_string()),
                "open_basedir must stay the container for {declaration:?}, got {basedir}"
            );
        }
    }

    /// Create a directory symlink, or return `false` when the platform refuses
    /// (Windows without the symlink privilege). Same skip-not-fail contract as
    /// the `site_overrides` suite: a host that cannot create a symlink cannot
    /// mount this attack either.
    fn try_symlink_dir(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(target, link).is_ok()
        }
    }

    /// Remove a directory symlink cross-platform: on Windows a directory
    /// symlink is unlinked with `remove_dir`, on Unix a symlink with
    /// `remove_file`.
    fn unlink_dir_symlink(link: &Path) {
        #[cfg(unix)]
        {
            std::fs::remove_file(link).unwrap();
        }
        #[cfg(windows)]
        {
            std::fs::remove_dir(link).unwrap();
        }
    }

    /// #394, the TOCTOU: `validate_declared_root` checks containment once, at
    /// load, but the resolved `document_root` is dereferenced live. A tenant
    /// deploys a valid `web/` (passes validation, seeds `site_roots`), then
    /// post-boot does `rmdir web; symlink('/', 'web')`. Because the **static
    /// path has no `open_basedir` backstop**, the only thing between the client
    /// and the escaped path is the root [`Router::contained_canonical_root`]
    /// returns — which must re-assert containment against the container on
    /// every resolve, not trust the load-time verdict.
    ///
    /// Note the ordering: the site is resolved (seeding `site_roots` with a
    /// valid resolution) *before* the swap, and `canonical_root` is never warmed
    /// on the pre-swap `web`, so the first resolve after the swap is exactly the
    /// escape window the pentester hit.
    #[test]
    fn site_override_symlink_swap_after_validation_is_refused() {
        let f = fleet();
        let container = f.site("attacker.test", &["web"]);
        f.override_for("attacker.test", "document_root = \"web\"\n");

        // A secret directory outside the site container — a stand-in for another
        // tenant's directory or the filesystem root.
        let outside = f.dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"CROSS-TENANT").unwrap();

        let router = f.router();
        // Resolve once with a valid `web` — this seeds `site_roots` exactly as
        // `seed_site_roots()` does at boot, without touching `canonical_roots`.
        let roots = router.resolve_site("attacker.test").roots;
        assert!(roots.declared(), "precondition: the override moved the web root");

        // The tenant swaps its validated `web` for a symlink escaping the
        // container (post-validation, from inside its own open_basedir).
        let web = container.join("web");
        std::fs::remove_dir_all(&web).unwrap();
        if !try_symlink_dir(&outside, &web) {
            return; // platform refuses symlinks
        }

        // The escape is real: a bare `canonicalize()` of the (unchanged)
        // document_root path now follows the symlink out of the container. This
        // is the value the pre-fix static path handed to `serve_file` as its
        // only boundary.
        assert_eq!(
            router.canonical_root(&roots.document_root),
            outside.canonicalize().ok(),
            "canonicalize must follow the swapped symlink out — the escape exists"
        );

        // The fix: containment is re-asserted against the container, so the
        // escaped root is refused (caller → 404/403), not served. Twice, to
        // prove a cached escaped `canonical_root` is still refused.
        assert_eq!(
            router.contained_canonical_root(&roots),
            None,
            "an escaped document root must be refused on every resolve (#394)"
        );
        assert_eq!(
            router.contained_canonical_root(&roots),
            None,
            "a cached escaped canonical_root must stay refused"
        );

        // And the PHP path is closed too (defense in depth; open_basedir only
        // partially contains it).
        std::fs::write(outside.join("index.php"), b"<?php echo 'pwned';").unwrap();
        assert!(
            !router.php_script_contained(&web.join("index.php"), &roots),
            "a PHP script reached through the escaped root must not execute"
        );
    }

    /// The mechanism self-heals and re-arms without ever serving the escape.
    /// Whether the swap is caught at load (`validate_declared_root`, when the
    /// symlink is present at resolve time) or at serve time
    /// ([`Router::contained_canonical_root`], the TOCTOU window), the resolved
    /// root is **never** the escaped `outside` directory; and restoring a real
    /// `web/` serves from it again. Fresh routers per phase so the outcome is a
    /// pure function of current filesystem state, not any cache TTL window.
    #[test]
    fn site_override_symlink_present_at_resolve_serves_container_not_escape() {
        let f = fleet();
        let container = f.site("app.test", &["web"]);
        f.override_for("app.test", "document_root = \"web\"\n");
        let web = container.join("web");
        let outside = f.dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();

        let canonical_container = container.canonicalize().unwrap();
        let canonical_outside = outside.canonicalize().unwrap();
        let canonical_web = web.canonicalize().unwrap();

        // Phase 1: legitimate real web → served from web.
        let r1 = f.router();
        let roots1 = r1.resolve_site("app.test").roots;
        assert_eq!(r1.contained_canonical_root(&roots1), Some(canonical_web.clone()));

        // Phase 2: the symlink is present when the site is (re-)resolved, so
        // the override is rejected at load and the site serves its CONTAINER —
        // never the escape. (The TOCTOU window where site_roots is already
        // cached valid is covered by the test above.)
        std::fs::remove_dir_all(&web).unwrap();
        if !try_symlink_dir(&outside, &web) {
            return;
        }
        let r2 = f.router();
        let roots2 = r2.resolve_site("app.test").roots;
        assert!(!roots2.declared(), "an escaping override must be rejected at load");
        let resolved2 = r2.contained_canonical_root(&roots2);
        assert_eq!(resolved2, Some(canonical_container), "must fall back to the container");
        assert_ne!(
            resolved2,
            Some(canonical_outside),
            "the escaped directory must never be the served root"
        );

        // Phase 3: restore a real web → served from web again (re-armed).
        unlink_dir_symlink(&web);
        std::fs::create_dir_all(&web).unwrap();
        let r3 = f.router();
        let roots3 = r3.resolve_site("app.test").roots;
        assert_eq!(r3.contained_canonical_root(&roots3), Some(canonical_web));
    }

    /// The per-vhost temp/session state root is derived from the container, so a
    /// site that gains an override keeps its existing sessions and uploads
    /// instead of silently orphaning them.
    #[test]
    fn site_override_does_not_move_the_vhost_state_root() {
        let f = fleet();
        f.site("app.test", &["web"]);

        let before = vhost_state_root(&f.router().resolve_site("app.test").roots.container);
        f.override_for("app.test", "document_root = \"web\"\n");
        let after = vhost_state_root(&f.router().resolve_site("app.test").roots.container);

        assert_eq!(before, after, "state root must follow the tenant, not its web root");
    }

    /// A vhost with no override — every site that predates this mechanism — is
    /// untouched: container and web root are the same directory.
    #[test]
    fn site_without_override_is_unchanged() {
        let f = fleet();
        let site = f.site("wordpress.test", &["wp-content"]);

        let roots = f.router().resolve_site("wordpress.test").roots;

        assert_eq!(roots.document_root, site);
        assert_eq!(roots.container, site);
        assert!(!roots.declared());
    }

    /// Mixed fleet, one router, one process: each site's own override decides.
    #[test]
    fn site_override_is_decided_per_site() {
        let f = fleet();
        let laravel = f.site("laravel.test", &["public"]);
        let wordpress = f.site("wordpress.test", &[]);
        f.override_for("laravel.test", "document_root = \"public\"\n");

        let router = f.router();
        assert_eq!(
            router.resolve_site("laravel.test").roots.document_root,
            laravel.join("public").canonicalize().map(ephpm_config::strip_verbatim_prefix).unwrap()
        );
        assert_eq!(router.resolve_site("wordpress.test").roots.document_root, wordpress);
    }

    /// Any directory name works — `web/`, `htdocs/`, `public_html/` — which is
    /// the point of a declaration over a fixed convention.
    #[test]
    fn site_override_accepts_any_declared_directory_name() {
        for name in ["web", "htdocs", "public_html", "www"] {
            let f = fleet();
            let site = f.site("app.test", &[name]);
            f.override_for("app.test", &format!("document_root = {name:?}\n"));

            assert_eq!(
                f.router().resolve_site("app.test").roots.document_root,
                site.join(name).canonicalize().map(ephpm_config::strip_verbatim_prefix).unwrap(),
                "declared {name} must be honoured"
            );
        }
    }

    /// The override must apply on the LAZY path too. A PR-preview host creates
    /// site directories while the server is running; they never go through
    /// `scan_sites_dir`. Missing this would present as "works after a restart,
    /// ignores the override when the preview is created".
    #[test]
    fn site_override_applies_to_lazy_discovery() {
        let f = fleet();

        // Router starts with an EMPTY sites_dir — nothing scanned.
        let router = f.router();
        assert_eq!(router.resolve_site("pr-42.preview.test").roots.document_root, f.dir.path());

        // Preview provisioned — checkout and override — while the server runs.
        let site = f.site("pr-42.preview.test", &["web"]);
        f.override_for("pr-42.preview.test", "document_root = \"web\"\n");
        router.unknown_site_cache.clear();

        let roots = router.resolve_site("pr-42.preview.test").roots;
        assert_eq!(
            roots.document_root,
            site.join("web").canonicalize().map(ephpm_config::strip_verbatim_prefix).unwrap(),
            "lazy discovery must read the override too"
        );
        assert_eq!(roots.container, site);
    }

    /// An override written for an ALREADY-DISCOVERED site takes effect without a
    /// restart, once `SITE_CONFIG_TTL` has elapsed. The registry path is the one
    /// that would otherwise be pinned to whatever was true at startup.
    #[test]
    fn site_override_change_is_picked_up_without_restart() {
        let f = fleet();
        let site = f.site("app.test", &["web"]);

        // Scanned at startup with NO override.
        let router = f.router();
        assert_eq!(router.resolve_site("app.test").roots.document_root, site);

        // Daemon writes the override while the server runs. Expire the cache
        // rather than sleeping for the TTL — the point under test is that the
        // registry path re-resolves, not how long two seconds is.
        f.override_for("app.test", "document_root = \"web\"\n");
        router.site_roots_cache.clear();

        assert_eq!(
            router.resolve_site("app.test").roots.document_root,
            site.join("web").canonicalize().map(ephpm_config::strip_verbatim_prefix).unwrap(),
            "an already-discovered site must pick up a new override"
        );

        // And removing it reverts, likewise without a restart.
        f.remove_override("app.test");
        router.site_roots_cache.clear();
        assert_eq!(router.resolve_site("app.test").roots.document_root, site);
    }

    /// Within the TTL the previous (already validated) resolution is reused —
    /// the override file is not read per request.
    #[test]
    fn site_override_resolution_is_cached_within_the_ttl() {
        let f = fleet();
        let site = f.site("app.test", &["web"]);
        f.override_for("app.test", "document_root = \"web\"\n");

        let router = f.router();
        let declared =
            site.join("web").canonicalize().map(ephpm_config::strip_verbatim_prefix).unwrap();
        assert_eq!(router.resolve_site("app.test").roots.document_root, declared);

        // Remove the override but do NOT expire the cache: the previous
        // resolution stands until the window closes.
        f.remove_override("app.test");
        assert_eq!(
            router.resolve_site("app.test").roots.document_root,
            declared,
            "within SITE_CONFIG_TTL the cached resolution is reused"
        );
    }

    /// `site_overrides_dir` unset switches the mechanism off entirely.
    #[test]
    fn site_override_disabled_serves_the_container() {
        let f = fleet();
        let site = f.site("laravel.test", &["web"]);
        f.override_for("laravel.test", "document_root = \"web\"\n");

        let roots = f.router_without_overrides().resolve_site("laravel.test").roots;

        assert_eq!(roots.document_root, site, "disabled: the container is the web root");
        assert_eq!(roots.container, site);
    }

    /// Bad declarations are rejected at the router boundary, not just in
    /// `site_overrides`' own tests — the site keeps serving its container.
    /// The writer is trusted, but ePHPm does not take that on faith.
    #[test]
    fn site_override_bad_declarations_serve_the_container() {
        let f = fleet();
        let site = f.site("evil.test", &[]);

        for bad in ["../../etc", "..", "/etc", "/", r"C:\Windows", "nope", "web/../.."] {
            f.override_for("evil.test", &format!("document_root = {bad:?}\n"));
            assert_eq!(
                f.router().resolve_site("evil.test").roots.document_root,
                site,
                "bad declaration {bad:?} must fall back to the container"
            );
        }
    }

    /// A half-written or malformed override must not break the site — a daemon
    /// interrupted mid-write is a real state.
    #[test]
    fn site_override_malformed_serves_the_container() {
        let f = fleet();
        let site = f.site("broken.test", &["web"]);

        for text in ["document_root =\n", "[[[\n", "not toml at all: {{{\n"] {
            f.override_for("broken.test", text);
            assert_eq!(f.router().resolve_site("broken.test").roots.document_root, site);
        }
    }

    /// The override filename MUST be the canonical site key. A daemon writing
    /// `preview-1234.toml` while the vhost is `pr-42.preview.test` fails open —
    /// the site serves its container — which is safe but silent, and is exactly
    /// why the docs spell the naming requirement out.
    #[test]
    fn site_override_named_for_another_key_is_ignored() {
        let f = fleet();
        let site = f.site("pr-42.preview.test", &["web"]);
        f.override_for("preview-1234", "document_root = \"web\"\n");

        assert_eq!(f.router().resolve_site("pr-42.preview.test").roots.document_root, site);
    }

    /// One tenant's override cannot move another tenant's web root, even though
    /// both files live in one directory: the filename is the tenant identity.
    #[test]
    fn site_override_cannot_affect_another_tenant() {
        let f = fleet();
        let a = f.site("a.test", &["web"]);
        let b = f.site("b.test", &["web"]);
        f.override_for("a.test", "document_root = \"web\"\n");

        let router = f.router();
        assert_eq!(
            router.resolve_site("a.test").roots.document_root,
            a.join("web").canonicalize().map(ephpm_config::strip_verbatim_prefix).unwrap()
        );
        assert_eq!(router.resolve_site("b.test").roots.document_root, b, "b is untouched");
    }

    /// A host that matched no vhost is not affected: it serves the default
    /// document root, which is a web root with no container above it, and no
    /// override is consulted for it.
    #[test]
    fn unmatched_host_is_unaffected_by_site_overrides() {
        let f = fleet();
        fs::create_dir_all(f.dir.path().join("web")).unwrap();
        // An override named for the unmatched host must not be read — an
        // unmatched host is not a tenant and has no site key (issue #291).
        f.override_for("nobody.test", "document_root = \"web\"\n");

        let roots = f.router().resolve_site("nobody.test").roots;

        assert_eq!(roots.document_root, f.dir.path());
        assert_eq!(roots.container, f.dir.path());
    }

    // ── Host-header path traversal (issue #275) ────────────────────

    #[test]
    fn site_key_normalization_strips_port_dot_and_case() {
        assert_eq!(normalize_host_key("Example.COM:8080"), "example.com");
        assert_eq!(normalize_host_key("blog.localhost."), "blog.localhost");
        assert_eq!(normalize_host_key("HOST"), "host");
        assert_eq!(normalize_host_key(""), "");
    }

    #[test]
    fn valid_site_keys_are_accepted() {
        for key in [
            "example.com",
            "site-a.test",
            "blog",
            "pr-1234.preview.example.com",
            "a_b.internal",
            "123.example",
            "xn--80ak6aa92e.com", // punycode (already ascii-lowercased)
        ] {
            assert!(is_valid_site_key(key), "expected `{key}` to be a valid site key");
        }
    }

    #[test]
    fn malicious_site_keys_are_rejected() {
        // These are the values AFTER `normalize_host_key`. The raw Host
        // headers that produce them are the C2 exploit payloads and their
        // variants.
        for key in [
            "",                   // empty Host / bare `..` after trailing-dot strip
            "..",                 // parent dir
            "../single",          // sibling docroot (arbitrary-docroot RCE)
            "../../../../../etc", // deep traversal → /etc
            "..\\..\\windows",    // backslash separator
            "a/b",                // embedded slash
            "/etc",               // absolute
            ".hidden",            // leading dot / empty first label
            "a..b",               // embedded double-dot
            "site.",              // trailing dot survives only if not stripped
            "a b.com",            // space
            "sub.%2e%2e",         // percent (never decoded into a key)
            "host\0name",         // NUL
            "café.com",           // non-ascii
            "UPPER.com",          // uppercase (must be normalized first)
        ] {
            assert!(!is_valid_site_key(key), "expected `{key:?}` to be rejected");
        }
    }

    #[test]
    fn resolve_site_never_escapes_sites_dir_via_traversal_host() {
        let dir = tempfile::tempdir().unwrap();
        let sites = dir.path().join("sites");
        fs::create_dir_all(sites.join("site-a.test")).unwrap();
        // A secret directory OUTSIDE sites_dir, reachable by `../secret`.
        let secret = dir.path().join("secret");
        fs::create_dir_all(&secret).unwrap();
        fs::write(secret.join("passwd"), "root:x:0:0").unwrap();

        let config = Config {
            server: ServerConfig {
                listen: "0.0.0.0:8080".to_string(),
                document_root: dir.path().to_path_buf(),
                sites_dir: Some(sites.clone()),
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        let router = Router::new(&config, test_store(), None, None, None, None, None);

        // A legitimate vhost still resolves.
        let good = router.resolve_site("site-a.test").roots.document_root;
        assert_eq!(good, sites.join("site-a.test"));

        // Every traversal host resolves to the DEFAULT document root, never to
        // the escaped `../secret` directory (which really exists on disk).
        for host in ["../secret", "../../secret", "..", "/etc", "..\\secret"] {
            let doc_root = router.resolve_site(host).roots.document_root;
            assert_eq!(
                doc_root,
                dir.path(),
                "traversal host `{host}` must fall back to default doc root, not escape"
            );
            assert_ne!(doc_root, secret, "traversal host `{host}` escaped sites_dir");
        }
    }

    /// Issue #397 (F2, released in v0.7.3). A `sites_domain_suffix` without a
    /// leading dot lets `Host: <suffix>` strip to the empty string; the empty
    /// key must NOT widen the document root (and hence `open_basedir`) to the
    /// whole `sites_dir` fleet. Before the fix `resolve_site` validated only the
    /// pre-strip host, pushed the empty stripped key into the lookup set, and
    /// `sites_dir.join("")` resolved to `sites_dir` itself — cross-tenant read
    /// and write from any tenant's PHP. The stripped key is now re-validated, so
    /// an empty (or otherwise invalid) stripped candidate is dropped and the
    /// request falls through to `default_site()`.
    ///
    /// This test builds the `Router` directly with the misconfigured suffix
    /// (bypassing `Config::validate`, which is the *second* layer of defense and
    /// now rejects a dotless suffix at load) to prove the routing layer fails
    /// closed on its own even if a dotless suffix were ever reached.
    #[test]
    fn dotless_suffix_empty_stripped_key_does_not_widen_open_basedir() {
        let dir = tempfile::tempdir().unwrap();
        let sites = dir.path().join("sites");
        // Two tenants side by side under sites_dir.
        fs::create_dir_all(sites.join("alpha")).unwrap();
        fs::create_dir_all(sites.join("bravo")).unwrap();

        let config = Config {
            server: ServerConfig {
                listen: "0.0.0.0:8080".to_string(),
                document_root: dir.path().to_path_buf(),
                sites_dir: Some(sites.clone()),
                // The trigger: a suffix with NO leading dot.
                sites_domain_suffix: Some("localhost".to_string()),
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        let router = Router::new(&config, test_store(), None, None, None, None, None);

        // `Host: localhost` strips to the empty key. It must resolve to the
        // DEFAULT document root with NO tenant identity — never to `sites_dir`
        // (which would put every tenant's container inside one `open_basedir`).
        let resolved = router.resolve_site("localhost");
        assert_eq!(
            resolved.key, None,
            "empty stripped key must name no tenant, got {:?}",
            resolved.key
        );
        assert_eq!(
            resolved.roots.document_root,
            dir.path(),
            "empty stripped key must fall back to the default document root"
        );
        assert_ne!(
            resolved.roots.document_root, sites,
            "empty stripped key must not resolve the whole sites_dir as one vhost's root"
        );

        // A real tenant whose bare name equals a directory still resolves via
        // the literal (`clean`) fallback — the fix only drops the *stripped*
        // candidate, never the literal one.
        let alpha = router.resolve_site("alpha");
        assert_eq!(alpha.key.as_deref(), Some("alpha"));
        assert_eq!(alpha.roots.document_root, sites.join("alpha"));
    }

    /// The good path is untouched: a correctly *dotted* suffix still strips to a
    /// valid key. `blog.preview.ephpm.dev` with suffix `.preview.ephpm.dev`
    /// resolves the `blog/` container.
    #[test]
    fn dotted_suffix_still_strips_to_the_vhost_key() {
        let dir = tempfile::tempdir().unwrap();
        let sites = dir.path().join("sites");
        fs::create_dir_all(sites.join("blog")).unwrap();

        let config = Config {
            server: ServerConfig {
                listen: "0.0.0.0:8080".to_string(),
                document_root: dir.path().to_path_buf(),
                sites_dir: Some(sites.clone()),
                sites_domain_suffix: Some(".preview.ephpm.dev".to_string()),
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };
        let router = Router::new(&config, test_store(), None, None, None, None, None);

        let resolved = router.resolve_site("blog.preview.ephpm.dev");
        assert_eq!(resolved.key.as_deref(), Some("blog"));
        assert_eq!(resolved.roots.document_root, sites.join("blog"));
    }

    // ── one canonical site key: the four derivations must agree ──────
    //
    // Issue #290. With `sites_domain_suffix` set, `Host: shop.local` and
    // `Host: shop` selected the same document root and the same temp/session
    // directory but *different* databases — one tenant, two databases. The
    // tests below do not re-test that one symptom; they pin the invariant that
    // makes the whole class impossible: for every legal spelling of a tenant's
    // `Host`, all four derivations come out of one canonical key.
    mod site_key_agreement {
        use std::collections::HashMap;

        use ephpm_config::SqliteConfig;
        use litewire::backend::{AuthRequest, ConnectionAuthenticator};

        use super::*;
        use crate::site_backends::SiteBackends;
        use crate::site_wire_auth::SiteWireAuth;

        const SALT: &[u8] = b"abcdefghijklmnopqrst";

        fn stats() -> ephpm_query_stats::QueryStats {
            ephpm_query_stats::QueryStats::new(ephpm_query_stats::StatsConfig {
                enabled: false,
                slow_query_threshold: Duration::from_secs(1),
                max_digests: 16,
                metric_label_series_max: 16,
            })
        }

        /// The `mysql_native_password` response a real client would send.
        fn client_response(password: &str, salt: &[u8]) -> Vec<u8> {
            use sha1::{Digest, Sha1};
            let stage1: [u8; 20] = Sha1::digest(password.as_bytes()).into();
            let stage2: [u8; 20] = Sha1::digest(stage1).into();
            let mut h = Sha1::new();
            h.update(salt);
            h.update(stage2);
            let mask = h.finalize();
            stage1.iter().zip(mask).map(|(s, m)| s ^ m).collect()
        }

        /// A multi-tenant router: `sites_dir`, per-site databases, and the
        /// multi-tenant wire listener's credential minter attached — i.e. all
        /// four derivations live.
        fn router_with(
            docroot: &Path,
            sites: &Path,
            dbdir: &Path,
            suffix: Option<&str>,
            auth: &SiteWireAuth,
        ) -> Router {
            let config = Config {
                server: ServerConfig {
                    listen: "0.0.0.0:8080".to_string(),
                    document_root: docroot.to_path_buf(),
                    sites_dir: Some(sites.to_path_buf()),
                    sites_domain_suffix: suffix.map(str::to_owned),
                    ..ServerConfig::default()
                },
                php: PhpConfig::default(),
                db: DbConfig {
                    sqlite: Some(SqliteConfig {
                        path: "unused-in-per-site-mode.db".to_string(),
                        dir: Some(dbdir.display().to_string()),
                        max_open_dbs: 8,
                        engine: "turso".to_string(),
                        proxy: ephpm_config::SqliteProxyConfig::default(),
                        sqld: None,
                        replication: ephpm_config::ReplicationConfig::default(),
                    }),
                    ..DbConfig::default()
                },
                kv: KvConfig::default(),
                cluster: ClusterConfig::default(),
                middleware: Vec::new(),
                opcache: ephpm_config::OpcacheConfig::default(),
            };
            let router = Router::new(&config, test_store(), None, None, None, None, None);
            assert!(router.per_site_db, "fixture must run in per-site database mode");
            router.with_per_site_db_wire(auth.clone(), "127.0.0.1:3306".to_string())
        }

        /// Everything ePHPm derives per tenant from one request's `Host`.
        #[derive(Debug, PartialEq, Eq)]
        struct Derivations {
            /// The canonical site key itself.
            key: Option<String>,
            /// (1) routing — the document root whose code runs.
            document_root: PathBuf,
            /// (2) the per-site database file the bridge and the wire listener
            /// would open.
            db_path: Option<PathBuf>,
            /// (3) the per-vhost temp + session state root.
            state_root: PathBuf,
            /// (4) the per-site wire credential injected into `$_SERVER`.
            wire_user: Option<String>,
            wire_password: Option<String>,
            /// The KV keyspace / RESP credential identity.
            kv: String,
            /// The OPcache invalidation vhost.
            opcache: String,
        }

        /// Run every derivation for `host` exactly the way a request does.
        fn derive(router: &Router, backends: &SiteBackends, host: &str) -> Derivations {
            let resolved = router.resolve_site(host);
            let ids = router.site_identities(resolved.key.as_deref(), host);
            let env: HashMap<String, String> = ids
                .db
                .as_deref()
                .map(|key| router.build_per_site_db_env_vars(key))
                .unwrap_or_default()
                .into_iter()
                .collect();
            Derivations {
                key: resolved.key.clone(),
                // The state root follows the site CONTAINER, matching
                // `handle_php` — so it names the tenant, not the tenant's web
                // root, and does not move when a site gains a `public/`.
                state_root: vhost_state_root(&resolved.roots.container),
                document_root: resolved.roots.document_root,
                db_path: ids
                    .db
                    .as_deref()
                    .map(|key| backends.db_path_for(key).expect("canonical key must be valid")),
                wire_user: env.get("DB_USER").cloned(),
                wire_password: env.get("DB_PASSWORD").cloned(),
                kv: ids.kv,
                opcache: ids.opcache,
            }
        }

        /// THE regression guard for #290: every legal spelling of one tenant's
        /// `Host` — with and without the configured suffix, with a port, in
        /// upper case, with a trailing FQDN dot — must produce byte-identical
        /// derivations. A difference in any field is an isolation or integrity
        /// bug; the shipped instance was `shop.local.db` vs `shop.db`.
        #[tokio::test]
        async fn every_host_spelling_of_one_tenant_agrees() {
            let dir = tempfile::tempdir().unwrap();
            let sites = dir.path().join("sites");
            let dbdir = dir.path().join("dbs");
            fs::create_dir_all(sites.join("shop")).unwrap();

            let backends =
                SiteBackends::new(dbdir.clone(), 8, stats(), tokio::runtime::Handle::current())
                    .expect("registry");
            let auth = SiteWireAuth::new(backends.clone()).expect("secret");
            let router = router_with(dir.path(), &sites, &dbdir, Some(".local"), &auth);

            let spellings = [
                "shop.local",      // the suffixed name a browser sends
                "shop",            // the bare directory name
                "SHOP.LOCAL",      // upper case
                "shop.local:8080", // with a port
                "shop.local.",     // trailing FQDN root dot
                "SHOP:8080",
                "shop.",
            ];

            let expected = derive(&router, &backends, spellings[0]);
            assert_eq!(expected.key.as_deref(), Some("shop"));
            assert_eq!(expected.document_root, sites.join("shop"));
            assert_eq!(expected.db_path, Some(dbdir.join("shop.db")));
            assert_eq!(expected.wire_user.as_deref(), Some("shop"));
            assert_eq!(expected.kv, "shop");
            assert_eq!(expected.opcache, "shop");

            for host in spellings {
                assert_eq!(
                    derive(&router, &backends, host),
                    expected,
                    "Host: {host} must derive exactly the same tenant identity as \
                     Host: {} — a disagreement here is issue #290",
                    spellings[0]
                );
            }
        }

        /// The wire half of the same invariant, checked by actually
        /// authenticating: the credential the router injects for a request must
        /// be accepted by the listener under the same key, and must reach the
        /// same database *file* the bridge would.
        #[tokio::test]
        async fn injected_credential_authenticates_to_the_same_database() {
            let dir = tempfile::tempdir().unwrap();
            let sites = dir.path().join("sites");
            let dbdir = dir.path().join("dbs");
            fs::create_dir_all(sites.join("shop")).unwrap();

            let backends =
                SiteBackends::new(dbdir.clone(), 8, stats(), tokio::runtime::Handle::current())
                    .expect("registry");
            let auth = SiteWireAuth::new(backends.clone()).expect("secret");
            let router = router_with(dir.path(), &sites, &dbdir, Some(".local"), &auth);

            // Credentials as injected for a request that arrived suffixed.
            let d = derive(&router, &backends, "shop.local");
            let user = d.wire_user.expect("per-site DB_USER");
            let password = d.wire_password.expect("per-site DB_PASSWORD");

            // The listener accepts them...
            let backend = auth
                .authenticate(&AuthRequest {
                    auth_plugin: "mysql_native_password",
                    username: user.as_bytes(),
                    salt: SALT,
                    auth_response: &client_response(&password, SALT),
                    local_addr: "127.0.0.1:3306".parse().unwrap(),
                    peer_addr: "127.0.0.1:40000".parse().unwrap(),
                })
                .await
                .expect("the credential the router injected must authenticate");

            // ...and hands back the same file the bridge would open.
            backend
                .connect()
                .await
                .expect("connect")
                .execute("CREATE TABLE t (v TEXT)", &[])
                .await
                .expect("write");
            assert!(dbdir.join("shop.db").exists(), "must be the canonical key's database");
            assert!(
                !dbdir.join("shop.local.db").exists(),
                "the suffixed spelling must not mint a second database — issue #290"
            );
        }

        /// Issue #291: a well-formed but unknown `Host` still gets the default
        /// document root, but no tenant identity — so nothing can mint
        /// `<that host>.db`.
        #[tokio::test]
        async fn unknown_host_gets_no_database_identity() {
            let dir = tempfile::tempdir().unwrap();
            let sites = dir.path().join("sites");
            let dbdir = dir.path().join("dbs");
            fs::create_dir_all(sites.join("shop")).unwrap();

            let backends =
                SiteBackends::new(dbdir.clone(), 8, stats(), tokio::runtime::Handle::current())
                    .expect("registry");
            let auth = SiteWireAuth::new(backends.clone()).expect("secret");
            let router = router_with(dir.path(), &sites, &dbdir, Some(".local"), &auth);

            for host in ["random.example.com", "127.0.0.1:8080", "not-a-site"] {
                let d = derive(&router, &backends, host);
                assert_eq!(d.key, None, "`{host}` names no vhost");
                assert_eq!(d.document_root, dir.path(), "unknown host still serves the default");
                assert_eq!(d.db_path, None, "`{host}` must not name a database — issue #291");
                assert_eq!(d.wire_user, None, "`{host}` must get no DB credential");
                assert_eq!(d.opcache, crate::opcache::DEFAULT_VHOST);
                // The KV keyspace deliberately still follows the host: a
                // catch-all document root may legitimately serve many names.
                assert_eq!(d.kv, normalize_host_key(host));
            }
        }

        /// Agreement must not over-merge: with no suffix configured, `shop` and
        /// `shop.local` are two different vhost directories and therefore two
        /// different tenants, with everything separate.
        #[tokio::test]
        async fn distinct_sites_stay_distinct_without_a_suffix() {
            let dir = tempfile::tempdir().unwrap();
            let sites = dir.path().join("sites");
            let dbdir = dir.path().join("dbs");
            fs::create_dir_all(sites.join("shop")).unwrap();
            fs::create_dir_all(sites.join("shop.local")).unwrap();

            let backends =
                SiteBackends::new(dbdir.clone(), 8, stats(), tokio::runtime::Handle::current())
                    .expect("registry");
            let auth = SiteWireAuth::new(backends.clone()).expect("secret");
            let router = router_with(dir.path(), &sites, &dbdir, None, &auth);

            let bare = derive(&router, &backends, "shop");
            let dotted = derive(&router, &backends, "shop.local");

            assert_eq!(bare.key.as_deref(), Some("shop"));
            assert_eq!(dotted.key.as_deref(), Some("shop.local"));
            assert_ne!(bare.document_root, dotted.document_root);
            assert_ne!(bare.db_path, dotted.db_path);
            assert_ne!(bare.state_root, dotted.state_root);
            assert_ne!(bare.wire_password, dotted.wire_password);
        }

        /// The wire path applies [`normalize_host_key`] to the client-asserted
        /// username. A canonical key must be a **fixed point** of it, or the
        /// key the router injects and the key the listener resolves would be
        /// two different tenants.
        #[tokio::test]
        async fn canonical_keys_are_fixed_points_of_the_wire_normalization() {
            let dir = tempfile::tempdir().unwrap();
            let sites = dir.path().join("sites");
            for name in ["shop", "blog.example.com", "a-b_c"] {
                fs::create_dir_all(sites.join(name)).unwrap();
            }
            let dbdir = dir.path().join("dbs");
            let backends =
                SiteBackends::new(dbdir.clone(), 8, stats(), tokio::runtime::Handle::current())
                    .expect("registry");
            let auth = SiteWireAuth::new(backends).expect("secret");
            let router = router_with(dir.path(), &sites, &dbdir, Some(".local"), &auth);

            for host in ["shop.local", "SHOP", "blog.example.com.", "a-b_c:8080"] {
                let key = router.canonical_site_key(host).expect("known site");
                assert_eq!(normalize_host_key(&key), key, "`{key}` must normalize to itself");
                assert!(is_valid_site_key(&key), "`{key}` must pass the allowlist gate");
            }
        }
    }

    // ── streaming compression (worker send_response_stream) ────────

    fn compression_with(streaming: StreamingCompression) -> CompressionSettings {
        CompressionSettings { enabled: true, level: 1, min_size: 1024, streaming }
    }

    fn sse_headers() -> Vec<(String, String)> {
        vec![
            ("Content-Type".to_string(), "text/event-stream".to_string()),
            ("Cache-Control".to_string(), "no-store".to_string()),
        ]
    }

    /// A fresh, un-tripped abort flag — the state `response_begin` hands to
    /// hyper for a healthy streamed response.
    fn live_stream() -> ephpm_php::worker_bridge::StreamAbortFlag {
        Arc::new(std::sync::atomic::AtomicBool::new(false))
    }

    #[test]
    fn streaming_compression_parse() {
        assert_eq!(StreamingCompression::parse("off"), Some(StreamingCompression::Off));
        assert_eq!(StreamingCompression::parse("sse"), Some(StreamingCompression::Sse));
        assert_eq!(StreamingCompression::parse("all"), Some(StreamingCompression::All));
        assert_eq!(StreamingCompression::parse("SSE"), Some(StreamingCompression::Sse));
        assert_eq!(StreamingCompression::parse("gzip"), None, "unknown must not parse");
        assert_eq!(StreamingCompression::parse(""), None);
    }

    #[test]
    fn streamed_brotli_predicate() {
        let sse = sse_headers();
        let html = vec![("Content-Type".to_string(), "text/html".to_string())];
        let encoded = vec![
            ("Content-Type".to_string(), "text/event-stream".to_string()),
            ("Content-Encoding".to_string(), "gzip".to_string()),
        ];

        // Off: never, regardless of everything else.
        assert!(!streamed_response_wants_brotli(&sse, true, StreamingCompression::Off));
        // No client brotli support: never.
        assert!(!streamed_response_wants_brotli(&sse, false, StreamingCompression::Sse));
        assert!(!streamed_response_wants_brotli(&sse, false, StreamingCompression::All));
        // Sse: only text/event-stream.
        assert!(streamed_response_wants_brotli(&sse, true, StreamingCompression::Sse));
        assert!(!streamed_response_wants_brotli(&html, true, StreamingCompression::Sse));
        // All: any content type.
        assert!(streamed_response_wants_brotli(&html, true, StreamingCompression::All));
        // Never double-encode a body PHP already encoded.
        assert!(!streamed_response_wants_brotli(&encoded, true, StreamingCompression::Sse));
        assert!(!streamed_response_wants_brotli(&encoded, true, StreamingCompression::All));
        // Charset suffix on the content type still matches.
        let sse_charset =
            vec![("content-type".to_string(), "text/event-stream; charset=utf-8".to_string())];
        assert!(streamed_response_wants_brotli(&sse_charset, true, StreamingCompression::Sse));
    }

    /// The zero-behavior-change contract: with the default `Off` the
    /// response has no compression headers and the body bytes pass
    /// through untouched, even for a brotli-capable client.
    #[tokio::test]
    async fn streamed_response_off_is_identity() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
        let resp = build_streamed_worker_response(
            200,
            sse_headers(),
            rx,
            live_stream(),
            true,
            compression_with(StreamingCompression::Off),
        );
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get("content-encoding").is_none(), "Off must not set an encoding");

        tx.send(Bytes::from_static(
            b"data: one

",
        ))
        .await
        .unwrap();
        tx.send(Bytes::from_static(
            b"data: two

",
        ))
        .await
        .unwrap();
        drop(tx);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            &body[..],
            b"data: one

data: two

"
        );
    }

    #[tokio::test]
    async fn streamed_response_sse_compresses_and_round_trips() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
        let resp = build_streamed_worker_response(
            200,
            sse_headers(),
            rx,
            live_stream(),
            true,
            compression_with(StreamingCompression::Sse),
        );
        assert_eq!(
            resp.headers().get("content-encoding").and_then(|v| v.to_str().ok()),
            Some("br")
        );
        assert_eq!(
            resp.headers().get("vary").and_then(|v| v.to_str().ok()),
            Some("Accept-Encoding")
        );
        // PHP-supplied headers survive.
        assert_eq!(
            resp.headers().get("cache-control").and_then(|v| v.to_str().ok()),
            Some("no-store")
        );

        tx.send(Bytes::from_static(
            b"data: one

",
        ))
        .await
        .unwrap();
        tx.send(Bytes::from_static(
            b"data: two

",
        ))
        .await
        .unwrap();
        drop(tx);
        let wire = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(!wire.is_empty());

        let mut plain = Vec::new();
        let mut dec = brotli::Decompressor::new(&wire[..], 4096);
        std::io::Read::read_to_end(&mut dec, &mut plain).expect("valid brotli stream");
        assert_eq!(
            &plain[..],
            b"data: one

data: two

"
        );
    }

    #[tokio::test]
    async fn streamed_response_sse_mode_leaves_non_sse_alone() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
        let headers = vec![("Content-Type".to_string(), "application/octet-stream".to_string())];
        let resp = build_streamed_worker_response(
            200,
            headers,
            rx,
            live_stream(),
            true,
            compression_with(StreamingCompression::Sse),
        );
        assert!(resp.headers().get("content-encoding").is_none());
        tx.send(Bytes::from_static(b" raw")).await.unwrap();
        drop(tx);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b" raw");
    }

    // -- a bailout mid-stream must break the response, not finish it ----
    //
    // Once `send_response_stream` has delivered status + headers they are on
    // the wire and cannot be retracted. The only remaining way to tell the
    // client "this failed" is to refuse to terminate the body correctly: hyper
    // must see an error, so it emits no terminating chunk under
    // `Transfer-Encoding: chunked`. A clean short body and an aborted short
    // body carry identical bytes — only the terminal item differs, which is
    // what these tests pin.

    /// Worker dies mid-stream, uncompressed: the body resolves to an **error**.
    /// Before the fix, dropping the chunk sender was indistinguishable from a
    /// normal end of body, so the client read a well-formed, truncated `200`.
    #[tokio::test]
    async fn streamed_response_aborts_when_worker_bails_mid_body() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
        let aborted = live_stream();
        let resp = build_streamed_worker_response(
            200,
            sse_headers(),
            rx,
            Arc::clone(&aborted),
            true,
            compression_with(StreamingCompression::Off),
        );
        // The 200 status line already went out. That is the premise here, not
        // a bug — it cannot be retracted, only contradicted by the framing.
        assert_eq!(resp.status(), StatusCode::OK);

        tx.send(Bytes::from_static(b"data: one\n\n")).await.unwrap();
        // Reproduce `clear_in_flight_streams`'s ordering exactly: set the flag,
        // THEN drop the sender. The reverse order would race.
        aborted.store(true, std::sync::atomic::Ordering::SeqCst);
        drop(tx);

        let err = resp
            .into_body()
            .collect()
            .await
            .expect_err("an aborted stream must not collect into a complete body");
        assert!(
            err.to_string().contains("incomplete"),
            "the error should say why the transfer failed, got: {err}"
        );
    }

    /// Same, through the brotli streaming wrapper: the flag is read on the
    /// outer stream, which ends only after the encoder task's upstream does, so
    /// compression must not swallow the abort.
    #[tokio::test]
    async fn streamed_brotli_response_aborts_when_worker_bails_mid_body() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
        let aborted = live_stream();
        let resp = build_streamed_worker_response(
            200,
            sse_headers(),
            rx,
            Arc::clone(&aborted),
            true,
            compression_with(StreamingCompression::Sse),
        );
        assert_eq!(
            resp.headers().get("content-encoding").and_then(|v| v.to_str().ok()),
            Some("br"),
            "precondition: this test must exercise the compressed path"
        );

        tx.send(Bytes::from_static(b"data: one\n\n")).await.unwrap();
        aborted.store(true, std::sync::atomic::Ordering::SeqCst);
        drop(tx);

        assert!(
            resp.into_body().collect().await.is_err(),
            "an aborted brotli stream must not collect into a complete body"
        );
    }

    /// The inverse guard: a stream that ends with the flag clear still
    /// completes normally. Without it, "always error" would satisfy the two
    /// tests above while breaking every streamed response in the product.
    #[tokio::test]
    async fn streamed_response_completes_when_abort_flag_stays_clear() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
        let aborted = live_stream();
        let resp = build_streamed_worker_response(
            200,
            sse_headers(),
            rx,
            Arc::clone(&aborted),
            true,
            compression_with(StreamingCompression::Off),
        );
        tx.send(Bytes::from_static(b"data: one\n\n")).await.unwrap();
        drop(tx);
        assert!(!aborted.load(std::sync::atomic::Ordering::SeqCst));
        let body = resp.into_body().collect().await.expect("clean EOF").to_bytes();
        assert_eq!(&body[..], b"data: one\n\n");
    }

    // -- metrics label helper (#136) --------------------------------

    #[test]
    fn method_metric_label_standard_methods_are_static() {
        // Standard verbs map to their canonical spelling with no allocation.
        assert_eq!(method_metric_label(&hyper::Method::GET), "GET");
        assert_eq!(method_metric_label(&hyper::Method::POST), "POST");
        assert_eq!(method_metric_label(&hyper::Method::PUT), "PUT");
        assert_eq!(method_metric_label(&hyper::Method::DELETE), "DELETE");
        assert_eq!(method_metric_label(&hyper::Method::HEAD), "HEAD");
        assert_eq!(method_metric_label(&hyper::Method::OPTIONS), "OPTIONS");
        assert_eq!(method_metric_label(&hyper::Method::PATCH), "PATCH");
        assert_eq!(method_metric_label(&hyper::Method::TRACE), "TRACE");
        assert_eq!(method_metric_label(&hyper::Method::CONNECT), "CONNECT");
    }

    #[test]
    fn method_metric_label_custom_method_collapses_to_other() {
        // A non-standard verb must NOT be reflected verbatim into the label --
        // that would let a client explode Prometheus `method` cardinality.
        let custom = hyper::Method::from_bytes(b"WHATEVER").unwrap();
        assert_eq!(method_metric_label(&custom), "OTHER");
    }

    #[test]
    fn method_metric_label_matches_as_str_for_standard_verbs() {
        // For the standard verbs the label is byte-identical to the previous
        // `method.as_str().to_string()` behavior -- this pins that the
        // allocation-free path did not change what the label reports.
        for m in [
            hyper::Method::GET,
            hyper::Method::POST,
            hyper::Method::PUT,
            hyper::Method::DELETE,
            hyper::Method::HEAD,
            hyper::Method::OPTIONS,
            hyper::Method::PATCH,
            hyper::Method::TRACE,
            hyper::Method::CONNECT,
        ] {
            assert_eq!(method_metric_label(&m), m.as_str());
        }
    }

    #[test]
    fn status_label_matches_code_for_known_statuses() {
        // The status label switched from `as_u16().to_string()` (an alloc) to
        // a `&'static str` lookup. For every code this server emits, the
        // label must still be the exact 3-digit string the old path produced.
        for code in [
            200u16, 201, 202, 204, 206, 301, 302, 303, 304, 307, 308, 400, 401, 403, 404, 405, 406,
            409, 410, 413, 415, 421, 422, 429, 500, 501, 502, 503, 504,
        ] {
            let status = StatusCode::from_u16(code).unwrap();
            assert_eq!(status_metric_label(status), code.to_string());
        }
    }

    #[test]
    fn status_label_unknown_code_collapses_to_other() {
        // An exotic status (e.g. a PHP app returning 418) must not become its
        // own Prometheus series -- it collapses to "other".
        assert_eq!(status_metric_label(StatusCode::from_u16(418).unwrap()), "other");
        assert_eq!(status_metric_label(StatusCode::from_u16(299).unwrap()), "other");
    }

    // -- request-timeout disable (#135) -----------------------------

    #[test]
    fn request_timeout_zero_yields_zero_duration() {
        // `[server.timeouts] request = 0` must produce a zero `request_timeout`
        // so `handle` takes the no-timer fast path. A non-zero value must not.
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config {
            server: ServerConfig {
                document_root: dir.path().to_path_buf(),
                ..ServerConfig::default()
            },
            php: PhpConfig::default(),
            db: DbConfig::default(),
            kv: KvConfig::default(),
            cluster: ClusterConfig::default(),
            middleware: Vec::new(),
            opcache: ephpm_config::OpcacheConfig::default(),
        };

        config.server.timeouts.request = 0;
        let router = Router::new(&config, test_store(), None, None, None, None, None);
        assert!(
            router.request_timeout.is_zero(),
            "request = 0 must disable the per-request deadline"
        );

        config.server.timeouts.request = 30;
        let router = Router::new(&config, test_store(), None, None, None, None, None);
        assert_eq!(router.request_timeout, Duration::from_secs(30));
    }

    // ── Readiness: database proxy upstream (issue #226) ───────────────────

    /// A `DbProxyHealth` with one MySQL proxy configured.
    fn health_with_mysql() -> Arc<crate::db_health::DbProxyHealth> {
        let mut config = Config::default();
        config.db.mysql = Some(ephpm_config::DbBackendConfig {
            url: "mysql://root@127.0.0.1:3307/main".to_string(),
            listen: Some("127.0.0.1:3306".to_string()),
            ..Default::default()
        });
        crate::db_health::DbProxyHealth::from_config(&config).expect("valid db config")
    }

    /// No `[db.mysql]` / `[db.postgres]` at all: readiness must not mention
    /// databases. That is the overwhelming majority of deployments (embedded
    /// SQLite, or no database) and must behave exactly as before.
    #[test]
    fn readiness_ignores_databases_when_no_proxy_is_configured() {
        let health = crate::db_health::DbProxyHealth::from_config(&Config::default()).unwrap();
        assert!(Router::db_not_ready(Some(&health)).is_none());
        assert!(Router::db_not_ready(None).is_none());
    }

    /// The reported hole: the proxy never reached its upstream, yet the
    /// server reported ready and served 500s.
    #[test]
    fn readiness_fails_while_a_proxy_has_never_reached_its_upstream() {
        let health = health_with_mysql();
        let resp = Router::db_not_ready(Some(&health)).expect("must report not ready");
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        health.mysql().unwrap().record_up();
        assert!(
            Router::db_not_ready(Some(&health)).is_none(),
            "one upstream handshake must clear the readiness gate"
        );
    }

    /// The 503 body must be parseable JSON that names the proxy — an operator
    /// reading `kubectl describe` should not have to open the logs to learn
    /// which upstream is unreachable.
    #[tokio::test]
    async fn readiness_body_names_the_pending_proxy() {
        let health = health_with_mysql();
        let resp = Router::db_not_ready(Some(&health)).expect("not ready");
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .expect("collect body")
            .to_bytes();
        let body = String::from_utf8(bytes.to_vec()).expect("utf-8 body");
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("probe body is JSON");
        assert_eq!(parsed["status"], "not_ready");
        let reason = parsed["reason"].as_str().expect("reason string");
        assert!(reason.contains("mysql"), "reason names the proxy kind: {reason}");
        assert!(reason.contains("127.0.0.1:3307"), "reason names the upstream: {reason}");
    }

    /// A post-startup outage must NOT flap readiness. If this test starts
    /// failing, someone changed the gate to read live upstream state — read
    /// the `crate::db_health` module docs before calling that a fix.
    #[test]
    fn readiness_does_not_flap_on_a_post_startup_outage() {
        let health = health_with_mysql();
        let mysql = health.mysql().unwrap();
        mysql.record_up();
        mysql.record_down(&"connection refused");
        assert!(!mysql.is_up(), "the live gauge must reflect the outage");
        assert!(
            Router::db_not_ready(Some(&health)).is_none(),
            "a database outage must not evict the pod from rotation"
        );
    }

    /// The whole probe path, including ordering against the PHP and
    /// worker-pool gates. Stub builds only: marking the PHP runtime ready is
    /// a flag flip there, rather than a real SAPI boot.
    #[cfg(not(php_linked))]
    #[test]
    fn ready_endpoint_reports_503_until_the_proxy_reaches_its_upstream() {
        let dir = tempfile::tempdir().unwrap();
        ephpm_php::PhpRuntime::init_with_ini_file(None).expect("stub init");

        let health = health_with_mysql();
        let router = test_router(dir.path()).with_db_health(Arc::clone(&health));
        assert_eq!(
            router.readiness_check().status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "/_ephpm/ready must be 503 while the proxy has never reached its upstream"
        );

        health.mysql().unwrap().record_up();
        assert_eq!(router.readiness_check().status(), StatusCode::OK);
    }

    // ── Primary probe: active-passive LB routing (/_ephpm/primary) ────────

    /// Drive a GET `/_ephpm/primary` through the full router and return the
    /// (status, JSON body) pair. Served before every security gate and before
    /// PHP, so no runtime init is needed.
    async fn fetch_primary(router: &Router) -> (StatusCode, serde_json::Value) {
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let req = Request::builder()
            .method("GET")
            .uri("/_ephpm/primary")
            .body(Empty::<Bytes>::new())
            .unwrap();
        let resp = router.handle(req, addr, false).await.unwrap();
        let status = resp.status();
        assert_eq!(
            resp.headers().get("content-type").and_then(|v| v.to_str().ok()),
            Some("application/json"),
        );
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap())
    }

    /// Non-clustered / standalone: the default `primary_view` is a constant
    /// `true`, so a node with no election is trivially writable and the probe
    /// returns 200 — safe to health-check in any topology, never a 404.
    #[tokio::test]
    async fn primary_endpoint_is_200_when_not_clustered() {
        let dir = tempfile::tempdir().unwrap();
        let router = test_router(dir.path());
        let (status, body) = fetch_primary(&router).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["primary"], true);
    }

    /// This node is the elected clustered-SQLite primary: the shared view is
    /// `true`, so the probe returns 200 and the LB routes writes here.
    #[tokio::test]
    async fn primary_endpoint_is_200_when_primary() {
        let dir = tempfile::tempdir().unwrap();
        let router = test_router(dir.path()).with_primary_view(Arc::new(AtomicBool::new(true)));
        let (status, body) = fetch_primary(&router).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["primary"], true);
    }

    /// This node is a clustered-SQLite replica: the shared view is `false`, so
    /// the probe returns 503 and the LB steers writes away (a write here would
    /// silently diverge and be lost).
    #[tokio::test]
    async fn primary_endpoint_is_503_when_replica() {
        let dir = tempfile::tempdir().unwrap();
        let router = test_router(dir.path()).with_primary_view(Arc::new(AtomicBool::new(false)));
        let (status, body) = fetch_primary(&router).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["primary"], false);
    }

    /// A failover flips the shared view live: the same router reports 503 as a
    /// replica, then 200 the instant the election promotes it — no rebuild, no
    /// restart (mirrors what `start_clustered_turso_cdc`'s role-change watcher
    /// does to this exact `AtomicBool`).
    #[tokio::test]
    async fn primary_endpoint_tracks_a_live_role_change() {
        let dir = tempfile::tempdir().unwrap();
        let view = Arc::new(AtomicBool::new(false));
        let router = test_router(dir.path()).with_primary_view(Arc::clone(&view));

        assert_eq!(fetch_primary(&router).await.0, StatusCode::SERVICE_UNAVAILABLE);

        // Election promotes this node to primary.
        view.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(fetch_primary(&router).await.0, StatusCode::OK);
    }

    #[test]
    fn json_escape_neutralizes_quotes_and_controls() {
        assert_eq!(json_escape("a\"b"), "a\\\"b");
        assert_eq!(json_escape("a\nb"), "a\\nb");
        assert_eq!(json_escape("a\\b"), "a\\\\b");
        assert_eq!(json_escape("plain"), "plain");
    }
}
