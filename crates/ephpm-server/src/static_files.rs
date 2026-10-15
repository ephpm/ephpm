//! Static file serving with MIME type detection and `ETag` support.
//!
//! Serves non-PHP files (CSS, JS, images, etc.) directly from the document root.

use std::hash::{DefaultHasher, Hasher};
use std::path::Path;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::{Response, StatusCode};

use crate::body::{self, ServerBody};

/// Serve a file from an already-resolved filesystem path.
///
/// The router has already verified the file exists and resolved it via
/// `fallback`. We still validate path traversal for defense in depth.
///
/// `canonical_root` must be the document root already passed through
/// `Path::canonicalize()` — the router caches that per site (the root is
/// constant for the process lifetime), saving a syscall on every static
/// hit. The per-request file path is still canonicalized here: that is
/// the traversal/symlink check and doubles as the existence probe.
///
/// `etag` (`[server.static] etag`) governs every static path alike: when it
/// is on, each response carries a weak `ETag` and a matching `if_none_match`
/// answers `304 Not Modified`; when it is off, no path sends an `ETag` or
/// answers `304`. Files up to 1 MiB read from disk get a content-hash tag;
/// streamed files and file-cache entries get an mtime+size tag. The caller
/// passes `if_none_match` only for GET/HEAD.
#[allow(clippy::too_many_arguments)]
pub async fn serve_file(
    canonical_root: &Path,
    file_path: &Path,
    accepts_gzip: bool,
    accepts_br: bool,
    cache_control: &str,
    compression: crate::router::CompressionSettings,
    etag: bool,
    if_none_match: Option<&str>,
    file_cache: Option<&crate::file_cache::FileCache>,
) -> Response<ServerBody> {
    // Security: ensure the resolved path is within the document root
    let Ok(canonical_file) = file_path.canonicalize() else {
        return not_found();
    };
    if !canonical_file.starts_with(canonical_root) {
        tracing::warn!(
            path = %file_path.display(),
            "path traversal attempt blocked"
        );
        return forbidden();
    }

    // `etag = false` turns conditional requests off on every path below, not
    // just the content-hash one.
    let if_none_match = if_none_match.filter(|_| etag);

    // Try the file cache first. Returns `None` for metadata-only entries
    // (large files without inlined content) so we fall through to streaming.
    if let Some(cache) = file_cache
        && let Some(entry) = cache.lookup(&canonical_file).await
        && let Some(resp) =
            serve_cached_entry(entry, accepts_gzip, cache_control, etag, if_none_match)
    {
        return resp;
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
            etag,
            if_none_match,
        )
        .await;
    }

    let Ok(content) = tokio::fs::read(&canonical_file).await else {
        return not_found();
    };

    // Insert into cache if enabled. For small files (which are inlined),
    // serve directly from the cache entry.
    if let Some(cache) = file_cache
        && let Ok(mtime) = metadata.modified()
    {
        let entry = cache.insert(&canonical_file, &content, mtime, mime, compression);
        if let Some(resp) =
            serve_cached_entry(entry, accepts_gzip, cache_control, etag, if_none_match)
        {
            return resp;
        }
    }

    // Uncached path — compute ETag from content hash.
    let etag_value = if etag { Some(compute_etag(&content)) } else { None };

    if let (Some(tag), Some(client_tag)) = (&etag_value, if_none_match)
        && if_none_match_matches(tag, client_tag)
    {
        let mut builder = Response::builder().status(StatusCode::NOT_MODIFIED);
        builder = builder.header("ETag", tag.as_str());
        if !cache_control.is_empty() {
            builder = builder.header("Cache-Control", cache_control);
        }
        return builder
            .body(body::buffered(Full::new(Bytes::new())))
            .unwrap_or_else(|_| internal_error());
    }

    // Prefer Brotli over gzip — better compression ratio for text assets.
    if accepts_br
        && let Some(compressed) = crate::router::brotli_compress(&content, mime, compression)
    {
        let mut builder = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", mime)
            .header("Content-Length", compressed.len())
            .header("Content-Encoding", "br")
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

    if accepts_gzip
        && let Some(compressed) = crate::router::gzip_compress(&content, mime, compression)
    {
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
///
/// Returns `None` when the cache entry has no inlined content (large
/// file), signalling the caller to fall through to the disk streaming
/// path.
fn serve_cached_entry(
    entry: crate::file_cache::CacheEntry,
    accepts_gzip: bool,
    cache_control: &str,
    etag: bool,
    if_none_match: Option<&str>,
) -> Option<Response<ServerBody>> {
    // ETag check — works even for metadata-only entries.
    if etag
        && let Some(client_tag) = if_none_match
        && if_none_match_matches(&entry.etag, client_tag)
    {
        let mut builder = Response::builder().status(StatusCode::NOT_MODIFIED);
        builder = builder.header("ETag", entry.etag.as_str());
        if !cache_control.is_empty() {
            builder = builder.header("Cache-Control", cache_control);
        }
        return Some(
            builder
                .body(body::buffered(Full::new(Bytes::new())))
                .unwrap_or_else(|_| internal_error()),
        );
    }

    // Serve pre-compressed gzip variant if available.
    if accepts_gzip && let Some(ref gzip) = entry.gzip_content {
        let mut builder = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", entry.mime.as_str())
            .header("Content-Length", gzip.len())
            .header("Content-Encoding", "gzip")
            .header("Vary", "Accept-Encoding");
        if etag {
            builder = builder.header("ETag", entry.etag.as_str());
        }
        if !cache_control.is_empty() {
            builder = builder.header("Cache-Control", cache_control);
        }
        return Some(
            builder
                .body(body::buffered(Full::new(gzip.clone())))
                .unwrap_or_else(|_| internal_error()),
        );
    }

    // Serve from cached content if inlined.
    if let Some(ref content) = entry.content {
        let mut builder = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", entry.mime.as_str())
            .header("Content-Length", content.len());
        if etag {
            builder = builder.header("ETag", entry.etag.as_str());
        }
        if !cache_control.is_empty() {
            builder = builder.header("Cache-Control", cache_control);
        }
        return Some(
            builder
                .body(body::buffered(Full::new(content.clone())))
                .unwrap_or_else(|_| internal_error()),
        );
    }

    // Content not cached (large file) — signal caller to use the
    // disk streaming path instead of returning an empty body.
    None
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
    content_type: &str,
    cache_control: &str,
    etag: bool,
    if_none_match: Option<&str>,
) -> Response<ServerBody> {
    let size = metadata.len();
    let modified_time = metadata.modified().ok().filter(|_| etag);

    // Compute metadata-based ETag (none when `etag` is off).
    let etag_value = modified_time.map(|mt| {
        let secs =
            mt.duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
        format!("W/\"{secs:x}-{size:x}\"")
    });

    // ETag check.
    if let (Some(tag), Some(client_tag)) = (&etag_value, if_none_match)
        && if_none_match_matches(tag, client_tag)
    {
        let mut builder = Response::builder().status(StatusCode::NOT_MODIFIED);
        builder = builder.header("ETag", tag.as_str());
        if !cache_control.is_empty() {
            builder = builder.header("Cache-Control", cache_control);
        }
        return builder
            .body(body::buffered(Full::new(Bytes::new())))
            .unwrap_or_else(|_| internal_error());
    }

    let Ok(file) = tokio::fs::File::open(path).await else {
        return not_found();
    };

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", content_type)
        .header("Content-Length", size);
    if !cache_control.is_empty() {
        builder = builder.header("Cache-Control", cache_control);
    }
    if let Some(ref tag) = etag_value {
        builder = builder.header("ETag", tag.as_str());
    }
    builder.body(body::streamed(file)).unwrap_or_else(|_| internal_error())
}

/// Compute a weak `ETag` from file content using a fast hash.
fn compute_etag(content: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    hasher.write(content);
    format!("W/\"{:016x}\"", hasher.finish())
}

/// Whether an `If-None-Match` header value matches `etag`, per RFC 9110
/// §13.1.2: `*` matches any current representation, otherwise the header is a
/// comma-separated list of entity tags compared with the **weak** comparison
/// function — `W/"x"` and `"x"` match each other. Commas inside a quoted tag
/// are part of the tag, not separators.
///
/// One rule for the static-file 304s and the PHP ETag cache. Exact string
/// comparison (what both did before) missed the weak/strong pairing: a client
/// or CDN that strips or adds the `W/` prefix never got a 304.
pub(crate) fn if_none_match_matches(etag: &str, if_none_match: &str) -> bool {
    let header = if_none_match.trim();
    if header == "*" {
        return true;
    }
    let ours = opaque_tag(etag.trim());
    if ours.is_empty() {
        return false;
    }
    entity_tags(header).into_iter().any(|tag| opaque_tag(tag) == ours)
}

/// The opaque part of an entity tag: the tag without its `W/` weakness
/// indicator, which is all weak comparison looks at.
fn opaque_tag(tag: &str) -> &str {
    tag.strip_prefix("W/").unwrap_or(tag)
}

/// Split an `If-None-Match` list into its entity tags, honouring quotes.
fn entity_tags(header: &str) -> Vec<&str> {
    let mut tags = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    for (i, c) in header.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                tags.push(header[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    tags.push(header[start..].trim());
    tags.retain(|tag| !tag.is_empty());
    tags
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

    /// Default compression settings (disabled) for tests.
    fn no_compression() -> crate::router::CompressionSettings {
        crate::router::CompressionSettings {
            enabled: false,
            level: 6,
            min_size: 1024,
            streaming: crate::router::StreamingCompression::Off,
        }
    }

    /// Collect a response body into a `Vec<u8>`.
    async fn body_bytes(resp: Response<ServerBody>) -> Vec<u8> {
        resp.into_body().collect().await.unwrap().to_bytes().to_vec()
    }

    /// Helper: serve a file using `serve_file` with default settings.
    ///
    /// Mirrors the router's contract: the document root is canonicalized
    /// once by the caller (the router caches it), not inside `serve_file`.
    async fn serve_test(docroot: &Path, filename: &str) -> Response<ServerBody> {
        let canonical_root = docroot.canonicalize().expect("canonicalize test docroot");
        let file_path = docroot.join(filename);
        serve_file(
            &canonical_root,
            &file_path,
            false,
            false,
            "",
            no_compression(),
            false,
            None,
            None,
        )
        .await
    }

    #[tokio::test]
    async fn test_serve_html_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("page.html"), "<h1>Hello</h1>").unwrap();

        let resp = serve_test(dir.path(), "page.html").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["content-type"], "text/html");
        assert_eq!(body_bytes(resp).await, b"<h1>Hello</h1>");
    }

    #[tokio::test]
    async fn test_serve_css_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("style.css"), "body{}").unwrap();

        let resp = serve_test(dir.path(), "style.css").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["content-type"], "text/css");
    }

    #[tokio::test]
    async fn test_serve_content_length_header() {
        let dir = tempfile::tempdir().unwrap();
        let content = "twelve chars";
        fs::write(dir.path().join("test.txt"), content).unwrap();

        let resp = serve_test(dir.path(), "test.txt").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["content-length"], content.len().to_string().as_str());
    }

    #[tokio::test]
    async fn test_serve_unknown_extension() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("data.ephpmtest"), "binary").unwrap();

        let resp = serve_test(dir.path(), "data.ephpmtest").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["content-type"], "application/octet-stream");
    }

    #[tokio::test]
    async fn test_serve_missing_file_returns_404() {
        let dir = tempfile::tempdir().unwrap();

        let resp = serve_test(dir.path(), "nonexistent.txt").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_serve_path_traversal_blocked() {
        let parent = tempfile::tempdir().unwrap();
        let docroot = parent.path().join("www");
        fs::create_dir(&docroot).unwrap();
        // Create a file outside docroot
        fs::write(parent.path().join("secret.txt"), "secret").unwrap();

        let canonical_root = docroot.canonicalize().unwrap();
        let file_path = docroot.join("..").join("secret.txt");
        let resp = serve_file(
            &canonical_root,
            &file_path,
            false,
            false,
            "",
            no_compression(),
            false,
            None,
            None,
        )
        .await;
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
        let resp = serve_test(dir.path(), "app.js").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp.headers()["content-type"].to_str().unwrap();
        assert!(ct.contains("javascript"), "expected javascript content-type, got {ct}");
    }

    #[tokio::test]
    async fn test_serve_png_image() {
        let dir = tempfile::tempdir().unwrap();
        // Minimal PNG header.
        let png = b"\x89PNG\r\n\x1a\n";
        fs::write(dir.path().join("icon.png"), png).unwrap();
        let resp = serve_test(dir.path(), "icon.png").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["content-type"], "image/png");
    }

    #[tokio::test]
    async fn test_serve_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("empty.txt"), "").unwrap();
        let resp = serve_test(dir.path(), "empty.txt").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["content-length"], "0");
    }

    #[tokio::test]
    async fn test_serve_nested_path() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("assets/css")).unwrap();
        fs::write(dir.path().join("assets/css/main.css"), "body{}").unwrap();
        let resp = serve_test(dir.path(), "assets/css/main.css").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["content-type"], "text/css");
    }

    #[tokio::test]
    async fn test_serve_binary_file_intact() {
        let dir = tempfile::tempdir().unwrap();
        let data: Vec<u8> = (0..=255).collect();
        fs::write(dir.path().join("binary.bin"), &data).unwrap();
        let resp = serve_test(dir.path(), "binary.bin").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_bytes(resp).await, data);
    }

    #[tokio::test]
    async fn test_etag_matches_wildcard() {
        assert!(if_none_match_matches("W/\"abc\"", "*"));
    }

    #[tokio::test]
    async fn test_etag_matches_exact() {
        assert!(if_none_match_matches("W/\"abc\"", "W/\"abc\""));
    }

    #[tokio::test]
    async fn test_etag_matches_list() {
        assert!(if_none_match_matches("W/\"b\"", "W/\"a\", W/\"b\", W/\"c\""));
    }

    #[tokio::test]
    async fn test_etag_no_match() {
        assert!(!if_none_match_matches("W/\"x\"", "W/\"a\", W/\"b\""));
    }

    /// RFC 9110 §13.1.2: If-None-Match uses WEAK comparison, so the `W/`
    /// prefix is ignored on either side.
    #[test]
    fn if_none_match_uses_weak_comparison() {
        assert!(if_none_match_matches("W/\"abc\"", "\"abc\""));
        assert!(if_none_match_matches("\"abc\"", "W/\"abc\""));
        assert!(if_none_match_matches("\"abc\"", " \"x\" , W/\"abc\" "));
        assert!(!if_none_match_matches("W/\"abc\"", "\"abd\""));
    }

    /// A comma inside a quoted tag is part of the tag (`etagc` allows it).
    #[test]
    fn if_none_match_does_not_split_inside_quotes() {
        assert!(if_none_match_matches("\"a,b\"", "\"x\", \"a,b\""));
        assert!(!if_none_match_matches("\"a\"", "\"a,b\""));
        assert!(!if_none_match_matches("\"b\"", "\"a,b\""));
    }

    #[test]
    fn if_none_match_never_matches_an_empty_tag() {
        assert!(!if_none_match_matches("", "\"a\""));
        assert!(!if_none_match_matches("\"a\"", ""));
        assert!(!if_none_match_matches("\"a\"", " , "));
    }

    /// Serve `name` with explicit `etag` / `If-None-Match` and an optional
    /// file cache.
    async fn serve_conditional(
        docroot: &Path,
        name: &str,
        etag: bool,
        if_none_match: Option<&str>,
        file_cache: Option<&crate::file_cache::FileCache>,
    ) -> Response<ServerBody> {
        let canonical_root = docroot.canonicalize().expect("canonicalize test docroot");
        serve_file(
            &canonical_root,
            &docroot.join(name),
            false,
            false,
            "",
            no_compression(),
            etag,
            if_none_match,
            file_cache,
        )
        .await
    }

    fn file_cache() -> crate::file_cache::FileCache {
        crate::file_cache::FileCache::new(&ephpm_config::FileCacheConfig {
            enabled: true,
            ..ephpm_config::FileCacheConfig::default()
        })
    }

    /// Every static path — small file, streamed large file, file-cache hit —
    /// sends an `ETag` and answers a matching `If-None-Match` with 304 when
    /// `etag` is on.
    #[tokio::test]
    async fn every_static_path_honours_if_none_match_when_etag_is_on() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("small.txt"), "small").unwrap();
        fs::write(dir.path().join("large.bin"), vec![7u8; 1_048_577]).unwrap();
        let cache = file_cache();

        for (name, cache) in [("small.txt", None), ("large.bin", None), ("small.txt", Some(&cache))]
        {
            let first = serve_conditional(dir.path(), name, true, None, cache).await;
            assert_eq!(first.status(), StatusCode::OK, "{name}");
            let tag = first.headers()["etag"].to_str().unwrap().to_owned();
            let again = serve_conditional(dir.path(), name, true, Some(&tag), cache).await;
            assert_eq!(
                again.status(),
                StatusCode::NOT_MODIFIED,
                "{name} (cache: {})",
                cache.is_some()
            );
            // The strong spelling of the same weak tag matches too.
            let strong = tag.trim_start_matches("W/").to_owned();
            let again = serve_conditional(dir.path(), name, true, Some(&strong), cache).await;
            assert_eq!(again.status(), StatusCode::NOT_MODIFIED, "{name} strong form");
        }
    }

    /// `etag = false` used to drop the `ETag` only on small uncached files:
    /// streamed files and file-cache hits still sent one, while the router no
    /// longer passed `If-None-Match`, so they advertised a validator they
    /// would never honour. Now no static path sends an `ETag` or a 304.
    #[tokio::test]
    async fn etag_off_disables_etags_and_304s_on_every_static_path() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("small.txt"), "small").unwrap();
        fs::write(dir.path().join("large.bin"), vec![7u8; 1_048_577]).unwrap();
        let cache = file_cache();

        for (name, cache) in [("small.txt", None), ("large.bin", None), ("small.txt", Some(&cache))]
        {
            for inm in [None, Some("*")] {
                // Twice, so the second file-cache request is a cache hit.
                for _ in 0..2 {
                    let resp = serve_conditional(dir.path(), name, false, inm, cache).await;
                    assert_eq!(resp.status(), StatusCode::OK, "{name} {inm:?}");
                    assert!(
                        resp.headers().get("etag").is_none(),
                        "{name} must not send an ETag with etag = false (cache: {})",
                        cache.is_some()
                    );
                }
            }
        }
    }
}
