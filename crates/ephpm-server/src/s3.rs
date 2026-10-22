//! A minimal S3-compatible object client for KV snapshots.
//!
//! Five operations — `PUT`, `GET`, `HEAD`, `DELETE` and `ListObjectsV2` —
//! over reqwest with [`crate::sigv4`] signing. Deliberately not the AWS SDK
//! or `object_store`: both bring a second rustls crypto provider (`ring`)
//! through their default reqwest features, which the workspace's
//! single-provider rule forbids (#241), and the snapshot store needs exactly
//! these five calls. Works against S3, Cloudflare R2, MinIO, and Google
//! Cloud Storage's S3-compatible API (any SigV4 endpoint).
//!
//! Uploads and downloads stream from and to a file with
//! `x-amz-content-sha256: UNSIGNED-PAYLOAD`, so a gigabyte snapshot is never
//! held in memory or hashed twice; the transport (TLS) protects the bytes
//! and the snapshot's own AEAD verifies them on load.
//!
//! Credentials come from the standard `AWS_ACCESS_KEY_ID` /
//! `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN` variables only — never from
//! `ephpm.toml`, never logged. Instance-profile and web-identity credential
//! providers are **not** implemented; mount static credentials from a Secret.

use std::path::Path;

use anyhow::{Context as _, bail};
use tokio::io::AsyncWriteExt as _;

use crate::sigv4::{self, Credentials, SignInput, UNSIGNED_PAYLOAD, sign};
use crate::tls::crypto_provider;

/// Where the objects live.
#[derive(Debug, Clone)]
pub struct S3Location {
    /// Bucket name.
    pub bucket: String,
    /// Endpoint origin (`https://s3.us-east-1.amazonaws.com`,
    /// `http://minio:9000`, `https://<account>.r2.cloudflarestorage.com`).
    pub endpoint: String,
    /// SigV4 region (`us-east-1`; `auto` for R2).
    pub region: String,
    /// `true`: `endpoint/bucket/key`; `false`: `scheme://bucket.host/key`.
    pub path_style: bool,
}

impl S3Location {
    /// Parse `s3://bucket[/prefix]` into the bucket and the key prefix
    /// (without a trailing slash; empty when none).
    ///
    /// # Errors
    ///
    /// Anything that is not `s3://<bucket>[/<prefix>]`.
    pub fn parse_url(url: &str) -> anyhow::Result<(String, String)> {
        let rest = url.strip_prefix("s3://").context("expected an s3://bucket[/prefix] URL")?;
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        if bucket.is_empty()
            || !bucket
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
        {
            bail!("invalid S3 bucket name in {url:?}");
        }
        Ok((bucket.to_string(), prefix.trim_matches('/').to_string()))
    }

    /// The endpoint for `region` when none is configured: AWS S3.
    #[must_use]
    pub fn aws_endpoint(region: &str) -> String {
        format!("https://s3.{region}.amazonaws.com")
    }

    /// `(url, host)` for `key`.
    fn url_for(&self, key: &str) -> (String, String) {
        let (scheme, authority) =
            self.endpoint.split_once("://").unwrap_or(("https", &self.endpoint));
        let authority = authority.trim_end_matches('/');
        let encoded_key = sigv4::uri_encode(key, false);
        if self.path_style {
            (format!("{scheme}://{authority}/{}/{encoded_key}", self.bucket), authority.to_string())
        } else {
            let host = format!("{}.{authority}", self.bucket);
            (format!("{scheme}://{host}/{encoded_key}"), host)
        }
    }

    /// The signed path for `key` (what the canonical request carries).
    fn path_for(&self, key: &str) -> String {
        if self.path_style { format!("/{}/{key}", self.bucket) } else { format!("/{key}") }
    }
}

/// Metadata of one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    /// Full key.
    pub key: String,
    /// Size in bytes.
    pub size: u64,
    /// `LastModified` as the server reported it (RFC 3339 / HTTP-date).
    pub last_modified: String,
}

/// The client.
pub struct S3Client {
    http: reqwest::Client,
    location: S3Location,
    creds: Credentials,
}

impl std::fmt::Debug for S3Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Client").field("location", &self.location).finish_non_exhaustive()
    }
}

impl S3Client {
    /// Build a client. The HTTPS client uses the process crypto provider and
    /// the bundled webpki roots, like every other outbound client here.
    ///
    /// # Errors
    ///
    /// An HTTP client that cannot be built.
    pub fn new(location: S3Location, creds: Credentials) -> anyhow::Result<Self> {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls = rustls::ClientConfig::builder_with_provider(crypto_provider())
            .with_safe_default_protocol_versions()
            .context("crypto provider does not support the default TLS versions")?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let http = reqwest::Client::builder()
            .use_preconfigured_tls(tls)
            .user_agent(concat!("ephpm/", env!("CARGO_PKG_VERSION"), " (kv snapshot)"))
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()
            .context("failed to build the S3 HTTP client")?;
        Ok(Self { http, location, creds })
    }

    /// Where this client points.
    #[must_use]
    pub fn location(&self) -> &S3Location {
        &self.location
    }

    /// Build a signed request for `key` (or the bucket itself when `key` is
    /// empty) with `query`.
    fn request(
        &self,
        method: reqwest::Method,
        key: &str,
        query: &[(&str, &str)],
        extra_headers: &[(&str, &str)],
        payload_hash: &str,
    ) -> reqwest::RequestBuilder {
        let (mut url, host) = self.location.url_for(key);
        let path = self.location.path_for(key);
        let signed = sign(
            &self.creds,
            &SignInput {
                method: method.as_str(),
                host: &host,
                path: &path,
                query,
                extra_headers,
                payload_hash,
                send_content_sha256: true,
                region: &self.location.region,
                service: "s3",
                unix_secs: sigv4::now_unix_secs(),
            },
        );
        if !signed.canonical_query.is_empty() {
            url.push('?');
            url.push_str(&signed.canonical_query);
        }
        let mut req = self
            .http
            .request(method, url)
            .header("x-amz-date", signed.amz_date)
            .header(reqwest::header::AUTHORIZATION, signed.authorization);
        for (k, v) in signed.headers {
            req = req.header(k, v);
        }
        for (k, v) in extra_headers {
            req = req.header(*k, *v);
        }
        req
    }

    /// Upload `file` as `key`, streaming. One `PutObject`: the S3 single-PUT
    /// limit is 5 GiB, far above any `[kv.snapshot] max_size` worth
    /// loading at startup.
    ///
    /// # Errors
    ///
    /// Transport failure or a non-2xx answer (the body's first line is in
    /// the message; it names the S3 error code).
    pub async fn put_file(&self, key: &str, file: &Path) -> anyhow::Result<()> {
        let f = tokio::fs::File::open(file)
            .await
            .with_context(|| format!("open {}", file.display()))?;
        let len = f.metadata().await.context("stat the snapshot file")?.len();
        let len_s = len.to_string();
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(f));
        let resp = self
            .request(
                reqwest::Method::PUT,
                key,
                &[],
                &[("content-length", &len_s)],
                UNSIGNED_PAYLOAD,
            )
            .body(body)
            .send()
            .await
            .with_context(|| format!("PUT s3://{}/{key}", self.location.bucket))?;
        Self::check(resp, "PutObject", key).await.map(|_| ())
    }

    /// Download `key` into `file` (created or truncated), streaming. `Ok(None)`
    /// when the object does not exist.
    ///
    /// # Errors
    ///
    /// Transport failure, a non-2xx/404 answer, or a local write failure.
    pub async fn get_to_file(&self, key: &str, file: &Path) -> anyhow::Result<Option<u64>> {
        let resp = self
            .request(reqwest::Method::GET, key, &[], &[], UNSIGNED_PAYLOAD)
            .send()
            .await
            .with_context(|| format!("GET s3://{}/{key}", self.location.bucket))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let mut resp = Self::check(resp, "GetObject", key).await?;
        let mut out = tokio::fs::File::create(file)
            .await
            .with_context(|| format!("create {}", file.display()))?;
        let mut written = 0u64;
        while let Some(chunk) = resp.chunk().await.context("reading the object body")? {
            out.write_all(&chunk).await.with_context(|| format!("write {}", file.display()))?;
            written += chunk.len() as u64;
        }
        out.flush().await?;
        out.sync_all().await?;
        Ok(Some(written))
    }

    /// `HeadObject`: size and last-modified, or `None` when absent.
    ///
    /// # Errors
    ///
    /// Transport failure or a non-2xx/404 answer.
    pub async fn head(&self, key: &str) -> anyhow::Result<Option<ObjectMeta>> {
        let resp = self
            .request(reqwest::Method::HEAD, key, &[], &[], sigv4::EMPTY_SHA256)
            .send()
            .await
            .with_context(|| format!("HEAD s3://{}/{key}", self.location.bucket))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resp = Self::check(resp, "HeadObject", key).await?;
        let size = resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let last_modified = resp
            .headers()
            .get(reqwest::header::LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        Ok(Some(ObjectMeta { key: key.to_string(), size, last_modified }))
    }

    /// `DeleteObject`. Deleting an absent key is success (as S3 defines it).
    ///
    /// # Errors
    ///
    /// Transport failure or a non-2xx/404 answer.
    pub async fn delete(&self, key: &str) -> anyhow::Result<()> {
        let resp = self
            .request(reqwest::Method::DELETE, key, &[], &[], sigv4::EMPTY_SHA256)
            .send()
            .await
            .with_context(|| format!("DELETE s3://{}/{key}", self.location.bucket))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        Self::check(resp, "DeleteObject", key).await.map(|_| ())
    }

    /// `ListObjectsV2` under `prefix`, following continuation tokens.
    ///
    /// # Errors
    ///
    /// Transport failure, a non-2xx answer, or an unparsable listing.
    pub async fn list(&self, prefix: &str) -> anyhow::Result<Vec<ObjectMeta>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query: Vec<(&str, &str)> = vec![("list-type", "2"), ("prefix", prefix)];
            if let Some(t) = &token {
                query.push(("continuation-token", t));
            }
            let resp = self
                .request(reqwest::Method::GET, "", &query, &[], sigv4::EMPTY_SHA256)
                .send()
                .await
                .with_context(|| format!("LIST s3://{}/{prefix}", self.location.bucket))?;
            let resp = Self::check(resp, "ListObjectsV2", prefix).await?;
            let text = resp.text().await.context("reading the listing")?;
            out.extend(parse_listing(&text));
            token = inner_text(&text, "NextContinuationToken").map(xml_unescape);
            if token.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// Turn a non-2xx answer into an error naming the S3 error code.
    async fn check(
        resp: reqwest::Response,
        op: &str,
        key: &str,
    ) -> anyhow::Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        let code = inner_text(&body, "Code").unwrap_or("");
        let message = inner_text(&body, "Message").unwrap_or("");
        bail!("S3 {op} {key:?} failed [{status}] {}", format!("{code} {message}").trim())
    }
}

/// Parse the `<Contents>` entries of a `ListObjectsV2` response. The
/// listing is small, flat and ours (keys we wrote), so a scanner over the
/// three elements is enough — no XML crate.
fn parse_listing(xml: &str) -> Vec<ObjectMeta> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<Contents>") {
        let after = &rest[start + "<Contents>".len()..];
        let Some(end) = after.find("</Contents>") else { break };
        let block = &after[..end];
        if let Some(key) = inner_text(block, "Key") {
            out.push(ObjectMeta {
                key: xml_unescape(key),
                size: inner_text(block, "Size").and_then(|s| s.parse().ok()).unwrap_or(0),
                last_modified: inner_text(block, "LastModified").unwrap_or("").to_string(),
            });
        }
        rest = &after[end + "</Contents>".len()..];
    }
    out
}

/// Text between `<tag>` and `</tag>` (first occurrence).
fn inner_text<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(&xml[start..end])
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s3_url_parsing() {
        assert_eq!(
            S3Location::parse_url("s3://my-bucket").unwrap(),
            ("my-bucket".into(), String::new())
        );
        assert_eq!(
            S3Location::parse_url("s3://my-bucket/kv/snapshots/").unwrap(),
            ("my-bucket".into(), "kv/snapshots".into())
        );
        assert!(S3Location::parse_url("https://x").is_err());
        assert!(S3Location::parse_url("s3://Bad_Bucket").is_err());
        assert!(S3Location::parse_url("s3://").is_err());
    }

    #[test]
    fn path_and_virtual_host_urls() {
        let loc = S3Location {
            bucket: "b".into(),
            endpoint: "http://minio:9000".into(),
            region: "us-east-1".into(),
            path_style: true,
        };
        assert_eq!(
            loc.url_for("kv/a b.kvsnap"),
            ("http://minio:9000/b/kv/a%20b.kvsnap".into(), "minio:9000".into())
        );
        assert_eq!(loc.path_for("kv/a b.kvsnap"), "/b/kv/a b.kvsnap");
        let aws = S3Location {
            bucket: "b".into(),
            endpoint: S3Location::aws_endpoint("eu-west-1"),
            region: "eu-west-1".into(),
            path_style: false,
        };
        assert_eq!(
            aws.url_for("k"),
            (
                "https://b.s3.eu-west-1.amazonaws.com/k".into(),
                "b.s3.eu-west-1.amazonaws.com".into()
            )
        );
        assert_eq!(aws.path_for("k"), "/k");
        assert_eq!(aws.url_for("").0, "https://b.s3.eu-west-1.amazonaws.com/");
    }

    #[test]
    fn listing_parser_reads_contents_blocks() {
        let xml = r"<?xml version='1.0'?><ListBucketResult><Name>b</Name><IsTruncated>false</IsTruncated>
<Contents><Key>kv/a.kvsnap</Key><LastModified>2026-10-22T10:00:00.000Z</LastModified><Size>123</Size></Contents>
<Contents><Key>kv/b&amp;c.kvsnap</Key><LastModified>2026-10-22T11:00:00.000Z</LastModified><Size>456</Size></Contents>
</ListBucketResult>";
        let items = parse_listing(xml);
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0],
            ObjectMeta {
                key: "kv/a.kvsnap".into(),
                size: 123,
                last_modified: "2026-10-22T10:00:00.000Z".into()
            }
        );
        assert_eq!(items[1].key, "kv/b&c.kvsnap");
        assert_eq!(inner_text(xml, "NextContinuationToken"), None);
    }
}
