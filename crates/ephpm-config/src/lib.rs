use std::path::PathBuf;

use figment::Figment;
use figment::providers::{Env, Format, Toml};
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to load configuration: {0}")]
    Load(#[from] Box<figment::Error>),
}

/// Top-level ePHPm configuration.
#[derive(Debug, Deserialize)]
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
    #[serde(default)]
    pub security: SecurityConfig,

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

    /// Idle connection timeout in seconds. Connections with no activity
    /// for this duration are closed.
    ///
    /// Default: 60 seconds.
    #[serde(default = "default_idle")]
    pub idle: u64,

    /// Total request processing timeout in seconds. Covers the entire
    /// request lifecycle including PHP execution.
    ///
    /// Default: 300 seconds (5 minutes).
    #[serde(default = "default_request_timeout")]
    pub request: u64,
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
    /// Default: `true` when `sites_dir` is set, `false` otherwise.
    /// Override per-site via `site.toml`.
    #[serde(default = "default_open_basedir")]
    pub open_basedir: bool,

    /// Disable dangerous PHP functions in multi-tenant mode.
    ///
    /// When `true`, `exec`, `shell_exec`, `system`, `passthru`,
    /// `proc_open`, `popen`, and `pcntl_exec` are disabled via
    /// `disable_functions`. Prevents shell escape from `open_basedir`.
    ///
    /// Default: `true` when `sites_dir` is set, `false` otherwise.
    /// Override per-site via `site.toml`.
    #[serde(default = "default_disable_shell_exec")]
    pub disable_shell_exec: bool,
}

fn default_open_basedir() -> bool {
    true
}

fn default_disable_shell_exec() -> bool {
    true
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
        Self {
            enabled: false,
            path: default_metrics_path(),
        }
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
    /// only have metadata cached (size, mtime, ETag, MIME type).
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

    /// Embedded SQLite configuration (via litewire).
    ///
    /// When enabled, starts an in-process SQLite database with MySQL/Hrana
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

/// Embedded SQLite database configuration (`[db.sqlite]`).
///
/// Uses litewire to expose SQLite via MySQL wire protocol, so PHP apps
/// can use their existing `pdo_mysql` drivers transparently.
#[derive(Debug, Deserialize, Clone)]
pub struct SqliteConfig {
    /// Path to the SQLite database file.
    ///
    /// Default: `"ephpm.db"` in the current working directory.
    #[serde(default = "default_sqlite_path")]
    pub path: String,

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

/// Wire protocol frontend addresses for the SQLite proxy (`[db.sqlite.proxy]`).
#[derive(Debug, Deserialize, Clone)]
pub struct SqliteProxyConfig {
    /// MySQL wire protocol listen address.
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
        Self {
            http_listen: default_sqld_http_listen(),
            grpc_listen: default_sqld_grpc_listen(),
        }
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
        Self {
            role: default_replication_role(),
            primary_grpc_url: String::new(),
        }
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

    /// Unix socket path for the proxy listener (faster than TCP for local PHP).
    ///
    /// When set, the proxy also listens on this socket in addition to `listen`.
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
    /// - `"lag-aware"` — skip replicas whose replication lag exceeds
    ///   `max_replica_lag`.
    ///
    /// Default: `"sticky-after-write"`.
    #[serde(default = "default_rw_strategy")]
    pub strategy: String,

    /// Duration string: after a write, how long reads stick to the primary.
    ///
    /// Default: `"2s"`.
    #[serde(default = "default_sticky_duration")]
    pub sticky_duration: String,

    /// Duration string: maximum acceptable replication lag (lag-aware strategy).
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

    /// Enable automatic `EXPLAIN` on slow queries.
    ///
    /// When enabled, the proxy automatically runs `EXPLAIN` on queries that
    /// exceed the slow query threshold.
    ///
    /// Default: `false`.
    #[serde(default)]
    pub auto_explain: bool,

    /// Output target for `EXPLAIN` analysis results.
    ///
    /// Values: `"stderr"`, `"stdout"`.
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
}

impl Default for DbAnalysisConfig {
    fn default() -> Self {
        Self {
            query_stats: default_query_stats_enabled(),
            slow_query_threshold: default_slow_query_threshold(),
            auto_explain: false,
            auto_explain_target: default_auto_explain_target(),
            digest_store_max_entries: default_digest_max_entries(),
        }
    }
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

    /// Optional Unix socket path (faster than TCP for local connections).
    #[serde(default)]
    pub socket: Option<String>,
}

impl Default for KvRedisCompatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: default_kv_listen(),
            socket: None,
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

    /// Number of dedicated PHP worker threads.
    ///
    /// Each worker runs in its own OS thread with PHP TLS initialized,
    /// allowing true concurrent PHP request execution.
    ///
    /// Default: logical CPU count, capped at 16.
    #[serde(default = "default_php_workers")]
    pub workers: usize,
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
        let config =
            Figment::new().merge(Env::prefixed("EPHPM_").split("__")).extract().map_err(Box::new)?;
        Ok(config)
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            document_root: default_document_root(),
            sites_dir: None,
            index_files: default_index_files(),
            fallback: default_fallback(),
            request: RequestConfig::default(),
            timeouts: TimeoutsConfig::default(),
            response: ResponseConfig::default(),
            static_files: StaticConfig::default(),
            php_etag_cache: PhpETagCacheConfig::default(),
            security: SecurityConfig::default(),
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
        }
    }
}

impl Default for ResponseConfig {
    fn default() -> Self {
        Self {
            compression: default_compression(),
            compression_level: default_compression_level(),
            compression_min_size: default_compression_min_size(),
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
        Self {
            access: String::new(),
            level: default_log_level(),
        }
    }
}

impl Default for PhpConfig {
    fn default() -> Self {
        Self {
            max_execution_time: default_max_execution_time(),
            memory_limit: default_memory_limit(),
            ini_file: None,
            ini_overrides: Vec::new(),
            workers: default_php_workers(),
        }
    }
}

fn default_sqlite_path() -> String {
    "ephpm.db".to_string()
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

    /// Base64-encoded 32-byte symmetric key for gossip encryption.
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

    /// Number of replica copies for large keys.
    #[serde(default = "default_replication_factor")]
    pub replication_factor: usize,

    /// Replication mode for large keys (`"async"` or `"sync"`).
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
        }
    }
}

fn default_php_workers() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get().min(16))
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
    vec![
        "$uri".to_string(),
        "$uri/".to_string(),
        "/index.php?$query_string".to_string(),
    ]
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
        assert_eq!(
            config.php.ini_overrides[1],
            ["error_reporting", "E_ALL"]
        );
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
            r#"
[server.php_etag_cache]
enabled = true
ttl_secs = -1
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        assert!(config.server.php_etag_cache.enabled);
        assert_eq!(config.server.php_etag_cache.ttl_secs, -1);
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
            r#"
[server.php_etag_cache]
enabled = false
"#,
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
        assert_eq!(
            sqlite.replication.primary_grpc_url,
            "http://10.0.1.2:5001"
        );
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

        temp_env::with_var(
            "EPHPM_DB__SQLITE__REPLICATION__ROLE",
            Some("primary"),
            || {
                let config = Config::load(&file).unwrap();
                let sqlite = config.db.sqlite.expect("sqlite should be present");
                assert_eq!(sqlite.replication.role, "primary");
            },
        );
    }
}
