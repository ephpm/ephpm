//! DigitalOcean DNS provider for the DNS-01 ACME lane.
//!
//! This is a second [`crate::dns01::DnsProvider`] implementation alongside
//! [`crate::dns01::CloudflareProvider`], selected by `[server.tls] dns_provider
//! = "digitalocean"`. It speaks the DigitalOcean v2 API
//! (<https://docs.digitalocean.com/reference/api/api-reference/#tag/Domain-Records>)
//! and does exactly what the [`DnsProvider`](crate::dns01::DnsProvider) seam
//! requires: publish and retract `_acme-challenge.<domain>` TXT records.
//!
//! ## DigitalOcean's data model vs Cloudflare's
//!
//! Two differences shape this code relative to the Cloudflare provider:
//!
//! - **Domains are top-level, records are relative.** Cloudflare has a numeric
//!   `zone_id`; DigitalOcean addresses a zone by its bare domain name in the URL
//!   path (`/domains/{domain}/records`) and the record's `name` is *relative* to
//!   that domain (`_acme-challenge.preview`, not the full FQDN). We resolve the
//!   registrable domain by walking the FQDN's parents and probing
//!   `GET /domains/{candidate}` — the longest parent that exists is the zone —
//!   then strip that suffix to form the relative record name.
//! - **Listing is paginated.** `delete_txt` reads back the TXT records to find
//!   the one it published; DigitalOcean paginates via `links.pages.next`, which
//!   we follow to completion so a zone with many records still finds the match.
//!
//! Everything else mirrors the Cloudflare provider: bearer auth, an `api_base`
//! override for the captured-server tests, `anyhow` context on every hop, and a
//! `Debug` impl that never renders the token.

use anyhow::{Context, bail};
use async_trait::async_trait;

use crate::dns01::DnsProvider;
use crate::tls::crypto_provider;

/// TXT record TTL requested from DigitalOcean, in seconds. Short, because the
/// record is deleted right after the challenge validates. DigitalOcean's floor
/// for a record TTL is 30 seconds, so this is the minimum it will accept.
const CHALLENGE_TXT_TTL_SECS: u32 = 30;

/// DigitalOcean API base URL. Overridable in tests via
/// [`DigitalOceanProvider::with_api_base`].
const DIGITALOCEAN_API_BASE: &str = "https://api.digitalocean.com/v2";

// ── DigitalOcean provider ────────────────────────────────────────────────────

/// A [`DnsProvider`] backed by the DigitalOcean v2 API.
///
/// Authenticates with a DigitalOcean personal access token (or an OAuth token)
/// carrying **write** scope on DNS. The token is never logged.
pub struct DigitalOceanProvider {
    client: reqwest::Client,
    token: String,
    api_base: String,
}

impl std::fmt::Debug for DigitalOceanProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render the token.
        f.debug_struct("DigitalOceanProvider")
            .field("api_base", &self.api_base)
            .finish_non_exhaustive()
    }
}

impl DigitalOceanProvider {
    /// Build a DigitalOcean provider from an API token.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTPS client cannot be constructed.
    pub fn new(token: String) -> anyhow::Result<Self> {
        let client = build_digitalocean_http_client()?;
        Ok(Self { client, token, api_base: DIGITALOCEAN_API_BASE.to_string() })
    }

    /// Override the API base URL. Test-only seam for a captured HTTP server.
    #[cfg(test)]
    #[must_use]
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }

    /// Resolve the DigitalOcean domain (zone) that `fqdn` lives under.
    ///
    /// DigitalOcean has no zone id; the domain *is* its name. We walk the FQDN's
    /// parent domains longest-first and probe `GET /domains/{candidate}`; the
    /// first that exists (HTTP 200) is the zone. A `404` means "not a domain on
    /// this account", so we try the next-shorter parent; any other status is a
    /// hard error (auth, rate limit, …).
    async fn domain_for(&self, fqdn: &str) -> anyhow::Result<String> {
        for candidate in domain_candidates(fqdn) {
            let url = format!("{}/domains/{candidate}", self.api_base);
            let resp = self
                .client
                .get(&url)
                .bearer_auth(&self.token)
                .send()
                .await
                .with_context(|| format!("DigitalOcean GET domains/{candidate} failed"))?;
            let status = resp.status();
            if status.is_success() {
                return Ok(candidate);
            }
            if status == reqwest::StatusCode::NOT_FOUND {
                continue;
            }
            let bytes =
                resp.bytes().await.context("reading DigitalOcean domain-lookup response")?;
            bail!(
                "DigitalOcean GET domains/{candidate} returned HTTP {status}: {}",
                extract_do_error(&bytes)
            );
        }
        bail!(
            "could not resolve a DigitalOcean domain for {fqdn}: no parent domain is registered on \
             this account"
        )
    }
}

#[async_trait]
impl DnsProvider for DigitalOceanProvider {
    async fn set_txt(&self, fqdn: &str, value: &str) -> anyhow::Result<()> {
        let domain = self.domain_for(fqdn).await?;
        let name = relative_record_name(fqdn, &domain);
        let url = format!("{}/domains/{domain}/records", self.api_base);
        let body = serde_json::json!({
            "type": "TXT",
            "name": name,
            "data": value,
            "ttl": CHALLENGE_TXT_TTL_SECS,
        });
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&body).expect("serializing a static json object cannot fail"))
            .send()
            .await
            .context("DigitalOcean create TXT record request failed")?;
        // The response carries the new record's id, but nothing downstream needs
        // it — appending never has to reference the created record, and deletion
        // re-discovers it by name+data. We only assert the create succeeded.
        ensure_do_success(resp, "create TXT record").await?;
        tracing::debug!(fqdn, "published DNS-01 challenge TXT record via DigitalOcean");
        Ok(())
    }

    async fn delete_txt(&self, fqdn: &str, value: &str) -> anyhow::Result<()> {
        let domain = self.domain_for(fqdn).await?;
        let name = relative_record_name(fqdn, &domain);

        // Page through the domain's TXT records, collecting every id whose
        // relative name and data match exactly what we published. Matching on
        // both is what lets the wildcard + apex pair (two TXT records at the same
        // name, different values) be retracted independently.
        let mut ids = Vec::new();
        let mut next = Some(format!("{}/domains/{domain}/records?type=TXT", self.api_base));
        while let Some(url) = next.take() {
            let resp = self
                .client
                .get(&url)
                .bearer_auth(&self.token)
                .send()
                .await
                .context("DigitalOcean list TXT records request failed")?;
            let bytes = ensure_do_success(resp, "list TXT records").await?;
            let parsed: DoRecordList = serde_json::from_slice(&bytes)
                .context("decoding DigitalOcean list-records response")?;
            for record in parsed.domain_records {
                if record.data == value && record.name.eq_ignore_ascii_case(&name) {
                    ids.push(record.id);
                }
            }
            next = parsed.links.pages.next.filter(|s| !s.is_empty());
        }

        for id in ids {
            let del_url = format!("{}/domains/{domain}/records/{id}", self.api_base);
            let resp = self
                .client
                .delete(&del_url)
                .bearer_auth(&self.token)
                .send()
                .await
                .context("DigitalOcean delete TXT record request failed")?;
            ensure_do_success(resp, "delete TXT record").await?;
        }
        tracing::debug!(fqdn, "retracted DNS-01 challenge TXT record via DigitalOcean");
        Ok(())
    }
}

/// Build the reqwest client for DigitalOcean with an explicit rustls config.
///
/// Identical rationale to the Cloudflare client: the workspace pins reqwest to
/// `rustls-tls-manual-roots-no-provider` (so it never drags in `ring` — see
/// [`crate::tls::crypto_provider`]), which means we must hand it a fully-built
/// [`rustls::ClientConfig`] — the shared aws-lc-rs provider plus the bundled
/// webpki roots.
fn build_digitalocean_http_client() -> anyhow::Result<reqwest::Client> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls = rustls::ClientConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()
        .context("crypto provider does not support the default TLS versions")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .build()
        .context("failed to build the DigitalOcean API HTTP client")
}

/// Generate the candidate domain apexes for `fqdn`, longest first.
///
/// For `_acme-challenge.preview.ephpm.dev` this yields
/// `["_acme-challenge.preview.ephpm.dev", "preview.ephpm.dev", "ephpm.dev"]`
/// (stopping at two labels, since a single-label TLD is never an account
/// domain). The caller probes each until one is a registered DigitalOcean
/// domain, which avoids bundling a public-suffix list.
fn domain_candidates(fqdn: &str) -> Vec<String> {
    let fqdn = fqdn.trim_end_matches('.');
    let labels: Vec<&str> = fqdn.split('.').collect();
    let mut out = Vec::new();
    // Keep suffixes with at least two labels.
    for start in 0..labels.len().saturating_sub(1) {
        out.push(labels[start..].join("."));
    }
    out
}

/// Compute the record `name` relative to its DigitalOcean domain.
///
/// DigitalOcean stores a record's name relative to the zone apex: the record at
/// `_acme-challenge.preview.ephpm.dev` in domain `ephpm.dev` has name
/// `_acme-challenge.preview`, and an apex record uses the sentinel `@`. The
/// suffix match is ASCII-case-insensitive because DNS names are.
fn relative_record_name(fqdn: &str, domain: &str) -> String {
    let fqdn = fqdn.trim_end_matches('.');
    let domain = domain.trim_end_matches('.');
    if fqdn.eq_ignore_ascii_case(domain) {
        return "@".to_string();
    }
    // `fqdn` must end in `.{domain}` for a relative name to exist. Since `domain`
    // is chosen from `fqdn`'s own parents this always holds, but guard anyway.
    let dotted_len = domain.len() + 1;
    if fqdn.len() > dotted_len
        && fqdn.as_bytes()[fqdn.len() - dotted_len] == b'.'
        && fqdn[fqdn.len() - domain.len()..].eq_ignore_ascii_case(domain)
    {
        return fqdn[..fqdn.len() - dotted_len].to_string();
    }
    // Fallback: not under `domain` — send the full name and let DigitalOcean
    // reject it, rather than silently mangling it.
    fqdn.to_string()
}

/// Consume a DigitalOcean response, returning its body bytes on success or a
/// contextual error (including DigitalOcean's own `message`) on any non-2xx.
async fn ensure_do_success(resp: reqwest::Response, action: &str) -> anyhow::Result<Vec<u8>> {
    let status = resp.status();
    let bytes =
        resp.bytes().await.with_context(|| format!("reading DigitalOcean {action} response"))?;
    if status.is_success() {
        return Ok(bytes.to_vec());
    }
    bail!("DigitalOcean API {action} failed (HTTP {status}): {}", extract_do_error(&bytes))
}

/// Pull the human-readable `message` out of a DigitalOcean error body, falling
/// back to the raw (possibly empty) payload when it isn't the expected shape.
fn extract_do_error(bytes: &[u8]) -> String {
    if let Ok(err) = serde_json::from_slice::<DoError>(bytes)
        && !err.message.is_empty()
    {
        return err.message;
    }
    String::from_utf8_lossy(bytes).trim().to_string()
}

// ── DigitalOcean API response envelopes ──────────────────────────────────────

/// A single DNS record as returned by the list endpoint. `id` is a numeric
/// record id; `name` is relative to the domain.
#[derive(serde::Deserialize)]
struct DoRecord {
    id: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    data: String,
}

/// The paginated list-records envelope.
#[derive(serde::Deserialize)]
struct DoRecordList {
    #[serde(default)]
    domain_records: Vec<DoRecord>,
    #[serde(default)]
    links: DoLinks,
}

#[derive(serde::Deserialize, Default)]
struct DoLinks {
    #[serde(default)]
    pages: DoPages,
}

#[derive(serde::Deserialize, Default)]
struct DoPages {
    /// Absolute URL of the next page, present only when more results remain.
    #[serde(default)]
    next: Option<String>,
}

/// The DigitalOcean error envelope (`{"id": "...", "message": "..."}`).
#[derive(serde::Deserialize)]
struct DoError {
    #[serde(default)]
    message: String,
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener as StdTcpListener;

    use super::*;

    #[test]
    fn domain_candidates_walks_parents_longest_first() {
        let got = domain_candidates("_acme-challenge.preview.ephpm.dev");
        assert_eq!(
            got,
            vec![
                "_acme-challenge.preview.ephpm.dev".to_string(),
                "preview.ephpm.dev".to_string(),
                "ephpm.dev".to_string(),
            ]
        );
    }

    #[test]
    fn domain_candidates_stops_at_two_labels() {
        assert_eq!(domain_candidates("ephpm.dev"), vec!["ephpm.dev".to_string()]);
        assert_eq!(domain_candidates("dev"), Vec::<String>::new());
    }

    #[test]
    fn relative_record_name_strips_the_domain_suffix() {
        assert_eq!(
            relative_record_name("_acme-challenge.preview.ephpm.dev", "ephpm.dev"),
            "_acme-challenge.preview"
        );
        assert_eq!(
            relative_record_name("_acme-challenge.ephpm.dev", "ephpm.dev"),
            "_acme-challenge"
        );
    }

    #[test]
    fn relative_record_name_apex_is_at_sign() {
        assert_eq!(relative_record_name("ephpm.dev", "ephpm.dev"), "@");
    }

    #[test]
    fn relative_record_name_is_case_insensitive() {
        assert_eq!(
            relative_record_name("_acme-challenge.Preview.EPHPM.dev", "ephpm.dev"),
            "_acme-challenge.Preview"
        );
    }

    /// A routing mock of the DigitalOcean API that captures every request and
    /// answers by method + path. It serves exactly `connections` sequential
    /// connections (reqwest opens a fresh one per call because we reply with
    /// `Connection: close`), then returns the captured request lines. This is
    /// the multi-request analogue of the Cloudflare provider's `one_shot_http`
    /// — DigitalOcean's provider makes several calls per operation (domain
    /// probing, then the mutation), so a single-shot server won't do.
    fn spawn_do_mock(connections: usize) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let mut captured = Vec::with_capacity(connections);
            for _ in 0..connections {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).expect("read");
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let (status, body) = route_do(&request);
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                stream.write_all(response.as_bytes()).expect("write");
                stream.flush().ok();
                captured.push(request);
            }
            captured
        });
        (format!("http://{addr}"), handle)
    }

    /// Decide the canned `(status_line, body)` for a captured request. Only the
    /// `ephpm.dev` domain "exists"; every longer parent probe 404s, which drives
    /// the provider's parent-walking resolution down to the real zone.
    fn route_do(request: &str) -> (&'static str, &'static str) {
        let line = request.lines().next().unwrap_or("");
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("");

        // Domain probe: GET /domains/<name> with no /records segment.
        if method == "GET" && path.starts_with("/domains/") && !path.contains("/records") {
            let name = path.trim_start_matches("/domains/");
            if name == "ephpm.dev" {
                return ("200 OK", r#"{"domain":{"name":"ephpm.dev","ttl":1800}}"#);
            }
            return (
                "404 Not Found",
                r#"{"id":"not_found","message":"The resource you were accessing could not be found."}"#,
            );
        }
        if method == "POST" && path.contains("/records") {
            return (
                "201 Created",
                r#"{"domain_record":{"id":98765,"type":"TXT","name":"_acme-challenge.preview","data":"the-txt-value","ttl":30}}"#,
            );
        }
        if method == "GET" && path.contains("/records") {
            return (
                "200 OK",
                r#"{"domain_records":[{"id":98765,"type":"TXT","name":"_acme-challenge.preview","data":"the-txt-value","ttl":30}],"links":{},"meta":{"total":1}}"#,
            );
        }
        if method == "DELETE" && path.contains("/records/") {
            return ("204 No Content", "");
        }
        ("400 Bad Request", r#"{"id":"bad_request","message":"unexpected request"}"#)
    }

    #[tokio::test]
    async fn digitalocean_set_txt_posts_the_right_record() {
        // 3 domain probes (2× 404, then ephpm.dev 200) + 1 POST = 4 connections.
        let (base, handle) = spawn_do_mock(4);
        let provider = DigitalOceanProvider::new("tok-secret".to_string())
            .expect("provider")
            .with_api_base(base);

        provider
            .set_txt("_acme-challenge.preview.ephpm.dev", "the-txt-value")
            .await
            .expect("set_txt");

        let requests = tokio::task::spawn_blocking(move || handle.join().expect("join"))
            .await
            .expect("captured requests");
        assert_eq!(requests.len(), 4, "unexpected request count: {requests:#?}");

        // Resolution starts at the longest parent.
        assert!(
            requests[0].starts_with("GET /domains/_acme-challenge.preview.ephpm.dev"),
            "first probe: {}",
            requests[0]
        );
        // The mutation lands on the resolved zone with a relative record name.
        let post = requests.iter().find(|r| r.starts_with("POST ")).expect("a POST request");
        assert!(post.starts_with("POST /domains/ephpm.dev/records"), "request line: {post}");
        assert!(post.contains("authorization: Bearer tok-secret"), "missing bearer: {post}");
        assert!(post.contains("content-type: application/json"), "missing content-type: {post}");
        assert!(post.contains(r#""name":"_acme-challenge.preview""#), "body: {post}");
        assert!(post.contains(r#""data":"the-txt-value""#), "body: {post}");
        assert!(post.contains(r#""type":"TXT""#), "body: {post}");
        assert!(post.contains(r#""ttl":30"#), "body: {post}");
    }

    #[tokio::test]
    async fn digitalocean_delete_txt_finds_and_deletes_by_name_and_data() {
        // 3 domain probes + 1 list + 1 delete = 5 connections.
        let (base, handle) = spawn_do_mock(5);
        let provider = DigitalOceanProvider::new("tok-secret".to_string())
            .expect("provider")
            .with_api_base(base);

        provider
            .delete_txt("_acme-challenge.preview.ephpm.dev", "the-txt-value")
            .await
            .expect("delete_txt");

        let requests = tokio::task::spawn_blocking(move || handle.join().expect("join"))
            .await
            .expect("captured requests");
        assert_eq!(requests.len(), 5, "unexpected request count: {requests:#?}");

        // The listing is scoped to TXT records on the resolved zone.
        let list = requests
            .iter()
            .find(|r| r.starts_with("GET /domains/ephpm.dev/records?type=TXT"))
            .expect("a TXT list request");
        assert!(list.contains("authorization: Bearer tok-secret"), "missing bearer: {list}");
        // The matched record is deleted by its numeric id.
        let del = requests.iter().find(|r| r.starts_with("DELETE ")).expect("a DELETE request");
        assert!(del.starts_with("DELETE /domains/ephpm.dev/records/98765"), "request line: {del}");
        assert!(del.contains("authorization: Bearer tok-secret"), "missing bearer: {del}");
    }

    #[tokio::test]
    async fn digitalocean_api_error_is_surfaced() {
        // A single probe that 404s everywhere means no domain resolves; the
        // resolution error is surfaced without ever reaching a mutation.
        let (base, handle) = spawn_do_mock(2);
        let provider =
            DigitalOceanProvider::new("bad".to_string()).expect("provider").with_api_base(base);

        let err = provider
            .set_txt("_acme-challenge.x.example", "v")
            .await
            .expect_err("must surface the resolution failure");
        let msg = format!("{err:#}");
        assert!(msg.contains("could not resolve a DigitalOcean domain"), "unexpected: {msg}");
        let _ = tokio::task::spawn_blocking(move || handle.join().ok()).await;
    }
}
