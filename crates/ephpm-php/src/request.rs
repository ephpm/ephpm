//! HTTP request to PHP request mapping.
//!
//! Converts an incoming HTTP request into the format expected by PHP's
//! embed SAPI, including populating `$_SERVER` variables.

use std::borrow::Cow;
use std::ffi::{CStr, CString};
use std::net::SocketAddr;
use std::path::PathBuf;

/// One PHP middleware mount resolved for a single request.
///
/// Produced by the router from a `[[middleware]] library = "php:<path>"` mount
/// whose `match` glob accepted this request. `script` is already resolved
/// against the request's own document root, so in multi-tenant mode it names
/// the tenant's own file and nothing else.
#[derive(Debug, Clone)]
pub struct PhpMiddleware {
    /// Absolute path to the middleware script.
    pub script: PathBuf,

    /// The mount's `config` table serialised to JSON, surfaced to the script as
    /// `ephpm_middleware_config()`. `None` when the mount declares no `config`.
    pub config_json: Option<String>,
}

/// How the PHP middleware chain ended for one request.
///
/// Mirrors the C `EPHPM_MW_*` codes; used for the
/// `ephpm_middleware_invocations_total{action=...}` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MiddlewareOutcome {
    /// Every mount ran and fell through to the application script.
    Continue,
    /// A mount short-circuited with `exit()` — the lane's `RESPOND`.
    Respond,
    /// A mount raised a fatal; the application script never ran (fail closed).
    Error,
}

impl MiddlewareOutcome {
    /// Metric label for this outcome.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Respond => "respond",
            Self::Error => "error",
        }
    }
}

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

    /// PHP middleware scripts to run — in chain order — inside this request,
    /// immediately before `script_filename`.
    ///
    /// Empty for every request that has no `php:` mount, which is the default
    /// and costs nothing. See [`PhpMiddleware`].
    pub middleware: Vec<PhpMiddleware>,
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

    /// Build the `$_SERVER` variables in FFI-ready form — same derivation as
    /// [`Self::server_variables`], see [`build_server_variables_c`].
    #[must_use]
    pub fn server_variables_c(&self) -> Vec<CServerVar> {
        build_server_variables_c(
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

/// A `$_SERVER` entry in FFI-ready form: NUL-terminated key and value, with
/// static (`&'static CStr`) storage for the strings that never vary between
/// requests, so the per-request invariant half of `$_SERVER` costs zero
/// allocations (issue #133).
pub type CServerVar = (Cow<'static, CStr>, Cow<'static, CStr>);

/// Build the `$_SERVER` variables from borrowed request fields.
///
/// Thin conversion wrapper over [`build_server_variables_c`], which is the
/// single source of truth for `$_SERVER` derivation — kept so tests (and any
/// caller that wants plain strings) can assert on the derivation without
/// touching `CStr`. The hot paths (fpm dispatch in `execute_php`, worker
/// dispatch in `ephpm-server`) call the `_c` variant directly and never build
/// this `String` form.
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
    build_server_variables_c(
        method,
        uri,
        query_string,
        script_filename,
        document_root,
        path,
        server_name,
        server_port,
        protocol,
        remote_addr,
        is_https,
        headers,
        env_vars,
    )
    .iter()
    .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
    .collect()
}

/// The `REQUEST_METHOD` value as a static C string for the common methods,
/// avoiding a per-request allocation; anything else is copied.
fn method_value(method: &str) -> Option<Cow<'static, CStr>> {
    let known: &'static CStr = match method {
        "GET" => c"GET",
        "POST" => c"POST",
        "HEAD" => c"HEAD",
        "PUT" => c"PUT",
        "DELETE" => c"DELETE",
        "PATCH" => c"PATCH",
        "OPTIONS" => c"OPTIONS",
        _ => return owned(method),
    };
    Some(Cow::Borrowed(known))
}

/// The `SERVER_PROTOCOL` value as a static C string for the protocols hyper
/// actually produces; anything else is copied.
fn protocol_value(protocol: &str) -> Option<Cow<'static, CStr>> {
    let known: &'static CStr = match protocol {
        "HTTP/1.1" => c"HTTP/1.1",
        "HTTP/2.0" => c"HTTP/2.0",
        "HTTP/1.0" => c"HTTP/1.0",
        _ => return owned(protocol),
    };
    Some(Cow::Borrowed(known))
}

/// Copy a string into an owned C string. `None` when it contains an interior
/// NUL — such a pair is dropped by the builder rather than truncated or
/// substituted, so PHP never sees a value that differs from what the client
/// (or the router) actually produced.
fn owned(s: &str) -> Option<Cow<'static, CStr>> {
    CString::new(s).ok().map(Cow::Owned)
}

/// Build the `$_SERVER` variables from borrowed request fields, directly in
/// the NUL-terminated form the SAPI FFI needs (issue #133).
///
/// This is the single source of truth for `$_SERVER` derivation, shared by
/// the fpm path ([`PhpRequest::server_variables_c`], consumed by
/// `execute_php`) and the worker dispatch path in `ephpm-server`. Both used
/// to build an intermediate `Vec<(String, String)>` and then convert every
/// pair to `CString` a second time — two allocations, two copies, and a NUL
/// scan per string, on every request. Building the C form once halves the
/// allocation traffic, and the entries that never vary between requests
/// (`SERVER_SOFTWARE`, `GATEWAY_INTERFACE`, `REDIRECT_STATUS`, `HTTPS`, the
/// fixed CGI keys, common methods/protocols, canonical header keys) are
/// `&'static CStr` literals that allocate nothing at all.
///
/// A key or value with an interior NUL drops that pair — the same behaviour
/// the fpm path always had. (The worker path previously substituted an empty
/// string for the unrepresentable half, which could register a header key
/// with someone else's value; dropping is strictly safer.)
///
/// Key distinction when fallback rewrites happen (e.g. `/blog/hello` → `/index.php`):
/// - `REQUEST_URI` = original URI (`/blog/hello`) — what the client asked for
/// - `SCRIPT_NAME` = resolved script (`/index.php`) — what PHP is executing
/// - `PHP_SELF` = same as `SCRIPT_NAME`
#[must_use]
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn build_server_variables_c(
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
) -> Vec<CServerVar> {
    // Derive SCRIPT_NAME from the resolved script_filename relative to
    // document_root. This is correct even after fallback rewrites.
    let script_name = script_filename
        .strip_prefix(document_root)
        .map_or_else(|_| path.to_owned(), |rel| format!("/{}", rel.to_string_lossy()));
    // One CString for SCRIPT_NAME + PHP_SELF; the clone is a plain memcpy.
    let script_name = CString::new(script_name).ok();

    let mut vars: Vec<CServerVar> =
        Vec::with_capacity(16 + headers.len() + env_vars.len() + usize::from(is_https));
    let mut push = |key: Cow<'static, CStr>, value: Option<Cow<'static, CStr>>| {
        if let Some(value) = value {
            vars.push((key, value));
        }
    };

    push(Cow::Borrowed(c"REQUEST_METHOD"), method_value(method));
    push(Cow::Borrowed(c"REQUEST_URI"), owned(uri));
    push(Cow::Borrowed(c"SCRIPT_FILENAME"), owned(&script_filename.to_string_lossy()));
    push(Cow::Borrowed(c"SCRIPT_NAME"), script_name.clone().map(Cow::Owned));
    push(Cow::Borrowed(c"DOCUMENT_ROOT"), owned(&document_root.to_string_lossy()));
    push(Cow::Borrowed(c"SERVER_NAME"), owned(server_name));
    push(Cow::Borrowed(c"SERVER_PORT"), owned(&server_port.to_string()));
    push(Cow::Borrowed(c"SERVER_SOFTWARE"), Some(Cow::Borrowed(c"ePHPm/0.1.0")));
    push(Cow::Borrowed(c"SERVER_PROTOCOL"), protocol_value(protocol));
    push(Cow::Borrowed(c"GATEWAY_INTERFACE"), Some(Cow::Borrowed(c"CGI/1.1")));
    push(Cow::Borrowed(c"QUERY_STRING"), owned(query_string));
    push(Cow::Borrowed(c"PHP_SELF"), script_name.map(Cow::Owned));
    push(Cow::Borrowed(c"REMOTE_ADDR"), owned(&remote_addr.ip().to_string()));
    push(Cow::Borrowed(c"REMOTE_PORT"), owned(&remote_addr.port().to_string()));
    push(Cow::Borrowed(c"REDIRECT_STATUS"), Some(Cow::Borrowed(c"200")));

    if is_https {
        push(Cow::Borrowed(c"HTTPS"), Some(Cow::Borrowed(c"on")));
    }

    // Map HTTP headers to $_SERVER variables. The canonical names get static
    // keys; everything else is one byte-pass allocation (see cgi_header_key).
    for (name, value) in headers {
        if let Some(key) = cgi_header_key_c(name) {
            push(key, owned(value));
        }
    }

    // Append extra environment variables (e.g. EPHPM_REDIS_* credentials).
    for (key, value) in env_vars {
        if let Some(key) = owned(key) {
            push(key, owned(value));
        }
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
/// byte pass, directly as a C string.
///
/// Headers `host`, `cookie`, `content-type`, `content-length` map to
/// non-`HTTP_` keys per the CGI spec (and PHP's SAPI conventions) — those
/// come back as `&'static CStr` with no allocation at all; everything else
/// becomes an owned `HTTP_<UPPER-WITH-UNDERSCORES>` built in a single ASCII
/// upper + dash-to-underscore pass over a pre-sized buffer. HTTP header
/// names are ASCII by RFC 7230.
///
/// `None` for a header name with an interior NUL (hyper never produces one;
/// dropping the pair is the fail-safe read).
#[must_use]
pub(crate) fn cgi_header_key_c(name: &str) -> Option<Cow<'static, CStr>> {
    // Special-case the ASCII-canonical spellings first (case-
    // insensitive). Doing this without a to_lowercase alloc is a
    // simple `eq_ignore_ascii_case`.
    if name.eq_ignore_ascii_case("host") {
        return Some(Cow::Borrowed(c"HTTP_HOST"));
    }
    if name.eq_ignore_ascii_case("cookie") {
        return Some(Cow::Borrowed(c"HTTP_COOKIE"));
    }
    if name.eq_ignore_ascii_case("content-type") {
        return Some(Cow::Borrowed(c"CONTENT_TYPE"));
    }
    if name.eq_ignore_ascii_case("content-length") {
        return Some(Cow::Borrowed(c"CONTENT_LENGTH"));
    }

    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(5 + bytes.len() + 1);
    out.extend_from_slice(b"HTTP_");
    for b in bytes {
        out.push(match *b {
            b'-' => b'_',
            b @ b'a'..=b'z' => b - 32,
            b => b,
        });
    }
    CString::new(out).ok().map(Cow::Owned)
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
            middleware: Vec::new(),
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

        // fpm path: via PhpRequest::server_variables_c() (what execute_php
        // registers).
        let request_mode = req.server_variables_c();

        // worker path: the exact call `handle_php_worker` makes, built from
        // borrowed/owned fields with no intermediate PhpRequest.
        let worker_mode = build_server_variables_c(
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

    /// The `String` form must stay a faithful view of the FFI form — it is
    /// what every derivation test in this module asserts on, so if the two
    /// ever diverged those tests would silently stop covering what PHP
    /// actually receives.
    #[test]
    fn test_string_form_matches_c_form() {
        let mut req = make_request();
        req.is_https = true;
        req.env_vars = vec![("DB_USER".into(), "site-a".into())];
        let strings = req.server_variables();
        let c_form = req.server_variables_c();
        assert_eq!(strings.len(), c_form.len());
        for ((sk, sv), (ck, cv)) in strings.iter().zip(c_form.iter()) {
            assert_eq!(sk.as_bytes(), ck.to_bytes());
            assert_eq!(sv.as_bytes(), cv.to_bytes());
        }
    }

    /// Multi-tenant guard for the #133 rework: `$_SERVER` is derived fresh
    /// from each request's own resolved site — there is deliberately **no**
    /// cross-request cache in this module (the only shared storage is
    /// `&'static CStr` literals for values identical for every tenant). Two
    /// consecutive builds for different sites on the same thread must each
    /// carry only their own tenant's docroot, host, and injected `DB_*`
    /// credentials.
    #[test]
    fn test_per_site_values_never_bleed_between_requests() {
        let build = |site: &str, docroot: &str, password: &str| {
            build_server_variables_c(
                "GET",
                "/index.php",
                "",
                &PathBuf::from(format!("{docroot}/index.php")),
                &PathBuf::from(docroot),
                "/index.php",
                site,
                443,
                "HTTP/1.1",
                "192.0.2.1:1234".parse().unwrap(),
                true,
                &[("host".to_string(), site.to_string())],
                &[
                    ("DB_USER".to_string(), site.to_string()),
                    ("DB_PASSWORD".to_string(), password.to_string()),
                ],
            )
        };

        let site_a = build("a.example", "/sites/a.example", "secret-a");
        let site_b = build("b.example", "/sites/b.example", "secret-b");

        let get = |vars: &[CServerVar], key: &CStr| -> Option<String> {
            vars.iter()
                .find(|(k, _)| k.as_ref() == key)
                .map(|(_, v)| v.to_string_lossy().into_owned())
        };

        for (vars, site, docroot, password) in [
            (&site_a, "a.example", "/sites/a.example", "secret-a"),
            (&site_b, "b.example", "/sites/b.example", "secret-b"),
        ] {
            assert_eq!(get(vars, c"SERVER_NAME").as_deref(), Some(site));
            assert_eq!(get(vars, c"HTTP_HOST").as_deref(), Some(site));
            assert_eq!(get(vars, c"DOCUMENT_ROOT").as_deref(), Some(docroot));
            assert_eq!(get(vars, c"DB_USER").as_deref(), Some(site));
            assert_eq!(get(vars, c"DB_PASSWORD").as_deref(), Some(password));
        }
        // And nothing of A survives into B's view (or vice versa).
        let b_values: Vec<String> =
            site_b.iter().map(|(_, v)| v.to_string_lossy().into_owned()).collect();
        assert!(!b_values.iter().any(|v| v.contains("a.example") || v.contains("secret-a")));
    }
}
