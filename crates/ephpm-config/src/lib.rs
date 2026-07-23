use std::path::PathBuf;

use figment::Figment;
use figment::providers::{Env, Format, Toml};
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to load configuration: {0}")]
    Load(#[from] Box<figment::Error>),

    /// A loaded config is internally inconsistent (e.g. worker mode without a
    /// resolvable `worker_script`). Surfaced by [`Config::validate`].
    #[error("invalid configuration: {0}")]
    Validation(String),
}

/// Top-level ePHPm configuration.
///
/// `Default` delegates to each section's own `Default` impl (all of
/// `ServerConfig`/`PhpConfig`/... define one), so `Config::default()` yields
/// the same values as loading an empty TOML file.
#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub php: PhpConfig,
    #[serde(default)]
    pub db: DbConfig,
    #[serde(default)]
    pub kv: KvConfig,
    #[serde(default)]
    pub cluster: ClusterConfig,

    /// Native middleware chain (`[[middleware]]` blocks). Each mount loads a
    /// shared library (`.so`/`.dylib`/`.dll`) at startup and evaluates it per
    /// PHP-bound request, before the request body is read — see the loader in
    /// `ephpm-server`. Mounts run in ascending `order`.
    ///
    /// Default: empty (no middleware loaded).
    #[serde(default)]
    pub middleware: Vec<MiddlewareMount>,

    /// OPcache clustering settings (`[opcache]`).
    ///
    /// Governs cluster-wide OPcache invalidation. See [`OpcacheConfig`].
    #[serde(default)]
    pub opcache: OpcacheConfig,
}

/// One native middleware mount (`[[middleware]]`).
///
/// ```toml
/// [[middleware]]
/// library = "rate-limit"
/// match = "/api/*"
/// order = 20
/// config = { per_ip_rps = 50, burst = 100 }
/// ```
#[derive(Debug, Deserialize)]
pub struct MiddlewareMount {
    /// Module to run. Checked against the builtin registry first (`jwt`,
    /// `cors`, `ratelimit`/`rate-limit`, `security-headers` and their
    /// `ephpm-middleware-*` long forms are compiled into every binary — no
    /// dlopen). Anything else is a shared library: either a bare name
    /// (resolved through the middleware search path with a platform suffix,
    /// e.g. `auth-jwt` → `auth-jwt.linux-x86_64.so`) or an explicit path — a
    /// value containing a path separator or a file extension is used as-is.
    /// Must not be empty (enforced by [`Config::validate`]).
    pub library: String,

    /// Glob the request path must match for this mount to run. `*` matches
    /// any character sequence (including `/`); everything else is literal.
    ///
    /// Default: unset (the middleware runs on every PHP-bound request).
    #[serde(rename = "match", default)]
    pub match_pattern: Option<String>,

    /// Position in the middleware chain. Lower values run first; mounts with
    /// equal `order` keep their declaration order. Required — no default.
    pub order: u32,

    /// Arbitrary configuration table for the module, serialised to JSON and
    /// passed to its `init`.
    ///
    /// Default: unset (the module's `init` receives NULL).
    #[serde(default)]
    pub config: Option<serde_json::Value>,
}

/// HTTP server configuration.
#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    /// Address to listen on (e.g. "0.0.0.0:8080").
    #[serde(default = "default_listen")]
    pub listen: String,

    /// Document root directory for serving files.
    #[serde(default = "default_document_root")]
    pub document_root: PathBuf,

    /// Virtual host directory. Each subdirectory is named after a domain.
    ///
    /// When set, the `Host` header is matched against subdirectory names.
    /// Matched sites use the subdirectory as their document root.
    /// Unmatched hosts fall back to `document_root`.
    ///
    /// Omit to disable vhosting (single-site mode).
    #[serde(default)]
    pub sites_dir: Option<PathBuf>,

    /// Optional domain suffix to strip from incoming `Host` headers when
    /// resolving vhosts. When set (e.g. `.localhost`), a directory named
    /// `~/sites/blog/` matches `Host: blog.localhost` — the suffix is
    /// stripped before the registry lookup and the on-disk lazy fallback.
    ///
    /// Primarily used by `ephpm dev --sites` so developers can keep short
    /// directory names while testing with `*.localhost` URLs. Production
    /// deployments typically leave this unset and name directories with
    /// the full FQDN (`~/sites/blog.example.com/`).
    #[serde(default)]
    pub sites_domain_suffix: Option<String>,

    /// Index file names to try when a directory is requested.
    #[serde(default = "default_index_files")]
    pub index_files: Vec<String>,

    /// Fallback chain for URL resolution. Checked in order for each request.
    ///
    /// Supported variables:
    /// - `$uri` — the request path (e.g. `/blog/hello`)
    /// - `$query_string` — the raw query string
    ///
    /// Entries ending with `/` are treated as directories (index files checked).
    /// The last entry is the fallback — if it starts with `=` it's a status code
    /// (e.g. `=404`), otherwise it's an internal rewrite target.
    ///
    /// Default: `["$uri", "$uri/", "/index.php?$query_string"]`
    #[serde(default = "default_fallback")]
    pub fallback: Vec<String>,

    /// Request limits.
    #[serde(default)]
    pub request: RequestConfig,

    /// Connection timeouts.
    #[serde(default)]
    pub timeouts: TimeoutsConfig,

    /// Response settings.
    #[serde(default)]
    pub response: ResponseConfig,

    /// Static file serving settings.
    #[serde(default, rename = "static")]
    pub static_files: StaticConfig,

    /// PHP `ETag` cache settings.
    #[serde(default, rename = "php_etag_cache")]
    pub php_etag_cache: PhpETagCacheConfig,

    /// Security settings.
    ///
    /// `None` when the `[server.security]` section is absent from the TOML
    /// (and no `EPHPM_SERVER__SECURITY__*` env var is set). Presence of the
    /// section feeds into the resolved defaults for `open_basedir` and
    /// `disable_shell_exec` — see [`ServerConfig::effective_open_basedir`]
    /// and [`ServerConfig::effective_disable_shell_exec`].
    #[serde(default)]
    pub security: Option<SecurityConfig>,

    /// Logging settings.
    #[serde(default)]
    pub logging: LoggingConfig,

    /// Metrics / observability settings.
    #[serde(default)]
    pub metrics: MetricsConfig,

    /// Rate limiting and connection limiting.
    #[serde(default)]
    pub limits: LimitsConfig,

    /// Open file cache for static file serving.
    #[serde(default)]
    pub file_cache: FileCacheConfig,

    /// TLS configuration. When present, enables HTTPS.
    #[serde(default)]
    pub tls: Option<TlsConfig>,
}

/// Request limits configuration (`[server.request]`).
#[derive(Debug, Deserialize)]
pub struct RequestConfig {
    /// Maximum request body size in bytes. Requests exceeding this limit
    /// receive a 413 Payload Too Large response.
    ///
    /// Default: 10 MiB (`10_485_760`). Set to 0 for unlimited.
    #[serde(default = "default_max_body_size")]
    pub max_body_size: u64,

    /// Maximum total size of request headers in bytes.
    ///
    /// Default: 8192 (8 KiB).
    #[serde(default = "default_max_header_size")]
    pub max_header_size: usize,

    /// Allowed `Host` header values. When non-empty, requests with
    /// a `Host` header not in this list receive a 421 Misdirected Request.
    ///
    /// Prevents host header injection attacks. Values should include
    /// the port if non-standard (e.g. `"example.com:8080"`).
    ///
    /// Default: `[]` (all hosts allowed).
    #[serde(default)]
    pub trusted_hosts: Vec<String>,
}

/// Connection timeout configuration (`[server.timeouts]`).
#[derive(Debug, Deserialize)]
pub struct TimeoutsConfig {
    /// Time in seconds to receive the complete request headers after
    /// connection is established.
    ///
    /// Default: 30 seconds.
    #[serde(default = "default_header_read")]
    pub header_read: u64,

    /// Idle connection timeout in seconds. Connections with no read or
    /// write activity for this duration are shut down gracefully.
    ///
    /// Set to `0` to disable the idle timeout.
    ///
    /// Default: 60 seconds.
    #[serde(default = "default_idle")]
    pub idle: u64,

    /// Total request processing timeout in seconds. Covers the entire
    /// request lifecycle including PHP execution.
    ///
    /// Set to `0` to disable the per-request deadline entirely - the router
    /// then runs each request without arming a tokio timer, which removes a
    /// small but measurable per-request overhead on very hot, short-request
    /// workloads. With the deadline off, a wedged request relies on the idle
    /// and header-read timeouts (and, in worker mode, the worker's own
    /// liveness handling) rather than a hard request cutoff.
    ///
    /// Default: 300 seconds (5 minutes).
    #[serde(default = "default_request_timeout")]
    pub request: u64,

    /// Grace period in seconds for in-flight connections to finish during
    /// shutdown. After this timeout, remaining connections are force-closed.
    ///
    /// Default: 30 seconds.
    #[serde(default = "default_shutdown_timeout")]
    pub shutdown: u64,
}

/// Response configuration (`[server.response]`).
#[derive(Debug, Deserialize)]
pub struct ResponseConfig {
    /// Enable gzip compression for text responses.
    ///
    /// Default: true.
    #[serde(default = "default_compression")]
    pub compression: bool,

    /// Gzip compression level (1–9). 1 is fastest, 9 is best compression.
    ///
    /// Default: 1.
    #[serde(default = "default_compression_level")]
    pub compression_level: u32,

    /// Minimum response size in bytes before compression is applied.
    ///
    /// Default: 1024 (1 KiB).
    #[serde(default = "default_compression_min_size")]
    pub compression_min_size: usize,

    /// Streaming (worker-mode `send_response_stream`) response compression.
    ///
    /// Values: `"off"`, `"sse"`, `"all"`.
    ///
    /// - `"off"` — streamed responses go out identity-encoded; the code
    ///   path is byte-for-byte identical to releases without this knob.
    /// - `"sse"` — streamed responses with Content-Type
    ///   `text/event-stream` are brotli-compressed with one encoder whose
    ///   window persists for the stream's lifetime, flushed per chunk so
    ///   each SSE event is decodable the moment it arrives. Repeated
    ///   re-renders of similar markup compress to tiny wire deltas.
    /// - `"all"` — every streamed worker response is compressed this way
    ///   (including binary downloads — usually wasteful; prefer `"sse"`).
    ///
    /// Only applies when `compression = true` and the client sent
    /// `Accept-Encoding: br`; otherwise the stream passes through
    /// untouched. Unknown values log a startup warning and behave as
    /// `"off"`. Buffered (fpm and worker `send_response`) responses are
    /// unaffected — they keep the existing whole-body compression.
    ///
    /// Default: `"off"`.
    #[serde(default = "default_compression_streaming")]
    pub compression_streaming: String,

    /// Custom headers added to every response (both PHP and static).
    ///
    /// Useful for security headers like HSTS, CSP, X-Frame-Options, CORS.
    ///
    /// Example: `{ "Strict-Transport-Security" = "max-age=31536000", "X-Frame-Options" = "DENY" }`
    ///
    /// Default: `{}` (none).
    #[serde(default)]
    pub headers: Vec<[String; 2]>,
}

/// Static file serving configuration (`[server.static]`).
#[derive(Debug, Deserialize)]
pub struct StaticConfig {
    /// Cache-Control header value for static file responses.
    /// Empty string means no Cache-Control header is added.
    ///
    /// Default: `""` (none).
    #[serde(default)]
    pub cache_control: String,

    /// How to handle requests for hidden files (paths with dot-prefixed
    /// segments like `.env`, `.git`, `.htaccess`).
    ///
    /// Values: `"deny"` (403), `"ignore"` (404), `"allow"`.
    ///
    /// Default: `"deny"`.
    #[serde(default = "default_hidden_files")]
    pub hidden_files: String,

    /// Enable `ETag` headers for static files and `304 Not Modified` responses.
    ///
    /// When enabled, static file responses include an `ETag` header based on
    /// a hash of the file content. Requests with a matching `If-None-Match`
    /// header receive a `304 Not Modified` response instead of the full body.
    ///
    /// Default: `true`.
    #[serde(default = "default_etag")]
    pub etag: bool,
}

/// PHP response `ETag` cache configuration (`[server.php_etag_cache]`).
#[derive(Clone, Debug, Deserialize)]
pub struct PhpETagCacheConfig {
    /// Enable `ETag` caching for PHP responses.
    ///
    /// When enabled, `ETags` from PHP response headers are cached in the KV store.
    /// Subsequent requests with matching `If-None-Match` headers receive
    /// `304 Not Modified` responses without executing PHP.
    ///
    /// Only applies to cacheable methods (GET, HEAD).
    ///
    /// Default: `false`.
    #[serde(default = "default_php_etag_cache_enabled")]
    pub enabled: bool,

    /// TTL (Time To Live) for cached `ETags` in seconds.
    ///
    /// - Positive number: Cache expires after N seconds. PHP executes again after expiry.
    /// - Zero or negative (e.g. `-1`): Cache indefinitely. User must manually clear via k/v API.
    ///
    /// To clear cached `ETags` manually (when using indefinite TTL):
    /// ```bash
    /// # Via RESP CLI (if redis_compat enabled):
    /// redis-cli DEL "etag:*"
    ///
    /// # Via native PHP function:
    /// ephpm_kv_del("etag:GET:/api/endpoint");
    /// ```
    ///
    /// Default: `300` (5 minutes).
    #[serde(default = "default_php_etag_cache_ttl")]
    pub ttl_secs: i64,

    /// Key prefix for `ETag` entries in the KV store.
    ///
    /// `ETag`s are stored with keys like `{prefix}{method}:{path}?{query}`.
    /// This allows organizing `ETag` data separately from other KV entries.
    ///
    /// Default: `"etag:"`.
    #[serde(default = "default_php_etag_cache_prefix")]
    pub key_prefix: String,
}

impl Default for PhpETagCacheConfig {
    fn default() -> Self {
        Self {
            enabled: default_php_etag_cache_enabled(),
            ttl_secs: default_php_etag_cache_ttl(),
            key_prefix: default_php_etag_cache_prefix(),
        }
    }
}

/// Security configuration (`[server.security]`).
#[derive(Debug, Default, Deserialize)]
pub struct SecurityConfig {
    /// Trusted reverse proxy addresses (CIDR notation).
    ///
    /// When a request comes from a trusted proxy, `X-Forwarded-For` is used
    /// for `REMOTE_ADDR` and `X-Forwarded-Proto` for HTTPS detection.
    ///
    /// Default: `[]` (trust no proxies).
    #[serde(default)]
    pub trusted_proxies: Vec<String>,

    /// Path patterns blocked from all access (returns 403).
    ///
    /// Supports glob-style patterns: `*` matches any sequence within a segment,
    /// `**` is not supported (use prefix matching instead).
    ///
    /// Examples: `["/wp-config.php", "/vendor/*", "/.env"]`
    ///
    /// Default: `[]` (nothing blocked beyond `hidden_files`).
    #[serde(default)]
    pub blocked_paths: Vec<String>,

    /// Glob patterns for PHP files allowed to execute. When non-empty,
    /// only matching PHP paths run; all others get 403.
    ///
    /// Patterns are matched against the URI path (e.g. `/index.php`,
    /// `/wp-admin/admin.php`). Use `*` for single-segment wildcards.
    ///
    /// Examples: `["/index.php", "/wp-login.php", "/wp-admin/*.php",
    ///            "/wp-cron.php", "/wp-comments-post.php",
    ///            "/xmlrpc.php", "/wp-trackback.php"]`
    ///
    /// Default: `[]` (all PHP files allowed).
    #[serde(default)]
    pub allowed_php_paths: Vec<String>,

    /// Restrict PHP filesystem access to each site's document root.
    ///
    /// When `true` and `sites_dir` is configured, PHP's `open_basedir` is
    /// set per-request to the site's directory + `/tmp`. PHP cannot read
    /// or write files outside that directory.
    ///
    /// An explicitly set value always wins. When unset, resolves to `true`
    /// if the `[server.security]` section is present OR `server.sites_dir`
    /// is set (multi-tenant mode); otherwise `false`. Use
    /// [`ServerConfig::effective_open_basedir`] to read the resolved value.
    #[serde(default)]
    pub open_basedir: Option<bool>,

    /// Disable dangerous PHP functions in multi-tenant mode.
    ///
    /// When `true`, `exec`, `shell_exec`, `system`, `passthru`,
    /// `proc_open`, `popen`, and `pcntl_exec` are disabled via
    /// `disable_functions`. Prevents shell escape from `open_basedir`.
    ///
    /// An explicitly set value always wins. When unset, resolves to `true`
    /// if the `[server.security]` section is present OR `server.sites_dir`
    /// is set (multi-tenant mode); otherwise `false`. Use
    /// [`ServerConfig::effective_disable_shell_exec`] to read the resolved
    /// value.
    #[serde(default)]
    pub disable_shell_exec: Option<bool>,
}

impl ServerConfig {
    /// Resolved value of `security.open_basedir`.
    ///
    /// An explicitly set value always wins. When unset, resolves to `true`
    /// if the `[server.security]` section is present (preserves the
    /// historical present-section default) OR `sites_dir` is set (so a
    /// multi-tenant deployment never silently runs without filesystem
    /// isolation); otherwise `false`.
    #[must_use]
    pub fn effective_open_basedir(&self) -> bool {
        self.resolve_security_flag(|s| s.open_basedir)
    }

    /// Resolved value of `security.disable_shell_exec`.
    ///
    /// Same resolution rules as [`Self::effective_open_basedir`]: explicit
    /// value wins; unset resolves to `true` when the `[server.security]`
    /// section is present or `sites_dir` is set, `false` otherwise.
    #[must_use]
    pub fn effective_disable_shell_exec(&self) -> bool {
        self.resolve_security_flag(|s| s.disable_shell_exec)
    }

    /// Shared resolution for the two isolation flags.
    fn resolve_security_flag(&self, field: impl Fn(&SecurityConfig) -> Option<bool>) -> bool {
        match &self.security {
            // Section present: unset fields default to true (compat with
            // the previous `#[serde(default = "true")]` behavior).
            Some(security) => field(security).unwrap_or(true),
            // Section absent: default on only in multi-tenant mode.
            None => self.sites_dir.is_some(),
        }
    }
}

/// Logging configuration (`[server.logging]`).
#[derive(Debug, Deserialize)]
pub struct LoggingConfig {
    /// Path to the access log file. Empty string disables access logging.
    ///
    /// Default: `""` (disabled).
    #[serde(default)]
    pub access: String,

    /// Log level for server output. Overridden by `RUST_LOG` env var.
    ///
    /// Values: `"trace"`, `"debug"`, `"info"`, `"warn"`, `"error"`.
    ///
    /// Default: `"info"`.
    #[serde(default = "default_log_level")]
    pub level: String,
}

/// Metrics / observability configuration (`[server.metrics]`).
#[derive(Debug, Deserialize)]
pub struct MetricsConfig {
    /// Enable the `/metrics` Prometheus endpoint.
    ///
    /// When `false`, all `metrics` facade calls are zero-cost no-ops.
    ///
    /// Default: `false`.
    #[serde(default)]
    pub enabled: bool,

    /// URL path for the metrics endpoint.
    ///
    /// Default: `"/metrics"`.
    #[serde(default = "default_metrics_path")]
    pub path: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self { enabled: false, path: default_metrics_path() }
    }
}

fn default_metrics_path() -> String {
    "/metrics".to_string()
}

/// Rate limiting and connection limiting (`[server.limits]`).
#[derive(Clone, Debug, Deserialize)]
pub struct LimitsConfig {
    /// Maximum total concurrent connections. New connections are rejected
    /// with 503 when at capacity. `0` means unlimited.
    ///
    /// Default: `0` (unlimited).
    #[serde(default)]
    pub max_connections: usize,

    /// Maximum concurrent connections per client IP. `0` means unlimited.
    ///
    /// Default: `0` (unlimited).
    #[serde(default)]
    pub per_ip_max_connections: usize,

    /// Maximum requests per second per client IP (token bucket rate).
    /// `0` means unlimited.
    ///
    /// Default: `0.0` (unlimited).
    #[serde(default)]
    pub per_ip_rate: f64,

    /// Burst size for per-IP rate limiting. Allows this many requests
    /// to be made instantly before the rate limit kicks in.
    ///
    /// Default: `50`.
    #[serde(default = "default_per_ip_burst")]
    pub per_ip_burst: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_connections: 0,
            per_ip_max_connections: 0,
            per_ip_rate: 0.0,
            per_ip_burst: default_per_ip_burst(),
        }
    }
}

fn default_per_ip_burst() -> u32 {
    50
}

/// Open file cache configuration (`[server.file_cache]`).
///
/// Caches file metadata (size, mtime, MIME type, `ETag`) and optionally
/// small file content in memory. Avoids repeated filesystem `stat` and
/// `read` calls for frequently accessed static files.
#[derive(Clone, Debug, Deserialize)]
pub struct FileCacheConfig {
    /// Enable the open file cache.
    ///
    /// Default: `false`.
    #[serde(default)]
    pub enabled: bool,

    /// Maximum number of cached entries. Oldest entries are evicted
    /// when this limit is reached.
    ///
    /// Default: `10000`.
    #[serde(default = "default_file_cache_max_entries")]
    pub max_entries: usize,

    /// Re-stat interval in seconds. Cached entries are re-validated
    /// against the filesystem at most this often.
    ///
    /// Default: `30`.
    #[serde(default = "default_file_cache_valid_secs")]
    pub valid_secs: u64,

    /// Evict entries not accessed within this many seconds.
    ///
    /// Default: `60`.
    #[serde(default = "default_file_cache_inactive_secs")]
    pub inactive_secs: u64,

    /// Cache file content below this size in bytes. Larger files
    /// only have metadata cached (size, mtime, `ETag`, MIME type).
    ///
    /// Default: `1048576` (1 MiB).
    #[serde(default = "default_file_cache_inline_threshold")]
    pub inline_threshold: usize,

    /// Pre-compute and cache gzip-compressed variants for small
    /// compressible files.
    ///
    /// Default: `true`.
    #[serde(default = "default_file_cache_precompress")]
    pub precompress: bool,
}

impl Default for FileCacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_entries: default_file_cache_max_entries(),
            valid_secs: default_file_cache_valid_secs(),
            inactive_secs: default_file_cache_inactive_secs(),
            inline_threshold: default_file_cache_inline_threshold(),
            precompress: default_file_cache_precompress(),
        }
    }
}

fn default_file_cache_max_entries() -> usize {
    10_000
}

fn default_file_cache_valid_secs() -> u64 {
    30
}

fn default_file_cache_inactive_secs() -> u64 {
    60
}

fn default_file_cache_inline_threshold() -> usize {
    1_048_576
}

fn default_file_cache_precompress() -> bool {
    true
}

/// TLS configuration (`[server.tls]`).
///
/// Supports two mutually exclusive modes:
///
/// - **Manual**: Provide `cert` and `key` paths to PEM files.
/// - **Automatic (ACME)**: Provide `domains` for zero-config Let's Encrypt.
///
/// If both `cert`/`key` and `domains` are set, manual mode takes precedence.
#[derive(Debug, Deserialize, Clone)]
pub struct TlsConfig {
    // --- Manual mode ---
    /// Path to the PEM-encoded certificate chain file.
    #[serde(default)]
    pub cert: Option<PathBuf>,

    /// Path to the PEM-encoded private key file.
    #[serde(default)]
    pub key: Option<PathBuf>,

    // --- ACME mode ---
    /// Domain names for automatic certificate provisioning via ACME.
    ///
    /// When set (and `cert`/`key` are not), the server automatically
    /// obtains and renews TLS certificates from Let's Encrypt.
    ///
    /// Example: `["example.com", "www.example.com"]`
    #[serde(default)]
    pub domains: Vec<String>,

    /// Contact email for ACME account registration.
    ///
    /// Let's Encrypt uses this to send certificate expiry warnings.
    /// Format: `"admin@example.com"` (the `mailto:` prefix is added automatically).
    #[serde(default)]
    pub email: Option<String>,

    /// Directory to cache ACME certificates and account keys.
    ///
    /// Strongly recommended for production — without caching, every restart
    /// requests a new certificate, which can hit Let's Encrypt rate limits
    /// (50 certificates per domain per week).
    ///
    /// Default: `"certs"` (relative to working directory).
    #[serde(default = "default_cache_dir")]
    pub cache_dir: PathBuf,

    /// Use Let's Encrypt staging environment for testing.
    ///
    /// Staging issues untrusted certificates but has relaxed rate limits.
    /// Use this during development to avoid hitting production rate limits.
    ///
    /// Default: `false` (use production Let's Encrypt).
    #[serde(default)]
    pub staging: bool,

    // --- Shared ---
    /// Optional separate listen address for HTTPS (e.g. `"0.0.0.0:443"`).
    ///
    /// When set, `server.listen` serves HTTP and this address serves HTTPS.
    /// When omitted, `server.listen` serves HTTPS directly (no HTTP listener).
    #[serde(default)]
    pub listen: Option<String>,

    /// When `true` and `listen` is set, the HTTP listener redirects
    /// all requests to HTTPS with a 301 Moved Permanently response.
    ///
    /// Default: `false`.
    #[serde(default)]
    pub redirect_http: bool,
}

impl TlsConfig {
    /// Returns `true` if manual TLS mode is configured (cert + key provided).
    #[must_use]
    pub fn is_manual(&self) -> bool {
        self.cert.is_some() && self.key.is_some()
    }

    /// Returns `true` if ACME auto-provisioning is configured.
    #[must_use]
    pub fn is_acme(&self) -> bool {
        !self.domains.is_empty() && !self.is_manual()
    }
}

/// Top-level database proxy configuration (`[db]`).
///
/// When present, ePHPm starts a transparent SQL proxy between PHP and the
/// real database. PHP connects to `127.0.0.1:3306` (or the configured
/// `listen` address) — it never talks to the database directly.
#[derive(Debug, Deserialize, Default)]
pub struct DbConfig {
    /// `MySQL` proxy configuration.
    #[serde(default)]
    pub mysql: Option<DbBackendConfig>,

    /// `PostgreSQL` proxy configuration.
    #[serde(default)]
    pub postgres: Option<DbBackendConfig>,

    /// TDS (`SQL Server`) proxy configuration.
    #[serde(default)]
    pub tds: Option<DbBackendConfig>,

    /// Embedded `SQLite` configuration (via litewire).

    ///
    /// When enabled, starts an in-process `SQLite` database with `MySQL`/Hrana
    /// wire protocol frontends. PHP connects via `pdo_mysql` — no external
    /// database server needed.
    #[serde(default)]
    pub sqlite: Option<SqliteConfig>,

    /// Read/write splitting settings (requires replicas on at least one backend).
    #[serde(default)]
    pub read_write_split: ReadWriteSplitConfig,

    /// Query analysis and optimization settings.
    #[serde(default)]
    pub analysis: DbAnalysisConfig,
}

/// Embedded `SQLite` database configuration (`[db.sqlite]`).
///
/// Uses litewire to expose `SQLite` via `MySQL` wire protocol, so PHP apps
/// can use their existing `pdo_mysql` drivers transparently.
#[derive(Debug, Deserialize, Clone)]
pub struct SqliteConfig {
    /// Path to the `SQLite` database file.
    ///
    /// Default: `"ephpm.db"` in the current working directory.
    #[serde(default = "default_sqlite_path")]
    pub path: String,

    /// Database engine: `"sqlite"` (default) or `"turso"` (**experimental**).
    ///
    /// - `"sqlite"` — the genuine `SQLite` C engine (rusqlite, bundled).
    ///   This is the default and the only production-supported engine.
    /// - `"turso"` — the Turso Database engine, a ground-up Rust rewrite
    ///   of `SQLite` that is **Beta upstream**. Single-node mode only:
    ///   combining `engine = "turso"` with clustered `SQLite` is rejected
    ///   at startup. Startup logs a warning when this engine is selected.
    ///
    /// Any other value is rejected at startup.
    #[serde(default = "default_sqlite_engine")]
    pub engine: String,

    /// Wire protocol proxy settings.
    #[serde(default)]
    pub proxy: SqliteProxyConfig,

    /// sqld process settings (clustered mode only).
    #[serde(default)]
    pub sqld: SqldConfig,

    /// Replication settings (clustered mode only).
    #[serde(default)]
    pub replication: ReplicationConfig,
}

/// Wire protocol frontend addresses for the `SQLite` proxy (`[db.sqlite.proxy]`).
#[derive(Debug, Deserialize, Clone)]
pub struct SqliteProxyConfig {
    /// `MySQL` wire protocol listen address.
    ///
    /// PHP connects here with `pdo_mysql`. Default: `"127.0.0.1:3306"`.
    #[serde(default = "default_sqlite_mysql_listen")]
    pub mysql_listen: String,

    /// Hrana HTTP API listen address (optional).
    ///
    /// Useful for CI tooling, health checks, and direct HTTP access.
    #[serde(default)]
    pub hrana_listen: Option<String>,

    /// `PostgreSQL` wire protocol listen address (optional).
    ///
    /// When set, PHP can connect via `pdo_pgsql` as if talking to a real
    /// `PostgreSQL` server. Default: disabled.
    #[serde(default)]
    pub postgres_listen: Option<String>,

    /// `TDS` wire protocol listen address (optional).
    ///
    /// When set, clients can connect via the `TDS` protocol (SQL Server).
    /// Default: disabled.
    #[serde(default)]
    pub tds_listen: Option<String>,
}

impl Default for SqliteProxyConfig {
    fn default() -> Self {
        Self {
            mysql_listen: default_sqlite_mysql_listen(),
            hrana_listen: None,
            postgres_listen: None,
            tds_listen: None,
        }
    }
}

/// sqld child process configuration (`[db.sqlite.sqld]`).
///
/// Controls the internal sqld instance used for replication in clustered mode.
/// Ignored in single-node mode.
#[derive(Debug, Deserialize, Clone)]
pub struct SqldConfig {
    /// Hrana HTTP listen address for litewire → sqld communication.
    #[serde(default = "default_sqld_http_listen")]
    pub http_listen: String,

    /// gRPC listen address for inter-node replication.
    #[serde(default = "default_sqld_grpc_listen")]
    pub grpc_listen: String,
}

impl Default for SqldConfig {
    fn default() -> Self {
        Self { http_listen: default_sqld_http_listen(), grpc_listen: default_sqld_grpc_listen() }
    }
}

/// Replication configuration (`[db.sqlite.replication]`).
///
/// Controls whether this node runs sqld as a primary or replica.
#[derive(Debug, Deserialize, Clone)]
pub struct ReplicationConfig {
    /// Replication role: `"auto"`, `"primary"`, or `"replica"`.
    ///
    /// - `"auto"`: elected via gossip (lowest-ordinal alive node wins)
    /// - `"primary"`: force this node as primary
    /// - `"replica"`: force this node as replica
    #[serde(default = "default_replication_role")]
    pub role: String,

    /// gRPC URL of the primary node (for replicas).
    ///
    /// Set automatically when `role = "auto"`. Required when `role = "replica"`.
    #[serde(default)]
    pub primary_grpc_url: String,
}

impl Default for ReplicationConfig {
    fn default() -> Self {
        Self { role: default_replication_role(), primary_grpc_url: String::new() }
    }
}

/// Configuration for a single database backend (`MySQL` or `PostgreSQL`).
#[derive(Debug, Deserialize, Clone)]
pub struct DbBackendConfig {
    /// Primary database URL.
    ///
    /// Format: `mysql://user:pass@host:port/dbname` or
    /// `postgres://user:pass@host:port/dbname`.
    pub url: String,

    /// TCP address for the proxy to listen on.
    ///
    /// PHP connects here. Default: `"127.0.0.1:3306"` for `MySQL`,
    /// `"127.0.0.1:5432"` for `PostgreSQL`.
    #[serde(default)]
    pub listen: Option<String>,

    /// Planned: not yet implemented. Unix socket path for the proxy listener
    /// (faster than TCP for local PHP). Currently parsed but not acted upon —
    /// only the TCP `listen` address is active, and a warning is logged at
    /// startup when this is set.
    #[serde(default)]
    pub socket: Option<std::path::PathBuf>,

    /// Minimum number of backend connections to keep open (warm pool).
    ///
    /// Default: `2`.
    #[serde(default = "default_min_connections")]
    pub min_connections: u32,

    /// Maximum total backend connections (in-use + idle).
    ///
    /// PHP requests that arrive when all connections are busy will wait up
    /// to `pool_timeout` before receiving a connection error.
    ///
    /// Default: `20`.
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,

    /// Duration string for closing idle backend connections.
    ///
    /// Default: `"300s"`.
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout: String,

    /// Duration string for maximum backend connection lifetime.
    ///
    /// Connections older than this are closed and replaced to prevent stale
    /// state from accumulating on the database server.
    ///
    /// Default: `"1800s"`.
    #[serde(default = "default_max_lifetime")]
    pub max_lifetime: String,

    /// Duration string to wait for an available connection before failing.
    ///
    /// Default: `"5s"`.
    #[serde(default = "default_pool_timeout")]
    pub pool_timeout: String,

    /// Duration string between backend connection health checks.
    ///
    /// Default: `"30s"`.
    #[serde(default = "default_health_check_interval")]
    pub health_check_interval: String,

    /// When `true`, inject `DB_HOST`, `DB_PORT`, `DB_NAME`, `DB_USER`,
    /// `DB_PASSWORD`, and `DATABASE_URL` environment variables into PHP
    /// pointing at the proxy listener. Framework auto-detection
    /// (Laravel, Symfony, `WordPress`) picks these up automatically.
    ///
    /// Default: `true`.
    #[serde(default = "default_inject_env")]
    pub inject_env: bool,

    /// Connection reset strategy when returning a connection to the pool.
    ///
    /// - `"smart"` — reset only after non-SELECT statements (`MySQL`:
    ///   `COM_RESET_CONNECTION`; `PostgreSQL`: `DISCARD ALL`). Best balance.
    /// - `"always"` — always reset on return. Safest, slight overhead.
    /// - `"never"` — skip reset. Fastest, but session state leaks between
    ///   PHP requests. Use only in trusted environments.
    ///
    /// Default: `"smart"`.
    #[serde(default = "default_reset_strategy")]
    pub reset_strategy: String,

    /// Read replica configuration.
    #[serde(default)]
    pub replicas: Option<ReplicasConfig>,
}

/// Read replica configuration for a database backend.
#[derive(Debug, Deserialize, Clone)]
pub struct ReplicasConfig {
    /// Replica database URLs. Reads are distributed across these;
    /// writes always go to the primary.
    pub urls: Vec<String>,
}

/// Read/write splitting configuration (`[db.read_write_split]`).
#[derive(Debug, Deserialize)]
pub struct ReadWriteSplitConfig {
    /// Enable read/write splitting. Requires at least one backend with replicas.
    ///
    /// Default: `false`.
    #[serde(default)]
    pub enabled: bool,

    /// Load balancing strategy for reads.
    ///
    /// - `"sticky-after-write"` — after a write, reads stay on the primary
    ///   for `sticky_duration` to avoid read-your-writes inconsistency.
    /// - `"lag-aware"` — (planned: not yet implemented) skip replicas whose
    ///   replication lag exceeds `max_replica_lag`.
    ///
    /// Default: `"sticky-after-write"`.
    #[serde(default = "default_rw_strategy")]
    pub strategy: String,

    /// Duration string: after a write, how long reads stick to the primary.
    ///
    /// Default: `"2s"`.
    #[serde(default = "default_sticky_duration")]
    pub sticky_duration: String,

    /// Planned: not yet implemented. Duration string for maximum acceptable
    /// replication lag (lag-aware strategy). Currently parsed but not acted upon.
    ///
    /// Default: `"500ms"`.
    #[serde(default = "default_max_replica_lag")]
    pub max_replica_lag: String,
}

impl Default for ReadWriteSplitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            strategy: default_rw_strategy(),
            sticky_duration: default_sticky_duration(),
            max_replica_lag: default_max_replica_lag(),
        }
    }
}

/// Query analysis and optimization configuration (`[db.analysis]`).
#[derive(Debug, Deserialize)]
pub struct DbAnalysisConfig {
    /// Enable query digest tracking and Prometheus metrics.
    ///
    /// When enabled, every SQL query is normalized, hashed, and tracked
    /// with timing, throughput, and error metrics. Disable to eliminate
    /// the per-query overhead on high-throughput workloads.
    ///
    /// Default: `true`.
    #[serde(default = "default_query_stats_enabled")]
    pub query_stats: bool,

    /// Duration threshold for logging slow queries.
    ///
    /// Queries exceeding this time trigger `EXPLAIN` analysis.
    ///
    /// Default: `"1s"`.
    #[serde(default = "default_slow_query_threshold")]
    pub slow_query_threshold: String,

    /// Planned: not yet implemented. Enable automatic `EXPLAIN` on slow queries.
    ///
    /// When enabled, the proxy will automatically run `EXPLAIN` on queries that
    /// exceed the slow query threshold. Currently parsed but not acted upon.
    ///
    /// Default: `false`.
    #[serde(default)]
    pub auto_explain: bool,

    /// Planned: not yet implemented. Output target for `EXPLAIN` analysis results.
    ///
    /// Values: `"stderr"`, `"stdout"`. Currently parsed but not acted upon.
    ///
    /// Default: `"stderr"`.
    #[serde(default = "default_auto_explain_target")]
    pub auto_explain_target: String,

    /// Maximum number of query digest entries to store in memory.
    ///
    /// Older entries are evicted when the limit is reached.
    ///
    /// Default: `100000`.
    #[serde(default = "default_digest_max_entries")]
    pub digest_store_max_entries: usize,

    /// Maximum number of distinct `digest` label values emitted to
    /// Prometheus. Digests beyond the cap fold into `digest="__other__"`,
    /// bounding metric cardinality. `0` = unlimited.
    ///
    /// Default: `1000`.
    #[serde(default = "default_metric_label_series_max")]
    pub metric_label_series_max: usize,
}

impl Default for DbAnalysisConfig {
    fn default() -> Self {
        Self {
            query_stats: default_query_stats_enabled(),
            slow_query_threshold: default_slow_query_threshold(),
            auto_explain: false,
            auto_explain_target: default_auto_explain_target(),
            digest_store_max_entries: default_digest_max_entries(),
            metric_label_series_max: default_metric_label_series_max(),
        }
    }
}

fn default_metric_label_series_max() -> usize {
    1000
}

fn default_query_stats_enabled() -> bool {
    true
}

/// KV store configuration (`[kv]`).
#[derive(Debug, Deserialize)]
pub struct KvConfig {
    /// Maximum memory in bytes for the KV store. Supports suffixes:
    /// plain number (bytes), or human-readable like `"256MB"`.
    ///
    /// Default: `"256MB"`.
    #[serde(default = "default_kv_memory_limit")]
    pub memory_limit: String,

    /// Eviction policy when the memory limit is reached.
    ///
    /// Values: `"noeviction"`, `"allkeys-lru"`, `"volatile-lru"`, `"allkeys-random"`.
    ///
    /// Default: `"allkeys-lru"`.
    #[serde(default = "default_kv_eviction_policy")]
    pub eviction_policy: String,

    /// Compression algorithm for stored values.
    ///
    /// Values: `"none"`, `"gzip"`, `"brotli"`, `"zstd"`.
    ///
    /// Default: `"none"` (no compression).
    #[serde(default = "default_kv_compression")]
    pub compression: String,

    /// Compression level (1 = fastest, 9 = best compression).
    ///
    /// Default: `6`.
    #[serde(default = "default_kv_compression_level")]
    pub compression_level: u32,

    /// Minimum value size in bytes before compression is applied.
    ///
    /// Values smaller than this threshold are stored uncompressed.
    /// Default: `1024` (1 KB).
    #[serde(default = "default_kv_compression_min_size")]
    pub compression_min_size: usize,

    /// Master secret for per-site RESP authentication. When set, per-site
    /// passwords are derived as `HMAC-SHA256(secret, hostname)`. ePHPm injects
    /// the derived password into PHP `$_ENV` as `EPHPM_REDIS_PASSWORD` for
    /// each request.
    ///
    /// If unset, per-site RESP AUTH is disabled: in multi-tenant (`sites_dir`)
    /// deployments with the RESP listener enabled, any client that can reach
    /// the listener can access the default store (a warning is logged at
    /// startup). In single-site (no `sites_dir`) mode, AUTH is not required.
    ///
    /// Default: `None`.
    #[serde(default)]
    pub secret: Option<String>,

    /// Redis-compatible RESP protocol listener.
    #[serde(default)]
    pub redis_compat: KvRedisCompatConfig,
}

impl Default for KvConfig {
    fn default() -> Self {
        Self {
            memory_limit: default_kv_memory_limit(),
            eviction_policy: default_kv_eviction_policy(),
            compression: default_kv_compression(),
            compression_level: default_kv_compression_level(),
            compression_min_size: default_kv_compression_min_size(),
            secret: None,
            redis_compat: KvRedisCompatConfig::default(),
        }
    }
}

/// RESP protocol listener configuration (`[kv.redis_compat]`).
///
/// **Security note for virtual hosting:** The RESP endpoint provides raw
/// access to the entire KV store — there is no per-tenant namespace
/// filtering. In multi-tenant (`sites_dir`) deployments, disable RESP
/// or restrict access to admin use only. PHP applications should use the
/// `ephpm_kv_*` SAPI functions instead, which are automatically namespaced
/// per virtual host.
#[derive(Debug, Deserialize)]
pub struct KvRedisCompatConfig {
    /// Enable the RESP protocol listener. When `false`, the KV store is
    /// only accessible via the `ephpm_kv_*` PHP functions (recommended
    /// for multi-tenant deployments).
    ///
    /// Default: `false`.
    #[serde(default)]
    pub enabled: bool,

    /// TCP listen address for the RESP listener.
    ///
    /// Default: `"127.0.0.1:6379"`.
    #[serde(default = "default_kv_listen")]
    pub listen: String,

    /// Planned: not yet implemented. Unix socket path for the RESP listener
    /// (faster than TCP for local connections). Currently parsed but not acted upon.
    #[serde(default)]
    pub socket: Option<String>,

    /// Optional password required for RESP AUTH. When set, clients must send
    /// `AUTH <password>` before any commands are accepted. Equivalent to
    /// Redis `requirepass`.
    ///
    /// Default: `None` (no authentication required).
    #[serde(default)]
    pub password: Option<String>,

    /// Maximum concurrent RESP connections. Excess clients are refused
    /// with `ERR max number of clients reached` (like Redis `maxclients`).
    /// `0` = unlimited.
    ///
    /// Default: `1000`.
    #[serde(default = "default_kv_max_connections")]
    pub max_connections: usize,

    /// Maximum RESP input buffer per connection, in bytes (like Redis'
    /// `client-query-buffer-limit`). This memory is per connection and is
    /// NOT counted against `[kv] memory_limit`.
    ///
    /// Default: `1048576` (1 MiB).
    #[serde(default = "default_kv_max_input_buffer")]
    pub max_input_buffer: usize,

    /// Idle timeout in seconds for RESP connections; silent connections
    /// are closed and their buffers freed. `0` = no timeout.
    ///
    /// Default: `300`.
    #[serde(default = "default_kv_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
}

fn default_kv_max_connections() -> usize {
    1000
}

fn default_kv_max_input_buffer() -> usize {
    1024 * 1024
}

fn default_kv_idle_timeout_secs() -> u64 {
    300
}

impl Default for KvRedisCompatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: default_kv_listen(),
            socket: None,
            password: None,
            max_connections: default_kv_max_connections(),
            max_input_buffer: default_kv_max_input_buffer(),
            idle_timeout_secs: default_kv_idle_timeout_secs(),
        }
    }
}

/// PHP runtime configuration.
#[derive(Debug, Deserialize)]
pub struct PhpConfig {
    /// Maximum execution time in seconds for a single PHP request.
    #[serde(default = "default_max_execution_time")]
    pub max_execution_time: u32,

    /// Memory limit for PHP (e.g. "128M").
    #[serde(default = "default_memory_limit")]
    pub memory_limit: String,

    /// Override OPcache timestamp validation (`opcache.validate_timestamps`).
    ///
    /// When PHP has OPcache loaded, `validate_timestamps` controls whether the
    /// engine `stat()`s each cached script on (re)use to detect edits. ePHPm
    /// picks a mode-appropriate default when this knob is left unset:
    ///
    /// - `ephpm serve` (production): **`false`** — trust the cache. Code changes
    ///   go live via `ephpm deploy` / `ephpm cache reset`, which invalidate
    ///   OPcache through the RESP listener (deploys-are-events). This avoids a
    ///   `stat()` per cached file every `revalidate_freq` seconds and yields a
    ///   deterministic "code changes only on a deploy" contract.
    /// - `ephpm dev` (bare `ephpm` / `ephpm dev`): **`true`** — instant
    ///   edit-refresh so the dev loop stays tight. Never overridden by the
    ///   serve-mode default.
    ///
    /// Set explicitly to force a value in *either* mode: `true` re-enables
    /// stat-on-use under `serve` (e.g. a bind-mounted docroot that changes
    /// without a deploy), `false` freezes the cache under `dev`.
    ///
    /// **Serve mode + `false` requires an invalidation lever.** If the RESP
    /// listener is disabled (`[kv.redis_compat] enabled = false`) there is no
    /// way for `ephpm deploy` / `ephpm cache reset` to reach the running
    /// server, so cached code can never be refreshed without a restart. Startup
    /// logs a WARN in that case.
    ///
    /// Only takes effect when OPcache is actually loaded (it is in the release
    /// build). With no OPcache extension, the directive is inert.
    ///
    /// Default: `None` (mode-appropriate: off under `serve`, on under `dev`).
    #[serde(default)]
    pub opcache_validate_timestamps: Option<bool>,

    /// Override OPcache revalidation frequency in seconds
    /// (`opcache.revalidate_freq`).
    ///
    /// Only meaningful when timestamp validation is on (see
    /// `opcache_validate_timestamps`). Bounds how often the engine re-`stat()`s
    /// a given cached script: at most once per this many seconds. PHP's own
    /// default is `2`. Raising it (e.g. `60`) cuts `stat()` traffic on
    /// container/overlay/network filesystems at the cost of picking up edits
    /// more slowly.
    ///
    /// Ignored when validation is off (nothing is re-stat'd).
    ///
    /// Default: `None` (PHP's built-in default of `2` applies).
    #[serde(default)]
    pub opcache_revalidate_freq: Option<u32>,

    /// Override `opcache.memory_consumption` (MB of shared opcode cache).
    ///
    /// When unset, ePHPm **auto-derives** this from the detected memory budget
    /// (container cgroup limit, else host `MemTotal`): ~18% of memory, clamped
    /// to `[64, 512]` MB. Set explicitly to pin the SHM size regardless of the
    /// detected budget. Only takes effect in serve mode (dev keeps PHP's
    /// default of 128 MB unless you set this).
    ///
    /// Default: `None` (auto-derived in serve, PHP default in dev).
    #[serde(default)]
    pub opcache_memory_consumption: Option<u32>,

    /// Override `opcache.interned_strings_buffer` (MB for interned strings).
    ///
    /// When unset, ePHPm auto-derives it to scale with the opcache SHM size:
    /// ~1 MB per 16 MB of `opcache.memory_consumption`, clamped to `[8, 64]`
    /// MB. Set explicitly to pin it. Serve mode only (dev keeps the PHP
    /// default).
    ///
    /// Default: `None` (auto-derived in serve, PHP default in dev).
    #[serde(default)]
    pub opcache_interned_strings_buffer: Option<u32>,

    /// Override `opcache.jit_buffer_size` (MB reserved for the JIT).
    ///
    /// When unset, ePHPm auto-derives a buffer size (~1/64 of the memory
    /// budget, clamped `[32, 64]` MB) and emits it in serve mode. **This does
    /// NOT enable JIT.** `opcache.jit` is left at PHP's default (opt-in via
    /// `ini_overrides`): JIT helps CPU-bound workloads but can regress the
    /// I/O-bound request path typical of web apps, so auto-enabling it is a
    /// separate benched decision. The buffer is merely pre-sized so enabling
    /// JIT later needs no config change.
    ///
    /// Default: `None` (auto-derived buffer in serve; JIT still off).
    #[serde(default)]
    pub opcache_jit_buffer_size: Option<u32>,

    /// Override `opcache.max_accelerated_files` (cap on cached script slots).
    ///
    /// When unset, ePHPm uses a generous **fixed** default of `20000` in serve
    /// mode. This is deliberately NOT derived from memory: the right value is
    /// shaped by how many `.php` files the *application* has, not by the
    /// machine size. 20000 comfortably covers large frameworks (Laravel /
    /// WordPress + plugins) while PHP rounds it up to the next prime internally.
    ///
    /// Default: `None` (fixed 20000 in serve, PHP default in dev).
    #[serde(default)]
    pub opcache_max_accelerated_files: Option<u32>,

    /// Override the derived per-request `memory_limit` (e.g. `"192M"`).
    ///
    /// Takes precedence over the legacy [`Self::memory_limit`] field **and**
    /// over the auto-derived value. When unset, ePHPm derives a per-request
    /// limit in serve mode from `(memory_budget − opcache_shm − ~64 MB
    /// overhead) / worker_count`, clamped to a `128 MB` floor; with no
    /// detectable memory budget it keeps PHP's `128M` default rather than
    /// inventing a huge number. Dev mode keeps [`Self::memory_limit`].
    ///
    /// Default: `None` (auto-derived in serve, `memory_limit` in dev).
    #[serde(default)]
    pub php_memory_limit: Option<String>,

    /// Override `realpath_cache_size` (e.g. `"16M"`).
    ///
    /// When unset, serve mode uses `16M` (up from PHP's stingy `256K`) to cut
    /// `realpath()`/`stat()` traffic on deep framework autoload trees; dev mode
    /// keeps the PHP default so freshly-created files resolve immediately. Set
    /// explicitly to pin it in either mode.
    ///
    /// Default: `None` (`16M` in serve, PHP default in dev).
    #[serde(default)]
    pub realpath_cache_size: Option<String>,

    /// Override `realpath_cache_ttl` in seconds.
    ///
    /// When unset, serve mode uses `600` (vs PHP's `120`) so realpath entries
    /// live longer between deploys; dev mode keeps the PHP default. Set
    /// explicitly to pin it.
    ///
    /// Default: `None` (`600` in serve, PHP default in dev).
    #[serde(default)]
    pub realpath_cache_ttl: Option<u32>,

    /// Override `zend.assertions`.
    ///
    /// When unset, serve mode uses `-1` (assertions compiled out — zero
    /// runtime cost, the production-recommended value) and dev mode uses `1`
    /// (assertions active). Set explicitly (`-1`, `0`, or `1`) to pin it.
    ///
    /// Default: `None` (`-1` in serve, `1` in dev).
    #[serde(default)]
    pub zend_assertions: Option<i8>,

    /// Optional path to a custom php.ini file.
    ///
    /// When set, ePHPm reads this file for PHP configuration before applying
    /// `ini_overrides`. This allows reusing an existing php.ini from your
    /// PHP installation or custom configuration.
    ///
    /// If not set, PHP uses its default ini locations (or none if not found).
    ///
    /// Default: `None` (no custom ini file).
    #[serde(default)]
    pub ini_file: Option<PathBuf>,

    /// INI directive overrides as `[key, value]` pairs.
    ///
    /// Applied after `ini_file` is loaded (if specified), so these take
    /// precedence over `ini_file` settings.
    #[serde(default)]
    pub ini_overrides: Vec<[String; 2]>,

    /// Shared PHP extensions to load at startup.
    ///
    /// Each entry is either a bare extension name (`"redis"`, `"imagick"`)
    /// or an absolute/relative path to a shared object. Bare names are
    /// emitted as `extension=<name>` so PHP's own `extension_dir` search
    /// resolves them; paths are emitted as `extension=<path>` verbatim.
    /// The lines are written into the generated php.ini *before* `ini_file`
    /// and `ini_overrides`, so those can still tune the extension's own ini
    /// settings.
    ///
    /// The extension binary must match the embedded PHP's ABI: same PHP
    /// minor version, same thread-safety mode (ZTS on Linux/macOS, NTS on
    /// Windows), and — on Linux — glibc (the release binary is
    /// glibc-dynamic). PHP verifies this at startup and rejects a mismatch
    /// with a clear "Unable to load dynamic library" error instead of
    /// crashing (verified: an NTS build fails with `undefined symbol:
    /// compiler_globals`). Note that Debian/Sury `php8.5-<ext>` packages
    /// are NTS-only (no `-zts` variants exist as of 2026-07), so on Linux a
    /// shared extension must currently be compiled for ZTS — e.g. `phpize`
    /// against a ZTS PHP of the same minor, or `gcc -shared` against the
    /// matching php-sdk headers. Windows (NTS `.dll`) and macOS (ZTS
    /// `.dylib`) work the same way via their dynamically-capable release
    /// binaries.
    ///
    /// Empty entries fail validation (`validate()`): PHP would silently
    /// ignore a bare `extension=` line, which would make the knob a silent
    /// no-op.
    ///
    /// Default: empty (only the ~45 statically linked extensions).
    #[serde(default)]
    pub extensions: Vec<String>,

    /// Maximum number of PHP requests that may execute concurrently.
    ///
    /// Equivalent to php-fpm's `pm.max_children`: requests beyond the cap
    /// queue until a slot frees up (still subject to the request timeout).
    /// Enforced with a semaphore around PHP execution — tokio's blocking
    /// pool itself is never capped, so static file serving and other
    /// blocking work cannot be starved by slow PHP scripts.
    ///
    /// `0` means unlimited (bounded only by tokio's blocking pool).
    ///
    /// Default: `0` (unlimited).
    ///
    /// **Ignored in worker mode** (`mode = "worker"`): concurrency is bounded
    /// by `worker_count` (parked threads) and `worker_backlog` (queue depth),
    /// not this semaphore. Startup logs a WARN if `workers > 0` under worker
    /// mode so the no-op is never silent.
    #[serde(default = "default_php_workers")]
    pub workers: usize,

    /// Request-execution model.
    ///
    /// - `"fpm"` (default) — php-fpm-shaped: each HTTP request runs a full
    ///   `php_request_startup`/`shutdown` cycle, so framework state never
    ///   leaks across requests. Behavior is byte-for-byte identical to
    ///   releases before worker mode existed.
    /// - `"worker"` — persistent worker mode (Octane/RoadRunner model): a
    ///   fixed pool of OS threads each boot the framework **once** via
    ///   `worker_script`, then loop over requests without re-bootstrapping.
    ///   5-20x throughput for heavy frameworks. Requires `worker_script`.
    ///
    /// Whole-server switch (not per-path). See `worker_*` fields below.
    ///
    /// Default: `"fpm"`.
    #[serde(default = "default_php_mode")]
    pub mode: String,

    /// Worker-mode entrypoint script, relative to `document_root`.
    ///
    /// The script is a loop that calls `\Ephpm\Worker\take_request()` /
    /// `\Ephpm\Worker\send_response()`. Real framework adapters (Octane,
    /// PSR-15) ship this; `examples/worker/worker.php` is the reference.
    ///
    /// **Required** when `mode = "worker"` — config load hard-errors if it is
    /// absent or does not resolve to a file under `document_root`. Ignored in
    /// fpm mode.
    ///
    /// Default: `None`.
    #[serde(default)]
    pub worker_script: Option<PathBuf>,

    /// Number of persistent worker threads (worker mode only).
    ///
    /// Each worker is a permanently-parked OS thread holding a fully-booted
    /// framework in memory, so — unlike `workers` — worker mode picks a
    /// concrete count. `0` derives it from the CPU count, clamped to
    /// `[2, 32]`. Heavy frameworks (WordPress ~40MB/worker) may want it lower.
    ///
    /// On NTS builds (Windows) this is forced to `1` with a WARN — there is a
    /// single PHP context, so requests serialize through one booted framework.
    ///
    /// Ignored in fpm mode.
    ///
    /// Default: `0` — derive from the cgroup CPU quota when running under one
    /// (`cpu.max` on cgroup v2, `cpu.cfs_quota_us`/`cpu.cfs_period_us` on v1;
    /// Linux only), otherwise from host parallelism clamped to `[2, 32]`. The
    /// quota-aware path is the sweet spot inside CPU-limited containers, where
    /// the host-parallelism derivation overshoots (measured 2026-07-09: at a
    /// 0.25-CPU quota, 1 worker beat the derived 2 by ~24% on hello c=16).
    #[serde(default = "default_worker_count")]
    pub worker_count: usize,

    /// Recycle a worker after it has handled this many requests (worker mode
    /// only). The worker's `take_request()` returns null on the next call, the
    /// framework loop exits, and the pool respawns a fresh worker with a clean
    /// boot — reclaiming any slow memory growth in the framework's own state
    /// (php-fpm `pm.max_requests` semantics).
    ///
    /// `0` disables recycling (never recycle on request count).
    ///
    /// Ignored in fpm mode.
    ///
    /// Default: `10000`. A pure leak guard — for a leak-free framework loop,
    /// recycling adds overhead (framework reboot) without any benefit. Raised
    /// from `500` (2026-07-09 roadmap): at 2,000 rps the old default recycled
    /// every ~0.25 s. Each recycle is logged at debug (worker id, requests
    /// served, uptime) so its frequency is visible.
    #[serde(default = "default_worker_max_requests")]
    pub worker_max_requests: u64,

    /// Dispatch-queue depth for handing requests to workers (worker mode
    /// only). When the queue is full, the HTTP handler suspends (backpressure)
    /// until a worker frees up, still bounded by the request timeout (504).
    ///
    /// `0` derives the depth from `worker_count` (one queued job per worker).
    ///
    /// Ignored in fpm mode.
    ///
    /// Default: `0` (= `worker_count`).
    #[serde(default = "default_worker_backlog")]
    pub worker_backlog: usize,

    /// Seconds a worker gets to boot the framework and reach its first
    /// `take_request()` (worker mode only). A worker still booting when this
    /// window expires is logged as an error and counted in
    /// `ephpm_worker_boot_timeouts_total`. The thread is NOT killed — a PHP
    /// thread cannot be terminated safely — and it still becomes ready if the
    /// boot eventually completes. A worker whose boot *fails* (the script
    /// exits before its first `take_request()`) is counted as a boot failure
    /// and respawned with exponential backoff, independent of this timeout.
    ///
    /// Ignored in fpm mode.
    ///
    /// Default: `30`.
    #[serde(default = "default_worker_boot_timeout")]
    pub worker_boot_timeout: u64,

    /// Populate native PHP superglobals (`$_GET`/`$_POST`/`$_SERVER`/...) per
    /// request in worker mode (worker mode only).
    ///
    /// Off by default: Octane/PSR-15 adapters build their own request object
    /// from the `Envelope` and never touch superglobals. Turn this on for the
    /// WordPress adapter, which assumes real superglobals.
    ///
    /// Ignored in fpm mode (fpm always builds superglobals natively).
    ///
    /// Default: `false`.
    #[serde(default)]
    pub worker_populate_superglobals: bool,

    /// Request-body size (bytes) at or above which the body is *streamed* into
    /// the worker in fixed-size chunks instead of buffered whole (worker mode
    /// only, Phase 3). Requests with a `Content-Length` at or above this — or
    /// with no `Content-Length` (chunked) — flow through
    /// `Envelope::bodyStream()` / PHP's POST reader without ePHPm holding the
    /// whole body in memory, keeping worker RSS flat for multi-GB uploads.
    ///
    /// Smaller requests stay on the buffered Phase-1 path (one copy each way),
    /// which is cheaper for the common small-body case.
    ///
    /// Ignored in fpm mode (the fpm path always buffers the body today).
    ///
    /// Default: `1048576` (1 MiB).
    #[serde(default = "default_worker_stream_threshold")]
    pub worker_stream_threshold: u64,
}

impl Config {
    /// Load configuration from a TOML file with environment variable overrides.
    ///
    /// Precedence (highest to lowest):
    /// 1. Environment variables prefixed with `EPHPM_` (e.g. `EPHPM_SERVER_LISTEN`)
    /// 2. TOML config file
    /// 3. Built-in defaults
    /// # Errors
    ///
    /// Returns `ConfigError::Load` if the TOML file cannot be read or parsed.
    pub fn load(path: &PathBuf) -> Result<Self, ConfigError> {
        let config = Figment::new()
            .merge(Toml::file(path))
            .merge(Env::prefixed("EPHPM_").split("__"))
            .extract()
            .map_err(Box::new)?;
        Ok(config)
    }

    /// Load configuration with defaults only (no file).
    ///
    /// # Errors
    ///
    /// Returns `ConfigError::Load` if environment variables contain invalid values.
    pub fn default_config() -> Result<Self, ConfigError> {
        let config = Figment::new()
            .merge(Env::prefixed("EPHPM_").split("__"))
            .extract()
            .map_err(Box::new)?;
        Ok(config)
    }

    /// Validate cross-field invariants that serde cannot express.
    ///
    /// Called after CLI overrides are applied (so `document_root` is final)
    /// and before the runtime starts, so misconfiguration fails fast with a
    /// clear message rather than a confusing runtime error.
    ///
    /// Worker-mode rules (see `worker-mode-design.md` §4.3):
    /// - `mode = "worker"` requires a `worker_script` that resolves to a file
    ///   under `document_root`.
    /// - `mode = "worker"` with `sites_dir` set is a Phase-1-unsupported
    ///   combination (per-host worker pools are a later phase) — hard error.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] if any invariant is violated.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // Reject unknown modes outright: a typo like "workr" would otherwise
        // silently mean fpm (the no-silent-knob rule).
        if self.php.mode != "fpm" && self.php.mode != "worker" {
            return Err(ConfigError::Validation(format!(
                "[php] mode must be \"fpm\" or \"worker\", got \"{}\"",
                self.php.mode,
            )));
        }

        if self.php.is_worker_mode() {
            if self.server.sites_dir.is_some() {
                return Err(ConfigError::Validation(
                    "[php] mode = \"worker\" is not supported together with \
                     [server] sites_dir (multi-tenant vhosting). Worker mode \
                     boots one framework per worker; per-host worker pools are \
                     a future phase. Use fpm mode for multi-tenant deployments."
                        .to_string(),
                ));
            }

            // worker_script is required and must resolve under document_root.
            self.resolve_worker_script()?;
        }

        // [php] extensions: an empty entry can never load anything, and PHP
        // silently ignores a bare `extension=` line — rejecting it here
        // keeps the knob from being a silent no-op.
        for (i, ext) in self.php.extensions.iter().enumerate() {
            if ext.trim().is_empty() {
                return Err(ConfigError::Validation(format!(
                    "[php] extensions entry {i} is empty — use a bare extension \
                     name (e.g. \"redis\") or a path to a shared object",
                )));
            }
            // The generated php.ini writes `extension={ext}` verbatim, so a
            // newline, carriage return, or NUL in an entry would inject a
            // second arbitrary ini directive. Reject them outright.
            if ext.contains(['\n', '\r', '\0']) {
                return Err(ConfigError::Validation(format!(
                    "[php] extensions entry {i} contains a newline, carriage \
                     return, or NUL — such an entry could inject an arbitrary \
                     ini directive into the generated php.ini",
                )));
            }
        }

        // Native middleware: an empty `library` can never resolve, and
        // silently skipping the mount would be a silent no-op config knob.
        for (i, mount) in self.middleware.iter().enumerate() {
            if mount.library.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "[[middleware]] entry {i} (order = {}): library must not be empty",
                    mount.order,
                )));
            }
        }
        Ok(())
    }

    /// Resolve the worker entrypoint to an absolute path under
    /// `document_root`, validating that it exists and does not escape the root.
    ///
    /// The script may be given as a path relative to `document_root`
    /// (`"worker.php"`) or as an absolute path that still lies under the root.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] when `worker_script` is absent, does
    /// not resolve to an existing file, or resolves outside `document_root`.
    pub fn resolve_worker_script(&self) -> Result<PathBuf, ConfigError> {
        let Some(script) = self.php.worker_script.as_ref() else {
            return Err(ConfigError::Validation(
                "[php] mode = \"worker\" requires [php] worker_script (the \
                 entrypoint loop, relative to document_root)"
                    .to_string(),
            ));
        };

        let doc_root = &self.server.document_root;
        let candidate = if script.is_absolute() { script.clone() } else { doc_root.join(script) };

        // Canonicalize both so `..` segments and symlinks can't be used to
        // escape the document root. If canonicalization fails the file almost
        // certainly does not exist — surface a clear "not found" error.
        let canon_script = candidate.canonicalize().map_err(|e| {
            ConfigError::Validation(format!(
                "[php] worker_script {} does not resolve to an existing file \
                 (looked under document_root {}): {e}",
                script.display(),
                doc_root.display(),
            ))
        })?;

        if !canon_script.is_file() {
            return Err(ConfigError::Validation(format!(
                "[php] worker_script {} is not a regular file",
                canon_script.display(),
            )));
        }

        // Enforce containment under document_root when the root itself exists.
        if let Ok(canon_root) = doc_root.canonicalize() {
            if !canon_script.starts_with(&canon_root) {
                return Err(ConfigError::Validation(format!(
                    "[php] worker_script {} resolves outside document_root {}",
                    canon_script.display(),
                    canon_root.display(),
                )));
            }
        }

        Ok(canon_script)
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            document_root: default_document_root(),
            sites_dir: None,
            sites_domain_suffix: None,
            index_files: default_index_files(),
            fallback: default_fallback(),
            request: RequestConfig::default(),
            timeouts: TimeoutsConfig::default(),
            response: ResponseConfig::default(),
            static_files: StaticConfig::default(),
            php_etag_cache: PhpETagCacheConfig::default(),
            security: None,
            logging: LoggingConfig::default(),
            metrics: MetricsConfig::default(),
            limits: LimitsConfig::default(),
            file_cache: FileCacheConfig::default(),
            tls: None,
        }
    }
}

impl Default for RequestConfig {
    fn default() -> Self {
        Self {
            max_body_size: default_max_body_size(),
            max_header_size: default_max_header_size(),
            trusted_hosts: Vec::new(),
        }
    }
}

impl Default for TimeoutsConfig {
    fn default() -> Self {
        Self {
            header_read: default_header_read(),
            idle: default_idle(),
            request: default_request_timeout(),
            shutdown: default_shutdown_timeout(),
        }
    }
}

impl Default for ResponseConfig {
    fn default() -> Self {
        Self {
            compression: default_compression(),
            compression_level: default_compression_level(),
            compression_min_size: default_compression_min_size(),
            compression_streaming: default_compression_streaming(),
            headers: Vec::new(),
        }
    }
}

impl Default for StaticConfig {
    fn default() -> Self {
        Self {
            cache_control: String::new(),
            hidden_files: default_hidden_files(),
            etag: default_etag(),
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self { access: String::new(), level: default_log_level() }
    }
}

impl Default for PhpConfig {
    fn default() -> Self {
        Self {
            max_execution_time: default_max_execution_time(),
            memory_limit: default_memory_limit(),
            opcache_validate_timestamps: None,
            opcache_revalidate_freq: None,
            opcache_memory_consumption: None,
            opcache_interned_strings_buffer: None,
            opcache_jit_buffer_size: None,
            opcache_max_accelerated_files: None,
            php_memory_limit: None,
            realpath_cache_size: None,
            realpath_cache_ttl: None,
            zend_assertions: None,
            ini_file: None,
            ini_overrides: Vec::new(),
            extensions: Vec::new(),
            workers: default_php_workers(),
            mode: default_php_mode(),
            worker_script: None,
            worker_count: default_worker_count(),
            worker_max_requests: default_worker_max_requests(),
            worker_backlog: default_worker_backlog(),
            worker_boot_timeout: default_worker_boot_timeout(),
            worker_populate_superglobals: false,
            worker_stream_threshold: default_worker_stream_threshold(),
        }
    }
}

impl PhpConfig {
    /// Whether persistent worker mode is requested (`mode = "worker"`).
    ///
    /// Case-insensitive so `"Worker"` / `"WORKER"` also match.
    #[must_use]
    pub fn is_worker_mode(&self) -> bool {
        self.mode.eq_ignore_ascii_case("worker")
    }

    /// Resolve the effective worker-thread count.
    ///
    /// Returns the configured `worker_count`, or — when it is `0` — a value
    /// derived from the cgroup CPU quota (Linux, when present) or otherwise
    /// from host parallelism clamped to `[2, 32]`. Never returns `0`. See
    /// [`Self::effective_worker_count_with_source`] to also learn *why* a
    /// given value was picked (for logging at pool startup).
    #[must_use]
    pub fn effective_worker_count(&self) -> usize {
        self.effective_worker_count_with_source().0
    }

    /// Same as [`Self::effective_worker_count`] but also reports the source of
    /// the derivation so the worker pool can log why it picked N threads.
    #[must_use]
    pub fn effective_worker_count_with_source(&self) -> (usize, WorkerCountSource) {
        if self.worker_count > 0 {
            return (self.worker_count, WorkerCountSource::Explicit);
        }
        if let Some(quota_cpus) = read_cgroup_cpu_quota() {
            // Round up so a 0.25 quota gives 1 worker, a 1.5 quota gives 2.
            // ceil().max(1.0) is always >= 1.0 and bounded by the small quotas
            // real containers use, so the cast is sign- and range-safe.
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            let n = quota_cpus.ceil().max(1.0) as usize;
            return (n, WorkerCountSource::CgroupQuota { quota_cpus });
        }
        let cpus = std::thread::available_parallelism().map_or(2, std::num::NonZeroUsize::get);
        (cpus.clamp(2, 32), WorkerCountSource::HostParallelism { cpus })
    }

    /// Resolve the effective dispatch-queue depth.
    ///
    /// Returns `worker_backlog`, or the effective worker count when it is `0`.
    /// Always at least `1`.
    #[must_use]
    pub fn effective_worker_backlog(&self) -> usize {
        if self.worker_backlog > 0 { self.worker_backlog } else { self.effective_worker_count() }
    }

    /// Resolve the effective `opcache.validate_timestamps` value for a run.
    ///
    /// `dev_mode` is `true` under `ephpm dev` / bare `ephpm` and `false` under
    /// `ephpm serve`. When `opcache_validate_timestamps` is set explicitly it
    /// wins in either mode; otherwise the mode default applies: `true` (on) for
    /// dev, `false` (off) for serve.
    #[must_use]
    pub fn effective_validate_timestamps(&self, dev_mode: bool) -> bool {
        self.opcache_validate_timestamps.unwrap_or(dev_mode)
    }

    /// Resolve the effective per-request PHP `memory_limit` string.
    ///
    /// Precedence: explicit [`Self::php_memory_limit`] → derived (serve only,
    /// when a memory budget is detectable) → the legacy [`Self::memory_limit`]
    /// field (which itself defaults to `128M`). See [`Self::derive_tuning`] for
    /// the derivation.
    #[must_use]
    pub fn effective_memory_limit(&self, dev_mode: bool) -> String {
        self.autotune(dev_mode).memory_limit.value
    }

    /// Compute the full resource-aware tuning profile for this run.
    ///
    /// Detects the CPU quota and memory budget (cgroup-aware, falling back to
    /// host totals), then resolves every tunable through the three-tier
    /// precedence: **explicit `[php]` config → auto-derived → PHP stock
    /// default**. The returned [`AutoTune`] records each value *and* where it
    /// came from so the caller can both emit ini lines and log a transparent
    /// summary.
    ///
    /// `dev_mode` selects the profile family: serve mode derives production
    /// values from the detected resources; dev mode keeps PHP-friendly defaults
    /// (timestamp validation on, assertions on, loose realpath) so the
    /// edit-refresh loop stays tight. Explicit config still wins in either mode.
    #[must_use]
    pub fn autotune(&self, dev_mode: bool) -> AutoTune {
        let (mem_budget, mem_source) = detect_memory_budget();
        let cpu_quota = read_cgroup_cpu_quota();
        let (workers, worker_source) = self.effective_worker_count_with_source();
        let derived = derive_tuning(cpu_quota, mem_budget, workers, dev_mode);

        // Three-tier resolution helper: explicit config wins, then the derived
        // value (if serve mode produced one), then the PHP stock default.
        fn resolve<T: Clone>(explicit: Option<T>, derived: Option<T>, default: T) -> TunedValue<T> {
            match (explicit, derived) {
                (Some(v), _) => TunedValue { value: v, origin: Origin::Explicit },
                (None, Some(v)) => TunedValue { value: v, origin: Origin::Derived },
                (None, None) => TunedValue { value: default, origin: Origin::Default },
            }
        }

        // validate_timestamps is a bool that always resolves (mode default),
        // so its "default" is the mode-appropriate value and any explicit knob
        // wins — track origin accordingly.
        let validate = TunedValue {
            value: self.effective_validate_timestamps(dev_mode),
            origin: if self.opcache_validate_timestamps.is_some() {
                Origin::Explicit
            } else {
                Origin::Derived
            },
        };

        AutoTune {
            cpu_quota,
            mem_budget,
            mem_source,
            workers,
            worker_source,
            dev_mode,
            validate_timestamps: validate,
            revalidate_freq: self
                .opcache_revalidate_freq
                .map(|f| TunedValue { value: f, origin: Origin::Explicit }),
            memory_consumption: resolve(
                self.opcache_memory_consumption,
                derived.opcache_memory_consumption,
                // PHP stock opcache.memory_consumption default is 128 MB.
                128,
            ),
            interned_strings_buffer: resolve(
                self.opcache_interned_strings_buffer,
                derived.opcache_interned_strings_buffer,
                8,
            ),
            jit_buffer_size: resolve(
                self.opcache_jit_buffer_size,
                derived.opcache_jit_buffer_size,
                0,
            ),
            max_accelerated_files: resolve(
                self.opcache_max_accelerated_files,
                derived.opcache_max_accelerated_files,
                10_000,
            ),
            memory_limit: resolve(
                self.php_memory_limit.clone(),
                derived.memory_limit.clone(),
                self.memory_limit.clone(),
            ),
            realpath_cache_size: resolve(
                self.realpath_cache_size.clone(),
                derived.realpath_cache_size.clone(),
                "256K".to_string(),
            ),
            realpath_cache_ttl: resolve(self.realpath_cache_ttl, derived.realpath_cache_ttl, 120),
            zend_assertions: resolve(self.zend_assertions, derived.zend_assertions, 1),
        }
    }

    /// OPcache/engine ini directives to write into the generated php.ini.
    ///
    /// Layers the full resource-aware autotuning profile (see
    /// [`Self::autotune`]): `opcache.validate_timestamps` (always), plus every
    /// tunable whose value came from explicit config **or** a serve-mode
    /// derivation. Values that resolved to the PHP stock default are omitted so
    /// the engine's own default applies (keeping dev mode's php.ini minimal).
    /// All lines are emitted *before* user `ini_overrides`, so an operator can
    /// still override any of them through `ini_overrides` as the final lever.
    #[must_use]
    pub fn opcache_ini_lines(&self, dev_mode: bool) -> Vec<(String, String)> {
        self.autotune(dev_mode).ini_lines()
    }
}

/// Where a resolved tunable's value came from — surfaced in the autotune log
/// so operators can see which values they pinned vs which ePHPm derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The operator set the `[php]` field explicitly — it wins over derivation.
    Explicit,
    /// ePHPm derived the value from detected CPU/memory (serve mode).
    Derived,
    /// Neither explicit nor derived — PHP's stock default applies (the line is
    /// omitted from the generated ini so the engine's own default takes hold).
    Default,
}

impl Origin {
    /// Single-char marker for the compact autotune log line
    /// (`*` = explicit/pinned, otherwise blank).
    #[must_use]
    fn marker(self) -> &'static str {
        match self {
            Self::Explicit => "*",
            Self::Derived | Self::Default => "",
        }
    }
}

/// A resolved tunable: its effective value plus where that value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TunedValue<T> {
    /// The effective value used for this run.
    pub value: T,
    /// Whether it was pinned by config, derived, or left at the PHP default.
    pub origin: Origin,
}

/// The raw derived values (serve mode only). Every field is `None` in dev mode
/// or when the required resource (memory budget) could not be detected — the
/// three-tier resolver then falls through to the PHP stock default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DerivedTuning {
    /// Derived `opcache.memory_consumption` (MB).
    pub opcache_memory_consumption: Option<u32>,
    /// Derived `opcache.interned_strings_buffer` (MB).
    pub opcache_interned_strings_buffer: Option<u32>,
    /// Derived `opcache.jit_buffer_size` (MB). JIT itself stays off.
    pub opcache_jit_buffer_size: Option<u32>,
    /// Derived `opcache.max_accelerated_files` (fixed, not resource-shaped).
    pub opcache_max_accelerated_files: Option<u32>,
    /// Derived per-request `memory_limit` (e.g. `"192M"`).
    pub memory_limit: Option<String>,
    /// Derived `realpath_cache_size` (e.g. `"16M"`).
    pub realpath_cache_size: Option<String>,
    /// Derived `realpath_cache_ttl` (seconds).
    pub realpath_cache_ttl: Option<u32>,
    /// Derived `zend.assertions` (`-1` in serve).
    pub zend_assertions: Option<i8>,
}

/// One mebibyte in bytes.
const MIB: u64 = 1024 * 1024;

/// Derive the resource-aware serve-mode tuning profile from the detected CPU
/// quota, memory budget, and effective worker count.
///
/// Returns an all-`None` [`DerivedTuning`] in **dev mode** (dev keeps
/// PHP-friendly defaults so edits refresh instantly and assertions stay on).
/// In serve mode:
///
/// - `opcache.memory_consumption` = ~18% of the memory budget, clamped
///   `[64, 512]` MB. (Always derived in serve, even with no memory budget:
///   the floor gives a sane 64 MB.)
/// - `opcache.interned_strings_buffer` = ~1 MB per 16 MB of opcache SHM,
///   clamped `[8, 64]` MB.
/// - `opcache.jit_buffer_size` = ~1/64 of the memory budget, clamped
///   `[32, 64]` MB (buffer only — JIT is **not** auto-enabled).
/// - `opcache.max_accelerated_files` = a generous fixed `20000` (app-file-count
///   shaped, not resource-shaped — see the field doc).
/// - `memory_limit` = `(budget − opcache_shm − 64 MB overhead) / workers`,
///   floored at `128 MB`. With no detectable memory budget it stays `None`
///   (keep PHP's `128M`) rather than inventing a huge number.
/// - `realpath_cache_size` = `16M`; `realpath_cache_ttl` = `600`.
/// - `zend.assertions` = `-1` (compiled out).
#[must_use]
pub fn derive_tuning(
    cpu_quota: Option<f64>,
    mem_bytes: Option<u64>,
    workers: usize,
    dev_mode: bool,
) -> DerivedTuning {
    // CPU quota is detected and logged, but no serve tunable is CPU-shaped
    // today (JIT stays off, and worker_count already consumes the quota). Bind
    // it so the signature documents the input and a future CPU-shaped knob has
    // it to hand.
    let _ = cpu_quota;

    if dev_mode {
        // Dev keeps PHP-friendly defaults across the board.
        return DerivedTuning::default();
    }

    // opcache.memory_consumption: ~18% of the budget, clamped [64, 512] MB.
    // With no detectable budget, the floor (64 MB) still gives a sane serve
    // value — opcache SHM is fixed-size and cheap relative to a modern host.
    let opcache_mb: u32 = {
        let by_ratio = mem_bytes.map_or(64, |b| (b * 18 / 100) / MIB) as u32;
        by_ratio.clamp(64, 512)
    };

    // interned_strings_buffer: ~1 MB per 16 MB of opcache SHM, clamped [8, 64].
    let interned_mb: u32 = (opcache_mb / 16).clamp(8, 64);

    // jit_buffer_size: ~1/64 of the budget, clamped [32, 64] MB. Buffer only.
    let jit_mb: u32 = {
        let by_ratio = mem_bytes.map_or(32, |b| (b / 64) / MIB) as u32;
        by_ratio.clamp(32, 64)
    };

    // Per-request memory_limit: only derived when we actually know the budget —
    // otherwise keep PHP's 128M (returned as None). Reserve the opcache SHM and
    // a ~64 MB engine/server overhead, then split across concurrent workers.
    let memory_limit: Option<String> = mem_bytes.map(|budget| {
        let overhead = 64 * MIB + u64::from(opcache_mb) * MIB;
        let per_request_bytes = budget.saturating_sub(overhead) / (workers.max(1) as u64);
        let per_request_mb = (per_request_bytes / MIB).max(128);
        // Cap the string at a u32-safe MB count for tidy formatting; budgets
        // this large are unrealistic but keep the cast honest.
        let mb = u32::try_from(per_request_mb).unwrap_or(u32::MAX);
        format!("{mb}M")
    });

    DerivedTuning {
        opcache_memory_consumption: Some(opcache_mb),
        opcache_interned_strings_buffer: Some(interned_mb),
        opcache_jit_buffer_size: Some(jit_mb),
        // Fixed, generous — deliberately NOT memory-shaped.
        opcache_max_accelerated_files: Some(20_000),
        memory_limit,
        realpath_cache_size: Some("16M".to_string()),
        realpath_cache_ttl: Some(600),
        zend_assertions: Some(-1),
    }
}

/// The fully-resolved resource-aware tuning profile for a run: the detected
/// inputs (CPU quota, memory budget + source, worker count + source) plus every
/// tunable resolved through the explicit → derived → default precedence.
///
/// Produced by [`PhpConfig::autotune`]. Feeds both the generated php.ini (via
/// [`Self::ini_lines`]) and the startup autotune log (via
/// [`Self::summary_line`]).
#[derive(Debug, Clone)]
pub struct AutoTune {
    /// Detected cgroup CPU quota in CPU units (`None` = unlimited/not-cgrouped).
    pub cpu_quota: Option<f64>,
    /// Detected memory budget in bytes (`None` = nothing detectable).
    pub mem_budget: Option<u64>,
    /// Where the memory figure came from.
    pub mem_source: MemorySource,
    /// Effective worker count driving the per-request `memory_limit` split.
    pub workers: usize,
    /// Where the worker count came from.
    pub worker_source: WorkerCountSource,
    /// Whether this is the dev-mode profile (vs serve).
    pub dev_mode: bool,
    /// Resolved `opcache.validate_timestamps`.
    pub validate_timestamps: TunedValue<bool>,
    /// Resolved `opcache.revalidate_freq` (only present when explicitly set).
    pub revalidate_freq: Option<TunedValue<u32>>,
    /// Resolved `opcache.memory_consumption` (MB).
    pub memory_consumption: TunedValue<u32>,
    /// Resolved `opcache.interned_strings_buffer` (MB).
    pub interned_strings_buffer: TunedValue<u32>,
    /// Resolved `opcache.jit_buffer_size` (MB). JIT stays off.
    pub jit_buffer_size: TunedValue<u32>,
    /// Resolved `opcache.max_accelerated_files`.
    pub max_accelerated_files: TunedValue<u32>,
    /// Resolved per-request `memory_limit` string.
    pub memory_limit: TunedValue<String>,
    /// Resolved `realpath_cache_size` string.
    pub realpath_cache_size: TunedValue<String>,
    /// Resolved `realpath_cache_ttl` (seconds).
    pub realpath_cache_ttl: TunedValue<u32>,
    /// Resolved `zend.assertions`.
    pub zend_assertions: TunedValue<i8>,
}

impl AutoTune {
    /// The ini `(key, value)` pairs to write, before user `ini_overrides`.
    ///
    /// `opcache.validate_timestamps` is always emitted (its default is
    /// mode-dependent, not a PHP stock value). Every other tunable is emitted
    /// only when its origin is [`Origin::Explicit`] or [`Origin::Derived`];
    /// values left at the PHP stock default are omitted so the engine default
    /// applies and dev-mode php.ini stays minimal.
    #[must_use]
    pub fn ini_lines(&self) -> Vec<(String, String)> {
        let mut lines: Vec<(String, String)> = Vec::new();

        // Always emit: the "default" here is the mode-appropriate value.
        lines.push((
            "opcache.validate_timestamps".to_string(),
            if self.validate_timestamps.value { "1" } else { "0" }.to_string(),
        ));
        if let Some(freq) = &self.revalidate_freq {
            lines.push(("opcache.revalidate_freq".to_string(), freq.value.to_string()));
        }

        // Emit a `<key>=<value>` line only when the value is pinned or derived.
        let mut push_if_set = |key: &str, tv_origin: Origin, value: String| {
            if tv_origin != Origin::Default {
                lines.push((key.to_string(), value));
            }
        };

        push_if_set(
            "opcache.memory_consumption",
            self.memory_consumption.origin,
            format!("{}", self.memory_consumption.value),
        );
        push_if_set(
            "opcache.interned_strings_buffer",
            self.interned_strings_buffer.origin,
            format!("{}", self.interned_strings_buffer.value),
        );
        push_if_set(
            "opcache.jit_buffer_size",
            self.jit_buffer_size.origin,
            // PHP accepts a bare MB integer for jit_buffer_size as bytes, so
            // append the M suffix explicitly.
            format!("{}M", self.jit_buffer_size.value),
        );
        push_if_set(
            "opcache.max_accelerated_files",
            self.max_accelerated_files.origin,
            format!("{}", self.max_accelerated_files.value),
        );
        push_if_set("memory_limit", self.memory_limit.origin, self.memory_limit.value.clone());
        push_if_set(
            "realpath_cache_size",
            self.realpath_cache_size.origin,
            self.realpath_cache_size.value.clone(),
        );
        push_if_set(
            "realpath_cache_ttl",
            self.realpath_cache_ttl.origin,
            format!("{}", self.realpath_cache_ttl.value),
        );
        push_if_set(
            "zend.assertions",
            self.zend_assertions.origin,
            format!("{}", self.zend_assertions.value),
        );

        lines
    }

    /// A single compact, human-readable summary line for the startup INFO log.
    ///
    /// A `*` after a value marks it as explicitly pinned by config (vs derived
    /// or defaulted), so operators can see at a glance what they overrode.
    #[must_use]
    pub fn summary_line(&self) -> String {
        let mode = if self.dev_mode { "dev" } else { "serve" };
        let cpu = self.cpu_quota.map_or_else(|| "unlimited".to_string(), |q| format!("{q:.2}"));
        let mem =
            self.mem_budget.map_or_else(|| "unknown".to_string(), |b| format!("{}MiB", b / MIB));
        let jit_state = if self.jit_buffer_size.origin == Origin::Default {
            "off"
        } else {
            "buffer-only, jit off"
        };
        format!(
            "autotune ({mode}): cpu_quota={cpu} mem={mem} ({}) -> workers={}[{}] \
             opcache.memory_consumption={}MB{} memory_limit={}{} interned={}MB{} \
             jit_buffer={}MB{} ({jit_state}) max_files={}{} realpath={}{}/ttl={}{} \
             validate_timestamps={}{} assertions={}{}",
            self.mem_source.label(),
            self.workers,
            self.worker_source.label(),
            self.memory_consumption.value,
            self.memory_consumption.origin.marker(),
            self.memory_limit.value,
            self.memory_limit.origin.marker(),
            self.interned_strings_buffer.value,
            self.interned_strings_buffer.origin.marker(),
            self.jit_buffer_size.value,
            self.jit_buffer_size.origin.marker(),
            self.max_accelerated_files.value,
            self.max_accelerated_files.origin.marker(),
            self.realpath_cache_size.value,
            self.realpath_cache_size.origin.marker(),
            self.realpath_cache_ttl.value,
            self.realpath_cache_ttl.origin.marker(),
            u8::from(self.validate_timestamps.value),
            self.validate_timestamps.origin.marker(),
            self.zend_assertions.value,
            self.zend_assertions.origin.marker(),
        )
    }
}

fn default_sqlite_path() -> String {
    "ephpm.db".to_string()
}

fn default_sqlite_engine() -> String {
    "sqlite".to_string()
}

fn default_sqlite_mysql_listen() -> String {
    "127.0.0.1:3306".to_string()
}

fn default_sqld_http_listen() -> String {
    "127.0.0.1:8081".to_string()
}

fn default_sqld_grpc_listen() -> String {
    "0.0.0.0:5001".to_string()
}

fn default_replication_role() -> String {
    "auto".to_string()
}

/// Clustering configuration (`[cluster]`).
///
/// Enables gossip-based peer discovery using the SWIM protocol.
#[derive(Debug, Deserialize, Clone)]
pub struct ClusterConfig {
    /// Enable gossip-based clustering.
    #[serde(default)]
    pub enabled: bool,

    /// Gossip UDP listen address.
    #[serde(default = "default_cluster_bind")]
    pub bind: String,

    /// Seed node addresses for initial cluster join.
    #[serde(default)]
    pub join: Vec<String>,

    /// Shared secret for cluster transport security.
    ///
    /// When set, all inter-node traffic (gossip UDP and the KV TCP data
    /// plane) is authenticated and encrypted with ChaCha20-Poly1305
    /// keys derived from this secret via HKDF-SHA256. Nodes without the
    /// matching secret cannot join, read, or inject traffic.
    ///
    /// Any high-entropy string works (e.g. `openssl rand -base64 32`).
    /// When empty, inter-node traffic is unauthenticated plaintext and
    /// a warning is logged at cluster startup.
    #[serde(default)]
    pub secret: String,

    /// Unique node identifier. Auto-generated if empty.
    #[serde(default)]
    pub node_id: String,

    /// Cluster identifier. Nodes with different cluster IDs ignore each other.
    #[serde(default = "default_cluster_id")]
    pub cluster_id: String,

    /// KV clustering settings.
    #[serde(default)]
    pub kv: ClusterKvConfig,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_cluster_bind(),
            join: Vec::new(),
            secret: String::new(),
            node_id: String::new(),
            cluster_id: default_cluster_id(),
            kv: ClusterKvConfig::default(),
        }
    }
}

/// KV clustering configuration (`[cluster.kv]`).
#[derive(Debug, Deserialize, Clone)]
pub struct ClusterKvConfig {
    /// Maximum value size (bytes) for the gossip tier.
    #[serde(default = "default_small_key_threshold")]
    pub small_key_threshold: usize,

    /// Number of copies kept for each large (data-plane) key.
    ///
    /// A large key lives on its primary owner (`hash(key) % alive_nodes`)
    /// plus the next `replication_factor - 1` distinct nodes on the
    /// sorted alive-node ring. The factor is clamped to the number of
    /// alive nodes, so a value larger than the cluster size simply keeps
    /// one copy per node (never an error). `1` disables replication
    /// (single owner copy). Default `2`.
    ///
    /// Replication is write-time only: a node that was down during a
    /// write does not receive the key until it is rewritten or
    /// fetched-through. Small (gossip-tier) values ignore this setting —
    /// they are always replicated to every node.
    #[serde(default = "default_replication_factor")]
    pub replication_factor: usize,

    /// How large-key replica writes propagate (`"async"` or `"sync"`).
    ///
    /// - `"async"` (default): the client write returns as soon as the
    ///   primary copy is written; the remaining replicas are updated in
    ///   the background (fire-and-forget, failures logged).
    /// - `"sync"`: the write also awaits every *reachable* replica
    ///   before returning (best-effort, read-your-writes durability
    ///   against live peers). A replica that is down is logged but does
    ///   not fail the write — this is not a quorum/consensus protocol.
    ///
    /// Any value other than `"sync"` (case-insensitive) is treated as
    /// `"async"`.
    #[serde(default = "default_replication_mode")]
    pub replication_mode: String,

    /// Enable hot key local caching.
    #[serde(default = "default_hot_key_cache")]
    pub hot_key_cache: bool,

    /// Remote fetches before promoting to cache.
    #[serde(default = "default_hot_key_threshold")]
    pub hot_key_threshold: u32,

    /// Time window (seconds) for counting remote fetches.
    #[serde(default = "default_hot_key_window_secs")]
    pub hot_key_window_secs: u64,

    /// Max age (seconds) of cached hot-key values.
    #[serde(default = "default_hot_key_local_ttl_secs")]
    pub hot_key_local_ttl_secs: u64,

    /// Memory budget for hot-key cache (e.g. `"64MB"`).
    #[serde(default = "default_hot_key_max_memory")]
    pub hot_key_max_memory: String,

    /// TCP listen port for the KV data plane.
    ///
    /// Used to fetch large values from the owner node when they exceed
    /// the gossip tier threshold. Binds on `0.0.0.0:{port}`.
    ///
    /// Default: `7947`.
    #[serde(default = "default_kv_data_port")]
    pub data_port: u16,
}

impl Default for ClusterKvConfig {
    fn default() -> Self {
        Self {
            small_key_threshold: default_small_key_threshold(),
            replication_factor: default_replication_factor(),
            replication_mode: default_replication_mode(),
            hot_key_cache: default_hot_key_cache(),
            hot_key_threshold: default_hot_key_threshold(),
            hot_key_window_secs: default_hot_key_window_secs(),
            hot_key_local_ttl_secs: default_hot_key_local_ttl_secs(),
            hot_key_max_memory: default_hot_key_max_memory(),
            data_port: default_kv_data_port(),
        }
    }
}

/// OPcache clustering configuration (`[opcache]`).
///
/// Governs the cluster-wide invalidation watcher that fires when the KV key
/// `opcache:version:<vhost>` changes. See
/// `site/content/roadmap/opcache-clustering.md` for the design.
#[derive(Debug, Default, Deserialize, Clone)]
pub struct OpcacheConfig {
    /// Watch the KV store for cluster-wide invalidation events.
    ///
    /// When enabled, every PHP request checks `opcache:version:<vhost>` before
    /// executing. If the version has advanced since the last invalidation on
    /// this node, `opcache_invalidate()` is called for every cached script
    /// under the vhost's document root before the request runs.
    ///
    /// The KV lookup is an in-process `DashMap::get` — sub-microsecond — so the
    /// per-request overhead is negligible.
    ///
    /// Default resolution (see [`OpcacheConfig::effective_cluster_invalidation`]):
    /// - `Some(true)` / `Some(false)` — explicit value from TOML
    /// - `None` — defaults to `true` when `[cluster] enabled = true`,
    ///   `false` otherwise (single-node: `ephpm cache reset` is the right
    ///   interface).
    ///
    /// **Applies to fpm mode only.** In worker mode
    /// (`[php] mode = "worker"`), the watcher is not currently invoked — the
    /// framework holds compiled bytecode in the booted process and cluster
    /// invalidation of a worker's OPcache is a future phase. Startup emits a
    /// WARN when `cluster_invalidation` resolves to true under worker mode so
    /// the no-op is never silent.
    #[serde(default)]
    pub cluster_invalidation: Option<bool>,
}

impl OpcacheConfig {
    /// Resolve the effective `cluster_invalidation` setting.
    ///
    /// `None` means "auto": on when clustering is enabled, off otherwise.
    #[must_use]
    pub fn effective_cluster_invalidation(&self, cluster_enabled: bool) -> bool {
        self.cluster_invalidation.unwrap_or(cluster_enabled)
    }
}

fn default_php_workers() -> usize {
    // Unlimited by default. A CPU-based default sounds attractive but is
    // dangerous: PHP scripts that block without I/O (sleep, long queries)
    // hold their slot past the HTTP request timeout, and a small cap lets a
    // handful of them starve all PHP traffic. Opt into a cap explicitly.
    0
}

fn default_php_mode() -> String {
    "fpm".to_string()
}

fn default_worker_count() -> usize {
    // 0 => derive at startup — cgroup CPU quota if present (Linux), otherwise
    // host parallelism clamped [2, 32]. See `PhpConfig::effective_worker_count`.
    0
}

/// Where the effective `worker_count` came from — surfaced for structured
/// logging at pool startup so operators can see why N threads were chosen.
#[derive(Debug, Clone, Copy)]
pub enum WorkerCountSource {
    /// The user set `worker_count = N` explicitly.
    Explicit,
    /// Derived from a container/cgroup CPU quota.
    CgroupQuota {
        /// Raw quota in CPU units (0.25 for a 25%-of-one-core limit).
        quota_cpus: f64,
    },
    /// Derived from host parallelism, clamped `[2, 32]`.
    HostParallelism {
        /// Detected host parallelism before clamping.
        cpus: usize,
    },
}

impl WorkerCountSource {
    /// A short label suitable for a `tracing` field.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::CgroupQuota { .. } => "cgroup_quota",
            Self::HostParallelism { .. } => "host_parallelism",
        }
    }
}

/// Read the cgroup CPU quota (v2 preferred, v1 fallback). Returns the quota in
/// CPU units — `Some(0.25)` for a 25%-of-one-core limit, `None` when no quota
/// is set (`cpu.max = "max"`), when not running under a cgroup, or on
/// non-Linux platforms.
#[cfg(target_os = "linux")]
fn read_cgroup_cpu_quota() -> Option<f64> {
    // cgroup v2: /sys/fs/cgroup/cpu.max = "<quota> <period>" or "max <period>".
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/cpu.max") {
        return parse_cgroup_v2_cpu_max(&s);
    }
    // cgroup v1: quota_us == -1 means unlimited.
    let quota = std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_quota_us").ok()?;
    let period = std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_period_us").ok()?;
    parse_cgroup_v1_cpu(&quota, &period)
}

/// Non-Linux: no cgroup CPU quota concept — always fall back to host cores.
#[cfg(not(target_os = "linux"))]
fn read_cgroup_cpu_quota() -> Option<f64> {
    None
}

/// Parse the two-word cgroup v2 `cpu.max` contents, e.g. `"25000 100000"` or
/// `"max 100000"`. Returns `Some(quota / period)` in CPU units, or `None` if
/// unlimited / malformed / period == 0.
///
/// Compiled everywhere so the unit tests (which run on Windows/macOS CI) can
/// exercise it against literal strings without touching a real cgroupfs.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn parse_cgroup_v2_cpu_max(contents: &str) -> Option<f64> {
    let line = contents.lines().next()?.trim();
    let mut parts = line.split_ascii_whitespace();
    let quota_str = parts.next()?;
    let period_str = parts.next()?;
    if quota_str.eq_ignore_ascii_case("max") {
        return None;
    }
    let quota: u64 = quota_str.parse().ok()?;
    let period: u64 = period_str.parse().ok()?;
    if period == 0 {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    Some(quota as f64 / period as f64)
}

/// Parse the cgroup v1 quota/period pair. `-1` in `cpu.cfs_quota_us` means
/// unlimited (returns `None`).
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn parse_cgroup_v1_cpu(quota_raw: &str, period_raw: &str) -> Option<f64> {
    let quota: i64 = quota_raw.trim().parse().ok()?;
    let period: u64 = period_raw.trim().parse().ok()?;
    if quota <= 0 || period == 0 {
        return None;
    }
    #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
    Some(quota as u64 as f64 / period as f64)
}

/// Where the memory figure used for autotuning came from — surfaced in the
/// startup log so operators can see whether ePHPm read a real container limit
/// or fell back to total host memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySource {
    /// A cgroup **v2** `memory.max` limit (bytes).
    CgroupV2,
    /// A cgroup **v1** `memory.limit_in_bytes` limit (bytes).
    CgroupV1,
    /// No cgroup limit — total system memory (`/proc/meminfo` `MemTotal`) is
    /// used instead. On non-Linux platforms this is the only source.
    SystemTotal,
    /// Neither a cgroup limit nor a readable system total — nothing to derive
    /// from, so memory-shaped tunables keep their PHP-stock defaults.
    Unknown,
}

impl MemorySource {
    /// A short label suitable for a `tracing` field / the autotune summary.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::CgroupV2 => "cgroup v2",
            Self::CgroupV1 => "cgroup v1",
            Self::SystemTotal => "system-total",
            Self::Unknown => "unknown",
        }
    }
}

/// Detect the memory budget (in bytes) to size PHP/OPcache against, plus where
/// the figure came from.
///
/// Resolution order (Linux):
/// 1. cgroup **v2** `/sys/fs/cgroup/memory.max` (a real container limit).
/// 2. cgroup **v1** `/sys/fs/cgroup/memory/memory.limit_in_bytes`.
/// 3. `/proc/meminfo` `MemTotal` (total host memory — no container limit).
///
/// A cgroup limit of `"max"` (v2) or an absurdly-large sentinel (v1) means "no
/// limit set" and is skipped so we fall through to the system total. On
/// non-Linux platforms only the (unavailable) system-total path applies, so
/// this returns `(None, MemorySource::Unknown)` and callers keep PHP defaults.
#[must_use]
pub fn detect_memory_budget() -> (Option<u64>, MemorySource) {
    if let Some(bytes) = read_cgroup_memory_limit() {
        // Distinguish v2 vs v1 purely for the log label; the value is the same
        // shape either way. read_cgroup_memory_limit already prefers v2.
        let source = if std::path::Path::new("/sys/fs/cgroup/memory.max").exists() {
            MemorySource::CgroupV2
        } else {
            MemorySource::CgroupV1
        };
        return (Some(bytes), source);
    }
    if let Some(total) = read_total_system_memory() {
        return (Some(total), MemorySource::SystemTotal);
    }
    (None, MemorySource::Unknown)
}

/// Read the cgroup memory limit in bytes (v2 preferred, v1 fallback).
///
/// Returns `None` when no limit is set (`memory.max = "max"` on v2, or a
/// near-`u64::MAX` sentinel on v1), when not running under a cgroup, or on
/// non-Linux platforms — the caller then falls back to total system memory.
#[cfg(target_os = "linux")]
fn read_cgroup_memory_limit() -> Option<u64> {
    // cgroup v2: /sys/fs/cgroup/memory.max = "<bytes>" or "max".
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/memory.max") {
        return parse_cgroup_v2_memory_max(&s);
    }
    // cgroup v1: memory.limit_in_bytes. Unlimited is represented by a huge
    // page-aligned sentinel close to i64::MAX / u64::MAX.
    let raw = std::fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes").ok()?;
    parse_cgroup_v1_memory_limit(&raw)
}

/// Non-Linux: no cgroup memory concept — always fall back to system total.
#[cfg(not(target_os = "linux"))]
fn read_cgroup_memory_limit() -> Option<u64> {
    None
}

/// Parse the cgroup v2 `memory.max` contents: a byte count, or the literal
/// `"max"` meaning "no limit" (returns `None`).
///
/// Compiled everywhere so the unit tests (which run on Windows/macOS CI) can
/// exercise it against literal strings without a real cgroupfs.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn parse_cgroup_v2_memory_max(contents: &str) -> Option<u64> {
    let line = contents.lines().next()?.trim();
    if line.eq_ignore_ascii_case("max") {
        return None;
    }
    let bytes: u64 = line.parse().ok()?;
    if bytes == 0 { None } else { Some(bytes) }
}

/// Parse the cgroup v1 `memory.limit_in_bytes` value. The kernel represents
/// "unlimited" as a huge page-aligned sentinel (typically
/// `0x7FFF_FFFF_FFFF_F000` — `i64::MAX` rounded down to a page boundary, or a
/// near-`u64::MAX` value on 64-bit). Any value at or above a conservative
/// threshold (half of `u64::MAX`, far larger than any real machine's RAM) is
/// treated as "no limit" and returns `None`.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn parse_cgroup_v1_memory_limit(raw: &str) -> Option<u64> {
    let bytes: u64 = raw.trim().parse().ok()?;
    // No physical machine has ~4 EiB of RAM; anything this large is the
    // "unlimited" sentinel, not a real cap. The classic v1 sentinel is
    // `i64::MAX` page-aligned (`0x7FFF_FFFF_FFFF_F000` ≈ 9.22 EiB), so the
    // threshold sits comfortably below it (`1 << 62` ≈ 4.6 EiB) yet far above
    // any real host's RAM.
    const UNLIMITED_THRESHOLD: u64 = 1 << 62;
    if bytes == 0 || bytes >= UNLIMITED_THRESHOLD { None } else { Some(bytes) }
}

/// Read total system memory in bytes from `/proc/meminfo` (`MemTotal`, which
/// is reported in kibibytes). Returns `None` on non-Linux platforms or if the
/// field is missing/unparseable — callers then keep PHP-stock defaults.
#[cfg(target_os = "linux")]
fn read_total_system_memory() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo_memtotal(&meminfo)
}

/// Non-Linux: no `/proc/meminfo`. We keep the dependency footprint minimal
/// (no `sysinfo` crate) rather than pull a platform abstraction just for the
/// fallback, so memory-shaped tunables keep PHP defaults off-Linux.
#[cfg(not(target_os = "linux"))]
fn read_total_system_memory() -> Option<u64> {
    None
}

/// Parse `MemTotal:` (kibibytes) out of `/proc/meminfo` contents and convert to
/// bytes. Format: `MemTotal:        4028860 kB`.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn parse_meminfo_memtotal(contents: &str) -> Option<u64> {
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kib: u64 = rest.split_ascii_whitespace().next()?.parse().ok()?;
            return Some(kib.saturating_mul(1024));
        }
    }
    None
}

fn default_worker_max_requests() -> u64 {
    // Pure leak guard, not a churn trigger: for a leak-free framework loop
    // recycling is pure overhead. Raised from 500 (2026-07-09 roadmap): at
    // 2,000 rps, the old default recycled every ~0.25 s.
    10_000
}

fn default_worker_backlog() -> usize {
    // 0 => = effective_worker_count (one queued job per worker).
    0
}

fn default_worker_stream_threshold() -> u64 {
    // 1 MiB: bodies at/above this stream; smaller ones buffer (cheaper).
    1024 * 1024
}

fn default_worker_boot_timeout() -> u64 {
    30
}

fn default_cluster_bind() -> String {
    "0.0.0.0:7946".to_string()
}

fn default_cluster_id() -> String {
    "ephpm".to_string()
}

fn default_small_key_threshold() -> usize {
    512
}

fn default_replication_factor() -> usize {
    2
}

fn default_replication_mode() -> String {
    "async".to_string()
}

fn default_hot_key_cache() -> bool {
    true
}

fn default_hot_key_threshold() -> u32 {
    5
}

fn default_hot_key_window_secs() -> u64 {
    10
}

fn default_hot_key_local_ttl_secs() -> u64 {
    30
}

fn default_hot_key_max_memory() -> String {
    "64MB".to_string()
}

fn default_kv_data_port() -> u16 {
    7947
}

fn default_listen() -> String {
    "0.0.0.0:8080".to_string()
}

fn default_document_root() -> PathBuf {
    PathBuf::from(".")
}

fn default_index_files() -> Vec<String> {
    vec!["index.php".to_string(), "index.html".to_string()]
}

fn default_fallback() -> Vec<String> {
    vec!["$uri".to_string(), "$uri/".to_string(), "/index.php?$query_string".to_string()]
}

fn default_max_body_size() -> u64 {
    10 * 1024 * 1024 // 10 MiB
}

fn default_header_read() -> u64 {
    30
}

fn default_idle() -> u64 {
    60
}

fn default_request_timeout() -> u64 {
    300
}

fn default_shutdown_timeout() -> u64 {
    30
}

fn default_max_header_size() -> usize {
    8192
}

fn default_compression() -> bool {
    true
}

fn default_compression_level() -> u32 {
    1
}

fn default_compression_min_size() -> usize {
    1024
}

fn default_compression_streaming() -> String {
    "off".to_string()
}

fn default_hidden_files() -> String {
    "deny".to_string()
}

fn default_etag() -> bool {
    true
}

fn default_php_etag_cache_enabled() -> bool {
    false
}

fn default_php_etag_cache_ttl() -> i64 {
    300
}

fn default_php_etag_cache_prefix() -> String {
    "etag:".to_string()
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_cache_dir() -> PathBuf {
    PathBuf::from("certs")
}

fn default_max_execution_time() -> u32 {
    30
}

fn default_memory_limit() -> String {
    "128M".to_string()
}

fn default_kv_memory_limit() -> String {
    "256MB".to_string()
}

fn default_kv_eviction_policy() -> String {
    "allkeys-lru".to_string()
}

fn default_kv_compression() -> String {
    "none".to_string()
}

fn default_kv_compression_level() -> u32 {
    6
}

fn default_kv_compression_min_size() -> usize {
    1024
}

fn default_kv_listen() -> String {
    "127.0.0.1:6379".to_string()
}

fn default_min_connections() -> u32 {
    2
}

fn default_max_connections() -> u32 {
    20
}

fn default_idle_timeout() -> String {
    "300s".to_string()
}

fn default_max_lifetime() -> String {
    "1800s".to_string()
}

fn default_pool_timeout() -> String {
    "5s".to_string()
}

fn default_health_check_interval() -> String {
    "30s".to_string()
}

fn default_inject_env() -> bool {
    true
}

fn default_reset_strategy() -> String {
    "smart".to_string()
}

fn default_rw_strategy() -> String {
    "sticky-after-write".to_string()
}

fn default_sticky_duration() -> String {
    "2s".to_string()
}

fn default_max_replica_lag() -> String {
    "500ms".to_string()
}

fn default_slow_query_threshold() -> String {
    "1s".to_string()
}

fn default_auto_explain_target() -> String {
    "stderr".to_string()
}

fn default_digest_max_entries() -> usize {
    100_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default_config().expect("default config should load");
        assert_eq!(config.server.listen, "0.0.0.0:8080");
        assert_eq!(config.php.max_execution_time, 30);
        assert_eq!(config.php.memory_limit, "128M");
        assert_eq!(config.server.index_files, vec!["index.php", "index.html"]);
    }

    #[test]
    fn test_load_valid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[server]
listen = "127.0.0.1:3000"
document_root = "/srv/app"
index_files = ["app.php"]

[php]
max_execution_time = 60
memory_limit = "256M"
ini_overrides = [["display_errors", "On"]]
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        assert_eq!(config.server.listen, "127.0.0.1:3000");
        assert_eq!(config.server.document_root, PathBuf::from("/srv/app"));
        assert_eq!(config.server.index_files, vec!["app.php"]);
        assert_eq!(config.php.max_execution_time, 60);
        assert_eq!(config.php.memory_limit, "256M");
        assert_eq!(config.php.ini_overrides.len(), 1);
        assert_eq!(config.php.ini_overrides[0], ["display_errors", "On"]);
    }

    #[test]
    fn test_load_partial_toml_fills_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[server]
listen = "127.0.0.1:3000"
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        assert_eq!(config.server.listen, "127.0.0.1:3000");
        // Unspecified fields use defaults
        assert_eq!(config.server.document_root, PathBuf::from("."));
        assert_eq!(config.php.max_execution_time, 30);
        assert_eq!(config.php.memory_limit, "128M");
    }

    #[test]
    fn test_load_missing_file_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nonexistent.toml");

        // figment Toml::file is non-strict — missing file falls through to defaults
        let config = Config::load(&file).unwrap();
        assert_eq!(config.server.listen, "0.0.0.0:8080");
        assert_eq!(config.php.max_execution_time, 30);
    }

    #[test]
    fn test_env_var_overrides_toml() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[server]
listen = "0.0.0.0:8080"
"#,
        )
        .unwrap();

        temp_env::with_var("EPHPM_SERVER__LISTEN", Some("127.0.0.1:9090"), || {
            let config = Config::load(&file).unwrap();
            assert_eq!(config.server.listen, "127.0.0.1:9090");
        });
    }

    #[test]
    fn test_env_var_override_without_file() {
        temp_env::with_var("EPHPM_PHP__MEMORY_LIMIT", Some("256M"), || {
            let config = Config::default_config().unwrap();
            assert_eq!(config.php.memory_limit, "256M");
        });
    }

    #[test]
    fn test_ini_overrides_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[php]
ini_overrides = [
    ["display_errors", "Off"],
    ["error_reporting", "E_ALL"],
]
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        assert_eq!(config.php.ini_overrides.len(), 2);
        assert_eq!(config.php.ini_overrides[0], ["display_errors", "Off"]);
        assert_eq!(config.php.ini_overrides[1], ["error_reporting", "E_ALL"]);
    }

    #[test]
    fn test_php_extensions_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[php]
extensions = ["redis", "/usr/lib/php/20240924/imagick.so"]
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        assert_eq!(config.php.extensions, vec!["redis", "/usr/lib/php/20240924/imagick.so"]);
        config.validate().expect("non-empty extension entries should validate");
    }

    #[test]
    fn test_php_extensions_default_empty() {
        let config = Config::default_config().unwrap();
        assert!(config.php.extensions.is_empty());
        config.validate().expect("empty extension list should validate");
    }

    #[test]
    fn test_php_extensions_empty_entry_fails_validation() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[php]
extensions = ["redis", ""]
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        let err = config.validate().expect_err("empty extension entry must be rejected");
        assert!(err.to_string().contains("extensions entry 1"), "unexpected error: {err}");
    }

    #[test]
    fn test_php_extensions_ini_injection_rejected() {
        // A newline/CR/NUL in an extension entry would inject a second ini
        // directive into the generated php.ini. Build the config directly so
        // the control characters survive verbatim.
        for bad in ["redis\nmemory_limit=999G", "redis\rfoo=bar", "redis\0evil"] {
            let mut config = Config::default();
            config.php.extensions = vec![bad.to_string()];
            let err = config
                .validate()
                .expect_err("extension entry with a control char must be rejected");
            assert!(matches!(err, ConfigError::Validation(_)));
            assert!(err.to_string().contains("extensions entry 0"), "unexpected error: {err}");
        }
    }

    #[test]
    fn test_middleware_mounts_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[[middleware]]
library = "auth-jwt"
match = "/api/*"
order = 10

[[middleware]]
library = "rate-limit"
order = 20
config = { per_ip_rps = 50, burst = 100 }
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        config.validate().unwrap();
        assert_eq!(config.middleware.len(), 2);

        assert_eq!(config.middleware[0].library, "auth-jwt");
        assert_eq!(config.middleware[0].match_pattern.as_deref(), Some("/api/*"));
        assert_eq!(config.middleware[0].order, 10);
        assert!(config.middleware[0].config.is_none());

        assert_eq!(config.middleware[1].library, "rate-limit");
        assert!(config.middleware[1].match_pattern.is_none());
        assert_eq!(config.middleware[1].order, 20);
        let mount_config = config.middleware[1].config.as_ref().expect("inline config table");
        assert_eq!(mount_config["per_ip_rps"], serde_json::json!(50));
        assert_eq!(mount_config["burst"], serde_json::json!(100));
        // The loader serialises this value back to JSON for the module's init.
        let json = serde_json::to_string(mount_config).unwrap();
        assert!(json.contains("per_ip_rps"));
    }

    #[test]
    fn test_middleware_missing_order_fails() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[[middleware]]
library = "auth-jwt"
"#,
        )
        .unwrap();

        assert!(Config::load(&file).is_err(), "order is required — no default");
    }

    #[test]
    fn test_middleware_empty_library_fails_validation() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[[middleware]]
library = ""
order = 10
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("library must not be empty"), "{err}");
    }

    #[test]
    fn test_php_etag_cache_defaults() {
        let config = Config::default_config().unwrap();
        assert!(!config.server.php_etag_cache.enabled);
        assert_eq!(config.server.php_etag_cache.ttl_secs, 300);
        assert_eq!(config.server.php_etag_cache.key_prefix, "etag:");
    }

    #[test]
    fn test_php_etag_cache_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[server.php_etag_cache]
enabled = true
ttl_secs = 600
key_prefix = "cache:etag:"
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        assert!(config.server.php_etag_cache.enabled);
        assert_eq!(config.server.php_etag_cache.ttl_secs, 600);
        assert_eq!(config.server.php_etag_cache.key_prefix, "cache:etag:");
    }

    #[test]
    fn test_php_etag_cache_indefinite_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r"
[server.php_etag_cache]
enabled = true
ttl_secs = -1
",
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        assert!(config.server.php_etag_cache.enabled);
        assert_eq!(config.server.php_etag_cache.ttl_secs, -1);
    }

    #[test]
    fn test_compression_streaming_defaults_off() {
        let config = Config::default_config().unwrap();
        assert_eq!(config.server.response.compression_streaming, "off");
    }

    #[test]
    fn test_compression_streaming_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[server.response]
compression_streaming = "sse"
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        assert_eq!(config.server.response.compression_streaming, "sse");
    }

    #[test]
    fn test_env_var_overrides_compression_streaming() {
        temp_env::with_var("EPHPM_SERVER__RESPONSE__COMPRESSION_STREAMING", Some("all"), || {
            let config = Config::default_config().unwrap();
            assert_eq!(config.server.response.compression_streaming, "all");
        });
    }

    #[test]
    fn test_kv_compression_defaults() {
        let config = Config::default_config().unwrap();
        assert_eq!(config.kv.compression, "none");
        assert_eq!(config.kv.compression_level, 6);
        assert_eq!(config.kv.compression_min_size, 1024);
    }

    #[test]
    fn test_kv_compression_gzip_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[kv]
compression = "gzip"
compression_level = 9
compression_min_size = 512
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        assert_eq!(config.kv.compression, "gzip");
        assert_eq!(config.kv.compression_level, 9);
        assert_eq!(config.kv.compression_min_size, 512);
    }

    #[test]
    fn test_kv_compression_zstd_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[kv]
compression = "zstd"
compression_level = 3
compression_min_size = 2048
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        assert_eq!(config.kv.compression, "zstd");
        assert_eq!(config.kv.compression_level, 3);
        assert_eq!(config.kv.compression_min_size, 2048);
    }

    #[test]
    fn test_kv_compression_brotli_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[kv]
compression = "brotli"
compression_level = 5
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        assert_eq!(config.kv.compression, "brotli");
        assert_eq!(config.kv.compression_level, 5);
    }

    #[test]
    fn test_env_var_overrides_php_etag_cache() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r"
[server.php_etag_cache]
enabled = false
",
        )
        .unwrap();

        temp_env::with_var("EPHPM_SERVER__PHP_ETAG_CACHE__ENABLED", Some("true"), || {
            let config = Config::load(&file).unwrap();
            assert!(config.server.php_etag_cache.enabled);
        });
    }

    #[test]
    fn test_env_var_overrides_kv_compression() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[kv]
compression = "none"
"#,
        )
        .unwrap();

        temp_env::with_var("EPHPM_KV__COMPRESSION", Some("gzip"), || {
            let config = Config::load(&file).unwrap();
            assert_eq!(config.kv.compression, "gzip");
        });
    }

    #[test]
    fn test_env_var_overrides_compression_level() {
        temp_env::with_var("EPHPM_KV__COMPRESSION_LEVEL", Some("8"), || {
            let config = Config::default_config().unwrap();
            assert_eq!(config.kv.compression_level, 8);
        });
    }

    #[test]
    fn test_env_var_overrides_compression_min_size() {
        temp_env::with_var("EPHPM_KV__COMPRESSION_MIN_SIZE", Some("4096"), || {
            let config = Config::default_config().unwrap();
            assert_eq!(config.kv.compression_min_size, 4096);
        });
    }

    #[test]
    fn test_env_var_overrides_vec_string() {
        temp_env::with_var(
            "EPHPM_CLUSTER__JOIN",
            Some(r#"["10.0.0.1:7946","10.0.0.2:7946"]"#),
            || {
                let config = Config::default_config().unwrap();
                assert_eq!(
                    config.cluster.join,
                    vec!["10.0.0.1:7946".to_string(), "10.0.0.2:7946".to_string()]
                );
            },
        );
    }

    #[test]
    fn test_env_var_overrides_vec_pair_string() {
        temp_env::with_var(
            "EPHPM_PHP__INI_OVERRIDES",
            Some(r#"[["display_errors","Off"],["error_reporting","E_ALL"]]"#),
            || {
                let config = Config::default_config().unwrap();
                assert_eq!(
                    config.php.ini_overrides,
                    vec![
                        ["display_errors".to_string(), "Off".to_string()],
                        ["error_reporting".to_string(), "E_ALL".to_string()],
                    ]
                );
            },
        );
    }

    #[test]
    fn test_combined_php_etag_and_compression_config() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[server.php_etag_cache]
enabled = true
ttl_secs = 3600
key_prefix = "etag:"

[kv]
compression = "zstd"
compression_level = 6
compression_min_size = 1024
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();

        // Verify ETag config
        assert!(config.server.php_etag_cache.enabled);
        assert_eq!(config.server.php_etag_cache.ttl_secs, 3600);
        assert_eq!(config.server.php_etag_cache.key_prefix, "etag:");

        // Verify compression config
        assert_eq!(config.kv.compression, "zstd");
        assert_eq!(config.kv.compression_level, 6);
        assert_eq!(config.kv.compression_min_size, 1024);
    }

    #[test]
    fn test_sqlite_defaults_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[db.sqlite]
path = "app.db"
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        let sqlite = config.db.sqlite.expect("sqlite should be present");
        assert_eq!(sqlite.path, "app.db");
        assert_eq!(sqlite.proxy.mysql_listen, "127.0.0.1:3306");
        assert!(sqlite.proxy.hrana_listen.is_none());
        assert_eq!(sqlite.sqld.http_listen, "127.0.0.1:8081");
        assert_eq!(sqlite.sqld.grpc_listen, "0.0.0.0:5001");
        assert_eq!(sqlite.replication.role, "auto");
        assert!(sqlite.replication.primary_grpc_url.is_empty());
        assert_eq!(sqlite.engine, "sqlite", "engine must default to the genuine SQLite C engine");
    }

    #[test]
    fn test_sqlite_engine_turso_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[db.sqlite]
path = "app.db"
engine = "turso"
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        let sqlite = config.db.sqlite.expect("sqlite should be present");
        assert_eq!(sqlite.engine, "turso");
    }

    #[test]
    fn test_sqlite_full_config_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[db.sqlite]
path = "/var/lib/ephpm/app.db"

[db.sqlite.proxy]
mysql_listen = "0.0.0.0:3307"
hrana_listen = "0.0.0.0:8080"

[db.sqlite.sqld]
http_listen = "127.0.0.1:9081"
grpc_listen = "0.0.0.0:6001"

[db.sqlite.replication]
role = "replica"
primary_grpc_url = "http://10.0.1.2:5001"
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        let sqlite = config.db.sqlite.expect("sqlite should be present");
        assert_eq!(sqlite.path, "/var/lib/ephpm/app.db");
        assert_eq!(sqlite.proxy.mysql_listen, "0.0.0.0:3307");
        assert_eq!(sqlite.proxy.hrana_listen.as_deref(), Some("0.0.0.0:8080"));
        assert_eq!(sqlite.sqld.http_listen, "127.0.0.1:9081");
        assert_eq!(sqlite.sqld.grpc_listen, "0.0.0.0:6001");
        assert_eq!(sqlite.replication.role, "replica");
        assert_eq!(sqlite.replication.primary_grpc_url, "http://10.0.1.2:5001");
    }

    #[test]
    fn test_sqlite_not_present_by_default() {
        let config = Config::default_config().unwrap();
        assert!(config.db.sqlite.is_none());
    }

    #[test]
    fn test_sqlite_env_var_override() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[db.sqlite]
path = "test.db"
"#,
        )
        .unwrap();

        temp_env::with_var("EPHPM_DB__SQLITE__REPLICATION__ROLE", Some("primary"), || {
            let config = Config::load(&file).unwrap();
            let sqlite = config.db.sqlite.expect("sqlite should be present");
            assert_eq!(sqlite.replication.role, "primary");
        });
    }

    // ── Security isolation default resolution ──────────────────────────
    //
    // `open_basedir` / `disable_shell_exec` resolve to `true` when the
    // `[server.security]` section is present OR `sites_dir` is set;
    // an explicitly set value always wins.

    #[test]
    fn test_security_section_absent_no_sites_dir_defaults_off() {
        let config = Config::default_config().unwrap();
        assert!(config.server.security.is_none());
        assert!(!config.server.effective_open_basedir());
        assert!(!config.server.effective_disable_shell_exec());
    }

    #[test]
    fn test_security_section_absent_sites_dir_defaults_on() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[server]
sites_dir = "/var/www/sites"
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        assert!(config.server.security.is_none(), "no [server.security] section in this config");
        assert!(config.server.effective_open_basedir());
        assert!(config.server.effective_disable_shell_exec());
    }

    #[test]
    fn test_security_explicit_false_wins_over_sites_dir() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[server]
sites_dir = "/var/www/sites"

[server.security]
open_basedir = false
disable_shell_exec = false
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        assert!(!config.server.effective_open_basedir());
        assert!(!config.server.effective_disable_shell_exec());
    }

    #[test]
    fn test_security_section_present_field_unset_no_sites_dir_defaults_on() {
        // Compat: existing configs that declare [server.security] (for e.g.
        // trusted_proxies) keep the historical "present section ⇒ true"
        // defaults even without sites_dir.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[server.security]
trusted_proxies = ["10.0.0.0/8"]
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        assert!(config.server.security.is_some());
        assert!(config.server.effective_open_basedir());
        assert!(config.server.effective_disable_shell_exec());
    }

    #[test]
    fn test_security_explicit_true_without_sites_dir() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r"
[server.security]
open_basedir = true
",
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        assert!(config.server.effective_open_basedir());
        // Unset sibling also resolves true because the section is present.
        assert!(config.server.effective_disable_shell_exec());
    }

    #[test]
    fn test_security_env_var_override_counts_as_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[server]
sites_dir = "/var/www/sites"
"#,
        )
        .unwrap();

        temp_env::with_var("EPHPM_SERVER__SECURITY__OPEN_BASEDIR", Some("false"), || {
            let config = Config::load(&file).unwrap();
            assert!(!config.server.effective_open_basedir());
            // The env var materializes the section, so the unset sibling
            // still resolves true (and sites_dir is set anyway).
            assert!(config.server.effective_disable_shell_exec());
        });
    }

    // ── OPcache timestamp validation ────────────────────────────────────

    #[test]
    fn test_opcache_validate_timestamps_defaults_to_none() {
        let cfg = PhpConfig::default();
        assert_eq!(cfg.opcache_validate_timestamps, None);
        assert_eq!(cfg.opcache_revalidate_freq, None);
    }

    #[test]
    fn test_opcache_mode_defaults_serve_off_dev_on() {
        let cfg = PhpConfig::default();
        // Unset → mode default: serve off, dev on.
        assert!(!cfg.effective_validate_timestamps(false), "serve default must be off");
        assert!(cfg.effective_validate_timestamps(true), "dev default must be on");
    }

    #[test]
    fn test_opcache_explicit_override_wins_in_both_modes() {
        let on = PhpConfig { opcache_validate_timestamps: Some(true), ..PhpConfig::default() };
        assert!(on.effective_validate_timestamps(false), "explicit true forces on under serve");
        assert!(on.effective_validate_timestamps(true));

        let off = PhpConfig { opcache_validate_timestamps: Some(false), ..PhpConfig::default() };
        assert!(!off.effective_validate_timestamps(true), "explicit false forces off under dev");
        assert!(!off.effective_validate_timestamps(false));
    }

    #[test]
    fn test_opcache_ini_lines_serve_default() {
        // Serve mode now emits the derived autotuning profile in addition to
        // validate_timestamps. The exact opcache/memory byte values depend on
        // the host's detected memory budget, so assert on the *keys* present
        // and the environment-independent ones (assertions, realpath, files).
        let cfg = PhpConfig::default();
        let lines = cfg.opcache_ini_lines(false);
        let keys: Vec<&str> = lines.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"opcache.validate_timestamps"));
        assert!(keys.contains(&"opcache.memory_consumption"));
        assert!(keys.contains(&"opcache.interned_strings_buffer"));
        assert!(keys.contains(&"opcache.jit_buffer_size"));
        assert!(keys.contains(&"opcache.max_accelerated_files"));
        assert!(keys.contains(&"realpath_cache_size"));
        assert!(keys.contains(&"realpath_cache_ttl"));
        assert!(keys.contains(&"zend.assertions"));
        // Environment-independent derived values.
        let get = |k: &str| lines.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        assert_eq!(get("opcache.validate_timestamps"), Some("0"));
        assert_eq!(get("opcache.max_accelerated_files"), Some("20000"));
        assert_eq!(get("realpath_cache_size"), Some("16M"));
        assert_eq!(get("realpath_cache_ttl"), Some("600"));
        assert_eq!(get("zend.assertions"), Some("-1"));
    }

    #[test]
    fn test_opcache_ini_lines_dev_default() {
        // Dev mode derives nothing — only the mode-appropriate
        // validate_timestamps line is emitted, keeping the dev php.ini minimal.
        let cfg = PhpConfig::default();
        let lines = cfg.opcache_ini_lines(true);
        assert_eq!(lines, vec![("opcache.validate_timestamps".to_string(), "1".to_string())]);
    }

    #[test]
    fn test_opcache_ini_lines_include_revalidate_freq_when_set() {
        // Dev mode so no derived lines interfere — assert exact output.
        let cfg = PhpConfig {
            opcache_validate_timestamps: Some(true),
            opcache_revalidate_freq: Some(60),
            ..PhpConfig::default()
        };
        let lines = cfg.opcache_ini_lines(true);
        assert_eq!(
            lines,
            vec![
                ("opcache.validate_timestamps".to_string(), "1".to_string()),
                ("opcache.revalidate_freq".to_string(), "60".to_string()),
            ]
        );
    }

    // --- Resource-aware autotuning: cgroup memory parsing ---

    #[test]
    fn test_parse_cgroup_v2_memory_max_real_limit() {
        // 320 MiB limit.
        let bytes = 320 * 1024 * 1024;
        assert_eq!(parse_cgroup_v2_memory_max(&format!("{bytes}\n")), Some(bytes));
    }

    #[test]
    fn test_parse_cgroup_v2_memory_max_unlimited() {
        assert_eq!(parse_cgroup_v2_memory_max("max\n"), None);
        assert_eq!(parse_cgroup_v2_memory_max("MAX"), None);
        // Zero is treated as no usable limit.
        assert_eq!(parse_cgroup_v2_memory_max("0"), None);
    }

    #[test]
    fn test_parse_cgroup_v1_memory_limit_real_limit() {
        let bytes = 4u64 * 1024 * 1024 * 1024; // 4 GiB
        assert_eq!(parse_cgroup_v1_memory_limit(&format!("{bytes}\n")), Some(bytes));
    }

    #[test]
    fn test_parse_cgroup_v1_memory_limit_unlimited_sentinel() {
        // The classic cgroup v1 "unlimited" sentinel (i64::MAX page-aligned).
        assert_eq!(parse_cgroup_v1_memory_limit("9223372036854771712"), None);
        // Near-u64::MAX also counts as unlimited.
        assert_eq!(parse_cgroup_v1_memory_limit(&u64::MAX.to_string()), None);
        // Zero => no limit.
        assert_eq!(parse_cgroup_v1_memory_limit("0"), None);
    }

    #[test]
    fn test_parse_meminfo_memtotal() {
        let sample = "MemTotal:        4028860 kB\nMemFree:  100000 kB\n";
        // 4028860 KiB -> bytes.
        assert_eq!(parse_meminfo_memtotal(sample), Some(4_028_860 * 1024));
        assert_eq!(parse_meminfo_memtotal("MemFree: 1 kB"), None);
    }

    // --- Resource-aware autotuning: derivation formulas ---

    #[test]
    fn test_derive_tuning_dev_mode_is_empty() {
        // Dev keeps PHP-friendly defaults regardless of resources.
        let d = derive_tuning(Some(4.0), Some(4 * 1024 * 1024 * 1024), 4, true);
        assert_eq!(d, DerivedTuning::default());
    }

    #[test]
    fn test_derive_tuning_small_pod_320mi_quarter_cpu() {
        // 320 MiB / 0.25 CPU => 1 worker.
        let mem = 320 * 1024 * 1024;
        let d = derive_tuning(Some(0.25), Some(mem), 1, false);
        // 18% of 320 MiB = 57.6 MiB -> clamps up to the 64 MB floor.
        assert_eq!(d.opcache_memory_consumption, Some(64));
        // interned: 64/16 = 4 -> clamps up to 8.
        assert_eq!(d.opcache_interned_strings_buffer, Some(8));
        // jit: 320MiB/64 = 5 MB -> clamps up to 32.
        assert_eq!(d.opcache_jit_buffer_size, Some(32));
        assert_eq!(d.opcache_max_accelerated_files, Some(20_000));
        // memory_limit: (320 - 64 opcache - 64 overhead)/1 = 192 MiB.
        assert_eq!(d.memory_limit.as_deref(), Some("192M"));
        assert_eq!(d.realpath_cache_size.as_deref(), Some("16M"));
        assert_eq!(d.realpath_cache_ttl, Some(600));
        assert_eq!(d.zend_assertions, Some(-1));
    }

    #[test]
    fn test_derive_tuning_large_4gi_4cpu() {
        let mem = 4u64 * 1024 * 1024 * 1024; // 4 GiB
        let d = derive_tuning(Some(4.0), Some(mem), 4, false);
        // 18% of 4096 MiB = 737 MiB -> clamps down to the 512 MB ceiling.
        assert_eq!(d.opcache_memory_consumption, Some(512));
        // interned: 512/16 = 32 (within [8,64]).
        assert_eq!(d.opcache_interned_strings_buffer, Some(32));
        // jit: 4096/64 = 64 MB (at the ceiling).
        assert_eq!(d.opcache_jit_buffer_size, Some(64));
        // memory_limit: (4096 - 512 - 64)/4 = 880 MiB.
        assert_eq!(d.memory_limit.as_deref(), Some("880M"));
    }

    #[test]
    fn test_derive_tuning_unlimited_memory_keeps_php_default() {
        // No detectable memory budget: opcache SHM still gets the sane 64 MB
        // floor, but per-request memory_limit stays None (keep PHP's 128M)
        // rather than inventing a huge number.
        let d = derive_tuning(None, None, 4, false);
        assert_eq!(d.opcache_memory_consumption, Some(64));
        assert_eq!(d.opcache_jit_buffer_size, Some(32));
        assert_eq!(d.memory_limit, None);
    }

    #[test]
    fn test_derive_tuning_memory_limit_floors_at_128() {
        // A tiny 128 MiB pod: (128 - 64 opcache - 64 overhead)/1 = 0 -> floor 128.
        let mem = 128 * 1024 * 1024;
        let d = derive_tuning(Some(0.25), Some(mem), 1, false);
        assert_eq!(d.memory_limit.as_deref(), Some("128M"));
    }

    // --- Three-tier override precedence ---

    #[test]
    fn test_autotune_explicit_beats_derived_beats_default() {
        // Explicit config value wins over any derivation.
        let cfg = PhpConfig {
            opcache_memory_consumption: Some(256),
            php_memory_limit: Some("777M".to_string()),
            zend_assertions: Some(0),
            ..PhpConfig::default()
        };
        let at = cfg.autotune(false);
        assert_eq!(at.memory_consumption.value, 256);
        assert_eq!(at.memory_consumption.origin, Origin::Explicit);
        assert_eq!(at.memory_limit.value, "777M");
        assert_eq!(at.memory_limit.origin, Origin::Explicit);
        assert_eq!(at.zend_assertions.value, 0);
        assert_eq!(at.zend_assertions.origin, Origin::Explicit);

        // Unset fields are derived (serve mode).
        assert_eq!(at.max_accelerated_files.origin, Origin::Derived);
        assert_eq!(at.max_accelerated_files.value, 20_000);
        assert_eq!(at.realpath_cache_size.origin, Origin::Derived);
        assert_eq!(at.realpath_cache_size.value, "16M");
    }

    #[test]
    fn test_autotune_dev_mode_leaves_values_at_php_default() {
        // Dev mode derives nothing, so unset knobs resolve to the PHP default
        // and are omitted from the ini (Origin::Default).
        let cfg = PhpConfig::default();
        let at = cfg.autotune(true);
        assert_eq!(at.memory_consumption.origin, Origin::Default);
        assert_eq!(at.max_accelerated_files.origin, Origin::Default);
        assert_eq!(at.zend_assertions.origin, Origin::Default);
        // But an explicit knob still wins in dev.
        let cfg2 = PhpConfig { zend_assertions: Some(-1), ..PhpConfig::default() };
        let at2 = cfg2.autotune(true);
        assert_eq!(at2.zend_assertions.value, -1);
        assert_eq!(at2.zend_assertions.origin, Origin::Explicit);
    }

    #[test]
    fn test_autotune_summary_line_marks_explicit() {
        let cfg = PhpConfig { opcache_memory_consumption: Some(200), ..PhpConfig::default() };
        let line = cfg.autotune(false).summary_line();
        assert!(line.contains("autotune (serve)"));
        // Explicit memory_consumption is marked with a `*` (after the MB unit).
        assert!(line.contains("opcache.memory_consumption=200MB*"), "got: {line}");
    }

    #[test]
    fn test_new_autotune_knobs_load_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[php]
opcache_memory_consumption = 256
opcache_interned_strings_buffer = 24
opcache_jit_buffer_size = 48
opcache_max_accelerated_files = 30000
php_memory_limit = "256M"
realpath_cache_size = "32M"
realpath_cache_ttl = 900
zend_assertions = 0
"#,
        )
        .unwrap();
        let config = Config::load(&file).unwrap();
        assert_eq!(config.php.opcache_memory_consumption, Some(256));
        assert_eq!(config.php.opcache_interned_strings_buffer, Some(24));
        assert_eq!(config.php.opcache_jit_buffer_size, Some(48));
        assert_eq!(config.php.opcache_max_accelerated_files, Some(30000));
        assert_eq!(config.php.php_memory_limit.as_deref(), Some("256M"));
        assert_eq!(config.php.realpath_cache_size.as_deref(), Some("32M"));
        assert_eq!(config.php.realpath_cache_ttl, Some(900));
        assert_eq!(config.php.zend_assertions, Some(0));
    }

    #[test]
    fn test_opcache_config_loads_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r"
[php]
opcache_validate_timestamps = true
opcache_revalidate_freq = 60
",
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        assert_eq!(config.php.opcache_validate_timestamps, Some(true));
        assert_eq!(config.php.opcache_revalidate_freq, Some(60));
    }

    // ── Worker mode config ──────────────────────────────────────────────

    #[test]
    fn test_php_mode_defaults_to_fpm() {
        let cfg = PhpConfig::default();
        assert_eq!(cfg.mode, "fpm");
        assert!(!cfg.is_worker_mode());
        assert_eq!(cfg.worker_count, 0);
        assert_eq!(cfg.worker_max_requests, 10_000);
        assert_eq!(cfg.worker_backlog, 0);
        assert_eq!(cfg.worker_boot_timeout, 30);
        assert!(!cfg.worker_populate_superglobals);
        assert_eq!(cfg.worker_stream_threshold, 1024 * 1024);
        assert!(cfg.worker_script.is_none());
    }

    #[test]
    fn test_is_worker_mode_case_insensitive() {
        let mut cfg = PhpConfig { mode: "Worker".to_string(), ..PhpConfig::default() };
        assert!(cfg.is_worker_mode());
        cfg.mode = "WORKER".to_string();
        assert!(cfg.is_worker_mode());
        cfg.mode = "fpm".to_string();
        assert!(!cfg.is_worker_mode());
    }

    #[test]
    fn test_effective_worker_count_derives_and_clamps() {
        // Explicit value passes through.
        let mut cfg = PhpConfig { worker_count: 7, ..PhpConfig::default() };
        assert_eq!(cfg.effective_worker_count(), 7);
        assert!(matches!(cfg.effective_worker_count_with_source().1, WorkerCountSource::Explicit));
        // Derived value is never zero; upper bound is [1, 32] (cgroup path may
        // return 1 inside a CPU-limited container, otherwise clamp is [2, 32]).
        cfg.worker_count = 0;
        let derived = cfg.effective_worker_count();
        assert!((1..=32).contains(&derived), "derived worker count out of range: {derived}");
    }

    #[test]
    fn test_parse_cgroup_v2_cpu_max() {
        // 25% of one core: 0.25 CPU units, ceil() -> 1 worker.
        assert!((parse_cgroup_v2_cpu_max("25000 100000").unwrap() - 0.25).abs() < 1e-9);
        // Exactly one core.
        assert!((parse_cgroup_v2_cpu_max("100000 100000").unwrap() - 1.0).abs() < 1e-9);
        // 2.5 cores.
        assert!((parse_cgroup_v2_cpu_max("250000 100000").unwrap() - 2.5).abs() < 1e-9);
        // Unlimited.
        assert_eq!(parse_cgroup_v2_cpu_max("max 100000"), None);
        assert_eq!(parse_cgroup_v2_cpu_max("MAX 100000"), None);
        // Trailing newline (real cgroupfs writes always include one).
        assert!((parse_cgroup_v2_cpu_max("25000 100000\n").unwrap() - 0.25).abs() < 1e-9);
        // Malformed / degenerate.
        assert_eq!(parse_cgroup_v2_cpu_max(""), None);
        assert_eq!(parse_cgroup_v2_cpu_max("only-one-word"), None);
        assert_eq!(parse_cgroup_v2_cpu_max("abc def"), None);
        assert_eq!(parse_cgroup_v2_cpu_max("100000 0"), None);
    }

    #[test]
    fn test_parse_cgroup_v1_cpu() {
        assert!((parse_cgroup_v1_cpu("25000", "100000").unwrap() - 0.25).abs() < 1e-9);
        assert!((parse_cgroup_v1_cpu("100000\n", "100000\n").unwrap() - 1.0).abs() < 1e-9);
        // -1 = unlimited (v1 sentinel).
        assert_eq!(parse_cgroup_v1_cpu("-1", "100000"), None);
        // Period 0 -> would divide by zero.
        assert_eq!(parse_cgroup_v1_cpu("100000", "0"), None);
        assert_eq!(parse_cgroup_v1_cpu("junk", "100000"), None);
    }

    #[test]
    fn test_worker_count_source_ceiling() {
        // Small quotas ceil to 1, fractional quotas above 1 ceil upward.
        // We can't force read_cgroup_cpu_quota() in tests, so exercise the
        // ceil() math via the parser results directly. Ceiled quotas are
        // always positive here (the parser returns None for <=0), so the
        // f64 -> u64 cast is sign- and range-safe.
        fn ceil_u64(q: f64) -> u64 {
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            let v = q.ceil() as u64;
            v
        }
        assert_eq!(parse_cgroup_v2_cpu_max("25000 100000").map(ceil_u64), Some(1));
        assert_eq!(parse_cgroup_v2_cpu_max("100000 100000").map(ceil_u64), Some(1));
        assert_eq!(parse_cgroup_v2_cpu_max("150000 100000").map(ceil_u64), Some(2));
        assert_eq!(parse_cgroup_v2_cpu_max("400000 100000").map(ceil_u64), Some(4));
    }

    #[test]
    fn test_effective_worker_backlog() {
        let mut cfg = PhpConfig { worker_count: 4, worker_backlog: 0, ..PhpConfig::default() };
        assert_eq!(cfg.effective_worker_backlog(), 4);
        cfg.worker_backlog = 16;
        assert_eq!(cfg.effective_worker_backlog(), 16);
    }

    #[test]
    fn test_worker_fields_parse_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[php]
mode = "worker"
worker_script = "worker.php"
worker_count = 8
worker_max_requests = 1000
worker_backlog = 12
worker_boot_timeout = 45
worker_populate_superglobals = true
worker_stream_threshold = 262144
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        assert!(config.php.is_worker_mode());
        assert_eq!(config.php.worker_script, Some(PathBuf::from("worker.php")));
        assert_eq!(config.php.worker_count, 8);
        assert_eq!(config.php.worker_max_requests, 1000);
        assert_eq!(config.php.worker_backlog, 12);
        assert_eq!(config.php.worker_boot_timeout, 45);
        assert!(config.php.worker_populate_superglobals);
        assert_eq!(config.php.worker_stream_threshold, 262_144);
    }

    #[test]
    fn test_validate_fpm_mode_always_ok() {
        let cfg = Config::default();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_worker_mode_missing_script_errors() {
        let mut cfg = Config::default();
        cfg.php.mode = "worker".to_string();
        cfg.php.worker_script = None;
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
        assert!(format!("{err}").contains("worker_script"));
    }

    #[test]
    fn test_validate_worker_mode_nonexistent_script_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.server.document_root = dir.path().to_path_buf();
        cfg.php.mode = "worker".to_string();
        cfg.php.worker_script = Some(PathBuf::from("does-not-exist.php"));
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }

    #[test]
    fn test_validate_worker_mode_valid_script_ok() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("worker.php");
        std::fs::write(&script, "<?php // loop").unwrap();

        let mut cfg = Config::default();
        cfg.server.document_root = dir.path().to_path_buf();
        cfg.php.mode = "worker".to_string();
        cfg.php.worker_script = Some(PathBuf::from("worker.php"));

        cfg.validate().expect("valid worker config");
        let resolved = cfg.resolve_worker_script().unwrap();
        assert!(resolved.is_file());
        assert!(resolved.ends_with("worker.php"));
    }

    #[test]
    fn test_validate_worker_mode_script_outside_docroot_errors() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let script = outside.path().join("worker.php");
        std::fs::write(&script, "<?php // loop").unwrap();

        let mut cfg = Config::default();
        cfg.server.document_root = root.path().to_path_buf();
        cfg.php.mode = "worker".to_string();
        // Absolute path pointing outside document_root.
        cfg.php.worker_script = Some(script.clone());
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
        assert!(format!("{err}").contains("outside document_root"));
    }

    #[test]
    fn test_validate_worker_mode_sites_dir_conflict_errors() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("worker.php");
        std::fs::write(&script, "<?php // loop").unwrap();

        let mut cfg = Config::default();
        cfg.server.document_root = dir.path().to_path_buf();
        cfg.server.sites_dir = Some(PathBuf::from("/var/www/sites"));
        cfg.php.mode = "worker".to_string();
        cfg.php.worker_script = Some(PathBuf::from("worker.php"));
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
        assert!(format!("{err}").contains("sites_dir"));
    }

    #[test]
    fn test_validate_rejects_unknown_php_mode() {
        // A typo like "workr" must hard-error, not silently mean fpm.
        let mut cfg = Config::default();
        cfg.php.mode = "workr".to_string();
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
        assert!(format!("{err}").contains("mode"));

        cfg.php.mode = "fpm".to_string();
        assert!(cfg.validate().is_ok());
    }

    // ── [db.analysis] metric_label_series_max ─────────────────────────
    //
    // Wired into StatsConfig::metric_label_series_max at
    // ephpm-server/src/lib.rs so a change to the config actually bounds
    // Prometheus digest-label cardinality. Both the default and an
    // explicit override must parse; 0 = unlimited.

    #[test]
    fn test_db_analysis_metric_label_series_max_default() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        // No [db.analysis] block at all — the default must land at 1000.
        std::fs::write(&file, "").unwrap();
        let config = Config::load(&file).unwrap();
        assert_eq!(config.db.analysis.metric_label_series_max, 1000);
    }

    #[test]
    fn test_db_analysis_metric_label_series_max_override_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r"
[db.analysis]
metric_label_series_max = 250
",
        )
        .unwrap();
        let config = Config::load(&file).unwrap();
        assert_eq!(config.db.analysis.metric_label_series_max, 250);
    }

    #[test]
    fn test_db_analysis_metric_label_series_max_zero_is_unlimited() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r"
[db.analysis]
metric_label_series_max = 0
",
        )
        .unwrap();
        let config = Config::load(&file).unwrap();
        // 0 is the documented "unlimited" sentinel — parses as 0 and is
        // interpreted by the query-stats crate as no cap.
        assert_eq!(config.db.analysis.metric_label_series_max, 0);
    }

    #[test]
    fn test_db_analysis_metric_label_series_max_env_var_override() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "").unwrap();

        temp_env::with_var("EPHPM_DB__ANALYSIS__METRIC_LABEL_SERIES_MAX", Some("5000"), || {
            let config = Config::load(&file).unwrap();
            assert_eq!(config.db.analysis.metric_label_series_max, 5000);
        });
    }
}
