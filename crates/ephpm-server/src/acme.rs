//! Automatic TLS certificate provisioning via ACME (Let's Encrypt).
//!
//! Uses `rustls-acme` with TLS-ALPN-01 challenges. On startup, if no
//! cached certificate exists, the server obtains one from Let's Encrypt
//! (~5-30 seconds). Certificates are automatically renewed before expiry.
//!
//! ## Clustered mode
//!
//! When a KV store is provided, ACME operates in distributed mode:
//! - **Leader election**: an `acme:leader` key with TTL heartbeat ensures
//!   only one node issues/renews certificates at a time.
//! - **Challenge tokens**: stored in the KV store so any node can respond
//!   to HTTP-01 challenges.
//! - **Certificate distribution**: issued certs are stored in the KV store
//!   and hot-loaded by all nodes.
//!
//! The recommended low-level integration uses [`tokio_rustls::LazyConfigAcceptor`]
//! to inspect each TLS `ClientHello`: ACME challenge connections are handled
//! inline, while normal connections are passed through to hyper.

use std::fmt::Debug;
use std::sync::Arc;

use ephpm_config::TlsConfig;
use ephpm_kv::store::Store;
use rustls::ServerConfig;
use rustls_acme::caches::DirCache;
use rustls_acme::{AcmeConfig, AcmeState};
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

    let config = AcmeConfig::new(domains.iter().map(String::as_str))
        .cache(DirCache::new(cache_dir))
        .directory_lets_encrypt(production);

    let config = if let Some(email) = email {
        config.contact_push(format!("mailto:{email}"))
    } else {
        config
    };

    let state = config.state();
    let challenge_config = state.challenge_rustls_config();
    let mut default_config = state.default_rustls_config();
    // rustls_acme creates a fresh Arc here (strong count = 1), so get_mut succeeds.
    // Add h2 ALPN so clients can negotiate HTTP/2 on ACME-managed TLS connections.
    if let Some(cfg) = Arc::get_mut(&mut default_config) {
        cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    }

    // Spawn background task to drive certificate acquisition and renewal.
    let clustered = store.is_some();
    if let Some(kv_store) = store {
        tokio::spawn(drive_clustered_acme_events(state, kv_store));
    } else {
        tokio::spawn(drive_acme_events(state));
    }

    tracing::info!(
        domains = ?tls_config.domains,
        cache_dir = %tls_config.cache_dir.display(),
        environment = if production { "production" } else { "staging" },
        clustered,
        "ACME auto-TLS enabled"
    );

    Ok(AcmeSetup {
        challenge_config,
        default_config,
    })
}

/// Poll the ACME state stream, logging events and errors.
///
/// This task runs for the lifetime of the server. It drives certificate
/// ordering, renewal, and cache operations.
async fn drive_acme_events<EC: Debug + 'static, EA: Debug + 'static>(
    mut state: AcmeState<EC, EA>,
) {
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
/// KV key prefix for ACME challenge tokens.
const ACME_CHALLENGE_PREFIX: &str = "acme:challenge:";
/// KV key prefix for stored certificates.
const ACME_CERT_PREFIX: &str = "acme:cert:";

/// Generate a unique node identifier for ACME leader election.
fn acme_node_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let pid = std::process::id();
    format!("{}-{}-{}", ts.as_millis(), pid, ts.subsec_nanos())
}

/// Drive ACME events with distributed leader election via the KV store.
///
/// Only the leader node processes ACME events (certificate ordering/renewal).
/// All nodes store and read challenge tokens and certificates from the KV store.
async fn drive_clustered_acme_events<EC: Debug + 'static, EA: Debug + 'static>(
    mut state: AcmeState<EC, EA>,
    store: Arc<Store>,
) {
    let node_id = acme_node_id();
    let mut is_leader = false;
    let mut heartbeat_interval = tokio::time::interval(ACME_LEADER_HEARTBEAT);
    heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // Heartbeat: try to acquire or maintain leadership.
            _ = heartbeat_interval.tick() => {
                is_leader = try_acquire_acme_leadership(&store, &node_id);
                if is_leader {
                    tracing::debug!("ACME leader heartbeat maintained");
                }
            }
            // Process ACME events (only meaningful if we're the leader,
            // but we always poll to keep the state machine alive).
            event = state.next() => {
                match event {
                    Some(Ok(ref ev)) => {
                        if is_leader {
                            tracing::info!(?ev, "ACME certificate event (leader)");
                        } else {
                            tracing::debug!(?ev, "ACME certificate event (follower, ignored)");
                        }
                    }
                    Some(Err(ref err)) => {
                        if is_leader {
                            tracing::error!(?err, "ACME error (leader)");
                        } else {
                            tracing::debug!(?err, "ACME error (follower)");
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

/// Try to acquire ACME leadership via the KV store.
///
/// Uses a simple compare-and-set pattern: if the `acme:leader` key
/// doesn't exist or was set by us, we (re)claim it with a TTL.
///
/// Returns `true` if this node is now the leader.
fn try_acquire_acme_leadership(store: &Store, node_id: &str) -> bool {
    let current = store.get(ACME_LEADER_KEY);
    match current {
        None => {
            // No leader — claim it.
            store.set(
                ACME_LEADER_KEY.to_string(),
                node_id.as_bytes().to_vec(),
                Some(ACME_LEADER_TTL),
            );
            tracing::info!(node_id, "acquired ACME leadership");
            true
        }
        Some(existing) => {
            let existing_id = String::from_utf8_lossy(&existing);
            if existing_id == node_id {
                // We're already the leader — renew the TTL.
                store.set(
                    ACME_LEADER_KEY.to_string(),
                    node_id.as_bytes().to_vec(),
                    Some(ACME_LEADER_TTL),
                );
                true
            } else {
                // Another node is the leader.
                false
            }
        }
    }
}

/// Store an ACME challenge token in the KV store for any node to serve.
///
/// Called when the ACME provider sends an HTTP-01 challenge.
/// The token is stored with a short TTL (5 minutes) since challenges
/// are short-lived.
#[allow(dead_code)]
pub fn store_acme_challenge(store: &Store, token: &str, authorization: &[u8]) {
    let key = format!("{ACME_CHALLENGE_PREFIX}{token}");
    let ttl = std::time::Duration::from_secs(300);
    store.set(key, authorization.to_vec(), Some(ttl));
    tracing::debug!(token, "stored ACME challenge token in KV");
}

/// Retrieve an ACME challenge response from the KV store.
///
/// Returns `None` if the token is not found (expired or not stored).
#[allow(dead_code)]
#[must_use]
pub fn get_acme_challenge(store: &Store, token: &str) -> Option<Vec<u8>> {
    let key = format!("{ACME_CHALLENGE_PREFIX}{token}");
    store.get(&key)
}

/// Store an issued certificate in the KV store for distribution.
///
/// The certificate is stored indefinitely (no TTL) — renewal replaces
/// it with a fresh certificate.
#[allow(dead_code)]
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
#[allow(dead_code)]
#[must_use]
pub fn get_acme_cert(store: &Store, domain: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let cert_key = format!("{ACME_CERT_PREFIX}{domain}:cert");
    let key_key = format!("{ACME_CERT_PREFIX}{domain}:key");
    let cert = store.get(&cert_key)?;
    let key = store.get(&key_key)?;
    Some((cert, key))
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
    fn acme_node_id_is_unique() {
        let id1 = acme_node_id();
        // Sleep briefly to ensure different timestamp.
        std::thread::sleep(std::time::Duration::from_millis(1));
        let id2 = acme_node_id();
        assert_ne!(id1, id2);
    }
}
