//! Request router.
//!
//! Routes incoming HTTP requests to either PHP execution or static file serving
//! based on the request path:
//! - Requests for `.php` files → PHP execution via `spawn_blocking`
//! - Everything else → static file serving

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use ephpm_config::Config;
use ephpm_php::PhpRuntime;
use ephpm_php::request::PhpRequest;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response, StatusCode};

use crate::static_files;

pub struct Router {
    document_root: PathBuf,
    index_files: Vec<String>,
    server_port: u16,
}

impl Router {
    #[must_use]
    pub fn new(config: &Config) -> Self {
        // Parse port from listen address
        let port =
            config.server.listen.rsplit_once(':').and_then(|(_, p)| p.parse().ok()).unwrap_or(8080);

        Self {
            document_root: config.server.document_root.clone(),
            index_files: config.server.index_files.clone(),
            server_port: port,
        }
    }

    /// Handle an incoming HTTP request.
    ///
    /// # Errors
    ///
    /// Returns `hyper::Error` if the response cannot be constructed.
    pub async fn handle(
        &self,
        req: Request<Incoming>,
        remote_addr: SocketAddr,
    ) -> Result<Response<Full<Bytes>>, hyper::Error> {
        let path = req.uri().path().to_string();

        // Resolve the filesystem path
        let fs_path = self.resolve_path(&path);

        if Self::is_php_request(&path, &fs_path) {
            // 404 if the resolved script doesn't exist on disk
            if !fs_path.exists() {
                return Ok(not_found());
            }
            Ok(self.handle_php(req, remote_addr, fs_path).await)
        } else {
            Ok(static_files::serve(&self.document_root, &path).await)
        }
    }

    /// Resolve a URL path to a filesystem path, checking for index files.
    fn resolve_path(&self, url_path: &str) -> PathBuf {
        let relative = url_path.trim_start_matches('/');
        let fs_path = self.document_root.join(relative);

        // If the path is a directory, try index files
        if fs_path.is_dir() {
            for index in &self.index_files {
                let candidate = fs_path.join(index);
                if candidate.exists() {
                    return candidate;
                }
            }
        }

        // If the path doesn't exist and doesn't have an extension,
        // try index files (WordPress pretty permalinks)
        if !fs_path.exists() && fs_path.extension().is_none() {
            for index in &self.index_files {
                let candidate = self.document_root.join(index);
                if candidate.exists() {
                    return candidate;
                }
            }
        }

        fs_path
    }

    /// Check if a request should be handled by PHP.
    fn is_php_request(url_path: &str, fs_path: &Path) -> bool {
        // Direct .php request
        if Path::new(url_path).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("php")) {
            return true;
        }

        // Resolved path is a .php file (e.g. directory → index.php)
        if let Some(ext) = fs_path.extension() {
            if ext == "php" {
                return true;
            }
        }

        // URL path has no extension and no static file exists — route to PHP
        // This handles WordPress pretty permalinks (/2024/01/hello-world/)
        if fs_path.extension().is_none() && !fs_path.exists() {
            return true;
        }

        false
    }

    /// Handle a PHP request by executing it in a blocking task.
    async fn handle_php(
        &self,
        req: Request<Incoming>,
        remote_addr: SocketAddr,
        script_filename: PathBuf,
    ) -> Response<Full<Bytes>> {
        let method = req.method().to_string();
        let uri = req.uri().to_string();
        let path = req.uri().path().to_string();
        let query_string = req.uri().query().unwrap_or("").to_string();
        let protocol = format!("{:?}", req.version());

        // Extract headers
        let headers: Vec<(String, String)> = req
            .headers()
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_str().unwrap_or("").to_string()))
            .collect();

        let content_type =
            req.headers().get("content-type").and_then(|v| v.to_str().ok()).map(String::from);

        let server_name = req
            .headers()
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("localhost")
            .split(':')
            .next()
            .unwrap_or("localhost")
            .to_string();

        // Collect request body
        let body = match req.collect().await {
            Ok(collected) => collected.to_bytes().to_vec(),
            Err(_) => Vec::new(),
        };

        let document_root = self.document_root.clone();
        let server_port = self.server_port;

        // Execute PHP in a blocking task to avoid blocking the tokio runtime
        let result = tokio::task::spawn_blocking(move || {
            let php_request = PhpRequest {
                method,
                uri,
                path,
                query_string,
                script_filename,
                document_root,
                headers,
                body,
                content_type,
                remote_addr,
                server_name,
                server_port,
                is_https: false,
                protocol,
            };

            PhpRuntime::execute(php_request)
        })
        .await;

        match result {
            Ok(Ok(php_response)) => {
                let status = StatusCode::from_u16(php_response.status).unwrap_or(StatusCode::OK);
                let mut response = Response::builder().status(status);

                for (name, value) in &php_response.headers {
                    response = response.header(name.as_str(), value.as_str());
                }

                response.body(Full::new(Bytes::from(php_response.body))).unwrap_or_else(|_| {
                    Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Full::new(Bytes::from("Internal Server Error")))
                        .unwrap()
                })
            }
            Ok(Err(err)) => {
                tracing::error!(%err, "PHP execution failed");
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Full::new(Bytes::from(format!("PHP execution error: {err}"))))
                    .unwrap()
            }
            Err(err) => {
                tracing::error!(%err, "spawn_blocking task failed");
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Full::new(Bytes::from("Internal Server Error")))
                    .unwrap()
            }
        }
    }
}

fn not_found() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header("content-type", "text/plain")
        .body(Full::new(Bytes::from("404 Not Found")))
        .expect("static 404 response")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use ephpm_config::{Config, PhpConfig, ServerConfig};

    use super::*;

    fn test_router(dir: &Path) -> Router {
        let config = Config {
            server: ServerConfig {
                listen: "0.0.0.0:8080".to_string(),
                document_root: dir.to_path_buf(),
                index_files: vec!["index.php".to_string(), "index.html".to_string()],
            },
            php: PhpConfig::default(),
        };
        Router::new(&config)
    }

    // ── Router::new port parsing ──────────────────────────────────────

    #[test]
    fn test_new_parses_port() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            server: ServerConfig {
                listen: "0.0.0.0:3000".to_string(),
                document_root: dir.path().to_path_buf(),
                index_files: Vec::new(),
            },
            php: PhpConfig::default(),
        };
        let router = Router::new(&config);
        assert_eq!(router.server_port, 3000);
    }

    #[test]
    fn test_new_defaults_port_when_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            server: ServerConfig {
                listen: "localhost:notaport".to_string(),
                document_root: dir.path().to_path_buf(),
                index_files: Vec::new(),
            },
            php: PhpConfig::default(),
        };
        let router = Router::new(&config);
        assert_eq!(router.server_port, 8080);
    }

    #[test]
    fn test_new_defaults_port_when_no_colon() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            server: ServerConfig {
                listen: "localhost".to_string(),
                document_root: dir.path().to_path_buf(),
                index_files: Vec::new(),
            },
            php: PhpConfig::default(),
        };
        let router = Router::new(&config);
        assert_eq!(router.server_port, 8080);
    }

    // ── resolve_path ──────────────────────────────────────────────────

    #[test]
    fn test_resolve_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("style.css"), "body{}").unwrap();

        let router = test_router(dir.path());
        let resolved = router.resolve_path("/style.css");
        assert_eq!(resolved, dir.path().join("style.css"));
    }

    #[test]
    fn test_resolve_directory_with_index_php() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("index.php"), "<?php").unwrap();

        let router = test_router(dir.path());
        let resolved = router.resolve_path("/");
        assert_eq!(resolved, dir.path().join("index.php"));
    }

    #[test]
    fn test_resolve_directory_falls_to_index_html() {
        let dir = tempfile::tempdir().unwrap();
        // No index.php, only index.html
        fs::write(dir.path().join("index.html"), "<html>").unwrap();

        let router = test_router(dir.path());
        let resolved = router.resolve_path("/");
        assert_eq!(resolved, dir.path().join("index.html"));
    }

    #[test]
    fn test_resolve_permalink_to_index_php() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("index.php"), "<?php").unwrap();

        let router = test_router(dir.path());
        // Extensionless path that doesn't exist -> WordPress permalink fallback
        let resolved = router.resolve_path("/2024/hello-world");
        assert_eq!(resolved, dir.path().join("index.php"));
    }

    #[test]
    fn test_resolve_nonexistent_with_extension() {
        let dir = tempfile::tempdir().unwrap();

        let router = test_router(dir.path());
        // Has extension, doesn't exist -> returned as-is (no fallback)
        let resolved = router.resolve_path("/missing.css");
        assert_eq!(resolved, dir.path().join("missing.css"));
    }

    #[test]
    fn test_resolve_nonexistent_no_index() {
        let dir = tempfile::tempdir().unwrap();
        // Empty docroot, no index files to fall back to

        let router = test_router(dir.path());
        let resolved = router.resolve_path("/foo/bar");
        assert_eq!(resolved, dir.path().join("foo/bar"));
    }

    // ── is_php_request ────────────────────────────────────────────────

    #[test]
    fn test_is_php_direct_url() {
        let dir = tempfile::tempdir().unwrap();
        let fs_path = dir.path().join("index.php");
        assert!(Router::is_php_request("/index.php", &fs_path));
    }

    #[test]
    fn test_is_php_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let fs_path = dir.path().join("INDEX.PHP");
        assert!(Router::is_php_request("/INDEX.PHP", &fs_path));
    }

    #[test]
    fn test_is_php_resolved_index() {
        let dir = tempfile::tempdir().unwrap();
        // URL is "/" but resolved fs_path is index.php
        let fs_path = dir.path().join("index.php");
        assert!(Router::is_php_request("/", &fs_path));
    }

    #[test]
    fn test_is_php_permalink() {
        let dir = tempfile::tempdir().unwrap();
        // Non-existent extensionless path -> PHP (permalink routing)
        let fs_path = dir.path().join("hello-world");
        assert!(!fs_path.exists());
        assert!(fs_path.extension().is_none());
        assert!(Router::is_php_request("/hello-world", &fs_path));
    }

    #[test]
    fn test_is_not_php_static_file() {
        let dir = tempfile::tempdir().unwrap();
        let fs_path = dir.path().join("style.css");
        fs::write(&fs_path, "body{}").unwrap();
        assert!(!Router::is_php_request("/style.css", &fs_path));
    }

    #[test]
    fn test_is_not_php_html_file() {
        let dir = tempfile::tempdir().unwrap();
        let fs_path = dir.path().join("page.html");
        fs::write(&fs_path, "<html>").unwrap();
        assert!(!Router::is_php_request("/page.html", &fs_path));
    }
}
