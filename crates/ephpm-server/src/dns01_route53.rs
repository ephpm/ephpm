//! AWS Route 53 [`DnsProvider`] for the DNS-01 wildcard ACME lane.
//!
//! This is the Route 53 sibling of [`crate::dns01::CloudflareProvider`]. It
//! implements the same two-operation [`DnsProvider`] seam, but Route 53 is a
//! harder target on two counts, both handled here:
//!
//! 1. **Authentication is AWS Signature Version 4.** Every request is signed
//!    with the account's access key over a canonical request + a derived
//!    per-day/region/service signing key. Rather than pull in the AWS SDK (or
//!    even the standalone `aws-sigv4` crate, which needs `aws-credential-types`
//!    to build an `Identity` and whose signing instructions do not map cleanly
//!    onto a `reqwest::RequestBuilder`), the SigV4 flow is implemented directly
//!    on top of the RustCrypto `hmac`/`sha2` crates already in the tree. It is
//!    ~80 lines and pulls in no `ring` (the workspace's single-crypto-provider
//!    rule, issue #241).
//!
//! 2. **A TXT record name is a single ResourceRecordSet holding a *list* of
//!    values**, not one record per value like Cloudflare. So "add a value
//!    without clobbering" — the invariant the [`DnsProvider`] trait requires so
//!    a wildcard order and its bare apex can keep two live challenge values at
//!    the same `_acme-challenge.<domain>` name — is a read-modify-write:
//!    `ListResourceRecordSets` to read the current value list, append the new
//!    (double-quoted) value, then `ChangeResourceRecordSets` `UPSERT` the whole
//!    list. Deleting one value rewrites the set without it (`UPSERT`), or
//!    `DELETE`s the whole ResourceRecordSet when the last value goes away.
//!
//! Route 53 is a global service; requests are signed for region `us-east-1`,
//! service `route53`. Requests and responses are XML.
//!
//! ## Live-validation status
//!
//! As with the Cloudflare provider, request shaping is covered by captured-HTTP
//! tests and the SigV4 key derivation by a documented AWS test vector; a real
//! end-to-end issuance against a live hosted zone is pending.

use std::fmt::Write as _;
use std::time::SystemTime;

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;

use crate::dns01::DnsProvider;
use crate::sigv4::{Credentials, SignInput, sign};
use crate::tls::crypto_provider;

/// Route 53 API base URL. Overridable in tests via
/// [`Route53Provider::with_api_base`].
const ROUTE53_API_BASE: &str = "https://route53.amazonaws.com";

/// The Route 53 API version prefix every path carries.
const API_VERSION: &str = "2013-04-01";

/// SigV4 region. Route 53 is global and is always signed for `us-east-1`.
const REGION: &str = "us-east-1";

/// SigV4 service name.
const SERVICE: &str = "route53";

/// TTL requested for the challenge TXT record. Short — it is deleted right
/// after validation. Mirrors the Cloudflare provider's constant.
const CHALLENGE_TXT_TTL_SECS: u32 = 60;

// ── Route 53 provider ────────────────────────────────────────────────────────

/// A [`DnsProvider`] backed by the AWS Route 53 API, authenticated with SigV4.
///
/// Authenticates with an IAM access key that can `ListResourceRecordSets` and
/// `ChangeResourceRecordSets` on the target hosted zone (plus
/// `ListHostedZonesByName` when the zone id is resolved rather than
/// configured). Neither the secret access key nor the access key id is ever
/// logged.
pub struct Route53Provider {
    client: reqwest::Client,
    access_key_id: String,
    secret_access_key: String,
    /// Explicit hosted zone id (`Z...` or `/hostedzone/Z...`), or `None` to
    /// resolve it from the record FQDN via `ListHostedZonesByName`.
    hosted_zone_id: Option<String>,
    api_base: String,
}

impl std::fmt::Debug for Route53Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render either credential.
        f.debug_struct("Route53Provider")
            .field("hosted_zone_id", &self.hosted_zone_id)
            .field("api_base", &self.api_base)
            .finish_non_exhaustive()
    }
}

impl Route53Provider {
    /// Build a Route 53 provider from an access key pair and optional hosted
    /// zone id.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTPS client cannot be constructed.
    pub fn new(
        access_key_id: String,
        secret_access_key: String,
        hosted_zone_id: Option<String>,
    ) -> anyhow::Result<Self> {
        let client = build_route53_http_client()?;
        Ok(Self {
            client,
            access_key_id,
            secret_access_key,
            hosted_zone_id,
            api_base: ROUTE53_API_BASE.to_string(),
        })
    }

    /// Override the API base URL. Test-only seam for a captured HTTP server.
    #[must_use]
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }

    /// The `Host` header value implied by `api_base` — i.e. the authority the
    /// SigV4 signature must bind to. `route53.amazonaws.com` for the real
    /// endpoint, `127.0.0.1:<port>` for the test server.
    fn host(&self) -> String {
        let after =
            self.api_base.split_once("://").map_or(self.api_base.as_str(), |(_, rest)| rest);
        after.split('/').next().unwrap_or(after).to_string()
    }

    /// Resolve the hosted zone id for `fqdn`, using the configured id if present.
    async fn zone_id_for(&self, fqdn: &str) -> anyhow::Result<String> {
        if let Some(id) = &self.hosted_zone_id {
            return Ok(id.strip_prefix("/hostedzone/").unwrap_or(id).to_string());
        }
        // No dnsname filter: list from the start and pick the longest zone name
        // that is a DNS suffix of the FQDN. Robust for the common (small) zone
        // count; accounts with >100 zones should set an explicit id (paging is
        // not implemented).
        let path = format!("/{API_VERSION}/hostedzonesbyname");
        let (status, body) = self.send(reqwest::Method::GET, &path, &[], None).await?;
        let text = String::from_utf8_lossy(&body);
        if !status.is_success() {
            bail!("Route 53 ListHostedZonesByName failed [{status}]: {}", error_detail(&text));
        }
        find_zone_id(&text, fqdn).ok_or_else(|| {
            anyhow!(
                "could not resolve a Route 53 hosted zone for {fqdn}: no parent domain is a hosted \
                 zone on this account (set [server.tls] route53_hosted_zone_id to skip resolution)"
            )
        })
    }

    /// Read the current TXT ResourceRecordSet for `fqdn`, returning
    /// `(ttl, values)`. `values` are the stored, still-double-quoted strings; an
    /// absent record yields the default TTL and an empty list.
    async fn list_txt_rrset(&self, zone: &str, fqdn: &str) -> anyhow::Result<(u32, Vec<String>)> {
        let path = format!("/{API_VERSION}/hostedzone/{zone}/rrset");
        let (status, body) = self
            .send(
                reqwest::Method::GET,
                &path,
                &[("name", fqdn), ("type", "TXT"), ("maxitems", "10")],
                None,
            )
            .await?;
        let text = String::from_utf8_lossy(&body);
        if !status.is_success() {
            bail!("Route 53 ListResourceRecordSets failed [{status}]: {}", error_detail(&text));
        }
        match find_txt_rrset(&text, fqdn) {
            Some(block) => {
                let ttl = inner_text(block, "TTL")
                    .and_then(|t| t.parse::<u32>().ok())
                    .unwrap_or(CHALLENGE_TXT_TTL_SECS);
                Ok((ttl, rrset_values(block)))
            }
            None => Ok((CHALLENGE_TXT_TTL_SECS, Vec::new())),
        }
    }

    /// Submit a `ChangeResourceRecordSets` batch with a single change.
    async fn change_rrset(
        &self,
        zone: &str,
        action: &str,
        fqdn: &str,
        ttl: u32,
        values: &[String],
    ) -> anyhow::Result<()> {
        let name = format!("{}.", fqdn.trim_end_matches('.'));
        let body = change_batch_xml(action, &name, ttl, values).into_bytes();
        let path = format!("/{API_VERSION}/hostedzone/{zone}/rrset");
        let (status, resp) = self.send(reqwest::Method::POST, &path, &[], Some(body)).await?;
        if !status.is_success() {
            let text = String::from_utf8_lossy(&resp);
            bail!(
                "Route 53 ChangeResourceRecordSets ({action}) failed [{status}]: {}",
                error_detail(&text)
            );
        }
        Ok(())
    }

    /// Sign and send a single Route 53 request, returning the status + body.
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> anyhow::Result<(reqwest::StatusCode, bytes::Bytes)> {
        let payload = body.as_deref().unwrap_or(b"");
        let unix_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let (amz_date, authorization, canonical_query) =
            self.sign(method.as_str(), &self.host(), path, query, payload, unix_secs);

        let mut url = format!("{}{path}", self.api_base);
        if !canonical_query.is_empty() {
            url.push('?');
            url.push_str(&canonical_query);
        }

        let mut req = self
            .client
            .request(method, &url)
            .header("x-amz-date", amz_date)
            .header(reqwest::header::AUTHORIZATION, authorization);
        if let Some(bytes) = body {
            req = req.header(reqwest::header::CONTENT_TYPE, "application/xml").body(bytes);
        }
        let resp = req.send().await.context("Route 53 request failed")?;
        let status = resp.status();
        let bytes = resp.bytes().await.context("reading Route 53 response body")?;
        Ok((status, bytes))
    }

    /// Produce the `(x-amz-date, Authorization, canonical query)` triple for
    /// a request via AWS Signature Version 4 ([`crate::sigv4`]). The signed
    /// header set is `host;x-amz-date`; the payload hash is sent in the
    /// canonical request only (Route 53 does not use
    /// `x-amz-content-sha256`).
    fn sign(
        &self,
        method: &str,
        host: &str,
        path: &str,
        query: &[(&str, &str)],
        payload: &[u8],
        unix_secs: u64,
    ) -> (String, String, String) {
        let creds = Credentials {
            access_key_id: self.access_key_id.clone(),
            secret_access_key: self.secret_access_key.clone(),
            session_token: None,
        };
        let payload_hash = crate::sigv4::sha256_hex(payload);
        let signed = sign(
            &creds,
            &SignInput {
                method,
                host,
                path,
                query,
                extra_headers: &[],
                payload_hash: &payload_hash,
                send_content_sha256: false,
                region: REGION,
                service: SERVICE,
                unix_secs,
            },
        );
        (signed.amz_date, signed.authorization, signed.canonical_query)
    }
}

#[async_trait]
impl DnsProvider for Route53Provider {
    async fn set_txt(&self, fqdn: &str, value: &str) -> anyhow::Result<()> {
        let zone = self.zone_id_for(fqdn).await?;
        let (_ttl, mut values) = self.list_txt_rrset(&zone, fqdn).await?;
        let quoted = quote_txt(value);
        if !values.iter().any(|v| v == &quoted) {
            values.push(quoted);
        }
        // UPSERT the whole value list — this both creates a fresh record and
        // appends to an existing one without clobbering the sibling value.
        self.change_rrset(&zone, "UPSERT", fqdn, CHALLENGE_TXT_TTL_SECS, &values).await?;
        tracing::debug!(fqdn, "published DNS-01 challenge TXT record via Route 53");
        Ok(())
    }

    async fn delete_txt(&self, fqdn: &str, value: &str) -> anyhow::Result<()> {
        let zone = self.zone_id_for(fqdn).await?;
        let (ttl, values) = self.list_txt_rrset(&zone, fqdn).await?;
        let quoted = quote_txt(value);
        if !values.iter().any(|v| v == &quoted) {
            // Nothing to retract — already gone. Best-effort cleanup, so this is
            // success, not an error.
            return Ok(());
        }
        let remaining: Vec<String> = values.into_iter().filter(|v| v != &quoted).collect();
        if remaining.is_empty() {
            // Last value: DELETE the whole ResourceRecordSet. Route 53 requires
            // the DELETE change to match the existing record exactly, so it
            // carries the value being removed and the record's own TTL.
            self.change_rrset(&zone, "DELETE", fqdn, ttl, &[quoted]).await?;
        } else {
            // Other values remain: rewrite the set without ours.
            self.change_rrset(&zone, "UPSERT", fqdn, CHALLENGE_TXT_TTL_SECS, &remaining).await?;
        }
        tracing::debug!(fqdn, "retracted DNS-01 challenge TXT record via Route 53");
        Ok(())
    }
}

/// Build the reqwest client for Route 53 with an explicit rustls config, for the
/// same reason the Cloudflare client does: reqwest is pinned to
/// `rustls-tls-manual-roots-no-provider`, so it never drags in `ring` (issue
/// #241) and must be handed a fully-built [`rustls::ClientConfig`] using the
/// shared aws-lc-rs provider plus the bundled webpki roots.
fn build_route53_http_client() -> anyhow::Result<reqwest::Client> {
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
        .context("failed to build the Route 53 API HTTP client")
}

// ── XML helpers ──────────────────────────────────────────────────────────────

/// Wrap a raw TXT value in the double quotes Route 53 requires.
fn quote_txt(value: &str) -> String {
    format!("\"{value}\"")
}

/// Escape XML text content (`&`, `<`, `>`). Quotes are literal in element
/// content and are left alone.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Serialize a single-change `ChangeResourceRecordSets` request body.
fn change_batch_xml(action: &str, name: &str, ttl: u32, values: &[String]) -> String {
    let mut records = String::new();
    for v in values {
        let _ =
            write!(records, "<ResourceRecord><Value>{}</Value></ResourceRecord>", xml_escape(v));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<ChangeResourceRecordSetsRequest xmlns=\"https://route53.amazonaws.com/doc/2013-04-01/\">\
<ChangeBatch><Changes><Change>\
<Action>{action}</Action>\
<ResourceRecordSet>\
<Name>{name}</Name><Type>TXT</Type><TTL>{ttl}</TTL>\
<ResourceRecords>{records}</ResourceRecords>\
</ResourceRecordSet>\
</Change></Changes></ChangeBatch>\
</ChangeResourceRecordSetsRequest>"
    )
}

/// Normalize a DNS name for comparison: strip a trailing dot, lowercase.
fn normalize_name(name: &str) -> String {
    name.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Whether `zone` is a DNS suffix of `fqdn` (equal, or on a label boundary).
fn is_dns_suffix(fqdn: &str, zone: &str) -> bool {
    fqdn == zone || fqdn.ends_with(&format!(".{zone}"))
}

/// Return the inner text of the first `<tag>...</tag>` in `xml`, trimmed.
fn inner_text<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim())
}

/// Find the TXT `ResourceRecordSet` block whose `Name` matches `fqdn`.
///
/// `ListResourceRecordSets` returns records at or after the requested name, so
/// the first block is not guaranteed to be ours — the name is matched exactly.
fn find_txt_rrset<'a>(xml: &'a str, fqdn: &str) -> Option<&'a str> {
    let target = normalize_name(fqdn);
    let mut rest = xml;
    while let Some(open) = rest.find("<ResourceRecordSet>") {
        let after = &rest[open + "<ResourceRecordSet>".len()..];
        let end = after.find("</ResourceRecordSet>")?;
        let block = &after[..end];
        let is_txt = inner_text(block, "Type") == Some("TXT");
        let name_matches =
            inner_text(block, "Name").map(normalize_name).as_deref() == Some(&target);
        if is_txt && name_matches {
            return Some(block);
        }
        rest = &after[end..];
    }
    None
}

/// Collect every `<Value>...</Value>` inside a ResourceRecordSet block. Values
/// are returned as stored (still double-quoted).
fn rrset_values(block: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = block;
    while let Some(open) = rest.find("<Value>") {
        let after = &rest[open + "<Value>".len()..];
        let Some(end) = after.find("</Value>") else { break };
        out.push(after[..end].trim().to_string());
        rest = &after[end..];
    }
    out
}

/// Pick the hosted zone id whose name is the longest DNS suffix of `fqdn`.
fn find_zone_id(xml: &str, fqdn: &str) -> Option<String> {
    let target = normalize_name(fqdn);
    let mut best: Option<(usize, String)> = None;
    let mut rest = xml;
    while let Some(open) = rest.find("<HostedZone>") {
        let after = &rest[open + "<HostedZone>".len()..];
        let Some(end) = after.find("</HostedZone>") else { break };
        let block = &after[..end];
        if let (Some(id), Some(name)) = (inner_text(block, "Id"), inner_text(block, "Name")) {
            let zname = normalize_name(name);
            if is_dns_suffix(&target, &zname)
                && best.as_ref().is_none_or(|(len, _)| zname.len() > *len)
            {
                let id = id.strip_prefix("/hostedzone/").unwrap_or(id).to_string();
                best = Some((zname.len(), id));
            }
        }
        rest = &after[end..];
    }
    best.map(|(_, id)| id)
}

/// Format the `Code`/`Message` from a Route 53 `<ErrorResponse>` body for an
/// error message. Falls back to the raw body when it is not the expected shape.
fn error_detail(body: &str) -> String {
    match (inner_text(body, "Code"), inner_text(body, "Message")) {
        (Some(code), Some(message)) => format!("[{code}] {message}"),
        (Some(code), None) => format!("[{code}]"),
        _ => body.trim().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener as StdTcpListener;
    use std::thread::JoinHandle;

    use super::*;

    #[test]
    fn find_txt_rrset_matches_exact_name_only() {
        let xml = "\
<ListResourceRecordSetsResponse>\
<ResourceRecordSets>\
<ResourceRecordSet><Name>other.example.com.</Name><Type>TXT</Type><TTL>60</TTL>\
<ResourceRecords><ResourceRecord><Value>\"nope\"</Value></ResourceRecord></ResourceRecords>\
</ResourceRecordSet>\
<ResourceRecordSet><Name>_acme-challenge.example.com.</Name><Type>TXT</Type><TTL>30</TTL>\
<ResourceRecords><ResourceRecord><Value>\"a\"</Value></ResourceRecord>\
<ResourceRecord><Value>\"b\"</Value></ResourceRecord></ResourceRecords>\
</ResourceRecordSet>\
</ResourceRecordSets></ListResourceRecordSetsResponse>";
        let block = find_txt_rrset(xml, "_acme-challenge.example.com").expect("match");
        assert_eq!(inner_text(block, "TTL"), Some("30"));
        assert_eq!(rrset_values(block), vec!["\"a\"".to_string(), "\"b\"".to_string()]);
        assert!(find_txt_rrset(xml, "_acme-challenge.absent.com").is_none());
    }

    #[test]
    fn find_zone_id_picks_longest_suffix() {
        let xml = "\
<ListHostedZonesByNameResponse><HostedZones>\
<HostedZone><Id>/hostedzone/ZPARENT</Id><Name>example.com.</Name></HostedZone>\
<HostedZone><Id>/hostedzone/ZCHILD</Id><Name>preview.example.com.</Name></HostedZone>\
</HostedZones></ListHostedZonesByNameResponse>";
        assert_eq!(
            find_zone_id(xml, "_acme-challenge.preview.example.com").as_deref(),
            Some("ZCHILD")
        );
        assert_eq!(find_zone_id(xml, "_acme-challenge.example.com").as_deref(), Some("ZPARENT"));
        assert!(find_zone_id(xml, "_acme-challenge.other.net").is_none());
    }

    // ── Captured-HTTP request-shaping tests ──────────────────────────────────

    /// A multi-request one-shot HTTP server. Accepts `responses.len()`
    /// connections, replies to each with the next canned `(status, body)` in
    /// order, and returns the captured raw requests. Each reqwest call uses a
    /// fresh connection (`Connection: close`), so N calls == N accepts.
    fn canned_http(responses: Vec<(u16, &'static str)>) -> (String, JoinHandle<Vec<String>>) {
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let mut captured = Vec::new();
            for (code, body) in responses {
                let (mut stream, _) = listener.accept().expect("accept");
                captured.push(read_http_request(&mut stream));
                let reason = if code == 200 { "OK" } else { "Error" };
                let response = format!(
                    "HTTP/1.1 {code} {reason}\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).expect("write");
                stream.flush().ok();
            }
            captured
        });
        (format!("http://{addr}"), handle)
    }

    /// Read one full HTTP request (headers + `Content-Length` body) into a
    /// string, so a POST body split across reads is still captured whole.
    fn read_http_request(stream: &mut std::net::TcpStream) -> String {
        let mut data = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = stream.read(&mut tmp).expect("read");
            if n == 0 {
                break;
            }
            data.extend_from_slice(&tmp[..n]);
            if let Some(headers_end) = find_subslice(&data, b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&data[..headers_end]);
                let content_length = headers
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("content-length:")
                            .or_else(|| l.strip_prefix("Content-Length:"))
                    })
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if data.len() >= headers_end + 4 + content_length {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&data).to_string()
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    fn provider(base: String) -> Route53Provider {
        Route53Provider::new(
            "AKIAEXAMPLE".to_string(),
            "secretEXAMPLE".to_string(),
            Some("Z123".to_string()),
        )
        .expect("provider")
        .with_api_base(base)
    }

    /// `set_txt` reads the existing value list and UPSERTs it with the new
    /// value appended — the existing sibling value must survive.
    #[tokio::test]
    async fn set_txt_appends_without_clobbering() {
        let list = "\
<ListResourceRecordSetsResponse><ResourceRecordSets>\
<ResourceRecordSet><Name>_acme-challenge.preview.ephpm.dev.</Name><Type>TXT</Type><TTL>60</TTL>\
<ResourceRecords><ResourceRecord><Value>\"existing\"</Value></ResourceRecord></ResourceRecords>\
</ResourceRecordSet></ResourceRecordSets></ListResourceRecordSetsResponse>";
        let change = "<ChangeResourceRecordSetsResponse><ChangeInfo><Status>PENDING</Status></ChangeInfo></ChangeResourceRecordSetsResponse>";
        let (base, handle) = canned_http(vec![(200, list), (200, change)]);
        let provider = provider(base);

        provider.set_txt("_acme-challenge.preview.ephpm.dev", "new-value").await.expect("set_txt");

        let reqs = tokio::task::spawn_blocking(move || handle.join().expect("join"))
            .await
            .expect("captured");
        assert_eq!(reqs.len(), 2, "expected a list then a change request");
        // Request 1: the GET list, correctly signed.
        assert!(
            reqs[0].starts_with("GET /2013-04-01/hostedzone/Z123/rrset?"),
            "list line: {}",
            reqs[0]
        );
        assert!(reqs[0].contains("authorization: AWS4-HMAC-SHA256 Credential=AKIAEXAMPLE/"));
        assert!(reqs[0].to_lowercase().contains("x-amz-date:"));
        // Request 2: the ChangeResourceRecordSets UPSERT with BOTH values.
        let change_req = &reqs[1];
        assert!(
            change_req.starts_with("POST /2013-04-01/hostedzone/Z123/rrset"),
            "change line: {change_req}"
        );
        assert!(change_req.contains("<Action>UPSERT</Action>"), "body: {change_req}");
        assert!(change_req.contains("<Type>TXT</Type>"), "body: {change_req}");
        assert!(
            change_req.contains("<Name>_acme-challenge.preview.ephpm.dev.</Name>"),
            "body: {change_req}"
        );
        assert!(change_req.contains("<Value>\"existing\"</Value>"), "kept: {change_req}");
        assert!(change_req.contains("<Value>\"new-value\"</Value>"), "added: {change_req}");
    }

    /// `set_txt` on a name with no existing record creates it (UPSERT, one
    /// value, default TTL).
    #[tokio::test]
    async fn set_txt_creates_when_absent() {
        let empty = "<ListResourceRecordSetsResponse><ResourceRecordSets></ResourceRecordSets></ListResourceRecordSetsResponse>";
        let change = "<ChangeResourceRecordSetsResponse><ChangeInfo><Status>PENDING</Status></ChangeInfo></ChangeResourceRecordSetsResponse>";
        let (base, handle) = canned_http(vec![(200, empty), (200, change)]);
        let provider = provider(base);

        provider.set_txt("_acme-challenge.x.example", "only").await.expect("set_txt");

        let reqs = tokio::task::spawn_blocking(move || handle.join().expect("join"))
            .await
            .expect("captured");
        let change_req = &reqs[1];
        assert!(change_req.contains("<Action>UPSERT</Action>"), "body: {change_req}");
        assert!(change_req.contains("<TTL>60</TTL>"), "default ttl: {change_req}");
        assert!(change_req.contains("<Value>\"only\"</Value>"), "body: {change_req}");
    }

    /// `delete_txt` on a multi-value set rewrites it (UPSERT) without the
    /// removed value, keeping the sibling.
    #[tokio::test]
    async fn delete_txt_rewrites_without_value() {
        let list = "\
<ListResourceRecordSetsResponse><ResourceRecordSets>\
<ResourceRecordSet><Name>_acme-challenge.preview.ephpm.dev.</Name><Type>TXT</Type><TTL>60</TTL>\
<ResourceRecords>\
<ResourceRecord><Value>\"keep\"</Value></ResourceRecord>\
<ResourceRecord><Value>\"drop\"</Value></ResourceRecord>\
</ResourceRecords></ResourceRecordSet></ResourceRecordSets></ListResourceRecordSetsResponse>";
        let change = "<ChangeResourceRecordSetsResponse><ChangeInfo><Status>PENDING</Status></ChangeInfo></ChangeResourceRecordSetsResponse>";
        let (base, handle) = canned_http(vec![(200, list), (200, change)]);
        let provider = provider(base);

        provider.delete_txt("_acme-challenge.preview.ephpm.dev", "drop").await.expect("delete_txt");

        let reqs = tokio::task::spawn_blocking(move || handle.join().expect("join"))
            .await
            .expect("captured");
        let change_req = &reqs[1];
        assert!(change_req.contains("<Action>UPSERT</Action>"), "body: {change_req}");
        assert!(change_req.contains("<Value>\"keep\"</Value>"), "kept: {change_req}");
        assert!(!change_req.contains("\"drop\""), "must not carry the dropped value: {change_req}");
    }

    /// `delete_txt` removing the last value DELETEs the whole ResourceRecordSet,
    /// carrying the value being removed and the record's own TTL.
    #[tokio::test]
    async fn delete_txt_removes_empty_rrset() {
        let list = "\
<ListResourceRecordSetsResponse><ResourceRecordSets>\
<ResourceRecordSet><Name>_acme-challenge.x.example.</Name><Type>TXT</Type><TTL>45</TTL>\
<ResourceRecords><ResourceRecord><Value>\"solo\"</Value></ResourceRecord></ResourceRecords>\
</ResourceRecordSet></ResourceRecordSets></ListResourceRecordSetsResponse>";
        let change = "<ChangeResourceRecordSetsResponse><ChangeInfo><Status>PENDING</Status></ChangeInfo></ChangeResourceRecordSetsResponse>";
        let (base, handle) = canned_http(vec![(200, list), (200, change)]);
        let provider = provider(base);

        provider.delete_txt("_acme-challenge.x.example", "solo").await.expect("delete_txt");

        let reqs = tokio::task::spawn_blocking(move || handle.join().expect("join"))
            .await
            .expect("captured");
        let change_req = &reqs[1];
        assert!(change_req.contains("<Action>DELETE</Action>"), "body: {change_req}");
        assert!(change_req.contains("<TTL>45</TTL>"), "existing ttl: {change_req}");
        assert!(change_req.contains("<Value>\"solo\"</Value>"), "body: {change_req}");
    }

    /// A Route 53 API error is surfaced with its `Code`/`Message`.
    #[tokio::test]
    async fn api_error_is_surfaced() {
        let err = "<ErrorResponse><Error><Code>AccessDenied</Code><Message>not authorized</Message></Error></ErrorResponse>";
        let (base, handle) = canned_http(vec![(403, err)]);
        let provider = provider(base);

        let outcome = provider.set_txt("_acme-challenge.x.example", "v").await;
        let e = outcome.expect_err("must surface the API error");
        let msg = format!("{e:#}");
        assert!(msg.contains("AccessDenied"), "unexpected: {msg}");
        assert!(msg.contains("not authorized"), "unexpected: {msg}");

        let _ = tokio::task::spawn_blocking(move || handle.join().ok()).await;
    }
}
