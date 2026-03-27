//! Automatic TLS certificate provisioning via ACME (Let's Encrypt).
//!
//! Uses `rustls-acme` with TLS-ALPN-01 challenges. On startup, if no
//! cached certificate exists, the server obtains one from Let's Encrypt
//! (~5-30 seconds). Certificates are automatically renewed before expiry.
//!
//! The recommended low-level integration uses [`tokio_rustls::LazyConfigAcceptor`]
//! to inspect each TLS `ClientHello`: ACME challenge connections are handled
//! inline, while normal connections are passed through to hyper.

use std::fmt::Debug;
use std::sync::Arc;

use ephpm_config::TlsConfig;
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
/// # Errors
///
/// Returns an error if the cache directory cannot be created.
pub fn start_acme(tls_config: &TlsConfig) -> anyhow::Result<AcmeSetup> {
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
    let default_config = state.default_rustls_config();

    // Spawn background task to drive certificate acquisition and renewal.
    tokio::spawn(drive_acme_events(state));

    tracing::info!(
        domains = ?tls_config.domains,
        cache_dir = %tls_config.cache_dir.display(),
        environment = if production { "production" } else { "staging" },
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
