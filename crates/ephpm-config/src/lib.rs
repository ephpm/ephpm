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

    /// Security settings.
    #[serde(default)]
    pub security: SecurityConfig,

    /// Logging settings.
    #[serde(default)]
    pub logging: LoggingConfig,
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

/// PHP runtime configuration.
#[derive(Debug, Deserialize)]
pub struct PhpConfig {
    /// Maximum execution time in seconds for a single PHP request.
    #[serde(default = "default_max_execution_time")]
    pub max_execution_time: u32,

    /// Memory limit for PHP (e.g. "128M").
    #[serde(default = "default_memory_limit")]
    pub memory_limit: String,

    /// INI directive overrides as `[key, value]` pairs.
    #[serde(default)]
    pub ini_overrides: Vec<[String; 2]>,
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
            index_files: default_index_files(),
            fallback: default_fallback(),
            request: RequestConfig::default(),
            timeouts: TimeoutsConfig::default(),
            response: ResponseConfig::default(),
            static_files: StaticConfig::default(),
            security: SecurityConfig::default(),
            logging: LoggingConfig::default(),
        }
    }
}

impl Default for RequestConfig {
    fn default() -> Self {
        Self {
            max_body_size: default_max_body_size(),
            max_header_size: default_max_header_size(),
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
        }
    }
}

impl Default for StaticConfig {
    fn default() -> Self {
        Self {
            cache_control: String::new(),
            hidden_files: default_hidden_files(),
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
            ini_overrides: Vec::new(),
        }
    }
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

fn default_log_level() -> String {
    "info".to_string()
}

fn default_max_execution_time() -> u32 {
    30
}

fn default_memory_limit() -> String {
    "128M".to_string()
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
}
