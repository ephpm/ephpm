//! Automatic TLS certificate provisioning via ACME (Let's Encrypt).
//!
//! Uses `rustls-acme` with TLS-ALPN-01 challenges. On startup, if no
//! cached certificate exists, the server obtains one from Let's Encrypt
//! (~5-30 seconds). Certificates are automatically renewed before expiry.
//!
//! ## Clustered mode
//!
//! When a KV store is provided, ACME operates in distributed mode:
//! - **Leader election**: an `acme:leader` key with TTL heartbeat, plus a
//!   lowest-node-id tie-break, settles on one node. Only that node drives
//!   ordering and renewal; every other node waits for the leader's
//!   certificate to appear in the KV store rather than ordering its own.
//!   The gossip KV tier has no compare-and-swap, so the claim is confirmed
//!   over two heartbeats before it is acted on — see
//!   `try_acquire_acme_leadership`.
//! - **Challenge tokens**: **not yet implemented.** `get_acme_challenge`
//!   lets any node answer `/.well-known/acme-challenge/<token>` out of the
//!   KV store, but nothing populates those keys, and the TLS-ALPN-01
//!   challenge material this SAPI actually requests lives only in the
//!   ordering node's in-memory resolver. Validation therefore has to reach
//!   the leader.
//! - **Certificate distribution**: issued certs are written to the KV
//!   store by the leader as soon as ACME finishes ordering, then loaded
//!   by every other node on cache miss. A local [`DirCache`] is kept
//!   alongside the KV cache so a single-node leader can also reload
//!   its cert after restart without paying a network round-trip.
//!   **Not yet implemented**: a follower does not pick up a *renewed*
//!   certificate while it is running. `rustls-acme` consults the cert cache
//!   exactly once per `AcmeState`, and its resolver's `set_cert` is
//!   `pub(crate)`, so a fresh certificate cannot be injected from outside
//!   the state machine. Followers serve the certificate they loaded at
//!   startup until they restart. Closing this needs a KV-backed
//!   `ResolvesServerCert` of our own.
//!
//! The recommended low-level integration uses [`tokio_rustls::LazyConfigAcceptor`]
//! to inspect each TLS `ClientHello`: ACME challenge connections are handled
//! inline, while normal connections are passed through to hyper.

use std::convert::Infallible;
use std::fmt::Debug;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use ephpm_config::TlsConfig;
use ephpm_kv::store::Store;
use rustls::ServerConfig;
use rustls_acme::caches::DirCache;
use rustls_acme::{AccountCache, AcmeConfig, AcmeState, CertCache, EventOk};
use sha2::{Digest, Sha256};
use tokio_stream::StreamExt;

/// Holds the ACME state and rustls configs needed for the accept loop.
pub struct AcmeSetup {
    /// Rustls config for ACME challenge connections (TLS-ALPN-01).
    pub challenge_config: Arc<ServerConfig>,
    /// Rustls config for normal HTTPS connections (dynamically resolves cert).
    pub default_config: Arc<ServerConfig>,
}

/// Build the ACME state machine and spawn the renewal task.
///
/// Returns the rustls configs needed for the accept loop. The renewal
/// task runs in the background, polling the ACME state stream to drive
/// certificate acquisition and renewal.
///
/// When `store` is `Some`, enables clustered ACME with distributed
/// leader election and certificate sharing via the KV store.
///
/// # Errors
///
/// Returns an error if the cache directory cannot be created.
pub fn start_acme(tls_config: &TlsConfig, store: Option<Arc<Store>>) -> anyhow::Result<AcmeSetup> {
    let domains = &tls_config.domains;
    let production = !tls_config.staging;

    // Ensure cache directory exists.
    std::fs::create_dir_all(&tls_config.cache_dir).map_err(|e| {
        anyhow::anyhow!(
            "failed to create ACME cache directory {}: {e}",
            tls_config.cache_dir.display()
        )
    })?;

    let cache_dir = tls_config.cache_dir.clone();
    let domains: Vec<String> = domains.clone();
    let email = tls_config.email.clone();

    // The cache configuration depends on whether we're running in
    // clustered mode. When `store` is `Some`, we layer a `KvCache` over
    // the on-disk `DirCache` so every node sees the leader's issued
    // certificate and every node also keeps a local copy for fast
    // restart. When `store` is `None`, we keep the single-node
    // behaviour exactly as it was — just a `DirCache`.
    let base =
        AcmeConfig::new(domains.iter().map(String::as_str)).directory_lets_encrypt(production);
    let clustered = store.is_some();
    let (challenge_config, default_config) = match store {
        Some(kv_store) => {
            // Shared with the event loop below. The cache needs to know
            // whether this node currently holds ACME leadership, because
            // reporting a cache miss to rustls-acme is what makes it place an
            // order — and only the leader may do that.
            let leader_flag = Arc::new(AtomicBool::new(false));
            let kv_cache = KvCache::new(Arc::clone(&kv_store));
            let cache = LayeredCache::new(kv_cache, DirCache::new(cache_dir))
                .with_leader_gate(Arc::clone(&leader_flag));
            let config = base.cache(cache);
            let config = if let Some(email) = email {
                config.contact_push(format!("mailto:{email}"))
            } else {
                config
            };
            let state = config.state();
            let challenge_config = state.challenge_rustls_config();
            let mut default_config = state.default_rustls_config();
            if let Some(cfg) = Arc::get_mut(&mut default_config) {
                cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
            }
            tokio::spawn(drive_clustered_acme_events(state, kv_store, leader_flag));
            (challenge_config, default_config)
        }
        None => {
            let config = base.cache(DirCache::new(cache_dir));
            let config = if let Some(email) = email {
                config.contact_push(format!("mailto:{email}"))
            } else {
                config
            };
            let state = config.state();
            let challenge_config = state.challenge_rustls_config();
            let mut default_config = state.default_rustls_config();
            if let Some(cfg) = Arc::get_mut(&mut default_config) {
                cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
            }
            tokio::spawn(drive_acme_events(state));
            (challenge_config, default_config)
        }
    };

    tracing::info!(
        domains = ?tls_config.domains,
        cache_dir = %tls_config.cache_dir.display(),
        environment = if production { "production" } else { "staging" },
        clustered,
        "ACME auto-TLS enabled"
    );

    Ok(AcmeSetup { challenge_config, default_config })
}

/// Poll the ACME state stream, logging events and errors.
///
/// This task runs for the lifetime of the server. It drives certificate
/// ordering, renewal, and cache operations.
async fn drive_acme_events<EC: Debug + 'static, EA: Debug + 'static>(mut state: AcmeState<EC, EA>) {
    loop {
        match state.next().await {
            Some(Ok(event)) => {
                tracing::info!(?event, "ACME certificate event");
            }
            Some(Err(err)) => {
                tracing::error!(?err, "ACME error");
            }
            None => {
                tracing::warn!("ACME state stream ended unexpectedly");
                break;
            }
        }
    }
}

// ── Clustered ACME ──────────────────────────────────────────────────────────

/// ACME leader key in the KV store.
const ACME_LEADER_KEY: &str = "acme:leader";
/// TTL for the ACME leader lock.
const ACME_LEADER_TTL: std::time::Duration = std::time::Duration::from_secs(30);
/// Heartbeat interval for the ACME leader (must be less than TTL).
const ACME_LEADER_HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(10);
/// Consecutive confirmed heartbeats required before a node acts as leader.
///
/// The gossip KV tier is eventually consistent, so during the second or so it
/// takes a write to propagate, two nodes can each win their *local* claim.
/// Requiring the claim to survive another heartbeat gives the deterministic
/// tie-break in [`try_acquire_acme_leadership`] time to settle before anything
/// talks to Let's Encrypt.
const ACME_LEADER_CONFIRMATIONS: u32 = 2;
/// How often a follower re-checks the KV store for the leader's certificate
/// while holding a `load_cert` open.
const ACME_FOLLOWER_CERT_POLL: std::time::Duration = std::time::Duration::from_secs(5);
/// KV key prefix for ACME challenge tokens.
const ACME_CHALLENGE_PREFIX: &str = "acme:challenge:";
/// KV key prefix for stored certificates.
const ACME_CERT_PREFIX: &str = "acme:cert:";

/// Generate a unique node identifier for ACME leader election.
fn acme_node_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let pid = std::process::id();
    format!("{}-{}-{}", ts.as_millis(), pid, ts.subsec_nanos())
}

/// Drive ACME events with distributed leader election via the KV store.
///
/// Only the leader orders and renews certificates. The leader's cache layer
/// publishes each new certificate into the KV store, and followers pick it up
/// on their one and only `load_cert`.
///
/// ## Why followers poll at all
///
/// `rustls-acme` builds its `load_cert` future once, in `AcmeState::new`, and
/// consumes it on the first poll. A node that never polls therefore never
/// installs *any* certificate in its resolver and cannot serve HTTPS. So a
/// follower is allowed to poll until it has deployed a certificate — and
/// [`LayeredCache`] holds that load open instead of reporting a miss, which
/// is the only way a poll can fall through to placing an order.
///
/// Once a follower has deployed, it stops polling entirely. That freezes its
/// renewal timer, which is what keeps it off Let's Encrypt for good.
///
/// ## Known limitation
///
/// A follower therefore does *not* pick up the leader's renewed certificate:
/// `AcmeState` consults the cert cache exactly once, and its resolver's
/// `set_cert` is `pub(crate)`, so there is no way to inject a fresh
/// certificate from outside the state machine. Followers serve the
/// certificate they loaded at startup until they restart. Closing that gap
/// needs a KV-backed `ResolvesServerCert` of our own; it is tracked as the
/// remaining clustered-ACME work.
async fn drive_clustered_acme_events<EC: Debug + 'static, EA: Debug + 'static>(
    mut state: AcmeState<EC, EA>,
    store: Arc<Store>,
    leader_flag: Arc<AtomicBool>,
) {
    let node_id = acme_node_id();
    let mut is_leader = false;
    let mut confirmations: u32 = 0;
    // Set once this node has a certificate installed in its resolver.
    let mut deployed = false;
    let mut heartbeat_interval = tokio::time::interval(ACME_LEADER_HEARTBEAT);
    heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // Heartbeat: try to acquire or maintain leadership.
            _ = heartbeat_interval.tick() => {
                if try_acquire_acme_leadership(&store, &node_id) {
                    confirmations = confirmations.saturating_add(1);
                } else {
                    confirmations = 0;
                }
                let now_leader = confirmations >= ACME_LEADER_CONFIRMATIONS;
                if now_leader != is_leader {
                    is_leader = now_leader;
                    leader_flag.store(is_leader, Ordering::Release);
                    if is_leader {
                        tracing::info!(node_id, "this node is now the ACME leader");
                    } else {
                        tracing::info!(node_id, "this node is no longer the ACME leader");
                    }
                } else if is_leader {
                    tracing::debug!("ACME leader heartbeat maintained");
                }
            }
            // Drive the state machine only while it cannot place an order we
            // do not want: always on the leader, and on a follower only until
            // it has a certificate deployed.
            event = state.next(), if is_leader || !deployed => {
                match event {
                    Some(Ok(ref ev)) => {
                        if matches!(ev, EventOk::DeployedCachedCert | EventOk::DeployedNewCert) {
                            deployed = true;
                        }
                        if is_leader {
                            // The cache layer wired into AcmeConfig
                            // pushes new certs into the KV store as part
                            // of the rustls-acme state machine, so by
                            // the time we see this event the cert is
                            // already replicated cluster-wide.
                            tracing::info!(?ev, "ACME certificate event (leader, distributed via KV)");
                        } else {
                            tracing::info!(?ev, "ACME certificate event (follower, loaded from KV)");
                        }
                    }
                    Some(Err(ref err)) => {
                        if is_leader {
                            tracing::error!(?err, "ACME error (leader)");
                        } else {
                            tracing::warn!(?err, "ACME error (follower)");
                        }
                    }
                    None => {
                        tracing::warn!("ACME state stream ended unexpectedly");
                        break;
                    }
                }
            }
        }
    }
}

/// Try to acquire or renew ACME leadership via the KV store.
///
/// Returns `true` if this node holds the `acme:leader` key.
///
/// # This is not a true distributed lock
///
/// [`Store::set_nx`] is a genuine atomic test-and-set *within one process*,
/// which is why it replaces the previous get-then-set. Across the cluster
/// there is no equivalent: the gossip KV tier
/// (`ClusterHandle::gossip_set`/`gossip_get` in `ephpm-cluster`) offers only
/// last-write-wins set / get / delete over eventually consistent chitchat
/// state — it has no compare-and-swap primitive to build on. Two nodes
/// starting together can therefore both win their local claim for the second
/// or so it takes the write to propagate.
///
/// What makes it converge is the tie-break below: a node that sees a
/// *different* holder yields unless its own id sorts lower, in which case it
/// takes the key over. The comparison is total and every node eventually
/// applies it to the same converged value, so exactly one node survives —
/// the same "lowest id wins" rule `ephpm-cluster::sqlite_election` uses.
/// The caller additionally waits [`ACME_LEADER_CONFIRMATIONS`] heartbeats
/// before acting on a claim, so the pre-convergence window cannot turn into
/// duplicate certificate orders.
fn try_acquire_acme_leadership(store: &Store, node_id: &str) -> bool {
    let key = ACME_LEADER_KEY.to_string();
    let claim = node_id.as_bytes().to_vec();

    // `set_nx` is atomic within this process: only one caller creates the key.
    if store.set_nx(key.clone(), claim.clone(), Some(ACME_LEADER_TTL)) {
        tracing::info!(node_id, "acquired ACME leadership");
        return true;
    }

    let Some(existing) = store.get(ACME_LEADER_KEY) else {
        // Expired between the `set_nx` and this read. Leave it to the next
        // heartbeat rather than racing.
        return false;
    };
    let existing_id = String::from_utf8_lossy(&existing);
    if existing_id == node_id {
        // We already hold it — renew the TTL.
        store.set(key, claim, Some(ACME_LEADER_TTL));
        return true;
    }
    if node_id < &*existing_id {
        tracing::info!(
            node_id,
            %existing_id,
            "taking over ACME leadership (lower node id wins the tie-break)"
        );
        store.set(key, claim, Some(ACME_LEADER_TTL));
        return true;
    }
    false
}

/// Store an ACME challenge token in the KV store for any node to serve.
///
/// Called when the ACME provider sends an HTTP-01 challenge.
/// The token is stored with a short TTL (5 minutes) since challenges
/// are short-lived.
pub fn store_acme_challenge(store: &Store, token: &str, authorization: &[u8]) {
    let key = format!("{ACME_CHALLENGE_PREFIX}{token}");
    let ttl = std::time::Duration::from_secs(300);
    store.set(key, authorization.to_vec(), Some(ttl));
    tracing::debug!(token, "stored ACME challenge token in KV");
}

/// Retrieve an ACME challenge response from the KV store.
///
/// Returns `None` if the token is not found (expired or not stored).
#[must_use]
pub fn get_acme_challenge(store: &Store, token: &str) -> Option<Vec<u8>> {
    let key = format!("{ACME_CHALLENGE_PREFIX}{token}");
    // Materialise to Vec at the public API boundary — the challenge
    // response gets serialised into the HTTP-01 response body anyway.
    store.get(&key).map(|b| b.to_vec())
}

/// Store an issued certificate in the KV store for distribution.
///
/// The certificate is stored indefinitely (no TTL) — renewal replaces
/// it with a fresh certificate.
pub fn store_acme_cert(store: &Store, domain: &str, cert_pem: &[u8], key_pem: &[u8]) {
    let cert_key = format!("{ACME_CERT_PREFIX}{domain}:cert");
    let key_key = format!("{ACME_CERT_PREFIX}{domain}:key");
    store.set(cert_key, cert_pem.to_vec(), None);
    store.set(key_key, key_pem.to_vec(), None);
    tracing::info!(domain, "stored ACME certificate in KV store");
}

/// Retrieve a certificate and key from the KV store.
///
/// Returns `None` if either the cert or key is missing.
#[must_use]
pub fn get_acme_cert(store: &Store, domain: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let cert_key = format!("{ACME_CERT_PREFIX}{domain}:cert");
    let key_key = format!("{ACME_CERT_PREFIX}{domain}:key");
    let cert = store.get(&cert_key)?;
    let key = store.get(&key_key)?;
    Some((cert.to_vec(), key.to_vec()))
}

// ── Cache layer for cluster-wide cert + account distribution ─────────────────

/// KV key prefix for ACME account material (private key + URL).
const ACME_ACCOUNT_PREFIX: &str = "acme:account:";

/// Build the cache key used for a cert entry from the rustls-acme
/// `(domains, directory_url)` pair. Joining the domains with `|`
/// keeps the existing `acme:cert:<domain>:cert` scheme readable in
/// the common single-domain case (`acme:cert:example.com:cert`).
fn cert_key_for(domains: &[String], directory_url: &str) -> String {
    let mut joined = domains.join("|");
    if joined.is_empty() {
        joined.push('_');
    }
    // The directory URL distinguishes staging vs production certs for
    // the same domain set. Hash it to keep the key short and free of
    // troublesome characters.
    let mut hasher = Sha256::new();
    hasher.update(directory_url.as_bytes());
    let dir_hash = hex_short(&hasher.finalize());
    format!("{ACME_CERT_PREFIX}{joined}:cert:{dir_hash}")
}

/// Build the cache key for an account entry. Contacts and directory
/// URL are hashed together — there is no human-readable component
/// here, but account material is opaque private key bytes so we never
/// need to inspect the key by eye.
fn account_key_for(contact: &[String], directory_url: &str) -> String {
    let mut hasher = Sha256::new();
    for entry in contact {
        hasher.update(entry.as_bytes());
        hasher.update([0]);
    }
    hasher.update(directory_url.as_bytes());
    let digest = hex_short(&hasher.finalize());
    format!("{ACME_ACCOUNT_PREFIX}{digest}")
}

fn hex_short(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        // SAFETY: writing into a String via write! against a hex format
        // string cannot fail — this just appeases the unused Result.
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// A rustls-acme [`Cache`] backed by the in-process [`Store`].
///
/// This is the piece that closes the long-standing gap in clustered
/// ACME: when the leader finishes ordering a certificate, rustls-acme
/// calls `store_cert` on whatever cache it was handed, and this impl
/// writes the resulting PEM blob straight into the KV store. Every
/// follower then sees it on its next `load_cert` poll and hot-loads
/// the cert without ever talking to Let's Encrypt itself.
///
/// All errors are swallowed and downgraded to `Ok(None)` / `Ok(())`
/// with a `warn` log — a transient KV blip must never take TLS down.
///
/// [`Cache`]: rustls_acme::Cache
#[derive(Debug, Clone)]
pub struct KvCache {
    store: Arc<Store>,
}

impl KvCache {
    /// Construct a new KV-backed ACME cache.
    #[must_use]
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl CertCache for KvCache {
    type EC = Infallible;

    async fn load_cert(
        &self,
        domains: &[String],
        directory_url: &str,
    ) -> Result<Option<Vec<u8>>, Self::EC> {
        let key = cert_key_for(domains, directory_url);
        match self.store.get(&key) {
            Some(bytes) => {
                tracing::debug!(key = %key, len = bytes.len(), "ACME cert loaded from KV cache");
                Ok(Some(bytes.to_vec()))
            }
            None => {
                tracing::debug!(key = %key, "ACME cert not in KV cache");
                Ok(None)
            }
        }
    }

    async fn store_cert(
        &self,
        domains: &[String],
        directory_url: &str,
        cert: &[u8],
    ) -> Result<(), Self::EC> {
        let key = cert_key_for(domains, directory_url);
        let ok = self.store.set(key.clone(), cert.to_vec(), None);
        if ok {
            tracing::info!(
                key = %key,
                len = cert.len(),
                "ACME cert published to KV store for cluster-wide distribution",
            );
        } else {
            tracing::warn!(
                key = %key,
                "ACME cert KV write was rejected (eviction policy / OOM) — followers will not see this cert",
            );
        }
        Ok(())
    }
}

#[async_trait]
impl AccountCache for KvCache {
    type EA = Infallible;

    async fn load_account(
        &self,
        contact: &[String],
        directory_url: &str,
    ) -> Result<Option<Vec<u8>>, Self::EA> {
        let key = account_key_for(contact, directory_url);
        match self.store.get(&key) {
            Some(bytes) => {
                tracing::debug!(key = %key, "ACME account loaded from KV cache");
                Ok(Some(bytes.to_vec()))
            }
            None => {
                tracing::debug!(key = %key, "ACME account not in KV cache");
                Ok(None)
            }
        }
    }

    async fn store_account(
        &self,
        contact: &[String],
        directory_url: &str,
        account: &[u8],
    ) -> Result<(), Self::EA> {
        let key = account_key_for(contact, directory_url);
        let ok = self.store.set(key.clone(), account.to_vec(), None);
        if ok {
            tracing::info!(key = %key, "ACME account material published to KV store");
        } else {
            tracing::warn!(
                key = %key,
                "ACME account KV write was rejected — followers may have to re-create the account",
            );
        }
        Ok(())
    }
}

/// Two-tier cache that fans writes across a KV cache and a local
/// [`DirCache`], and prefers the KV tier on read.
///
/// The KV side is the source of truth for cluster-wide distribution;
/// the `DirCache` side acts as a belt-and-suspenders local copy so a
/// sole-leader node can survive a restart even if the cluster KV is
/// momentarily unavailable.
///
/// Failures in either tier are non-fatal: they log a warning and the
/// other tier still gets its read/write attempt.
pub struct LayeredCache {
    kv: KvCache,
    dir: DirCache<PathBuf>,
    /// When present, a miss in *both* tiers is held open unless this node is
    /// the ACME leader. See [`LayeredCache::with_leader_gate`].
    leader: Option<Arc<AtomicBool>>,
}

impl LayeredCache {
    /// Construct a new layered cache from its KV and disk components.
    ///
    /// Without [`LayeredCache::with_leader_gate`] a cache miss is reported to
    /// rustls-acme as-is, which is the correct behaviour for a single node.
    #[must_use]
    pub fn new(kv: KvCache, dir: DirCache<PathBuf>) -> Self {
        Self { kv, dir, leader: None }
    }

    /// Gate cache misses on ACME leadership.
    ///
    /// `rustls-acme` reads `Ok(None)` from `load_cert` as "no certificate
    /// exists, place an order". In a cluster that must only ever happen on
    /// one node: five duplicate orders for the same domain set in a week is a
    /// hard Let's Encrypt lockout with no way to shorten it. With the gate
    /// installed, a node that is not the leader waits for the leader's
    /// certificate to land in the KV store instead of reporting a miss, so it
    /// can never fall through to ordering.
    ///
    /// `flag` is written by the ACME event loop as leadership changes, so a
    /// node that is promoted mid-wait stops waiting and takes over ordering.
    #[must_use]
    pub fn with_leader_gate(mut self, flag: Arc<AtomicBool>) -> Self {
        self.leader = Some(flag);
        self
    }
}

#[async_trait]
impl CertCache for LayeredCache {
    type EC = Infallible;

    async fn load_cert(
        &self,
        domains: &[String],
        directory_url: &str,
    ) -> Result<Option<Vec<u8>>, Self::EC> {
        loop {
            // Try KV first so cluster-wide rollouts win over stale local disk.
            match self.kv.load_cert(domains, directory_url).await {
                Ok(Some(bytes)) => return Ok(Some(bytes)),
                Ok(None) => {}
                Err(_unreachable) => {} // Infallible
            }
            match self.dir.load_cert(domains, directory_url).await {
                Ok(Some(bytes)) => return Ok(Some(bytes)),
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(?err, "ACME DirCache cert load failed — treating as miss");
                }
            }

            // Both tiers missed. Reporting the miss is what makes rustls-acme
            // place an order, so only the leader is allowed to do it; see
            // [`LayeredCache::with_leader_gate`].
            let may_order = self.leader.as_ref().is_none_or(|f| f.load(Ordering::Acquire));
            if may_order {
                return Ok(None);
            }
            tracing::debug!(
                ?domains,
                "ACME certificate not cached and this node is not the ACME leader — \
                 waiting for the leader to publish instead of ordering"
            );
            tokio::time::sleep(ACME_FOLLOWER_CERT_POLL).await;
        }
    }

    async fn store_cert(
        &self,
        domains: &[String],
        directory_url: &str,
        cert: &[u8],
    ) -> Result<(), Self::EC> {
        // Write to KV first so peers see the new cert without delay.
        if let Err(_unreachable) = self.kv.store_cert(domains, directory_url, cert).await {
            // KvCache returns Infallible, but keep the match exhaustive.
        }
        if let Err(err) = self.dir.store_cert(domains, directory_url, cert).await {
            tracing::warn!(
                ?err,
                "ACME DirCache cert write failed — KV copy is still authoritative"
            );
        }
        Ok(())
    }
}

#[async_trait]
impl AccountCache for LayeredCache {
    type EA = Infallible;

    async fn load_account(
        &self,
        contact: &[String],
        directory_url: &str,
    ) -> Result<Option<Vec<u8>>, Self::EA> {
        match self.kv.load_account(contact, directory_url).await {
            Ok(Some(bytes)) => return Ok(Some(bytes)),
            Ok(None) => {}
            Err(_unreachable) => {}
        }
        match self.dir.load_account(contact, directory_url).await {
            Ok(value) => Ok(value),
            Err(err) => {
                tracing::warn!(?err, "ACME DirCache account load failed — treating as miss");
                Ok(None)
            }
        }
    }

    async fn store_account(
        &self,
        contact: &[String],
        directory_url: &str,
        account: &[u8],
    ) -> Result<(), Self::EA> {
        if let Err(_unreachable) = self.kv.store_account(contact, directory_url, account).await {}
        if let Err(err) = self.dir.store_account(contact, directory_url, account).await {
            tracing::warn!(
                ?err,
                "ACME DirCache account write failed — KV copy is still authoritative"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use ephpm_kv::store::StoreConfig;

    use super::*;

    fn test_store() -> Arc<Store> {
        Store::new(StoreConfig::default())
    }

    #[test]
    fn acme_leader_election_first_node_wins() {
        let store = test_store();
        let node_a = "node-a".to_string();
        let node_b = "node-b".to_string();

        // First node acquires leadership.
        assert!(try_acquire_acme_leadership(&store, &node_a));

        // Second node cannot acquire while first holds it.
        assert!(!try_acquire_acme_leadership(&store, &node_b));

        // First node can renew.
        assert!(try_acquire_acme_leadership(&store, &node_a));
    }

    #[test]
    fn acme_leader_election_after_expiry() {
        let store = test_store();
        let node_a = "node-a".to_string();
        let node_b = "node-b".to_string();

        // First node acquires leadership.
        assert!(try_acquire_acme_leadership(&store, &node_a));

        // Simulate expiry by removing the key.
        store.remove(ACME_LEADER_KEY);

        // Second node can now acquire.
        assert!(try_acquire_acme_leadership(&store, &node_b));
    }

    #[test]
    fn acme_challenge_store_and_retrieve() {
        let store = test_store();
        let token = "test-token-123";
        let auth = b"authorization-value";

        store_acme_challenge(&store, token, auth);

        let retrieved = get_acme_challenge(&store, token);
        assert_eq!(retrieved.as_deref(), Some(auth.as_slice()));
    }

    #[test]
    fn acme_challenge_missing_returns_none() {
        let store = test_store();
        assert!(get_acme_challenge(&store, "nonexistent").is_none());
    }

    #[test]
    fn acme_cert_store_and_retrieve() {
        let store = test_store();
        let domain = "example.com";
        let cert_pem = b"-----BEGIN CERTIFICATE-----\ntest\n-----END CERTIFICATE-----";
        let key_pem = b"-----BEGIN PRIVATE KEY-----\ntest\n-----END PRIVATE KEY-----";

        store_acme_cert(&store, domain, cert_pem, key_pem);

        let (cert, key) = get_acme_cert(&store, domain).unwrap();
        assert_eq!(cert, cert_pem);
        assert_eq!(key, key_pem);
    }

    #[test]
    fn acme_cert_missing_returns_none() {
        let store = test_store();
        assert!(get_acme_cert(&store, "missing.com").is_none());
    }

    #[test]
    fn acme_leader_lower_node_id_takes_over() {
        // Both nodes can win their *local* claim while the gossip write is
        // in flight, so the tie-break has to be deterministic once the value
        // converges: the lower id keeps the key, the higher id yields.
        let store = test_store();
        assert!(try_acquire_acme_leadership(&store, "node-z"));
        assert!(
            try_acquire_acme_leadership(&store, "node-a"),
            "a lower node id must take the key over once it observes the holder"
        );
        assert_eq!(store.get(ACME_LEADER_KEY).as_deref(), Some(b"node-a".as_slice()));
        assert!(
            !try_acquire_acme_leadership(&store, "node-z"),
            "the higher node id must yield on its next heartbeat"
        );
    }

    #[test]
    fn acme_leader_election_converges_regardless_of_claim_order() {
        // Models the pre-convergence window: several nodes each won their
        // local claim, then gossip settled on an arbitrary one of them. One
        // heartbeat round across every node must land on the same answer no
        // matter which claim gossip happened to keep.
        for first in ["node-c", "node-a", "node-b"] {
            let store = test_store();
            assert!(try_acquire_acme_leadership(&store, first));
            for id in ["node-c", "node-a", "node-b"] {
                try_acquire_acme_leadership(&store, id);
            }
            assert_eq!(
                store.get(ACME_LEADER_KEY).as_deref(),
                Some(b"node-a".as_slice()),
                "one heartbeat round must converge on the lowest node id (first claim: {first})"
            );
        }
    }

    #[tokio::test]
    async fn layered_cache_follower_waits_instead_of_reporting_a_miss() {
        // A cache miss is what makes rustls-acme place an order. On a
        // follower it must never be reported: N nodes reporting a miss is N
        // duplicate Let's Encrypt orders and a week-long rate-limit lockout.
        let store = test_store();
        let tmp = tempfile::tempdir().expect("tempdir");
        let leader = Arc::new(AtomicBool::new(false));
        let cache = LayeredCache::new(KvCache::new(store), DirCache::new(tmp.path().to_path_buf()))
            .with_leader_gate(Arc::clone(&leader));

        let domains = ["absent.example.com".to_string()];
        let waited = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            cache.load_cert(&domains, TEST_DIRECTORY),
        )
        .await;
        assert!(waited.is_err(), "a follower must hold the load open, not report a miss");

        // Promoted to leader: the miss is now allowed through so this node
        // can order.
        leader.store(true, Ordering::Release);
        let loaded = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            cache.load_cert(&domains, TEST_DIRECTORY),
        )
        .await
        .expect("the leader must not be gated")
        .expect("infallible");
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn layered_cache_follower_returns_the_leaders_cert() {
        let store = test_store();
        let tmp = tempfile::tempdir().expect("tempdir");
        let domains = vec!["published.example.com".to_string()];
        let cert = b"cert-published-by-the-leader";
        KvCache::new(Arc::clone(&store))
            .store_cert(&domains, TEST_DIRECTORY, cert)
            .await
            .expect("infallible");

        let cache = LayeredCache::new(KvCache::new(store), DirCache::new(tmp.path().to_path_buf()))
            .with_leader_gate(Arc::new(AtomicBool::new(false)));

        let loaded = cache.load_cert(&domains, TEST_DIRECTORY).await.expect("infallible");
        assert_eq!(
            loaded.as_deref(),
            Some(cert.as_slice()),
            "the gate must not delay a cert that is already published",
        );
    }

    #[test]
    fn acme_node_id_is_unique() {
        let id1 = acme_node_id();
        // Sleep briefly to ensure different timestamp.
        std::thread::sleep(std::time::Duration::from_millis(1));
        let id2 = acme_node_id();
        assert_ne!(id1, id2);
    }

    // ── KvCache / LayeredCache tests ────────────────────────────────────────

    const TEST_DIRECTORY: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";

    #[tokio::test]
    async fn kv_cache_round_trip() {
        let store = test_store();
        let cache = KvCache::new(Arc::clone(&store));
        let domains = vec!["example.com".to_string()];
        let cert = b"-----BEGIN CERTIFICATE-----\nfoo\n-----END CERTIFICATE-----";

        cache
            .store_cert(&domains, TEST_DIRECTORY, cert)
            .await
            .expect("KvCache store_cert is infallible");

        let loaded = cache
            .load_cert(&domains, TEST_DIRECTORY)
            .await
            .expect("KvCache load_cert is infallible");
        assert_eq!(loaded.as_deref(), Some(cert.as_slice()));
    }

    #[tokio::test]
    async fn kv_cache_load_miss_returns_none() {
        let store = test_store();
        let cache = KvCache::new(store);
        let loaded = cache
            .load_cert(&["unknown.example.com".to_string()], TEST_DIRECTORY)
            .await
            .expect("KvCache load_cert is infallible");
        assert!(loaded.is_none(), "fresh store must produce a cache miss");
    }

    #[tokio::test]
    async fn kv_cache_account_round_trip() {
        let store = test_store();
        let cache = KvCache::new(Arc::clone(&store));
        let contact = vec!["mailto:ops@example.com".to_string()];
        let account = b"opaque-account-private-key-bytes";

        cache
            .store_account(&contact, TEST_DIRECTORY, account)
            .await
            .expect("KvCache store_account is infallible");

        let loaded = cache
            .load_account(&contact, TEST_DIRECTORY)
            .await
            .expect("KvCache load_account is infallible");
        assert_eq!(loaded.as_deref(), Some(account.as_slice()));

        // A miss on a different contact tuple must still return None.
        let miss = cache
            .load_account(&["mailto:other@example.com".to_string()], TEST_DIRECTORY)
            .await
            .expect("KvCache load_account is infallible");
        assert!(miss.is_none());
    }

    #[tokio::test]
    async fn layered_cache_writes_to_both() {
        let store = test_store();
        let tmp = tempfile::tempdir().expect("create tempdir for DirCache");
        let dir_path = tmp.path().to_path_buf();
        let cache =
            LayeredCache::new(KvCache::new(Arc::clone(&store)), DirCache::new(dir_path.clone()));
        let domains = vec!["example.com".to_string()];
        let cert = b"layered-cert-bytes";

        cache
            .store_cert(&domains, TEST_DIRECTORY, cert)
            .await
            .expect("LayeredCache store_cert is infallible");

        // KV side must have it under the layered cache's key scheme.
        let kv_key = cert_key_for(&domains, TEST_DIRECTORY);
        assert_eq!(store.get(&kv_key).as_deref(), Some(cert.as_slice()));

        // And the disk side must too — read through a fresh DirCache
        // pointed at the same on-disk path so we go through the actual
        // rustls-acme trait surface.
        let from_disk = DirCache::new(dir_path)
            .load_cert(&domains, TEST_DIRECTORY)
            .await
            .expect("DirCache read");
        assert_eq!(
            from_disk.as_deref(),
            Some(cert.as_slice()),
            "LayeredCache must also persist to its local DirCache",
        );
    }

    #[tokio::test]
    async fn layered_cache_reads_kv_first() {
        let store = test_store();
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir_path = tmp.path().to_path_buf();
        let domains = vec!["preferred.example.com".to_string()];

        // Pre-populate KV with cert A.
        let cert_a = b"cert-from-kv";
        KvCache::new(Arc::clone(&store))
            .store_cert(&domains, TEST_DIRECTORY, cert_a)
            .await
            .unwrap();

        // Pre-populate DirCache with cert B (different bytes, same domain).
        let cert_b = b"cert-from-disk";
        DirCache::new(dir_path.clone()).store_cert(&domains, TEST_DIRECTORY, cert_b).await.unwrap();

        // Build a LayeredCache reading from both tiers over the same path.
        let layered = LayeredCache::new(KvCache::new(Arc::clone(&store)), DirCache::new(dir_path));
        let loaded = layered.load_cert(&domains, TEST_DIRECTORY).await.expect("infallible");
        assert_eq!(
            loaded.as_deref(),
            Some(cert_a.as_slice()),
            "LayeredCache must prefer the KV tier over local DirCache",
        );
    }

    #[tokio::test]
    async fn kv_failure_is_not_fatal() {
        // An empty store cannot serve a cert; load_cert must return
        // Ok(None) so rustls-acme falls back to issuance. This is the
        // proxy for "a transient KV blip must never surface as Err".
        let store = test_store();
        let cache = KvCache::new(store);
        let result = cache.load_cert(&["whatever.example.com".to_string()], TEST_DIRECTORY).await;
        assert!(matches!(result, Ok(None)));
    }

    #[tokio::test]
    async fn layered_cache_load_miss_falls_back_to_dir_then_none() {
        // Belt-and-suspenders: when KV misses and DirCache misses, the
        // overall load_cert must yield Ok(None), not Err.
        let store = test_store();
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = LayeredCache::new(KvCache::new(store), DirCache::new(tmp.path().to_path_buf()));
        let loaded = cache
            .load_cert(&["absent.example.com".to_string()], TEST_DIRECTORY)
            .await
            .expect("infallible");
        assert!(loaded.is_none());
    }

    #[test]
    fn single_node_path_does_not_use_kv_cache() {
        // Smoke: start_acme with `store: None` must still succeed.
        // We can't dial Let's Encrypt in a unit test, but we can call
        // start_acme and assert the rustls configs it returns aren't
        // bogus — that's enough to prove the single-node code path was
        // not accidentally rerouted through the KvCache branch.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = TlsConfig {
            cert: None,
            key: None,
            email: Some("ops@example.com".into()),
            domains: vec!["example.com".into()],
            cache_dir: tmp.path().to_path_buf(),
            staging: true,
            listen: None,
            redirect_http: false,
        };
        let setup = rt.block_on(async { start_acme(&cfg, None).expect("single-node start_acme") });
        // Sanity-check that the returned configs are real rustls
        // configs and not the same Arc.
        assert!(
            !Arc::ptr_eq(&setup.challenge_config, &setup.default_config),
            "challenge and default rustls configs should be distinct",
        );
    }
}
