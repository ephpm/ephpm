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
use std::time::Duration;

use ephpm_config::Config;
use ephpm_kv::store::Store;
use ephpm_php::PhpRuntime;
use ephpm_php::request::PhpRequest;
use flate2::Compression;
use flate2::write::GzEncoder;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response, StatusCode};
use ipnet::IpNet;

use crate::body::{self, ServerBody};
use crate::{metrics, static_files};

#[allow(unused_imports)]
use ::metrics::{counter, gauge, histogram};

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
}

/// Per-site configuration resolved at startup from `sites_dir`.
struct SiteConfig {
    document_root: PathBuf,
    index_files: Vec<String>,
    fallback: Vec<String>,
}

pub struct Router {
    document_root: PathBuf,
    sites: HashMap<String, SiteConfig>,
    /// Optional path to the sites directory for lazy vhost discovery.
    /// When set, unknown hosts are checked against the filesystem.
    sites_dir: Option<PathBuf>,
    index_files: Vec<String>,
    fallback: Vec<String>,
    server_port: u16,
    max_body_size: u64,
    compression: CompressionSettings,
    hidden_files: String,
    cache_control: String,
    etag: bool,
    request_timeout: Duration,
    trusted_proxies: Vec<IpNet>,
    blocked_paths: Vec<String>,
    allowed_php_paths: Vec<String>,
    trusted_hosts: Vec<String>,
    response_headers: Vec<(String, String)>,
    store: Arc<Store>,
    multi_tenant_kv: Option<ephpm_kv::multi_tenant::MultiTenantStore>,
    open_basedir: bool,
    disable_shell_exec: bool,
    php_etag_cache_config: ephpm_config::PhpETagCacheConfig,
    metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
    metrics_path: String,
    limiter: Option<Arc<crate::rate_limit::Limiter>>,
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
    db_env_vars: Vec<(String, String)>,
}

/// Scan `sites_dir` for virtual host subdirectories.
///
/// Each subdirectory becomes a virtual host keyed by its name (lowercased).
/// Returns an empty map if `sites_dir` is `None`.
fn scan_sites_dir(
    sites_dir: Option<&Path>,
    default_index_files: &[String],
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
                document_root: path,
                index_files: default_index_files.to_vec(),
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
    #[must_use]
    pub fn new(
        config: &Config,
        store: Arc<Store>,
        metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
        limiter: Option<Arc<crate::rate_limit::Limiter>>,
        file_cache: Option<Arc<crate::file_cache::FileCache>>,
    ) -> Self {
        let port =
            config.server.listen.rsplit_once(':').and_then(|(_, p)| p.parse().ok()).unwrap_or(8080);

        let trusted_proxies: Vec<IpNet> = config
            .server
            .security
            .trusted_proxies
            .iter()
            .filter_map(|cidr| {
                cidr.parse::<IpNet>()
                    .map_err(|e| tracing::warn!(cidr, %e, "ignoring invalid trusted_proxy"))
                    .ok()
            })
            .collect();

        // Scan sites_dir for virtual host directories.
        let sites = scan_sites_dir(
            config.server.sites_dir.as_deref(),
            &config.server.index_files,
            &config.server.fallback,
        );

        Self {
            document_root: config.server.document_root.clone(),
            sites,
            sites_dir: config.server.sites_dir.clone(),
            index_files: config.server.index_files.clone(),
            fallback: config.server.fallback.clone(),
            server_port: port,
            max_body_size: config.server.request.max_body_size,
            compression: CompressionSettings {
                enabled: config.server.response.compression,
                level: config.server.response.compression_level,
                min_size: config.server.response.compression_min_size,
            },
            hidden_files: config.server.static_files.hidden_files.clone(),
            cache_control: config.server.static_files.cache_control.clone(),
            etag: config.server.static_files.etag,
            request_timeout: Duration::from_secs(config.server.timeouts.request),
            trusted_proxies,
            blocked_paths: config.server.security.blocked_paths.clone(),
            allowed_php_paths: config.server.security.allowed_php_paths.clone(),
            trusted_hosts: config.server.request.trusted_hosts.clone(),
            response_headers: config
                .server
                .response
                .headers
                .iter()
                .map(|[k, v]| (k.clone(), v.clone()))
                .collect(),
            open_basedir: config.server.security.open_basedir,
            disable_shell_exec: config.server.security.disable_shell_exec,
            multi_tenant_kv: if config.server.sites_dir.is_some() {
                Some(ephpm_kv::multi_tenant::MultiTenantStore::new(
                    Arc::clone(&store),
                    ephpm_kv::store::StoreConfig::default(),
                ))
            } else {
                None
            },
            store,
            php_etag_cache_config: config.server.php_etag_cache.clone(),
            metrics_handle,
            metrics_path: config.server.metrics.path.clone(),
            limiter,
            file_cache,
            kv_secret: config.kv.secret.clone(),
            kv_listen: config.kv.redis_compat.listen.clone(),
            kv_redis_compat_enabled: config.kv.redis_compat.enabled,
            db_env_vars: build_db_env_vars(config),
        }
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

        let password = ephpm_kv::auth::derive_site_password(secret, hostname);

        // Parse host:port from the listen address.
        let (host, port) = self
            .kv_listen
            .rsplit_once(':')
            .unwrap_or(("127.0.0.1", "6379"));

        vec![
            ("EPHPM_REDIS_HOST".into(), host.into()),
            ("EPHPM_REDIS_PORT".into(), port.into()),
            ("EPHPM_REDIS_USERNAME".into(), hostname.into()),
            ("EPHPM_REDIS_PASSWORD".into(), password),
        ]
    }

    fn resolve_site(&self, host: &str) -> (PathBuf, &[String], &[String]) {
        if self.sites_dir.is_none() && self.sites.is_empty() {
            return (self.document_root.clone(), &self.index_files, &self.fallback);
        }

        // Strip port and trailing dot, lowercase.
        let clean = host
            .split(':')
            .next()
            .unwrap_or("")
            .trim_end_matches('.')
            .to_ascii_lowercase();

        // Check the startup-scanned registry first.
        // Verify the directory still exists — it may have been removed (teardown).
        if let Some(site) = self.sites.get(&clean) {
            if site.document_root.is_dir() {
                return (site.document_root.clone(), &site.index_files, &site.fallback);
            }
        }

        // Lazy filesystem check: if sites_dir is set and the directory exists,
        // serve from it. No restart needed — new sites are discovered on demand.
        if let Some(ref sites_dir) = self.sites_dir {
            let candidate = sites_dir.join(&clean);
            if candidate.is_dir() {
                tracing::info!(host = %clean, path = %candidate.display(), "discovered new virtual host (lazy)");
                return (candidate, &self.index_files, &self.fallback);
            }
        }

        (self.document_root.clone(), &self.index_files, &self.fallback)
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
    pub async fn handle(
        &self,
        req: Request<Incoming>,
        remote_addr: SocketAddr,
        is_tls: bool,
    ) -> Result<Response<ServerBody>, hyper::Error> {
        let method = req.method().as_str().to_ascii_uppercase();

        // Per-IP rate limiting (uses effective IP after proxy resolution).
        if let Some(ref limiter) = self.limiter {
            let (effective_addr, _) = self.resolve_proxy_info(&req, remote_addr, is_tls);
            if !limiter.check_rate(effective_addr.ip()) {
                counter!("ephpm_rate_limited_total").increment(1);
                return Ok(error_response(
                    StatusCode::TOO_MANY_REQUESTS,
                    "429 Too Many Requests",
                ));
            }
        }

        gauge!("ephpm_http_requests_in_flight").increment(1.0);
        let start = std::time::Instant::now();

        let (result, handler) = if let Ok(result) = tokio::time::timeout(
            self.request_timeout,
            self.handle_inner(req, remote_addr, is_tls),
        )
        .await
        {
            let handler = result.as_ref().map_or("error", |(_, h)| *h);
            (result.map(|(resp, _)| resp), handler)
        } else {
            counter!("ephpm_http_timeouts_total", "stage" => "request").increment(1);
            (Ok(error_response(StatusCode::GATEWAY_TIMEOUT, "504 Gateway Timeout")), "error")
        };

        let elapsed = start.elapsed().as_secs_f64();
        gauge!("ephpm_http_requests_in_flight").decrement(1.0);
        if let Ok(ref resp) = result {
            let status = resp.status().as_u16().to_string();
            counter!("ephpm_http_requests_total",
                "method" => method.clone(),
                "status" => status,
                "handler" => handler
            )
            .increment(1);
            histogram!("ephpm_http_request_duration_seconds",
                "method" => method,
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
    async fn handle_inner(
        &self,
        req: Request<Incoming>,
        remote_addr: SocketAddr,
        is_tls: bool,
    ) -> Result<(Response<ServerBody>, &'static str), hyper::Error> {
        // Validate Host header against trusted hosts list.
        if let Some(resp) = self.check_trusted_host(&req) {
            return Ok((resp, "error"));
        }

        let uri_path = req.uri().path().to_string();
        let query_string = req.uri().query().unwrap_or("").to_string();
        let method = req.method().as_str().to_ascii_uppercase();

        // Internal ePHPm endpoints — served before security checks since
        // they are not user-supplied content.
        if method == "GET" {
            if let Some(ref handle) = self.metrics_handle {
                if uri_path == self.metrics_path {
                    return Ok((metrics::render(handle), "metrics"));
                }
            }

            // Liveness probe — always 200 if the server is running.
            if uri_path == "/_ephpm/health" {
                return Ok((json_response(StatusCode::OK, r#"{"status":"ok"}"#), "health"));
            }

            // Readiness probe — checks PHP initialization and DB proxy.
            if uri_path == "/_ephpm/ready" {
                return Ok((self.readiness_check(), "health"));
            }
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

        // Block explicitly forbidden paths
        if is_path_blocked(&uri_path, &self.blocked_paths) {
            return Ok((error_response(StatusCode::FORBIDDEN, "403 Forbidden"), "error"));
        }

        // Resolve real client IP and HTTPS status from trusted proxy headers
        let (effective_addr, is_https) = self.resolve_proxy_info(&req, remote_addr, is_tls);

        let accepts_gzip = self.compression.enabled && accepts_encoding(&req, "gzip");

        // Resolve virtual host — determines document root, index files, fallback.
        let host = extract_server_name(&req);
        let (site_root, site_index, site_fallback) = self.resolve_site(&host);

        // Extract If-None-Match for ETag support before consuming the request.
        let if_none_match = if self.etag {
            req.headers()
                .get("if-none-match")
                .and_then(|v| v.to_str().ok())
                .map(String::from)
        } else {
            None
        };

        let (mut response, handler) = match self.resolve_fallback(&uri_path, &query_string, &site_root, site_index, site_fallback) {
            Resolved::File(fs_path) => {
                if is_php_file(&fs_path) {
                    if self.is_php_allowed(&uri_path) {
                        let is_cacheable = (method == "GET" || method == "HEAD") && self.php_etag_cache_config.enabled;

                        // Pre-check: bypass PHP if client's ETag matches stored value.
                        if is_cacheable {
                            if let Some(client_tag) = &if_none_match {
                                let key = php_etag_cache_key(&self.php_etag_cache_config.key_prefix, &method, &uri_path, &query_string);
                                if let Some(stored) = self.store.get(&key) {
                                    let stored_etag = String::from_utf8_lossy(&stored);
                                    if etag_matches_value(&stored_etag, client_tag) {
                                        return Ok((Response::builder()
                                            .status(StatusCode::NOT_MODIFIED)
                                            .header("etag", stored_etag.as_ref())
                                            .body(body::buffered(Full::new(Bytes::new())))
                                            .expect("304 builder"), "php"));
                                    }
                                }
                            }
                        }

                        // Execute PHP
                        let resp = self.handle_php(req, effective_addr, is_https, fs_path, accepts_gzip, site_root.clone())
                            .await;

                        // Post-store: cache any ETag PHP set in the response.
                        if is_cacheable {
                            if let Some(etag_val) = resp.headers().get("etag").and_then(|v| v.to_str().ok()) {
                                let key = php_etag_cache_key(&self.php_etag_cache_config.key_prefix, &method, &uri_path, &query_string);
                                #[allow(clippy::cast_sign_loss)]
                                let ttl = if self.php_etag_cache_config.ttl_secs > 0 {
                                    Some(Duration::from_secs(self.php_etag_cache_config.ttl_secs as u64))
                                } else {
                                    None
                                };
                                self.store.set(key, etag_val.as_bytes().to_vec(), ttl);
                            }
                        }

                        (resp, "php")
                    } else {
                        (error_response(StatusCode::FORBIDDEN, "403 Forbidden"), "error")
                    }
                } else {
                    (static_files::serve_file(
                        &site_root,
                        &fs_path,
                        accepts_gzip,
                        &self.cache_control,
                        self.compression,
                        self.etag,
                        if_none_match.as_deref(),
                        self.file_cache.as_deref(),
                    )
                    .await, "static")
                }
            }
            Resolved::Status(code) => {
                let status = StatusCode::from_u16(code).unwrap_or(StatusCode::NOT_FOUND);
                (error_response(status, &format!("{code} {}", status.canonical_reason().unwrap_or("Error"))), "error")
            }
        };

        // Apply custom response headers to all responses.
        self.apply_response_headers(&mut response);

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
    fn probe_path(&self, expanded: &str, doc_root: &Path, index_files: &[String]) -> Option<PathBuf> {
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
            if fs_path.is_file() {
                Some(fs_path)
            } else {
                None
            }
        }
    }

    /// Handle a PHP request by executing it in a blocking task.
    async fn handle_php(
        &self,
        req: Request<Incoming>,
        remote_addr: SocketAddr,
        is_https: bool,
        script_filename: PathBuf,
        accepts_gzip: bool,
        document_root: PathBuf,
    ) -> Response<ServerBody> {
        let method = req.method().to_string();
        let uri = req.uri().to_string();
        let path = req.uri().path().to_string();
        let query_string = req.uri().query().unwrap_or("").to_string();
        let protocol = format!("{:?}", req.version());
        let headers = extract_headers(&req);
        let content_type =
            req.headers().get("content-type").and_then(|v| v.to_str().ok()).map(String::from);
        let server_name = extract_server_name(&req);

        // Reject oversized request bodies before reading
        if let Some(resp) = self.check_body_size(&req) {
            return resp;
        }

        let body = match req.collect().await {
            Ok(collected) => collected.to_bytes().to_vec(),
            Err(_) => Vec::new(),
        };
        #[allow(clippy::cast_precision_loss)]
        histogram!("ephpm_http_request_body_bytes", "method" => method.clone())
            .record(body.len() as f64);

        let server_port = self.server_port;
        let multi_tenant_kv = self.multi_tenant_kv.clone();
        let vhost_open_basedir = self.sites_dir.is_some() && self.open_basedir;
        let vhost_disable_shell = self.sites_dir.is_some() && self.disable_shell_exec;

        // Build EPHPM_REDIS_* env vars for multi-tenant RESP auth injection,
        // plus DB_* env vars for framework auto-discovery.
        let mut env_vars = self.build_kv_env_vars(&server_name);
        env_vars.extend_from_slice(&self.db_env_vars);

        let php_start = std::time::Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            // Scope KV store to this virtual host for multi-tenant isolation.
            ephpm_php::kv_bridge::set_site_store(
                multi_tenant_kv.as_ref().map(|mt| mt.get_site_store(&server_name))
            );

            // Apply per-request PHP sandbox for multi-tenant isolation.
            if vhost_open_basedir {
                let basedir = format!("{}:/tmp", document_root.display());
                PhpRuntime::set_request_ini("open_basedir", &basedir);
            }
            if vhost_disable_shell {
                PhpRuntime::set_request_ini(
                    "disable_functions",
                    "exec,passthru,shell_exec,system,proc_open,popen,pcntl_exec",
                );
            }

            PhpRuntime::execute(PhpRequest {
                method, uri, path, query_string, script_filename,
                document_root, headers, body, content_type, remote_addr,
                server_name, server_port, is_https, protocol, env_vars,
            })
        })
        .await;
        let php_elapsed = php_start.elapsed().as_secs_f64();

        histogram!("ephpm_php_execution_duration_seconds").record(php_elapsed);
        let exec_status = match &result {
            Ok(Ok(_)) => "ok",
            Ok(Err(_)) | Err(_) => "error",
        };
        counter!("ephpm_php_executions_total", "status" => exec_status).increment(1);

        build_php_response(result, accepts_gzip, self.compression)
    }

    /// Return 413 if Content-Length exceeds the limit.
    fn check_body_size(&self, req: &Request<Incoming>) -> Option<Response<ServerBody>> {
        if self.max_body_size == 0 {
            return None;
        }
        let len: u64 = req.headers().get("content-length")
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
            Some(error_response(status, &format!("{} {}", status.as_u16(), status.canonical_reason().unwrap_or("Error"))))
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
        self.allowed_php_paths.iter().any(|pattern| glob_match(pattern, uri_path))
    }

    /// Resolve real client address and HTTPS status from proxy headers.
    ///
    /// When the request comes from a trusted proxy, reads `X-Forwarded-For`
    /// (rightmost untrusted IP) and `X-Forwarded-Proto` for HTTPS detection.
    fn resolve_proxy_info(
        &self,
        req: &Request<Incoming>,
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
    fn check_trusted_host(&self, req: &Request<Incoming>) -> Option<Response<ServerBody>> {
        if self.trusted_hosts.is_empty() {
            return None;
        }
        let host = req
            .headers()
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        // Compare with and without port.
        let host_no_port = host.split(':').next().unwrap_or(host);
        let is_trusted = self.trusted_hosts.iter().any(|trusted| {
            host.eq_ignore_ascii_case(trusted) || host_no_port.eq_ignore_ascii_case(trusted)
        });
        if is_trusted {
            None
        } else {
            tracing::debug!(host, "rejected untrusted host");
            Some(error_response(
                StatusCode::MISDIRECTED_REQUEST,
                "421 Misdirected Request",
            ))
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
        json_response(StatusCode::OK, r#"{"status":"ready"}"#)
    }

    /// Apply custom response headers from config.
    fn apply_response_headers(&self, response: &mut Response<ServerBody>) {
        let headers = response.headers_mut();
        for (name, value) in &self.response_headers {
            if let (Ok(name), Ok(value)) = (
                hyper::header::HeaderName::from_bytes(name.as_bytes()),
                hyper::header::HeaderValue::from_str(value),
            ) {
                headers.insert(name, value);
            }
        }
    }

    /// Check if an IP address matches any trusted proxy CIDR.
    fn is_trusted_proxy(&self, ip: IpAddr) -> bool {
        self.trusted_proxies.iter().any(|net| net.contains(&ip))
    }

    /// Walk X-Forwarded-For from right to left, return the first untrusted IP.
    fn resolve_xff(&self, xff: &str) -> Option<IpAddr> {
        let ips: Vec<&str> = xff.split(',').map(str::trim).collect();
        for ip_str in ips.iter().rev() {
            if let Ok(ip) = ip_str.parse::<IpAddr>() {
                if !self.is_trusted_proxy(ip) {
                    return Some(ip);
                }
            }
        }
        // All IPs in the chain are trusted — use the leftmost
        ips.first().and_then(|s| s.parse().ok())
    }
}

/// Check if a URI path contains a hidden (dot-prefixed) segment.
fn has_hidden_segment(uri_path: &str) -> bool {
    uri_path.split('/').any(|segment| {
        segment.starts_with('.') && !segment.is_empty() && segment != "." && segment != ".."
    })
}

/// Check if a URI path matches any blocked path pattern.
fn is_path_blocked(uri_path: &str, blocked: &[String]) -> bool {
    blocked.iter().any(|pattern| glob_match(pattern, uri_path))
}

/// Simple glob matching for URI paths.
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

fn extract_headers(req: &Request<Incoming>) -> Vec<(String, String)> {
    req.headers()
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_str().unwrap_or("").to_string()))
        .collect()
}

fn extract_server_name(req: &Request<Incoming>) -> String {
    req.headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost")
        .split(':')
        .next()
        .unwrap_or("localhost")
        .to_string()
}

/// Build a JSON response with the given status and body.
fn json_response(status: StatusCode, body: &str) -> Response<ServerBody> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(body::buffered(Full::new(Bytes::from(body.to_string()))))
        .expect("static json response")
}

/// Build database environment variables from config for PHP injection.
///
/// When a DB backend has `inject_env = true`, produces `DB_HOST`, `DB_PORT`,
/// `DB_NAME`, `DB_USER`, `DB_PASSWORD`, `DB_CONNECTION`, and `DATABASE_URL`
/// pointing at the proxy listener. PHP frameworks auto-discover these.
fn build_db_env_vars(config: &Config) -> Vec<(String, String)> {
    // MySQL takes precedence (most common for PHP).
    if let Some(ref mysql) = config.db.mysql {
        if mysql.inject_env {
            let listen = mysql
                .listen
                .as_deref()
                .unwrap_or("127.0.0.1:3306");
            return db_env_from_url(listen, &mysql.url, "mysql");
        }
    }
    if let Some(ref pg) = config.db.postgres {
        if pg.inject_env {
            let listen = pg
                .listen
                .as_deref()
                .unwrap_or("127.0.0.1:5432");
            return db_env_from_url(listen, &pg.url, "pgsql");
        }
    }
    Vec::new()
}

/// Parse a database URL and proxy listen address into env var pairs.
fn db_env_from_url(listen: &str, backend_url: &str, driver: &str) -> Vec<(String, String)> {
    let (host, port) = listen.rsplit_once(':').unwrap_or((listen, "3306"));

    // Parse: scheme://user:password@host:port/dbname
    let rest = backend_url
        .find("://")
        .map_or(backend_url, |i| &backend_url[i + 3..]);
    let (creds, host_db) = rest.split_once('@').unwrap_or(("", rest));
    let (user, password) = creds.split_once(':').unwrap_or((creds, ""));
    let db_name = host_db
        .split_once('/')
        .map_or("", |(_, db)| db)
        .split('?')
        .next()
        .unwrap_or("");

    vec![
        ("DB_HOST".into(), host.into()),
        ("DB_PORT".into(), port.into()),
        ("DB_NAME".into(), db_name.into()),
        ("DB_USER".into(), user.into()),
        ("DB_PASSWORD".into(), password.into()),
        ("DB_CONNECTION".into(), driver.into()),
        (
            "DATABASE_URL".into(),
            format!("{driver}://{user}:{password}@{host}:{port}/{db_name}"),
        ),
    ]
}

/// Build a simple error response with a text body.
fn error_response(status: StatusCode, body: &str) -> Response<ServerBody> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(body::buffered(Full::new(Bytes::from(body.to_string()))))
        .expect("static error response")
}

/// Build an HTTP response from a PHP execution result, optionally gzip-compressing.
fn build_php_response(
    result: Result<Result<ephpm_php::response::PhpResponse, ephpm_php::PhpError>, tokio::task::JoinError>,
    accepts_gzip: bool,
    compression: CompressionSettings,
) -> Response<ServerBody> {
    match result {
        Ok(Ok(php_response)) => {
            let status = StatusCode::from_u16(php_response.status).unwrap_or(StatusCode::OK);
            let ct = php_response.headers.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                .map_or("", |(_, v)| v.as_str());

            let original_len = php_response.body.len();
            #[allow(clippy::cast_precision_loss)]
            {
                histogram!("ephpm_http_response_body_bytes", "handler" => "php")
                    .record(original_len as f64);
                histogram!("ephpm_php_output_bytes").record(original_len as f64);
            }

            let (body_bytes, compressed) = if accepts_gzip {
                gzip_compress(&php_response.body, ct, compression)
                    .map_or((php_response.body, false), |c| (c, true))
            } else {
                (php_response.body, false)
            };

            if compressed && original_len > 0 {
                #[allow(clippy::cast_precision_loss)]
                histogram!("ephpm_http_compression_ratio")
                    .record(body_bytes.len() as f64 / original_len as f64);
            }

            let mut resp = Response::builder().status(status);
            for (name, value) in &php_response.headers {
                resp = resp.header(name.as_str(), value.as_str());
            }
            if compressed {
                resp = resp.header("content-encoding", "gzip").header("vary", "Accept-Encoding");
            }
            resp = resp.header("content-length", body_bytes.len());

            resp.body(body::buffered(Full::new(Bytes::from(body_bytes)))).unwrap_or_else(|_| {
                error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
            })
        }
        Ok(Err(err)) => {
            tracing::error!(%err, "PHP execution failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, &format!("PHP execution error: {err}"))
        }
        Err(err) => {
            tracing::error!(%err, "spawn_blocking task failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
        }
    }
}

/// Check if a filesystem path is a PHP file.
fn is_php_file(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("php"))
}

/// Expand `$uri` and `$query_string` variables in a `fallback` entry.
fn expand_variables(entry: &str, uri_path: &str, query_string: &str) -> String {
    entry.replace("$uri", uri_path).replace("$query_string", query_string)
}

/// Split an expanded path into the path component and optional query string.
fn split_path_query(expanded: &str) -> (&str, &str) {
    expanded.split_once('?').unwrap_or((expanded, ""))
}

/// Check if the request's Accept-Encoding header contains the given encoding.
fn accepts_encoding(req: &Request<Incoming>, encoding: &str) -> bool {
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
pub fn gzip_compress(data: &[u8], content_type: &str, settings: CompressionSettings) -> Option<Vec<u8>> {
    if data.len() < settings.min_size || !is_compressible(content_type) {
        return None;
    }
    let level = Compression::new(settings.level);
    let mut encoder = GzEncoder::new(Vec::new(), level);
    encoder.write_all(data).ok()?;
    let compressed = encoder.finish().ok()?;
    if compressed.len() < data.len() {
        Some(compressed)
    } else {
        None
    }
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
        };
        Router::new(&config, test_store(), None, None, None)
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
        };
        Router::new(&config, test_store(), None, None, None)
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
        };
        config.server.static_files.etag = true;
        Router::new(&config, store, None, None, None)
    }

    fn default_compression() -> CompressionSettings {
        CompressionSettings {
            enabled: true,
            level: 1,
            min_size: 1024,
        }
    }

    /// Test helper: call resolve_fallback with the router's own defaults.
    fn resolve_fb(router: &Router, uri: &str, qs: &str) -> Resolved {
        router.resolve_fallback(uri, qs, &router.document_root, &router.index_files, &router.fallback)
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
        let settings = CompressionSettings { enabled: true, level: 1, min_size: 4096 };
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
        };
        config.server.security.trusted_proxies = vec!["10.0.0.0/8".to_string()];
        let router = Router::new(&config, test_store(), None, None, None);

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
        };
        config.server.security.trusted_proxies = vec!["10.0.0.0/8".to_string()];
        let router = Router::new(&config, test_store(), None, None, None);

        let xff = "10.0.0.2, 10.0.0.1";
        let ip = router.resolve_xff(xff);
        assert_eq!(ip, Some("10.0.0.2".parse().unwrap()));
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
        };
        let router = Router::new(&config, test_store(), None, None, None);
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
        };
        let router = Router::new(&config, test_store(), None, None, None);
        assert_eq!(router.server_port, 8080);
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
        };
        let router = Router::new(&config, test_store(), None, None, None);
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
        };
        config.server.security.allowed_php_paths = vec![
            "/index.php".to_string(),
            "/wp-login.php".to_string(),
        ];
        let router = Router::new(&config, test_store(), None, None, None);
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
        };
        config.server.security.allowed_php_paths = vec![
            "/index.php".to_string(),
            "/wp-admin/*.php".to_string(),
        ];
        let router = Router::new(&config, test_store(), None, None, None);
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
        let blocked = vec![
            "/wp-config.php".to_string(),
            "/vendor/*".to_string(),
            "/.env".to_string(),
        ];
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
        };
        let router = Router::new(&config, test_store(), None, None, None);
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
        assert_eq!(stored.unwrap(), b"\"v1\"");
    }

    #[test]
    fn etag_store_overwrites_previous() {
        let store = test_store();
        let key = php_etag_cache_key("etag:", "GET", "/test.php", "");

        store.set(key.clone(), b"\"v1\"".to_vec(), None);
        store.set(key.clone(), b"\"v2\"".to_vec(), None);

        let stored = store.get(&key);
        assert_eq!(stored.unwrap(), b"\"v2\"");
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
        assert_eq!(stored.unwrap(), b"\"forever\"");
    }

    #[test]
    fn etag_cache_different_methods_different_keys() {
        let store = test_store();
        let get_key = php_etag_cache_key("etag:", "GET", "/page", "");
        let head_key = php_etag_cache_key("etag:", "HEAD", "/page", "");

        store.set(get_key.clone(), b"\"get-v1\"".to_vec(), None);
        store.set(head_key.clone(), b"\"head-v1\"".to_vec(), None);

        assert_eq!(store.get(&get_key).unwrap(), b"\"get-v1\"");
        assert_eq!(store.get(&head_key).unwrap(), b"\"head-v1\"");
    }

    #[test]
    fn etag_cache_different_paths_different_keys() {
        let store = test_store();
        let key_a = php_etag_cache_key("etag:", "GET", "/page-a", "");
        let key_b = php_etag_cache_key("etag:", "GET", "/page-b", "");

        store.set(key_a.clone(), b"\"a-v1\"".to_vec(), None);
        store.set(key_b.clone(), b"\"b-v1\"".to_vec(), None);

        assert_eq!(store.get(&key_a).unwrap(), b"\"a-v1\"");
        assert_eq!(store.get(&key_b).unwrap(), b"\"b-v1\"");
    }

    #[test]
    fn etag_cache_query_string_differentiates() {
        let store = test_store();
        let key_no_qs = php_etag_cache_key("etag:", "GET", "/api", "");
        let key_with_qs = php_etag_cache_key("etag:", "GET", "/api", "v=2");

        store.set(key_no_qs.clone(), b"\"no-qs\"".to_vec(), None);
        store.set(key_with_qs.clone(), b"\"with-qs\"".to_vec(), None);

        assert_eq!(store.get(&key_no_qs).unwrap(), b"\"no-qs\"");
        assert_eq!(store.get(&key_with_qs).unwrap(), b"\"with-qs\"");
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
        use hyper::body::Empty;
        use http_body_util::BodyExt;
        use ephpm_php::PhpRuntime;
        use serial_test::serial;

        use super::*;

        /// Helper to read response body bytes
        async fn body_bytes(resp: Response<ServerBody>) -> Vec<u8> {
            resp.into_body().collect().await.unwrap().to_bytes().to_vec()
        }

        /// Helper to create a test request
        fn make_request(method: &str, path: &str, if_none_match: Option<&str>) -> Request<Empty> {
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
        };
        let router = Router::new(&config, test_store(), None, None, None);

        let (doc_root, _, _) = router.resolve_site("example.com");
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
        };
        let router = Router::new(&config, test_store(), None, None, None);

        let (doc_root, _, _) = router.resolve_site("unknown.com");
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
        };
        let router = Router::new(&config, test_store(), None, None, None);

        let (doc_root, _, _) = router.resolve_site("example.com:8080");
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
        };
        let router = Router::new(&config, test_store(), None, None, None);

        let (doc_root, _, _) = router.resolve_site("Example.COM");
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
        };
        let router = Router::new(&config, test_store(), None, None, None);

        let (doc_root, _, _) = router.resolve_site("anything.com");
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
        };
        let router = Router::new(&config, test_store(), None, None, None);

        let (doc_root, index_files, fallback) = router.resolve_site("myblog.com");
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
        };
        let router = Router::new(&config, test_store(), None, None, None);

        // Host doesn't exist yet — should fall back to default.
        let (doc_root, _, _) = router.resolve_site("new-site.com");
        assert_eq!(doc_root, dir.path());

        // Create the directory AFTER router startup (simulates switchboard deploying).
        let new_site = sites.join("new-site.com");
        fs::create_dir_all(&new_site).unwrap();
        fs::write(new_site.join("index.html"), "<html>live!</html>").unwrap();

        // Now it should be discovered lazily.
        let (doc_root, _, _) = router.resolve_site("new-site.com");
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
        };
        let router = Router::new(&config, test_store(), None, None, None);

        // Site exists — should resolve.
        let (doc_root, _, _) = router.resolve_site("temp-site.com");
        assert_eq!(doc_root, site_dir);

        // Delete the directory (simulates switchboard tearing down).
        fs::remove_dir_all(&site_dir).unwrap();

        // Should fall back to default now.
        let (doc_root, _, _) = router.resolve_site("temp-site.com");
        assert_eq!(doc_root, dir.path());
    }
}
