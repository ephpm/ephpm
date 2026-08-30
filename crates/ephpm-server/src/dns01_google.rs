//! DNS-01 provider for **Google Cloud DNS**.
//!
//! A second [`DnsProvider`](crate::dns01::DnsProvider) implementation alongside
//! [`CloudflareProvider`](crate::dns01::CloudflareProvider). The trait, the
//! order/renewal state machine, and the hot-swap TLS resolver all live in
//! [`crate::dns01`]; this file is *only* the provider — publish and retract the
//! `_acme-challenge` TXT records — plus the OAuth2 plumbing Google requires.
//!
//! ## Why this one is more than a REST wrapper
//!
//! Cloudflare authenticates with a single bearer token. Google Cloud DNS
//! authenticates with a **service-account key**: a JSON key file whose
//! `private_key` (a PKCS#8 RSA key) signs a short-lived JWT assertion, which is
//! exchanged at Google's token endpoint for an OAuth2 bearer access token
//! (scope `…/auth/ndev.clouddns.readwrite`). We mint that token here rather than
//! pull in a full OAuth2 crate — the whole flow is one RS256 signature plus one
//! form POST, and the RSA signing is done with `aws-lc-rs`, the crypto provider
//! already linked for rustls/TLS (see [`crate::tls::crypto_provider`]). That
//! keeps `ring` out of the tree, the same single-provider discipline #241
//! enforces everywhere else.
//!
//! ## The append subtlety (the reason [`DnsProvider::set_txt`] exists)
//!
//! Cloud DNS models a name as a *ResourceRecordSet*: one `(name, type)` with a
//! list of `rrdatas`. There is no "add one value" call — every mutation is a
//! [`Changes: create`] that atomically deletes an old RRSet and adds a new one.
//! A wildcard order plus its bare apex produce **two** challenge values at the
//! *same* `_acme-challenge.<domain>` name, and both must be live at once, so
//! `set_txt` is a read-modify-write: read the current RRSet, then submit a
//! change deleting it and adding an RRSet whose `rrdatas` is the old list plus
//! the new (double-quoted) value. `delete_txt` is the mirror — read, drop the
//! one matching value, and either add back the remainder or, if it was the last
//! value, add nothing.
//!
//! [`Changes: create`]: https://cloud.google.com/dns/docs/reference/v1/changes/create
//!
//! ## Live-validation status
//!
//! As with the Cloudflare lane, the request shaping (token exchange, rrset list,
//! change create) is exercised against a captured local HTTP server; a real
//! end-to-end issuance needs a Cloud DNS zone plus a live service account, which
//! is pending.

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use base64ct::{Base64, Base64UrlUnpadded, Encoding};

use crate::dns01::DnsProvider;
use crate::tls::crypto_provider;

/// Google Cloud DNS API base URL. Overridable in tests via
/// [`GoogleProvider::with_api_base`].
const GOOGLE_DNS_API_BASE: &str = "https://dns.googleapis.com/dns/v1";

/// Google's OAuth2 token endpoint. Overridable in tests via
/// [`GoogleProvider::with_token_url`].
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// OAuth2 scope for read/write access to Cloud DNS resource records — the
/// minimum a challenge publisher needs.
const GOOGLE_DNS_SCOPE: &str = "https://www.googleapis.com/auth/ndev.clouddns.readwrite";

/// Lifetime of the signed JWT assertion, in seconds. Google caps this at one
/// hour; the assertion is used once, immediately, so a full hour is generous.
const JWT_LIFETIME_SECS: u64 = 3600;

/// TXT record TTL requested when the RRSet is created fresh. Short, because the
/// record is deleted right after validation.
const CHALLENGE_TXT_TTL_SECS: u32 = 60;

// ── Service account credential ───────────────────────────────────────────────

/// A parsed service-account credential: the issuer identity plus the RSA key
/// that signs JWT assertions. The key material never leaves this struct and is
/// never logged.
struct ServiceAccount {
    /// The `client_email` — becomes the JWT `iss` (and `sub`).
    client_email: String,
    /// The RSA signing key parsed from the JSON key's `private_key` PEM.
    key_pair: RsaKeyPair,
}

/// The subset of the Google service-account JSON key we consume.
#[derive(serde::Deserialize)]
struct ServiceAccountKey {
    client_email: String,
    private_key: String,
}

impl ServiceAccount {
    /// Parse a service-account JSON key: pull out `client_email`, decode the
    /// `private_key` PEM to PKCS#8 DER, and load it as an RSA signing key.
    fn from_json(json: &str) -> anyhow::Result<Self> {
        let key: ServiceAccountKey =
            serde_json::from_str(json).context("parsing the service-account JSON key")?;
        anyhow::ensure!(
            !key.client_email.trim().is_empty(),
            "service-account JSON key has no client_email"
        );
        let der = pem_to_pkcs8_der(&key.private_key)
            .context("decoding the service-account private_key PEM")?;
        // The error from a rejected key describes the rejection, not the bytes,
        // so it is safe to surface.
        let key_pair = RsaKeyPair::from_pkcs8(&der).map_err(|e| {
            anyhow!("service-account private key is not a usable PKCS#8 RSA key: {e}")
        })?;
        Ok(Self { client_email: key.client_email, key_pair })
    }

    /// RS256-sign `message`, returning the raw signature bytes.
    fn sign(&self, message: &[u8]) -> anyhow::Result<Vec<u8>> {
        let mut signature = vec![0u8; self.key_pair.public_modulus_len()];
        let rng = SystemRandom::new();
        self.key_pair
            .sign(&RSA_PKCS1_SHA256, &rng, message, &mut signature)
            .map_err(|_| anyhow!("signing the OAuth2 JWT assertion failed"))?;
        Ok(signature)
    }
}

/// Decode a `-----BEGIN PRIVATE KEY-----` PEM body to PKCS#8 DER.
///
/// Deliberately hand-rolled and whitespace-tolerant: the `private_key` field in
/// a service-account JSON key carries embedded `\n`s that become real newlines
/// once the JSON string is parsed, and a strict base64 decoder rejects those.
fn pem_to_pkcs8_der(pem: &str) -> anyhow::Result<Vec<u8>> {
    const BEGIN: &str = "-----BEGIN PRIVATE KEY-----";
    const END: &str = "-----END PRIVATE KEY-----";
    let start = pem.find(BEGIN).context("private_key PEM missing a BEGIN PRIVATE KEY header")?;
    let body_start = start + BEGIN.len();
    let end =
        pem[body_start..].find(END).context("private_key PEM missing an END PRIVATE KEY footer")?;
    let body: String =
        pem[body_start..body_start + end].chars().filter(|c| !c.is_whitespace()).collect();
    Base64::decode_vec(&body)
        .map_err(|e| anyhow!("service-account private_key is not valid base64: {e}"))
}

// ── Google Cloud DNS provider ────────────────────────────────────────────────

/// A [`DnsProvider`] backed by the Google Cloud DNS v1 API.
///
/// Authenticates with a service-account key (see the module docs). The key and
/// every minted access token are kept out of logs and out of [`Debug`].
pub struct GoogleProvider {
    client: reqwest::Client,
    account: ServiceAccount,
    /// The GCP project that owns the managed zone.
    project: String,
    /// Explicit managed-zone name, or `None` to resolve it from the record FQDN.
    managed_zone: Option<String>,
    api_base: String,
    token_url: String,
}

impl std::fmt::Debug for GoogleProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render the signing key or the client_email.
        f.debug_struct("GoogleProvider")
            .field("project", &self.project)
            .field("managed_zone", &self.managed_zone)
            .field("api_base", &self.api_base)
            .field("token_url", &self.token_url)
            .finish_non_exhaustive()
    }
}

impl GoogleProvider {
    /// Build a Google Cloud DNS provider.
    ///
    /// `service_account_json` is the **contents** of a service-account JSON key
    /// (not a path). `project` is the owning GCP project id. `managed_zone` is
    /// the Cloud DNS managed-zone name; when `None` it is resolved by listing
    /// the project's zones and matching the registrable parent of the record.
    ///
    /// # Errors
    ///
    /// Returns an error if the JSON key cannot be parsed, its private key is not
    /// a usable PKCS#8 RSA key, or the HTTPS client cannot be constructed.
    pub fn new(
        service_account_json: String,
        project: String,
        managed_zone: Option<String>,
    ) -> anyhow::Result<Self> {
        let account = ServiceAccount::from_json(&service_account_json)?;
        let client = build_google_http_client()?;
        Ok(Self {
            client,
            account,
            project,
            managed_zone,
            api_base: GOOGLE_DNS_API_BASE.to_string(),
            token_url: GOOGLE_TOKEN_URL.to_string(),
        })
    }

    /// Override the API base URL. Test-only seam for a captured HTTP server.
    #[must_use]
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }

    /// Override the OAuth2 token URL. Test-only seam for a captured HTTP server.
    #[must_use]
    pub fn with_token_url(mut self, url: impl Into<String>) -> Self {
        self.token_url = url.into();
        self
    }

    /// Build and sign the JWT assertion Google exchanges for an access token.
    fn signed_assertion(&self) -> anyhow::Result<String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_secs();
        // A fixed header — RS256, JWT.
        let header = br#"{"alg":"RS256","typ":"JWT"}"#;
        let claims = serde_json::json!({
            "iss": self.account.client_email,
            "sub": self.account.client_email,
            "scope": GOOGLE_DNS_SCOPE,
            "aud": self.token_url,
            "iat": now,
            "exp": now + JWT_LIFETIME_SECS,
        });
        let claims = serde_json::to_vec(&claims).expect("serializing JWT claims cannot fail");
        let signing_input = format!(
            "{}.{}",
            Base64UrlUnpadded::encode_string(header),
            Base64UrlUnpadded::encode_string(&claims)
        );
        let signature = self.account.sign(signing_input.as_bytes())?;
        Ok(format!("{signing_input}.{}", Base64UrlUnpadded::encode_string(&signature)))
    }

    /// Exchange a freshly signed assertion for an OAuth2 access token.
    async fn access_token(&self) -> anyhow::Result<String> {
        let assertion = self.signed_assertion()?;
        let resp = self
            .client
            .post(&self.token_url)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", assertion.as_str()),
            ])
            .send()
            .await
            .context("Google OAuth2 token request failed")?;
        let status = resp.status();
        let bytes = resp.bytes().await.context("reading the Google token response")?;
        if !status.is_success() {
            // The token endpoint's error body is `{error, error_description}` —
            // no secret material — but keep it terse.
            bail!("Google OAuth2 token exchange failed (HTTP {})", status.as_u16());
        }
        let parsed: TokenResponse =
            serde_json::from_slice(&bytes).context("decoding the Google token response")?;
        Ok(parsed.access_token)
    }

    /// Resolve the managed-zone name for `fqdn`, using the configured name when
    /// present, else listing the project's zones and matching the longest
    /// `dnsName` that is a parent of the record.
    async fn managed_zone_for(&self, token: &str, fqdn: &str) -> anyhow::Result<String> {
        if let Some(zone) = &self.managed_zone {
            return Ok(zone.clone());
        }
        let url = format!("{}/projects/{}/managedZones", self.api_base, self.project);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .context("Google Cloud DNS list managedZones request failed")?;
        let status = resp.status();
        let bytes = resp.bytes().await.context("reading the managedZones response")?;
        ensure_google_ok(status, &bytes, "list managedZones")?;
        let parsed: ManagedZonesList =
            serde_json::from_slice(&bytes).context("decoding the managedZones response")?;
        select_zone(parsed.managed_zones, &absolute_name(fqdn)).ok_or_else(|| {
            anyhow!(
                "no Google Cloud DNS managed zone in project {} is a parent of {fqdn} (set the \
                 managed zone explicitly)",
                self.project
            )
        })
    }

    /// Fetch the existing TXT RRSet at `name` (absolute, trailing dot), if any.
    async fn get_txt_rrset(
        &self,
        token: &str,
        zone: &str,
        name: &str,
    ) -> anyhow::Result<Option<RrSet>> {
        let url = format!(
            "{}/projects/{}/managedZones/{zone}/rrsets?name={name}&type=TXT",
            self.api_base, self.project
        );
        let resp = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .context("Google Cloud DNS list rrsets request failed")?;
        let status = resp.status();
        let bytes = resp.bytes().await.context("reading the rrsets response")?;
        ensure_google_ok(status, &bytes, "list rrsets")?;
        let parsed: RrSetsList =
            serde_json::from_slice(&bytes).context("decoding the rrsets response")?;
        Ok(parsed.rrsets.into_iter().find(|r| r.name == name && r.record_type == "TXT"))
    }

    /// Submit a `Changes: create` atomically deleting `deletions` and adding
    /// `additions`.
    async fn submit_change(
        &self,
        token: &str,
        zone: &str,
        additions: Vec<RrSet>,
        deletions: Vec<RrSet>,
    ) -> anyhow::Result<()> {
        let url =
            format!("{}/projects/{}/managedZones/{zone}/changes", self.api_base, self.project);
        let body = serde_json::json!({ "additions": additions, "deletions": deletions });
        let resp = self
            .client
            .post(&url)
            .bearer_auth(token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&body).expect("serializing a change request cannot fail"))
            .send()
            .await
            .context("Google Cloud DNS changes create request failed")?;
        let status = resp.status();
        let bytes = resp.bytes().await.context("reading the changes response")?;
        ensure_google_ok(status, &bytes, "create change")
    }
}

#[async_trait]
impl DnsProvider for GoogleProvider {
    async fn set_txt(&self, fqdn: &str, value: &str) -> anyhow::Result<()> {
        let token = self.access_token().await?;
        let zone = self.managed_zone_for(&token, fqdn).await?;
        let name = absolute_name(fqdn);
        let quoted = quote_txt(value);

        // Read-modify-write: keep every value already at this name and add ours.
        let existing = self.get_txt_rrset(&token, &zone, &name).await?;
        let (mut rrdatas, ttl, deletions) = match existing {
            Some(rr) => {
                let ttl = rr.ttl;
                (rr.rrdatas.clone(), ttl, vec![rr])
            }
            None => (Vec::new(), CHALLENGE_TXT_TTL_SECS, Vec::new()),
        };
        if rrdatas.iter().any(|d| d == &quoted) {
            // Already published (e.g. a retried order) — nothing to change.
            return Ok(());
        }
        rrdatas.push(quoted);
        let additions =
            vec![RrSet { name: name.clone(), record_type: "TXT".to_string(), ttl, rrdatas }];
        self.submit_change(&token, &zone, additions, deletions).await?;
        tracing::debug!(fqdn, "published DNS-01 challenge TXT record via Google Cloud DNS");
        Ok(())
    }

    async fn delete_txt(&self, fqdn: &str, value: &str) -> anyhow::Result<()> {
        let token = self.access_token().await?;
        let zone = self.managed_zone_for(&token, fqdn).await?;
        let name = absolute_name(fqdn);
        let quoted = quote_txt(value);

        let Some(existing) = self.get_txt_rrset(&token, &zone, &name).await? else {
            return Ok(()); // nothing at this name — already retracted
        };
        let remaining: Vec<String> =
            existing.rrdatas.iter().filter(|d| *d != &quoted).cloned().collect();
        if remaining.len() == existing.rrdatas.len() {
            return Ok(()); // our value was not present — nothing to retract
        }
        let ttl = existing.ttl;
        let deletions = vec![existing];
        // Add back the survivors; if ours was the last value, add nothing so the
        // RRSet is removed entirely.
        let additions = if remaining.is_empty() {
            Vec::new()
        } else {
            vec![RrSet {
                name: name.clone(),
                record_type: "TXT".to_string(),
                ttl,
                rrdatas: remaining,
            }]
        };
        self.submit_change(&token, &zone, additions, deletions).await?;
        tracing::debug!(fqdn, "retracted DNS-01 challenge TXT record via Google Cloud DNS");
        Ok(())
    }
}

// ── Pure helpers ─────────────────────────────────────────────────────────────

/// Normalise a record name to the absolute form Cloud DNS uses (trailing dot).
fn absolute_name(fqdn: &str) -> String {
    if fqdn.ends_with('.') { fqdn.to_string() } else { format!("{fqdn}.") }
}

/// Wrap a TXT value in the double quotes Cloud DNS `rrdatas` require.
fn quote_txt(value: &str) -> String {
    format!("\"{value}\"")
}

/// Pick the managed zone whose `dnsName` is the longest parent suffix of
/// `absolute_fqdn` (which must carry the trailing dot). Longest wins so a
/// delegated sub-zone is preferred over its parent.
fn select_zone(zones: Vec<ManagedZone>, absolute_fqdn: &str) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for zone in zones {
        if absolute_fqdn.ends_with(zone.dns_name.as_str()) {
            let len = zone.dns_name.len();
            if best.as_ref().is_none_or(|(best_len, _)| len > *best_len) {
                best = Some((len, zone.name));
            }
        }
    }
    best.map(|(_, name)| name)
}

/// Turn a non-2xx Cloud DNS response into an error carrying the API's own
/// message when it parses.
fn ensure_google_ok(status: reqwest::StatusCode, body: &[u8], action: &str) -> anyhow::Result<()> {
    if status.is_success() {
        return Ok(());
    }
    let detail = serde_json::from_slice::<GoogleErrorEnvelope>(body)
        .ok()
        .map_or_else(|| format!("HTTP {}", status.as_u16()), |e| e.error.message);
    bail!("Google Cloud DNS {action} failed: {detail}")
}

/// Build the reqwest client for Google with an explicit rustls config — the
/// same shape [`crate::dns01`] builds for Cloudflare, so both share the one
/// aws-lc-rs crypto provider (reqwest itself is compiled with no provider and no
/// root store; see [`crate::tls::crypto_provider`]).
fn build_google_http_client() -> anyhow::Result<reqwest::Client> {
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
        .context("failed to build the Google Cloud DNS API HTTP client")
}

// ── Google API JSON shapes ───────────────────────────────────────────────────

/// The one field we read from an OAuth2 token response.
#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
}

/// A Cloud DNS ResourceRecordSet. Serialised into change `additions`/`deletions`
/// and deserialised from the `rrsets` list.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct RrSet {
    name: String,
    #[serde(rename = "type")]
    record_type: String,
    #[serde(default)]
    ttl: u32,
    #[serde(default)]
    rrdatas: Vec<String>,
}

/// The `rrsets` list response.
#[derive(serde::Deserialize)]
struct RrSetsList {
    #[serde(default)]
    rrsets: Vec<RrSet>,
}

/// A Cloud DNS managed zone (only the fields zone-resolution needs).
#[derive(serde::Deserialize)]
struct ManagedZone {
    name: String,
    #[serde(rename = "dnsName")]
    dns_name: String,
}

/// The `managedZones` list response.
#[derive(serde::Deserialize)]
struct ManagedZonesList {
    #[serde(default, rename = "managedZones")]
    managed_zones: Vec<ManagedZone>,
}

/// Google's `{ "error": { "code", "message" } }` error envelope.
#[derive(serde::Deserialize)]
struct GoogleErrorEnvelope {
    error: GoogleErrorBody,
}

#[derive(serde::Deserialize)]
struct GoogleErrorBody {
    message: String,
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener as StdTcpListener;

    use super::*;

    /// Generate a throwaway service-account JSON key with a real RSA key, so JWT
    /// signing in the provider actually succeeds. We only assert request shape,
    /// never Google-side validation.
    fn test_service_account_json() -> String {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_RSA_SHA256).expect("rsa keypair");
        let pem = key.serialize_pem();
        format!(
            r#"{{"type":"service_account","client_email":"svc@proj.iam.gserviceaccount.com","private_key":{}}}"#,
            serde_json::to_string(&pem).expect("json-encode pem")
        )
    }

    /// A local HTTP server that accepts `bodies.len()` sequential connections,
    /// captures each request, and replies with the matching canned body. Every
    /// reply sets `Connection: close`, so reqwest opens a fresh connection per
    /// request and the server sees them one per `accept`.
    fn serve(bodies: Vec<&'static str>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for body in bodies {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).expect("read");
                requests.push(String::from_utf8_lossy(&buf[..n]).to_string());
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).expect("write");
                stream.flush().ok();
            }
            requests
        });
        (format!("http://{addr}"), handle)
    }

    const TOKEN_BODY: &str =
        r#"{"access_token":"ya29.test-access-token","token_type":"Bearer","expires_in":3600}"#;
    const CHANGE_OK: &str = r#"{"kind":"dns#change","status":"pending"}"#;

    fn provider(sa_json: String, api_base: &str, token_url: &str) -> GoogleProvider {
        GoogleProvider::new(sa_json, "my-project".to_string(), Some("my-zone".to_string()))
            .expect("provider")
            .with_api_base(api_base)
            .with_token_url(token_url)
    }

    #[test]
    fn absolute_name_appends_a_trailing_dot() {
        assert_eq!(absolute_name("_acme-challenge.ephpm.dev"), "_acme-challenge.ephpm.dev.");
        assert_eq!(absolute_name("_acme-challenge.ephpm.dev."), "_acme-challenge.ephpm.dev.");
    }

    #[test]
    fn quote_txt_wraps_in_double_quotes() {
        assert_eq!(quote_txt("abc"), "\"abc\"");
    }

    #[test]
    fn select_zone_prefers_the_longest_parent() {
        let zones = vec![
            ManagedZone { name: "root".into(), dns_name: "ephpm.dev.".into() },
            ManagedZone { name: "sub".into(), dns_name: "preview.ephpm.dev.".into() },
            ManagedZone { name: "other".into(), dns_name: "example.com.".into() },
        ];
        assert_eq!(
            select_zone(zones, "_acme-challenge.preview.ephpm.dev.").as_deref(),
            Some("sub")
        );
    }

    #[test]
    fn select_zone_none_when_no_parent_matches() {
        let zones = vec![ManagedZone { name: "other".into(), dns_name: "example.com.".into() }];
        assert!(select_zone(zones, "_acme-challenge.ephpm.dev.").is_none());
    }

    #[test]
    fn new_rejects_a_key_without_a_private_key() {
        let err = GoogleProvider::new(
            r#"{"client_email":"svc@proj.iam.gserviceaccount.com","private_key":"not-a-pem"}"#
                .to_string(),
            "p".to_string(),
            None,
        )
        .expect_err("must reject a bogus private key");
        let msg = format!("{err:#}");
        assert!(msg.contains("private_key"), "unexpected: {msg}");
    }

    #[tokio::test]
    async fn token_exchange_posts_a_jwt_bearer_assertion() {
        let sa = test_service_account_json();
        let (token_base, token_handle) = serve(vec![TOKEN_BODY]);
        // api_base is unused on this path; point it at a dead address.
        let provider = provider(sa, "http://127.0.0.1:1", &token_base);

        let token = provider.access_token().await.expect("access token");
        assert_eq!(token, "ya29.test-access-token");

        let requests =
            tokio::task::spawn_blocking(move || token_handle.join().expect("join")).await.unwrap();
        let req = &requests[0];
        assert!(req.starts_with("POST / HTTP/1.1"), "request line: {req}");
        assert!(
            req.contains("content-type: application/x-www-form-urlencoded"),
            "missing form content-type: {req}"
        );
        assert!(req.contains("grant_type="), "missing grant_type: {req}");
        assert!(req.contains("jwt-bearer"), "missing jwt-bearer grant: {req}");
        assert!(req.contains("assertion="), "missing assertion: {req}");
    }

    #[tokio::test]
    async fn set_txt_creates_a_change_when_the_name_is_empty() {
        let sa = test_service_account_json();
        let (token_base, token_handle) = serve(vec![TOKEN_BODY]);
        // rrsets list (empty) then the change create.
        let (api_base, api_handle) = serve(vec![r#"{"rrsets":[]}"#, CHANGE_OK]);
        let provider = provider(sa, &api_base, &token_base);

        provider
            .set_txt("_acme-challenge.preview.ephpm.dev", "the-txt-value")
            .await
            .expect("set_txt");

        let _ =
            tokio::task::spawn_blocking(move || token_handle.join().expect("join")).await.unwrap();
        let api =
            tokio::task::spawn_blocking(move || api_handle.join().expect("join")).await.unwrap();

        let list = &api[0];
        assert!(
            list.starts_with("GET /projects/my-project/managedZones/my-zone/rrsets?"),
            "list line: {list}"
        );
        assert!(list.contains("name=_acme-challenge.preview.ephpm.dev."), "list name: {list}");
        assert!(list.contains("type=TXT"), "list type: {list}");
        assert!(list.contains("authorization: Bearer ya29.test-access-token"), "list auth: {list}");

        let change = &api[1];
        assert!(
            change.starts_with("POST /projects/my-project/managedZones/my-zone/changes"),
            "change line: {change}"
        );
        assert!(change.contains("content-type: application/json"), "change ctype: {change}");
        assert!(
            change.contains("authorization: Bearer ya29.test-access-token"),
            "change auth: {change}"
        );
        assert!(change.contains("\"deletions\":[]"), "empty deletions expected: {change}");
        assert!(
            change.contains("\"name\":\"_acme-challenge.preview.ephpm.dev.\""),
            "change name: {change}"
        );
        assert!(change.contains("\"type\":\"TXT\""), "change type: {change}");
        // The rrdata is the value wrapped in literal double quotes.
        assert!(change.contains(r#"\"the-txt-value\""#), "change rrdata: {change}");
    }

    #[tokio::test]
    async fn set_txt_appends_without_clobbering_an_existing_value() {
        let sa = test_service_account_json();
        let (token_base, token_handle) = serve(vec![TOKEN_BODY]);
        // An existing RRSet already holds the apex challenge value; the wildcard
        // value must be ADDED, not replace it.
        let list = r#"{"rrsets":[{"name":"_acme-challenge.preview.ephpm.dev.","type":"TXT","ttl":60,"rrdatas":["\"apex-value\""]}]}"#;
        let (api_base, api_handle) = serve(vec![list, CHANGE_OK]);
        let provider = provider(sa, &api_base, &token_base);

        provider
            .set_txt("_acme-challenge.preview.ephpm.dev", "wildcard-value")
            .await
            .expect("set_txt");

        let _ =
            tokio::task::spawn_blocking(move || token_handle.join().expect("join")).await.unwrap();
        let api =
            tokio::task::spawn_blocking(move || api_handle.join().expect("join")).await.unwrap();

        let change = &api[1];
        // The old RRSet is deleted and re-added carrying BOTH values.
        assert!(change.contains("\"deletions\":["), "must delete the old rrset: {change}");
        assert!(change.contains(r#"\"apex-value\""#), "must keep the apex value: {change}");
        assert!(change.contains(r#"\"wildcard-value\""#), "must add the wildcard value: {change}");
    }

    #[tokio::test]
    async fn delete_txt_removes_only_the_matching_value() {
        let sa = test_service_account_json();
        let (token_base, token_handle) = serve(vec![TOKEN_BODY]);
        let list = r#"{"rrsets":[{"name":"_acme-challenge.preview.ephpm.dev.","type":"TXT","ttl":60,"rrdatas":["\"apex-value\"","\"wildcard-value\""]}]}"#;
        let (api_base, api_handle) = serve(vec![list, CHANGE_OK]);
        let provider = provider(sa, &api_base, &token_base);

        provider
            .delete_txt("_acme-challenge.preview.ephpm.dev", "wildcard-value")
            .await
            .expect("delete_txt");

        let _ =
            tokio::task::spawn_blocking(move || token_handle.join().expect("join")).await.unwrap();
        let api =
            tokio::task::spawn_blocking(move || api_handle.join().expect("join")).await.unwrap();

        let change = &api[1];
        assert!(change.contains("\"deletions\":["), "must delete the old rrset: {change}");
        // The survivor is added back; the deleted value is not in the additions.
        assert!(change.contains(r#"\"apex-value\""#), "must keep the apex value: {change}");
        assert!(
            change.matches(r#"\"wildcard-value\""#).count() == 1,
            "the deleted value must appear only in the deletions rrset, not the additions: {change}"
        );
    }

    #[tokio::test]
    async fn delete_txt_removing_the_last_value_adds_nothing() {
        let sa = test_service_account_json();
        let (token_base, token_handle) = serve(vec![TOKEN_BODY]);
        let list = r#"{"rrsets":[{"name":"_acme-challenge.ephpm.dev.","type":"TXT","ttl":60,"rrdatas":["\"only-value\""]}]}"#;
        let (api_base, api_handle) = serve(vec![list, CHANGE_OK]);
        let provider = provider(sa, &api_base, &token_base);

        provider.delete_txt("_acme-challenge.ephpm.dev", "only-value").await.expect("delete_txt");

        let _ =
            tokio::task::spawn_blocking(move || token_handle.join().expect("join")).await.unwrap();
        let api =
            tokio::task::spawn_blocking(move || api_handle.join().expect("join")).await.unwrap();

        let change = &api[1];
        assert!(change.contains("\"additions\":[]"), "additions must be empty: {change}");
        assert!(change.contains("\"deletions\":["), "must delete the old rrset: {change}");
    }

    #[tokio::test]
    async fn google_api_error_is_surfaced() {
        let sa = test_service_account_json();
        let (token_base, token_handle) = serve(vec![TOKEN_BODY]);
        // A non-2xx would normally carry a non-200 status; here we assert the
        // error-envelope decoding by returning an error body on a 200 is not
        // enough, so drive it through a real 4xx via a raw response.
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let api_handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf).expect("read");
            let body = r#"{"error":{"code":403,"message":"Permission denied on resource"}}"#;
            let response = format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).expect("write");
            stream.flush().ok();
        });
        let api_base = format!("http://{addr}");
        let provider = provider(sa, &api_base, &token_base);

        let err = provider
            .set_txt("_acme-challenge.ephpm.dev", "v")
            .await
            .expect_err("must surface the API error");
        let msg = format!("{err:#}");
        assert!(msg.contains("Permission denied"), "unexpected: {msg}");

        let _ = tokio::task::spawn_blocking(move || token_handle.join().ok()).await;
        let _ = tokio::task::spawn_blocking(move || api_handle.join().ok()).await;
    }
}
