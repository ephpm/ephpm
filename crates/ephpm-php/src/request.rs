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
}

impl PhpRequest {
    /// Build the `$_SERVER` variables that `WordPress` and other PHP apps expect.
    #[must_use]
    pub fn server_variables(&self) -> Vec<(String, String)> {
        let mut vars = vec![
            ("REQUEST_METHOD".into(), self.method.clone()),
            ("REQUEST_URI".into(), self.uri.clone()),
            ("SCRIPT_FILENAME".into(), self.script_filename.to_string_lossy().into_owned()),
            ("SCRIPT_NAME".into(), self.path.clone()),
            ("DOCUMENT_ROOT".into(), self.document_root.to_string_lossy().into_owned()),
            ("SERVER_NAME".into(), self.server_name.clone()),
            ("SERVER_PORT".into(), self.server_port.to_string()),
            ("SERVER_SOFTWARE".into(), "ePHPm/0.1.0".into()),
            ("SERVER_PROTOCOL".into(), self.protocol.clone()),
            ("QUERY_STRING".into(), self.query_string.clone()),
            ("PHP_SELF".into(), self.path.clone()),
            ("REMOTE_ADDR".into(), self.remote_addr.ip().to_string()),
            ("REMOTE_PORT".into(), self.remote_addr.port().to_string()),
            ("PATH_INFO".into(), String::new()),
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
        assert_eq!(find_var(&vars, "PHP_SELF"), Some("/index.php"));
    }

    #[test]
    fn test_server_variables_script_paths() {
        let req = make_request();
        let vars = req.server_variables();

        // These contain PathBuf-derived strings, so compare as PathBuf
        let script = find_var(&vars, "SCRIPT_FILENAME").unwrap();
        assert_eq!(PathBuf::from(script), PathBuf::from("/var/www/html/index.php"));

        let docroot = find_var(&vars, "DOCUMENT_ROOT").unwrap();
        assert_eq!(PathBuf::from(docroot), PathBuf::from("/var/www/html"));

        assert_eq!(find_var(&vars, "SCRIPT_NAME"), Some("/index.php"));
    }

    #[test]
    fn test_server_variables_http_header_mapping() {
        let req = make_request();
        let vars = req.server_variables();

        assert_eq!(
            find_var(&vars, "HTTP_ACCEPT_ENCODING"),
            Some("gzip, deflate")
        );
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
        req.headers
            .push(("content-type".into(), "application/json".into()));
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
        req.headers
            .push(("Cookie".into(), "session=abc123".into()));
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
        req.headers
            .push(("COOKIE".into(), "token=xyz".into()));
        assert_eq!(req.cookie_string(), "token=xyz");
    }
}
