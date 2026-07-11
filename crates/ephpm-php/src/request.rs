//! HTTP request to PHP request mapping.
//!
//! Converts an incoming HTTP request into the format expected by PHP's
//! embed SAPI, including populating `$_SERVER` variables.

use std::net::SocketAddr;
use std::path::PathBuf;

/// A PHP request, constructed from an incoming HTTP request.
///
/// Contains all the information needed to set up a PHP execution context
/// via the SAPI callbacks.
#[derive(Debug)]
pub struct PhpRequest {
    /// HTTP method (GET, POST, etc.)
    pub method: String,

    /// Full request URI including query string (e.g. "/wp-admin/index.php?page=1")
    pub uri: String,

    /// The path component of the URI (e.g. "/wp-admin/index.php")
    pub path: String,

    /// Query string without leading '?' (e.g. "page=1")
    pub query_string: String,

    /// Absolute path to the PHP script to execute.
    pub script_filename: PathBuf,

    /// Document root directory.
    pub document_root: PathBuf,

    /// Request headers as (name, value) pairs.
    pub headers: Vec<(String, String)>,

    /// POST body data.
    pub body: Vec<u8>,

    /// Content-Type header value.
    pub content_type: Option<String>,

    /// Remote client address.
    pub remote_addr: SocketAddr,

    /// Server name (from Host header).
    pub server_name: String,

    /// Server port.
    pub server_port: u16,

    /// Whether the request came over HTTPS.
    pub is_https: bool,

    /// HTTP protocol version string (e.g. "HTTP/1.1").
    pub protocol: String,

    /// Extra environment variables to inject into PHP `$_SERVER`.
    ///
    /// These are added after the standard CGI variables and HTTP headers,
    /// so they can override built-in values if needed. Used for injecting
    /// `EPHPM_REDIS_*` credentials in multi-tenant mode.
    pub env_vars: Vec<(String, String)>,
}

impl PhpRequest {
    /// Build the `$_SERVER` variables that `WordPress` and other PHP apps expect.
    ///
    /// Key distinction when fallback rewrites happen (e.g. `/blog/hello` → `/index.php`):
    /// - `REQUEST_URI` = original URI (`/blog/hello`) — what the client asked for
    /// - `SCRIPT_NAME` = resolved script (`/index.php`) — what PHP is executing
    /// - `PHP_SELF` = same as `SCRIPT_NAME`
    #[must_use]
    pub fn server_variables(&self) -> Vec<(String, String)> {
        build_server_variables(
            &self.method,
            &self.uri,
            &self.query_string,
            &self.script_filename,
            &self.document_root,
            &self.path,
            &self.server_name,
            self.server_port,
            &self.protocol,
            self.remote_addr,
            self.is_https,
            &self.headers,
            &self.env_vars,
        )
    }

    /// Extract the cookie string from the request headers.
    #[must_use]
    pub fn cookie_string(&self) -> String {
        cookie_string_from_headers(&self.headers)
    }
}

/// Build the `$_SERVER` variables from borrowed request fields.
///
/// This is the single source of truth for `$_SERVER` derivation, shared by
/// both [`PhpRequest::server_variables`] (fpm path) and the worker dispatch
/// path in `ephpm-server`. Keeping it a free function lets the worker path
/// build `$_SERVER` directly from its owned locals without allocating a
/// throwaway [`PhpRequest`].
///
/// Key distinction when fallback rewrites happen (e.g. `/blog/hello` → `/index.php`):
/// - `REQUEST_URI` = original URI (`/blog/hello`) — what the client asked for
/// - `SCRIPT_NAME` = resolved script (`/index.php`) — what PHP is executing
/// - `PHP_SELF` = same as `SCRIPT_NAME`
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn build_server_variables(
    method: &str,
    uri: &str,
    query_string: &str,
    script_filename: &std::path::Path,
    document_root: &std::path::Path,
    path: &str,
    server_name: &str,
    server_port: u16,
    protocol: &str,
    remote_addr: SocketAddr,
    is_https: bool,
    headers: &[(String, String)],
    env_vars: &[(String, String)],
) -> Vec<(String, String)> {
    // Derive SCRIPT_NAME from the resolved script_filename relative to
    // document_root. This is correct even after fallback rewrites.
    let script_name = script_filename
        .strip_prefix(document_root)
        .map_or_else(|_| path.to_owned(), |rel| format!("/{}", rel.to_string_lossy()));

    let mut vars = vec![
        ("REQUEST_METHOD".into(), method.to_owned()),
        ("REQUEST_URI".into(), uri.to_owned()),
        ("SCRIPT_FILENAME".into(), script_filename.to_string_lossy().into_owned()),
        ("SCRIPT_NAME".into(), script_name.clone()),
        ("DOCUMENT_ROOT".into(), document_root.to_string_lossy().into_owned()),
        ("SERVER_NAME".into(), server_name.to_owned()),
        ("SERVER_PORT".into(), server_port.to_string()),
        ("SERVER_SOFTWARE".into(), "ePHPm/0.1.0".into()),
        ("SERVER_PROTOCOL".into(), protocol.to_owned()),
        ("GATEWAY_INTERFACE".into(), "CGI/1.1".into()),
        ("QUERY_STRING".into(), query_string.to_owned()),
        ("PHP_SELF".into(), script_name),
        ("REMOTE_ADDR".into(), remote_addr.ip().to_string()),
        ("REMOTE_PORT".into(), remote_addr.port().to_string()),
        ("REDIRECT_STATUS".into(), "200".into()),
    ];

    if is_https {
        vars.push(("HTTPS".into(), "on".into()));
    }

    // Map HTTP headers to $_SERVER variables.
    //
    // Formerly built the CGI-style key with two String allocs per
    // header (`to_uppercase()` then `.replace('-','_')`). Header
    // names are ASCII by RFC 7230, so we can do a single byte
    // pass — one allocation per non-canonicalised header, with
    // the `HTTP_` prefix filled in place.
    for (name, value) in headers {
        let key = cgi_header_key(name);
        vars.push((key, value.clone()));
    }

    // Append extra environment variables (e.g. EPHPM_REDIS_* credentials).
    for (key, value) in env_vars {
        vars.push((key.clone(), value.clone()));
    }

    vars
}

/// Extract the cookie string from request headers (first `Cookie` header,
/// case-insensitive; empty string if absent).
///
/// Shared by [`PhpRequest::cookie_string`] and the worker dispatch path so
/// both derive the cookie data identically.
#[must_use]
pub fn cookie_string_from_headers(headers: &[(String, String)]) -> String {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("cookie"))
        .map(|(_, value)| value.clone())
        .unwrap_or_default()
}

/// Build the CGI-style `$_SERVER` key for an HTTP header name in one
/// byte pass.
///
/// Headers `host`, `cookie`, `content-type`, `content-length` map to
/// non-`HTTP_` keys per the CGI spec (and PHP's SAPI conventions);
/// everything else becomes `HTTP_<UPPER-WITH-UNDERSCORES>`. The old
/// implementation called `to_lowercase()` for dispatch and then, on
/// the general path, `to_uppercase()` and `.replace('-', '_')` — three
/// String allocations per header. HTTP header names are ASCII by RFC
/// 7230 so this can be a single ASCII upper + dash-to-underscore
/// pass writing into a pre-sized `String`.
#[must_use]
pub(crate) fn cgi_header_key(name: &str) -> String {
    // Special-case the ASCII-canonical spellings first (case-
    // insensitive). Doing this without a to_lowercase alloc is a
    // simple `eq_ignore_ascii_case`.
    if name.eq_ignore_ascii_case("host") {
        return "HTTP_HOST".to_string();
    }
    if name.eq_ignore_ascii_case("cookie") {
        return "HTTP_COOKIE".to_string();
    }
    if name.eq_ignore_ascii_case("content-type") {
        return "CONTENT_TYPE".to_string();
    }
    if name.eq_ignore_ascii_case("content-length") {
        return "CONTENT_LENGTH".to_string();
    }

    let bytes = name.as_bytes();
    let mut out = String::with_capacity(5 + bytes.len());
    out.push_str("HTTP_");
    for b in bytes {
        let c = match *b {
            b'-' => b'_',
            b @ b'a'..=b'z' => b - 32,
            b => b,
        };
        out.push(char::from(c));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::path::PathBuf;

    use super::*;

    fn make_request() -> PhpRequest {
        PhpRequest {
            method: "GET".into(),
            uri: "/index.php?page=1".into(),
            path: "/index.php".into(),
            query_string: "page=1".into(),
            script_filename: PathBuf::from("/var/www/html/index.php"),
            document_root: PathBuf::from("/var/www/html"),
            headers: vec![
                ("host".into(), "example.com".into()),
                ("accept-encoding".into(), "gzip, deflate".into()),
            ],
            body: Vec::new(),
            content_type: None,
            remote_addr: "192.168.1.1:54321".parse::<SocketAddr>().unwrap(),
            server_name: "example.com".into(),
            server_port: 8080,
            is_https: false,
            protocol: "HTTP/1.1".into(),
            env_vars: Vec::new(),
        }
    }

    /// Helper to find a server variable by key.
    fn find_var<'a>(vars: &'a [(String, String)], key: &str) -> Option<&'a str> {
        vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn test_server_variables_core_fields() {
        let req = make_request();
        let vars = req.server_variables();

        assert_eq!(find_var(&vars, "REQUEST_METHOD"), Some("GET"));
        assert_eq!(find_var(&vars, "REQUEST_URI"), Some("/index.php?page=1"));
        assert_eq!(find_var(&vars, "QUERY_STRING"), Some("page=1"));
        assert_eq!(find_var(&vars, "SERVER_NAME"), Some("example.com"));
        assert_eq!(find_var(&vars, "SERVER_PORT"), Some("8080"));
        assert_eq!(find_var(&vars, "SERVER_SOFTWARE"), Some("ePHPm/0.1.0"));
        assert_eq!(find_var(&vars, "SERVER_PROTOCOL"), Some("HTTP/1.1"));
        assert_eq!(find_var(&vars, "REMOTE_ADDR"), Some("192.168.1.1"));
        assert_eq!(find_var(&vars, "REMOTE_PORT"), Some("54321"));
        // SCRIPT_NAME derived from script_filename relative to document_root
        assert_eq!(find_var(&vars, "SCRIPT_NAME"), Some("/index.php"));
        assert_eq!(find_var(&vars, "PHP_SELF"), Some("/index.php"));
        assert_eq!(find_var(&vars, "GATEWAY_INTERFACE"), Some("CGI/1.1"));
        assert_eq!(find_var(&vars, "REDIRECT_STATUS"), Some("200"));
    }

    #[test]
    fn test_server_variables_script_paths() {
        let req = make_request();
        let vars = req.server_variables();

        let script = find_var(&vars, "SCRIPT_FILENAME").unwrap();
        assert_eq!(PathBuf::from(script), PathBuf::from("/var/www/html/index.php"));

        let docroot = find_var(&vars, "DOCUMENT_ROOT").unwrap();
        assert_eq!(PathBuf::from(docroot), PathBuf::from("/var/www/html"));

        assert_eq!(find_var(&vars, "SCRIPT_NAME"), Some("/index.php"));
    }

    #[test]
    fn test_server_variables_rewritten_request() {
        // Simulate fallback rewrite: /blog/hello → /index.php
        let mut req = make_request();
        req.uri = "/blog/hello?preview=true".into();
        req.path = "/blog/hello".into();
        req.query_string = "preview=true".into();
        // script_filename stays as /var/www/html/index.php (from fallback)
        let vars = req.server_variables();

        // REQUEST_URI keeps original
        assert_eq!(find_var(&vars, "REQUEST_URI"), Some("/blog/hello?preview=true"));
        // SCRIPT_NAME derived from resolved script
        assert_eq!(find_var(&vars, "SCRIPT_NAME"), Some("/index.php"));
        assert_eq!(find_var(&vars, "PHP_SELF"), Some("/index.php"));
    }

    #[test]
    fn test_server_variables_http_header_mapping() {
        let req = make_request();
        let vars = req.server_variables();

        assert_eq!(find_var(&vars, "HTTP_ACCEPT_ENCODING"), Some("gzip, deflate"));
    }

    #[test]
    fn test_server_variables_host_header() {
        let req = make_request();
        let vars = req.server_variables();

        // "host" header should map to HTTP_HOST, not HTTP_HTTP_HOST
        assert_eq!(find_var(&vars, "HTTP_HOST"), Some("example.com"));
        assert!(find_var(&vars, "HTTP_HTTP_HOST").is_none());
    }

    #[test]
    fn test_server_variables_content_type_no_http_prefix() {
        let mut req = make_request();
        req.headers.push(("content-type".into(), "application/json".into()));
        let vars = req.server_variables();

        assert_eq!(find_var(&vars, "CONTENT_TYPE"), Some("application/json"));
        assert!(find_var(&vars, "HTTP_CONTENT_TYPE").is_none());
    }

    #[test]
    fn test_server_variables_content_length_no_http_prefix() {
        let mut req = make_request();
        req.headers.push(("content-length".into(), "42".into()));
        let vars = req.server_variables();

        assert_eq!(find_var(&vars, "CONTENT_LENGTH"), Some("42"));
        assert!(find_var(&vars, "HTTP_CONTENT_LENGTH").is_none());
    }

    #[test]
    fn test_server_variables_https_on() {
        let mut req = make_request();
        req.is_https = true;
        let vars = req.server_variables();

        assert_eq!(find_var(&vars, "HTTPS"), Some("on"));
    }

    #[test]
    fn test_server_variables_https_absent_when_false() {
        let req = make_request();
        assert!(!req.is_https);
        let vars = req.server_variables();

        assert!(find_var(&vars, "HTTPS").is_none());
    }

    #[test]
    fn test_cookie_string_found() {
        let mut req = make_request();
        req.headers.push(("Cookie".into(), "session=abc123".into()));
        assert_eq!(req.cookie_string(), "session=abc123");
    }

    #[test]
    fn test_cookie_string_missing() {
        let req = make_request();
        assert_eq!(req.cookie_string(), "");
    }

    #[test]
    fn test_cookie_string_case_insensitive() {
        let mut req = make_request();
        req.headers.push(("COOKIE".into(), "token=xyz".into()));
        assert_eq!(req.cookie_string(), "token=xyz");
    }

    #[test]
    fn test_env_vars_injected_into_server_variables() {
        let mut req = make_request();
        req.env_vars = vec![
            ("EPHPM_REDIS_HOST".into(), "127.0.0.1".into()),
            ("EPHPM_REDIS_PORT".into(), "6379".into()),
            ("EPHPM_REDIS_USERNAME".into(), "example.com".into()),
            ("EPHPM_REDIS_PASSWORD".into(), "abc123".into()),
        ];
        let vars = req.server_variables();

        assert_eq!(find_var(&vars, "EPHPM_REDIS_HOST"), Some("127.0.0.1"));
        assert_eq!(find_var(&vars, "EPHPM_REDIS_PORT"), Some("6379"));
        assert_eq!(find_var(&vars, "EPHPM_REDIS_USERNAME"), Some("example.com"));
        assert_eq!(find_var(&vars, "EPHPM_REDIS_PASSWORD"), Some("abc123"));
    }

    #[test]
    fn test_env_vars_empty_by_default() {
        let req = make_request();
        let vars = req.server_variables();
        assert!(find_var(&vars, "EPHPM_REDIS_HOST").is_none());
    }

    /// The worker dispatch path in `ephpm-server` builds `$_SERVER` by calling
    /// [`build_server_variables`] directly from its owned locals rather than
    /// constructing a `PhpRequest`. This test guards that both derivations
    /// produce byte-identical output for the same synthetic request — if this
    /// ever diverges, worker mode and fpm mode would present PHP with
    /// different `$_SERVER`, which is a correctness bug.
    #[test]
    fn test_worker_path_server_variables_match_request_mode() {
        // A request that exercises every interesting branch: HTTPS on, a
        // fallback rewrite (uri != script), custom + canonical headers, and
        // injected env vars.
        let mut req = make_request();
        req.uri = "/blog/hello?preview=true".into();
        req.path = "/blog/hello".into();
        req.query_string = "preview=true".into();
        req.is_https = true;
        req.headers = vec![
            ("host".into(), "example.com".into()),
            ("accept-encoding".into(), "gzip, deflate".into()),
            ("content-type".into(), "application/json".into()),
            ("content-length".into(), "42".into()),
            ("x-custom-header".into(), "value".into()),
            ("cookie".into(), "session=abc123".into()),
        ];
        req.env_vars = vec![
            ("EPHPM_REDIS_HOST".into(), "127.0.0.1".into()),
            ("EPHPM_REDIS_PORT".into(), "6379".into()),
        ];

        // fpm path: via PhpRequest::server_variables().
        let request_mode = req.server_variables();

        // worker path: the exact call `handle_php_worker` makes, built from
        // borrowed/owned fields with no intermediate PhpRequest.
        let worker_mode = build_server_variables(
            &req.method,
            &req.uri,
            &req.query_string,
            &req.script_filename,
            &req.document_root,
            &req.path,
            &req.server_name,
            req.server_port,
            &req.protocol,
            req.remote_addr,
            req.is_https,
            &req.headers,
            &req.env_vars,
        );

        // Byte-identical, including order.
        assert_eq!(worker_mode, request_mode);
    }

    #[test]
    fn test_worker_path_cookie_matches_request_mode() {
        let mut req = make_request();
        req.headers.push(("Cookie".into(), "session=abc123".into()));
        assert_eq!(cookie_string_from_headers(&req.headers), req.cookie_string());
    }
}
