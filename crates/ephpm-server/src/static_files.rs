//! Static file serving with MIME type detection and `ETag` support.
//!
//! Serves non-PHP files (CSS, JS, images, etc.) directly from the document root.

use std::hash::{DefaultHasher, Hasher};
use std::path::Path;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::{Response, StatusCode};

use crate::body::{self, ServerBody};

/// Serve a static file from the document root.
///
/// Returns a 404 response if the file doesn't exist or is outside the document root.
pub async fn serve(document_root: &Path, url_path: &str) -> Response<ServerBody> {
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
        .body(body::buffered(Full::new(Bytes::from(content))))
        .unwrap_or_else(|_| internal_error())
}

/// Serve a file from an already-resolved filesystem path.
///
/// The router has already verified the file exists and resolved it via
/// `fallback`. We still validate path traversal for defense in depth.
///
/// When `etag` is enabled, computes a hash-based `ETag` for the content.
/// If `if_none_match` contains a matching `ETag`, returns 304 Not Modified.
#[allow(clippy::too_many_arguments)]
pub async fn serve_file(
    document_root: &Path,
    file_path: &Path,
    accepts_gzip: bool,
    cache_control: &str,
    compression: crate::router::CompressionSettings,
    etag: bool,
    if_none_match: Option<&str>,
    file_cache: Option<&crate::file_cache::FileCache>,
) -> Response<ServerBody> {
    // Security: ensure the resolved path is within the document root
    let Ok(canonical_root) = document_root.canonicalize() else {
        return not_found();
    };
    let Ok(canonical_file) = file_path.canonicalize() else {
        return not_found();
    };
    if !canonical_file.starts_with(&canonical_root) {
        tracing::warn!(
            path = %file_path.display(),
            "path traversal attempt blocked"
        );
        return forbidden();
    }

    // Try the file cache first.
    if let Some(cache) = file_cache {
        if let Some(entry) = cache.lookup(&canonical_file).await {
            return serve_cached_entry(
                entry, accepts_gzip, cache_control, if_none_match,
            );
        }
    }

    // Check file size — stream large files instead of reading into memory.
    let metadata = match tokio::fs::metadata(&canonical_file).await {
        Ok(m) => m,
        Err(_) => return not_found(),
    };

    let mime =
        mime_guess::from_path(&canonical_file).first_raw().unwrap_or("application/octet-stream");

    // Stream threshold: 1 MiB. Files above this are streamed from disk.
    const STREAM_THRESHOLD: u64 = 1_048_576;

    if metadata.len() > STREAM_THRESHOLD {
        return serve_streamed(
            &canonical_file,
            &metadata,
            mime,
            cache_control,
            if_none_match,
        )
        .await;
    }

    let Ok(content) = tokio::fs::read(&canonical_file).await else {
        return not_found();
    };

    // Insert into cache if enabled.
    if let Some(cache) = file_cache {
        if let Ok(mtime) = metadata.modified() {
            let entry = cache.insert(&canonical_file, &content, mtime, mime, compression);
            return serve_cached_entry(
                entry, accepts_gzip, cache_control, if_none_match,
            );
        }
    }

    // Uncached path — compute ETag from content hash.
    let etag_value = if etag { Some(compute_etag(&content)) } else { None };

    if let (Some(tag), Some(client_tag)) = (&etag_value, if_none_match) {
        if etag_matches(tag, client_tag) {
            let mut builder = Response::builder().status(StatusCode::NOT_MODIFIED);
            builder = builder.header("ETag", tag.as_str());
            if !cache_control.is_empty() {
                builder = builder.header("Cache-Control", cache_control);
            }
            return builder
                .body(body::buffered(Full::new(Bytes::new())))
                .unwrap_or_else(|_| internal_error());
        }
    }

    if accepts_gzip {
        if let Some(compressed) = crate::router::gzip_compress(&content, mime, compression) {
            let mut builder = Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", mime)
                .header("Content-Length", compressed.len())
                .header("Content-Encoding", "gzip")
                .header("Vary", "Accept-Encoding");
            if !cache_control.is_empty() {
                builder = builder.header("Cache-Control", cache_control);
            }
            if let Some(ref tag) = etag_value {
                builder = builder.header("ETag", tag.as_str());
            }
            return builder
                .body(body::buffered(Full::new(Bytes::from(compressed))))
                .unwrap_or_else(|_| internal_error());
        }
    }

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", mime)
        .header("Content-Length", content.len());
    if !cache_control.is_empty() {
        builder = builder.header("Cache-Control", cache_control);
    }
    if let Some(ref tag) = etag_value {
        builder = builder.header("ETag", tag.as_str());
    }
    builder
        .body(body::buffered(Full::new(Bytes::from(content))))
        .unwrap_or_else(|_| internal_error())
}

/// Serve a response from a cached file entry.
fn serve_cached_entry(
    entry: crate::file_cache::CacheEntry,
    accepts_gzip: bool,
    cache_control: &str,
    if_none_match: Option<&str>,
) -> Response<ServerBody> {
    // ETag check.
    if let Some(client_tag) = if_none_match {
        if etag_matches(&entry.etag, client_tag) {
            let mut builder = Response::builder().status(StatusCode::NOT_MODIFIED);
            builder = builder.header("ETag", entry.etag.as_str());
            if !cache_control.is_empty() {
                builder = builder.header("Cache-Control", cache_control);
            }
            return builder
                .body(body::buffered(Full::new(Bytes::new())))
                .unwrap_or_else(|_| internal_error());
        }
    }

    // Serve pre-compressed gzip variant if available.
    if accepts_gzip {
        if let Some(ref gzip) = entry.gzip_content {
            let mut builder = Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", entry.mime.as_str())
                .header("Content-Length", gzip.len())
                .header("Content-Encoding", "gzip")
                .header("Vary", "Accept-Encoding")
                .header("ETag", entry.etag.as_str());
            if !cache_control.is_empty() {
                builder = builder.header("Cache-Control", cache_control);
            }
            return builder
                .body(body::buffered(Full::new(gzip.clone())))
                .unwrap_or_else(|_| internal_error());
        }
    }

    // Serve from cached content if inlined.
    if let Some(ref content) = entry.content {
        let mut builder = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", entry.mime.as_str())
            .header("Content-Length", content.len())
            .header("ETag", entry.etag.as_str());
        if !cache_control.is_empty() {
            builder = builder.header("Cache-Control", cache_control);
        }
        return builder
            .body(body::buffered(Full::new(content.clone())))
            .unwrap_or_else(|_| internal_error());
    }

    // Content not cached (large file) — return metadata-only response.
    // Caller will need to read from disk or stream.
    // For now, return a response with just headers — the streaming path
    // (Phase 3b) will handle this case properly.
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", entry.mime.as_str())
        .header("Content-Length", entry.size)
        .header("ETag", entry.etag.as_str());
    if !cache_control.is_empty() {
        builder = builder.header("Cache-Control", cache_control);
    }
    builder
        .body(body::buffered(Full::new(Bytes::new())))
        .unwrap_or_else(|_| internal_error())
}

/// Serve a large file by streaming from disk.
///
/// Reads the file in chunks via [`body::streamed`], avoiding loading
/// the entire file into memory. Compression is skipped for streamed
/// files — large compressible files should use pre-compressed variants
/// via the file cache.
async fn serve_streamed(
    path: &Path,
    metadata: &std::fs::Metadata,
    mime: &str,
    cache_control: &str,
    if_none_match: Option<&str>,
) -> Response<ServerBody> {
    let size = metadata.len();
    let mtime = metadata.modified().ok();

    // Compute metadata-based ETag.
    let etag_value = mtime.map(|mt| {
        let secs = mt
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        format!("W/\"{secs:x}-{size:x}\"")
    });

    // ETag check.
    if let (Some(tag), Some(client_tag)) = (&etag_value, if_none_match) {
        if etag_matches(tag, client_tag) {
            let mut builder = Response::builder().status(StatusCode::NOT_MODIFIED);
            builder = builder.header("ETag", tag.as_str());
            if !cache_control.is_empty() {
                builder = builder.header("Cache-Control", cache_control);
            }
            return builder
                .body(body::buffered(Full::new(Bytes::new())))
                .unwrap_or_else(|_| internal_error());
        }
    }

    let Ok(file) = tokio::fs::File::open(path).await else {
        return not_found();
    };

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", mime)
        .header("Content-Length", size);
    if !cache_control.is_empty() {
        builder = builder.header("Cache-Control", cache_control);
    }
    if let Some(ref tag) = etag_value {
        builder = builder.header("ETag", tag.as_str());
    }
    builder
        .body(body::streamed(file))
        .unwrap_or_else(|_| internal_error())
}

/// Compute a weak `ETag` from file content using a fast hash.
fn compute_etag(content: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    hasher.write(content);
    format!("W/\"{:016x}\"", hasher.finish())
}

/// Check if a client's `If-None-Match` value matches our `ETag`.
///
/// Handles `*` and comma-separated lists of tags per HTTP spec.
fn etag_matches(etag: &str, if_none_match: &str) -> bool {
    let trimmed = if_none_match.trim();
    if trimmed == "*" {
        return true;
    }
    trimmed.split(',').any(|tag| tag.trim() == etag)
}

fn not_found() -> Response<ServerBody> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header("Content-Type", "text/plain")
        .body(body::buffered(Full::new(Bytes::from("404 Not Found"))))
        .unwrap()
}

fn forbidden() -> Response<ServerBody> {
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header("Content-Type", "text/plain")
        .body(body::buffered(Full::new(Bytes::from("403 Forbidden"))))
        .unwrap()
}

fn internal_error() -> Response<ServerBody> {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(body::buffered(Full::new(Bytes::from("Internal Server Error"))))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use http_body_util::BodyExt;

    use super::*;

    /// Collect a response body into a `Vec<u8>`.
    async fn body_bytes(resp: Response<ServerBody>) -> Vec<u8> {
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

    #[tokio::test]
    async fn test_serve_javascript_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("app.js"), "console.log('hi')").unwrap();
        let resp = serve(dir.path(), "/app.js").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp.headers()["content-type"].to_str().unwrap();
        assert!(
            ct.contains("javascript"),
            "expected javascript content-type, got {ct}"
        );
    }

    #[tokio::test]
    async fn test_serve_png_image() {
        let dir = tempfile::tempdir().unwrap();
        // Minimal PNG header.
        let png = b"\x89PNG\r\n\x1a\n";
        fs::write(dir.path().join("icon.png"), png).unwrap();
        let resp = serve(dir.path(), "/icon.png").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["content-type"], "image/png");
    }

    #[tokio::test]
    async fn test_serve_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("empty.txt"), "").unwrap();
        let resp = serve(dir.path(), "/empty.txt").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["content-length"], "0");
    }

    #[tokio::test]
    async fn test_serve_nested_path() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("assets/css")).unwrap();
        fs::write(dir.path().join("assets/css/main.css"), "body{}").unwrap();
        let resp = serve(dir.path(), "/assets/css/main.css").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["content-type"], "text/css");
    }

    #[tokio::test]
    async fn test_serve_binary_file_intact() {
        let dir = tempfile::tempdir().unwrap();
        let data: Vec<u8> = (0..=255).collect();
        fs::write(dir.path().join("binary.bin"), &data).unwrap();
        let resp = serve(dir.path(), "/binary.bin").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_bytes(resp).await, data);
    }
}
