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
        // Derive SCRIPT_NAME from the resolved script_filename relative to
        // document_root. This is correct even after fallback rewrites.
        let script_name = self
            .script_filename
            .strip_prefix(&self.document_root)
            .map_or_else(|_| self.path.clone(), |rel| format!("/{}", rel.to_string_lossy()));

        let mut vars = vec![
            ("REQUEST_METHOD".into(), self.method.clone()),
            ("REQUEST_URI".into(), self.uri.clone()),
            ("SCRIPT_FILENAME".into(), self.script_filename.to_string_lossy().into_owned()),
            ("SCRIPT_NAME".into(), script_name.clone()),
            ("DOCUMENT_ROOT".into(), self.document_root.to_string_lossy().into_owned()),
            ("SERVER_NAME".into(), self.server_name.clone()),
            ("SERVER_PORT".into(), self.server_port.to_string()),
            ("SERVER_SOFTWARE".into(), "ePHPm/0.1.0".into()),
            ("SERVER_PROTOCOL".into(), self.protocol.clone()),
            ("GATEWAY_INTERFACE".into(), "CGI/1.1".into()),
            ("QUERY_STRING".into(), self.query_string.clone()),
            ("PHP_SELF".into(), script_name),
            ("REMOTE_ADDR".into(), self.remote_addr.ip().to_string()),
            ("REMOTE_PORT".into(), self.remote_addr.port().to_string()),
            ("REDIRECT_STATUS".into(), "200".into()),
        ];

        if self.is_https {
            vars.push(("HTTPS".into(), "on".into()));
        }

        // Map HTTP headers to $_SERVER variables
        for (name, value) in &self.headers {
            let key = match name.to_lowercase().as_str() {
                "host" => "HTTP_HOST".to_string(),
                "cookie" => "HTTP_COOKIE".to_string(),
                "content-type" => "CONTENT_TYPE".to_string(),
                "content-length" => "CONTENT_LENGTH".to_string(),
                _ => {
                    // Convert "Accept-Encoding" → "HTTP_ACCEPT_ENCODING"
                    format!("HTTP_{}", name.to_uppercase().replace('-', "_"))
                }
            };
            vars.push((key, value.clone()));
        }

        // Append extra environment variables (e.g. EPHPM_REDIS_* credentials).
        for (key, value) in &self.env_vars {
            vars.push((key.clone(), value.clone()));
        }

        vars
    }

    /// Extract the cookie string from the request headers.
    #[must_use]
    pub fn cookie_string(&self) -> String {
        self.headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("cookie"))
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    }
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
}
