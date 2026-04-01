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
    php_etag_cache_config: ephpm_config::PhpETagCacheConfig,
    metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
    metrics_path: String,
    limiter: Option<Arc<crate::rate_limit::Limiter>>,
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
            store,
            php_etag_cache_config: config.server.php_etag_cache.clone(),
            metrics_handle,
            metrics_path: config.server.metrics.path.clone(),
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

        // Metrics endpoint — served before security checks since it is an
        // internal ePHPm route, not user-supplied content.
        if method == "GET" {
            if let Some(ref handle) = self.metrics_handle {
                if uri_path == self.metrics_path {
                    return Ok((metrics::render(handle), "metrics"));
                }
            }
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

        let php_start = std::time::Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            // Scope KV keys to this virtual host for multi-tenant isolation.
            ephpm_php::kv_bridge::set_namespace(&server_name);
            PhpRuntime::execute(PhpRequest {
                method, uri, path, query_string, script_filename,
                document_root, headers, body, content_type, remote_addr,
                server_name, server_port, is_https, protocol,
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

    use ephpm_config::{Config, PhpConfig, ServerConfig};
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
            db: Default::default(),
            kv: Default::default(),
            cluster: Default::default(),
        };
        Router::new(&config, test_store(), None)
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
            db: Default::default(),
            kv: Default::default(),
            cluster: Default::default(),
        };
        Router::new(&config, test_store(), None)
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
            db: Default::default(),
            kv: Default::default(),
            cluster: Default::default(),
        };
        config.server.static_files.etag = true;
        Router::new(&config, store, None)
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
            db: Default::default(),
            kv: Default::default(),
            cluster: Default::default(),
        };
        config.server.security.trusted_proxies = vec!["10.0.0.0/8".to_string()];
        let router = Router::new(&config, test_store(), None);

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
            db: Default::default(),
            kv: Default::default(),
            cluster: Default::default(),
        };
        config.server.security.trusted_proxies = vec!["10.0.0.0/8".to_string()];
        let router = Router::new(&config, test_store(), None);

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
            db: Default::default(),
            kv: Default::default(),
            cluster: Default::default(),
        };
        let router = Router::new(&config, test_store(), None);
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
            db: Default::default(),
            kv: Default::default(),
            cluster: Default::default(),
        };
        let router = Router::new(&config, test_store(), None);
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
            db: Default::default(),
            kv: Default::default(),
            cluster: Default::default(),
        };
        let router = Router::new(&config, test_store(), None);
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
            db: Default::default(),
            kv: Default::default(),
            cluster: Default::default(),
        };
        config.server.security.allowed_php_paths = vec![
            "/index.php".to_string(),
            "/wp-login.php".to_string(),
        ];
        let router = Router::new(&config, test_store(), None);
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
            db: Default::default(),
            kv: Default::default(),
            cluster: Default::default(),
        };
        config.server.security.allowed_php_paths = vec![
            "/index.php".to_string(),
            "/wp-admin/*.php".to_string(),
        ];
        let router = Router::new(&config, test_store(), None);
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
            db: Default::default(),
            kv: Default::default(),
            cluster: Default::default(),
        };
        let router = Router::new(&config, test_store(), None);
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

    // ── PHP-linked ETag integration tests ────────────────────────────
    //
    // These tests require PHP to be linked. They verify that PHP-set
    // ETags are properly cached in the KV store and matched on
    // subsequent requests.
    //
    // Run with: cargo nextest run -p ephpm-server --run-ignored all

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
            db: Default::default(),
            kv: Default::default(),
            cluster: Default::default(),
        };
        let router = Router::new(&config, test_store(), None);

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
            db: Default::default(),
            kv: Default::default(),
            cluster: Default::default(),
        };
        let router = Router::new(&config, test_store(), None);

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
            db: Default::default(),
            kv: Default::default(),
            cluster: Default::default(),
        };
        let router = Router::new(&config, test_store(), None);

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
            db: Default::default(),
            kv: Default::default(),
            cluster: Default::default(),
        };
        let router = Router::new(&config, test_store(), None);

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
            db: Default::default(),
            kv: Default::default(),
            cluster: Default::default(),
        };
        let router = Router::new(&config, test_store(), None);

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
            db: Default::default(),
            kv: Default::default(),
            cluster: Default::default(),
        };
        let router = Router::new(&config, test_store(), None);

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
            db: Default::default(),
            kv: Default::default(),
            cluster: Default::default(),
        };
        let router = Router::new(&config, test_store(), None);

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
            db: Default::default(),
            kv: Default::default(),
            cluster: Default::default(),
        };
        let router = Router::new(&config, test_store(), None);

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
