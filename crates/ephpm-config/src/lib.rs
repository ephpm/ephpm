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
