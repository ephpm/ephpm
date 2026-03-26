//! Static file serving with MIME type detection.
//!
//! Serves non-PHP files (CSS, JS, images, etc.) directly from the document root.

use std::path::Path;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::{Response, StatusCode};

/// Serve a static file from the document root.
///
/// Returns a 404 response if the file doesn't exist or is outside the document root.
pub async fn serve(document_root: &Path, url_path: &str) -> Response<Full<Bytes>> {
    let relative = url_path.trim_start_matches('/');
    let file_path = document_root.join(relative);

    // Security: ensure the resolved path is within the document root
    let Ok(canonical_root) = document_root.canonicalize() else {
        return not_found();
    };
    let Ok(canonical_file) = file_path.canonicalize() else {
        return not_found();
    };
    if !canonical_file.starts_with(&canonical_root) {
        tracing::warn!(
            path = %url_path,
            "path traversal attempt blocked"
        );
        return forbidden();
    }

    // Read the file
    let Ok(content) = tokio::fs::read(&canonical_file).await else {
        return not_found();
    };

    // Detect MIME type from file extension
    let mime =
        mime_guess::from_path(&canonical_file).first_raw().unwrap_or("application/octet-stream");

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", mime)
        .header("Content-Length", content.len())
        .body(Full::new(Bytes::from(content)))
        .unwrap_or_else(|_| internal_error())
}

fn not_found() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header("Content-Type", "text/plain")
        .body(Full::new(Bytes::from("404 Not Found")))
        .unwrap()
}

fn forbidden() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header("Content-Type", "text/plain")
        .body(Full::new(Bytes::from("403 Forbidden")))
        .unwrap()
}

fn internal_error() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Full::new(Bytes::from("Internal Server Error")))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use http_body_util::BodyExt;

    use super::*;

    /// Collect a `Full<Bytes>` body into a `Vec<u8>`.
    async fn body_bytes(resp: Response<Full<Bytes>>) -> Vec<u8> {
        resp.into_body().collect().await.unwrap().to_bytes().to_vec()
    }

    #[tokio::test]
    async fn test_serve_html_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("page.html"), "<h1>Hello</h1>").unwrap();

        let resp = serve(dir.path(), "/page.html").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["content-type"], "text/html");
        assert_eq!(body_bytes(resp).await, b"<h1>Hello</h1>");
    }

    #[tokio::test]
    async fn test_serve_css_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("style.css"), "body{}").unwrap();

        let resp = serve(dir.path(), "/style.css").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["content-type"], "text/css");
    }

    #[tokio::test]
    async fn test_serve_content_length_header() {
        let dir = tempfile::tempdir().unwrap();
        let content = "twelve chars";
        fs::write(dir.path().join("test.txt"), content).unwrap();

        let resp = serve(dir.path(), "/test.txt").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()["content-length"],
            content.len().to_string().as_str()
        );
    }

    #[tokio::test]
    async fn test_serve_unknown_extension() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("data.ephpmtest"), "binary").unwrap();

        let resp = serve(dir.path(), "/data.ephpmtest").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()["content-type"],
            "application/octet-stream"
        );
    }

    #[tokio::test]
    async fn test_serve_missing_file_returns_404() {
        let dir = tempfile::tempdir().unwrap();

        let resp = serve(dir.path(), "/nonexistent.txt").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_serve_path_traversal_blocked() {
        let parent = tempfile::tempdir().unwrap();
        let docroot = parent.path().join("www");
        fs::create_dir(&docroot).unwrap();
        // Create a file outside docroot
        fs::write(parent.path().join("secret.txt"), "secret").unwrap();

        let resp = serve(&docroot, "/../secret.txt").await;
        // Should be 403 (path resolves outside docroot) or 404 (canonicalize fails)
        let status = resp.status();
        assert!(
            status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND,
            "expected 403 or 404, got {status}"
        );
    }
}
