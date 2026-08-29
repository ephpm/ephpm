//! DNS-01 wildcard ACME provisioning.
//!
//! This is the **opt-in** ACME lane selected by `[server.tls] challenge =
//! "dns-01"`. It exists alongside — never replacing — the default TLS-ALPN-01
//! path in [`crate::acme`]. An operator picks exactly one; the default keeps
//! the previous zero-config behaviour.
//!
//! ## Why DNS-01 at all
//!
//! TLS-ALPN-01 answers the challenge inline on the TLS socket, which is simple
//! and needs no credentials — but a certificate authority cannot validate a
//! **wildcard** identifier (`*.preview.ephpm.dev`) that way: there is no single
//! hostname to connect to. Wildcards therefore *require* DNS-01, where control
//! is proven by publishing a `_acme-challenge.<domain>` TXT record. One
//! wildcard certificate also consolidates every ephemeral preview subdomain
//! into a single order, sidestepping Let's Encrypt's "50 certificates per
//! registered domain per week" rate limit that per-subdomain issuance would
//! otherwise burn through.
//!
//! ## Architecture
//!
//! `rustls-acme` (the TLS-ALPN-01 engine) speaks only TLS-ALPN-01, so this lane
//! is built on the lower-level [`instant_acme`] client, which drives the ACME
//! protocol but owns neither TLS serving nor renewal. Those two jobs are ours:
//!
//! - **[`DnsProvider`]** is the `libdns`-equivalent seam. It has exactly two
//!   operations — publish and retract a TXT record — so a new provider is a
//!   ~150-line wrapper. [`CloudflareProvider`] is the first and only impl.
//! - **[`Dns01CertResolver`]** is a hot-swappable [`rustls::server::ResolvesServerCert`].
//!   The renewal task swaps a freshly issued certificate in with no restart and
//!   no connection drop — which also closes the follower-can't-see-a-renewal
//!   gap that the `rustls-acme` lane documents, because *this* resolver is ours
//!   to mutate.
//! - The **renewal + clustering machinery is reused from [`crate::acme`]**:
//!   [`crate::acme::try_acquire_acme_leadership`] for leader election over the
//!   same `acme:leader` KV key, and [`crate::acme::store_acme_cert`] /
//!   [`crate::acme::get_acme_cert_cluster`] for cluster-wide certificate
//!   distribution. Only the elected leader talks to Let's Encrypt; every other
//!   node installs the leader's certificate out of the KV store. That reuse is
//!   deliberate — duplicating a second, subtly different leader election is
//!   exactly how a cluster ends up ordering five duplicate certificates and
//!   getting locked out for a week.
//!
//!   "Every other node installs the leader's certificate" is a claim about the
//!   KV tier the certificate is written to, and it was **false** until the
//!   write moved to `Store::set_broadcast`. A certificate chain is bigger than
//!   `[cluster.kv] small_key_threshold`, so a plain `set` sharded it across
//!   `replication_factor` nodes; on a three-node cluster the other two served
//!   no certificate at all. Distribution now has two halves, and both are load
//!   bearing: the leader **broadcasts** to every node alive at issuance, and a
//!   follower's poll below reads through
//!   [`crate::acme::get_acme_cert_cluster`], which fetches from a peer when
//!   this node has no local copy — the case for a node that joined after
//!   issuance.
//!
//! ## Live-validation status
//!
//! The order flow is exercised against a mocked [`DnsProvider`] and the request
//! shaping against a captured HTTP server; a real end-to-end issuance needs a
//! zone on Cloudflare plus a live token, which is pending. See the PR body for
//! the exact live-validation steps.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use ephpm_config::TlsConfig;
use ephpm_kv::store::Store;
use instant_acme::{
    Account, AuthorizationStatus, ChallengeType, Identifier, LetsEncrypt, NewAccount, NewOrder,
    OrderStatus, RetryPolicy,
};
use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use sha2::{Digest, Sha256};
use tokio_rustls::TlsAcceptor;

use crate::tls::crypto_provider;

/// Renew when the certificate is this old. Let's Encrypt issues 90-day
/// certificates; renewing at 60 days leaves a 30-day safety margin, matching
/// the 2/3-of-validity policy `rustls-acme` hardcodes for the other lane.
const RENEW_AFTER: Duration = Duration::from_secs(60 * 24 * 60 * 60);

/// How long to wait after publishing the TXT records before telling the CA to
/// validate. Cloudflare's authoritative edge is usually consistent within a
/// couple of seconds; 15s is a conservative floor before we lean on the CA's
/// own validation retries.
const PROPAGATION_WAIT: Duration = Duration::from_secs(15);

/// TXT record TTL requested from the provider. Short, because the record is
/// deleted right after validation.
const CHALLENGE_TXT_TTL_SECS: u32 = 60;

/// Cloudflare API base URL. Overridable in tests via
/// [`CloudflareProvider::with_api_base`].
const CLOUDFLARE_API_BASE: &str = "https://api.cloudflare.com/client/v4";

// ── DnsProvider seam ─────────────────────────────────────────────────────────

/// A DNS provider capable of publishing and retracting the `_acme-challenge`
/// TXT records a DNS-01 challenge requires.
///
/// This is intentionally tiny — the entire ACME/order state machine lives in
/// [`instant_acme`] and this crate, so a provider is nothing more than "put a
/// TXT record" and "take it away again". `set_txt` must **add** a record rather
/// than replace: a wildcard order plus its bare apex produce two challenges at
/// the *same* `_acme-challenge.<domain>` name with different values, and both
/// must be live simultaneously. `delete_txt` therefore takes the value so it
/// can retract precisely the record it published.
#[async_trait]
pub trait DnsProvider: Send + Sync {
    /// Publish a `TXT` record at `fqdn` with the given `value`.
    ///
    /// Adds a record; it must not clobber an existing TXT record at the same
    /// name (see the trait docs).
    ///
    /// # Errors
    ///
    /// Returns an error if the record could not be created.
    async fn set_txt(&self, fqdn: &str, value: &str) -> anyhow::Result<()>;

    /// Retract the `TXT` record at `fqdn` whose content is exactly `value`.
    ///
    /// Best-effort cleanup: a failure here leaves a stale record but does not
    /// invalidate the issued certificate.
    ///
    /// # Errors
    ///
    /// Returns an error if the lookup or deletion failed.
    async fn delete_txt(&self, fqdn: &str, value: &str) -> anyhow::Result<()>;
}

// ── Cloudflare provider ──────────────────────────────────────────────────────

/// A [`DnsProvider`] backed by the Cloudflare v4 API.
///
/// Authenticates with a **zone-scoped** API token (`Zone.DNS:Edit`, plus
/// `Zone:Read` only when the zone id is resolved rather than configured). The
/// token is never logged.
pub struct CloudflareProvider {
    client: reqwest::Client,
    token: String,
    /// Explicit zone id, or `None` to resolve it from the record FQDN.
    zone_id: Option<String>,
    api_base: String,
}

impl std::fmt::Debug for CloudflareProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render the token.
        f.debug_struct("CloudflareProvider")
            .field("zone_id", &self.zone_id)
            .field("api_base", &self.api_base)
            .finish_non_exhaustive()
    }
}

impl CloudflareProvider {
    /// Build a Cloudflare provider from an API token and optional zone id.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTPS client cannot be constructed.
    pub fn new(token: String, zone_id: Option<String>) -> anyhow::Result<Self> {
        let client = build_cloudflare_http_client()?;
        Ok(Self { client, token, zone_id, api_base: CLOUDFLARE_API_BASE.to_string() })
    }

    /// Override the API base URL. Test-only seam for a captured HTTP server.
    #[must_use]
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }

    /// Resolve the zone id for `fqdn`, using the configured id if present.
    async fn zone_id_for(&self, fqdn: &str) -> anyhow::Result<String> {
        if let Some(id) = &self.zone_id {
            return Ok(id.clone());
        }
        for candidate in zone_candidates(fqdn) {
            let url = format!("{}/zones?name={candidate}", self.api_base);
            let resp = self
                .client
                .get(&url)
                .bearer_auth(&self.token)
                .send()
                .await
                .with_context(|| format!("Cloudflare GET zones?name={candidate} failed"))?;
            let body = resp.bytes().await.context("reading Cloudflare zones response")?;
            let parsed: CfList<Zone> =
                serde_json::from_slice(&body).context("decoding Cloudflare zones response")?;
            parsed.ensure_success("list zones")?;
            if let Some(zone) = parsed.result.into_iter().next() {
                return Ok(zone.id);
            }
        }
        bail!(
            "could not resolve a Cloudflare zone for {fqdn}: no parent domain is a zone on this \
             account (set [server.tls] cloudflare_zone_id to skip resolution)"
        )
    }
}

#[async_trait]
impl DnsProvider for CloudflareProvider {
    async fn set_txt(&self, fqdn: &str, value: &str) -> anyhow::Result<()> {
        let zone = self.zone_id_for(fqdn).await?;
        let url = format!("{}/zones/{zone}/dns_records", self.api_base);
        let body = serde_json::json!({
            "type": "TXT",
            "name": fqdn,
            "content": value,
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
            .context("Cloudflare create TXT record request failed")?;
        let bytes = resp.bytes().await.context("reading Cloudflare create-record response")?;
        let parsed: CfStatus =
            serde_json::from_slice(&bytes).context("decoding Cloudflare create-record response")?;
        parsed.ensure_success("create TXT record")?;
        tracing::debug!(fqdn, "published DNS-01 challenge TXT record via Cloudflare");
        Ok(())
    }

    async fn delete_txt(&self, fqdn: &str, value: &str) -> anyhow::Result<()> {
        let zone = self.zone_id_for(fqdn).await?;
        // Find the record whose content matches, then delete by id.
        let list_url = format!(
            "{}/zones/{zone}/dns_records?type=TXT&name={fqdn}&content={value}",
            self.api_base
        );
        let resp = self
            .client
            .get(&list_url)
            .bearer_auth(&self.token)
            .send()
            .await
            .context("Cloudflare list TXT records request failed")?;
        let bytes = resp.bytes().await.context("reading Cloudflare list-records response")?;
        let parsed: CfList<DnsRecord> =
            serde_json::from_slice(&bytes).context("decoding Cloudflare list-records response")?;
        parsed.ensure_success("list TXT records")?;
        for record in parsed.result {
            let del_url = format!("{}/zones/{zone}/dns_records/{}", self.api_base, record.id);
            let resp = self
                .client
                .delete(&del_url)
                .bearer_auth(&self.token)
                .send()
                .await
                .context("Cloudflare delete TXT record request failed")?;
            let bytes = resp.bytes().await.context("reading Cloudflare delete-record response")?;
            let parsed: CfStatus = serde_json::from_slice(&bytes)
                .context("decoding Cloudflare delete-record response")?;
            parsed.ensure_success("delete TXT record")?;
        }
        tracing::debug!(fqdn, "retracted DNS-01 challenge TXT record via Cloudflare");
        Ok(())
    }
}

/// Build the reqwest client for Cloudflare with an explicit rustls config.
///
/// The workspace pins reqwest to `rustls-tls-manual-roots-no-provider` (so it
/// never drags in `ring` — see [`crate::tls::crypto_provider`]), which means we
/// must hand it a fully-built [`rustls::ClientConfig`]: the shared aws-lc-rs
/// provider plus the bundled webpki roots.
fn build_cloudflare_http_client() -> anyhow::Result<reqwest::Client> {
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
        .context("failed to build the Cloudflare API HTTP client")
}

/// Generate the candidate zone apexes for `fqdn`, longest first.
///
/// For `_acme-challenge.preview.ephpm.dev` this yields
/// `["_acme-challenge.preview.ephpm.dev", "preview.ephpm.dev", "ephpm.dev"]`
/// (stopping at two labels, since a single-label TLD is never an account
/// zone). The caller queries each until one matches an actual zone, which
/// avoids bundling a public-suffix list.
fn zone_candidates(fqdn: &str) -> Vec<String> {
    let fqdn = fqdn.trim_end_matches('.');
    let labels: Vec<&str> = fqdn.split('.').collect();
    let mut out = Vec::new();
    // Keep suffixes with at least two labels.
    for start in 0..labels.len().saturating_sub(1) {
        out.push(labels[start..].join("."));
    }
    out
}

// ── Cloudflare API response envelopes ────────────────────────────────────────

#[derive(serde::Deserialize)]
struct CfList<T> {
    success: bool,
    #[serde(default)]
    errors: Vec<CfError>,
    #[serde(default = "Vec::new")]
    result: Vec<T>,
}

/// A Cloudflare response envelope where we only care about success/errors
/// (create and delete return a `result` we do not read).
#[derive(serde::Deserialize)]
struct CfStatus {
    success: bool,
    #[serde(default)]
    errors: Vec<CfError>,
}

#[derive(serde::Deserialize)]
struct CfError {
    code: i64,
    message: String,
}

#[derive(serde::Deserialize)]
struct Zone {
    id: String,
}

#[derive(serde::Deserialize)]
struct DnsRecord {
    id: String,
}

impl<T> CfList<T> {
    fn ensure_success(&self, action: &str) -> anyhow::Result<()> {
        ensure_cf_success(self.success, &self.errors, action)
    }
}

impl CfStatus {
    fn ensure_success(&self, action: &str) -> anyhow::Result<()> {
        ensure_cf_success(self.success, &self.errors, action)
    }
}

fn ensure_cf_success(success: bool, errors: &[CfError], action: &str) -> anyhow::Result<()> {
    if success {
        return Ok(());
    }
    let detail =
        errors.iter().map(|e| format!("[{}] {}", e.code, e.message)).collect::<Vec<_>>().join("; ");
    bail!("Cloudflare API {action} failed: {detail}")
}

// ── Hot-swappable rustls resolver ────────────────────────────────────────────

/// A [`ResolvesServerCert`] whose certificate can be replaced at runtime.
///
/// The renewal task installs a freshly issued certificate here; in-flight and
/// future handshakes pick it up on their next `resolve` call. Before the first
/// certificate is installed (or loaded from cache), `resolve` returns `None` —
/// a handshake attempted in that window fails, which is the honest signal that
/// no certificate exists yet rather than serving a wrong one.
#[derive(Debug)]
pub struct Dns01CertResolver {
    current: RwLock<Option<Arc<CertifiedKey>>>,
}

impl Dns01CertResolver {
    /// Construct an empty resolver (no certificate installed yet).
    #[must_use]
    pub fn new() -> Self {
        Self { current: RwLock::new(None) }
    }

    /// Install a certificate chain + private key (both PEM) as the resolver's
    /// current certificate.
    ///
    /// # Errors
    ///
    /// Returns an error if the PEM cannot be parsed or the key does not match
    /// what the crypto provider supports.
    pub fn install(&self, cert_pem: &[u8], key_pem: &[u8]) -> anyhow::Result<()> {
        let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem)
            .collect::<Result<_, _>>()
            .context("parsing DNS-01 certificate chain PEM")?;
        anyhow::ensure!(!certs.is_empty(), "DNS-01 certificate PEM contained no certificates");
        let key = PrivateKeyDer::from_pem_slice(key_pem)
            .context("parsing DNS-01 certificate private key PEM")?;
        let signing_key = crypto_provider()
            .key_provider
            .load_private_key(key)
            .context("crypto provider rejected the DNS-01 certificate key")?;
        let certified = CertifiedKey::new(certs, signing_key);
        *self.current.write().unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(Arc::new(certified));
        Ok(())
    }

    /// Whether a certificate is currently installed.
    #[must_use]
    pub fn has_cert(&self) -> bool {
        self.current.read().unwrap_or_else(std::sync::PoisonError::into_inner).is_some()
    }
}

impl Default for Dns01CertResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl ResolvesServerCert for Dns01CertResolver {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.current.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
}

// ── Setup + lifecycle ────────────────────────────────────────────────────────

/// The rustls acceptor for the DNS-01 lane, plus the resolver the renewal task
/// mutates.
pub struct Dns01Setup {
    /// TLS acceptor whose cert resolver is hot-swapped by the renewal task.
    pub acceptor: TlsAcceptor,
}

/// Everything the renewal loop needs, gathered once at startup.
struct Dns01Context {
    resolver: Arc<Dns01CertResolver>,
    provider: Arc<dyn DnsProvider>,
    domains: Vec<String>,
    canonical: String,
    contact: Vec<String>,
    directory_url: String,
    store: Option<Arc<Store>>,
    cache_dir: PathBuf,
    /// This node's ACME leader-election identity — the configured
    /// `[cluster] node_id` when set. See [`crate::acme::acme_node_id`] for why
    /// it must be stable across restarts.
    node_id: String,
}

/// Resolve the Cloudflare API token from the config, honouring the file → env
/// precedence.
///
/// The file (`cloudflare_api_token_file`) wins when present; otherwise the
/// inline / env-populated `cloudflare_api_token` is used. The raw token is
/// trimmed of trailing whitespace/newlines (a token file commonly ends in one).
///
/// # Errors
///
/// Returns an error if the file cannot be read or no token is available.
pub fn resolve_cloudflare_token(tls: &TlsConfig) -> anyhow::Result<String> {
    if let Some(path) = &tls.cloudflare_api_token_file {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading Cloudflare token file {}", path.display()))?;
        let token = raw.trim().to_string();
        anyhow::ensure!(!token.is_empty(), "Cloudflare token file {} is empty", path.display());
        return Ok(token);
    }
    if let Some(token) = tls.cloudflare_api_token.as_deref() {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }
    bail!(
        "no Cloudflare API token available: set [server.tls] cloudflare_api_token_file or the \
         EPHPM_SERVER__TLS__CLOUDFLARE_API_TOKEN environment variable"
    )
}

/// Start the DNS-01 ACME lane: build the hot-swap acceptor, seed it from any
/// cached certificate, and spawn the renewal/leadership task.
///
/// When `store` is `Some`, the lane runs clustered: leadership election and
/// certificate distribution reuse [`crate::acme`]'s KV machinery so only the
/// leader talks to Let's Encrypt.
///
/// # Errors
///
/// Returns an error if the credential/provider cannot be constructed or the
/// TLS acceptor cannot be built.
/// `cluster_node_id` is the configured `[cluster] node_id`; see
/// [`crate::acme::acme_node_id`] for why a stable identity is required for the
/// leader election to converge.
pub fn start_dns01_acme(
    tls_config: &TlsConfig,
    store: Option<Arc<Store>>,
    cluster_node_id: Option<&str>,
) -> anyhow::Result<Dns01Setup> {
    anyhow::ensure!(
        tls_config.dns_provider.as_deref().is_some_and(|p| p.eq_ignore_ascii_case("cloudflare")),
        "DNS-01 lane requires dns_provider = \"cloudflare\""
    );
    let token = resolve_cloudflare_token(tls_config)?;
    let provider: Arc<dyn DnsProvider> =
        Arc::new(CloudflareProvider::new(token, tls_config.cloudflare_zone_id.clone())?);

    let resolver = Arc::new(Dns01CertResolver::new());
    let cache_dir = tls_config.cache_dir.join("dns01");
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating DNS-01 cache directory {}", cache_dir.display()))?;

    let canonical = canonical_domain_key(&tls_config.domains);

    // Seed the resolver from cache so a restart serves TLS immediately instead
    // of waiting for a fresh order. KV first (cluster-wide truth), then disk.
    if let Some((cert, key)) = load_cached_cert(store.as_deref(), &cache_dir, &canonical) {
        match resolver.install(&cert, &key) {
            Ok(()) => {
                tracing::info!(canonical, "DNS-01: seeded resolver from cached certificate");
            }
            Err(e) => {
                tracing::warn!(error = %e, "DNS-01: cached certificate unusable, will reorder");
            }
        }
    }

    // Build the acceptor around the hot-swap resolver.
    let mut server_config = ServerConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()
        .context("crypto provider does not support the default TLS versions")?
        .with_no_client_auth()
        .with_cert_resolver(Arc::clone(&resolver) as Arc<dyn ResolvesServerCert>);
    server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    let contact =
        tls_config.email.as_ref().map(|e| vec![format!("mailto:{e}")]).unwrap_or_default();
    let directory_url = if tls_config.staging {
        LetsEncrypt::Staging.url().to_owned()
    } else {
        LetsEncrypt::Production.url().to_owned()
    };

    let ctx = Dns01Context {
        resolver: Arc::clone(&resolver),
        provider,
        domains: tls_config.domains.clone(),
        canonical,
        contact,
        directory_url,
        store,
        cache_dir,
        node_id: crate::acme::acme_node_id(cluster_node_id),
    };

    tracing::info!(
        domains = ?tls_config.domains,
        wildcard = tls_config.has_wildcard_domain(),
        clustered = ctx.store.is_some(),
        node_id = %ctx.node_id,
        stable_node_id = cluster_node_id.map(str::trim).is_some_and(|id| !id.is_empty()),
        environment = if tls_config.staging { "staging" } else { "production" },
        "DNS-01 ACME (Cloudflare) enabled"
    );

    tokio::spawn(run_dns01_lifecycle(ctx));

    Ok(Dns01Setup { acceptor })
}

/// The renewal + leadership loop. Runs for the lifetime of the server.
async fn run_dns01_lifecycle(ctx: Dns01Context) {
    let node_id = ctx.node_id.clone();
    let mut is_leader = false;
    let mut confirmations: u32 = 0;
    let mut last_issued: Option<SystemTime> =
        read_issued_at(ctx.store.as_deref(), &ctx.cache_dir, &ctx.canonical);
    let mut installed_fp: Option<[u8; 32]> = if ctx.resolver.has_cert() {
        load_cached_cert(ctx.store.as_deref(), &ctx.cache_dir, &ctx.canonical)
            .map(|(cert, _)| fingerprint(&cert))
    } else {
        None
    };

    let mut ticker = tokio::time::interval(crate::acme::ACME_LEADER_HEARTBEAT);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

        let standalone = ctx.store.is_none();
        if let Some(store) = &ctx.store {
            if crate::acme::try_acquire_acme_leadership(store, &node_id) {
                confirmations = confirmations.saturating_add(1);
            } else {
                confirmations = 0;
            }
            let now_leader = confirmations >= crate::acme::ACME_LEADER_CONFIRMATIONS;
            if now_leader != is_leader {
                is_leader = now_leader;
                if is_leader {
                    // On promotion, trust the KV issuance timestamp the previous
                    // leader wrote so we don't re-order a certificate that was
                    // just issued — a needless trip toward the rate limit.
                    last_issued =
                        read_issued_at(ctx.store.as_deref(), &ctx.cache_dir, &ctx.canonical);
                    tracing::info!(node_id, "DNS-01: this node is now the ACME leader");
                } else {
                    tracing::info!(node_id, "DNS-01: this node is no longer the ACME leader");
                }
            }
        }

        if standalone || is_leader {
            let due = match last_issued {
                None => true,
                Some(t) => t.elapsed().map_or(true, |age| age >= RENEW_AFTER),
            };
            if due {
                match obtain_certificate(&ctx).await {
                    Ok((cert_pem, key_pem)) => match ctx.resolver.install(&cert_pem, &key_pem) {
                        Ok(()) => {
                            let now = SystemTime::now();
                            last_issued = Some(now);
                            installed_fp = Some(fingerprint(&cert_pem));
                            persist_cert(&ctx, &cert_pem, &key_pem, now);
                            tracing::info!(
                                canonical = ctx.canonical,
                                "DNS-01: certificate issued and installed"
                            );
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "DNS-01: freshly issued certificate would not install");
                        }
                    },
                    Err(e) => {
                        tracing::error!(error = %format!("{e:#}"), "DNS-01: issuance failed, will retry");
                    }
                }
            }
        } else if let Some(store) = &ctx.store
            && let Some((cert_pem, key_pem)) =
                crate::acme::get_acme_cert_cluster(store, &ctx.canonical).await
        {
            // Follower: pick up the leader's certificate from the KV store.
            let fp = fingerprint(&cert_pem);
            if installed_fp.as_ref() != Some(&fp) {
                match ctx.resolver.install(&cert_pem, &key_pem) {
                    Ok(()) => {
                        installed_fp = Some(fp);
                        tracing::info!(
                            canonical = ctx.canonical,
                            "DNS-01: follower installed the leader's certificate from KV"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "DNS-01: follower could not install the leader's certificate");
                    }
                }
            }
        }
    }
}

/// Run one full DNS-01 order and return `(cert_chain_pem, private_key_pem)`.
async fn obtain_certificate(ctx: &Dns01Context) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let account = load_or_create_account(ctx).await?;

    let identifiers: Vec<Identifier> =
        ctx.domains.iter().map(|d| Identifier::Dns(d.clone())).collect();
    let mut order = account
        .new_order(&NewOrder::new(identifiers.as_slice()))
        .await
        .context("creating the ACME order")?;

    // Pass 1: publish every challenge's TXT record. Track what we published so
    // we can always retract it, even on the error paths.
    let mut published: Vec<(String, String)> = Vec::new();
    {
        let mut authorizations = order.authorizations();
        while let Some(result) = authorizations.next().await {
            let mut authz = result.context("fetching an ACME authorization")?;
            match authz.status {
                AuthorizationStatus::Pending => {}
                AuthorizationStatus::Valid => continue,
                other => bail!("unexpected ACME authorization status: {other:?}"),
            }
            let challenge = authz
                .challenge(ChallengeType::Dns01)
                .ok_or_else(|| anyhow!("ACME authorization has no dns-01 challenge"))?;
            let domain = challenge.identifier().to_string();
            let fqdn = challenge_fqdn(&domain);
            let value = challenge.key_authorization().dns_value();
            published.push((fqdn.clone(), value.clone()));
        }
    }

    // Publish outside the authorizations borrow so the provider calls don't
    // hold the order borrowed across await points we don't need to.
    let publish_result = publish_all(ctx.provider.as_ref(), &published).await;
    if let Err(e) = publish_result {
        retract_all(ctx.provider.as_ref(), &published).await;
        return Err(e.context("publishing DNS-01 challenge records"));
    }

    tokio::time::sleep(PROPAGATION_WAIT).await;

    // Pass 2: tell the CA each challenge is ready.
    let ready = set_all_ready(&mut order).await;
    if let Err(e) = ready {
        retract_all(ctx.provider.as_ref(), &published).await;
        return Err(e.context("signalling DNS-01 challenge readiness"));
    }

    // Poll to Ready, finalize, fetch the chain — then always retract records.
    let outcome = finalize_order(&mut order).await;
    retract_all(ctx.provider.as_ref(), &published).await;
    outcome
}

/// Publish every `(fqdn, value)` challenge record.
async fn publish_all(
    provider: &dyn DnsProvider,
    records: &[(String, String)],
) -> anyhow::Result<()> {
    for (fqdn, value) in records {
        provider.set_txt(fqdn, value).await.with_context(|| format!("publishing TXT {fqdn}"))?;
    }
    Ok(())
}

/// Best-effort retraction of every published record.
async fn retract_all(provider: &dyn DnsProvider, records: &[(String, String)]) {
    for (fqdn, value) in records {
        if let Err(e) = provider.delete_txt(fqdn, value).await {
            tracing::warn!(fqdn, error = %e, "DNS-01: failed to retract challenge TXT record");
        }
    }
}

/// Signal readiness for every pending dns-01 challenge on the order.
async fn set_all_ready(order: &mut instant_acme::Order) -> anyhow::Result<()> {
    let mut authorizations = order.authorizations();
    while let Some(result) = authorizations.next().await {
        let mut authz = result.context("re-fetching an ACME authorization")?;
        match authz.status {
            AuthorizationStatus::Pending => {}
            AuthorizationStatus::Valid => continue,
            other => bail!("unexpected ACME authorization status: {other:?}"),
        }
        let mut challenge = authz
            .challenge(ChallengeType::Dns01)
            .ok_or_else(|| anyhow!("ACME authorization has no dns-01 challenge"))?;
        challenge.set_ready().await.context("marking dns-01 challenge ready")?;
    }
    Ok(())
}

/// Poll the order to Ready, finalize, and return `(cert_pem, key_pem)`.
async fn finalize_order(order: &mut instant_acme::Order) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let status = order
        .poll_ready(&RetryPolicy::default())
        .await
        .context("polling the ACME order to ready")?;
    if status != OrderStatus::Ready {
        bail!("ACME order did not reach Ready (status: {status:?})");
    }
    let key_pem = order.finalize().await.context("finalizing the ACME order")?;
    let cert_pem = order
        .poll_certificate(&RetryPolicy::default())
        .await
        .context("retrieving the ACME certificate chain")?;
    Ok((cert_pem.into_bytes(), key_pem.into_bytes()))
}

/// Restore the ACME account from cached credentials, or create and cache one.
async fn load_or_create_account(ctx: &Dns01Context) -> anyhow::Result<Account> {
    if let Some(raw) = load_account_creds(ctx) {
        match serde_json::from_slice::<instant_acme::AccountCredentials>(&raw) {
            Ok(creds) => match Account::builder()?.from_credentials(creds).await {
                Ok(account) => return Ok(account),
                Err(e) => {
                    tracing::warn!(error = %e, "DNS-01: cached account credentials unusable, creating a new account");
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "DNS-01: cached account credentials corrupt, creating a new account");
            }
        }
    }
    let contact: Vec<&str> = ctx.contact.iter().map(String::as_str).collect();
    let (account, credentials) = Account::builder()?
        .create(
            &NewAccount {
                contact: &contact,
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            ctx.directory_url.clone(),
            None,
        )
        .await
        .context("creating the ACME account")?;
    if let Ok(bytes) = serde_json::to_vec(&credentials) {
        persist_account_creds(ctx, &bytes);
    }
    Ok(account)
}

// ── Cache helpers (KV + disk) ────────────────────────────────────────────────

/// KV key for the certificate issuance timestamp (unix seconds).
fn issued_kv_key(canonical: &str) -> String {
    format!("acme:cert:{canonical}:issued")
}

/// Build the `_acme-challenge.<domain>` FQDN, tolerating a wildcard prefix.
fn challenge_fqdn(domain: &str) -> String {
    let base = domain.strip_prefix("*.").unwrap_or(domain);
    format!("_acme-challenge.{base}")
}

/// A canonical, stable key for a domain set: sorted, comma-joined.
fn canonical_domain_key(domains: &[String]) -> String {
    let mut sorted: Vec<String> = domains.to_vec();
    sorted.sort();
    sorted.join(",")
}

/// SHA-256 of a byte slice, for cheap change detection.
fn fingerprint(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Sanitise a canonical domain key into a filename-safe stem.
fn cache_stem(canonical: &str) -> String {
    canonical
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' })
        .collect()
}

/// Load a cached cert `(cert_pem, key_pem)` — KV first, then disk.
fn load_cached_cert(
    store: Option<&Store>,
    cache_dir: &Path,
    canonical: &str,
) -> Option<(Vec<u8>, Vec<u8>)> {
    if let Some(store) = store
        && let Some((cert, key)) = crate::acme::get_acme_cert(store, canonical)
    {
        return Some((cert, key));
    }
    let stem = cache_stem(canonical);
    let cert = std::fs::read(cache_dir.join(format!("{stem}.crt"))).ok()?;
    let key = std::fs::read(cache_dir.join(format!("{stem}.key"))).ok()?;
    if cert.is_empty() || key.is_empty() {
        return None;
    }
    Some((cert, key))
}

/// Read the issuance timestamp — KV first, then the on-disk sidecar.
fn read_issued_at(store: Option<&Store>, cache_dir: &Path, canonical: &str) -> Option<SystemTime> {
    if let Some(store) = store
        && let Some(bytes) = store.get(&issued_kv_key(canonical))
        && let Some(t) = parse_unix_secs(bytes.as_ref())
    {
        return Some(t);
    }
    let stem = cache_stem(canonical);
    let bytes = std::fs::read(cache_dir.join(format!("{stem}.issued"))).ok()?;
    parse_unix_secs(&bytes)
}

fn parse_unix_secs(bytes: &[u8]) -> Option<SystemTime> {
    let secs: u64 = std::str::from_utf8(bytes).ok()?.trim().parse().ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

/// Persist the certificate to KV (cluster-wide) and disk (fast local restart).
fn persist_cert(ctx: &Dns01Context, cert_pem: &[u8], key_pem: &[u8], issued: SystemTime) {
    let secs =
        issued.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default();
    if let Some(store) = &ctx.store {
        crate::acme::store_acme_cert(store, &ctx.canonical, cert_pem, key_pem);
        store.set(issued_kv_key(&ctx.canonical), secs.to_string().into_bytes(), None);
    }
    let stem = cache_stem(&ctx.canonical);
    write_file(&ctx.cache_dir.join(format!("{stem}.crt")), cert_pem);
    write_file(&ctx.cache_dir.join(format!("{stem}.key")), key_pem);
    write_file(&ctx.cache_dir.join(format!("{stem}.issued")), secs.to_string().as_bytes());
}

/// Load account credential JSON — KV first, then disk.
fn load_account_creds(ctx: &Dns01Context) -> Option<Vec<u8>> {
    if let Some(store) = &ctx.store
        && let Some(bytes) = store.get(&account_kv_key(&ctx.directory_url))
    {
        return Some(bytes.to_vec());
    }
    std::fs::read(ctx.cache_dir.join("account.json")).ok()
}

/// Persist account credential JSON to KV (cluster-wide) and disk.
///
/// Broadcast rather than `set` for the same reason the certificate is: the
/// credential JSON carries a private key and is large enough to take the
/// sharded large-value tier, which would leave it on a subset of nodes. Every
/// node must resolve the *same* ACME account.
fn persist_account_creds(ctx: &Dns01Context, bytes: &[u8]) {
    if let Some(store) = &ctx.store {
        store.set_broadcast(account_kv_key(&ctx.directory_url), bytes.to_vec(), None);
    }
    write_file(&ctx.cache_dir.join("account.json"), bytes);
}

/// KV key for the ACME account credentials, namespaced by directory URL so
/// staging and production accounts never collide.
fn account_kv_key(directory_url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(directory_url.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(16);
    for b in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    format!("acme:account:dns01:{hex}")
}

/// Write a file, logging (not propagating) any error — cache writes are
/// best-effort and must never take TLS down.
fn write_file(path: &Path, bytes: &[u8]) {
    if let Err(e) = std::fs::write(path, bytes) {
        tracing::warn!(path = %path.display(), error = %e, "DNS-01: cache write failed");
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener as StdTcpListener;

    use super::*;

    #[test]
    fn zone_candidates_walks_parents_longest_first() {
        let got = zone_candidates("_acme-challenge.preview.ephpm.dev");
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
    fn zone_candidates_stops_at_two_labels() {
        assert_eq!(zone_candidates("ephpm.dev"), vec!["ephpm.dev".to_string()]);
        assert_eq!(zone_candidates("dev"), Vec::<String>::new());
    }

    #[test]
    fn challenge_fqdn_strips_wildcard() {
        assert_eq!(challenge_fqdn("*.preview.ephpm.dev"), "_acme-challenge.preview.ephpm.dev");
        assert_eq!(challenge_fqdn("ephpm.dev"), "_acme-challenge.ephpm.dev");
    }

    #[test]
    fn canonical_domain_key_is_order_independent() {
        let a = canonical_domain_key(&["b.example".into(), "a.example".into()]);
        let b = canonical_domain_key(&["a.example".into(), "b.example".into()]);
        assert_eq!(a, b);
        assert_eq!(a, "a.example,b.example");
    }

    #[test]
    fn cache_stem_is_filename_safe() {
        assert_eq!(cache_stem("*.preview.ephpm.dev,ephpm.dev"), "_.preview.ephpm.dev_ephpm.dev");
    }

    #[test]
    fn resolver_starts_empty_and_installs() {
        let resolver = Dns01CertResolver::new();
        assert!(!resolver.has_cert());
        // A self-signed pair proves the install/parse path without a CA.
        let key = rcgen::KeyPair::generate().expect("keypair");
        let params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).expect("params");
        let cert = params.self_signed(&key).expect("self-sign");
        assert!(resolver.install(cert.pem().as_bytes(), key.serialize_pem().as_bytes()).is_ok());
        assert!(resolver.has_cert());
    }

    #[test]
    fn account_kv_key_differs_by_directory() {
        let staging = account_kv_key(LetsEncrypt::Staging.url());
        let production = account_kv_key(LetsEncrypt::Production.url());
        assert_ne!(staging, production);
        assert!(staging.starts_with("acme:account:dns01:"));
    }

    /// A minimal one-shot HTTP server that captures the first request and
    /// replies with a canned body. Enough to assert Cloudflare request shaping
    /// without touching the network.
    fn one_shot_http(response_body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).expect("read");
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            // Read a fixed-size body if the headers announced one; the test
            // payloads are tiny so the initial read already has everything.
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).expect("write");
            stream.flush().ok();
            request
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn cloudflare_set_txt_posts_the_right_record() {
        let body = r#"{"success":true,"errors":[],"result":{"id":"rec123"}}"#;
        let (base, handle) = one_shot_http(body);
        let provider =
            CloudflareProvider::new("tok-secret".to_string(), Some("zone999".to_string()))
                .expect("provider")
                .with_api_base(base);

        provider
            .set_txt("_acme-challenge.preview.ephpm.dev", "the-txt-value")
            .await
            .expect("set_txt");

        let request = tokio::task::spawn_blocking(move || handle.join().expect("join"))
            .await
            .expect("captured request");

        assert!(request.starts_with("POST /zones/zone999/dns_records"), "request line: {request}");
        assert!(request.contains("authorization: Bearer tok-secret"), "missing bearer: {request}");
        assert!(request.contains("content-type: application/json"), "missing content-type");
        assert!(
            request.contains("\"name\":\"_acme-challenge.preview.ephpm.dev\""),
            "body: {request}"
        );
        assert!(request.contains("\"content\":\"the-txt-value\""), "body: {request}");
        assert!(request.contains("\"type\":\"TXT\""), "body: {request}");
    }

    #[tokio::test]
    async fn cloudflare_api_error_is_surfaced() {
        let body = r#"{"success":false,"errors":[{"code":10000,"message":"Authentication error"}],"result":null}"#;
        let (base, handle) = one_shot_http(body);
        let provider = CloudflareProvider::new("bad".to_string(), Some("z".to_string()))
            .expect("provider")
            .with_api_base(base);

        let err = provider
            .set_txt("_acme-challenge.x.example", "v")
            .await
            .expect_err("must surface the API error");
        let msg = format!("{err:#}");
        assert!(msg.contains("Authentication error"), "unexpected: {msg}");
        let _ = tokio::task::spawn_blocking(move || handle.join().ok()).await;
    }
}
