use std::path::{Component, Path, PathBuf};
use std::process;

use figment::Figment;
use figment::providers::{Env, Format, Toml};
use serde::Deserialize;

#[cfg(test)]
mod test_env;

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
///
/// # Unknown keys are rejected — per section, not at the root
///
/// **Every section struct is `#[serde(deny_unknown_fields)]`**, so an
/// unrecognized key under `[server]`, `[php]`, `[db.*]`, `[kv]`, `[cluster]`,
/// `[opcache]` or `[[middleware]]` fails startup with an error naming the key.
/// A key this binary does not declare is far more likely to be a typo, or a
/// knob from a newer version, than something safe to ignore — and ignoring it
/// silently turns an operator's explicit instruction into a no-op that every
/// health check reports green. That is the defect #429 hit with
/// `[db.sqlite.replication] per_site`; this generalizes the fix.
///
/// Knobs that were **removed** stay *declared* (see [`DeprecatedSqldConfig`]
/// and `ReplicationConfig::cdc_experimental`) so upgrading configs keep
/// parsing and get a warning or a migration message, not a bare "unknown
/// field".
///
/// ## Two deliberate exceptions
///
/// 1. **This struct — the root — is lenient.** `Config::load` merges
///    `Env::prefixed("EPHPM_")`, which is unfiltered: *every* `EPHPM_*`
///    variable in the environment becomes a top-level key, including ones that
///    are not configuration. ePHPm sets one itself — the Windows service
///    wrapper exports `EPHPM_SERVICE_LOG_FILE` before the server starts, which
///    figment turns into a top-level `service_log_file` — and the e2e harness
///    sets `EPHPM_URL` / `EPHPM_BINARY`. A strict root would make ePHPm refuse
///    to start as a Windows service. Nested sections have no such exposure:
///    nothing sets an `EPHPM_*` variable containing the `__` nesting
///    separator, so every key that reaches a section came from an operator
///    asking for something. Pinned by
///    `config_root_stays_lenient_so_non_config_env_vars_cannot_block_startup`.
/// 2. **[`DeprecatedSqldConfig`] is lenient**, because tolerating keys this
///    binary no longer declares is its entire purpose.
///
/// Adding a section means adding a case to
/// `unknown_keys_are_rejected_in_every_strict_section`.
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

/// One middleware mount (`[[middleware]]`).
///
/// ```toml
/// [[middleware]]
/// library = "rate-limit"
/// match = "/api/*"
/// order = 20
/// config = { per_ip_rps = 50, burst = 100 }
///
/// # EXPERIMENTAL: plain-PHP middleware, no compiler and no shared library.
/// [[middleware]]
/// library = "php:middleware.php"
/// match = "/api/*"
/// order = 30
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MiddlewareMount {
    /// Module to run. Resolved in three steps:
    ///
    /// 1. A `php:` prefix selects the **PHP middleware lane** (EXPERIMENTAL) —
    ///    the rest of the value is a path to a `.php` file, relative to the
    ///    request's document root. See [`MiddlewareMount::php_script`].
    /// 2. Otherwise the value is checked against the builtin registry (`jwt`,
    ///    `cors`, `ratelimit`/`rate-limit`, `security-headers` and their
    ///    `ephpm-middleware-*` long forms are compiled into every binary — no
    ///    dlopen).
    /// 3. Anything else is a shared library: either a bare name (resolved
    ///    through the middleware search path with a platform suffix, e.g.
    ///    `auth-jwt` → `auth-jwt.linux-x86_64.so`) or an explicit path — a
    ///    value containing a path separator or a file extension is used as-is.
    ///
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
    /// passed to its `init`. On a `php:` mount the same JSON is what
    /// `ephpm_middleware_config()` returns to the script.
    ///
    /// Default: unset (the module's `init` receives NULL; a `php:` mount's
    /// `ephpm_middleware_config()` returns `null`).
    #[serde(default)]
    pub config: Option<serde_json::Value>,
}

/// Prefix that selects the PHP middleware lane in `library`.
pub const PHP_MIDDLEWARE_PREFIX: &str = "php:";

/// Maximum PHP middleware mounts that may run for one request.
///
/// Matches `MAX_REQUEST_MIDDLEWARE` in `crates/ephpm-php/ephpm_wrapper.c`;
/// [`Config::validate`] refuses a config that declares more so the C-side cap
/// can never silently drop a mount.
pub const MAX_PHP_MIDDLEWARE: usize = 16;

impl MiddlewareMount {
    /// The document-root-relative script path when this is a `php:` mount.
    ///
    /// `None` for builtin and shared-library mounts. The returned path is the
    /// raw configured value; [`Config::validate`] has already rejected the
    /// unsafe shapes (empty, absolute, `..`, backslash, Windows drive prefix),
    /// so a value that survives validation can be joined onto a document root
    /// without escaping it.
    #[must_use]
    pub fn php_script(&self) -> Option<&str> {
        self.library.strip_prefix(PHP_MIDDLEWARE_PREFIX)
    }
}

/// HTTP server configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// The `Host` value is normalized (port/trailing-dot stripped, lowercased)
    /// and validated against a strict DNS-label allowlist before it is joined
    /// onto this directory, so a crafted header cannot escape it via `..` or a
    /// path separator (see the router's `is_valid_site_key`). Malformed hosts
    /// are rejected with 404 independently of `[server.request] trusted_hosts`.
    ///
    /// Omit to disable vhosting (single-site mode).
    #[serde(default)]
    pub sites_dir: Option<PathBuf>,

    /// **Multi-tenant only.** Directory of **operator-supplied per-site
    /// overrides**, one file per virtual host.
    ///
    /// A vhost directory under [`sites_dir`](Self::sites_dir) is the site
    /// **container** — the whole checkout, including the parts that must not be
    /// reachable over HTTP. Modern PHP frameworks keep their front controller in
    /// a subdirectory (`public/`, `web/`, `htdocs/`, `public_html/`) precisely so
    /// that `composer.json`, `vendor/`, `config/` and `storage/logs/` sit *above*
    /// the web root; serving the container itself publishes all of them. An
    /// override file is how the *operator* (or the provisioning daemon that laid
    /// out the checkout) tells ePHPm a site's real web root:
    ///
    /// ```toml
    /// # <site_overrides_dir>/<site-key>.toml
    /// document_root = "web"   # relative to the site container
    /// ```
    ///
    /// A site with no override file behaves **exactly** as it always has: the
    /// container is the document root. Nothing about an existing deployment
    /// changes until an override is written.
    ///
    /// # This directory is operator-owned and MUST NOT be tenant-writable
    ///
    /// That is the entire security property, so it is worth stating bluntly:
    ///
    /// * **This is not the application manifest.** It is not `ephpm.yaml`, and
    ///   ePHPm never reads anything from inside a tenant's checkout to decide
    ///   routing. A provisioning daemon that consumes an application manifest is
    ///   welcome to *derive* these files from it; ePHPm only ever reads the
    ///   derived, operator-owned artifact.
    /// * **It must live outside `sites_dir`.** A file inside a site container is
    ///   inside that tenant's `open_basedir` by construction, so the tenant's own
    ///   PHP can rewrite it. Startup fails closed if this path is inside
    ///   `sites_dir`.
    /// * Ordinary filesystem permissions are what keep it operator-owned; ePHPm
    ///   cannot verify that for you beyond the containment check above.
    ///
    /// # It can never widen `open_basedir`
    ///
    /// An override may only *narrow* which files are served. `open_basedir`
    /// stays the site container, and is structurally unreachable from these
    /// files. All tenants share one process and one uid (see
    /// [`run_as_user`](Self::run_as_user)), so `open_basedir` is the primary
    /// cross-tenant boundary and is not per-site configurable.
    ///
    /// The declared `document_root` is still validated as if hostile — relative,
    /// no `..`, canonicalized and required to resolve inside the site container.
    /// "The daemon validated it" is a claim about another codebase's current
    /// behaviour, not an invariant ePHPm can enforce. A rejected or malformed
    /// override logs a warning and the site serves its container.
    ///
    /// # File naming
    ///
    /// `<site_overrides_dir>/<site-key>.toml`, where `<site-key>` is the
    /// **canonical site key** — the same validated `[a-z0-9._-]` string that
    /// names the vhost directory under `sites_dir`, selects `<dir>/<key>.db`, and
    /// derives the `pdo_mysql` credential. A file whose name does not match a
    /// site key is simply never read: the site serves its container, silently.
    ///
    /// Unset (the default) disables the mechanism entirely. Ignored in
    /// single-site mode (`sites_dir` unset), where `document_root` already *is*
    /// the web root and there are no tenants to distinguish.
    ///
    /// Default: unset. Env: `EPHPM_SERVER__SITE_OVERRIDES_DIR`.
    #[serde(default)]
    pub site_overrides_dir: Option<PathBuf>,

    /// Unprivileged user to drop to after binding privileged ports and
    /// opening root-owned files (Unix only).
    ///
    /// Accepts a numeric uid (e.g. `"1000"`) or a username resolved via
    /// `getpwnam` (e.g. `"www-data"`). When set and the process starts as
    /// root, ePHPm binds every listener, starts the DB proxies, and opens the
    /// generated php.ini **as root**, then permanently drops to this uid
    /// (and [`run_as_group`](Self::run_as_group)) with `setgroups` +
    /// `setgid` + `setuid` before it begins serving. The drop is
    /// process-wide (glibc broadcasts it to every thread) and irreversible —
    /// startup fails closed if the effective uid is still 0 afterwards.
    ///
    /// **This is a single non-root uid for the whole process, not a
    /// per-tenant uid.** It removes the root-escalation blast radius (a
    /// PHP/FFI compromise no longer runs as root), but every tenant still
    /// shares this one uid, so cross-tenant isolation still rests on
    /// `open_basedir` + the `disable_functions` denylist, not on kernel
    /// permissions. See the multi-tenant guide.
    ///
    /// Before dropping, ePHPm `chown`s the directories it must keep writing
    /// after the drop to the target uid/gid: `[db.sqlite] dir` (per-site
    /// database files), the per-vhost temp/session base
    /// (`<tmpdir>/ephpm-vhosts`), and the ACME cache directory when TLS-ACME
    /// is configured.
    ///
    /// Default: unset (no privilege drop — the process keeps whatever uid it
    /// was started with). Ignored with a startup warning on Windows and when
    /// the process is not running as root.
    #[serde(default)]
    pub run_as_user: Option<String>,

    /// Unprivileged group to drop to alongside
    /// [`run_as_user`](Self::run_as_user) (Unix only).
    ///
    /// Accepts a numeric gid or a group name resolved via `getgrnam`. Only
    /// consulted when `run_as_user` is set. When omitted, the target group
    /// is the user's primary group (for a named user) or the same numeric id
    /// as the uid (for a numeric user). Supplementary groups are dropped.
    ///
    /// Default: unset.
    #[serde(default)]
    pub run_as_group: Option<String>,

    /// Optional domain suffix to strip from incoming `Host` headers when
    /// resolving vhosts. When set (e.g. `.localhost`), a directory named
    /// `~/sites/blog/` matches `Host: blog.localhost` — the suffix is
    /// stripped before the registry lookup and the on-disk lazy fallback.
    ///
    /// **Must begin with a dot.** `Config::validate` rejects a dotless suffix at
    /// startup (issue #397): the leading dot is what makes only a genuine
    /// subdomain match. A dotless suffix would let the apex host `Host: <suffix>`
    /// strip to the empty vhost key, which resolves the whole `sites_dir` as one
    /// virtual host — collapsing every tenant into a single `open_basedir`.
    ///
    /// Primarily used by `ephpm dev --sites` so developers can keep short
    /// directory names while testing with `*.localhost` URLs. Production
    /// deployments typically leave this unset and name directories with
    /// the full FQDN (`~/sites/blog.example.com/`).
    ///
    /// The stripped name is the tenant's **whole** identity, not just its
    /// document-root lookup key: `Host: blog.localhost` and `Host: blog` are
    /// one tenant and therefore share one database, one private temp/session
    /// directory, one KV keyspace and one set of `DB_*` credentials. (Before
    /// issue #290 the database key was derived without stripping the suffix, so
    /// the same tenant silently used two database files depending on how it was
    /// addressed.)
    #[serde(default)]
    pub sites_domain_suffix: Option<String>,

    /// Index file names to try when a directory is requested.
    #[serde(default = "default_index_files")]
    pub index_files: Vec<String>,

    /// Entrypoint script names to try, in order, when a WebSocket upgrade
    /// request arrives — the `index_files` of the WebSocket path.
    ///
    /// Resolved against the **vhost's** document root, so each virtual host has
    /// its own WebSocket handler (or none). The first name that exists on disk
    /// wins and receives every event for connections upgraded on that vhost:
    /// `connect`, `message` and `disconnect`, distinguished by
    /// `$_SERVER['WS_EVENT']`.
    ///
    /// If **no** name in this list exists in the resolved document root, the
    /// upgrade request is answered `404` — it never falls through to static
    /// files, `index.php`, or the `[server] fallback` chain. A vhost that has
    /// not opted into WebSockets therefore cannot accidentally serve one.
    ///
    /// Only consulted when `[server.websocket] enabled = true`; with the
    /// feature off, upgrade requests are routed exactly as any other GET.
    ///
    /// Default: `["websocket.php"]`. Env override:
    /// `EPHPM_SERVER__WEBSOCKET_FILES`.
    #[serde(default = "default_websocket_files")]
    pub websocket_files: Vec<String>,

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

    /// Preview-host preset (`[server] preview = true`).
    ///
    /// One switch that makes an instance sane to run as a **not-production**
    /// PR-preview host. When `true`:
    ///
    /// - Every `[server.limits]` knob that the operator did NOT set explicitly
    ///   resolves to a preview default instead of "off":
    ///   `max_connections = 256`, `per_ip_max_connections = 32`,
    ///   `per_ip_rate = 10.0`, `per_ip_burst = 50`, `per_site_rate = 5.0`,
    ///   `per_site_burst = 20`. An explicitly set value always wins — including
    ///   an explicit `0`/`0.0`, which disables that limit even under preview.
    ///   See [`ServerConfig::effective_limits`].
    /// - Every HTTP response carries `X-Ephpm-Preview: 1`, so a preview
    ///   instance can never be mistaken for production by tooling or humans.
    ///
    /// Startup logs exactly which limits the preset supplied and which were
    /// operator-set (never silent). Env override: `EPHPM_SERVER__PREVIEW=true`.
    ///
    /// Default: `false` (no preset, no marker header; `[server.limits]`
    /// resolves to its regular all-off defaults).
    #[serde(default)]
    pub preview: bool,

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

    /// Per-vhost kernel network policy (Linux-only). See
    /// [`TenantNetworkConfig`]. Default: disabled — zero cost, no BPF loaded.
    #[serde(default)]
    pub tenant_network: TenantNetworkConfig,

    /// Logging settings.
    #[serde(default)]
    pub logging: LoggingConfig,

    /// Metrics / observability settings.
    #[serde(default)]
    pub metrics: MetricsConfig,

    /// Per-request diagnostics settings.
    #[serde(default)]
    pub diagnostics: DiagnosticsConfig,

    /// Rate limiting and connection limiting.
    #[serde(default)]
    pub limits: LimitsConfig,

    /// Open file cache for static file serving.
    #[serde(default)]
    pub file_cache: FileCacheConfig,

    /// TLS configuration. When present, enables HTTPS.
    #[serde(default)]
    pub tls: Option<TlsConfig>,

    /// HTTP/3 (QUIC) settings.
    #[serde(default)]
    pub http3: Http3Config,

    /// Native WebSocket support (experimental, off by default).
    #[serde(default)]
    pub websocket: WebSocketConfig,

    /// Built-in reverse-proxy rules (`[[server.proxy]]`), evaluated in order.
    ///
    /// Each rule matches on host and path and forwards a matched request to a
    /// single upstream (a single-hop forwarder, **not** an edge load balancer).
    /// Rules are tried top-to-bottom and the **first match wins**; a matched
    /// rule short-circuits all local serving (static files, PHP, native
    /// WebSocket termination). An empty list (the default) disables the feature
    /// entirely — the request path pays one `slice::is_empty()`.
    ///
    /// See [`ProxyRuleConfig`] for the per-rule fields and their validation.
    #[serde(default)]
    pub proxy: Vec<ProxyRuleConfig>,
}

/// One reverse-proxy rule (`[[server.proxy]]`).
///
/// A rule matches on **host** and **path** and forwards the request to one
/// **upstream**. Matching, precedence and the "single-hop forwarder, not a load
/// balancer" scope are described on [`ServerConfig::proxy`].
///
/// # Host matcher (`host`)
///
/// One string, whose syntax selects the match kind (unambiguous on sight):
///
/// | `host` value            | matches                                            |
/// |-------------------------|----------------------------------------------------|
/// | `"app.example.com"`     | exact (case-insensitive; port/trailing-dot ignored)|
/// | `"*.example.com"`       | wildcard — exactly one leftmost label              |
/// | `".example.com"`        | suffix — the apex and any subdomain                |
/// | `"*"` or omitted        | any host                                           |
///
/// # Path matcher (`path` + `path_exact`)
///
/// `path` is a **prefix** by default (`/api` matches `/api`, `/api/v1`, …); the
/// default `"/"` matches every path. Set `path_exact = true` to require the
/// request path to equal `path` exactly. Two explicit fields — never a marker
/// smuggled into the string.
///
/// # Upstream (`upstream`)
///
/// A `http://host[:port]` URL — **scheme + authority only**. Validated at
/// startup (fail-closed, so a mistake never becomes a silent no-op):
///
/// * `https://` upstreams are a **hard error in v1** — TLS to the upstream
///   (client cert stores, SNI) is out of scope; v1 targets loopback plaintext
///   backends. Terminate TLS at this instance and proxy plaintext.
/// * A **path/query/fragment** in the URL is a **hard error in v1** — v1
///   forwards the original request path unchanged and does **no** request-path
///   rewriting (prefix strip/prepend). Give host+port only.
///
/// # Timeouts
///
/// `connect_timeout_secs` bounds the TCP connect to the upstream; a rule that
/// matches an unreachable upstream returns `502 Bad Gateway` within this bound
/// rather than hanging. `read_timeout_secs` bounds the time to receive the
/// upstream **response head** (time-to-first-byte); it deliberately does **not**
/// bound the streamed response body, so Server-Sent Events and long downloads
/// are not cut off. The outer `[server.timeouts] request` ceiling still applies
/// to the whole request exactly as it does to every handler.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyRuleConfig {
    /// Host matcher. `None` (omitted) or `"*"` matches any host. See the type
    /// docs for the exact/wildcard/suffix syntax.
    #[serde(default)]
    pub host: Option<String>,

    /// Path matcher. A prefix by default (see [`path_exact`](Self::path_exact)).
    ///
    /// Default: `"/"` (every path). Must begin with `/`.
    #[serde(default = "default_proxy_path")]
    pub path: String,

    /// When `true`, [`path`](Self::path) must equal the request path exactly
    /// rather than being a prefix.
    ///
    /// Default: `false` (prefix match).
    #[serde(default)]
    pub path_exact: bool,

    /// Upstream URL — `http://host[:port]`, scheme + authority only.
    ///
    /// Required. `https://` and any path/query/fragment are startup errors in
    /// v1 (see the type docs).
    pub upstream: String,

    /// TCP connect timeout to the upstream, in seconds.
    ///
    /// A matched-but-unreachable upstream returns `502` within this bound. `0`
    /// means no explicit connect deadline (the OS default applies).
    ///
    /// Default: 5 seconds.
    #[serde(default = "default_proxy_connect_timeout")]
    pub connect_timeout_secs: u64,

    /// Time-to-first-byte timeout for the upstream response head, in seconds.
    ///
    /// Bounds how long ePHPm waits for the upstream to begin responding; it does
    /// **not** bound the streamed body (so SSE/long downloads are unaffected).
    /// `0` disables the head-read deadline (the outer `[server.timeouts] request`
    /// ceiling still applies).
    ///
    /// Default: 60 seconds.
    #[serde(default = "default_proxy_read_timeout")]
    pub read_timeout_secs: u64,
}

impl ProxyRuleConfig {
    /// Validate and normalize the [`upstream`](Self::upstream) URL, returning
    /// the bare `host[:port]` authority ePHPm forwards to.
    ///
    /// This is the single source of truth for the v1 upstream rules; both
    /// [`Config::validate`] (fail-closed at startup) and the server's rule
    /// compiler call it, so they can never disagree about what a valid upstream
    /// is.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message when the URL is not `http://`, uses
    /// `https://`, carries a path/query/fragment or userinfo, or has an empty
    /// or malformed authority — all of which are v1 scope errors, not no-ops.
    pub fn upstream_authority(&self) -> Result<String, String> {
        let raw = self.upstream.trim();
        if raw.strip_prefix("https://").is_some() {
            return Err(format!(
                "upstream {raw:?} uses https:// — TLS to the upstream is not supported in v1 \
                 (v1 targets loopback plaintext backends). Terminate TLS at this instance and \
                 use an http:// upstream."
            ));
        }
        let authority = raw.strip_prefix("http://").ok_or_else(|| {
            format!(
                "upstream {raw:?} must be an absolute http:// URL \
                 (e.g. \"http://127.0.0.1:9000\")"
            )
        })?;
        if authority.is_empty() {
            return Err(format!("upstream {raw:?} has no host"));
        }
        if let Some(pos) = authority.find(['/', '?', '#']) {
            return Err(format!(
                "upstream {raw:?} has a path/query/fragment ({rest:?}) — request-path \
                 rewriting (prefix strip/prepend) is not supported in v1; the original request \
                 path is forwarded unchanged. Give scheme + host + port only.",
                rest = &authority[pos..],
            ));
        }
        if authority.contains('@') {
            return Err(format!(
                "upstream {raw:?} contains userinfo ('@'), which is not supported"
            ));
        }
        // A lightweight authority sanity check (ephpm-config deliberately has no
        // `http` dependency): non-empty host, and if a port is present it must be
        // a valid `u16`. The server re-parses with `http::uri::Authority` as a
        // strict second gate.
        let host_part = if let Some(rest) = authority.strip_prefix('[') {
            // IPv6 literal: `[::1]` or `[::1]:9000`.
            let (inside, after) = rest
                .split_once(']')
                .ok_or_else(|| format!("upstream {raw:?} has an unterminated IPv6 literal"))?;
            if inside.is_empty() {
                return Err(format!("upstream {raw:?} has an empty IPv6 literal"));
            }
            if let Some(port) = after.strip_prefix(':') {
                validate_proxy_port(raw, port)?;
            } else if !after.is_empty() {
                return Err(format!("upstream {raw:?} has trailing junk after the IPv6 literal"));
            }
            inside
        } else if let Some((host, port)) = authority.rsplit_once(':') {
            validate_proxy_port(raw, port)?;
            host
        } else {
            authority
        };
        if host_part.is_empty() {
            return Err(format!("upstream {raw:?} has an empty host"));
        }
        Ok(authority.to_string())
    }

    /// Validate the [`host`](Self::host) matcher syntax.
    ///
    /// # Errors
    ///
    /// Returns a message when a wildcard host is not a single leftmost `*.`
    /// label, or when the host is otherwise empty.
    pub fn validate_host(&self) -> Result<(), String> {
        let Some(host) = self.host.as_deref() else { return Ok(()) };
        let host = host.trim();
        if host.is_empty() {
            return Err("host is empty — omit the key or use \"*\" to match any host".to_string());
        }
        if host == "*" || host.starts_with('.') {
            return Ok(());
        }
        if let Some(rest) = host.strip_prefix("*.") {
            if rest.is_empty() || rest.contains('*') {
                return Err(format!(
                    "host {host:?} is not a valid wildcard — a wildcard matches exactly one \
                     leftmost label, so it must look like \"*.example.com\""
                ));
            }
            return Ok(());
        }
        if host.contains('*') {
            return Err(format!(
                "host {host:?} contains a '*' that is not a leftmost \"*.\" label — the only \
                 wildcard form is a single leftmost label (\"*.example.com\")"
            ));
        }
        Ok(())
    }
}

/// Validate a proxy upstream port component.
fn validate_proxy_port(raw: &str, port: &str) -> Result<(), String> {
    if port.is_empty() {
        return Err(format!("upstream {raw:?} has an empty port after ':'"));
    }
    port.parse::<u16>()
        .map(|_| ())
        .map_err(|_| format!("upstream {raw:?} has an invalid port {port:?} (must be 0-65535)"))
}

/// Request limits configuration (`[server.request]`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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

    /// Maximum number of request-body bytes buffered and exposed to
    /// **request-phase** native middleware through the `request_body` ABI
    /// accessor.
    ///
    /// `0` (the default) **disables** body buffering: the middleware chain runs
    /// before any body byte is read — so a `RESPOND` verdict (auth deny, rate
    /// limit) never pays for the body transfer — and `request_body` returns
    /// empty. This preserves the reject-before-transfer property by default.
    ///
    /// When `> 0` **and** a `[[middleware]]` chain is mounted, the request body
    /// is buffered before the chain so middleware can inspect up to this many
    /// bytes (a longer body is truncated to this limit *for the middleware
    /// view only* — the complete body, subject to `max_body_size`, still
    /// reaches PHP unchanged). Enables webhook/HMAC signature verification,
    /// CSRF-with-body, and payload validation. Note: buffering the body up
    /// front bypasses worker-mode request streaming for such requests.
    ///
    /// Independent of `max_body_size`, which remains the hard cap that 413s an
    /// oversized body. Env override: `EPHPM_SERVER_REQUEST__MIDDLEWARE_BODY_LIMIT`.
    #[serde(default = "default_middleware_body_limit")]
    pub middleware_body_limit: u64,
}

/// Connection timeout configuration (`[server.timeouts]`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
    /// **Multi-tenant only.** The restriction is applied on the vhost
    /// request path in `ephpm-server`, which runs only when `sites_dir` is
    /// set: PHP's `open_basedir` is set per-request to the site's directory
    /// plus the system temp dir, so a site cannot read or write outside it.
    ///
    /// **In single-site mode (`sites_dir` unset) this flag does nothing** —
    /// no `open_basedir` is applied, whatever it resolves to. The
    /// multi-tenant value is the *site container* directory
    /// (`sites_dir/<host>`), which holds the whole application; a
    /// single-site `document_root` is the *web root*, and confining PHP to
    /// it would break every framework that keeps its code above the web
    /// root (Laravel/Symfony `require __DIR__.'/../vendor/autoload.php'`).
    /// So the mechanism is not transferable as-is, and enabling this knob
    /// in single-site mode logs a warning at startup instead of silently
    /// doing nothing. To sandbox a single-site deployment today, set PHP's
    /// own directive through `[php] ini_overrides` (it is written into the
    /// generated php.ini and read at MINIT):
    /// `ini_overrides = [["open_basedir", "/app:/tmp"]]`.
    ///
    /// An explicitly set value always wins. When unset, resolves to `true`
    /// if the `[server.security]` section is present OR `server.sites_dir`
    /// is set (multi-tenant mode); otherwise `false`. Use
    /// [`ServerConfig::effective_open_basedir`] to read the resolved value
    /// and [`ServerConfig::inert_security_flags`] to detect the
    /// enabled-but-inert case.
    #[serde(default)]
    pub open_basedir: Option<bool>,

    /// Disable dangerous PHP functions in multi-tenant mode.
    ///
    /// **Multi-tenant only.** When `true` *and* `sites_dir` is set,
    /// `exec`, `shell_exec`, `system`, `passthru`, `proc_open`, `popen`,
    /// and `pcntl_exec` are disabled via a `disable_functions` line in the
    /// php.ini ePHPm generates at startup. Prevents shell escape from
    /// `open_basedir`.
    ///
    /// **In single-site mode (`sites_dir` unset) this flag does nothing** —
    /// the `disable_functions` line is not emitted and the functions stay
    /// callable. Enabling it there logs a warning at startup. The
    /// equivalent single-site setting is
    /// `[php] ini_overrides = [["disable_functions", "exec,shell_exec,…"]]`,
    /// which lands in the same generated php.ini and takes effect at MINIT.
    ///
    /// An explicitly set value always wins. When unset, resolves to `true`
    /// if the `[server.security]` section is present OR `server.sites_dir`
    /// is set (multi-tenant mode); otherwise `false`. Use
    /// [`ServerConfig::effective_disable_shell_exec`] to read the resolved
    /// value and [`ServerConfig::inert_security_flags`] to detect the
    /// enabled-but-inert case.
    #[serde(default)]
    pub disable_shell_exec: Option<bool>,

    /// Apply the multi-tenant confidentiality/integrity hardening preset.
    ///
    /// **Multi-tenant only.** When `true` *and* `sites_dir` is set, ePHPm
    /// extends the generated php.ini with the denylist a hostile-tenant
    /// pentest proved closes every cross-tenant read/write channel that the
    /// shell-exec baseline alone leaves open:
    ///
    /// - `disable_functions` gains, on top of the shell-exec family:
    ///   `pcntl_*`, `posix_kill`/`posix_setuid`/`posix_setgid`/
    ///   `posix_seteuid`/`posix_setegid`, `pfsockopen`/`fsockopen`
    ///   (persistent-socket inheritance), the SysV IPC family
    ///   `shm_*`/`sem_*`/`msg_*`, `opcache_reset`/`opcache_compile_file`,
    ///   `dl`, and `mail`. The list is composed as a **union** with any
    ///   operator-supplied `disable_functions` (from `[php] ini_overrides`),
    ///   never clobbering it.
    /// - `mysqli.allow_persistent = 0` (persistent mysqli handles are keyed
    ///   without a tenant component, so one tenant could inherit another's).
    /// - `opcache.restrict_api` is pointed at an unreachable sentinel path so
    ///   userland cannot call the remaining OPcache API — **but only when
    ///   `[opcache] cluster_invalidation` is off**, because ePHPm's own
    ///   cluster invalidator calls `opcache_get_status`/`opcache_invalidate`
    ///   through the function table and `restrict_api` would block it too.
    ///   With cluster invalidation on, those two functions stay callable by
    ///   tenants (a metadata/per-file-invalidation residual, logged at
    ///   startup); the DoS-grade `opcache_reset` is disabled either way.
    ///
    /// **Cost:** persistent database/socket connections are disabled — Redis
    /// `pconnect`, mysqli `p:` hosts, and `pfsockopen`/`fsockopen` stop
    /// working. Non-persistent connections (PDO, `stream_socket_client`,
    /// curl) are unaffected. See the multi-tenant guide.
    ///
    /// **In single-site mode (`sites_dir` unset) this flag does nothing** and
    /// enabling it logs a warning at startup, exactly like the two flags
    /// above.
    ///
    /// An explicitly set value always wins. When unset, resolves to `true`
    /// if the `[server.security]` section is present OR `server.sites_dir`
    /// is set (multi-tenant mode); otherwise `false`. Set it to `false` to
    /// opt out and keep persistent connections. Use
    /// [`ServerConfig::effective_multi_tenant_hardening`] to read the
    /// resolved value.
    #[serde(default)]
    pub multi_tenant_hardening: Option<bool>,

    /// Assert that network egress is enforced *below* PHP — at the
    /// network/kernel layer (nftables, eBPF cgroup hooks, systemd
    /// `IPAddressDeny`, or a cloud security group) — so ePHPm may drop the
    /// **reachability-only** function blocks from the multi-tenant hardening
    /// preset.
    ///
    /// When `true` **and** the hardening preset is active (`sites_dir` set and
    /// `multi_tenant_hardening` on), ePHPm stops adding `fsockopen` to
    /// `disable_functions`. `fsockopen` opens a *non-persistent* raw socket;
    /// blocking it was only ever a reachability control, and it is redundant
    /// once the kernel decides which destinations a tenant can reach —
    /// especially since `stream_socket_client`/`curl` remain open and reach
    /// the same destinations anyway. Lifting it makes the "the network layer
    /// owns egress" posture consistent instead of blocking one raw-socket API
    /// while leaving the equivalent ones open.
    ///
    /// **What this does NOT lift.** `pfsockopen` stays disabled, and
    /// `mysqli.allow_persistent`/`pgsql.allow_persistent` stay `0`. Those close
    /// a *persistence* leak, not a reachability one: in the shared ZTS worker
    /// pool a persistent connection opened for one tenant survives in the
    /// thread's `EG(persistent_list)` and can be handed to the next tenant that
    /// thread serves. The network layer does nothing about that, so persistence
    /// stays off regardless of this flag. Nor does it touch the process-control,
    /// SysV-IPC, `dl`, `mail`, or OPcache blocks — none of those are about
    /// reachability.
    ///
    /// **Default `false`** (safe): ePHPm cannot verify that an external egress
    /// control actually exists, so the reachability block stays on unless the
    /// operator explicitly asserts otherwise. Setting it `true` where no such
    /// control exists re-opens `fsockopen` as an egress path. Only meaningful
    /// on the multi-tenant hardening path; setting it elsewhere logs a warning
    /// at startup (it has no effect). Use
    /// [`ServerConfig::effective_network_egress_externally_managed`] to read the
    /// resolved value.
    #[serde(default)]
    pub network_egress_externally_managed: Option<bool>,
}

/// `[server.tenant_network]` — per-vhost kernel network policy (Linux-only).
///
/// Off by default: when `ebpf_policy = false` ePHPm loads no BPF programs,
/// attaches nothing, and the request path writes no tag — literally zero cost,
/// the byte-identical hot path that shipped before this feature existed.
///
/// When enabled (Linux only, multi-tenant mode), ePHPm attaches
/// `cgroup/bind4+6` and `cgroup/connect4+6` programs to its own cgroup and
/// tags each serving thread with the canonical site key of the request it is
/// running, so the kernel can enforce per-vhost loopback authorization and
/// give each vhost a private view of a shared loopback port (transparent
/// sidecar port-rewrite). See `crates/ephpm-server/src/tenant_ebpf.rs` and the
/// roadmap doc.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TenantNetworkConfig {
    /// Enable the eBPF per-vhost network policy: per-thread vhost tagging,
    /// `cgroup/bind4+6` transparent sidecar port-rewrite, and
    /// `cgroup/connect4+6` per-vhost loopback authorization.
    ///
    /// **Linux-only.** On any other platform, setting this `true` is a hard
    /// startup error (see [`Config::validate`]) — never a silent no-op.
    ///
    /// **Loopback handoff:** when `true`, the eBPF `connect4/6` policy becomes
    /// the sole arbiter of loopback for tagged (per-vhost) traffic, so a static
    /// nftables floor that blanket-drops loopback for the ePHPm cgroup MUST hand
    /// that off to BPF (the two ship together — see the egress-hardening guide).
    #[serde(default)]
    pub ebpf_policy: bool,

    /// Cgroup path ePHPm attaches the programs to. Defaults (when `None`) to the
    /// process's own cgroup, read from `/proc/self/cgroup`, so the policy covers
    /// exactly ePHPm's threads and any sidecars it spawns. Override only for
    /// unusual systemd slice layouts.
    #[serde(default)]
    pub cgroup_path: Option<String>,

    /// Dedicated range of **real** sidecar ports ePHPm hands out (inclusive,
    /// `"low-high"`). `bind4` pops a free port from this range when a vhost binds
    /// a virtual loopback port; `sock_release` returns it. Range size is the
    /// box-wide sidecar concurrency cap.
    ///
    /// **HARD CONSTRAINT — must NOT overlap the kernel ephemeral range**
    /// (`net.ipv4.ip_local_port_range`, default `32768-60999`): otherwise a
    /// tenant's outbound `connect()` could be auto-assigned a *source* port that
    /// ePHPm also wants to hand out as a sidecar *real* port. The sidecar range
    /// therefore sits BELOW the ephemeral floor; ePHPm reads `ip_local_port_range`
    /// at load time and refuses to start on overlap (`serve()`, fail-closed).
    #[serde(default = "default_sidecar_port_range")]
    pub sidecar_port_range: String,

    /// Anti-port-bomb: maximum concurrent sidecar real ports a single vhost may
    /// hold. Enforced IN-KERNEL at the allocation point (`bind4`) against the
    /// un-forgeable, ePHPm-set vhost tag, so a tenant cannot bypass it and one
    /// tenant cannot starve siblings out of the shared pool. Small default (`8`).
    #[serde(default = "default_max_sidecar_ports_per_vhost")]
    pub max_sidecar_ports_per_vhost: u32,
}

fn default_sidecar_port_range() -> String {
    // Below the default ephemeral floor (32768). ~12.7k ports box-wide.
    "20000-32767".to_string()
}

fn default_max_sidecar_ports_per_vhost() -> u32 {
    8
}

impl Default for TenantNetworkConfig {
    fn default() -> Self {
        Self {
            ebpf_policy: false,
            cgroup_path: None,
            sidecar_port_range: default_sidecar_port_range(),
            max_sidecar_ports_per_vhost: default_max_sidecar_ports_per_vhost(),
        }
    }
}

impl TenantNetworkConfig {
    /// Parse `sidecar_port_range` into an inclusive `(low, high)`.
    ///
    /// # Errors
    /// Returns a human-readable message when the string is not `"low-high"`,
    /// a bound is unparseable, `low` is `0`, or `low > high`.
    pub fn parse_range(&self) -> Result<(u16, u16), String> {
        let (lo, hi) = self.sidecar_port_range.split_once('-').ok_or_else(|| {
            format!("sidecar_port_range must be \"low-high\", got {:?}", self.sidecar_port_range)
        })?;
        let lo: u16 =
            lo.trim().parse().map_err(|_| "sidecar_port_range: bad low port".to_string())?;
        let hi: u16 =
            hi.trim().parse().map_err(|_| "sidecar_port_range: bad high port".to_string())?;
        if lo == 0 || lo > hi {
            return Err(format!("sidecar_port_range invalid: {lo}-{hi} (need 1 <= low <= high)"));
        }
        Ok((lo, hi))
    }
}

impl ServerConfig {
    /// The directory of operator-supplied per-site overrides, or `None` when the
    /// mechanism is switched off.
    ///
    /// `None` when `site_overrides_dir` is unset and in single-site mode, where
    /// there are no tenants to distinguish and `document_root` is already the
    /// web root. Callers that get `None` must treat the vhost directory itself
    /// as the document root — the behaviour that predates this mechanism.
    ///
    /// [`Config::validate`] has already refused to start if this path is inside
    /// `sites_dir`, so a `Some` here is a directory no tenant can write.
    #[must_use]
    pub fn effective_site_overrides_dir(&self) -> Option<&Path> {
        self.sites_dir.as_ref()?;
        self.site_overrides_dir.as_deref()
    }

    /// Resolved value of `security.open_basedir`.
    ///
    /// An explicitly set value always wins. When unset, resolves to `true`
    /// if the `[server.security]` section is present (preserves the
    /// historical present-section default) OR `sites_dir` is set (so a
    /// multi-tenant deployment never silently runs without filesystem
    /// isolation); otherwise `false`.
    ///
    /// Resolving to `true` does **not** mean the restriction is applied:
    /// only the multi-tenant path acts on it. See
    /// [`Self::inert_security_flags`].
    #[must_use]
    pub fn effective_open_basedir(&self) -> bool {
        self.resolve_security_flag(|s| s.open_basedir)
    }

    /// Resolved value of `security.disable_shell_exec`.
    ///
    /// Same resolution rules as [`Self::effective_open_basedir`]: explicit
    /// value wins; unset resolves to `true` when the `[server.security]`
    /// section is present or `sites_dir` is set, `false` otherwise. Same
    /// caveat too — a `true` here is only acted upon in multi-tenant mode.
    #[must_use]
    pub fn effective_disable_shell_exec(&self) -> bool {
        self.resolve_security_flag(|s| s.disable_shell_exec)
    }

    /// Resolved value of `security.multi_tenant_hardening`.
    ///
    /// Same resolution rules as [`Self::effective_open_basedir`]: an explicit
    /// value wins; unset resolves to `true` when the `[server.security]`
    /// section is present or `sites_dir` is set, `false` otherwise. Same
    /// caveat — a `true` here is only acted upon in multi-tenant mode
    /// (`sites_dir` set), where it extends the generated php.ini denylist.
    #[must_use]
    pub fn effective_multi_tenant_hardening(&self) -> bool {
        self.resolve_security_flag(|s| s.multi_tenant_hardening)
    }

    /// Resolved value of `security.network_egress_externally_managed`.
    ///
    /// Unlike the isolation flags above, this **defaults to `false`** whether
    /// or not the `[server.security]` section is present: it asserts an
    /// external property (kernel/network egress control) that ePHPm cannot
    /// verify, so it must be opted into explicitly and never inferred from
    /// multi-tenant mode. When `true`, the hardening preset omits the
    /// reachability-only `fsockopen` block; see
    /// [`SecurityConfig::network_egress_externally_managed`].
    #[must_use]
    pub fn effective_network_egress_externally_managed(&self) -> bool {
        self.security.as_ref().and_then(|s| s.network_egress_externally_managed).unwrap_or(false)
    }

    /// The `[server.security]` isolation flags that resolve to `true` but
    /// have no effect, because both are implemented only on the
    /// multi-tenant path and `sites_dir` is unset.
    ///
    /// Returns the config key names, in declaration order. Empty in
    /// multi-tenant mode, and empty in single-site mode when neither flag
    /// resolves to `true` — so a config that never mentions
    /// `[server.security]` stays quiet. `ephpm` warns once at startup for
    /// each name returned here (the no-silent-knob rule): an operator who
    /// asked for sandboxing must never be left believing they got it.
    #[must_use]
    pub fn inert_security_flags(&self) -> Vec<&'static str> {
        if self.sites_dir.is_some() {
            return Vec::new();
        }
        let mut inert = Vec::new();
        if self.effective_open_basedir() {
            inert.push("open_basedir");
        }
        if self.effective_disable_shell_exec() {
            inert.push("disable_shell_exec");
        }
        if self.effective_multi_tenant_hardening() {
            inert.push("multi_tenant_hardening");
        }
        inert
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

    /// Resolve `[server.limits]` to the values enforcement runs on.
    ///
    /// Each field the operator set explicitly is taken verbatim — including
    /// explicit `0`/`0.0`, which disables that limit. Each unset field takes
    /// the preview preset value when `[server] preview = true`
    /// ([`ResolvedLimits::preview_preset`]), and the regular all-off default
    /// ([`ResolvedLimits::default`]) otherwise. Section-absent and
    /// section-present-but-empty behave identically (all fields unset).
    #[must_use]
    pub fn effective_limits(&self) -> ResolvedLimits {
        let base =
            if self.preview { ResolvedLimits::preview_preset() } else { ResolvedLimits::default() };
        let l = &self.limits;
        ResolvedLimits {
            max_connections: l.max_connections.unwrap_or(base.max_connections),
            per_ip_max_connections: l.per_ip_max_connections.unwrap_or(base.per_ip_max_connections),
            per_ip_rate: l.per_ip_rate.unwrap_or(base.per_ip_rate),
            per_ip_burst: l.per_ip_burst.unwrap_or(base.per_ip_burst),
            per_site_rate: l.per_site_rate.unwrap_or(base.per_site_rate),
            per_site_burst: l.per_site_burst.unwrap_or(base.per_site_burst),
        }
    }

    /// Which `[server.limits]` fields the preview preset supplied, as
    /// `(key, resolved_value)` pairs in declaration order — i.e. the fields
    /// the operator left unset while `[server] preview = true`.
    ///
    /// Empty when `preview` is off, and empty when the operator explicitly
    /// set every limit. Startup logs this so the preset is never silent
    /// (the no-silent-knob rule); the complement — explicitly set fields —
    /// is exactly what the log reports as operator-chosen.
    #[must_use]
    pub fn preview_preset_applied(&self) -> Vec<(&'static str, String)> {
        if !self.preview {
            return Vec::new();
        }
        let preset = ResolvedLimits::preview_preset();
        let l = &self.limits;
        let mut applied = Vec::new();
        if l.max_connections.is_none() {
            applied.push(("max_connections", preset.max_connections.to_string()));
        }
        if l.per_ip_max_connections.is_none() {
            applied.push(("per_ip_max_connections", preset.per_ip_max_connections.to_string()));
        }
        if l.per_ip_rate.is_none() {
            applied.push(("per_ip_rate", preset.per_ip_rate.to_string()));
        }
        if l.per_ip_burst.is_none() {
            applied.push(("per_ip_burst", preset.per_ip_burst.to_string()));
        }
        if l.per_site_rate.is_none() {
            applied.push(("per_site_rate", preset.per_site_rate.to_string()));
        }
        if l.per_site_burst.is_none() {
            applied.push(("per_site_burst", preset.per_site_burst.to_string()));
        }
        applied
    }
}

/// Logging configuration (`[server.logging]`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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

/// Per-request diagnostics configuration (`[server.diagnostics]`).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsConfig {
    /// Enable the in-memory request timeline and its `/_ephpm/requests`
    /// endpoint. The server keeps the last 256 completed requests (method,
    /// path, status, total duration, worker-queue wait, PHP execution time,
    /// response size, timestamp) in a fixed-size ring buffer and serves them
    /// as JSON, newest first.
    ///
    /// Unset resolves per mode (see
    /// [`DiagnosticsConfig::effective_request_log`]): **on** under
    /// `ephpm dev` / bare `ephpm`, **off** under `ephpm serve`. Set `true` /
    /// `false` to force a value in either mode. When off, `GET
    /// /_ephpm/requests` answers 404 naming this knob — the path is never
    /// routed to the application, because the whole `/_ephpm/` namespace is
    /// reserved by the server (issue #444; it used to fall through, which in
    /// worker mode meant the framework answered for a diagnostics endpoint).
    ///
    /// Env override: `EPHPM_SERVER__DIAGNOSTICS__REQUEST_LOG`.
    ///
    /// Default: unset (dev: on, serve: off).
    #[serde(default)]
    pub request_log: Option<bool>,

    /// OTLP trace exporter endpoint, e.g. `"http://127.0.0.1:4318"`
    /// (OTLP/HTTP with protobuf payloads; `/v1/traces` is appended when the
    /// value does not already end with it). Only acted upon in binaries built
    /// with the `otlp` cargo feature; without that feature a set value logs a
    /// startup warning and exports nothing.
    ///
    /// The standard `OTEL_EXPORTER_OTLP_ENDPOINT` /
    /// `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` environment variables take
    /// precedence over this knob. When neither the env vars nor this knob are
    /// set, no exporter task is started and no background threads are
    /// spawned.
    ///
    /// Default: unset (no trace export).
    #[serde(default)]
    pub otlp_endpoint: Option<String>,

    /// OTLP wire protocol for [`Self::otlp_endpoint`]: `"grpc"` or
    /// `"http/protobuf"`.
    ///
    /// The two differ in more than encoding, so this must match what the
    /// collector expects: `http/protobuf` takes a *signal* URL and gets
    /// `/v1/traces` appended when missing (conventional port 4318), while
    /// `grpc` takes a *base* URL used verbatim (conventional port 4317).
    /// Pointing one at the other's port is the usual cause of "no traces
    /// arrive"; ePHPm logs a startup warning when it detects that.
    ///
    /// `"http/json"` is a real OTLP protocol that ePHPm does **not**
    /// implement, and is rejected at startup rather than silently falling
    /// back to another one.
    ///
    /// The standard `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` /
    /// `OTEL_EXPORTER_OTLP_PROTOCOL` environment variables take precedence
    /// over this knob, mirroring how the endpoint variables behave.
    ///
    /// Env override: `EPHPM_SERVER__DIAGNOSTICS__OTLP_PROTOCOL`.
    ///
    /// Default: unset, meaning `http/protobuf` — the OTel default, and what
    /// ePHPm used before gRPC was supported.
    #[serde(default)]
    pub otlp_protocol: Option<String>,
}

impl DiagnosticsConfig {
    /// Resolve the effective request-timeline setting for the given mode.
    ///
    /// `dev_mode` is `true` under `ephpm dev` / bare `ephpm` and `false`
    /// under `ephpm serve`. An explicit `request_log` value wins either way;
    /// unset means "on in dev, off in serve".
    #[must_use]
    pub fn effective_request_log(&self, dev_mode: bool) -> bool {
        self.request_log.unwrap_or(dev_mode)
    }
}

/// Rate limiting and connection limiting (`[server.limits]`).
///
/// Every field is optional so that "the operator set this" is distinguishable
/// from "left at the default" — the `[server] preview` preset only fills in
/// fields the operator did NOT set (see [`ServerConfig::effective_limits`]).
/// Enforcement reads the **resolved** values ([`ResolvedLimits`]), never these
/// raw options; the effective defaults documented per field below are what an
/// absent field resolves to without the preview preset.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    /// Maximum total concurrent connections. New connections are rejected
    /// with a raw 503 at accept time (before TLS) when at capacity.
    /// `0` means unlimited.
    ///
    /// Default: `0` (unlimited). Preview preset: `256`.
    #[serde(default)]
    pub max_connections: Option<usize>,

    /// Maximum concurrent connections per client IP. `0` means unlimited.
    ///
    /// Default: `0` (unlimited). Preview preset: `32`.
    #[serde(default)]
    pub per_ip_max_connections: Option<usize>,

    /// Maximum requests per second per client IP (token bucket rate).
    /// Over-limit requests get 429. `0` means unlimited.
    ///
    /// Default: `0.0` (unlimited). Preview preset: `10.0`.
    #[serde(default)]
    pub per_ip_rate: Option<f64>,

    /// Burst size for per-IP rate limiting. Allows this many requests
    /// to be made instantly before the rate limit kicks in. Only meaningful
    /// when `per_ip_rate` resolves to a non-zero value.
    ///
    /// Default: `50` (also the preview preset value).
    #[serde(default)]
    pub per_ip_burst: Option<u32>,

    /// Maximum PHP executions per second **per virtual host** (token bucket
    /// rate), keyed by the canonical site key `Router::resolve_site` derives
    /// (never re-derived from the `Host` header). Caps each tenant so one
    /// preview going viral cannot starve the whole node. Over-limit requests
    /// get 429 with a `Retry-After` header, before PHP runs.
    ///
    /// Scope: PHP dispatch only — static files and PHP-`ETag`-cache 304s are
    /// not counted (they are not what eats the box). Requests whose host
    /// matches no site (key = `None`) are not per-site-capped; in multi-site
    /// mode they get no per-site database or credentials anyway. In
    /// single-site mode every request has key = `None`, so this knob only
    /// acts when `[server] sites_dir` is set. `0` means unlimited.
    ///
    /// Default: `0.0` (unlimited). Preview preset: `5.0`.
    #[serde(default)]
    pub per_site_rate: Option<f64>,

    /// Burst size for the per-site rate limit: this many PHP executions may
    /// happen instantly per site before `per_site_rate` kicks in. Only
    /// meaningful when `per_site_rate` resolves to a non-zero value.
    ///
    /// Default: `20` (also the preview preset value).
    #[serde(default)]
    pub per_site_burst: Option<u32>,
}

/// The `[server.limits]` values enforcement actually runs on, after resolving
/// each optional field against the base defaults or — under
/// `[server] preview = true` — the preview preset. Built by
/// [`ServerConfig::effective_limits`]; an explicitly configured value always
/// wins over either default set.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedLimits {
    /// See [`LimitsConfig::max_connections`]. `0` = unlimited.
    pub max_connections: usize,
    /// See [`LimitsConfig::per_ip_max_connections`]. `0` = unlimited.
    pub per_ip_max_connections: usize,
    /// See [`LimitsConfig::per_ip_rate`]. `0.0` = unlimited.
    pub per_ip_rate: f64,
    /// See [`LimitsConfig::per_ip_burst`].
    pub per_ip_burst: u32,
    /// See [`LimitsConfig::per_site_rate`]. `0.0` = unlimited.
    pub per_site_rate: f64,
    /// See [`LimitsConfig::per_site_burst`].
    pub per_site_burst: u32,
}

impl Default for ResolvedLimits {
    /// The no-preset resolution: everything off, standard burst sizes.
    fn default() -> Self {
        Self {
            max_connections: 0,
            per_ip_max_connections: 0,
            per_ip_rate: 0.0,
            per_ip_burst: 50,
            per_site_rate: 0.0,
            per_site_burst: 20,
        }
    }
}

impl ResolvedLimits {
    /// The preview preset: what an unset field resolves to under
    /// `[server] preview = true`. Sized for a small PR-preview box — a
    /// 1-vCPU node renders roughly 10 real-WordPress pages/s, so the
    /// per-site cap (5/s, burst 20) keeps any single tenant at about half
    /// the node while still absorbing a page-load burst.
    #[must_use]
    pub fn preview_preset() -> Self {
        Self {
            max_connections: 256,
            per_ip_max_connections: 32,
            per_ip_rate: 10.0,
            per_ip_burst: 50,
            per_site_rate: 5.0,
            per_site_burst: 20,
        }
    }
}

/// Open file cache configuration (`[server.file_cache]`).
///
/// Caches file metadata (size, mtime, MIME type, `ETag`) and optionally
/// small file content in memory. Avoids repeated filesystem `stat` and
/// `read` calls for frequently accessed static files.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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

    // --- ACME challenge selection ---
    /// ACME challenge type: `"tls-alpn-01"` (default) or `"dns-01"`.
    ///
    /// - `"tls-alpn-01"` — the zero-config default. The server answers the
    ///   challenge inline on the TLS listener (via `rustls-acme`); no DNS
    ///   credentials are needed, but **wildcard certificates are impossible**
    ///   (the CA cannot prove wildcard control over a single hostname).
    /// - `"dns-01"` — provisions the challenge as a `_acme-challenge` TXT
    ///   record through a [`Self::dns_provider`]. This is the **only** way to
    ///   obtain a wildcard certificate (`*.example.com`), and it works for
    ///   hosts that never accept inbound TLS. Requires `dns_provider` and a
    ///   provider credential (see [`Self::cloudflare_api_token_file`]).
    ///
    /// Default: `"tls-alpn-01"` (preserves the previous behaviour exactly).
    /// Any other value is rejected at startup by `Config::validate`.
    #[serde(default = "default_tls_challenge")]
    pub challenge: String,

    /// DNS provider used to satisfy `dns-01` challenges.
    ///
    /// One of `"cloudflare"`, `"linode"`, `"digitalocean"`, `"route53"`, or
    /// `"google"` (Google Cloud DNS). Ignored unless `challenge = "dns-01"`.
    /// `Config::validate` requires it in that mode, requires the selected
    /// provider's credential(s) (see the `*_api_token*` / `route53_*` /
    /// `google_*` fields below), and rejects unknown providers rather than
    /// silently doing nothing.
    #[serde(default)]
    pub dns_provider: Option<String>,

    /// Path to a file containing the Cloudflare API token (preferred).
    ///
    /// The token must be **zone-scoped** with the `Zone.DNS:Edit` permission on
    /// the zone(s) that hold the challenge records. Keeping the secret in a
    /// `0600` file (or a mounted secret) keeps it out of `ephpm.toml`.
    ///
    /// Precedence when both are present: this file wins over
    /// [`Self::cloudflare_api_token`]. The token may instead be supplied
    /// entirely via the environment as
    /// `EPHPM_SERVER__TLS__CLOUDFLARE_API_TOKEN` (which populates
    /// `cloudflare_api_token`). Only read when `dns_provider = "cloudflare"`.
    #[serde(default)]
    pub cloudflare_api_token_file: Option<PathBuf>,

    /// Cloudflare API token supplied inline (discouraged).
    ///
    /// Prefer [`Self::cloudflare_api_token_file`] or the
    /// `EPHPM_SERVER__TLS__CLOUDFLARE_API_TOKEN` environment variable so the
    /// secret does not live in the config file. This field exists primarily as
    /// the landing spot for that env var. Only read when
    /// `dns_provider = "cloudflare"`; the token file takes precedence.
    #[serde(default)]
    pub cloudflare_api_token: Option<String>,

    /// Explicit Cloudflare zone id for the challenge records.
    ///
    /// Optional. When absent, the zone is resolved from the challenge FQDN by
    /// walking its parent domains against the Cloudflare `zones` API. Set it to
    /// skip that lookup (one fewer API round-trip, and it removes the
    /// `Zone:Read` requirement from the token). Only used when
    /// `dns_provider = "cloudflare"`.
    #[serde(default)]
    pub cloudflare_zone_id: Option<String>,

    // --- Linode DNS provider (dns_provider = "linode") ---
    /// Path to a file holding a Linode API v4 token (scope `domains:read_write`).
    /// File wins over the inline value. Only used when `dns_provider = "linode"`.
    #[serde(default)]
    pub linode_api_token_file: Option<PathBuf>,
    /// Linode API token — prefer the file or the
    /// `EPHPM_SERVER__TLS__LINODE_API_TOKEN` environment variable.
    #[serde(default)]
    pub linode_api_token: Option<String>,

    // --- DigitalOcean DNS provider (dns_provider = "digitalocean") ---
    /// Path to a file holding a DigitalOcean API token (write scope).
    #[serde(default)]
    pub digitalocean_api_token_file: Option<PathBuf>,
    /// DigitalOcean API token — prefer the file or the
    /// `EPHPM_SERVER__TLS__DIGITALOCEAN_API_TOKEN` environment variable.
    #[serde(default)]
    pub digitalocean_api_token: Option<String>,

    // --- AWS Route 53 DNS provider (dns_provider = "route53") ---
    /// AWS access key id. Required when `dns_provider = "route53"`.
    #[serde(default)]
    pub route53_access_key_id: Option<String>,
    /// Path to a file holding the AWS secret access key. File wins over inline.
    #[serde(default)]
    pub route53_secret_access_key_file: Option<PathBuf>,
    /// AWS secret access key — prefer the file or the
    /// `EPHPM_SERVER__TLS__ROUTE53_SECRET_ACCESS_KEY` environment variable.
    #[serde(default)]
    pub route53_secret_access_key: Option<String>,
    /// Optional explicit Route 53 hosted zone id. When absent, resolved from the
    /// challenge FQDN via `ListHostedZonesByName` (which does not paginate — set
    /// this explicitly on an account with more than ~100 zones).
    #[serde(default)]
    pub route53_hosted_zone_id: Option<String>,

    // --- Google Cloud DNS provider (dns_provider = "google") ---
    /// Path to the service-account JSON key file. File wins over inline.
    #[serde(default)]
    pub google_service_account_json_file: Option<PathBuf>,
    /// Service-account JSON key **contents** — prefer the file or the
    /// `EPHPM_SERVER__TLS__GOOGLE_SERVICE_ACCOUNT_JSON` environment variable.
    #[serde(default)]
    pub google_service_account_json: Option<String>,
    /// GCP project id owning the Cloud DNS zone. Required for `google`.
    #[serde(default)]
    pub google_project: Option<String>,
    /// Optional Cloud DNS managed-zone name. When absent, resolved from the
    /// challenge FQDN by listing the project's managed zones.
    #[serde(default)]
    pub google_managed_zone: Option<String>,

    // --- Shared ---
    /// Optional separate listen address for HTTPS (e.g. `"0.0.0.0:443"`).
    ///
    /// When set, `server.listen` serves HTTP and this address serves HTTPS.
    /// When omitted, `server.listen` serves HTTPS directly (no HTTP listener).
    ///
    /// "`server.listen` serves HTTP" holds unconditionally — it does not
    /// depend on [`redirect_http`](Self::redirect_http). Setting this address
    /// is what splits the two protocols across two ports; TLS is never
    /// negotiated on `server.listen` while this is set.
    #[serde(default)]
    pub listen: Option<String>,

    /// When `true` and `listen` is set, the HTTP listener redirects
    /// all requests to HTTPS with a 301 Moved Permanently response.
    ///
    /// This chooses only what the plain-HTTP listener *says*. When `false`
    /// that listener still serves plain HTTP — it answers requests normally
    /// instead of redirecting. It never turns the HTTP listener into a
    /// second HTTPS listener.
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

    /// Returns `true` if ACME is configured to use the `dns-01` challenge.
    ///
    /// This selects the [`crate::TlsConfig::dns_provider`]-driven wildcard lane
    /// instead of the default TLS-ALPN-01 path. Only meaningful when
    /// [`Self::is_acme`] is also `true`.
    #[must_use]
    pub fn is_dns01(&self) -> bool {
        self.is_acme() && self.challenge.eq_ignore_ascii_case("dns-01")
    }

    /// Returns `true` if any configured domain is a wildcard (`*.example.com`).
    #[must_use]
    pub fn has_wildcard_domain(&self) -> bool {
        self.domains.iter().any(|d| d.starts_with("*."))
    }
}

/// HTTP/3 (QUIC) configuration (`[server.http3]`).
///
/// HTTP/3 runs over **UDP**, alongside — never instead of — the TCP
/// listeners that serve HTTP/1.1 and HTTP/2. Enabling it therefore binds an
/// additional UDP socket; nothing about the TCP side changes.
///
/// QUIC mandates TLS 1.3, so HTTP/3 only starts when TLS is configured with a
/// static certificate (`[server.tls] cert` + `key`). ACME-provisioned
/// certificates are **not** wired into the QUIC endpoint yet — see
/// [`Http3Config::enabled`].
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Http3Config {
    /// Enable the HTTP/3 (QUIC) listener.
    ///
    /// Default: `false`. HTTP/3 is opt-in because it binds an extra UDP
    /// socket and requires a static TLS certificate.
    ///
    /// When `true` but TLS is absent or is in ACME mode, startup logs a
    /// warning and the QUIC listener does not start — the TCP listeners are
    /// unaffected. ePHPm never silently continues without the HTTP/3
    /// listener an operator asked for.
    #[serde(default)]
    pub enabled: bool,

    /// UDP address for the QUIC listener (e.g. `"0.0.0.0:443"`).
    ///
    /// Absent (the default) means "derive from the HTTPS listener": the UDP
    /// socket binds the same address and port that serves HTTPS over TCP
    /// (`[server.tls] listen` when set, otherwise `[server] listen`). That
    /// matches how browsers expect to find HTTP/3 — same authority, same
    /// port number, different transport.
    ///
    /// Set explicitly only to move QUIC to a different port; the port used
    /// here is what gets advertised in `Alt-Svc`.
    #[serde(default)]
    pub listen: Option<String>,

    /// `max-age` in seconds for the `Alt-Svc` response header advertised on
    /// HTTPS responses (`Alt-Svc: h3=":443"; ma=86400`).
    ///
    /// Browsers do not attempt HTTP/3 until they have seen this header over
    /// TCP, so it is the discovery mechanism, not a hint. It caps how long a
    /// client may keep using the advertised HTTP/3 endpoint without
    /// re-confirming it.
    ///
    /// Default: `86400` (24 hours). Set to `0` to suppress the header
    /// entirely — HTTP/3 then only serves clients that were told about it
    /// some other way (e.g. `curl --http3-only`, `HTTPS`/`SVCB` DNS records).
    #[serde(default = "default_alt_svc_max_age")]
    pub alt_svc_max_age: u64,
}

impl Default for Http3Config {
    fn default() -> Self {
        Self { enabled: false, listen: None, alt_svc_max_age: default_alt_svc_max_age() }
    }
}

/// Native WebSocket configuration (`[server.websocket]`) — **experimental**.
///
/// ePHPm terminates WebSockets in Rust and invokes PHP **per event**, never
/// per connection: a `connect` / `message` / `disconnect` event runs the
/// vhost's `[server] websocket_files` entrypoint through the ordinary PHP
/// request path and then returns. Idle connections cost reactor memory and a
/// registry entry — no PHP thread, no worker, no process.
///
/// The whole feature is off unless [`WebSocketConfig::enabled`] is `true`. With
/// it off, an upgrade request is routed exactly like any other GET (which is
/// what ePHPm did before this section existed), so turning the section on is
/// the only behaviour change.
///
/// WebSockets are negotiated over **HTTP/1.1** only. An upgrade cannot be
/// expressed on the HTTP/2 or HTTP/3 request paths ePHPm serves (RFC 8441
/// extended CONNECT is not implemented), and browsers open a dedicated
/// HTTP/1.1 connection for `ws:`/`wss:` regardless — so this is not a
/// limitation clients encounter in practice.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct WebSocketConfig {
    /// Enable native WebSocket support.
    ///
    /// Default: `false`. Experimental and opt-in: it changes how upgrade
    /// requests route (they resolve `[server] websocket_files` and 404 when no
    /// entrypoint exists) and it admits long-lived connections that outlive
    /// their HTTP request, so it is never on implicitly.
    ///
    /// Env override: `EPHPM_SERVER__WEBSOCKET__ENABLED`.
    #[serde(default)]
    pub enabled: bool,

    /// Maximum concurrent WebSocket connections across every virtual host.
    ///
    /// Default: `10000`. `0` = unlimited. An upgrade beyond the cap is refused
    /// with `503`, before the handshake completes.
    ///
    /// This is a **separate** budget from `[server.limits] max_connections`: an
    /// upgraded socket is handed off to its own task and stops occupying an
    /// HTTP connection slot, so the HTTP cap cannot bound it.
    #[serde(default = "default_ws_max_connections")]
    pub max_connections: usize,

    /// Maximum concurrent WebSocket connections for any single virtual host.
    ///
    /// Default: `1000`. `0` = unlimited. Enforced in addition to
    /// [`WebSocketConfig::max_connections`] so one tenant on a shared
    /// deployment cannot consume the whole budget. Refused upgrades get `503`.
    #[serde(default = "default_ws_max_connections_per_site")]
    pub max_connections_per_site: usize,

    /// Maximum size, in bytes, of a single inbound WebSocket **message**
    /// (after reassembling continuation frames).
    ///
    /// Default: `1048576` (1 MiB). A message larger than this closes the
    /// connection rather than allocating for it. This is the value PHP could be
    /// asked to read as a request body, so it is deliberately independent of
    /// `[server.request] max_body_size`.
    #[serde(default = "default_ws_max_message_size")]
    pub max_message_size: usize,

    /// Maximum size, in bytes, of a single inbound WebSocket **frame**.
    ///
    /// Default: `1048576` (1 MiB). Bounds one read; `max_message_size` bounds
    /// the reassembled total.
    #[serde(default = "default_ws_max_frame_size")]
    pub max_frame_size: usize,

    /// Depth, in frames, of each connection's outbound queue.
    ///
    /// Default: `64`. When a connection's queue is full, the frame is **not**
    /// buffered: `ephpm_ws_send` / `ephpm_ws_broadcast` return failure for that
    /// connection and the socket is closed with WebSocket status `1013`. A slow
    /// reader costs one connection, never the server's memory.
    ///
    /// `0` is not a valid depth and is normalized to `1` with a warning.
    #[serde(default = "default_ws_send_queue")]
    pub send_queue: usize,

    /// Seconds between server-initiated WebSocket pings.
    ///
    /// Default: `30`. `0` disables keepalive pings. Pings are what keep an
    /// otherwise-idle connection's [`WebSocketConfig::idle_timeout_secs`] from
    /// expiring, so disabling them means idle connections are dropped.
    #[serde(default = "default_ws_ping_interval_secs")]
    pub ping_interval_secs: u64,

    /// Seconds a connection may go without receiving **any** frame (including a
    /// pong) before it is closed.
    ///
    /// Default: `120`. `0` disables the check. Keep this comfortably larger
    /// than [`WebSocketConfig::ping_interval_secs`] — a client that answers
    /// pings refreshes the timer, so the timeout only fires for a peer that has
    /// genuinely gone away without a TCP reset.
    #[serde(default = "default_ws_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
}

impl Default for WebSocketConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_connections: default_ws_max_connections(),
            max_connections_per_site: default_ws_max_connections_per_site(),
            max_message_size: default_ws_max_message_size(),
            max_frame_size: default_ws_max_frame_size(),
            send_queue: default_ws_send_queue(),
            ping_interval_secs: default_ws_ping_interval_secs(),
            idle_timeout_secs: default_ws_idle_timeout_secs(),
        }
    }
}

/// Top-level database proxy configuration (`[db]`).
///
/// When present, ePHPm starts a transparent SQL proxy between PHP and the
/// real database. PHP connects to `127.0.0.1:3306` (or the configured
/// `listen` address) — it never talks to the database directly.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
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
/// Unknown keys are rejected at startup for the same reason as
/// [`ReplicationConfig`]: `[db.sqlite]` selects the embedded-database mode, and
/// a key this binary does not know is far more likely to be a typo or a knob
/// from a newer version than something safe to ignore. The v0.7.0 removals
/// (`sqld`, `engine = "rusqlite"`) are still *declared* — as
/// [`sqld`](Self::sqld) and a validated [`engine`](Self::engine) — so upgrading
/// configs keep parsing and get a warning or a migration message instead of a
/// bare "unknown field".
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct SqliteConfig {
    /// Path to the `SQLite` database file.
    ///
    /// Used in **single-site** mode (no `[server] sites_dir`). In multi-site
    /// mode this is ignored in favour of [`dir`](Self::dir) — see below.
    ///
    /// Default: `"ephpm.db"` in the current working directory.
    #[serde(default = "default_sqlite_path")]
    pub path: String,

    /// Directory holding **per-site** database files (multi-site mode only).
    ///
    /// When `[server] sites_dir` is set (multi-site / multi-tenant mode) each
    /// virtual host gets its **own** database file at `<dir>/<site-key>.db`,
    /// opened lazily on that site's first query and cached (bounded by
    /// [`max_open_dbs`](Self::max_open_dbs)). This is the isolation unit for
    /// secure multi-tenancy: Turso has no per-schema ACL, so the database file
    /// is the only tenant boundary — one file per site means one tenant's SQL
    /// cannot name, read, or write another tenant's data.
    ///
    /// The filename component is the **canonical site key**: the same validated
    /// `[a-z0-9._-]` key that selected the document root — `Host` normalized
    /// (port and trailing dot stripped, lowercased) with
    /// [`sites_domain_suffix`](ServerConfig::sites_domain_suffix) removed. A
    /// database path is never derived from a raw `Host` header, and a tenant
    /// reached by two of its names still has exactly one database (issue #290).
    ///
    /// A well-formed but **unknown** host has no site key, so it gets no
    /// database at all rather than minting one named after the header
    /// (issue #291).
    ///
    /// **Required in multi-site mode** — startup fails closed if `sites_dir` is
    /// set, `[db.sqlite]` is present (single-node), and `dir` is unset, rather
    /// than silently scattering per-site files or falling back to one shared
    /// database (which would defeat tenant isolation). Ignored (with a startup
    /// warning) in single-site mode. Not yet supported in clustered mode.
    ///
    /// Default: `None`.
    #[serde(default)]
    pub dir: Option<String>,

    /// Maximum number of per-site databases held open at once (multi-site
    /// mode only).
    ///
    /// Turso keeps a file open per `Database` factory (roughly `db` + `-wal`,
    /// so budget ~3 fds each). This bounds the number of simultaneously-open
    /// site databases: when the cache is full, the least-recently-used **idle**
    /// site (one with no in-flight request or live bridge session) is closed to
    /// make room; a later request for it re-opens transparently. A site with a
    /// live session is never evicted, so this is a *soft* bound on file
    /// descriptors — size it with headroom under the process `RLIMIT_NOFILE`
    /// (`max_open_dbs × ~3 + server sockets`).
    ///
    /// Default: `256`.
    #[serde(default = "default_sqlite_max_open_dbs")]
    pub max_open_dbs: usize,

    /// Database engine. As of v0.7.0 the only value is `"turso"` (the
    /// default) — the Turso Database engine, a ground-up Rust rewrite of
    /// `SQLite`.
    ///
    /// The legacy `"sqlite"` / `"rusqlite"` in-process C engine and the
    /// `sqld` clustered sidecar were removed in v0.7.0. Setting `engine` to
    /// either of those legacy values is a hard startup error (with a
    /// migration message) rather than a silent fallback to a now-absent
    /// backend. Any other value is likewise rejected at startup.
    #[serde(default = "default_sqlite_engine")]
    pub engine: String,

    /// Wire protocol proxy settings.
    #[serde(default)]
    pub proxy: SqliteProxyConfig,

    /// DEPRECATED (removed in v0.7.0): the `[db.sqlite.sqld]` block.
    ///
    /// sqld and the rusqlite backend were removed in v0.7.0. Clustered
    /// SQLite now replicates through the in-process Turso CDC path (no sqld
    /// sidecar), so this section no longer controls anything. It is still
    /// parsed so upgrading configs do not hard-fail; startup logs a warning
    /// when it is present. Delete it.
    #[serde(default)]
    pub sqld: Option<DeprecatedSqldConfig>,

    /// Replication settings (clustered mode only).
    #[serde(default)]
    pub replication: ReplicationConfig,
}

/// Wire protocol frontend addresses for the `SQLite` proxy (`[db.sqlite.proxy]`).
///
/// Unknown keys are rejected — see [`ReplicationConfig`]. A mis-typed listen
/// address key here would silently leave a frontend unbound (or bound to the
/// default address), which surfaces only as every client getting connection
/// refused at runtime.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
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

    /// Maximum concurrent wire connections across the `MySQL`, `PostgreSQL`
    /// and `TDS` frontends combined. `0` means unlimited.
    ///
    /// Connections beyond the cap are refused at accept time (`MySQL`
    /// clients receive error 1040 "Too many connections"; other protocols
    /// get a clean close) — never queued. Each accepted wire session holds
    /// one OS thread in litewire's session-worker model, so this cap also
    /// bounds those threads. The Hrana HTTP frontend is stateless and is
    /// not counted.
    ///
    /// Default: `0` (unlimited), matching `[db.mysql] max_connections` and
    /// `[server.limits] max_connections`.
    #[serde(default)]
    pub max_connections: usize,

    /// Whether to start the `MySQL` wire listener at all.
    ///
    /// Default: `true` (the listener is bound — current behavior preserved).
    ///
    /// Set to `false` for **bridge-only** multi-tenant deployments where every
    /// app talks to its per-site database exclusively through the in-process
    /// native `ephpm_db_*` SAPI bridge and nothing uses stock `pdo_mysql`. When
    /// `false`, ePHPm does **not** bind `mysql_listen` (no `:3306` frontend) and
    /// injects no `DB_HOST`/`DB_PORT`/`DB_USER`/`DB_PASSWORD` into requests — one
    /// fewer local attack surface on a hardened preview host. The per-site
    /// database registry and the `ephpm_db_*` bridge are still wired up, so
    /// in-process database access is unaffected; only the wire *frontend* is
    /// skipped. Applies to the per-site (multi-tenant) MySQL listener only.
    #[serde(default = "default_mysql_wire_enabled")]
    pub mysql_wire_enabled: bool,
}

impl Default for SqliteProxyConfig {
    fn default() -> Self {
        Self {
            mysql_listen: default_sqlite_mysql_listen(),
            hrana_listen: None,
            postgres_listen: None,
            tds_listen: None,
            max_connections: 0,
            mysql_wire_enabled: default_mysql_wire_enabled(),
        }
    }
}

/// DEPRECATED (removed in v0.7.0): `[db.sqlite.sqld]`.
///
/// The sqld child process and its Hrana/gRPC listeners were removed in
/// v0.7.0 along with the rusqlite backend. Clustered SQLite now replicates
/// through the in-process Turso CDC path — there is no sqld process to
/// configure. Every field here is parsed-but-ignored purely so an existing
/// config does not hard-fail on upgrade; startup logs a warning when the
/// section (or `write_permits`) is present. Delete the section.
///
/// # Deliberately NOT `deny_unknown_fields`
///
/// Every other section rejects unknown keys (see [`Config`]). This one must
/// not: it exists *only* so a config written for v0.6.x still parses, and its
/// knobs were deleted in v0.7.0. Rejecting a key we no longer declare would
/// break exactly the upgrade path the block was kept for — the three fields
/// below are the ones worth warning about, not an exhaustive record of what
/// sqld once accepted. Pinned by
/// `deprecated_sqld_block_tolerates_its_own_removed_keys`.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct DeprecatedSqldConfig {
    /// Removed: sqld's Hrana HTTP listen address. Ignored.
    #[serde(default)]
    pub http_listen: Option<String>,

    /// Removed: sqld's gRPC replication listen address. Ignored.
    #[serde(default)]
    pub grpc_listen: Option<String>,

    /// Removed: sqld's single-writer admission semaphore. Turso is MVCC
    /// (concurrent writers), so there is no single writer to gate — the
    /// c>=8 write collapse this mitigated (issue #217) cannot occur.
    /// Ignored.
    #[serde(default)]
    pub write_permits: Option<usize>,
}

/// Replication configuration (`[db.sqlite.replication]`).
///
/// Selects this node's clustered-replication role and, via
/// [`per_site`](Self::per_site), the *tenancy* of the replicated database.
///
/// # Unknown keys are a hard startup error
///
/// This struct is `deny_unknown_fields` because every knob in it selects a
/// **mode**, and a mis-typed or not-yet-supported mode knob is the one kind of
/// config error that passes every health check while running the wrong thing.
/// The motivating case: `per_site = true` on a binary that predates the knob
/// parsed happily, was ignored, and the node came up in whole-database
/// clustered mode — every tenant sharing one database — with nothing in the
/// logs to say so. Serde's default (silently drop unknown fields) turns an
/// operator's explicit instruction into a no-op, which is exactly what the
/// "no silent no-op config knobs" rule in `CLAUDE.md` forbids.
///
/// The forward-compatibility cost is intended: a config naming a knob this
/// binary does not implement must fail loudly rather than run a different mode
/// than the operator asked for. Removed-but-still-honoured knobs
/// ([`cdc_experimental`](Self::cdc_experimental)) stay declared here precisely
/// so upgrading configs keep parsing — they warn at startup instead.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
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

    /// DEPRECATED (removed in v0.7.0): the CDC-native replication opt-in.
    ///
    /// Default: `false`. In v0.6.x this gated the experimental Turso CDC
    /// clustered path against the sqld sidecar. As of v0.7.0 sqld is gone
    /// and CDC is the *only* clustered SQLite replication path, always
    /// active in clustered mode — so this flag no longer selects anything.
    /// It is still parsed so upgrading configs do not hard-fail; startup
    /// logs a warning when it is set. Delete it.
    #[serde(default)]
    pub cdc_experimental: bool,

    /// Maximum snapshot-bootstrap payload a cold replica will accept
    /// from the primary, in bytes. Default: 1 GiB.
    ///
    /// Consulted on the clustered Turso CDC path, where a joining replica
    /// pulls a logical dump of the primary's database over the cluster
    /// channel.
    /// The advertised length in the snapshot header is checked against
    /// this before any buffer is reserved, and the running total is
    /// checked as chunks arrive — so a peer that streams forever, or
    /// claims an absurd length, is cut off instead of exhausting
    /// memory.
    ///
    /// Raise it if a legitimate database dump is larger than the
    /// default; bootstrap fails with a message naming this knob when it
    /// is too low.
    #[serde(default = "default_max_snapshot_bytes")]
    pub max_snapshot_bytes: u64,

    /// Replicate **per-site** databases across the cluster (multi-tenant
    /// clustered mode). Default: `false`.
    ///
    /// Only meaningful when clustered mode is active (`[cluster] enabled`
    /// with `replication.role` = `auto`/`primary`/`replica`) **and**
    /// `[server] sites_dir` is set (multi-tenant, one Turso database per
    /// virtual host at `[db.sqlite] dir`/`<site-key>.db`). When `true`,
    /// each site's database replicates across every node so any node can
    /// serve any site's reads, and ownership of a site's writes is chosen
    /// by rendezvous hashing (HRW) over the alive nodes — on a node death
    /// only that node's sites move, each to a node already holding a warm
    /// replica.
    ///
    /// When `false` (the default), a clustered + multi-site config gets
    /// **no** per-site isolation: all virtual hosts share the single
    /// clustered database (the pre-existing behaviour, with a startup
    /// warning). Setting this to `true` turns on the per-site clustered
    /// replication path instead.
    ///
    /// Ignored (no effect) outside clustered multi-site mode: a
    /// single-node multi-site deployment already isolates per site, and a
    /// single-database clustered deployment has no per-site dimension.
    ///
    /// **Writes are owner-served.** A request for a site this node does not
    /// own has its `ephpm_db_*` statements forwarded to the site's HRW owner
    /// over the (authenticated, encrypted) cluster channel, which executes
    /// them against its local database so the write is captured into CDC and
    /// replicates everywhere. Reads and writes therefore both work on any
    /// node, and read-your-writes holds because one node serves both.
    ///
    /// **Gap:** forwarding is wired into the `ephpm_db_*` bridge only. A
    /// stock `pdo_mysql` connection to a non-owner node still resolves that
    /// node's *local* database, so its writes are not forwarded and not
    /// replicated. Use the `db-*` drop-in packages (which call `ephpm_db_*`)
    /// on the per-site clustered path.
    ///
    /// **Experimental.** The Turso engine is Beta upstream and this mode
    /// layers per-site CDC on top of it.
    #[serde(default = "default_replication_per_site")]
    pub per_site: bool,
}

impl Default for ReplicationConfig {
    fn default() -> Self {
        Self {
            role: default_replication_role(),
            primary_grpc_url: String::new(),
            cdc_experimental: false,
            max_snapshot_bytes: default_max_snapshot_bytes(),
            per_site: default_replication_per_site(),
        }
    }
}

/// Default for [`ReplicationConfig::per_site`]: off. Per-site clustered
/// replication is experimental and opt-in — a clustered multi-site config
/// keeps its pre-existing shared-database behaviour unless this is set.
fn default_replication_per_site() -> bool {
    false
}

fn default_max_snapshot_bytes() -> u64 {
    1024 * 1024 * 1024
}

/// Configuration for a single database backend (`MySQL` or `PostgreSQL`).
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
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

/// Hand-written so a Rust-constructed `DbBackendConfig` lands on exactly
/// the values an empty TOML table would produce. A derived `Default` would
/// give `min_connections = 0` and empty duration strings — values no
/// TOML-loaded config can ever hold, which is a trap for tests.
impl Default for DbBackendConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            listen: None,
            socket: None,
            min_connections: default_min_connections(),
            max_connections: default_max_connections(),
            idle_timeout: default_idle_timeout(),
            max_lifetime: default_max_lifetime(),
            pool_timeout: default_pool_timeout(),
            health_check_interval: default_health_check_interval(),
            inject_env: default_inject_env(),
            reset_strategy: default_reset_strategy(),
            replicas: None,
        }
    }
}

/// Read replica configuration for a database backend.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ReplicasConfig {
    /// Replica database URLs. Reads are distributed across these;
    /// writes always go to the primary.
    pub urls: Vec<String>,
}

/// Read/write splitting configuration (`[db.read_write_split]`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
    /// Anything else is rejected by [`Config::validate`] at startup — an
    /// unrecognised value used to fall back to `"allkeys-lru"` silently.
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
    /// the derived password into PHP's `$_SERVER` superglobal as
    /// `EPHPM_REDIS_PASSWORD` for each request. (It is a `$_SERVER` variable,
    /// registered through the SAPI's `register_server_variables` hook — not a
    /// process environment variable, so `$_ENV` and `getenv()` do not see it.)
    ///
    /// This secret is **required** to run the RESP listener in multi-tenant
    /// (`sites_dir`) mode: with `[kv.redis_compat] enabled = true` and
    /// `sites_dir` set but no secret, per-site AUTH cannot be derived and the
    /// listener would serve one shared global store to every tenant — so
    /// [`Config::validate`] **refuses to start** (fail closed). In single-site
    /// (no `sites_dir`) mode the shared store is the intended behavior and a
    /// secret is not required; RESP AUTH is then a no-op unless a
    /// `[kv.redis_compat] password` is set.
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

impl KvConfig {
    /// Whether a usable `[kv] secret` is configured.
    ///
    /// A secret that is absent, empty, or whitespace-only cannot derive
    /// per-site RESP AUTH passwords (`HMAC-SHA256(secret, hostname)`), so it is
    /// treated as unset. This is the fail-closed signal used to decide whether
    /// a multi-tenant RESP listener may start — see [`Config::validate`].
    #[must_use]
    pub fn secret_is_set(&self) -> bool {
        self.secret.as_deref().is_some_and(|s| !s.trim().is_empty())
    }
}

/// RESP protocol listener configuration (`[kv.redis_compat]`).
///
/// **Security note for virtual hosting:** whether the RESP endpoint is
/// per-tenant scoped depends entirely on `[kv] secret`.
///
/// - **`secret` set, in multi-tenant mode** (`[server] sites_dir` set, which
///   is what wires the `MultiTenantStore`): a client must send
///   `AUTH <hostname> <HMAC-SHA256(secret, hostname)>`, and the connection is
///   then bound to that hostname's own `Store` for its lifetime. That is real
///   per-tenant isolation — separate stores, not a key-prefix convention — and
///   it is the same store the site's `ephpm_kv_*` SAPI calls use.
/// - **`secret` unset:** there is no per-tenant scoping. Every connection
///   dispatches against the process-wide default store, so any client that can
///   reach the listener sees every key, including other tenants'. The optional
///   `password` below gates access but does not scope it. In multi-tenant
///   deployments, either set `[kv] secret` or leave `enabled = false` and let
///   PHP use the `ephpm_kv_*` SAPI functions, which are namespaced per vhost
///   regardless of this listener.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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

/// FPM request-execution engine (fpm mode only).
///
/// Selects **how** a per-request (php-fpm-shaped) PHP execution is scheduled
/// onto an OS thread. Both engines run the byte-for-byte identical per-request
/// setup/teardown (per-site DB session, KV keyspace, `open_basedir`,
/// `max_execution_time`, the bailout crash guard) — they differ only in which
/// thread pool the blocking PHP call lands on. Ignored in worker mode
/// (`mode = "worker"`), where concurrency is bounded by the persistent worker
/// pool instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FpmEngine {
    /// **Default.** Dispatch each PHP request onto tokio's shared
    /// `spawn_blocking` pool. Behaviour is unchanged from every release before
    /// this knob existed. Concurrency is bounded (optionally) by the `[php]
    /// workers` semaphore; the blocking pool itself is never capped, so static
    /// file I/O cannot be starved by slow PHP.
    SpawnBlocking,

    /// **Experimental / opt-in.** Dispatch each PHP request onto ePHPm's OWN
    /// fixed pool of dedicated OS threads (not `spawn_blocking`). The pool size
    /// equals [`PhpConfig::effective_worker_count`] and IS the concurrency cap
    /// for this engine, so the `[php] workers` semaphore is redundant and
    /// bypassed (a full dispatch queue applies backpressure → 504 via the
    /// request timeout; a draining/empty pool → 503). Benchmark before enabling
    /// in production.
    Pool,
}

/// What a PHP-bound request does when there is no execution slot for it
/// (`[php] overload_policy`).
///
/// Overload has to end *somewhere*. Before this knob existed the only two
/// endings were "wait until a slot frees up" and "the client gives up" — an
/// overloaded ePHPm returned no error status of any kind, just 200s and client
/// timeouts (issue #301, measured under open-loop flood at 1.5-3x capacity).
/// [`Self::Shed`] adds the third, honest ending: an immediate `503` with
/// `Retry-After`, cheap enough to answer at any arrival rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverloadPolicy {
    /// **Default.** Queue and wait for an execution slot. Historical behaviour,
    /// unchanged: the pool engine's bounded dispatch backlog applies
    /// backpressure and the `[php] workers` semaphore makes requests line up,
    /// with the outer `[server.timeouts] request` deadline as the only bound.
    Wait,

    /// Reject with `503 Service Unavailable` + `Retry-After` once a request has
    /// waited `[php] shed_after_ms` for an execution slot, instead of waiting
    /// further. Turns overload into fast, countable errors rather than client
    /// timeouts — the load-shedding behaviour a proxy or load balancer expects
    /// to see from a saturated backend.
    Shed,
}

/// PHP runtime configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhpConfig {
    /// Maximum PHP script execution time, in seconds (`0` = unlimited).
    ///
    /// Natively enforced on Linux ZTS builds whose libphp was compiled with
    /// per-thread execution timers (`--enable-zend-max-execution-timers`, the
    /// default for the shipped Linux SDK). ePHPm writes this value into the
    /// generated php.ini and PHP arms its own **per-thread POSIX timer**
    /// (`timer_create` + `SIGRTMIN`, delivered only to the owning PHP thread —
    /// safe under tokio's `spawn_blocking` pool). The limit is **wall-clock**
    /// (CLOCK_BOOTTIME, so `sleep()` counts), **catchable**, and overridable at
    /// runtime with `set_time_limit()`. Exceeding it raises the standard PHP
    /// fatal ("Maximum execution time exceeded"), runs registered shutdown
    /// functions, and flushes buffered output — an ordinary HTTP 500, not a
    /// hard worker kill.
    ///
    /// `[server.timeouts] request` remains the OUTER hard ceiling, enforced at
    /// the HTTP layer: it still fires (504) for a request wedged in a C
    /// extension or syscall that never returns to the VM to observe the timer.
    /// Keep `max_execution_time` below the request timeout, or the outer
    /// deadline preempts it (startup warns when it does not).
    ///
    /// On builds without per-thread timers (macOS, Windows — ZTS but its SDK
    /// lacks `ZEND_MAX_EXECUTION_TIMERS` — or a Linux SDK built without the
    /// flag) PHP's only native mechanism is the process-wide
    /// setitimer/SIGPROF timer, which is unsafe on tokio worker threads and
    /// stays disabled — there this value is not natively enforced and the
    /// request-layer deadline is the only ceiling (startup warns).
    ///
    /// Default: `30`.
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
    /// (container cgroup limit or Windows job-object limit, else total physical
    /// RAM — `MemTotal` / `GlobalMemoryStatusEx`): ~18% of memory, clamped
    /// to `[64, 512]` MB on Unix and `[64, 256]` MB on Windows, where the
    /// segment is pagefile-backed and commit-charged in full at startup (see
    /// [`opcache_shm_ceiling_mb`]). Set explicitly to pin the SHM size
    /// regardless of the detected budget — an explicit value is honoured on
    /// every platform, though on Windows one above the ceiling also warns at
    /// startup ([`AutoTune::shm_warning`]). Only takes effect in serve mode
    /// (dev keeps PHP's default of 128 MB unless you set this).
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
    /// budget, clamped `[32, 64]` MB) and emits it in serve mode. This sizes
    /// the buffer only — whether the JIT *uses* it is governed by
    /// [`Self::opcache_jit`] (shaped default: `disable` in every mode, on
    /// every platform). When the JIT is on and no size was derived (dev
    /// mode), the same derivation is forced so an enabled JIT is never
    /// silently bufferless. An explicit `0` is respected but makes the JIT
    /// inert — startup warns.
    ///
    /// Default: `None` (auto-derived buffer in serve; JIT state is governed
    /// by [`Self::opcache_jit`]).
    #[serde(default)]
    pub opcache_jit_buffer_size: Option<u32>,

    /// OPcache JIT mode (`opcache.jit`): `"tracing"`, `"function"`, or
    /// `"disable"`.
    ///
    /// When set explicitly it wins in **every** mode (dev, serve, worker,
    /// multi-tenant) and is written into the generated php.ini as
    /// `opcache.jit=<value>`; a non-`disable` value also guarantees a
    /// non-zero `opcache.jit_buffer_size` (the autotune-derived size, see
    /// [`Self::opcache_jit_buffer_size`]).
    ///
    /// When **absent**, ePHPm picks a shaped default:
    ///
    /// - **Single-site `ephpm serve`** (no `[server] sites_dir`, `[php] mode`
    ///   not `"worker"`): **`disable`** — PHP's tracing JIT dereferences a
    ///   freed `op_array` when it compiles a **side trace** in a *later*
    ///   request than the one that compiled the parent trace, and kills the
    ///   process: `0xC0000005` on Windows, `SIGSEGV` on Unix, with no PHP
    ///   error and nothing in the ePHPm log. This is an upstream php-src
    ///   defect, not an ePHPm one — it reproduces on **stock `php -S`** with
    ///   no ePHPm involved, on Windows *and* Linux. Tracked at php-src PR
    ///   <https://github.com/php/php-src/pull/21710> (open); introduced by
    ///   php-src PR 21368, first released in PHP 8.4.24 / 8.5.5. Was
    ///   Windows-only in #372 and extended to every platform once the Linux
    ///   reproduction landed; revisit when the fix ships and the pinned SDK is
    ///   bumped past it. See [`JitReason::TracingJitBug`] for the mechanism
    ///   and the measured request counts.
    /// - **Multi-tenant serve** (`[server] sites_dir` set): **`disable`** —
    ///   per-vhost deploys invalidate OPcache via `opcache_invalidate`, and
    ///   invalidation **never reclaims JIT buffer** (measured: `buffer_free`
    ///   is untouched; only a full `opcache_reset` reclaims, and that is
    ///   disabled by the multi-tenant hardening preset). Deploy churn would
    ///   silently fill the buffer until the JIT stops compiling with no
    ///   error. Set the knob explicitly to accept that cost.
    /// - **Worker mode** (`[php] mode = "worker"`): **`disable`** — the JIT
    ///   has not been positively verified against the persistent-worker
    ///   request lifecycle (one long-lived PHP request per worker). Opt in
    ///   explicitly if your workload benefits.
    /// - **Dev mode**: **`disable`** via PHP's own defaults (no
    ///   `opcache.jit` line is emitted).
    ///
    /// Escape hatch: a suspected JIT miscompile is turned off with
    /// `opcache_jit = "disable"` — no other change required. Watch the
    /// `ephpm_opcache_jit_buffer_free_bytes` gauge for buffer exhaustion.
    /// `"function"` is the middle setting for anyone who still wants a JIT: it
    /// compiles whole hot functions and never builds traces, so it cannot hit
    /// the side-trace defect above (verified clean on Windows over 150
    /// requests and on Linux over 300 where `"tracing"` dies at 2–3).
    ///
    /// Env override: `EPHPM_PHP__OPCACHE_JIT`.
    ///
    /// Default: `None` (shaped: `disable` in every mode — see above for the
    /// per-mode reason).
    #[serde(default)]
    pub opcache_jit: Option<JitMode>,

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
    /// minor version, ZTS thread-safety mode (every ePHPm platform is ZTS,
    /// Windows included — #326), and — on Linux — glibc (the release binary
    /// is glibc-dynamic). PHP verifies this at startup and rejects a
    /// mismatch with a clear "Unable to load dynamic library" error instead
    /// of crashing (verified: an NTS build fails with `undefined symbol:
    /// compiler_globals`). Note that Debian/Sury `php8.5-<ext>` packages
    /// are NTS-only (no `-zts` variants exist as of 2026-07), so on Linux a
    /// shared extension must currently be compiled for ZTS — e.g. `phpize`
    /// against a ZTS PHP of the same minor, or `gcc -shared` against the
    /// matching php-sdk headers. macOS (ZTS `.dylib`) works the same way;
    /// Windows `.dll` loading is not yet validated (a DLL would also need
    /// to be a ZTS build).
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

    /// FPM request-execution engine (fpm mode only).
    ///
    /// - `"spawn_blocking"` (**default**) — run each PHP request on tokio's
    ///   shared blocking pool. Unchanged from every prior release.
    /// - `"pool"` (**EXPERIMENTAL, opt-in**) — run each PHP request on ePHPm's
    ///   own dedicated OS-thread pool sized to
    ///   [`Self::effective_worker_count`]. The pool size is the concurrency cap,
    ///   so `[php] workers` is bypassed. Benchmark-it-first: intended to be
    ///   flipped on in the lab and compared against the default.
    ///
    /// An unrecognised value is a **startup error** (serde rejects it), never a
    /// silent fallback. Ignored in worker mode (`mode = "worker"`); startup logs
    /// a WARN if `pool` is requested there so the no-op is never silent.
    ///
    /// Env override: `EPHPM_PHP__FPM_ENGINE=pool`.
    ///
    /// Default: `"spawn_blocking"`.
    #[serde(default = "default_fpm_engine")]
    pub fpm_engine: FpmEngine,

    /// **EXPERIMENTAL.** Contain a PHP C-stack overflow instead of letting it
    /// abort the whole process (`fpm_engine = "pool"` only).
    ///
    /// A deep object-graph free (`zend_object_std_dtor` ↔
    /// `zend_objects_store_del` C recursion) overflows the executing thread's
    /// stack and the resulting `SIGSEGV` kills the **entire** server — every
    /// other tenant with it. With this on, that specific fault class is caught
    /// at the signal handler, the offending request is answered `500`, and the
    /// thread that ran it is retired and replaced.
    ///
    /// **Only stack-overflow faults are contained.** Heap corruption and wild
    /// writes produce the same `SIGSEGV` but may already have damaged another
    /// thread's or a shared allocator's memory, so they are deliberately **not**
    /// caught — the process still dies with the usual fatal-signal diagnostic.
    /// The two are told apart by the faulting address.
    ///
    /// Costs, so it is off by default: each contained crash abandons the
    /// poisoned thread's Zend context, and once any crash has been contained the
    /// process **skips** PHP module shutdown at exit (walking an abandoned TSRM
    /// entry is a certain `SIGABRT`). The abandoned contexts leak, but the leak
    /// plateaus — measured on a 4-thread pool, RSS rose ~90 MiB over the first
    /// ~1000 contained crashes and then stopped growing.
    ///
    /// Requires `fpm_engine = "pool"`: containment is only safe when ePHPm owns
    /// the executing thread and can genuinely retire it, which tokio's shared
    /// `spawn_blocking` pool cannot do. Setting this without the pool engine (or
    /// in worker mode) logs a WARN at startup and changes nothing.
    ///
    /// Env override: `EPHPM_PHP__CRASH_CONTAINMENT=true`.
    ///
    /// Default: `false`.
    #[serde(default = "default_crash_containment")]
    pub crash_containment: bool,

    /// What to do with a PHP-bound request that cannot get an execution slot
    /// (fpm mode). See [`OverloadPolicy`].
    ///
    /// - `"wait"` (**default**) — queue and wait. Historical behaviour.
    /// - `"shed"` — answer `503` + `Retry-After` after `shed_after_ms` of
    ///   waiting.
    ///
    /// **Unset is not the same as `"wait"`**: an unset value takes the preview
    /// preset (`"shed"`) under `[server] preview = true`, and `"wait"`
    /// otherwise — resolved by [`Config::effective_overload_policy`], the same
    /// explicit-wins rule `[server.limits]` uses. Startup logs which one is in
    /// force and what it will actually do on the active engine.
    ///
    /// What `"shed"` bounds depends on the engine, because that is where the
    /// admission queue lives:
    ///
    /// - `fpm_engine = "pool"` — the bounded dispatch backlog
    ///   (`worker_backlog`, default = pool size). Full backlog → shed.
    /// - `fpm_engine = "spawn_blocking"` (default) — the `[php] workers`
    ///   semaphore. **With `workers = 0` (the default) there is no admission
    ///   queue to bound and nothing is shed**; tokio's blocking queue itself is
    ///   unbounded and its entries are uncancellable, so ePHPm cannot reject
    ///   from there. Startup WARNs about exactly this combination.
    ///
    /// Ignored in worker mode (`mode = "worker"`), which has its own bounded
    /// worker pool; startup WARNs if it is set there.
    ///
    /// Env override: `EPHPM_PHP__OVERLOAD_POLICY=shed`.
    ///
    /// Default: unset (`"wait"`, or `"shed"` under `[server] preview`).
    #[serde(default)]
    pub overload_policy: Option<OverloadPolicy>,

    /// How long, in milliseconds, a request may wait for a PHP execution slot
    /// before `overload_policy = "shed"` answers `503`. Ignored when the policy
    /// is `"wait"`.
    ///
    /// `0` (the default) means "do not wait at all": take a slot if one is
    /// immediately available, otherwise shed. On the pool engine that is the
    /// natural reading of #301's ask — the `worker_backlog` queue is *already*
    /// the buffer, so a full backlog means saturated. On the `spawn_blocking`
    /// engine it makes `[php] workers` a strict concurrency cap with no queue.
    /// Raise it to buy a grace window that absorbs bursts before shedding.
    ///
    /// Env override: `EPHPM_PHP__SHED_AFTER_MS=250`.
    ///
    /// Default: `0` (shed as soon as there is no free slot).
    #[serde(default = "default_shed_after_ms")]
    pub shed_after_ms: u64,

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
    /// Applies on every platform: Windows is ZTS like Linux/macOS (#326), so
    /// multiple workers serve concurrently there too. (Historically this was
    /// forced to `1` on Windows on the wrong belief that Windows builds were
    /// NTS.)
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
        // Reading the `EPHPM_*` environment is a read of process-global state.
        // Under `cfg(test)` that read is serialised against the tests that
        // override those variables — see `crate::test_env` for the full story.
        // Compiled out entirely in real builds.
        #[cfg(test)]
        let _env_guard = test_env::read_guard();

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
        // See the note in `load` — same process-global read, same guard.
        #[cfg(test)]
        let _env_guard = test_env::read_guard();

        let config = Figment::new()
            .merge(Env::prefixed("EPHPM_").split("__"))
            .extract()
            .map_err(Box::new)?;
        Ok(config)
    }

    /// Resolve `[php] overload_policy` against the `[server] preview` preset.
    ///
    /// An explicit value always wins — including an explicit `"wait"` under
    /// preview, which is how an operator opts *out* of the preset. An unset
    /// value is [`OverloadPolicy::Shed`] under `[server] preview = true` and
    /// [`OverloadPolicy::Wait`] otherwise: a preview box is exactly the place
    /// where a fast, honest `503` beats a request that quietly eats the node
    /// for the client's whole timeout.
    ///
    /// Cross-section on purpose (`[php]` knob, `[server]` preset), which is why
    /// it lives here and not on [`PhpConfig`] — there is one resolution and one
    /// place to find it, mirroring [`ServerConfig::effective_limits`].
    #[must_use]
    pub fn effective_overload_policy(&self) -> OverloadPolicy {
        self.php.overload_policy.unwrap_or(if self.server.preview {
            OverloadPolicy::Shed
        } else {
            OverloadPolicy::Wait
        })
    }

    /// Whether [`Self::effective_overload_policy`] came from the preview preset
    /// rather than from an operator-set value. Startup logging uses it to name
    /// the source, so the preset is never silent.
    #[must_use]
    pub fn overload_policy_from_preview_preset(&self) -> bool {
        self.server.preview && self.php.overload_policy.is_none()
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

        // [server.tenant_network] ebpf_policy: fail closed, never a silent
        // no-op. The runtime-capability gate (kernel too old / no BTF / missing
        // CAP_BPF) can't be decided from config alone — that is a hard startup
        // error in serve() at load time. Here we catch the statically-decidable
        // misconfigurations.
        if self.server.tenant_network.ebpf_policy {
            // (1) Platform gate — the BPF hooks are Linux-only.
            if !cfg!(target_os = "linux") {
                return Err(ConfigError::Validation(
                    "[server.tenant_network] ebpf_policy = true is Linux-only \
                     (cgroup/bind4 + connect4 BPF hooks). Remove it on this \
                     platform, or run ePHPm on Linux >= 5.10 with CONFIG_CGROUP_BPF \
                     and BTF."
                        .to_string(),
                ));
            }
            // (2) Per-vhost tagging is keyed by the canonical site key, which
            //     only exists in multi-tenant mode (sites_dir set).
            if self.server.sites_dir.is_none() {
                return Err(ConfigError::Validation(
                    "[server.tenant_network] ebpf_policy = true requires \
                     [server] sites_dir (multi-tenant mode) — there are no \
                     vhosts to isolate in single-site mode."
                        .to_string(),
                ));
            }
            // (3) Current scope: the tag is written on the fpm per-request path
            //     (run_php). Worker mode's persistent loop would need per-event
            //     tagging inside the PSR-7 envelope — deferred.
            //
            //     Today this is belt-and-suspenders: `mode = "worker"` + sites_dir
            //     is itself a hard error (per-host worker pools are a later
            //     phase — see the worker-mode rule below), and ebpf_policy
            //     requires sites_dir via (2), so worker+ebpf can't reach the
            //     runtime anyway. Kept as an explicit, feature-scoped message so
            //     that if per-host worker pools ever land, the eBPF feature still
            //     correctly declares itself fpm-only until per-event tagging is
            //     added.
            if self.php.is_worker_mode() {
                return Err(ConfigError::Validation(
                    "[server.tenant_network] ebpf_policy = true is not yet \
                     supported with [php] mode = \"worker\" (fpm mode only)."
                        .to_string(),
                ));
            }
            // (4) A malformed sidecar_port_range is a fail-closed startup error,
            //     not a silent fallback to the default. The kernel-ephemeral
            //     OVERLAP check is a /proc read done in serve() at load time.
            self.server
                .tenant_network
                .parse_range()
                .map_err(|e| ConfigError::Validation(format!("[server.tenant_network] {e}")))?;
        }

        // Native WebSockets dispatch each event through the fpm per-request
        // path (a fresh entrypoint execution per event). Worker mode routes
        // every request into the persistent worker's PSR-7 envelope loop
        // instead, so the entrypoint would never run. Hard error rather than a
        // warning: ePHPm does not come up quietly without the WebSocket support
        // an operator asked for (the `[server.http3]` precedent).
        //
        // Checked BEFORE the worker-mode block below so the incompatibility is
        // what the operator is told about, rather than a downstream complaint
        // about the worker script.
        if self.server.websocket.enabled && self.php.is_worker_mode() {
            return Err(ConfigError::Validation(
                "[server.websocket] enabled = true is not supported together with \
                 [php] mode = \"worker\". WebSocket events are dispatched through \
                 the fpm per-request path; in worker mode every request is served \
                 by the persistent worker loop, so the `websocket_files` \
                 entrypoint would never execute. Use fpm mode (the default) for \
                 WebSockets."
                    .to_string(),
            ));
        }

        // An entrypoint name is what makes a vhost WebSocket-capable at all —
        // an empty list means every upgrade request 404s, which is a silently
        // disabled feature rather than a configuration.
        if self.server.websocket.enabled && self.server.websocket_files.is_empty() {
            return Err(ConfigError::Validation(
                "[server] websocket_files is empty but [server.websocket] enabled = \
                 true — every upgrade request would 404. Name at least one \
                 entrypoint (default: [\"websocket.php\"])."
                    .to_string(),
            ));
        }

        // TLS / ACME challenge validation. All fail-closed: a wildcard cert
        // that cannot be issued, or a dns-01 lane with no credential, must stop
        // startup rather than come up serving no certificate.
        if let Some(tls) = self.server.tls.as_ref() {
            let challenge = tls.challenge.to_ascii_lowercase();
            if challenge != "tls-alpn-01" && challenge != "dns-01" {
                return Err(ConfigError::Validation(format!(
                    "[server.tls] challenge must be \"tls-alpn-01\" or \"dns-01\", got \
                     \"{}\". (tls-alpn-01 is the default; dns-01 is required for wildcard \
                     certificates.)",
                    tls.challenge,
                )));
            }

            // A wildcard identifier can only be validated over DNS-01: the CA
            // has no single host to answer a TLS-ALPN-01 or HTTP-01 challenge
            // for `*.example.com`. Reject the combination loudly rather than
            // let ACME fail opaquely at order time.
            if tls.has_wildcard_domain() && challenge != "dns-01" {
                return Err(ConfigError::Validation(
                    "[server.tls] a wildcard domain (\"*.example.com\") requires \
                     challenge = \"dns-01\". TLS-ALPN-01 cannot prove control of a \
                     wildcard. Set challenge = \"dns-01\" and configure a dns_provider."
                        .to_string(),
                ));
            }

            // dns-01 needs somewhere to put the TXT record and something to
            // request it for. Enforce the full triple: domains, a known
            // provider, and a credential source.
            if challenge == "dns-01" {
                if tls.is_manual() {
                    return Err(ConfigError::Validation(
                        "[server.tls] challenge = \"dns-01\" is an ACME (automatic) mode \
                         but cert/key were also provided. Remove cert/key to use ACME, or \
                         drop the challenge setting to serve the static certificate."
                            .to_string(),
                    ));
                }
                if tls.domains.is_empty() {
                    return Err(ConfigError::Validation(
                        "[server.tls] challenge = \"dns-01\" requires at least one entry in \
                         `domains` (e.g. [\"*.preview.example.com\"])."
                            .to_string(),
                    ));
                }
                // A credential is "present" if a `*_file` path is set or an
                // inline/env value is non-empty.
                let has_cred = |file: &Option<PathBuf>, inline: &Option<String>| -> bool {
                    file.is_some() || inline.as_deref().is_some_and(|t| !t.trim().is_empty())
                };
                let nonempty =
                    |v: &Option<String>| v.as_deref().is_some_and(|s| !s.trim().is_empty());
                match tls.dns_provider.as_deref() {
                    Some(p) if p.eq_ignore_ascii_case("cloudflare") => {
                        if !has_cred(&tls.cloudflare_api_token_file, &tls.cloudflare_api_token) {
                            return Err(ConfigError::Validation(
                                "[server.tls] dns_provider = \"cloudflare\" requires an API \
                                 token: set `cloudflare_api_token_file` (a zone-scoped \
                                 Zone.DNS:Edit token) or the \
                                 EPHPM_SERVER__TLS__CLOUDFLARE_API_TOKEN environment variable."
                                    .to_string(),
                            ));
                        }
                    }
                    Some(p) if p.eq_ignore_ascii_case("linode") => {
                        if !has_cred(&tls.linode_api_token_file, &tls.linode_api_token) {
                            return Err(ConfigError::Validation(
                                "[server.tls] dns_provider = \"linode\" requires a token: set \
                                 `linode_api_token_file` or the \
                                 EPHPM_SERVER__TLS__LINODE_API_TOKEN environment variable."
                                    .to_string(),
                            ));
                        }
                    }
                    Some(p) if p.eq_ignore_ascii_case("digitalocean") => {
                        if !has_cred(&tls.digitalocean_api_token_file, &tls.digitalocean_api_token)
                        {
                            return Err(ConfigError::Validation(
                                "[server.tls] dns_provider = \"digitalocean\" requires a token: \
                                 set `digitalocean_api_token_file` or the \
                                 EPHPM_SERVER__TLS__DIGITALOCEAN_API_TOKEN environment variable."
                                    .to_string(),
                            ));
                        }
                    }
                    Some(p) if p.eq_ignore_ascii_case("route53") => {
                        if !nonempty(&tls.route53_access_key_id) {
                            return Err(ConfigError::Validation(
                                "[server.tls] dns_provider = \"route53\" requires \
                                 `route53_access_key_id`."
                                    .to_string(),
                            ));
                        }
                        if !has_cred(
                            &tls.route53_secret_access_key_file,
                            &tls.route53_secret_access_key,
                        ) {
                            return Err(ConfigError::Validation(
                                "[server.tls] dns_provider = \"route53\" requires a secret key: \
                                 set `route53_secret_access_key_file` or the \
                                 EPHPM_SERVER__TLS__ROUTE53_SECRET_ACCESS_KEY environment variable."
                                    .to_string(),
                            ));
                        }
                    }
                    Some(p) if p.eq_ignore_ascii_case("google") => {
                        if !has_cred(
                            &tls.google_service_account_json_file,
                            &tls.google_service_account_json,
                        ) {
                            return Err(ConfigError::Validation(
                                "[server.tls] dns_provider = \"google\" requires a \
                                 service-account key: set `google_service_account_json_file` or \
                                 the EPHPM_SERVER__TLS__GOOGLE_SERVICE_ACCOUNT_JSON environment \
                                 variable."
                                    .to_string(),
                            ));
                        }
                        if !nonempty(&tls.google_project) {
                            return Err(ConfigError::Validation(
                                "[server.tls] dns_provider = \"google\" requires `google_project`."
                                    .to_string(),
                            ));
                        }
                    }
                    Some(other) => {
                        return Err(ConfigError::Validation(format!(
                            "[server.tls] dns_provider = \"{other}\" is not supported; \
                             implemented providers: cloudflare, linode, digitalocean, route53, \
                             google.",
                        )));
                    }
                    None => {
                        return Err(ConfigError::Validation(
                            "[server.tls] challenge = \"dns-01\" requires `dns_provider` \
                             (cloudflare, linode, digitalocean, route53, or google)."
                                .to_string(),
                        ));
                    }
                }
            }
        }

        // Fail closed (issue #397): a `sites_domain_suffix` without a leading
        // dot is an operator error with a critical consequence. The suffix is
        // stripped off the incoming `Host` to derive the vhost key, so a dotless
        // suffix lets `Host: <suffix>` strip to the EMPTY key — and an empty key
        // joined onto `sites_dir` is `sites_dir` itself, i.e. one vhost whose
        // document root and `open_basedir` become the entire tenant fleet
        // (cross-tenant read AND write). A legitimate suffix always begins with
        // `.` (`.localhost`, `.preview.ephpm.dev`) so that only a genuine
        // subdomain — never the apex — matches. There is no legitimate dotless
        // suffix, so reject rather than silently normalize: normalizing would
        // hide the mistake, and the operator should see it at startup. (The
        // router re-validates the stripped key as a second, load-bearing layer;
        // this check surfaces the misconfiguration before it can matter.)
        if let Some(suffix) = self.server.sites_domain_suffix.as_deref()
            && !suffix.starts_with('.')
        {
            return Err(ConfigError::Validation(format!(
                "[server] sites_domain_suffix ({suffix:?}) must begin with a dot \
                 (e.g. \".localhost\" or \".preview.ephpm.dev\"). Without the leading \
                 dot the apex host `Host: {suffix}` strips to the empty vhost key, \
                 which resolves the entire sites_dir as a single virtual host — every \
                 tenant's files would share one open_basedir. Prefix the suffix with \
                 a dot so only subdomains match, or remove the setting to name \
                 directories with their full host.",
            )));
        }

        // Fail closed: per-site overrides are trusted because they live where no
        // tenant can write. A `site_overrides_dir` inside `sites_dir` is inside
        // some tenant's container, hence inside that tenant's `open_basedir`,
        // hence rewritable by that tenant's own PHP — at which point ePHPm would
        // be taking routing instructions from the code it is sandboxing. Refuse
        // to start rather than serve with the property quietly absent.
        if let (Some(overrides), Some(sites)) =
            (self.server.site_overrides_dir.as_deref(), self.server.sites_dir.as_deref())
            && overrides_dir_is_inside_sites_dir(overrides, sites)
        {
            return Err(ConfigError::Validation(format!(
                "[server] site_overrides_dir ({}) is inside [server] sites_dir ({}). \
                 Per-site overrides are trusted precisely because tenants cannot write \
                 them; a directory inside sites_dir is inside a tenant's own \
                 open_basedir, so its PHP could rewrite its own routing. Put the \
                 override directory somewhere no tenant checkout lives, e.g. \
                 /var/lib/ephpm/site-overrides.",
                overrides.display(),
                sites.display(),
            )));
        }

        // Never a silent no-op: `site_overrides_dir` only acts in multi-tenant
        // mode. `ephpm-config` has no `tracing` dependency (config returns data;
        // the binary and the server log), so the inert case is surfaced by
        // `Router::new` at startup — the same shape as the inert
        // `[server.security]` flags.

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
        let mut php_mounts = 0usize;
        for (i, mount) in self.middleware.iter().enumerate() {
            if mount.library.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "[[middleware]] entry {i} (order = {}): library must not be empty",
                    mount.order,
                )));
            }

            let Some(script) = mount.php_script() else { continue };
            php_mounts += 1;
            let bad = |why: &str| {
                Err(ConfigError::Validation(format!(
                    "[[middleware]] entry {i} (library = \"{}\"): {why}",
                    mount.library,
                )))
            };

            if script.is_empty() {
                return bad("\"php:\" must be followed by a script path");
            }
            // A `php:` path is joined onto the REQUEST's document root, which
            // in multi-tenant mode is the tenant's own directory. Every shape
            // that could make that join land somewhere else is refused here,
            // at startup, rather than being re-checked per request:
            //
            //   - absolute paths and Windows drive prefixes escape the join
            //     entirely, and would additionally sit outside every vhost's
            //     `open_basedir`, so PHP could not load them anyway;
            //   - `..` walks out of the document root;
            //   - a backslash is a separator on Windows but an ordinary
            //     filename character on Unix, so allowing it would make the
            //     same config mean two different things.
            if script.starts_with('/') || script.starts_with('\\') {
                return bad(
                    "the script path must be relative to the document root, not absolute — \
                     an operator-owned file shared by every tenant would need a hole in each \
                     vhost's open_basedir, which this lane deliberately does not create",
                );
            }
            if script.contains('\\') {
                return bad("the script path must use `/` as its separator, not `\\`");
            }
            if script.len() >= 2 && script.as_bytes()[1] == b':' {
                return bad(
                    "the script path must be relative to the document root, not a drive path",
                );
            }
            if script.split('/').any(|seg| seg == "..") {
                return bad("the script path must not contain `..`");
            }
            // Worker mode boots the framework once and owns the request loop;
            // there is no per-request `php_request_startup` to prepend into, so
            // the mount would never run. Refuse rather than mount a policy
            // layer that silently does nothing.
            if self.php.mode == "worker" {
                return bad(
                    "PHP middleware is not supported in worker mode (`[php] mode = \"worker\"`) — \
                     the worker script owns the request loop; use the framework's own middleware \
                     (PSR-15 / Octane) there",
                );
            }
        }
        if php_mounts > MAX_PHP_MIDDLEWARE {
            return Err(ConfigError::Validation(format!(
                "{php_mounts} `php:` [[middleware]] mounts declared, but at most \
                 {MAX_PHP_MIDDLEWARE} can run per request",
            )));
        }

        // Fail closed: a multi-tenant RESP listener with no `[kv] secret` would
        // serve ONE shared global KV store to every tenant with no per-site
        // authentication. The RESP AUTH scoping that isolates tenants
        // (`AUTH <hostname> <derived-password>` → that vhost's store) can only
        // be derived from `[kv] secret`; without it every client reaching the
        // listener talks to the shared default store and can read and write
        // every site's KV. Refuse to start rather than expose it silently.
        if self.server.sites_dir.is_some()
            && self.kv.redis_compat.enabled
            && !self.kv.secret_is_set()
        {
            return Err(ConfigError::Validation(
                "[kv.redis_compat] enabled = true with [server] sites_dir \
                 (multi-tenant vhosting) but no [kv] secret: the RESP listener \
                 would serve a single shared KV store to every tenant with no \
                 authentication, exposing every site's keys to every other \
                 site. Set [kv] secret (e.g. `openssl rand -base64 32`) so \
                 per-site RESP AUTH (`AUTH <hostname> <derived-password>`) \
                 scopes each connection to its own site store, or disable the \
                 listener with [kv.redis_compat] enabled = false. Single-site \
                 deployments (no [server] sites_dir) are unaffected."
                    .to_string(),
            ));
        }

        // [kv] eviction_policy: `EvictionPolicy::from_str_lossy` maps every
        // unrecognised string to `allkeys-lru`, so a typo like
        // "allkey-lru" would silently turn `noeviction` into eviction —
        // data loss under memory pressure that the operator explicitly
        // asked not to happen. Reject it here, the same way `[php] mode`
        // rejects a typo'd mode (the no-silent-knob rule).
        if !KV_EVICTION_POLICIES.contains(&self.kv.eviction_policy.as_str()) {
            return Err(ConfigError::Validation(format!(
                "[kv] eviction_policy must be one of {}, got \"{}\"",
                KV_EVICTION_POLICIES.join(", "),
                self.kv.eviction_policy,
            )));
        }

        // [[server.proxy]]: validate every rule at startup so a bad upstream,
        // an out-of-scope-for-v1 URL (https/path), or a malformed host matcher
        // fails closed rather than being silently dropped from the rule list.
        for (i, rule) in self.server.proxy.iter().enumerate() {
            if !rule.path.starts_with('/') {
                return Err(ConfigError::Validation(format!(
                    "[[server.proxy]] rule {i}: path {:?} must begin with '/'",
                    rule.path,
                )));
            }
            rule.validate_host()
                .map_err(|e| ConfigError::Validation(format!("[[server.proxy]] rule {i}: {e}")))?;
            rule.upstream_authority()
                .map_err(|e| ConfigError::Validation(format!("[[server.proxy]] rule {i}: {e}")))?;
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
        if let Ok(canon_root) = doc_root.canonicalize()
            && !canon_script.starts_with(&canon_root)
        {
            return Err(ConfigError::Validation(format!(
                "[php] worker_script {} resolves outside document_root {}",
                canon_script.display(),
                canon_root.display(),
            )));
        }

        // On Windows, `canonicalize()` returns an extended-length *verbatim*
        // path (`\\?\C:\...`). PHP's stream layer cannot open verbatim paths —
        // the worker boot's `require` fails with "Failed to open stream: No
        // such file or directory", so every worker dies before reaching
        // take_request() and worker mode never comes up on Windows. Simplify
        // back to a normal path before handing it to PHP. The containment
        // check above already ran on the verbatim forms (both sides
        // canonicalized, so their prefixes agree).
        Ok(strip_verbatim_prefix(canon_script))
    }
}

/// Strip Windows' verbatim (extended-length) `\\?\` prefix from a
/// canonicalized path so it can be consumed by PHP's stream layer, which
/// cannot open verbatim paths.
///
/// `\\?\C:\dir\file` becomes `C:\dir\file`, and `\\?\UNC\server\share\p`
/// becomes `\\server\share\p`. Non-verbatim paths — and every path on
/// non-Windows targets — are returned unchanged.
///
/// Public because **every** path that reaches PHP after a `canonicalize()` needs
/// this, not just `worker_script`: the per-site document-root override
/// (`ephpm-server`'s `site_overrides`) canonicalizes to prove containment and
/// then hands the result to PHP as `DOCUMENT_ROOT`/`SCRIPT_FILENAME`. Two copies
/// of this would be two places to forget the `UNC` case.
#[must_use]
pub fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.as_os_str().to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest.to_string());
        }
    }
    path
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            document_root: default_document_root(),
            sites_dir: None,
            site_overrides_dir: None,
            run_as_user: None,
            run_as_group: None,
            sites_domain_suffix: None,
            index_files: default_index_files(),
            websocket_files: default_websocket_files(),
            fallback: default_fallback(),
            preview: false,
            request: RequestConfig::default(),
            timeouts: TimeoutsConfig::default(),
            response: ResponseConfig::default(),
            static_files: StaticConfig::default(),
            php_etag_cache: PhpETagCacheConfig::default(),
            security: None,
            tenant_network: TenantNetworkConfig::default(),
            logging: LoggingConfig::default(),
            metrics: MetricsConfig::default(),
            diagnostics: DiagnosticsConfig::default(),
            limits: LimitsConfig::default(),
            file_cache: FileCacheConfig::default(),
            tls: None,
            http3: Http3Config::default(),
            websocket: WebSocketConfig::default(),
            proxy: Vec::new(),
        }
    }
}

impl Default for RequestConfig {
    fn default() -> Self {
        Self {
            max_body_size: default_max_body_size(),
            max_header_size: default_max_header_size(),
            trusted_hosts: Vec::new(),
            middleware_body_limit: default_middleware_body_limit(),
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
            opcache_jit: None,
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
            fpm_engine: default_fpm_engine(),
            crash_containment: default_crash_containment(),
            overload_policy: None,
            shed_after_ms: default_shed_after_ms(),
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

    /// Whether the experimental dedicated FPM thread-pool engine is requested
    /// **and applicable** — i.e. `fpm_engine = "pool"` in fpm mode. Always
    /// `false` in worker mode (the persistent worker pool owns concurrency
    /// there, so `fpm_engine` is inert). This is the single predicate the server
    /// uses to decide whether to build the pool and bypass the `workers`
    /// semaphore, so the two decisions can never disagree.
    #[must_use]
    pub fn is_pool_engine(&self) -> bool {
        !self.is_worker_mode() && self.fpm_engine == FpmEngine::Pool
    }

    /// Whether stack-overflow crash containment is requested **and applicable**
    /// — i.e. `crash_containment = true` together with the dedicated FPM thread
    /// pool ([`Self::is_pool_engine`]).
    ///
    /// Containment is deliberately gated on the pool engine: recovering from the
    /// fault leaves the executing thread's Zend context poisoned, so the *only*
    /// safe follow-up is to retire that OS thread and spawn a replacement —
    /// which is possible only on threads ePHPm owns. On tokio's shared
    /// `spawn_blocking` pool a poisoned thread stays in rotation and fails every
    /// later request, which is worse than the crash it prevented.
    ///
    /// Startup warns when `crash_containment` is set but this returns `false`,
    /// so the no-op is never silent.
    #[must_use]
    pub fn is_crash_containment_active(&self) -> bool {
        self.crash_containment && self.is_pool_engine()
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
        // memory_limit does not depend on the tenancy shape, so the
        // multi-tenant flag is irrelevant here.
        self.autotune(dev_mode, false).memory_limit.value
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
    ///
    /// `multi_tenant` is `true` when `[server] sites_dir` is set — it shapes
    /// the `opcache.jit` default (JIT off in multi-tenant mode, because
    /// per-vhost invalidation never reclaims JIT buffer; see
    /// [`Self::opcache_jit`]).
    #[must_use]
    pub fn autotune(&self, dev_mode: bool, multi_tenant: bool) -> AutoTune {
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

        // opcache.jit: explicit knob wins everywhere; otherwise the shaped
        // default is `disable` in every mode, for a different reason in each —
        // multi-tenant (invalidation never reclaims JIT buffer), worker mode
        // (not positively verified against the persistent-worker lifecycle),
        // dev (line omitted, PHP defaults keep the JIT off), and single-site
        // serve (upstream tracing-JIT use-after-free, #365 — see
        // `JitReason::TracingJitBug`). The order of these arms is the order
        // the reasons are reported in, not a precedence over behaviour: they
        // all resolve to the same mode.
        let worker_mode = self.mode == "worker";
        let (jit_mode, jit_reason) = match self.opcache_jit {
            Some(mode) => {
                (TunedValue { value: mode, origin: Origin::Explicit }, JitReason::Explicit)
            }
            None if dev_mode => {
                (TunedValue { value: JitMode::Disable, origin: Origin::Default }, JitReason::Dev)
            }
            None if multi_tenant => (
                TunedValue { value: JitMode::Disable, origin: Origin::Derived },
                JitReason::MultiTenant,
            ),
            None if worker_mode => (
                TunedValue { value: JitMode::Disable, origin: Origin::Derived },
                JitReason::WorkerMode,
            ),
            None => (
                TunedValue { value: JitMode::Disable, origin: Origin::Derived },
                JitReason::TracingJitBug,
            ),
        };

        // A JIT that is on needs a non-zero buffer. Serve mode always derives
        // one; the remaining case is an explicit `opcache_jit` in dev mode,
        // where no derivation ran and the bottom tier is 0 (PHP ≤8.3's stock
        // default) — force the same memory-shaped derivation so an explicit
        // "tracing" can never be silently bufferless.
        let mut jit_buffer_size =
            resolve(self.opcache_jit_buffer_size, derived.opcache_jit_buffer_size, 0);
        if jit_mode.value.is_on() && jit_buffer_size.origin == Origin::Default {
            jit_buffer_size =
                TunedValue { value: derive_jit_buffer_mb(mem_budget), origin: Origin::Derived };
        }

        AutoTune {
            cpu_quota,
            mem_budget,
            mem_source,
            workers,
            worker_source,
            dev_mode,
            multi_tenant,
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
            jit: jit_mode,
            jit_reason,
            jit_buffer_size,
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
    /// [`Self::autotune`]): `opcache.validate_timestamps` and `memory_limit`
    /// (both always), plus every tunable whose value came from explicit config
    /// **or** a serve-mode derivation. Values that resolved to the PHP stock
    /// default are omitted so the engine's own default applies (keeping dev
    /// mode's php.ini minimal).
    /// All lines are emitted *before* user `ini_overrides`, so an operator can
    /// still override any of them through `ini_overrides` as the final lever.
    #[must_use]
    pub fn opcache_ini_lines(&self, dev_mode: bool, multi_tenant: bool) -> Vec<(String, String)> {
        self.autotune(dev_mode, multi_tenant).ini_lines()
    }
}

/// OPcache JIT mode (`[php] opcache_jit` / `opcache.jit`).
///
/// Deliberately restricted to the three named modes — PHP's raw CRTO digit
/// syntax is not accepted (an operator who needs it can still set
/// `opcache.jit` through `ini_overrides`, which is applied after these
/// lines and therefore wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JitMode {
    /// Tracing JIT (`opcache.jit=tracing`) — traces hot loops/calls; the
    /// recommended and default-on mode for single-site serve.
    Tracing,
    /// Function JIT (`opcache.jit=function`) — compiles whole hot functions;
    /// usually slower than tracing, offered for A/B comparison.
    Function,
    /// JIT off (`opcache.jit=disable`).
    Disable,
}

impl JitMode {
    /// The literal `opcache.jit` ini value.
    #[must_use]
    pub fn as_ini(self) -> &'static str {
        match self {
            Self::Tracing => "tracing",
            Self::Function => "function",
            Self::Disable => "disable",
        }
    }

    /// Whether this mode actually compiles code (anything but `disable`).
    #[must_use]
    pub fn is_on(self) -> bool {
        !matches!(self, Self::Disable)
    }
}

/// Why the resolved JIT mode is what it is — feeds the dedicated startup log
/// line so the JIT state is never silent (see [`AutoTune::jit_line`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitReason {
    /// `[php] opcache_jit` was set explicitly — operator's choice, any mode.
    Explicit,
    /// Shaped default: multi-tenant serve (`sites_dir` set) → `disable`,
    /// because per-vhost `opcache_invalidate` never reclaims JIT buffer.
    MultiTenant,
    /// Shaped default: `[php] mode = "worker"` → `disable` (not positively
    /// verified against the persistent-worker lifecycle).
    WorkerMode,
    /// Shaped default on **every platform**: single-site serve → `disable`,
    /// because PHP's tracing JIT kills a long-lived multi-request process
    /// (issue #365).
    ///
    /// `zend_jit_escape_if_undef()` resolves the original VM handler through
    /// `ZEND_FUNC_INFO(exit_info->op_array)` at side-trace compile time. That
    /// `op_array` can be a **heap** op_array — a method of a linked class that
    /// the inheritance cache could not persist back into SHM, so it is copied
    /// to the heap during linking and never written back, yet is still
    /// JIT-instrumented. It is freed at request shutdown while the parent
    /// trace's `exit_info` lives on in shared memory, so compiling a side
    /// trace for that exit in a *later* request reads a dangling pointer:
    /// `0xC0000005` on Windows, `SIGSEGV` on Unix, no PHP error, process gone.
    /// Upstream: php-src PR <https://github.com/php/php-src/pull/21710>
    /// (open), regression from php-src PR 21368, first released in PHP 8.5.5.
    ///
    /// **Not Windows-only.** Windows is hit harder — a *stock* Laravel app
    /// dies there after three requests, because class linking misses the
    /// inheritance cache more often on that platform — but the defect is
    /// engine-level and reproduces on Linux just as hard once any class in the
    /// app links against a parent that is not in SHM. Measured on Linux
    /// x86_64 with the pinned SDKs, ePHPm serve + Laravel 13, default config:
    /// **SIGSEGV at request 2, 5/5 runs**, in the identical faulting frames
    /// (`zend_jit_trace_hot_side` → `zend_jit_compile_side_trace` →
    /// `zend_jit_trace` → `zend_jit_trace_deoptimization` →
    /// `zend_jit_escape_if_undef`). It reproduces the same way on stock
    /// `php -S` with no ePHPm involved, on both 8.4.23 and 8.5.7. Keeping a
    /// parent out of SHM needs nothing exotic — an `eval()`-defined parent
    /// (PHPUnit/Mockery mocks, proxy generators) and an OPcache-blacklisted
    /// parent file both do it, and a full or restarting OPcache has the same
    /// effect. PHP 8.3 predates the regression and is unaffected.
    ///
    /// `"function"` is the JIT mode that stays available: it never builds
    /// traces, so it cannot reach this path (verified clean where `"tracing"`
    /// dies at request 2). Revisit this default when php-src PR 21710 merges
    /// and the pinned SDK is bumped past it.
    TracingJitBug,
    /// Shaped default: dev mode → off (no line emitted; PHP defaults apply).
    Dev,
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
    /// Derived `opcache.jit_buffer_size` (MB). The JIT mode itself is
    /// resolved in [`PhpConfig::autotune`] (`opcache_jit` / shaped default).
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

/// Ceiling on the **derived** `opcache.memory_consumption` (MB) on Windows.
///
/// Lower than [`UNIX_OPCACHE_SHM_CEILING_MB`] because the two platforms charge
/// the OPcache shared segment very differently — see
/// [`opcache_shm_ceiling_mb`]. 256 MB still holds tens of thousands of cached
/// scripts, comfortably more than WordPress-plus-plugins or a large Laravel
/// app compiles, so the lower ceiling costs no realistic cache capacity.
pub const WINDOWS_OPCACHE_SHM_CEILING_MB: u32 = 256;

/// Ceiling on the **derived** `opcache.memory_consumption` (MB) on Unix.
pub const UNIX_OPCACHE_SHM_CEILING_MB: u32 = 512;

/// The derived-`opcache.memory_consumption` ceiling for the current target.
///
/// The two platforms differ because their shared-memory backends differ:
///
/// - **Unix** (`ext/opcache/shared_alloc_mmap.c`) maps an anonymous
///   `MAP_SHARED` region. Pages are committed lazily as the cache fills, so a
///   generous ceiling costs address space rather than memory.
/// - **Windows** (`ext/opcache/shared_alloc_win32.c`) creates a pagefile-backed
///   section with `CreateFileMapping(INVALID_HANDLE_VALUE, …)`. Windows charges
///   the **entire** segment against the system commit limit the moment it is
///   created, before a single script has been cached. A failure there is not a
///   degradation: PHP calls `zend_accel_error(ACCEL_LOG_FATAL, …)`, which
///   `exit(-2)`s the process from inside `php_module_startup`.
///
/// This ceiling bounds the **derived** value only. An explicit
/// `[php] opcache_memory_consumption` is always honoured as written; on Windows
/// an explicit value above the ceiling additionally warns (see
/// [`AutoTune::shm_warning`]).
#[must_use]
pub const fn opcache_shm_ceiling_mb() -> u32 {
    if cfg!(windows) { WINDOWS_OPCACHE_SHM_CEILING_MB } else { UNIX_OPCACHE_SHM_CEILING_MB }
}

/// Derive `opcache.jit_buffer_size` (MB) from the memory budget: ~1/64 of the
/// budget, clamped `[32, 64]` MB; the 32 MB floor also covers an undetectable
/// budget. Shared by the serve-mode derivation and the "explicit JIT in dev
/// mode" forcing path so both produce the same size.
#[must_use]
pub fn derive_jit_buffer_mb(mem_bytes: Option<u64>) -> u32 {
    let by_ratio = mem_bytes.map_or(32, |b| (b / 64) / MIB) as u32;
    by_ratio.clamp(32, 64)
}

/// Derive the resource-aware serve-mode tuning profile from the detected CPU
/// quota, memory budget, and effective worker count.
///
/// Returns an all-`None` [`DerivedTuning`] in **dev mode** (dev keeps
/// PHP-friendly defaults so edits refresh instantly and assertions stay on).
/// In serve mode:
///
/// - `opcache.memory_consumption` = ~18% of the memory budget, clamped
///   `[64, opcache_shm_ceiling_mb()]` MB — 512 on Unix, 256 on Windows (see
///   [`opcache_shm_ceiling_mb`]). (Always derived in serve, even with no memory
///   budget: the floor gives a sane 64 MB.)
/// - `opcache.interned_strings_buffer` = ~1 MB per 16 MB of opcache SHM,
///   clamped `[8, 64]` MB.
/// - `opcache.jit_buffer_size` = ~1/64 of the memory budget, clamped
///   `[32, 64]` MB (buffer sizing only — the JIT mode is resolved in
///   [`PhpConfig::autotune`]).
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
    // today (the JIT default is tenancy-shaped, not CPU-shaped, and
    // worker_count already consumes the quota). Bind
    // it so the signature documents the input and a future CPU-shaped knob has
    // it to hand.
    let _ = cpu_quota;

    if dev_mode {
        // Dev keeps PHP-friendly defaults across the board.
        return DerivedTuning::default();
    }

    // opcache.memory_consumption: ~18% of the budget, clamped to
    // [64, opcache_shm_ceiling_mb()] MB — 512 on Unix, 256 on Windows, where
    // the segment is pagefile-backed and fully commit-charged up front.
    // With no detectable budget, the floor (64 MB) still gives a sane serve
    // value — opcache SHM is fixed-size and cheap relative to a modern host.
    let opcache_mb: u32 = {
        let by_ratio = mem_bytes.map_or(64, |b| (b * 18 / 100) / MIB) as u32;
        by_ratio.clamp(64, opcache_shm_ceiling_mb())
    };

    // interned_strings_buffer: ~1 MB per 16 MB of opcache SHM, clamped [8, 64].
    let interned_mb: u32 = (opcache_mb / 16).clamp(8, 64);

    // jit_buffer_size: ~1/64 of the budget, clamped [32, 64] MB. Whether the
    // JIT actually uses it is governed by the `opcache.jit` resolution in
    // `PhpConfig::autotune` (shaped default / `[php] opcache_jit`).
    let jit_mb: u32 = derive_jit_buffer_mb(mem_bytes);

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
    /// Whether `[server] sites_dir` is set (multi-tenant vhosting) — shapes
    /// the `opcache.jit` default and its startup log line.
    pub multi_tenant: bool,
    /// Resolved `opcache.validate_timestamps`.
    pub validate_timestamps: TunedValue<bool>,
    /// Resolved `opcache.revalidate_freq` (only present when explicitly set).
    pub revalidate_freq: Option<TunedValue<u32>>,
    /// Resolved `opcache.memory_consumption` (MB).
    pub memory_consumption: TunedValue<u32>,
    /// Resolved `opcache.interned_strings_buffer` (MB).
    pub interned_strings_buffer: TunedValue<u32>,
    /// Resolved `opcache.jit` mode (see [`PhpConfig::opcache_jit`] for the
    /// shaped default). [`Origin::Default`] means no line is emitted (dev
    /// mode with the knob absent) and PHP's own defaults keep the JIT off.
    pub jit: TunedValue<JitMode>,
    /// Why [`Self::jit`] resolved the way it did — drives the dedicated
    /// startup log line ([`Self::jit_line`]).
    pub jit_reason: JitReason,
    /// Resolved `opcache.jit_buffer_size` (MB). Guaranteed non-zero-capable
    /// (derived) whenever [`Self::jit`] is on, unless the operator explicitly
    /// pinned `opcache_jit_buffer_size = 0` (see [`Self::jit_warning`]).
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
    /// mode-dependent, not a PHP stock value), and so is `memory_limit` (its
    /// bottom tier is the `[php] memory_limit` field, not a PHP stock value).
    /// Every other tunable is emitted only when its origin is
    /// [`Origin::Explicit`] or [`Origin::Derived`]; values left at the PHP
    /// stock default are omitted so the engine default applies and dev-mode
    /// php.ini stays minimal.
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

        // Windows only: give this process a PRIVATE OPcache shared-memory
        // namespace, so ePHPm always takes the segment-*create* path and never
        // the cross-process *reattach* path.
        //
        // PHP's Windows SHM backend names its section object
        //   ZendOPcache.SharedMemoryArea@<md5(user + opcache.cache_id)>
        //                               @<sapi_name>@<zend_system_id><size_hex>
        // (`shared_alloc_win32.c`, `create_name_with_username`) in the
        // per-session object namespace. Any second process computing the same
        // name therefore *reattaches* to the first one's segment instead of
        // creating its own. Reattach then demands that `execute_ex` sit at the
        // identical address in both images — cached op_arrays store absolute
        // opcode-handler pointers into the loaded image — and refuses when it
        // has moved:
        //
        //     execute_ex_moved = (void *)execute_ex != execute_ex_base;
        //
        // Two ePHPm images at different paths get different ASLR bases, so the
        // second one fails that check and PHP calls `zend_accel_error(
        // ACCEL_LOG_FATAL, "Opcode handlers are unusable due to ASLR…")`, which
        // `exit(-2)`s the process from inside `php_module_startup` — the server
        // dies at launch having served nothing (issue #362). Because the size is
        // part of the object name, this presented as a size lottery: whichever
        // `opcache.memory_consumption` collided with an already-running
        // instance was fatal while every other size worked.
        //
        // A per-process `cache_id` makes the name unique, so the collision
        // cannot occur. Nothing is lost: ePHPm is a single multi-threaded (ZTS)
        // process that never forks workers, so it had no use for a shared
        // segment, and this is exactly how it already behaves on Unix, where
        // cross-process reattachment does not exist at all
        // (`ZEND_OPCACHE_SHM_REATTACHMENT` is Windows-only).
        //
        // Emitted before `ini_overrides`, so an operator who genuinely wants a
        // shared segment can still pin their own `opcache.cache_id`.
        if cfg!(windows) {
            lines.push(("opcache.cache_id".to_string(), format!("ephpm-{}", process::id())));
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
        // `opcache.jit` — emitted whenever resolved (explicit or shaped).
        // Emitting the `disable` default explicitly is load-bearing: PHP
        // ≤8.3's stock `opcache.jit` is `tracing`, so once a jit_buffer_size
        // line is present (it always is in serve mode), omitting this line
        // would silently ENABLE the JIT on those versions.
        push_if_set("opcache.jit", self.jit.origin, self.jit.value.as_ini().to_string());
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
        // `memory_limit` is emitted unconditionally, unlike every other
        // tunable above. Its bottom resolution tier is the `[php] memory_limit`
        // field, which serde always populates (defaulting to `128M`) and which
        // an operator may have pinned — it is *not* a PHP stock value the
        // engine would apply on its own. Routing it through `push_if_set`
        // dropped the line whenever the origin came out `Origin::Default`,
        // which is exactly the case where the operator's own value lives
        // there: dev mode (no derivation at all) and serve mode on
        // macOS/Windows (no detectable memory budget). PHP then fell back to
        // its own compiled default and the configured value vanished.
        // Passing a non-`Default` origin is what forces emission; a bare
        // `lines.push` here cannot compile, because `push_if_set` holds a
        // mutable borrow of `lines` that is still live for the calls below.
        push_if_set("memory_limit", Origin::Explicit, self.memory_limit.value.clone());
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
        // `off (php default)` = no opcache.jit line emitted at all (dev mode,
        // knob absent) — PHP's own defaults keep the JIT off.
        let jit_state = if self.jit.origin == Origin::Default {
            "jit=off (php default)".to_string()
        } else {
            format!("jit={}{}", self.jit.value.as_ini(), self.jit.origin.marker())
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

    /// The dedicated startup INFO line stating the JIT state **and why** —
    /// the JIT default is shaped (mode-dependent), so it must never be
    /// silent. Companion to [`Self::summary_line`], same pattern as the
    /// timestamp-validation contract lines in the CLI.
    #[must_use]
    pub fn jit_line(&self) -> String {
        match self.jit_reason {
            JitReason::Explicit => format!(
                "opcache JIT {} ([php] opcache_jit set explicitly)",
                if self.jit.value.is_on() {
                    format!("ON ({})", self.jit.value.as_ini())
                } else {
                    "OFF".to_string()
                }
            ),
            JitReason::MultiTenant => "opcache JIT OFF — multi-tenant default (sites_dir set): \
                 per-vhost OPcache invalidation never reclaims JIT buffer, so deploy churn \
                 would silently exhaust it; set [php] opcache_jit = \"tracing\" to override"
                .to_string(),
            JitReason::WorkerMode => "opcache JIT OFF — worker-mode default ([php] mode = \
                 \"worker\"); set [php] opcache_jit = \"tracing\" to opt in"
                .to_string(),
            JitReason::TracingJitBug => "opcache JIT OFF — default since #365: PHP's tracing \
                 JIT (8.5.5+, and 8.4.24+) reads a freed op_array when it compiles a side \
                 trace in a later request and kills the process — no PHP error, no log line \
                 (php-src PR 21710, still open; reproduces on stock php too, on Linux and \
                 Windows alike). Set [php] opcache_jit = \"function\" for the unaffected JIT \
                 mode, or \"tracing\" to accept the risk"
                .to_string(),
            JitReason::Dev => {
                "opcache JIT OFF (dev mode default; PHP's own defaults apply)".to_string()
            }
        }
    }

    /// A startup WARN for JIT configurations that work but carry a documented
    /// operational hazard. `None` when there is nothing to warn about.
    ///
    /// - JIT explicitly enabled in multi-tenant mode: per-vhost
    ///   `opcache_invalidate` never reclaims JIT buffer (measured), so every
    ///   deploy leaks buffer until the JIT silently stops compiling; only a
    ///   full `opcache_reset` (disabled by the hardening preset) reclaims it.
    /// - JIT enabled with an explicitly pinned `opcache_jit_buffer_size = 0`:
    ///   the JIT will never compile anything.
    #[must_use]
    pub fn jit_warning(&self) -> Option<String> {
        if !self.jit.value.is_on() || self.jit.origin != Origin::Explicit {
            return None;
        }
        if self.jit_buffer_size.value == 0 {
            return Some(
                "[php] opcache_jit is on but opcache_jit_buffer_size = 0 — the JIT has no \
                 buffer and will never compile anything"
                    .to_string(),
            );
        }
        if self.jit.value == JitMode::Tracing {
            return Some(
                "[php] opcache_jit = \"tracing\": PHP's tracing JIT (8.4.24+ / 8.5.5+) \
                 dereferences a freed op_array when it compiles a side trace in a later \
                 request, killing the process with no PHP error and nothing in the ePHPm \
                 log. A stock Laravel app dies after 3 requests on Windows; on Linux it \
                 takes 2 requests once any class links against a parent that is not in \
                 OPcache SHM (an eval()-defined or non-cached parent — mock/proxy \
                 generators and a full OPcache both do it). Upstream php-src PR 21710 is \
                 open. Use opcache_jit = \"function\" (never builds traces, unaffected) \
                 unless you have measured that your workload survives"
                    .to_string(),
            );
        }
        if self.multi_tenant {
            return Some(format!(
                "[php] opcache_jit = \"{}\" with sites_dir set: per-vhost OPcache \
                 invalidation does NOT reclaim JIT buffer, so each deploy permanently \
                 consumes some of the {}MB jit_buffer_size until the JIT silently stops \
                 compiling new code; only a restart (or full opcache_reset) reclaims it. \
                 Watch ephpm_opcache_jit_buffer_free_bytes — note the multi-tenant \
                 hardening preset removes the OPcache status API the gauge samples \
                 unless [opcache] cluster_invalidation keeps it open",
                self.jit.value.as_ini(),
                self.jit_buffer_size.value
            ));
        }
        None
    }

    /// A startup WARN when an operator has explicitly pinned an
    /// `opcache.memory_consumption` above the platform ceiling on Windows.
    ///
    /// The explicit value is still honoured verbatim — [`opcache_shm_ceiling_mb`]
    /// bounds only the *derived* value. This warns because the Windows failure
    /// mode is unusually harsh: the segment is pagefile-backed and charged
    /// against the system commit limit in full at startup, and if PHP cannot
    /// create it the engine calls `zend_accel_error(ACCEL_LOG_FATAL, …)`, which
    /// `exit(-2)`s the process from inside `php_module_startup`. There is no
    /// return value to inspect and no way to fall back to a smaller size in
    /// process, so the only useful thing ePHPm can do is say so in advance.
    ///
    /// Returns `None` on Unix, where the mapping is anonymous and lazily
    /// committed and a large explicit value is unremarkable.
    #[must_use]
    pub fn shm_warning(&self) -> Option<String> {
        if !cfg!(windows) || self.memory_consumption.origin != Origin::Explicit {
            return None;
        }
        let ceiling = opcache_shm_ceiling_mb();
        if self.memory_consumption.value <= ceiling {
            return None;
        }
        Some(format!(
            "[php] opcache_memory_consumption = {} exceeds the {}MB ePHPm derives on Windows. \
             The Windows OPcache segment is pagefile-backed and its full size is charged against \
             the system commit limit at startup; if that reservation fails, PHP aborts the \
             process (exit -2) from module startup rather than starting without OPcache. Lower \
             it if ePHPm fails to start.",
            self.memory_consumption.value, ceiling
        ))
    }
}

fn default_sqlite_path() -> String {
    "ephpm.db".to_string()
}

fn default_sqlite_engine() -> String {
    "turso".to_string()
}

fn default_sqlite_max_open_dbs() -> usize {
    256
}

fn default_sqlite_mysql_listen() -> String {
    "127.0.0.1:3306".to_string()
}

fn default_mysql_wire_enabled() -> bool {
    true
}

fn default_replication_role() -> String {
    "auto".to_string()
}

/// Clustering configuration (`[cluster]`).
///
/// Enables gossip-based peer discovery using the SWIM protocol.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
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
    ///
    /// When empty, clustering refuses to start unless
    /// [`allow_insecure_no_auth`](Self::allow_insecure_no_auth) is set to
    /// `true`. This is a fail-closed guard: an empty secret means the
    /// gossip transport and the KV TCP data plane run in unauthenticated
    /// plaintext, letting any host on the cluster network forge KV writes
    /// (sessions, rate-limit counters, ACME/OPcache keys).
    #[serde(default)]
    pub secret: String,

    /// Explicitly permit running the cluster without an authenticating
    /// [`secret`](Self::secret).
    ///
    /// Default `false`. When `false` and clustering is enabled with an
    /// empty `secret`, startup fails closed with an actionable error. Set
    /// this to `true` only on a fully trusted private network (VPC,
    /// WireGuard, Tailscale) where every host with access to the gossip
    /// and KV data-plane ports is trusted; a loud warning is still logged.
    /// Not recommended.
    #[serde(default)]
    pub allow_insecure_no_auth: bool,

    /// Unique node identifier. Auto-generated if empty.
    #[serde(default)]
    pub node_id: String,

    /// Cluster identifier. Nodes with different cluster IDs ignore each other.
    #[serde(default = "default_cluster_id")]
    pub cluster_id: String,

    /// KV clustering settings.
    #[serde(default)]
    pub kv: ClusterKvConfig,

    /// Cluster channel settings — the multiplexed, authenticated
    /// data-plane listener used by opt-in cluster features (Turso CDC
    /// replication today; snapshot bootstrap and watermark sync in
    /// future phases).
    ///
    /// The channel listener is **only bound when at least one feature
    /// asks for it** — a config that ships no channel-using feature
    /// produces zero new sockets, zero task spawns, and zero startup
    /// log lines above `debug!`. This is the "opt-in transport"
    /// contract; adding this block to your config is not itself an
    /// opt-in.
    #[serde(default)]
    pub channel: ClusterChannelConfig,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_cluster_bind(),
            join: Vec::new(),
            secret: String::new(),
            allow_insecure_no_auth: false,
            node_id: String::new(),
            cluster_id: default_cluster_id(),
            kv: ClusterKvConfig::default(),
            channel: ClusterChannelConfig::default(),
        }
    }
}

/// Cluster channel configuration (`[cluster.channel]`).
///
/// The cluster channel is a single, authenticated, `yamux`-multiplexed
/// TCP listener that opt-in cluster features share (Turso CDC
/// replication today; snapshot bootstrap and watermark sync reserved).
///
/// **Lazy bind contract:** the listener is only started when at least
/// one registered channel feature is enabled on this node. A config
/// that ships no such feature produces zero new sockets and zero
/// startup log noise above `debug!`. Adding this block to your config
/// is not itself an opt-in — a feature elsewhere must ask for it.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct ClusterChannelConfig {
    /// Listen address for the cluster channel TCP listener.
    ///
    /// When unset (the default) the address is derived at runtime as
    /// `<cluster.bind IP>:<cluster.bind port + 2>` — `+ 2` and not
    /// `+ 1` because the KV data plane already claims `gossip + 1`
    /// (7947 with default ports). With defaults the channel lands on
    /// 7948, so operators who exposed the gossip port only have to
    /// remember one small port range. Set this explicitly to override.
    ///
    /// Ignored (parsed but not acted upon) when no channel feature is
    /// enabled — see the [`ClusterChannelConfig`] type docs.
    #[serde(default)]
    pub listen: Option<String>,

    /// Shared secret for the cluster channel handshake and stream
    /// framing.
    ///
    /// When unset (the default), the channel falls back to
    /// `[cluster] secret` (the same secret used to authenticate gossip
    /// and the KV data plane). A distinct HKDF domain
    /// (`ephpm-cluster-channel-v1`) is used to derive the channel key
    /// so ciphertexts are never valid across planes.
    ///
    /// The channel refuses to bind when no secret is available (fail
    /// closed): setting `[cluster] secret` — or explicitly setting
    /// this field — is a prerequisite for any channel feature.
    #[serde(default)]
    pub secret: Option<String>,
}

impl ClusterConfig {
    /// Whether clustering may start given the current security settings.
    ///
    /// Returns an error (fail closed) when clustering is enabled with an
    /// empty [`secret`](Self::secret) and
    /// [`allow_insecure_no_auth`](Self::allow_insecure_no_auth) is
    /// `false`. An empty secret means the gossip transport and the KV TCP
    /// data plane run unauthenticated, so any host on the cluster network
    /// can forge KV writes. The error message tells the operator how to
    /// fix it.
    ///
    /// This is a no-op (returns `Ok(())`) when clustering is disabled, so
    /// single-node deployments are never affected.
    ///
    /// # Errors
    ///
    /// Returns an error if `enabled` is `true`, `secret` is empty, and
    /// `allow_insecure_no_auth` is `false`.
    pub fn ensure_secure(&self) -> Result<(), String> {
        if self.enabled && self.secret.is_empty() && !self.allow_insecure_no_auth {
            return Err(
                "[cluster] enabled = true but no [cluster] secret is set: gossip and the KV \
                 data plane would run as unauthenticated plaintext, letting any host on the \
                 cluster network forge KV writes (sessions, rate limits, ACME/OPcache keys). \
                 Set [cluster] secret (e.g. `openssl rand -base64 32`), or explicitly set \
                 [cluster] allow_insecure_no_auth = true to run clustering without \
                 authentication -- NOT recommended."
                    .to_string(),
            );
        }
        Ok(())
    }
}

/// KV clustering configuration (`[cluster.kv]`).
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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

fn default_shed_after_ms() -> u64 {
    // No grace window. The admission queue a shed decision looks at is already
    // a buffer (`worker_backlog` on the pool engine, `workers` slots on the
    // spawn_blocking one), so "full" already means saturated — waiting on top
    // of it is the backpressure-into-client-timeout behaviour issue #301 is
    // about. Operators who want a burst absorber set this explicitly.
    0
}

fn default_php_mode() -> String {
    "fpm".to_string()
}

fn default_fpm_engine() -> FpmEngine {
    FpmEngine::SpawnBlocking
}

fn default_crash_containment() -> bool {
    // OFF. Recovering from a SIGSEGV is only defensible for one narrow fault
    // class, and it is not free: the recovered thread's Zend context is
    // abandoned (a bounded leak) and PHP module shutdown must be skipped at
    // process exit. An operator opts into that trade knowingly, per deployment
    // — it is never the default.
    false
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
    /// A **Windows job-object** memory limit (`JOB_OBJECT_LIMIT_JOB_MEMORY` /
    /// `JOB_OBJECT_LIMIT_PROCESS_MEMORY`) — the closest Windows analogue of a
    /// cgroup limit. Only reported when it is strictly below physical RAM.
    JobObject,
    /// No container limit — total physical system memory is used instead
    /// (`/proc/meminfo` `MemTotal` on Linux, `GlobalMemoryStatusEx`'s
    /// `ullTotalPhys` on Windows).
    SystemTotal,
    /// Neither a container limit nor a readable system total — nothing to
    /// derive from, so memory-shaped tunables keep their PHP-stock defaults.
    /// This is the only outcome on platforms with no probe (macOS today).
    Unknown,
}

impl MemorySource {
    /// A short label suitable for a `tracing` field / the autotune summary.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::CgroupV2 => "cgroup v2",
            Self::CgroupV1 => "cgroup v1",
            Self::JobObject => "job-object",
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
/// Resolution order (Windows):
/// 1. The calling process's **job object** memory limit
///    (`QueryInformationJobObject` / `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`) —
///    used only when it is strictly below physical RAM.
/// 2. Physical RAM (`GlobalMemoryStatusEx`'s `ullTotalPhys`).
///
/// A cgroup limit of `"max"` (v2) or an absurdly-large sentinel (v1) means "no
/// limit set" and is skipped so we fall through to the system total. On
/// platforms with no probe at all (macOS today) this returns
/// `(None, MemorySource::Unknown)` and callers keep PHP defaults.
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
    // Off Windows `read_job_object_memory_limit()` is a `None` stub, so this
    // reduces to exactly the previous "system total, else unknown" behaviour.
    select_memory_budget(read_job_object_memory_limit(), read_total_system_memory())
}

/// Pick between a job-object (container-ish) limit and physical RAM.
///
/// A job limit only wins when it is a **real restriction** — strictly below
/// physical RAM, or physical RAM is unknown. A job limit at or above physical
/// RAM caps nothing that the hardware doesn't already cap, so reporting it
/// would overstate the budget; physical RAM is the honest figure there.
///
/// Split out as a pure function so it is unit-testable on every platform
/// without a real job object.
fn select_memory_budget(
    job_limit: Option<u64>,
    physical: Option<u64>,
) -> (Option<u64>, MemorySource) {
    match (job_limit, physical) {
        (Some(job), Some(phys)) if job < phys => (Some(job), MemorySource::JobObject),
        (Some(job), None) => (Some(job), MemorySource::JobObject),
        (_, Some(phys)) => (Some(phys), MemorySource::SystemTotal),
        (None, None) => (None, MemorySource::Unknown),
    }
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
/// is reported in kibibytes). Returns `None` if the field is missing or
/// unparseable — callers then keep PHP-stock defaults.
#[cfg(target_os = "linux")]
fn read_total_system_memory() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo_memtotal(&meminfo)
}

/// Read total physical RAM in bytes via `GlobalMemoryStatusEx` (`ullTotalPhys`).
///
/// This is the Windows counterpart of `/proc/meminfo`'s `MemTotal`: the amount
/// of physical memory the OS reports for this machine (or, inside a Windows
/// container, whatever the silo reports to it). Returns `None` if the call
/// fails or reports zero, so the caller falls back to `MemorySource::Unknown`
/// and PHP-stock defaults rather than inventing a number.
#[cfg(windows)]
#[allow(unsafe_code)] // One Win32 call; every unsafe block carries a SAFETY note.
fn read_total_system_memory() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    // SAFETY: `MEMORYSTATUSEX` is a plain-old-data struct of integers with no
    // padding invariants, pointers, or niches, so an all-zero bit pattern is a
    // valid value. The only field the API reads on input (`dwLength`) is
    // assigned immediately below, before the struct is handed to the kernel.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = u32::try_from(std::mem::size_of::<MEMORYSTATUSEX>()).ok()?;

    // SAFETY: `status` is a live, correctly-aligned `MEMORYSTATUSEX` owned by
    // this frame, and `dwLength` tells the kernel exactly how many bytes it may
    // write, so the write stays inside the buffer. `GlobalMemoryStatusEx` has
    // no other preconditions and cannot fail in a way that leaves `status`
    // partially-initialized in an invalid state (it is POD either way).
    let ok = unsafe { GlobalMemoryStatusEx(&raw mut status) };
    if ok == 0 {
        return None;
    }
    if status.ullTotalPhys == 0 { None } else { Some(status.ullTotalPhys) }
}

/// Platforms with no implemented probe (macOS today): report nothing rather
/// than guess. We keep the dependency footprint minimal (no `sysinfo` crate)
/// rather than pull a platform abstraction, so memory-shaped tunables keep PHP
/// defaults there.
#[cfg(not(any(target_os = "linux", windows)))]
fn read_total_system_memory() -> Option<u64> {
    None
}

/// `JOB_OBJECT_LIMIT_PROCESS_MEMORY` — a per-process committed-memory cap is
/// in force, so `ProcessMemoryLimit` is meaningful.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
const JOB_OBJECT_LIMIT_PROCESS_MEMORY_FLAG: u32 = 0x0000_0100;
/// `JOB_OBJECT_LIMIT_JOB_MEMORY` — a job-wide committed-memory cap is in force,
/// so `JobMemoryLimit` is meaningful.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
const JOB_OBJECT_LIMIT_JOB_MEMORY_FLAG: u32 = 0x0000_0200;

// The two constants above are re-declared (rather than imported) so the pure
// selection logic below compiles — and is unit-tested — on every platform.
// Pin them to the real Win32 values at compile time on Windows so they can
// never drift from the header.
#[cfg(windows)]
const _: () = {
    use windows_sys::Win32::System::JobObjects::{
        JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };
    assert!(JOB_OBJECT_LIMIT_PROCESS_MEMORY_FLAG == JOB_OBJECT_LIMIT_PROCESS_MEMORY);
    assert!(JOB_OBJECT_LIMIT_JOB_MEMORY_FLAG == JOB_OBJECT_LIMIT_JOB_MEMORY);
};

/// Read the memory limit of the job object the current process belongs to.
///
/// Windows has no cgroups; the equivalent restriction is a **job object**, which
/// is what Windows containers and job-based sandboxes use. A `NULL` job handle
/// asks about the job the *calling process* is in; when the process is not in a
/// job (the normal desktop/service case) the call fails and we return `None`.
///
/// Only the limits whose `LimitFlags` bit is set are meaningful — an unset
/// limit leaves its field at zero, which is emphatically *not* "0 bytes".
#[cfg(windows)]
#[allow(unsafe_code)] // One Win32 call; every unsafe block carries a SAFETY note.
fn read_job_object_memory_limit() -> Option<u64> {
    use windows_sys::Win32::System::JobObjects::{
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        QueryInformationJobObject,
    };

    // SAFETY: `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` is a plain-old-data struct
    // of integers and nested POD structs; an all-zero bit pattern is valid. It
    // is a pure out-parameter — the kernel overwrites it on success and we
    // ignore its contents on failure.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    let size = u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).ok()?;

    // SAFETY: a NULL `hJob` is documented as "the job associated with the
    // calling process"; if there is none the call simply returns FALSE (it does
    // not write through the pointer). The buffer is a live, correctly-aligned
    // `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` owned by this frame and `size` is
    // its exact size, so the kernel's write stays in bounds. The optional
    // return-length out-parameter is allowed to be NULL.
    let ok = unsafe {
        QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            (&raw mut info).cast(),
            size,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }
    select_job_memory_limit(
        info.BasicLimitInformation.LimitFlags,
        u64::try_from(info.ProcessMemoryLimit).unwrap_or(u64::MAX),
        u64::try_from(info.JobMemoryLimit).unwrap_or(u64::MAX),
    )
}

/// Non-Windows: no job objects. The cgroup path covers Linux; everything else
/// falls through to the system total.
#[cfg(not(windows))]
fn read_job_object_memory_limit() -> Option<u64> {
    None
}

/// Pick the effective job-object memory cap from the queried limit block.
///
/// Both `JOB_OBJECT_LIMIT_PROCESS_MEMORY` (per process) and
/// `JOB_OBJECT_LIMIT_JOB_MEMORY` (whole job) can be set; the smaller of the
/// enabled ones is the ceiling this process actually lives under. Fields whose
/// flag is clear are ignored, and a zero value is treated as "not set" rather
/// than a literal zero-byte cap.
///
/// Compiled everywhere so the unit tests exercise it without a real job object.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn select_job_memory_limit(
    limit_flags: u32,
    process_memory_limit: u64,
    job_memory_limit: u64,
) -> Option<u64> {
    let mut limit: Option<u64> = None;
    let mut consider = |bytes: u64| {
        if bytes > 0 {
            limit = Some(limit.map_or(bytes, |cur: u64| cur.min(bytes)));
        }
    };
    if limit_flags & JOB_OBJECT_LIMIT_PROCESS_MEMORY_FLAG != 0 {
        consider(process_memory_limit);
    }
    if limit_flags & JOB_OBJECT_LIMIT_JOB_MEMORY_FLAG != 0 {
        consider(job_memory_limit);
    }
    limit
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

/// Whether `overrides` is lexically inside `sites`.
///
/// Used to fail startup closed when `[server] site_overrides_dir` points inside
/// `[server] sites_dir`: an override file inside a site container sits inside
/// that tenant's own `open_basedir`, so the tenant's PHP can rewrite it and the
/// "operator-owned" property — the entire basis for trusting these files — is
/// gone.
///
/// Deliberately **lexical**: neither directory is required to exist at config
/// load time, so `canonicalize` is not available. It catches the realistic
/// mistake (`sites_dir = "/var/www/sites"`, `site_overrides_dir =
/// "/var/www/sites/_overrides"`), not a determined operator using symlinks to
/// defeat their own guard rail.
fn overrides_dir_is_inside_sites_dir(overrides: &Path, sites: &Path) -> bool {
    let normalize = |p: &Path| -> PathBuf {
        // Drop `.` segments so `./sites` and `sites` compare equal; keep
        // everything else verbatim.
        p.components().filter(|c| !matches!(c, Component::CurDir)).collect()
    };
    normalize(overrides).starts_with(normalize(sites))
}

/// Default WebSocket entrypoint names — one, mirroring `index.php`'s role on
/// the HTTP path.
fn default_websocket_files() -> Vec<String> {
    vec!["websocket.php".to_string()]
}

/// Global WebSocket connection ceiling. Chosen to be large enough that no
/// ordinary deployment meets it, but finite: the whole point of terminating
/// sockets in Rust is cheap idle connections, and "cheap" is not "free".
fn default_ws_max_connections() -> usize {
    10_000
}

/// Per-vhost WebSocket ceiling — a tenth of the global default, so a
/// multi-tenant node needs ten busy tenants before the global cap is the
/// binding constraint.
fn default_ws_max_connections_per_site() -> usize {
    1_000
}

fn default_ws_max_message_size() -> usize {
    1024 * 1024 // 1 MiB
}

fn default_ws_max_frame_size() -> usize {
    1024 * 1024 // 1 MiB
}

/// Outbound queue depth per connection. Deep enough to absorb a burst from one
/// broadcast, shallow enough that a stalled reader is detected in frames rather
/// than megabytes.
fn default_ws_send_queue() -> usize {
    64
}

fn default_ws_ping_interval_secs() -> u64 {
    30
}

fn default_ws_idle_timeout_secs() -> u64 {
    120
}

fn default_fallback() -> Vec<String> {
    vec!["$uri".to_string(), "$uri/".to_string(), "/index.php?$query_string".to_string()]
}

fn default_max_body_size() -> u64 {
    10 * 1024 * 1024 // 10 MiB
}

/// Request-body buffering for middleware is opt-in: `0` disables it, preserving
/// the reject-before-body-transfer property. Operators that need `request_body`
/// (webhook/HMAC, CSRF-with-body) set an explicit byte cap.
fn default_middleware_body_limit() -> u64 {
    0
}

/// Default `Alt-Svc` max-age: 24 hours, the value nginx and Caddy advertise.
fn default_alt_svc_max_age() -> u64 {
    86400
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

fn default_proxy_path() -> String {
    "/".to_string()
}

fn default_proxy_connect_timeout() -> u64 {
    5
}

fn default_proxy_read_timeout() -> u64 {
    60
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

/// Default ACME challenge type — TLS-ALPN-01, the zero-config path that
/// predates the DNS-01 lane. Chosen so an existing `[server.tls]` with
/// `domains` behaves exactly as before this knob existed.
fn default_tls_challenge() -> String {
    "tls-alpn-01".to_string()
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

/// Every accepted `[kv] eviction_policy` value.
///
/// Must stay in sync with `ephpm_kv::store::EvictionPolicy::from_str_lossy`,
/// which is the parser the server actually calls. That parser is lossy by
/// design (it has no error channel), so [`Config::validate`] is what turns a
/// typo into a startup failure instead of a silent switch to `allkeys-lru`.
const KV_EVICTION_POLICIES: [&str; 4] =
    ["noeviction", "allkeys-lru", "volatile-lru", "allkeys-random"];

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
    // Every test here reads the `EPHPM_*` process environment through
    // `Config::load` / `Config::default_config`. Tests that need an override
    // must install it with `EnvVars` — see `crate::test_env`; nothing else in
    // this module may touch `std::env` directly.
    use crate::test_env::EnvVars;

    // ── [[middleware]] library = "php:<path>" ────────────────────────

    /// Load a config from TOML and run `validate`, returning the message.
    fn validation_error(toml: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, toml).unwrap();
        match Config::load(&file).unwrap().validate() {
            Ok(()) => panic!("expected a validation error for:\n{toml}"),
            Err(e) => e.to_string(),
        }
    }

    /// A single `php:` mount. Uses a TOML *literal* string so a backslash in
    /// `library` reaches validation verbatim instead of being an escape.
    fn php_mount_toml(library: &str) -> String {
        format!("[[middleware]]\nlibrary = '{library}'\norder = 10\n")
    }

    #[test]
    fn php_mount_is_recognised_and_yields_its_script_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, php_mount_toml("php:auth/middleware.php")).unwrap();

        let config = Config::load(&file).unwrap();
        config.validate().expect("a plain relative path is valid");
        assert_eq!(config.middleware[0].php_script(), Some("auth/middleware.php"));
        // Non-`php:` mounts are untouched by the new lane.
        assert_eq!(
            MiddlewareMount { library: "jwt".into(), match_pattern: None, order: 0, config: None }
                .php_script(),
            None
        );
    }

    // ── [server.tls] DNS-01 challenge validation ────────────────────────

    /// Load a config from TOML and validate, expecting success. Returns it.
    fn load_ok(toml: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, toml).unwrap();
        let config = Config::load(&file).unwrap();
        config.validate().unwrap_or_else(|e| panic!("expected valid config, got: {e}\n{toml}"));
        config
    }

    #[test]
    fn tls_challenge_defaults_to_tls_alpn_01() {
        let config = load_ok("[server.tls]\ndomains = [\"example.com\"]\n");
        let tls = config.server.tls.expect("tls section present");
        assert_eq!(tls.challenge, "tls-alpn-01");
        assert!(tls.is_acme());
        assert!(!tls.is_dns01(), "the default must not select the dns-01 lane");
    }

    #[test]
    fn dns01_without_token_is_rejected() {
        let err = validation_error(
            "[server.tls]\n\
             domains = [\"*.preview.example.com\"]\n\
             challenge = \"dns-01\"\n\
             dns_provider = \"cloudflare\"\n",
        );
        assert!(err.contains("requires an API token"), "unexpected: {err}");
    }

    #[test]
    fn dns01_without_provider_is_rejected() {
        let err =
            validation_error("[server.tls]\ndomains = [\"example.com\"]\nchallenge = \"dns-01\"\n");
        assert!(err.contains("requires `dns_provider`"), "unexpected: {err}");
    }

    #[test]
    fn dns01_unknown_provider_is_rejected() {
        let err = validation_error(
            "[server.tls]\n\
             domains = [\"example.com\"]\n\
             challenge = \"dns-01\"\n\
             dns_provider = \"gandi\"\n\
             cloudflare_api_token = \"x\"\n",
        );
        assert!(err.contains("not supported"), "unexpected: {err}");
    }

    #[test]
    fn dns01_new_providers_accepted_with_credentials() {
        // Each non-Cloudflare provider validates when its credential(s) are set.
        let base = "[server.tls]\ndomains = [\"*.p.example.com\"]\nchallenge = \"dns-01\"\n";
        for (provider, creds) in [
            ("linode", "linode_api_token = \"tok\"\n"),
            ("digitalocean", "digitalocean_api_token = \"tok\"\n"),
            ("route53", "route53_access_key_id = \"AKIA\"\nroute53_secret_access_key = \"s\"\n"),
            ("google", "google_service_account_json = \"{}\"\ngoogle_project = \"proj\"\n"),
        ] {
            let toml = format!("{base}dns_provider = \"{provider}\"\n{creds}");
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("ephpm.toml");
            std::fs::write(&file, &toml).unwrap();
            Config::load(&file).unwrap().validate().unwrap_or_else(|e| {
                panic!("provider {provider} should validate with its credentials, got: {e}")
            });
        }
    }

    #[test]
    fn dns01_new_providers_require_credentials() {
        let base = "[server.tls]\ndomains = [\"*.p.example.com\"]\nchallenge = \"dns-01\"\n";
        for provider in ["linode", "digitalocean", "route53", "google"] {
            let err = validation_error(&format!("{base}dns_provider = \"{provider}\"\n"));
            assert!(
                err.contains(provider) && err.contains("requires"),
                "provider {provider} with no creds should be rejected: {err}"
            );
        }
    }

    #[test]
    fn unknown_challenge_is_rejected() {
        let err = validation_error(
            "[server.tls]\ndomains = [\"example.com\"]\nchallenge = \"http-01\"\n",
        );
        assert!(err.contains("challenge must be"), "unexpected: {err}");
    }

    #[test]
    fn wildcard_under_tls_alpn_01_is_rejected() {
        let err = validation_error("[server.tls]\ndomains = [\"*.preview.example.com\"]\n");
        assert!(err.contains("requires challenge = \"dns-01\""), "unexpected: {err}");
    }

    #[test]
    fn dns01_wildcard_with_inline_token_validates() {
        let config = load_ok(
            "[server.tls]\n\
             domains = [\"*.preview.example.com\", \"preview.example.com\"]\n\
             challenge = \"dns-01\"\n\
             dns_provider = \"cloudflare\"\n\
             cloudflare_api_token = \"tok\"\n",
        );
        let tls = config.server.tls.expect("tls present");
        assert!(tls.is_dns01());
        assert!(tls.has_wildcard_domain());
    }

    #[test]
    fn dns01_with_token_file_validates() {
        // The file need not exist at validate() time — its readability is a
        // runtime (fail-closed at startup) concern; presence of the path is
        // what validate() checks.
        let config = load_ok(
            "[server.tls]\n\
             domains = [\"example.com\"]\n\
             challenge = \"dns-01\"\n\
             dns_provider = \"cloudflare\"\n\
             cloudflare_api_token_file = \"/run/secrets/cf-token\"\n",
        );
        assert!(config.server.tls.expect("tls present").is_dns01());
    }

    #[test]
    fn dns01_token_via_env_satisfies_validation() {
        // The env var lands in `cloudflare_api_token`, so no file/inline token
        // is needed in the TOML.
        let _env = EnvVars::set("EPHPM_SERVER__TLS__CLOUDFLARE_API_TOKEN", "env-token");
        let config = load_ok(
            "[server.tls]\n\
             domains = [\"example.com\"]\n\
             challenge = \"dns-01\"\n\
             dns_provider = \"cloudflare\"\n",
        );
        let tls = config.server.tls.expect("tls present");
        assert_eq!(tls.cloudflare_api_token.as_deref(), Some("env-token"));
        assert!(tls.is_dns01());
    }

    /// The whole tenant-isolation story rests on the path being joined onto the
    /// REQUEST's document root. Every shape that could escape that join is
    /// refused at startup rather than re-checked per request.
    #[test]
    fn php_mount_rejects_paths_that_escape_the_document_root() {
        for (library, expect) in [
            ("php:", "must be followed by a script path"),
            ("php:/etc/ephpm/mw.php", "not absolute"),
            ("php:\\etc\\mw.php", "not absolute"),
            ("php:C:/mw.php", "not a drive path"),
            ("php:../../etc/mw.php", "must not contain `..`"),
            ("php:app/../../mw.php", "must not contain `..`"),
            ("php:app\\mw.php", "must use `/` as its separator"),
        ] {
            let err = validation_error(&php_mount_toml(library));
            assert!(
                err.contains(expect),
                "\"{library}\" should be rejected with {expect:?}: {err}"
            );
            assert!(err.contains(library), "the error must name the mount: {err}");
        }
    }

    /// A dot-segment that merely *starts* with `..` is a legal filename and
    /// must not be caught by the traversal check.
    #[test]
    fn php_mount_allows_filenames_that_start_with_dots() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, php_mount_toml("php:..hidden/..mw.php")).unwrap();
        Config::load(&file).unwrap().validate().expect("`..hidden` is a filename, not a traversal");
    }

    /// Worker mode owns the request loop, so there is no per-request
    /// `php_request_startup` to prepend into. Refuse rather than mount a policy
    /// layer that silently never runs.
    #[test]
    fn php_mount_is_rejected_in_worker_mode() {
        let dir = tempfile::tempdir().unwrap();
        let docroot = dir.path().join("public");
        std::fs::create_dir_all(&docroot).unwrap();
        std::fs::write(docroot.join("worker.php"), "<?php\n").unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            format!(
                "[server]\ndocument_root = {docroot:?}\n\
                 [php]\nmode = \"worker\"\nworker_script = \"worker.php\"\n\
                 {}",
                php_mount_toml("php:middleware.php"),
            ),
        )
        .unwrap();

        let err = match Config::load(&file).unwrap().validate() {
            Ok(()) => panic!("a php: mount under worker mode must be rejected"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("worker mode"), "{err}");
        assert!(err.contains("PSR-15"), "the error should point at the alternative: {err}");
    }

    /// The Rust-side cap must match the C wrapper's `MAX_REQUEST_MIDDLEWARE`,
    /// so a mount can never be silently dropped at request time.
    #[test]
    fn php_mounts_beyond_the_per_request_cap_are_rejected() {
        let mut toml = String::new();
        for i in 0..=MAX_PHP_MIDDLEWARE {
            use std::fmt::Write as _;
            let _ = writeln!(toml, "[[middleware]]\nlibrary = 'php:mw{i}.php'\norder = {i}");
        }
        let err = validation_error(&toml);
        assert!(err.contains(&MAX_PHP_MIDDLEWARE.to_string()), "{err}");
    }

    // ── [server.http3] ───────────────────────────────────────────────

    /// Section absent: HTTP/3 must be off, with the documented Alt-Svc
    /// max-age. `ServerConfig` has a hand-written `Default`, so this also
    /// guards against that impl and the serde field defaults drifting apart.
    #[test]
    fn http3_section_absent_defaults_to_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server]\nlisten = \"0.0.0.0:8080\"\n").unwrap();

        let config = Config::load(&file).unwrap();
        assert!(!config.server.http3.enabled, "HTTP/3 must be opt-in");
        assert_eq!(config.server.http3.listen, None);
        assert_eq!(config.server.http3.alt_svc_max_age, 86400);

        // ...and the struct default agrees with what TOML parsing produced.
        let direct = ServerConfig::default();
        assert!(!direct.http3.enabled);
        assert_eq!(direct.http3.alt_svc_max_age, 86400);
    }

    /// Section present but partial: the unset fields must fall back to their
    /// field-level defaults, not to zero/None-by-derive.
    #[test]
    fn http3_section_present_keeps_field_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server.http3]\nenabled = true\n").unwrap();

        let config = Config::load(&file).unwrap();
        assert!(config.server.http3.enabled);
        assert_eq!(config.server.http3.listen, None, "listen derives from the HTTPS address");
        assert_eq!(
            config.server.http3.alt_svc_max_age, 86400,
            "a partial [server.http3] section must not zero the Alt-Svc max-age"
        );
    }

    #[test]
    fn http3_section_parses_every_field() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[server.http3]
enabled = true
listen = "0.0.0.0:8443"
alt_svc_max_age = 3600
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        assert!(config.server.http3.enabled);
        assert_eq!(config.server.http3.listen.as_deref(), Some("0.0.0.0:8443"));
        assert_eq!(config.server.http3.alt_svc_max_age, 3600);
    }

    // ── [[server.proxy]] ─────────────────────────────────────────────

    /// No `[[server.proxy]]` section: the rule list is empty (feature off).
    #[test]
    fn proxy_absent_defaults_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server]\nlisten = \"0.0.0.0:8080\"\n").unwrap();

        let config = Config::load(&file).unwrap();
        assert!(config.server.proxy.is_empty(), "no proxy section means no rules");
        assert!(ServerConfig::default().proxy.is_empty(), "the hand-written Default agrees");
    }

    /// A full rule parses every field; the per-rule field defaults hold for a
    /// partial rule.
    #[test]
    fn proxy_rule_parses_fields_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[[server.proxy]]
host = "pr-a.example.com"
path = "/api"
path_exact = true
upstream = "http://127.0.0.1:9084"
connect_timeout_secs = 3
read_timeout_secs = 45

[[server.proxy]]
upstream = "http://127.0.0.1:9085"
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        config.validate().expect("valid proxy rules must pass validate()");
        assert_eq!(config.server.proxy.len(), 2, "array-of-tables order preserved");

        let first = &config.server.proxy[0];
        assert_eq!(first.host.as_deref(), Some("pr-a.example.com"));
        assert_eq!(first.path, "/api");
        assert!(first.path_exact);
        assert_eq!(first.connect_timeout_secs, 3);
        assert_eq!(first.read_timeout_secs, 45);

        // Partial rule: host/path/timeouts fall back to their field defaults.
        let second = &config.server.proxy[1];
        assert_eq!(second.host, None, "omitted host matches any");
        assert_eq!(second.path, "/", "default path is the catch-all prefix");
        assert!(!second.path_exact);
        assert_eq!(second.connect_timeout_secs, 5);
        assert_eq!(second.read_timeout_secs, 60);
    }

    #[test]
    fn proxy_https_upstream_is_a_v1_startup_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[[server.proxy]]\nupstream = \"https://backend.example.com\"\n")
            .unwrap();

        let config = Config::load(&file).unwrap();
        let err = config.validate().expect_err("https upstream must be rejected in v1");
        assert!(err.to_string().contains("https"), "{err}");
    }

    #[test]
    fn proxy_upstream_with_path_is_a_v1_startup_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[[server.proxy]]\nupstream = \"http://127.0.0.1:9000/api\"\n")
            .unwrap();

        let config = Config::load(&file).unwrap();
        let err = config.validate().expect_err("a path in the upstream must be rejected in v1");
        assert!(err.to_string().contains("rewriting"), "{err}");
    }

    #[test]
    fn proxy_bad_host_wildcard_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            "[[server.proxy]]\nhost = \"a.*.example.com\"\nupstream = \"http://127.0.0.1:9000\"\n",
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        let err = config.validate().expect_err("a non-leftmost wildcard must be rejected");
        assert!(err.to_string().contains("wildcard"), "{err}");
    }

    #[test]
    fn proxy_relative_path_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            "[[server.proxy]]\npath = \"api\"\nupstream = \"http://127.0.0.1:9000\"\n",
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        let err = config.validate().expect_err("a path not starting with / must be rejected");
        assert!(err.to_string().contains("must begin with '/'"), "{err}");
    }

    #[test]
    fn proxy_upstream_authority_forms() {
        let ok = |u: &str| {
            ProxyRuleConfig {
                host: None,
                path: "/".to_string(),
                path_exact: false,
                upstream: u.to_string(),
                connect_timeout_secs: 5,
                read_timeout_secs: 60,
            }
            .upstream_authority()
        };

        assert_eq!(ok("http://127.0.0.1:9000").unwrap(), "127.0.0.1:9000");
        assert_eq!(ok("http://backend").unwrap(), "backend");
        assert_eq!(ok("http://[::1]:9000").unwrap(), "[::1]:9000");
        assert!(ok("ftp://x").is_err(), "non-http scheme");
        assert!(ok("http://host:70000").is_err(), "port out of range");
        assert!(ok("http://host:").is_err(), "empty port");
        assert!(ok("http://").is_err(), "no host");
    }

    #[test]
    fn test_env_var_overrides_http3_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server.http3]\nenabled = false\n").unwrap();

        let _env = EnvVars::set("EPHPM_SERVER__HTTP3__ENABLED", "true");
        let config = Config::load(&file).unwrap();
        assert!(config.server.http3.enabled);
    }

    #[test]
    fn test_env_var_overrides_http3_alt_svc_max_age() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server]\nlisten = \"0.0.0.0:8080\"\n").unwrap();

        let _env = EnvVars::set("EPHPM_SERVER__HTTP3__ALT_SVC_MAX_AGE", "120");
        let config = Config::load(&file).unwrap();
        assert_eq!(config.server.http3.alt_svc_max_age, 120);
    }

    // ── [server.websocket] + [server] websocket_files ────────────────

    /// Section absent: the feature is off and every bound resolves to its
    /// documented default. Also guards against the `[server.security]` lesson —
    /// a missing section must not zero the carefully chosen field defaults.
    #[test]
    fn websocket_section_absent_is_off_with_documented_bounds() {
        let config = Config::default_config().expect("default config should load");
        let ws = &config.server.websocket;
        assert!(!ws.enabled, "native websockets must be opt-in");
        assert_eq!(ws.max_connections, 10_000);
        assert_eq!(ws.max_connections_per_site, 1_000);
        assert_eq!(ws.max_message_size, 1024 * 1024);
        assert_eq!(ws.max_frame_size, 1024 * 1024);
        assert_eq!(ws.send_queue, 64);
        assert_eq!(ws.ping_interval_secs, 30);
        assert_eq!(ws.idle_timeout_secs, 120);
        // The hand-written Default must not drift from the serde defaults.
        let hand_written = WebSocketConfig::default();
        assert_eq!(hand_written.max_connections, ws.max_connections);
        assert_eq!(hand_written.send_queue, ws.send_queue);
        assert_eq!(hand_written.idle_timeout_secs, ws.idle_timeout_secs);
    }

    /// `websocket_files` mirrors `index_files`: absent means one documented
    /// name, and setting it replaces the list wholesale.
    #[test]
    fn websocket_files_defaults_to_one_entrypoint() {
        let config = Config::default_config().expect("default config should load");
        assert_eq!(config.server.websocket_files, vec!["websocket.php"]);
    }

    #[test]
    fn websocket_files_is_configurable_as_an_ordered_list() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server]\nwebsocket_files = [\"ws.php\", \"public/ws.php\"]\n")
            .unwrap();

        let config = Config::load(&file).unwrap();
        assert_eq!(config.server.websocket_files, vec!["ws.php", "public/ws.php"]);
    }

    /// Section present but partial: the one named field takes effect and every
    /// other bound keeps its default rather than collapsing to zero.
    #[test]
    fn websocket_section_partial_keeps_other_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server.websocket]\nenabled = true\n").unwrap();

        let config = Config::load(&file).unwrap();
        assert!(config.server.websocket.enabled);
        assert_eq!(config.server.websocket.send_queue, 64);
        assert_eq!(config.server.websocket.max_connections, 10_000);
        assert_eq!(config.server.websocket.idle_timeout_secs, 120);
    }

    #[test]
    fn websocket_bounds_are_configurable() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r"
[server.websocket]
enabled = true
max_connections = 50
max_connections_per_site = 5
max_message_size = 4096
max_frame_size = 2048
send_queue = 8
ping_interval_secs = 5
idle_timeout_secs = 15
",
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        let ws = &config.server.websocket;
        assert!(ws.enabled);
        assert_eq!(ws.max_connections, 50);
        assert_eq!(ws.max_connections_per_site, 5);
        assert_eq!(ws.max_message_size, 4096);
        assert_eq!(ws.max_frame_size, 2048);
        assert_eq!(ws.send_queue, 8);
        assert_eq!(ws.ping_interval_secs, 5);
        assert_eq!(ws.idle_timeout_secs, 15);
    }

    /// Worker mode never runs the entrypoint, so the combination must fail at
    /// startup rather than serving upgrades that silently do nothing.
    #[test]
    fn websockets_with_worker_mode_is_a_startup_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            "[server.websocket]\nenabled = true\n\n[php]\nmode = \"worker\"\nworker_script = \"w.php\"\n",
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        let err = config.validate().expect_err("worker + websocket must not validate");
        let msg = err.to_string();
        assert!(msg.contains("websocket"), "unhelpful error: {msg}");
        assert!(msg.contains("worker"), "unhelpful error: {msg}");
    }

    /// An enabled feature with no entrypoint name would 404 every upgrade —
    /// a silently disabled feature, which is exactly what the no-silent-no-op
    /// rule forbids.
    #[test]
    fn websockets_enabled_with_no_entrypoint_names_is_a_startup_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            "[server]\nwebsocket_files = []\n\n[server.websocket]\nenabled = true\n",
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        let err = config.validate().expect_err("empty websocket_files must not validate");
        assert!(err.to_string().contains("websocket_files"), "unhelpful error: {err}");
    }

    /// The feature being off must not make an empty list an error, and must
    /// not make worker mode an error.
    #[test]
    fn websocket_validation_is_inert_when_the_feature_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        let script = dir.path().join("w.php");
        std::fs::write(&script, "<?php\n").unwrap();
        std::fs::write(
            &file,
            format!(
                "[server]\ndocument_root = {:?}\nwebsocket_files = []\n\n[php]\nmode = \"worker\"\nworker_script = \"w.php\"\n",
                dir.path().to_string_lossy(),
            ),
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        assert!(!config.server.websocket.enabled);
        config.validate().expect("websocket checks must not fire when the feature is off");
    }

    #[test]
    fn test_env_var_overrides_websocket_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server.websocket]\nenabled = false\n").unwrap();

        let _env = EnvVars::set("EPHPM_SERVER__WEBSOCKET__ENABLED", "true");
        let config = Config::load(&file).unwrap();
        assert!(config.server.websocket.enabled);
    }

    #[test]
    fn test_env_var_overrides_websocket_send_queue() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server]\nlisten = \"0.0.0.0:8080\"\n").unwrap();

        let _env = EnvVars::set("EPHPM_SERVER__WEBSOCKET__SEND_QUEUE", "4");
        let config = Config::load(&file).unwrap();
        assert_eq!(config.server.websocket.send_queue, 4);
    }

    // ── [server.diagnostics] ─────────────────────────────────────────

    /// Section absent: both knobs must be unset, and the mode-dependent
    /// resolution must give dev "on" and serve "off". Also guards the
    /// hand-written `ServerConfig::default()` against drifting from the
    /// serde defaults.
    #[test]
    fn diagnostics_section_absent_defaults_unset() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server]\nlisten = \"0.0.0.0:8080\"\n").unwrap();

        let config = Config::load(&file).unwrap();
        assert_eq!(config.server.diagnostics.request_log, None);
        assert_eq!(config.server.diagnostics.otlp_endpoint, None);
        assert_eq!(config.server.diagnostics.otlp_protocol, None);
        assert!(
            config.server.diagnostics.effective_request_log(true),
            "unset request_log must resolve ON in dev mode"
        );
        assert!(
            !config.server.diagnostics.effective_request_log(false),
            "unset request_log must resolve OFF in serve mode"
        );

        // ...and the struct default agrees with what TOML parsing produced.
        let direct = ServerConfig::default();
        assert_eq!(direct.diagnostics.request_log, None);
        assert_eq!(direct.diagnostics.otlp_endpoint, None);
        assert_eq!(direct.diagnostics.otlp_protocol, None);
    }

    /// An explicit value wins over the mode default in both directions.
    #[test]
    fn diagnostics_explicit_request_log_beats_mode_default() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server.diagnostics]\nrequest_log = true\n").unwrap();

        let config = Config::load(&file).unwrap();
        assert_eq!(config.server.diagnostics.request_log, Some(true));
        assert!(config.server.diagnostics.effective_request_log(false));

        std::fs::write(&file, "[server.diagnostics]\nrequest_log = false\n").unwrap();
        let config = Config::load(&file).unwrap();
        assert!(!config.server.diagnostics.effective_request_log(true));
    }

    #[test]
    fn diagnostics_section_parses_every_field() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            "[server.diagnostics]\nrequest_log = true\notlp_endpoint = \"http://127.0.0.1:4318\"\n\
             otlp_protocol = \"grpc\"\n",
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        assert_eq!(config.server.diagnostics.request_log, Some(true));
        assert_eq!(
            config.server.diagnostics.otlp_endpoint.as_deref(),
            Some("http://127.0.0.1:4318")
        );
        assert_eq!(config.server.diagnostics.otlp_protocol.as_deref(), Some("grpc"));
    }

    #[test]
    fn test_env_var_overrides_diagnostics_otlp_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server.diagnostics]\notlp_protocol = \"http/protobuf\"\n").unwrap();

        let _env = EnvVars::set("EPHPM_SERVER__DIAGNOSTICS__OTLP_PROTOCOL", "grpc");
        let config = Config::load(&file).unwrap();
        assert_eq!(config.server.diagnostics.otlp_protocol.as_deref(), Some("grpc"));
    }

    #[test]
    fn test_env_var_overrides_diagnostics_request_log() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server.diagnostics]\nrequest_log = false\n").unwrap();

        let _env = EnvVars::set("EPHPM_SERVER__DIAGNOSTICS__REQUEST_LOG", "true");
        let config = Config::load(&file).unwrap();
        assert_eq!(config.server.diagnostics.request_log, Some(true));
    }

    #[test]
    fn test_env_var_overrides_diagnostics_otlp_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server]\nlisten = \"0.0.0.0:8080\"\n").unwrap();

        let _env = EnvVars::set("EPHPM_SERVER__DIAGNOSTICS__OTLP_ENDPOINT", "http://otel:4318");
        let config = Config::load(&file).unwrap();
        assert_eq!(config.server.diagnostics.otlp_endpoint.as_deref(), Some("http://otel:4318"));
    }

    #[test]
    fn cluster_disabled_empty_secret_is_ok() {
        // Single-node (clustering off) is never affected by the gate.
        let cfg = ClusterConfig::default();
        assert!(!cfg.enabled);
        assert!(cfg.secret.is_empty());
        assert!(cfg.ensure_secure().is_ok());
    }

    #[test]
    fn cluster_enabled_empty_secret_fails_closed() {
        let cfg = ClusterConfig { enabled: true, ..ClusterConfig::default() };
        let err = cfg.ensure_secure().expect_err("empty secret + enabled must fail closed");
        assert!(err.contains("secret"), "error should mention the secret: {err}");
        assert!(err.contains("allow_insecure_no_auth"), "error should point at the opt-in: {err}");
    }

    #[test]
    fn cluster_enabled_with_secret_is_ok() {
        let cfg = ClusterConfig {
            enabled: true,
            secret: "s3cret-value".to_string(),
            ..ClusterConfig::default()
        };
        assert!(cfg.ensure_secure().is_ok());
    }

    #[test]
    fn cluster_enabled_empty_secret_opt_in_bypasses_gate() {
        let cfg = ClusterConfig {
            enabled: true,
            allow_insecure_no_auth: true,
            ..ClusterConfig::default()
        };
        assert!(
            cfg.ensure_secure().is_ok(),
            "explicit allow_insecure_no_auth must bypass the gate"
        );
    }

    #[test]
    fn test_default_config() {
        let config = Config::default_config().expect("default config should load");
        assert_eq!(config.server.listen, "0.0.0.0:8080");
        assert_eq!(config.php.max_execution_time, 30);
        assert_eq!(config.php.memory_limit, "128M");
        assert_eq!(config.server.index_files, vec!["index.php", "index.html"]);
    }

    // ── [php] fpm_engine ─────────────────────────────────────────────────

    /// The struct default is the safe, unchanged engine.
    #[test]
    fn fpm_engine_defaults_to_spawn_blocking() {
        assert_eq!(PhpConfig::default().fpm_engine, FpmEngine::SpawnBlocking);
        assert!(!PhpConfig::default().is_pool_engine());
        let config = Config::default_config().expect("default config should load");
        assert_eq!(config.php.fpm_engine, FpmEngine::SpawnBlocking);
    }

    /// `[php]` section present but `fpm_engine` absent must resolve to the
    /// default, not be zeroed by a section-level derive.
    #[test]
    fn fpm_engine_section_present_absent_field_is_spawn_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[php]\nmax_execution_time = 60\n").unwrap();

        let config = Config::load(&file).unwrap();
        assert_eq!(
            config.php.fpm_engine,
            FpmEngine::SpawnBlocking,
            "a partial [php] section must not flip the engine"
        );
        assert!(!config.php.is_pool_engine());
    }

    /// A whole config with no `[php]` section at all still defaults the engine.
    #[test]
    fn fpm_engine_section_absent_is_spawn_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server]\nlisten = \"0.0.0.0:8080\"\n").unwrap();

        let config = Config::load(&file).unwrap();
        assert_eq!(config.php.fpm_engine, FpmEngine::SpawnBlocking);
    }

    /// Explicit `pool` parses and is applicable in fpm mode.
    #[test]
    fn fpm_engine_pool_parses_and_is_applicable_in_fpm_mode() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[php]\nfpm_engine = \"pool\"\n").unwrap();

        let config = Config::load(&file).unwrap();
        assert_eq!(config.php.fpm_engine, FpmEngine::Pool);
        assert!(config.php.is_pool_engine(), "pool engine applies in fpm mode");
    }

    /// `fpm_engine` is inert in worker mode: `is_pool_engine()` is false even
    /// when `pool` is requested, so the server never builds the fpm pool there.
    #[test]
    fn fpm_engine_pool_is_inert_in_worker_mode() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[php]\nmode = \"worker\"\nfpm_engine = \"pool\"\n").unwrap();

        let config = Config::load(&file).unwrap();
        assert_eq!(config.php.fpm_engine, FpmEngine::Pool);
        assert!(!config.php.is_pool_engine(), "fpm_engine is ignored in worker mode");
    }

    /// An unrecognised value is a hard startup error, never a silent fallback.
    #[test]
    fn fpm_engine_invalid_value_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[php]\nfpm_engine = \"threads\"\n").unwrap();

        assert!(Config::load(&file).is_err(), "an unknown fpm_engine must fail to load");
    }

    /// The lab flips the engine via env with no code change:
    /// `EPHPM_PHP__FPM_ENGINE=pool` must parse.
    #[test]
    fn fpm_engine_env_override_parses() {
        let _env = EnvVars::set("EPHPM_PHP__FPM_ENGINE", "pool");
        let config = Config::default_config().unwrap();
        assert_eq!(config.php.fpm_engine, FpmEngine::Pool);
        assert!(config.php.is_pool_engine());
    }

    // ── [php] overload_policy / shed_after_ms (issue #301) ───────────────

    /// Shedding is OFF unless asked for — from the struct default, from an
    /// empty file, and from a `[php]` section that exists but omits the field
    /// (the serde section-default trap). Behaviour must be byte-identical to
    /// releases before the knob existed.
    #[test]
    fn overload_policy_defaults_to_wait() {
        assert_eq!(PhpConfig::default().overload_policy, None);
        assert_eq!(PhpConfig::default().shed_after_ms, 0);

        let dir = tempfile::tempdir().unwrap();

        let empty = dir.path().join("empty.toml");
        std::fs::write(&empty, "").unwrap();
        let config = Config::load(&empty).unwrap();
        assert_eq!(config.effective_overload_policy(), OverloadPolicy::Wait);
        assert!(!config.overload_policy_from_preview_preset());

        // `[php]` present, field absent.
        let partial = dir.path().join("partial.toml");
        std::fs::write(&partial, "[php]\nmemory_limit = \"128M\"\n").unwrap();
        assert_eq!(
            Config::load(&partial).unwrap().effective_overload_policy(),
            OverloadPolicy::Wait
        );

        // `[server]` present without `preview` must not flip it either.
        let server_only = dir.path().join("server.toml");
        std::fs::write(&server_only, "[server]\nlisten = \"0.0.0.0:8080\"\n").unwrap();
        assert_eq!(
            Config::load(&server_only).unwrap().effective_overload_policy(),
            OverloadPolicy::Wait
        );
    }

    /// Explicit values parse, and an unknown one is a hard startup error rather
    /// than a silent fallback to `wait` (which would look like working config
    /// while shedding nothing).
    #[test]
    fn overload_policy_parses_and_rejects_unknown_values() {
        let dir = tempfile::tempdir().unwrap();

        let shed = dir.path().join("shed.toml");
        std::fs::write(&shed, "[php]\noverload_policy = \"shed\"\nshed_after_ms = 250\n").unwrap();
        let config = Config::load(&shed).unwrap();
        assert_eq!(config.php.overload_policy, Some(OverloadPolicy::Shed));
        assert_eq!(config.php.shed_after_ms, 250);
        assert_eq!(config.effective_overload_policy(), OverloadPolicy::Shed);

        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "[php]\noverload_policy = \"drop\"\n").unwrap();
        assert!(Config::load(&bad).is_err(), "an unknown overload_policy must fail to load");
    }

    /// `[server] preview = true` supplies `shed` for an unset policy — a
    /// preview box should answer a saturating client rather than absorb it —
    /// and an explicit value still wins, including an explicit `"wait"` (the
    /// documented way to opt out of the preset).
    #[test]
    fn preview_preset_supplies_shed_but_explicit_wins() {
        let dir = tempfile::tempdir().unwrap();

        let preview = dir.path().join("preview.toml");
        std::fs::write(&preview, "[server]\npreview = true\n").unwrap();
        let config = Config::load(&preview).unwrap();
        assert_eq!(config.effective_overload_policy(), OverloadPolicy::Shed);
        assert!(config.overload_policy_from_preview_preset(), "startup must name the preset");

        let opted_out = dir.path().join("opted-out.toml");
        std::fs::write(
            &opted_out,
            "[server]\npreview = true\n\n[php]\noverload_policy = \"wait\"\n",
        )
        .unwrap();
        let config = Config::load(&opted_out).unwrap();
        assert_eq!(
            config.effective_overload_policy(),
            OverloadPolicy::Wait,
            "an explicit value must beat the preview preset"
        );
        assert!(!config.overload_policy_from_preview_preset());
    }

    /// Both knobs are reachable from the environment, which is how the overload
    /// lab and container deployments set them.
    #[test]
    fn overload_policy_env_override_parses() {
        let _policy = EnvVars::set("EPHPM_PHP__OVERLOAD_POLICY", "shed");
        let _after = EnvVars::set("EPHPM_PHP__SHED_AFTER_MS", "150");
        let config = Config::default_config().unwrap();
        assert_eq!(config.php.overload_policy, Some(OverloadPolicy::Shed));
        assert_eq!(config.php.shed_after_ms, 150);
        assert_eq!(config.effective_overload_policy(), OverloadPolicy::Shed);
    }

    // ── [php] crash_containment ──────────────────────────────────────────

    /// Containment is OFF unless explicitly requested — from the struct
    /// default, from an empty file, and from a `[php]` section that exists but
    /// omits the field (the serde section-default trap).
    #[test]
    fn crash_containment_defaults_to_off() {
        assert!(!PhpConfig::default().crash_containment);
        assert!(!PhpConfig::default().is_crash_containment_active());

        let dir = tempfile::tempdir().unwrap();

        let empty = dir.path().join("empty.toml");
        std::fs::write(&empty, "").unwrap();
        assert!(!Config::load(&empty).unwrap().php.crash_containment);

        // `[php]` present, field absent — must still be off.
        let partial = dir.path().join("partial.toml");
        std::fs::write(&partial, "[php]\nmemory_limit = \"128M\"\n").unwrap();
        let config = Config::load(&partial).unwrap();
        assert!(
            !config.php.crash_containment,
            "a present [php] section without the field must not enable containment"
        );
        assert!(!config.php.is_crash_containment_active());
    }

    /// Requested WITH the pool engine → active. This is the only combination
    /// that arms the guard.
    #[test]
    fn crash_containment_active_with_pool_engine() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[php]\nfpm_engine = \"pool\"\ncrash_containment = true\n").unwrap();

        let config = Config::load(&file).unwrap();
        assert!(config.php.crash_containment);
        assert!(
            config.php.is_crash_containment_active(),
            "containment applies with the pool engine in fpm mode"
        );
    }

    /// Requested WITHOUT the pool engine → parsed but inert. Containment needs
    /// a thread ePHPm can retire; tokio's shared blocking pool cannot provide
    /// one. Startup warns (see `crates/ephpm/src/main.rs`).
    #[test]
    fn crash_containment_is_inert_without_pool_engine() {
        let dir = tempfile::tempdir().unwrap();

        // Default (spawn_blocking) engine.
        let sb = dir.path().join("sb.toml");
        std::fs::write(&sb, "[php]\ncrash_containment = true\n").unwrap();
        let config = Config::load(&sb).unwrap();
        assert!(config.php.crash_containment, "the field still parses");
        assert!(
            !config.php.is_crash_containment_active(),
            "containment must not arm on the spawn_blocking engine"
        );

        // Worker mode makes `fpm_engine` inert, so containment is inert too.
        let worker = dir.path().join("worker.toml");
        std::fs::write(
            &worker,
            "[php]\nmode = \"worker\"\nfpm_engine = \"pool\"\ncrash_containment = true\n",
        )
        .unwrap();
        let config = Config::load(&worker).unwrap();
        assert!(
            !config.php.is_crash_containment_active(),
            "containment must not arm in worker mode"
        );
    }

    /// The e2e harness and the lab flip it via env with no config edit.
    #[test]
    fn crash_containment_env_override_parses() {
        let _engine = EnvVars::set("EPHPM_PHP__FPM_ENGINE", "pool");
        let _env = EnvVars::set("EPHPM_PHP__CRASH_CONTAINMENT", "true");
        let config = Config::default_config().unwrap();
        assert!(config.php.crash_containment);
        assert!(config.php.is_crash_containment_active());
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

        let _env = EnvVars::set("EPHPM_SERVER__LISTEN", "127.0.0.1:9090");
        let config = Config::load(&file).unwrap();
        assert_eq!(config.server.listen, "127.0.0.1:9090");
    }

    #[test]
    fn test_env_var_override_without_file() {
        let _env = EnvVars::set("EPHPM_PHP__MEMORY_LIMIT", "256M");
        let config = Config::default_config().unwrap();
        assert_eq!(config.php.memory_limit, "256M");
    }

    // ── [db.sqlite] strict-key parsing ──────────────────────────────────
    //
    // The `[db.sqlite]` structs are `deny_unknown_fields` because their knobs
    // select a *mode*. The defect these pin: `per_site = true` on a binary that
    // predated the knob parsed fine, was dropped on the floor, and the node
    // came up in whole-database clustered mode — all tenants sharing one
    // database — with every health check green.

    /// Load a config from TOML, expecting the load to succeed. Deliberately
    /// does **not** call `validate()`: these tests are about which *keys*
    /// deserialize, not about whether the resulting combination is a coherent
    /// deployment.
    fn load_toml(toml: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, toml).unwrap();
        Config::load(&file).unwrap_or_else(|e| panic!("expected a valid config, got: {e}\n{toml}"))
    }

    /// Load a config from TOML expecting the **load** (deserialize) step to
    /// fail, returning the error text.
    fn load_error(toml: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, toml).unwrap();
        match Config::load(&file) {
            Ok(_) => panic!("expected a load error for:\n{toml}"),
            Err(e) => e.to_string(),
        }
    }

    /// A misspelled mode knob must name the offending key and refuse to start,
    /// never parse-and-ignore.
    #[test]
    fn unknown_key_under_sqlite_replication_is_rejected() {
        let err = load_error(
            "[db.sqlite]\n\
             dir = \"/var/lib/ephpm/sites\"\n\
             \n\
             [db.sqlite.replication]\n\
             role = \"auto\"\n\
             per_sites = true\n",
        );
        assert!(
            err.contains("per_sites"),
            "the error must name the unknown key so an operator can find the typo, got: {err}"
        );
    }

    /// The same strictness one level up, and on the proxy block.
    #[test]
    fn unknown_key_under_sqlite_and_proxy_is_rejected() {
        let err = load_error("[db.sqlite]\nmax_open_db = 32\n");
        assert!(err.contains("max_open_db"), "unexpected: {err}");

        let err = load_error("[db.sqlite.proxy]\nmysql_listens = \"127.0.0.1:3306\"\n");
        assert!(err.contains("mysql_listens"), "unexpected: {err}");
    }

    /// Strictness must not cost the documented upgrade path: every knob that is
    /// declared — including the v0.7.0 removals kept purely so old configs keep
    /// parsing — still loads.
    #[test]
    fn known_and_deprecated_sqlite_keys_still_parse() {
        let config = load_toml(
            "[db.sqlite]\n\
             dir = \"/var/lib/ephpm/sites\"\n\
             engine = \"turso\"\n\
             max_open_dbs = 32\n\
             \n\
             [db.sqlite.sqld]\n\
             write_permits = 4\n\
             \n\
             [db.sqlite.replication]\n\
             role = \"auto\"\n\
             per_site = true\n\
             cdc_experimental = true\n\
             max_snapshot_bytes = 2048\n",
        );
        let sqlite = config.db.sqlite.expect("sqlite section present");
        assert!(sqlite.replication.per_site);
        assert_eq!(sqlite.replication.max_snapshot_bytes, 2048);
        // Removed-but-declared knobs parse (and warn at startup) rather than
        // hard-failing an upgrading config with "unknown field".
        assert!(sqlite.replication.cdc_experimental);
        assert_eq!(sqlite.sqld.and_then(|s| s.write_permits), Some(4));
    }

    /// `deny_unknown_fields` must not break the `EPHPM_` env-var override lane:
    /// figment's `Env::prefixed("EPHPM_").split("__")` feeds the same structs,
    /// so a strict struct that rejected its own env keys would be a regression.
    #[test]
    fn env_var_override_still_reaches_strict_sqlite_replication() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[db.sqlite]\ndir = \"/var/lib/ephpm/sites\"\n").unwrap();

        let _env = EnvVars::set("EPHPM_DB__SQLITE__REPLICATION__PER_SITE", "true");
        let config = Config::load(&file).unwrap();
        let sqlite = config.db.sqlite.expect("sqlite section present");
        assert!(
            sqlite.replication.per_site,
            "an EPHPM_ env override must still reach a deny_unknown_fields struct"
        );
    }

    // ── strict-key parsing, every section ───────────────────────────────
    //
    // Extends the `[db.sqlite]` strictness above to the rest of the config.
    // Same defect, same reasoning: a key this binary does not know is far more
    // likely to be a typo — or a knob from a newer version — than something
    // safe to drop on the floor, and dropping it silently turns an operator's
    // explicit instruction into a no-op that every health check reports green.
    //
    // Two structs are deliberately NOT strict; see `config_root_stays_lenient`
    // and `deprecated_sqld_block_tolerates_its_own_removed_keys` for why.

    /// Every section that is `deny_unknown_fields`, with a minimal valid block
    /// plus one unknown key. Each case must fail the load *and* name the key.
    ///
    /// Table-driven so adding a section to the config means adding one line
    /// here, rather than quietly shipping a section that still swallows typos.
    #[test]
    fn unknown_keys_are_rejected_in_every_strict_section() {
        // (description, toml, the key the error must name)
        let cases: &[(&str, &str, &str)] = &[
            ("[server]", "[server]\nlistens = \"0.0.0.0:8080\"\n", "listens"),
            ("[server.request]", "[server.request]\nmax_body_sizes = 1024\n", "max_body_sizes"),
            ("[server.timeouts]", "[server.timeouts]\nrequests = 30\n", "requests"),
            ("[server.response]", "[server.response]\nbogus_key = 1\n", "bogus_key"),
            ("[server.static]", "[server.static]\nbogus_key = 1\n", "bogus_key"),
            ("[server.php_etag_cache]", "[server.php_etag_cache]\nbogus_key = 1\n", "bogus_key"),
            ("[server.security]", "[server.security]\nbogus_key = 1\n", "bogus_key"),
            ("[server.logging]", "[server.logging]\nbogus_key = 1\n", "bogus_key"),
            ("[server.metrics]", "[server.metrics]\nbogus_key = 1\n", "bogus_key"),
            ("[server.diagnostics]", "[server.diagnostics]\nbogus_key = 1\n", "bogus_key"),
            ("[server.limits]", "[server.limits]\nbogus_key = 1\n", "bogus_key"),
            ("[server.file_cache]", "[server.file_cache]\nbogus_key = 1\n", "bogus_key"),
            ("[server.tls]", "[server.tls]\nbogus_key = 1\n", "bogus_key"),
            ("[server.http3]", "[server.http3]\nbogus_key = 1\n", "bogus_key"),
            ("[server.websocket]", "[server.websocket]\nbogus_key = 1\n", "bogus_key"),
            ("[server.tenant_network]", "[server.tenant_network]\nbogus_key = 1\n", "bogus_key"),
            (
                "[[server.proxy]]",
                "[[server.proxy]]\nupstream = \"http://127.0.0.1:9000\"\nbogus_key = 1\n",
                "bogus_key",
            ),
            ("[db]", "[db]\nbogus_key = 1\n", "bogus_key"),
            (
                "[db.mysql]",
                "[db.mysql]\nurl = \"mysql://u:p@127.0.0.1:3306/d\"\nbogus_key = 1\n",
                "bogus_key",
            ),
            (
                "[db.postgres]",
                "[db.postgres]\nurl = \"postgres://u:p@127.0.0.1:5432/d\"\nbogus_key = 1\n",
                "bogus_key",
            ),
            (
                "[db.mysql.replicas]",
                "[db.mysql]\nurl = \"mysql://u:p@127.0.0.1:3306/d\"\n\
                 \n[db.mysql.replicas]\nurls = []\nbogus_key = 1\n",
                "bogus_key",
            ),
            ("[db.read_write_split]", "[db.read_write_split]\nbogus_key = 1\n", "bogus_key"),
            ("[db.analysis]", "[db.analysis]\nbogus_key = 1\n", "bogus_key"),
            ("[kv]", "[kv]\nbogus_key = 1\n", "bogus_key"),
            ("[kv.redis_compat]", "[kv.redis_compat]\nbogus_key = 1\n", "bogus_key"),
            ("[php]", "[php]\nmemory_limits = \"256M\"\n", "memory_limits"),
            ("[cluster]", "[cluster]\nenabled_typo = true\n", "enabled_typo"),
            ("[cluster.channel]", "[cluster.channel]\nbogus_key = 1\n", "bogus_key"),
            ("[cluster.kv]", "[cluster.kv]\nbogus_key = 1\n", "bogus_key"),
            ("[opcache]", "[opcache]\nbogus_key = 1\n", "bogus_key"),
            (
                "[[middleware]]",
                "[[middleware]]\nlibrary = \"jwt\"\norder = 10\nbogus_key = 1\n",
                "bogus_key",
            ),
        ];

        for (section, toml, key) in cases {
            let err = load_error(toml);
            assert!(
                err.contains(key),
                "{section}: the error must name the unknown key {key:?} so an operator can \
                 find the typo, got: {err}"
            );
        }
    }

    /// Strictness must not cost the `EPHPM_` env-var override lane. Figment's
    /// `Env::prefixed("EPHPM_").split("__")` feeds the *same* structs as the
    /// TOML provider, so a strict struct that rejected its own env keys would
    /// break every containerized deployment.
    ///
    /// The `[db.sqlite]` lane is covered above; this pins a nested struct under
    /// `[server]` and one under `[cluster]`, the two deepest env paths in
    /// common use.
    #[test]
    fn env_var_overrides_still_reach_strict_nested_structs() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server]\nlisten = \"0.0.0.0:8080\"\n").unwrap();

        let _env = EnvVars::many([
            ("EPHPM_SERVER__TIMEOUTS__REQUEST", Some("600")),
            ("EPHPM_CLUSTER__ENABLED", Some("true")),
        ]);
        let config = Config::load(&file).unwrap();
        assert_eq!(
            config.server.timeouts.request, 600,
            "an EPHPM_ env override must still reach a deny_unknown_fields struct"
        );
        assert!(config.cluster.enabled);
    }

    /// **The top-level `Config` must stay lenient**, and this is the reason.
    ///
    /// `Env::prefixed("EPHPM_")` is unfiltered, so *every* `EPHPM_*` variable in
    /// the environment becomes a top-level key — including ones that are not
    /// config at all. ePHPm sets one itself: the Windows service wrapper exports
    /// `EPHPM_SERVICE_LOG_FILE` before the server starts
    /// (`crates/ephpm/src/service/windows.rs`), which figment turns into a
    /// top-level `service_log_file`. Making the root strict would make ePHPm
    /// refuse to start as a Windows service. The e2e harness vars (`EPHPM_URL`,
    /// `EPHPM_BINARY`, …) are the same class.
    ///
    /// The nested sections have no such problem: nothing sets an `EPHPM_*`
    /// variable containing `__`, so every key that reaches a section came from
    /// an operator asking for something.
    #[test]
    fn config_root_stays_lenient_so_non_config_env_vars_cannot_block_startup() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server]\nlisten = \"0.0.0.0:8080\"\n").unwrap();

        let _env = EnvVars::set("EPHPM_SERVICE_LOG_FILE", "C:/ProgramData/ephpm/ephpm.log");
        let config = Config::load(&file)
            .expect("a non-config EPHPM_* variable must not stop the server from starting");
        assert_eq!(config.server.listen, "0.0.0.0:8080");

        // Same via the TOML provider: an unknown *top-level* key is tolerated.
        let config = load_toml("bogus_root_key = 1\n[server]\nlisten = \"0.0.0.0:8080\"\n");
        assert_eq!(config.server.listen, "0.0.0.0:8080");
    }

    /// `[db.sqlite.sqld]` is the one section that must keep swallowing unknown
    /// keys. It exists *only* so configs written for v0.6.x still parse; its
    /// own knobs were deleted in v0.7.0, so rejecting a key we no longer
    /// declare would break exactly the upgrade path the block was kept for.
    #[test]
    fn deprecated_sqld_block_tolerates_its_own_removed_keys() {
        let config = load_toml(
            "[db.sqlite]\n\
             dir = \"/var/lib/ephpm/sites\"\n\
             \n\
             [db.sqlite.sqld]\n\
             write_permits = 4\n\
             some_forgotten_sqld_knob = \"whatever\"\n",
        );
        let sqlite = config.db.sqlite.expect("sqlite section present");
        assert_eq!(sqlite.sqld.and_then(|s| s.write_permits), Some(4));
    }

    /// Every config shipped in this repo must load under the strict structs.
    ///
    /// Strictness is only safe if what we hand people actually parses. This
    /// catches the embarrassing failure mode directly: an example config, a
    /// smoke-test config, or the reference `ephpm.toml` carrying a key no
    /// struct declares.
    #[test]
    fn every_config_shipped_in_this_repo_loads() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let shipped = [
            "ephpm.toml",
            "examples/php-middleware/demo/ephpm.toml",
            "examples/rust-middleware/demo/ephpm.toml",
            "examples/wordpress-compose/ephpm.toml",
            "tests/ephpm-test.toml",
            "tests/smoke/ephpm-laravel.toml",
            "tests/smoke/ephpm-symfony.toml",
            "tests/smoke/ephpm-wordpress.toml",
        ];
        for rel in shipped {
            let path = root.join(rel);
            assert!(path.exists(), "{rel} is listed here but missing from the repo");
            Config::load(&path).unwrap_or_else(|e| {
                panic!(
                    "{rel} does not load. If this is an unknown-field error, either the config \
                     has a typo or a knob was removed without keeping a declared shim: {e}"
                )
            });
        }
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
    fn test_middleware_body_limit_defaults_to_disabled() {
        // Opt-in by design: 0 preserves the reject-before-body-transfer
        // property. Both the field default and the whole-section-absent path
        // (RequestConfig::default) must land on 0.
        assert_eq!(default_middleware_body_limit(), 0);
        assert_eq!(RequestConfig::default().middleware_body_limit, 0);
    }

    #[test]
    fn test_middleware_body_limit_parses_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r"
[server.request]
middleware_body_limit = 1048576
",
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        config.validate().unwrap();
        assert_eq!(config.server.request.middleware_body_limit, 1_048_576);
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
        let _env = EnvVars::set("EPHPM_SERVER__RESPONSE__COMPRESSION_STREAMING", "all");
        let config = Config::default_config().unwrap();
        assert_eq!(config.server.response.compression_streaming, "all");
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

        let _env = EnvVars::set("EPHPM_SERVER__PHP_ETAG_CACHE__ENABLED", "true");
        let config = Config::load(&file).unwrap();
        assert!(config.server.php_etag_cache.enabled);
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

        let _env = EnvVars::set("EPHPM_KV__COMPRESSION", "gzip");
        let config = Config::load(&file).unwrap();
        assert_eq!(config.kv.compression, "gzip");
    }

    #[test]
    fn test_env_var_overrides_compression_level() {
        let _env = EnvVars::set("EPHPM_KV__COMPRESSION_LEVEL", "8");
        let config = Config::default_config().unwrap();
        assert_eq!(config.kv.compression_level, 8);
    }

    #[test]
    fn test_env_var_overrides_compression_min_size() {
        let _env = EnvVars::set("EPHPM_KV__COMPRESSION_MIN_SIZE", "4096");
        let config = Config::default_config().unwrap();
        assert_eq!(config.kv.compression_min_size, 4096);
    }

    #[test]
    fn test_env_var_overrides_vec_string() {
        let _env = EnvVars::set("EPHPM_CLUSTER__JOIN", r#"["10.0.0.1:7946","10.0.0.2:7946"]"#);
        let config = Config::default_config().unwrap();
        assert_eq!(
            config.cluster.join,
            vec!["10.0.0.1:7946".to_string(), "10.0.0.2:7946".to_string()]
        );
    }

    #[test]
    fn test_env_var_overrides_vec_pair_string() {
        let _env = EnvVars::set(
            "EPHPM_PHP__INI_OVERRIDES",
            r#"[["display_errors","Off"],["error_reporting","E_ALL"]]"#,
        );
        let config = Config::default_config().unwrap();
        assert_eq!(
            config.php.ini_overrides,
            vec![
                ["display_errors".to_string(), "Off".to_string()],
                ["error_reporting".to_string(), "E_ALL".to_string()],
            ]
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
        assert_eq!(
            sqlite.proxy.max_connections, 0,
            "max_connections must default to 0 (unlimited) when [db.sqlite.proxy] is absent — \
             a surprise cap would refuse connections on upgrade"
        );
        assert!(
            sqlite.proxy.mysql_wire_enabled,
            "mysql_wire_enabled must default to true (the wire listener is bound) — the toggle \
             only turns the frontend OFF for bridge-only deployments"
        );
        assert!(
            sqlite.sqld.is_none(),
            "the [db.sqlite.sqld] block is removed in v0.7.0 and absent by default"
        );
        assert_eq!(sqlite.replication.role, "auto");
        assert!(sqlite.replication.primary_grpc_url.is_empty());
        assert_eq!(sqlite.engine, "turso", "engine must default to \"turso\" (the only engine)");
        assert!(sqlite.dir.is_none(), "per-site `dir` must be absent by default (single-site)");
        assert_eq!(
            sqlite.max_open_dbs, 256,
            "max_open_dbs must default to 256 open per-site databases"
        );
    }

    #[test]
    fn test_sqlite_per_site_knobs_parse() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[db.sqlite]
dir = "/var/lib/ephpm/dbs"
max_open_dbs = 32
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        let sqlite = config.db.sqlite.expect("sqlite should be present");
        assert_eq!(sqlite.dir.as_deref(), Some("/var/lib/ephpm/dbs"));
        assert_eq!(sqlite.max_open_dbs, 32);
    }

    #[test]
    fn test_replication_per_site_defaults_off_and_parses() {
        // Absent: the knob defaults off, and its sibling replication
        // defaults are undisturbed (section-level serde defaults do not
        // silently zero a field default).
        assert!(!ReplicationConfig::default().per_site);

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[db.sqlite]
dir = "/var/lib/ephpm/dbs"

[db.sqlite.replication]
per_site = true
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        let sqlite = config.db.sqlite.expect("sqlite should be present");
        assert!(sqlite.replication.per_site, "per_site = true must parse");
        // Present-section sibling defaults still hold.
        assert_eq!(sqlite.replication.role, "auto");
        assert_eq!(sqlite.replication.max_snapshot_bytes, default_max_snapshot_bytes());
    }

    #[test]
    fn test_sqlite_proxy_max_connections_parses() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[db.sqlite]
path = "app.db"

[db.sqlite.proxy]
max_connections = 64
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        let sqlite = config.db.sqlite.expect("sqlite should be present");
        assert_eq!(sqlite.proxy.max_connections, 64);
        // Setting max_connections alone must not disturb sibling defaults
        // (the section is now present, so serde section-level defaults no
        // longer apply — each field default must hold on its own).
        assert_eq!(sqlite.proxy.mysql_listen, "127.0.0.1:3306");
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
        assert!(sqlite.sqld.is_none(), "no [db.sqlite.sqld] block was set");
        assert_eq!(sqlite.replication.role, "replica");
        assert_eq!(sqlite.replication.primary_grpc_url, "http://10.0.1.2:5001");
    }

    /// `mysql_wire_enabled` defaults true (via the section-level `Default`)
    /// even when `[db.sqlite.proxy]` is present but omits the key, and can be
    /// turned off explicitly for bridge-only deployments.
    #[test]
    fn test_mysql_wire_enabled_toggle() {
        // Section-level default: present proxy block, key omitted → true.
        assert!(
            SqliteProxyConfig::default().mysql_wire_enabled,
            "SqliteProxyConfig::default() must have the wire listener enabled"
        );

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[db.sqlite]
path = "app.db"

[db.sqlite.proxy]
mysql_wire_enabled = false
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        let sqlite = config.db.sqlite.expect("sqlite should be present");
        assert!(
            !sqlite.proxy.mysql_wire_enabled,
            "mysql_wire_enabled = false must disable the wire listener"
        );
        // The listen address still parses/defaults — the frontend is skipped
        // by the server, not unset in config.
        assert_eq!(sqlite.proxy.mysql_listen, "127.0.0.1:3306");
    }

    /// A stale `[db.sqlite.sqld]` block from a pre-v0.7.0 config must still
    /// parse (into the deprecated, ignored shim) so upgrading users do not
    /// hit a hard TOML error; startup warns about it separately.
    #[test]
    fn test_deprecated_sqld_block_still_parses() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[db.sqlite]
path = "/var/lib/ephpm/app.db"

[db.sqlite.sqld]
write_permits = 4
http_listen = "127.0.0.1:9081"
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        let sqlite = config.db.sqlite.expect("sqlite should be present");
        let sqld = sqlite.sqld.expect("deprecated [db.sqlite.sqld] block should parse");
        assert_eq!(sqld.write_permits, Some(4));
        assert_eq!(sqld.http_listen.as_deref(), Some("127.0.0.1:9081"));
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

        let _env = EnvVars::set("EPHPM_DB__SQLITE__REPLICATION__ROLE", "primary");
        let config = Config::load(&file).unwrap();
        let sqlite = config.db.sqlite.expect("sqlite should be present");
        assert_eq!(sqlite.replication.role, "primary");
    }

    // ── [server] site_overrides_dir (operator per-site overrides) ──────

    /// The `[server]` section is ABSENT entirely — the case where a
    /// `#[derive(Default)]` on the parent could zero a carefully chosen field
    /// default. Unset is the intended default here, and it must stay unset.
    #[test]
    fn test_site_overrides_dir_defaults_unset_with_section_absent() {
        let config = Config::default_config().unwrap();
        assert_eq!(config.server.site_overrides_dir, None);
        assert_eq!(config.server.effective_site_overrides_dir(), None);
    }

    /// The `[server]` section is PRESENT but does not mention the field.
    #[test]
    fn test_site_overrides_dir_defaults_unset_with_section_present() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server]\nsites_dir = \"/var/www/sites\"\n").unwrap();

        let config = Config::load(&file).unwrap();
        config.validate().unwrap();
        assert_eq!(config.server.effective_site_overrides_dir(), None);
    }

    #[test]
    fn test_site_overrides_dir_set_alongside_sites_dir() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            "[server]\nsites_dir = \"/var/www/sites\"\n\
             site_overrides_dir = \"/var/lib/ephpm/site-overrides\"\n",
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.server.effective_site_overrides_dir(),
            Some(Path::new("/var/lib/ephpm/site-overrides"))
        );
    }

    /// Per-site overrides only act in multi-tenant mode. Without `sites_dir` the
    /// knob resolves inert (and `Router::new` warns) rather than silently
    /// pretending to work.
    #[test]
    fn test_site_overrides_dir_is_inert_without_sites_dir() {
        let mut cfg = Config::default_config().unwrap();
        cfg.server.site_overrides_dir = Some(PathBuf::from("/var/lib/ephpm/site-overrides"));
        assert!(cfg.server.sites_dir.is_none());
        cfg.validate().unwrap();
        assert_eq!(cfg.server.effective_site_overrides_dir(), None);
    }

    /// Fail closed: an override directory inside `sites_dir` is inside some
    /// tenant's `open_basedir`, so that tenant's PHP could rewrite its own
    /// routing. This is the security property the whole design rests on.
    #[test]
    fn test_site_overrides_dir_inside_sites_dir_is_rejected() {
        for (sites, overrides) in [
            ("/var/www/sites", "/var/www/sites/_overrides"),
            ("/var/www/sites", "/var/www/sites"),
            ("/var/www/sites", "/var/www/sites/blog.example.com/conf"),
            ("./sites", "sites/_overrides"),
        ] {
            let mut cfg = Config::default_config().unwrap();
            cfg.server.sites_dir = Some(PathBuf::from(sites));
            cfg.server.site_overrides_dir = Some(PathBuf::from(overrides));
            let err = cfg.validate().expect_err("must reject overrides inside sites_dir");
            let msg = format!("{err}");
            assert!(
                msg.contains("site_overrides_dir"),
                "error should name the knob for {overrides}: {msg}"
            );
        }
    }

    /// Issue #397: a `sites_domain_suffix` without a leading dot is rejected at
    /// startup — it is the operator error that lets `Host: <suffix>` strip to
    /// the empty vhost key and collapse the whole `sites_dir` into one
    /// `open_basedir`.
    #[test]
    fn test_dotless_sites_domain_suffix_is_rejected() {
        for suffix in ["localhost", "example.com", "preview.ephpm.dev", ""] {
            let mut cfg = Config::default_config().unwrap();
            cfg.server.sites_domain_suffix = Some(suffix.to_string());
            let err = cfg.validate().expect_err("a dotless sites_domain_suffix must be rejected");
            let msg = format!("{err}");
            assert!(
                msg.contains("sites_domain_suffix"),
                "error should name the knob for {suffix:?}: {msg}"
            );
        }
    }

    /// The good path: a correctly *dotted* suffix passes validation, and an
    /// unset suffix is fine too.
    #[test]
    fn test_dotted_sites_domain_suffix_is_accepted() {
        for suffix in [Some(".localhost"), Some(".preview.ephpm.dev"), None] {
            let mut cfg = Config::default_config().unwrap();
            cfg.server.sites_domain_suffix = suffix.map(str::to_owned);
            cfg.validate()
                .unwrap_or_else(|e| panic!("dotted/unset suffix {suffix:?} must validate: {e}"));
        }
    }

    /// A sibling directory whose path merely *starts with the same characters*
    /// is not inside `sites_dir` — the check is component-wise, not textual.
    #[test]
    fn test_site_overrides_dir_sibling_path_is_accepted() {
        let mut cfg = Config::default_config().unwrap();
        cfg.server.sites_dir = Some(PathBuf::from("/var/www/sites"));
        cfg.server.site_overrides_dir = Some(PathBuf::from("/var/www/sites-overrides"));
        cfg.validate().expect("a sibling directory is not inside sites_dir");
    }

    #[test]
    fn test_site_overrides_dir_env_override() {
        let _env = EnvVars::set("EPHPM_SERVER__SITE_OVERRIDES_DIR", "/srv/overrides");
        let config = Config::default_config().unwrap();
        assert_eq!(config.server.site_overrides_dir, Some(PathBuf::from("/srv/overrides")));
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

        let _env = EnvVars::set("EPHPM_SERVER__SECURITY__OPEN_BASEDIR", "false");
        let config = Config::load(&file).unwrap();
        assert!(!config.server.effective_open_basedir());
        // The env var materializes the section, so the unset sibling
        // still resolves true (and sites_dir is set anyway).
        assert!(config.server.effective_disable_shell_exec());
    }

    // ── Enabled-but-inert isolation flags (single-site mode) ───────────
    //
    // Both flags are implemented only on the multi-tenant path. Resolving
    // to `true` without `sites_dir` therefore buys nothing, and `ephpm`
    // warns at startup for every name `inert_security_flags` returns.

    #[test]
    fn test_inert_security_flags_quiet_when_nothing_enabled() {
        let config = Config::default_config().unwrap();
        assert!(
            config.server.inert_security_flags().is_empty(),
            "a config that never mentions [server.security] must not warn"
        );
    }

    #[test]
    fn test_inert_security_flags_empty_in_multi_tenant_mode() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[server]
sites_dir = "/var/www/sites"

[server.security]
open_basedir = true
disable_shell_exec = true
"#,
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        assert!(
            config.server.inert_security_flags().is_empty(),
            "with sites_dir set, both flags are actually applied"
        );
    }

    #[test]
    fn test_inert_security_flags_lists_both_in_single_site_mode() {
        // The audit case: a single-site operator adds [server.security]
        // for trusted_proxies, both isolation flags silently resolve to
        // `true`, and nothing sandboxes anything.
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
        assert!(config.server.sites_dir.is_none());
        assert_eq!(
            config.server.inert_security_flags(),
            vec!["open_basedir", "disable_shell_exec", "multi_tenant_hardening"],
        );
    }

    #[test]
    fn test_inert_security_flags_reports_only_the_enabled_one() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r"
[server.security]
open_basedir = true
disable_shell_exec = false
multi_tenant_hardening = false
",
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        assert_eq!(config.server.inert_security_flags(), vec!["open_basedir"]);
    }

    #[test]
    fn test_inert_security_flags_quiet_when_explicitly_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r"
[server.security]
open_basedir = false
disable_shell_exec = false
multi_tenant_hardening = false
",
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        assert!(
            config.server.inert_security_flags().is_empty(),
            "nothing was asked for, so there is nothing to warn about"
        );
    }

    #[test]
    fn test_multi_tenant_hardening_resolution() {
        // Section absent + sites_dir set → on (multi-tenant secure default).
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server]\nsites_dir = \"/var/www/sites\"\n").unwrap();
        let config = Config::load(&file).unwrap();
        assert!(config.server.effective_multi_tenant_hardening());

        // Section absent + no sites_dir → off (single-site).
        let file2 = dir.path().join("ephpm2.toml");
        std::fs::write(&file2, "[server]\nlisten = \"0.0.0.0:8080\"\n").unwrap();
        let config2 = Config::load(&file2).unwrap();
        assert!(!config2.server.effective_multi_tenant_hardening());

        // Explicit false wins even with sites_dir set.
        let file3 = dir.path().join("ephpm3.toml");
        std::fs::write(
            &file3,
            "[server]\nsites_dir = \"/var/www/sites\"\n\n[server.security]\nmulti_tenant_hardening = false\n",
        )
        .unwrap();
        let config3 = Config::load(&file3).unwrap();
        assert!(!config3.server.effective_multi_tenant_hardening());
    }

    #[test]
    fn test_network_egress_externally_managed_resolution() {
        let dir = tempfile::tempdir().unwrap();

        // Absent everywhere → false (safe: never inferred from multi-tenant
        // mode, because ePHPm cannot verify an external egress control exists).
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server]\nsites_dir = \"/var/www/sites\"\n").unwrap();
        let config = Config::load(&file).unwrap();
        assert!(!config.server.effective_network_egress_externally_managed());

        // Section present but field unset → still false (does NOT inherit the
        // "section present ⇒ true" default the isolation flags use).
        let file2 = dir.path().join("ephpm2.toml");
        std::fs::write(
            &file2,
            "[server]\nsites_dir = \"/var/www/sites\"\n\n[server.security]\nopen_basedir = true\n",
        )
        .unwrap();
        let config2 = Config::load(&file2).unwrap();
        assert!(!config2.server.effective_network_egress_externally_managed());

        // Explicit true wins.
        let file3 = dir.path().join("ephpm3.toml");
        std::fs::write(
            &file3,
            "[server]\nsites_dir = \"/var/www/sites\"\n\n[server.security]\nnetwork_egress_externally_managed = true\n",
        )
        .unwrap();
        let config3 = Config::load(&file3).unwrap();
        assert!(config3.server.effective_network_egress_externally_managed());
    }

    // ── [server] preview preset + [server.limits] resolution ───────────

    // Exact float equality is intended in these tests: the values either come
    // verbatim from TOML/env parsing or from `unwrap_or` of a literal — no
    // arithmetic happens, so bit-exact comparison is the correct assertion.
    #[test]
    #[allow(clippy::float_cmp)]
    fn test_preview_defaults_off_and_limits_resolve_all_off() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server]\nlisten = \"0.0.0.0:8080\"\n").unwrap();

        let config = Config::load(&file).unwrap();
        assert!(!config.server.preview, "preview must default to off");
        assert_eq!(
            config.server.effective_limits(),
            ResolvedLimits::default(),
            "without preview, absent [server.limits] must resolve to all-off \
             (max_connections=0, per_ip*=0, per_site_rate=0, bursts 50/20)"
        );
        assert!(config.server.preview_preset_applied().is_empty());

        // The worst-case default check the knob checklist requires: the
        // no-preset resolution must not impose any limit.
        let limits = config.server.effective_limits();
        assert_eq!(limits.max_connections, 0);
        assert_eq!(limits.per_ip_max_connections, 0);
        assert_eq!(limits.per_ip_rate, 0.0);
        assert_eq!(limits.per_ip_burst, 50);
        assert_eq!(limits.per_site_rate, 0.0);
        assert_eq!(limits.per_site_burst, 20);
    }

    /// The serde section-default footgun: `[server.limits]` present-but-empty
    /// must behave exactly like the section being absent.
    #[test]
    fn test_limits_section_present_but_empty_equals_absent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server.limits]\n").unwrap();
        let present = Config::load(&file).unwrap();

        let file2 = dir.path().join("ephpm2.toml");
        std::fs::write(&file2, "[server]\nlisten = \"0.0.0.0:8080\"\n").unwrap();
        let absent = Config::load(&file2).unwrap();

        assert_eq!(present.server.effective_limits(), absent.server.effective_limits());

        // And the same equivalence under the preview preset.
        let file3 = dir.path().join("ephpm3.toml");
        std::fs::write(&file3, "[server]\npreview = true\n\n[server.limits]\n").unwrap();
        let preview_present = Config::load(&file3).unwrap();
        let file4 = dir.path().join("ephpm4.toml");
        std::fs::write(&file4, "[server]\npreview = true\n").unwrap();
        let preview_absent = Config::load(&file4).unwrap();
        assert_eq!(
            preview_present.server.effective_limits(),
            preview_absent.server.effective_limits()
        );
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn test_preview_preset_fills_unset_limits() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server]\npreview = true\n").unwrap();

        let config = Config::load(&file).unwrap();
        assert!(config.server.preview);
        assert_eq!(
            config.server.effective_limits(),
            ResolvedLimits::preview_preset(),
            "preview with no explicit limits must resolve to the full preset"
        );

        // The startup log's source of truth: every field was preset-supplied.
        let applied = config.server.preview_preset_applied();
        assert_eq!(
            applied.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            vec![
                "max_connections",
                "per_ip_max_connections",
                "per_ip_rate",
                "per_ip_burst",
                "per_site_rate",
                "per_site_burst"
            ],
        );
        assert_eq!(ResolvedLimits::preview_preset().max_connections, 256);
        assert_eq!(ResolvedLimits::preview_preset().per_site_rate, 5.0);
        assert_eq!(ResolvedLimits::preview_preset().per_site_burst, 20);
    }

    /// Explicit operator values always beat the preview preset — including
    /// explicit `0`, which disables that limit even under preview.
    #[test]
    #[allow(clippy::float_cmp)]
    fn test_preview_explicit_limits_win_over_preset() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            "[server]\npreview = true\n\n[server.limits]\n\
             max_connections = 0\nper_site_rate = 2.5\n",
        )
        .unwrap();

        let config = Config::load(&file).unwrap();
        let limits = config.server.effective_limits();
        assert_eq!(limits.max_connections, 0, "explicit 0 must disable, even under preview");
        assert_eq!(limits.per_site_rate, 2.5, "explicit value must win over the preset 5.0");
        // Unset fields still take the preset.
        assert_eq!(limits.per_ip_max_connections, 32);
        assert_eq!(limits.per_ip_rate, 10.0);
        assert_eq!(limits.per_site_burst, 20);

        // The applied list reports only the preset-supplied fields.
        let applied = config.server.preview_preset_applied();
        let keys: Vec<_> = applied.iter().map(|(k, _)| *k).collect();
        assert!(!keys.contains(&"max_connections"));
        assert!(!keys.contains(&"per_site_rate"));
        assert!(keys.contains(&"per_ip_rate"));
    }

    /// Without preview, explicitly set limits are enforced verbatim and the
    /// new per-site knobs parse.
    #[test]
    #[allow(clippy::float_cmp)]
    fn test_per_site_limits_parse_without_preview() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server.limits]\nper_site_rate = 7.0\nper_site_burst = 3\n")
            .unwrap();

        let config = Config::load(&file).unwrap();
        let limits = config.server.effective_limits();
        assert_eq!(limits.per_site_rate, 7.0);
        assert_eq!(limits.per_site_burst, 3);
        // Setting the per-site pair alone must not disturb sibling defaults.
        assert_eq!(limits.max_connections, 0);
        assert_eq!(limits.per_ip_burst, 50);
        assert!(config.server.preview_preset_applied().is_empty(), "no preview → nothing applied");
    }

    #[test]
    fn test_env_var_overrides_preview() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server]\nlisten = \"0.0.0.0:8080\"\n").unwrap();

        let _env = EnvVars::set("EPHPM_SERVER__PREVIEW", "true");
        let config = Config::load(&file).unwrap();
        assert!(config.server.preview, "EPHPM_SERVER__PREVIEW=true must enable preview mode");
        assert_eq!(config.server.effective_limits(), ResolvedLimits::preview_preset());
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn test_env_var_overrides_per_site_rate() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server]\npreview = true\n").unwrap();

        let _env = EnvVars::set("EPHPM_SERVER__LIMITS__PER_SITE_RATE", "1.5");
        let config = Config::load(&file).unwrap();
        assert_eq!(
            config.server.effective_limits().per_site_rate,
            1.5,
            "env override is an explicit value and must beat the preview preset"
        );
    }

    // ── [kv] eviction_policy validation ────────────────────────────────
    //
    // `EvictionPolicy::from_str_lossy` folds every unknown string into
    // `allkeys-lru`, so validation is the only thing standing between a
    // typo and a silent change of eviction behaviour.

    #[test]
    fn test_kv_eviction_policy_accepts_every_documented_value() {
        for policy in KV_EVICTION_POLICIES {
            let mut cfg = Config::default_config().unwrap();
            cfg.kv.eviction_policy = policy.to_string();
            cfg.validate().unwrap_or_else(|e| panic!("{policy} must validate, got: {e}"));
        }
    }

    #[test]
    fn test_kv_eviction_policy_default_validates() {
        let cfg = Config::default_config().unwrap();
        assert_eq!(cfg.kv.eviction_policy, "allkeys-lru");
        cfg.validate().expect("the default must validate");
    }

    #[test]
    fn test_kv_eviction_policy_typo_is_rejected() {
        let mut cfg = Config::default_config().unwrap();
        cfg.kv.eviction_policy = "allkey-lru".to_string();
        let err = cfg.validate().expect_err("a typo must not silently mean allkeys-lru");
        assert!(matches!(err, ConfigError::Validation(_)));
        let msg = err.to_string();
        assert!(msg.contains("allkey-lru"), "the error must name the bad value: {msg}");
        for policy in KV_EVICTION_POLICIES {
            assert!(msg.contains(policy), "the error must list {policy}: {msg}");
        }
    }

    #[test]
    fn test_kv_eviction_policy_is_case_sensitive() {
        // `from_str_lossy` matches the exact lowercase spellings, so
        // "AllKeys-LRU" would have fallen through to the default.
        let mut cfg = Config::default_config().unwrap();
        cfg.kv.eviction_policy = "AllKeys-LRU".to_string();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_kv_eviction_policy_rejected_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[kv]
eviction_policy = "lru"
"#,
        )
        .unwrap();

        // Parsing still succeeds — the field is a plain String; it is
        // `validate()` (called by `ephpm` before startup) that rejects it.
        let config = Config::load(&file).unwrap();
        assert!(config.validate().is_err());
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
        let lines = cfg.opcache_ini_lines(false, false);
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

    /// The Windows-only per-process `opcache.cache_id` line, in the position
    /// [`AutoTune::ini_lines`] emits it — after `validate_timestamps` /
    /// `revalidate_freq`, before every derived tunable. Empty on Unix, where
    /// the directive does not exist in PHP at all.
    fn cache_id_lines() -> Vec<(String, String)> {
        if cfg!(windows) {
            vec![("opcache.cache_id".to_string(), format!("ephpm-{}", process::id()))]
        } else {
            Vec::new()
        }
    }

    #[test]
    fn test_opcache_ini_lines_dev_default() {
        // Dev mode derives nothing — only the mode-appropriate
        // validate_timestamps line plus the always-emitted memory_limit,
        // keeping the dev php.ini minimal. (On Windows the per-process
        // opcache.cache_id rides along: the ASLR reattach collision kills
        // `ephpm dev` just as dead as `ephpm serve`.)
        let cfg = PhpConfig::default();
        let lines = cfg.opcache_ini_lines(true, false);
        let mut expected = vec![("opcache.validate_timestamps".to_string(), "1".to_string())];
        expected.extend(cache_id_lines());
        expected.push(("memory_limit".to_string(), "128M".to_string()));
        assert_eq!(lines, expected);
    }

    #[test]
    fn test_opcache_ini_lines_include_revalidate_freq_when_set() {
        // Dev mode so no derived lines interfere — assert exact output.
        let cfg = PhpConfig {
            opcache_validate_timestamps: Some(true),
            opcache_revalidate_freq: Some(60),
            ..PhpConfig::default()
        };
        let lines = cfg.opcache_ini_lines(true, false);
        let mut expected = vec![
            ("opcache.validate_timestamps".to_string(), "1".to_string()),
            ("opcache.revalidate_freq".to_string(), "60".to_string()),
        ];
        expected.extend(cache_id_lines());
        expected.push(("memory_limit".to_string(), "128M".to_string()));
        assert_eq!(lines, expected);
    }

    #[test]
    fn test_memory_limit_emitted_in_dev_mode() {
        // Regression: `[php] memory_limit` resolves through the bottom
        // ("stock default") tier of the three-tier resolver, so it used to be
        // filtered out of the generated php.ini as `Origin::Default`. Dev mode
        // derives nothing, so this is where the drop always bit.
        let cfg = PhpConfig { memory_limit: "512M".to_string(), ..PhpConfig::default() };
        let lines = cfg.opcache_ini_lines(true, false);
        assert!(
            lines.contains(&("memory_limit".to_string(), "512M".to_string())),
            "dev-mode php.ini must carry [php] memory_limit, got: {lines:?}"
        );
    }

    #[test]
    fn test_memory_limit_emitted_when_origin_is_default() {
        // Platform-independent form of the same regression: whatever the host
        // detects, a memory_limit that resolved to the bottom tier must still
        // reach the generated ini. This is the serve-mode path on macOS, where
        // `read_total_system_memory()` returns `None` so `derive_tuning`
        // produces no `memory_limit`.
        let mut at = PhpConfig::default().autotune(true, false);
        at.memory_limit = TunedValue { value: "384M".to_string(), origin: Origin::Default };
        let lines = at.ini_lines();
        assert!(
            lines.contains(&("memory_limit".to_string(), "384M".to_string())),
            "Origin::Default memory_limit must still be emitted, got: {lines:?}"
        );
    }

    #[cfg(not(any(target_os = "linux", windows)))]
    #[test]
    fn test_memory_limit_emitted_in_serve_mode_without_memory_budget() {
        // On hosts with no memory probe (macOS) `detect_memory_budget()` yields
        // `None`, so serve mode derives no memory_limit and the configured
        // value lands in the bottom tier — the second half of the same
        // regression. Linux (cgroup/meminfo) and Windows (GlobalMemoryStatusEx)
        // both detect a budget and therefore derive over it.
        let cfg = PhpConfig { memory_limit: "512M".to_string(), ..PhpConfig::default() };
        let lines = cfg.opcache_ini_lines(false, false);
        assert!(
            lines.contains(&("memory_limit".to_string(), "512M".to_string())),
            "serve-mode php.ini must carry [php] memory_limit when nothing is \
             derived, got: {lines:?}"
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

    // --- Resource-aware autotuning: job-object / physical-RAM selection ---

    #[test]
    fn test_select_memory_budget_prefers_a_restricting_job_limit() {
        let gib = 1024 * MIB;
        // A 2 GiB job limit on a 128 GiB box is a real restriction.
        assert_eq!(
            select_memory_budget(Some(2 * gib), Some(128 * gib)),
            (Some(2 * gib), MemorySource::JobObject)
        );
    }

    #[test]
    fn test_select_memory_budget_ignores_a_non_restricting_job_limit() {
        let gib = 1024 * MIB;
        // A job limit at or above physical RAM caps nothing the hardware does
        // not already cap — report the honest physical figure instead.
        assert_eq!(
            select_memory_budget(Some(256 * gib), Some(128 * gib)),
            (Some(128 * gib), MemorySource::SystemTotal)
        );
        assert_eq!(
            select_memory_budget(Some(128 * gib), Some(128 * gib)),
            (Some(128 * gib), MemorySource::SystemTotal)
        );
    }

    #[test]
    fn test_select_memory_budget_without_a_job_limit() {
        let gib = 1024 * MIB;
        assert_eq!(
            select_memory_budget(None, Some(64 * gib)),
            (Some(64 * gib), MemorySource::SystemTotal)
        );
    }

    #[test]
    fn test_select_memory_budget_job_limit_only() {
        let gib = 1024 * MIB;
        // Physical RAM unreadable but a job limit is known: still better than
        // nothing, and labelled as what it is.
        assert_eq!(
            select_memory_budget(Some(4 * gib), None),
            (Some(4 * gib), MemorySource::JobObject)
        );
    }

    #[test]
    fn test_select_memory_budget_nothing_detectable_stays_unknown() {
        // The fallback must remain "unknown" + PHP-stock floors, never a guess.
        assert_eq!(select_memory_budget(None, None), (None, MemorySource::Unknown));
        assert_eq!(MemorySource::Unknown.label(), "unknown");
        assert_eq!(MemorySource::JobObject.label(), "job-object");
        assert_eq!(MemorySource::SystemTotal.label(), "system-total");
    }

    #[test]
    fn test_select_job_memory_limit_requires_the_flag() {
        let gib = 1024 * MIB;
        // Fields are only meaningful when their LimitFlags bit is set; a stale
        // non-zero value with no flag must not be read as a limit.
        assert_eq!(select_job_memory_limit(0, 2 * gib, 4 * gib), None);
        // Flag set but the value is zero => not a 0-byte cap, just unset.
        assert_eq!(select_job_memory_limit(JOB_OBJECT_LIMIT_JOB_MEMORY_FLAG, 0, 0), None);
    }

    #[test]
    fn test_select_job_memory_limit_picks_the_smaller_enabled_cap() {
        let gib = 1024 * MIB;
        assert_eq!(
            select_job_memory_limit(JOB_OBJECT_LIMIT_PROCESS_MEMORY_FLAG, 2 * gib, 4 * gib),
            Some(2 * gib)
        );
        assert_eq!(
            select_job_memory_limit(JOB_OBJECT_LIMIT_JOB_MEMORY_FLAG, 2 * gib, 4 * gib),
            Some(4 * gib)
        );
        // Both set => the process actually lives under the smaller of the two.
        assert_eq!(
            select_job_memory_limit(
                JOB_OBJECT_LIMIT_PROCESS_MEMORY_FLAG | JOB_OBJECT_LIMIT_JOB_MEMORY_FLAG,
                6 * gib,
                4 * gib,
            ),
            Some(4 * gib)
        );
        // Other job limits (e.g. JOB_OBJECT_LIMIT_ACTIVE_PROCESS = 0x8) must
        // not be mistaken for memory limits.
        assert_eq!(select_job_memory_limit(0x0000_0008, 2 * gib, 4 * gib), None);
    }

    /// Behavioural: on a real Windows host the physical-RAM probe must return a
    /// plausible figure, not `None` — this is the bug this coverage exists for
    /// (`mem=unknown` on Windows regardless of installed RAM).
    #[cfg(windows)]
    #[test]
    fn test_windows_physical_memory_probe_reports_a_real_figure() {
        let phys = read_total_system_memory().expect("GlobalMemoryStatusEx must report total RAM");
        // Any machine that can run ePHPm has >= 256 MiB; nothing has 1 PiB.
        assert!(phys >= 256 * MIB, "implausibly small physical RAM: {phys} bytes");
        assert!(phys < 1024 * 1024 * MIB, "implausibly large physical RAM: {phys} bytes");
    }

    /// Behavioural: the whole detection chain must produce a budget on Windows,
    /// with a source label that matches which probe won.
    #[cfg(windows)]
    #[test]
    fn test_windows_detect_memory_budget_is_never_unknown() {
        let (budget, source) = detect_memory_budget();
        assert!(budget.is_some(), "Windows memory budget must be detectable, got {source:?}");
        assert!(
            matches!(source, MemorySource::SystemTotal | MemorySource::JobObject),
            "unexpected Windows memory source: {source:?}"
        );
    }

    /// Behavioural: with a budget in hand, serve mode must derive the
    /// memory-shaped knobs instead of falling back to the PHP-stock floors.
    #[cfg(windows)]
    #[test]
    fn test_windows_serve_autotune_derives_memory_shaped_knobs() {
        let at = PhpConfig::default().autotune(false, false);
        assert_eq!(at.memory_consumption.origin, Origin::Derived);
        assert_eq!(at.memory_limit.origin, Origin::Derived);
        assert_ne!(at.mem_source, MemorySource::Unknown);
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
        // 18% of 4096 MiB = 737 MiB -> clamps down to the platform ceiling:
        // 512 MB on Unix, 256 MB on Windows (pagefile-backed, commit-charged
        // in full at startup).
        let ceiling = opcache_shm_ceiling_mb();
        assert_eq!(d.opcache_memory_consumption, Some(ceiling));
        // interned: ceiling/16 -> 32 on Unix, 16 on Windows (both within [8,64]).
        assert_eq!(d.opcache_interned_strings_buffer, Some(ceiling / 16));
        // jit: 4096/64 = 64 MB (at the ceiling) — not platform-shaped.
        assert_eq!(d.opcache_jit_buffer_size, Some(64));
        // memory_limit: (4096 - ceiling - 64)/4 -> 880M on Unix, 944M on Windows.
        let expected_limit = format!("{}M", (4096 - u64::from(ceiling) - 64) / 4);
        assert_eq!(d.memory_limit.as_deref(), Some(expected_limit.as_str()));
    }

    // --- Windows OPcache SHM: ASLR reattach collision (issue #362) ---

    #[test]
    fn test_windows_opcache_shm_ceiling_is_pinned() {
        // The Windows ceiling is load-bearing, not cosmetic: PHP's Windows SHM
        // segment is pagefile-backed and commit-charged in full at startup, and
        // a create failure is a hard exit(-2) inside php_module_startup. Pin
        // both constants so a future tuning sweep cannot quietly raise them.
        assert_eq!(WINDOWS_OPCACHE_SHM_CEILING_MB, 256);
        assert_eq!(UNIX_OPCACHE_SHM_CEILING_MB, 512);
        if cfg!(windows) {
            assert_eq!(opcache_shm_ceiling_mb(), 256);
        } else {
            assert_eq!(opcache_shm_ceiling_mb(), 512);
        }
    }

    #[test]
    fn test_derived_opcache_shm_never_exceeds_platform_ceiling() {
        // Sweep budgets from tiny to absurd; the derived value must always stay
        // inside [64, ceiling] on whatever platform this is compiled for.
        let ceiling = opcache_shm_ceiling_mb();
        for gib in [0u64, 1, 2, 4, 8, 16, 64, 128, 512] {
            let mem = if gib == 0 { None } else { Some(gib * 1024 * MIB) };
            let d = derive_tuning(Some(4.0), mem, 4, false);
            let mb = d.opcache_memory_consumption.expect("serve mode always derives a value");
            assert!(
                (64..=ceiling).contains(&mb),
                "budget {gib}GiB derived {mb}MB, outside [64, {ceiling}]"
            );
        }
    }

    #[test]
    fn test_explicit_opcache_shm_is_honoured_above_the_ceiling() {
        // The ceiling bounds the DERIVED value only — an operator who pins a
        // larger size still gets exactly what they asked for on every platform.
        let cfg = PhpConfig { opcache_memory_consumption: Some(1024), ..PhpConfig::default() };
        let at = cfg.autotune(false, false);
        assert_eq!(at.memory_consumption.value, 1024);
        assert_eq!(at.memory_consumption.origin, Origin::Explicit);
        assert!(
            at.ini_lines()
                .contains(&("opcache.memory_consumption".to_string(), "1024".to_string()))
        );
    }

    #[test]
    fn test_shm_warning_only_fires_for_explicit_over_ceiling_on_windows() {
        // Derived values never warn (they are already capped).
        assert_eq!(PhpConfig::default().autotune(false, false).shm_warning(), None);

        // An explicit value at or below the ceiling never warns.
        let at_low = PhpConfig {
            opcache_memory_consumption: Some(opcache_shm_ceiling_mb()),
            ..PhpConfig::default()
        }
        .autotune(false, false);
        assert_eq!(at_low.shm_warning(), None);

        // An explicit value above it warns on Windows, and stays silent on
        // Unix where the mapping is anonymous and lazily committed.
        let at_high = PhpConfig { opcache_memory_consumption: Some(2048), ..PhpConfig::default() }
            .autotune(false, false);
        if cfg!(windows) {
            let w = at_high.shm_warning().expect("windows warns above the ceiling");
            assert!(w.contains("2048"), "warning should name the value: {w}");
        } else {
            assert_eq!(at_high.shm_warning(), None);
        }
    }

    #[test]
    fn test_windows_emits_private_opcache_cache_id() {
        // The real fix for the "Opcode handlers are unusable due to ASLR"
        // startup abort: a per-process `opcache.cache_id` puts this process in
        // its own SHM namespace, so PHP always takes the segment-create path
        // and never the cross-process reattach path that performs the
        // execute_ex address check. `opcache.cache_id` is a Windows-only PHP
        // directive (the struct field does not exist on POSIX), so it must
        // NOT be emitted elsewhere.
        for (dev, multi) in [(false, false), (true, false), (false, true)] {
            let lines = PhpConfig::default().opcache_ini_lines(dev, multi);
            let found: Vec<&String> =
                lines.iter().filter(|(k, _)| k == "opcache.cache_id").map(|(_, v)| v).collect();
            if cfg!(windows) {
                assert_eq!(found.len(), 1, "dev={dev} multi={multi}: expected one cache_id");
                assert_eq!(found[0], &format!("ephpm-{}", process::id()));
            } else {
                assert!(found.is_empty(), "dev={dev} multi={multi}: cache_id is Windows-only");
            }
        }
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
        let at = cfg.autotune(false, false);
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
        let at = cfg.autotune(true, false);
        assert_eq!(at.memory_consumption.origin, Origin::Default);
        assert_eq!(at.max_accelerated_files.origin, Origin::Default);
        assert_eq!(at.zend_assertions.origin, Origin::Default);
        // But an explicit knob still wins in dev.
        let cfg2 = PhpConfig { zend_assertions: Some(-1), ..PhpConfig::default() };
        let at2 = cfg2.autotune(true, false);
        assert_eq!(at2.zend_assertions.value, -1);
        assert_eq!(at2.zend_assertions.origin, Origin::Explicit);
    }

    #[test]
    fn test_autotune_summary_line_marks_explicit() {
        let cfg = PhpConfig { opcache_memory_consumption: Some(200), ..PhpConfig::default() };
        let line = cfg.autotune(false, false).summary_line();
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

    // ── OPcache JIT: shaped default + `[php] opcache_jit` ──────────────

    #[test]
    fn test_opcache_jit_defaults_to_none() {
        assert_eq!(PhpConfig::default().opcache_jit, None);
    }

    #[test]
    fn test_jit_shaped_default_single_site_serve() {
        let at = PhpConfig::default().autotune(false, false);
        assert_eq!(at.jit.origin, Origin::Derived);
        assert!(at.jit_warning().is_none(), "the shaped default must not warn");

        // Issue #365: PHP's tracing JIT (8.4.24+ / 8.5.5+) kills the process
        // when a side trace compiles in a later request than its parent. A
        // stock Laravel app dies after 3 requests on Windows; on Linux it dies
        // at request 2 once any class links against a parent that is not in
        // OPcache SHM. Same faulting frames on both, and on stock `php -S`
        // with no ePHPm involved — so the default is off on every platform.
        //
        // The `disable` line must be emitted, not omitted: PHP <=8.3's stock
        // opcache.jit is `tracing` and serve mode always emits a non-zero
        // jit_buffer_size, so omitting it would leave the JIT on.
        assert_eq!(at.jit.value, JitMode::Disable);
        assert_eq!(at.jit_reason, JitReason::TracingJitBug);
        let lines = at.ini_lines();
        assert!(
            lines.contains(&("opcache.jit".to_string(), "disable".to_string())),
            "single-site serve must emit opcache.jit=disable, got: {lines:?}"
        );
    }

    #[test]
    fn test_jit_default_off_and_explicit_tracing_warns() {
        // The shaped default and its warning are a matched pair: the default
        // protects an operator who never touches the knob, and the warning
        // tells one who overrides it exactly what they are buying.
        let at = PhpConfig::default().autotune(false, false);
        assert_eq!(at.jit.value, JitMode::Disable);
        assert!(at.jit_line().contains("21710"), "got: {}", at.jit_line());

        let explicit = PhpConfig { opcache_jit: Some(JitMode::Tracing), ..PhpConfig::default() }
            .autotune(false, false);
        assert_eq!(explicit.jit.value, JitMode::Tracing, "an explicit knob must always win");
        assert_eq!(explicit.jit_reason, JitReason::Explicit);
        let warning =
            explicit.jit_warning().expect("explicit tracing JIT must warn about php-src PR 21710");
        assert!(warning.contains("21710"), "got: {warning}");

        // `function` never builds traces, so it cannot reach the side-trace
        // path and must never carry the tracing warning.
        let func = PhpConfig { opcache_jit: Some(JitMode::Function), ..PhpConfig::default() }
            .autotune(false, false);
        assert_eq!(func.jit.value, JitMode::Function);
        assert!(func.jit_warning().is_none());
    }

    #[test]
    fn test_jit_shaped_default_multi_tenant_is_disable() {
        let at = PhpConfig::default().autotune(false, true);
        assert_eq!(at.jit.value, JitMode::Disable);
        assert_eq!(at.jit.origin, Origin::Derived);
        assert_eq!(at.jit_reason, JitReason::MultiTenant);
        assert!(at.jit_warning().is_none());
        // Emitting `disable` explicitly is load-bearing: PHP <=8.3's stock
        // opcache.jit is `tracing`, and serve mode always emits a non-zero
        // jit_buffer_size — omitting the line would enable the JIT there.
        let lines = at.ini_lines();
        assert!(
            lines.contains(&("opcache.jit".to_string(), "disable".to_string())),
            "multi-tenant serve must emit opcache.jit=disable, got: {lines:?}"
        );
    }

    #[test]
    fn test_jit_shaped_default_worker_mode_is_disable() {
        let cfg = PhpConfig { mode: "worker".to_string(), ..PhpConfig::default() };
        let at = cfg.autotune(false, false);
        assert_eq!(at.jit.value, JitMode::Disable);
        assert_eq!(at.jit_reason, JitReason::WorkerMode);
        assert!(
            at.ini_lines().contains(&("opcache.jit".to_string(), "disable".to_string())),
            "worker mode must emit opcache.jit=disable"
        );
    }

    #[test]
    fn test_jit_shaped_default_dev_mode_emits_nothing() {
        // Dev keeps the generated ini minimal: no opcache.jit line at all —
        // PHP's own defaults keep the JIT off (8.3: buffer 0; 8.4+: disable).
        let at = PhpConfig::default().autotune(true, false);
        assert_eq!(at.jit.value, JitMode::Disable);
        assert_eq!(at.jit.origin, Origin::Default);
        assert_eq!(at.jit_reason, JitReason::Dev);
        let lines = at.ini_lines();
        let keys: Vec<&str> = lines.iter().map(|(k, _)| k.as_str()).collect();
        assert!(!keys.contains(&"opcache.jit"), "dev mode must not emit opcache.jit");
        assert!(!keys.contains(&"opcache.jit_buffer_size"));
    }

    #[test]
    fn test_jit_explicit_overrides_shaped_default() {
        // Explicit disable in single-site serve (the "JIT miscompile" escape
        // hatch — must always win).
        let off = PhpConfig { opcache_jit: Some(JitMode::Disable), ..PhpConfig::default() };
        let at = off.autotune(false, false);
        assert_eq!(at.jit.value, JitMode::Disable);
        assert_eq!(at.jit.origin, Origin::Explicit);
        assert_eq!(at.jit_reason, JitReason::Explicit);
        assert!(at.ini_lines().contains(&("opcache.jit".to_string(), "disable".to_string())));

        // Explicit tracing in multi-tenant: operator's documented-cost choice —
        // applied, but warned about. The tracing-JIT crash (#365) is the louder
        // hazard and is reported first on **every** platform — a dead process
        // beats a leaking buffer. Not platform-shaped: `jit_warning` checks the
        // tracing hazard before the multi-tenant one with no `cfg!` involved.
        let on = PhpConfig { opcache_jit: Some(JitMode::Tracing), ..PhpConfig::default() };
        let at = on.autotune(false, true);
        assert_eq!(at.jit.value, JitMode::Tracing);
        assert_eq!(at.jit.origin, Origin::Explicit);
        assert!(at.ini_lines().contains(&("opcache.jit".to_string(), "tracing".to_string())));
        let warning = at.jit_warning().expect("explicit JIT in multi-tenant mode must warn");
        assert!(warning.contains("21710"), "got: {warning}");

        // Explicit *function* mode in multi-tenant: never builds traces, so the
        // crash hazard does not apply and the buffer-exhaustion warning is the
        // one that surfaces. This keeps that branch covered — it used to be
        // reachable only through the non-Windows half of a `cfg!(windows)`.
        let on_fn = PhpConfig { opcache_jit: Some(JitMode::Function), ..PhpConfig::default() };
        let at = on_fn.autotune(false, true);
        assert_eq!(at.jit.value, JitMode::Function);
        let warning = at.jit_warning().expect("explicit JIT in multi-tenant mode must warn");
        assert!(warning.contains("reclaim"), "got: {warning}");

        // Explicit function mode in worker mode: applied, no warning.
        let func = PhpConfig {
            opcache_jit: Some(JitMode::Function),
            mode: "worker".to_string(),
            ..PhpConfig::default()
        };
        let at = func.autotune(false, false);
        assert_eq!(at.jit.value, JitMode::Function);
        assert!(at.jit_warning().is_none());
        assert!(at.ini_lines().contains(&("opcache.jit".to_string(), "function".to_string())));
    }

    #[test]
    fn test_jit_explicit_in_dev_mode_forces_a_buffer() {
        // Dev derives no jit_buffer_size and the bottom tier is 0 (PHP <=8.3's
        // stock default) — an explicit "tracing" must still get a buffer or it
        // silently does nothing on 8.3.
        let cfg = PhpConfig { opcache_jit: Some(JitMode::Tracing), ..PhpConfig::default() };
        let at = cfg.autotune(true, false);
        assert_eq!(at.jit.value, JitMode::Tracing);
        assert_eq!(at.jit_buffer_size.origin, Origin::Derived);
        assert!(at.jit_buffer_size.value >= 32);
        let lines = at.ini_lines();
        assert!(lines.contains(&("opcache.jit".to_string(), "tracing".to_string())));
        assert!(
            lines.iter().any(|(k, _)| k == "opcache.jit_buffer_size"),
            "explicit JIT in dev must emit a jit_buffer_size, got: {lines:?}"
        );
    }

    #[test]
    fn test_jit_warns_on_explicitly_zero_buffer() {
        let cfg = PhpConfig {
            opcache_jit: Some(JitMode::Tracing),
            opcache_jit_buffer_size: Some(0),
            ..PhpConfig::default()
        };
        let at = cfg.autotune(false, false);
        // Explicit 0 is respected (never overridden)…
        assert_eq!(at.jit_buffer_size.value, 0);
        assert_eq!(at.jit_buffer_size.origin, Origin::Explicit);
        // …but it means the JIT can never compile, which must warn.
        let warning = at.jit_warning().expect("jit on + zero buffer must warn");
        assert!(warning.contains("buffer"), "got: {warning}");
    }

    #[test]
    fn test_jit_line_states_the_why() {
        let single = PhpConfig::default().autotune(false, false);
        assert!(single.jit_line().contains("default since #365"), "{}", single.jit_line());

        let multi = PhpConfig::default().autotune(false, true);
        assert!(multi.jit_line().contains("multi-tenant default"), "{}", multi.jit_line());

        let worker =
            PhpConfig { mode: "worker".to_string(), ..PhpConfig::default() }.autotune(false, false);
        assert!(worker.jit_line().contains("worker-mode default"), "{}", worker.jit_line());

        let dev = PhpConfig::default().autotune(true, false);
        assert!(dev.jit_line().contains("dev mode default"), "{}", dev.jit_line());

        let explicit = PhpConfig { opcache_jit: Some(JitMode::Tracing), ..PhpConfig::default() }
            .autotune(false, false);
        assert!(explicit.jit_line().contains("explicitly"), "{}", explicit.jit_line());
    }

    #[test]
    fn test_jit_summary_line_shows_mode() {
        // Single-site serve: `disable` on every platform (#365).
        let line = PhpConfig::default().autotune(false, false).summary_line();
        assert!(line.contains("(jit=disable)"), "got: {line}");
        let line = PhpConfig::default().autotune(false, true).summary_line();
        assert!(line.contains("(jit=disable)"), "got: {line}");
        // Explicit values carry the `*` pin marker like every other tunable.
        let line = PhpConfig { opcache_jit: Some(JitMode::Disable), ..PhpConfig::default() }
            .autotune(false, false)
            .summary_line();
        assert!(line.contains("(jit=disable*)"), "got: {line}");
        // Dev, knob absent: no line emitted — PHP defaults.
        let line = PhpConfig::default().autotune(true, false).summary_line();
        assert!(line.contains("(jit=off (php default))"), "got: {line}");
    }

    #[test]
    fn test_jit_loads_from_toml_and_rejects_unknown_values() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[php]\nopcache_jit = \"tracing\"\n").unwrap();
        let config = Config::load(&file).unwrap();
        assert_eq!(config.php.opcache_jit, Some(JitMode::Tracing));

        std::fs::write(&file, "[php]\nopcache_jit = \"function\"\n").unwrap();
        assert_eq!(Config::load(&file).unwrap().php.opcache_jit, Some(JitMode::Function));

        std::fs::write(&file, "[php]\nopcache_jit = \"disable\"\n").unwrap();
        assert_eq!(Config::load(&file).unwrap().php.opcache_jit, Some(JitMode::Disable));

        // PHP's raw CRTO syntax (and typos) are a hard config error, not a
        // silent fallback.
        std::fs::write(&file, "[php]\nopcache_jit = \"1254\"\n").unwrap();
        assert!(Config::load(&file).is_err(), "unknown opcache_jit value must be rejected");
    }

    #[test]
    fn test_jit_env_override() {
        let _env = test_env::EnvVars::set("EPHPM_PHP__OPCACHE_JIT", "disable");
        let config = Config::default_config().unwrap();
        assert_eq!(config.php.opcache_jit, Some(JitMode::Disable));
    }

    #[test]
    fn test_jit_default_survives_section_absent_and_present() {
        // The `[server.security]` lesson: a section-level serde default must
        // not zero the field default. Absent `[php]` section…
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(&file, "[server]\nlisten = \"127.0.0.1:0\"\n").unwrap();
        let absent = Config::load(&file).unwrap();
        assert_eq!(absent.php.opcache_jit, None);

        // …and `[php]` present without the knob must resolve identically.
        std::fs::write(&file, "[server]\nlisten = \"127.0.0.1:0\"\n[php]\nworkers = 2\n").unwrap();
        let present = Config::load(&file).unwrap();
        assert_eq!(present.php.opcache_jit, None);

        // Same shaped default either way, and it is no longer platform-shaped:
        // `disable` everywhere (issue #365 — the upstream tracing-JIT
        // side-trace defect reproduces on Linux too, see
        // `JitReason::TracingJitBug`). What this test pins is that the
        // section's presence cannot change it.
        let shaped = JitMode::Disable;
        assert_eq!(absent.php.autotune(false, false).jit.value, shaped);
        assert_eq!(present.php.autotune(false, false).jit.value, shaped);
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

    // ── KV RESP listener fail-closed (multi-tenant needs [kv] secret) ──
    //
    // Trigger: sites_dir set (multi-tenant) AND [kv.redis_compat] enabled
    // AND no usable [kv] secret. Only that exact combination must refuse to
    // start — every other combination keeps working.

    /// Build a `Config` for the KV fail-closed matrix.
    fn kv_matrix_config(multi_tenant: bool, resp_enabled: bool, secret: Option<&str>) -> Config {
        let mut cfg = Config::default();
        if multi_tenant {
            cfg.server.sites_dir = Some(PathBuf::from("/var/www/sites"));
        }
        cfg.kv.redis_compat.enabled = resp_enabled;
        cfg.kv.secret = secret.map(str::to_string);
        cfg
    }

    #[test]
    fn kv_multi_tenant_resp_without_secret_fails_closed() {
        // The vuln: multi-tenant + RESP listener + no secret would serve a
        // shared global store to all tenants. Must refuse to start.
        let cfg = kv_matrix_config(true, true, None);
        let err = cfg.validate().expect_err("must fail closed");
        assert!(matches!(err, ConfigError::Validation(_)));
        let msg = err.to_string();
        assert!(msg.contains("secret"), "error should point at [kv] secret: {msg}");
        assert!(msg.contains("sites_dir"), "error should mention multi-tenant: {msg}");
    }

    #[test]
    fn kv_multi_tenant_resp_empty_secret_fails_closed() {
        // An empty / whitespace-only secret cannot derive per-site AUTH and
        // must be treated as unset (fail closed), not accepted.
        for secret in ["", "   ", "\t"] {
            let cfg = kv_matrix_config(true, true, Some(secret));
            let err = cfg.validate().expect_err("empty secret must fail closed like an unset one");
            assert!(matches!(err, ConfigError::Validation(_)));
        }
    }

    #[test]
    fn kv_multi_tenant_resp_with_secret_starts() {
        // The secure config: per-site AUTH scoping is derivable — must start.
        let cfg = kv_matrix_config(true, true, Some("s3cret-value"));
        assert!(cfg.validate().is_ok(), "multi-tenant + secret is the secure mode");
    }

    #[test]
    fn kv_single_tenant_resp_without_secret_starts() {
        // Single-site: the shared store IS the correct behavior. Unaffected.
        let cfg = kv_matrix_config(false, true, None);
        assert!(cfg.validate().is_ok(), "single-tenant RESP must keep working");
    }

    #[test]
    fn kv_multi_tenant_resp_disabled_without_secret_starts() {
        // Multi-tenant but the RESP listener is off — no exposure, no secret
        // needed (PHP uses the per-vhost ephpm_kv_* functions).
        let cfg = kv_matrix_config(true, false, None);
        assert!(cfg.validate().is_ok(), "no RESP listener means no shared-store exposure");
    }

    #[test]
    fn kv_fail_closed_survives_absent_kv_section() {
        // Section-absent guard (the [server.security] serde-default lesson):
        // sites_dir + [kv.redis_compat] enabled with NO [kv] secret key and no
        // explicit secret must still fail closed — the serde default (None)
        // must not mask the exposure.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[server]
sites_dir = "/var/www/sites"

[kv.redis_compat]
enabled = true
"#,
        )
        .unwrap();
        let cfg = Config::load(&file).unwrap();
        assert!(cfg.kv.secret.is_none(), "no secret key present");
        let err = cfg.validate().expect_err("absent [kv] secret must still fail closed");
        assert!(matches!(err, ConfigError::Validation(_)));
    }

    #[test]
    fn kv_fail_closed_secret_present_in_toml_starts() {
        // Section-present counterpart: same multi-tenant + RESP config but with
        // the secret set in TOML must validate.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            r#"
[server]
sites_dir = "/var/www/sites"

[kv]
secret = "openssl-rand-base64-32-here"

[kv.redis_compat]
enabled = true
"#,
        )
        .unwrap();
        let cfg = Config::load(&file).unwrap();
        assert!(cfg.kv.secret_is_set());
        cfg.validate().expect("multi-tenant + RESP + secret is the secure config");
    }

    #[test]
    fn kv_secret_is_set_treats_blank_as_unset() {
        let mut kv = KvConfig::default();
        assert!(!kv.secret_is_set(), "None is unset");
        kv.secret = Some(String::new());
        assert!(!kv.secret_is_set(), "empty string is unset");
        kv.secret = Some("   ".to_string());
        assert!(!kv.secret_is_set(), "whitespace is unset");
        kv.secret = Some("real".to_string());
        assert!(kv.secret_is_set(), "non-blank is set");
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

        let _env = EnvVars::set("EPHPM_DB__ANALYSIS__METRIC_LABEL_SERIES_MAX", "5000");
        let config = Config::load(&file).unwrap();
        assert_eq!(config.db.analysis.metric_label_series_max, 5000);
    }

    // ── [server.tenant_network] eBPF per-vhost network policy ──────────

    #[test]
    fn test_tenant_network_defaults() {
        let config = Config::default_config().unwrap();
        let tn = &config.server.tenant_network;
        assert!(!tn.ebpf_policy, "eBPF policy must be OFF by default (zero cost)");
        assert_eq!(tn.sidecar_port_range, "20000-32767");
        assert_eq!(tn.max_sidecar_ports_per_vhost, 8);
        assert_eq!(tn.parse_range().unwrap(), (20000, 32767));
        // Default sidecar range sits BELOW the default kernel ephemeral floor
        // (32768) — the non-overlap invariant serve() enforces against /proc.
        assert!(tn.parse_range().unwrap().1 < 32768);
    }

    #[test]
    fn test_tenant_network_parse_range() {
        let mk = |s: &str| TenantNetworkConfig {
            sidecar_port_range: s.to_string(),
            ..Default::default()
        };
        assert_eq!(mk("20000-32767").parse_range().unwrap(), (20000, 32767));
        assert_eq!(mk(" 1000 - 2000 ").parse_range().unwrap(), (1000, 2000));
        assert!(mk("20000").parse_range().is_err(), "missing dash");
        assert!(mk("0-100").parse_range().is_err(), "low port 0 is invalid");
        assert!(mk("3000-2000").parse_range().is_err(), "low > high");
        assert!(mk("abc-2000").parse_range().is_err(), "non-numeric low");
        assert!(mk("1000-zzz").parse_range().is_err(), "non-numeric high");
        assert!(mk("1000-99999").parse_range().is_err(), "high exceeds u16::MAX");
    }

    // On a non-Linux host the platform gate fires first and unconditionally,
    // so ebpf_policy = true is always a hard error here (never a silent no-op).
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn test_ebpf_policy_rejected_off_linux() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            "[server]\nsites_dir = \"/var/www/sites\"\n\n[server.tenant_network]\nebpf_policy = true\n",
        )
        .unwrap();
        let err = Config::load(&file).unwrap().validate().expect_err("ebpf is Linux-only");
        assert!(matches!(err, ConfigError::Validation(m) if m.contains("Linux-only")));
    }

    // On Linux the platform gate passes, so the fail-closed misconfiguration
    // checks become reachable.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_ebpf_policy_requires_sites_dir() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            "[server]\nlisten = \"0.0.0.0:8080\"\n\n[server.tenant_network]\nebpf_policy = true\n",
        )
        .unwrap();
        let err = Config::load(&file).unwrap().validate().expect_err("ebpf needs sites_dir");
        assert!(matches!(err, ConfigError::Validation(m) if m.contains("sites_dir")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_ebpf_policy_rejects_worker_mode() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            "[server]\nsites_dir = \"/var/www/sites\"\n\n[php]\nmode = \"worker\"\n\n[server.tenant_network]\nebpf_policy = true\n",
        )
        .unwrap();
        let err =
            Config::load(&file).unwrap().validate().expect_err("worker unsupported in v0.8.1");
        assert!(matches!(err, ConfigError::Validation(m) if m.contains("worker")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_ebpf_policy_rejects_bad_range() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ephpm.toml");
        std::fs::write(
            &file,
            "[server]\nsites_dir = \"/var/www/sites\"\n\n[server.tenant_network]\nebpf_policy = true\nsidecar_port_range = \"nope\"\n",
        )
        .unwrap();
        let err = Config::load(&file).unwrap().validate().expect_err("malformed range");
        assert!(matches!(err, ConfigError::Validation(m) if m.contains("sidecar_port_range")));
    }
}
