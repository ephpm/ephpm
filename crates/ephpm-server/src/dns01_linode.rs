//! Linode DNS provider for the DNS-01 ACME lane.
//!
//! A second [`crate::dns01::DnsProvider`] implementation alongside
//! [`crate::dns01::CloudflareProvider`], backed by the **Linode API v4**
//! (`https://api.linode.com/v4`). It is selected by `[server.tls] dns_provider
//! = "linode"`; the wiring that maps that config string to this type lives in
//! `start_dns01_acme` and is added separately.
//!
//! ## Shape of the API
//!
//! Linode differs from Cloudflare in two ways that matter here:
//!
//! - **Domains are addressed by numeric id, not by name.** There is no
//!   name-in-URL record endpoint, so every operation first resolves the FQDN to
//!   the owning domain's id by listing domains (walking parent domains exactly
//!   like the Cloudflare provider, via [`parent_domain_candidates`]). The
//!   listing is narrowed with an `X-Filter` header so a busy account does not
//!   have to be paginated.
//! - **Errors are HTTP status codes, not a `success` flag.** A failed call
//!   returns a non-2xx status with an `{"errors":[{"reason":...}]}` body, which
//!   [`read_success`] turns into an `anyhow` error.
//!
//! Record `name`s on Linode are **relative to the domain** (`_acme-challenge`
//! under `preview.ephpm.dev`, or `_acme-challenge.preview` under `ephpm.dev`) —
//! [`relative_record_name`] computes that from the absolute FQDN and the
//! resolved domain.
//!
//! ## Append semantics
//!
//! Per the [`DnsProvider`] contract, `set_txt` **appends** a TXT record and does
//! not clobber an existing one at the same name — a wildcard order plus its bare
//! apex publish two challenges at the *same* `_acme-challenge.<domain>` name with
//! different values, and both must be live at once. Linode allows multiple TXT
//! records at one name, so a plain `POST` is enough. `delete_txt` lists the
//! records at that name and removes only the one whose `target` matches the value
//! it published.
//!
//! The token is never logged.

use anyhow::{Context, bail};
use async_trait::async_trait;
use serde::Deserialize;

use crate::dns01::DnsProvider;
use crate::tls::crypto_provider;

/// TXT record TTL requested from Linode, in seconds. Short, because the record
/// is deleted right after the challenge validates.
const CHALLENGE_TXT_TTL_SECS: u32 = 30;

/// Linode API v4 base URL. Overridable in tests via
/// [`LinodeProvider::with_api_base`].
const LINODE_API_BASE: &str = "https://api.linode.com/v4";

// ── Linode provider ──────────────────────────────────────────────────────────

/// A [`DnsProvider`] backed by the Linode API v4.
///
/// Authenticates with a personal access token (scoped to `domains:read_write`)
/// sent as `Authorization: Bearer <token>`. The token is never logged.
pub struct LinodeProvider {
    client: reqwest::Client,
    token: String,
    api_base: String,
}

impl std::fmt::Debug for LinodeProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render the token.
        f.debug_struct("LinodeProvider").field("api_base", &self.api_base).finish_non_exhaustive()
    }
}

impl LinodeProvider {
    /// Build a Linode provider from an API token.
    ///
    /// The token is the only credential — the domain id is always resolved from
    /// the challenge FQDN at call time.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTPS client cannot be constructed.
    pub fn new(token: String) -> anyhow::Result<Self> {
        let client = build_linode_http_client()?;
        Ok(Self { client, token, api_base: LINODE_API_BASE.to_string() })
    }

    /// Override the API base URL. Test-only seam for a captured HTTP server.
    #[must_use]
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }

    /// Resolve the `(domain_id, domain_name)` that owns `fqdn`.
    ///
    /// Walks parent domains longest-first ([`parent_domain_candidates`]) and,
    /// for each, lists domains narrowed by an `X-Filter` header until one
    /// matches. Mirrors the Cloudflare provider: it trusts the server-side
    /// filter and takes the first returned domain rather than re-checking the
    /// name locally.
    async fn domain_for(&self, fqdn: &str) -> anyhow::Result<(u64, String)> {
        for candidate in parent_domain_candidates(fqdn) {
            let url = format!("{}/domains", self.api_base);
            let filter = serde_json::json!({ "domain": candidate }).to_string();
            let resp = self
                .client
                .get(&url)
                .bearer_auth(&self.token)
                .header("X-Filter", filter)
                .send()
                .await
                .with_context(|| format!("Linode GET domains (filter {candidate}) failed"))?;
            let bytes = read_success(resp, "list domains").await?;
            let parsed: LinodePage<LinodeDomain> =
                serde_json::from_slice(&bytes).context("decoding Linode domains response")?;
            if let Some(domain) = parsed.data.into_iter().next() {
                return Ok((domain.id, domain.domain));
            }
        }
        bail!(
            "could not resolve a Linode domain for {fqdn}: no parent domain is a zone on this \
             account"
        )
    }
}

#[async_trait]
impl DnsProvider for LinodeProvider {
    async fn set_txt(&self, fqdn: &str, value: &str) -> anyhow::Result<()> {
        let (domain_id, domain_name) = self.domain_for(fqdn).await?;
        let name = relative_record_name(fqdn, &domain_name);
        let url = format!("{}/domains/{domain_id}/records", self.api_base);
        let body = serde_json::json!({
            "type": "TXT",
            "name": name,
            "target": value,
            "ttl_sec": CHALLENGE_TXT_TTL_SECS,
        });
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&body).expect("serializing a static json object cannot fail"))
            .send()
            .await
            .context("Linode create TXT record request failed")?;
        read_success(resp, "create TXT record").await?;
        tracing::debug!(fqdn, "published DNS-01 challenge TXT record via Linode");
        Ok(())
    }

    async fn delete_txt(&self, fqdn: &str, value: &str) -> anyhow::Result<()> {
        let (domain_id, domain_name) = self.domain_for(fqdn).await?;
        let name = relative_record_name(fqdn, &domain_name);
        // List the TXT records at this name, then delete the one whose target
        // matches — leaving any sibling challenge record at the same name alone.
        let list_url = format!("{}/domains/{domain_id}/records", self.api_base);
        let filter = serde_json::json!({ "type": "TXT", "name": name }).to_string();
        let resp = self
            .client
            .get(&list_url)
            .bearer_auth(&self.token)
            .header("X-Filter", filter)
            .send()
            .await
            .context("Linode list records request failed")?;
        let bytes = read_success(resp, "list records").await?;
        let parsed: LinodePage<LinodeRecord> =
            serde_json::from_slice(&bytes).context("decoding Linode records response")?;
        for record in parsed.data {
            if record.target != value || record.name != name {
                continue;
            }
            let del_url = format!("{}/domains/{domain_id}/records/{}", self.api_base, record.id);
            let resp = self
                .client
                .delete(&del_url)
                .bearer_auth(&self.token)
                .send()
                .await
                .context("Linode delete TXT record request failed")?;
            read_success(resp, "delete TXT record").await?;
        }
        tracing::debug!(fqdn, "retracted DNS-01 challenge TXT record via Linode");
        Ok(())
    }
}

/// Build the reqwest client for Linode with an explicit rustls config.
///
/// Mirrors the Cloudflare client: the workspace pins reqwest to
/// `rustls-tls-manual-roots-no-provider` (so it never drags in `ring`), which
/// means the client must be handed a fully-built [`rustls::ClientConfig`] — the
/// shared aws-lc-rs provider ([`crate::tls::crypto_provider`]) plus the bundled
/// webpki roots.
fn build_linode_http_client() -> anyhow::Result<reqwest::Client> {
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
        .context("failed to build the Linode API HTTP client")
}

/// Read a Linode API response, turning a non-2xx status into an `anyhow` error.
///
/// Unlike Cloudflare (which always answers `200` with a `success` flag), Linode
/// signals failure with the HTTP status code and an `{"errors":[...]}` body.
async fn read_success(resp: reqwest::Response, action: &str) -> anyhow::Result<Vec<u8>> {
    let status = resp.status();
    let bytes =
        resp.bytes().await.with_context(|| format!("reading Linode {action} response"))?.to_vec();
    if status.is_success() {
        return Ok(bytes);
    }
    let detail = serde_json::from_slice::<LinodeErrors>(&bytes)
        .ok()
        .map(|e| e.errors.into_iter().map(|x| x.reason).collect::<Vec<_>>().join("; "))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned());
    bail!("Linode API {action} failed ({status}): {detail}")
}

/// Generate the candidate domain apexes for `fqdn`, longest first.
///
/// For `_acme-challenge.preview.ephpm.dev` this yields
/// `["_acme-challenge.preview.ephpm.dev", "preview.ephpm.dev", "ephpm.dev"]`
/// (stopping at two labels, since a single-label TLD is never an account
/// domain). The caller lists each until one matches an actual domain, which
/// avoids bundling a public-suffix list. Identical to the Cloudflare provider's
/// walk.
fn parent_domain_candidates(fqdn: &str) -> Vec<String> {
    let fqdn = fqdn.trim_end_matches('.');
    let labels: Vec<&str> = fqdn.split('.').collect();
    let mut out = Vec::new();
    // Keep suffixes with at least two labels.
    for start in 0..labels.len().saturating_sub(1) {
        out.push(labels[start..].join("."));
    }
    out
}

/// Compute the record name **relative to the domain**, as Linode stores it.
///
/// `_acme-challenge.preview.ephpm.dev` under domain `ephpm.dev` becomes
/// `_acme-challenge.preview`; under `preview.ephpm.dev` it becomes
/// `_acme-challenge`. An FQDN equal to the domain (an apex record) yields an
/// empty string. Comparison is case-insensitive because the domain name comes
/// back from the API and may differ in case from the challenge FQDN. If the
/// FQDN is not actually under the domain, it is returned unchanged.
fn relative_record_name(fqdn: &str, domain: &str) -> String {
    let fqdn = fqdn.trim_end_matches('.');
    if fqdn.eq_ignore_ascii_case(domain) {
        return String::new();
    }
    let suffix = format!(".{domain}");
    if fqdn.len() > suffix.len() && fqdn[fqdn.len() - suffix.len()..].eq_ignore_ascii_case(&suffix)
    {
        return fqdn[..fqdn.len() - suffix.len()].to_string();
    }
    fqdn.to_string()
}

// ── Linode API response envelopes ────────────────────────────────────────────

/// A paginated Linode list response; only the `data` array is read.
#[derive(Deserialize)]
struct LinodePage<T> {
    #[serde(default = "Vec::new")]
    data: Vec<T>,
}

#[derive(Deserialize)]
struct LinodeDomain {
    id: u64,
    domain: String,
}

#[derive(Deserialize)]
struct LinodeRecord {
    id: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    target: String,
}

#[derive(Deserialize)]
struct LinodeErrors {
    #[serde(default)]
    errors: Vec<LinodeApiError>,
}

#[derive(Deserialize)]
struct LinodeApiError {
    reason: String,
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener as StdTcpListener;
    use std::thread::JoinHandle;

    use super::*;

    #[test]
    fn parent_domain_candidates_walks_parents_longest_first() {
        assert_eq!(
            parent_domain_candidates("_acme-challenge.preview.ephpm.dev"),
            vec![
                "_acme-challenge.preview.ephpm.dev".to_string(),
                "preview.ephpm.dev".to_string(),
                "ephpm.dev".to_string(),
            ]
        );
    }

    #[test]
    fn parent_domain_candidates_stops_at_two_labels() {
        assert_eq!(parent_domain_candidates("ephpm.dev"), vec!["ephpm.dev".to_string()]);
        assert_eq!(parent_domain_candidates("dev"), Vec::<String>::new());
    }

    #[test]
    fn relative_record_name_is_relative_to_the_domain() {
        assert_eq!(
            relative_record_name("_acme-challenge.preview.ephpm.dev", "ephpm.dev"),
            "_acme-challenge.preview"
        );
        assert_eq!(
            relative_record_name("_acme-challenge.preview.ephpm.dev", "preview.ephpm.dev"),
            "_acme-challenge"
        );
    }

    #[test]
    fn relative_record_name_apex_is_empty_and_case_insensitive() {
        assert_eq!(relative_record_name("ephpm.dev", "ephpm.dev"), "");
        assert_eq!(
            relative_record_name("_acme-challenge.PREVIEW.Ephpm.Dev", "ephpm.dev"),
            "_acme-challenge.PREVIEW"
        );
    }

    /// A one-shot HTTP server that answers `responses.len()` sequential
    /// requests, each on a fresh connection (`Connection: close`), and returns
    /// every captured request. Linode flows make more than one call — domain
    /// resolution then the record operation — so unlike the Cloudflare test's
    /// single-shot server this one scripts a small sequence.
    fn mock_http(responses: Vec<(u16, &'static str)>) -> (String, JoinHandle<Vec<String>>) {
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let mut captured = Vec::new();
            for (code, body) in responses {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).expect("read");
                captured.push(String::from_utf8_lossy(&buf[..n]).to_string());
                let reason = match code {
                    200 => "OK",
                    401 => "Unauthorized",
                    _ => "Error",
                };
                let response = format!(
                    "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                stream.write_all(response.as_bytes()).expect("write");
                stream.flush().ok();
            }
            captured
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn set_txt_resolves_domain_then_posts_record() {
        let domains = r#"{"data":[{"id":4321,"domain":"ephpm.dev"}],"page":1,"pages":1}"#;
        let created =
            r#"{"id":99,"type":"TXT","name":"_acme-challenge.preview","target":"the-txt-value"}"#;
        let (base, handle) = mock_http(vec![(200, domains), (200, created)]);
        let provider =
            LinodeProvider::new("tok-secret".to_string()).expect("provider").with_api_base(base);

        provider
            .set_txt("_acme-challenge.preview.ephpm.dev", "the-txt-value")
            .await
            .expect("set_txt");

        let reqs = tokio::task::spawn_blocking(move || handle.join().expect("join"))
            .await
            .expect("captured requests");
        assert_eq!(reqs.len(), 2, "expected a domain lookup then a create");

        // 1) Domain resolution.
        assert!(reqs[0].starts_with("GET /domains"), "domain lookup line: {}", reqs[0]);
        assert!(
            reqs[0].contains("authorization: Bearer tok-secret"),
            "missing bearer: {}",
            reqs[0]
        );

        // 2) Record creation, addressed by the resolved numeric domain id.
        assert!(reqs[1].starts_with("POST /domains/4321/records"), "create line: {}", reqs[1]);
        assert!(
            reqs[1].contains("authorization: Bearer tok-secret"),
            "missing bearer: {}",
            reqs[1]
        );
        assert!(reqs[1].contains("content-type: application/json"), "missing content-type");
        assert!(reqs[1].contains("\"type\":\"TXT\""), "body: {}", reqs[1]);
        assert!(reqs[1].contains("\"name\":\"_acme-challenge.preview\""), "body: {}", reqs[1]);
        assert!(reqs[1].contains("\"target\":\"the-txt-value\""), "body: {}", reqs[1]);
        assert!(reqs[1].contains("\"ttl_sec\":30"), "body: {}", reqs[1]);
    }

    #[tokio::test]
    async fn delete_txt_deletes_only_the_matching_target() {
        let domains = r#"{"data":[{"id":4321,"domain":"ephpm.dev"}],"page":1,"pages":1}"#;
        // Two challenge records at the same name (wildcard + apex); only the one
        // whose target matches must be deleted.
        let records = r#"{"data":[
            {"id":11,"type":"TXT","name":"_acme-challenge.preview","target":"other-value"},
            {"id":22,"type":"TXT","name":"_acme-challenge.preview","target":"the-txt-value"}
        ],"page":1,"pages":1}"#;
        let (base, handle) = mock_http(vec![(200, domains), (200, records), (200, "{}")]);
        let provider =
            LinodeProvider::new("tok-secret".to_string()).expect("provider").with_api_base(base);

        provider
            .delete_txt("_acme-challenge.preview.ephpm.dev", "the-txt-value")
            .await
            .expect("delete_txt");

        let reqs = tokio::task::spawn_blocking(move || handle.join().expect("join"))
            .await
            .expect("captured requests");
        assert_eq!(reqs.len(), 3, "expected domain lookup, records list, then delete");

        assert!(reqs[0].starts_with("GET /domains"), "domain lookup line: {}", reqs[0]);
        assert!(reqs[1].starts_with("GET /domains/4321/records"), "records list line: {}", reqs[1]);
        // Delete addresses record id 22 (target "the-txt-value"), never 11.
        assert!(reqs[2].starts_with("DELETE /domains/4321/records/22"), "delete line: {}", reqs[2]);
        assert!(
            reqs[2].contains("authorization: Bearer tok-secret"),
            "missing bearer: {}",
            reqs[2]
        );
    }

    #[tokio::test]
    async fn api_error_is_surfaced() {
        let body = r#"{"errors":[{"reason":"Invalid Token"}]}"#;
        let (base, handle) = mock_http(vec![(401, body)]);
        let provider =
            LinodeProvider::new("bad".to_string()).expect("provider").with_api_base(base);

        let err = provider
            .set_txt("_acme-challenge.preview.ephpm.dev", "v")
            .await
            .expect_err("must surface the API error");
        let msg = format!("{err:#}");
        assert!(msg.contains("Invalid Token"), "unexpected: {msg}");

        let _ = tokio::task::spawn_blocking(move || handle.join().ok()).await;
    }
}
