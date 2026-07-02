//! Symmetric encryption for cluster transport.
//!
//! When `[cluster] secret` is set, all inter-node traffic — gossip UDP
//! datagrams and KV data plane TCP frames — is authenticated and
//! encrypted with ChaCha20-Poly1305. The 32-byte keys are derived from
//! the shared secret via HKDF-SHA256 with domain-separated info strings
//! so the gossip plane and the KV data plane never share a key:
//!
//! - gossip UDP: `"ephpm-gossip-v1"`
//! - KV data plane TCP: `"ephpm-kv-data-v1"`
//!
//! ## Sealed message layout
//!
//! ```text
//! [nonce: 12 bytes (random)][ciphertext + 16-byte Poly1305 tag]
//! ```
//!
//! Nodes without the right secret cannot join the gossip mesh, read KV
//! traffic, or inject messages: undecryptable datagrams are silently
//! dropped (rate-limited warning), so a wrong-secret or plaintext peer
//! is invisible rather than an error loop.
//!
//! The gossip side plugs into chitchat as an [`EncryptedUdpTransport`].
//! chitchat's [`Socket`] trait sends and receives typed
//! [`ChitchatMessage`]s (serialization happens inside the socket), so
//! the encrypted transport owns its own UDP socket and mirrors
//! chitchat's `UdpTransport`, sealing after serialization and opening
//! before deserialization. A generic wrapper over an arbitrary inner
//! `Transport` is not possible with this API — there is no byte-level
//! hook to intercept.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::Context;
use async_trait::async_trait;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use chitchat::transport::{Socket, Transport};
use chitchat::{ChitchatMessage, Deserializable, Serializable};
use hkdf::Hkdf;
use sha2::Sha256;

/// HKDF domain-separation info string for the gossip UDP key.
const GOSSIP_INFO: &[u8] = b"ephpm-gossip-v1";

/// HKDF domain-separation info string for the KV data plane key.
const KV_DATA_INFO: &[u8] = b"ephpm-kv-data-v1";

/// ChaCha20-Poly1305 nonce length in bytes.
const NONCE_LEN: usize = 12;

/// Poly1305 authentication tag length in bytes.
const TAG_LEN: usize = 16;

/// Total sealing overhead per message: nonce + tag.
pub const SEAL_OVERHEAD: usize = NONCE_LEN + TAG_LEN;

/// Maximum UDP datagram payload size (mirrors chitchat's internal
/// `MAX_UDP_DATAGRAM_PAYLOAD_SIZE`, which is not public).
const MAX_UDP_PAYLOAD: usize = 65_507;

/// Minimum interval between `warn!`-level logs for dropped datagrams.
///
/// A peer with the wrong secret retries every gossip interval, so
/// without rate limiting the log would fill with one warning per
/// second per bad peer. Drops within the window are logged at debug.
const DROP_WARN_INTERVAL: Duration = Duration::from_secs(30);

/// A symmetric cipher for sealing and opening cluster messages.
///
/// Construct with [`ClusterCipher::for_gossip`] or
/// [`ClusterCipher::for_kv_data_plane`] — the two derive different keys
/// from the same secret so ciphertexts are never valid across planes.
#[derive(Clone)]
pub struct ClusterCipher {
    cipher: ChaCha20Poly1305,
}

impl std::fmt::Debug for ClusterCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print key material.
        f.debug_struct("ClusterCipher").finish_non_exhaustive()
    }
}

impl ClusterCipher {
    /// Derive a cipher from the shared secret with the given HKDF info.
    fn derive(secret: &str, info: &[u8]) -> Self {
        let hkdf = Hkdf::<Sha256>::new(None, secret.as_bytes());
        let mut key = [0u8; 32];
        hkdf.expand(info, &mut key).expect("32 bytes is a valid HKDF-SHA256 output length");
        Self { cipher: ChaCha20Poly1305::new(Key::from_slice(&key)) }
    }

    /// Create the cipher used for gossip UDP datagrams.
    #[must_use]
    pub fn for_gossip(secret: &str) -> Self {
        Self::derive(secret, GOSSIP_INFO)
    }

    /// Create the cipher used for KV data plane TCP frames.
    #[must_use]
    pub fn for_kv_data_plane(secret: &str) -> Self {
        Self::derive(secret, KV_DATA_INFO)
    }

    /// Seal a plaintext message: `nonce || ciphertext+tag`.
    ///
    /// A fresh random 12-byte nonce is generated per message.
    ///
    /// # Errors
    ///
    /// Returns an error if encryption fails (only possible for inputs
    /// near the ChaCha20 length limit of 256 GiB).
    pub fn seal(&self, plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ciphertext = self
            .cipher
            .encrypt(&nonce, plaintext)
            .map_err(|_| anyhow::anyhow!("ChaCha20-Poly1305 encryption failed"))?;
        let mut sealed = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&ciphertext);
        Ok(sealed)
    }

    /// Open a sealed message, returning the plaintext.
    ///
    /// Returns `None` if the message is too short, was sealed with a
    /// different key, or failed authentication (tampered). Callers must
    /// treat `None` as "drop and ignore" — never as a protocol error to
    /// report back to the peer.
    #[must_use]
    pub fn open(&self, sealed: &[u8]) -> Option<Vec<u8>> {
        if sealed.len() < SEAL_OVERHEAD {
            return None;
        }
        let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
        self.cipher.decrypt(Nonce::from_slice(nonce), ciphertext).ok()
    }
}

/// A chitchat [`Transport`] that seals every gossip datagram with
/// ChaCha20-Poly1305 (key derived from `[cluster] secret`).
///
/// Peers without the matching secret cannot join, read, or inject:
/// their datagrams fail authentication and are dropped, and this node's
/// datagrams are undecryptable garbage to them.
pub struct EncryptedUdpTransport {
    cipher: ClusterCipher,
}

impl EncryptedUdpTransport {
    /// Create a transport whose gossip key is derived from `secret`.
    #[must_use]
    pub fn new(secret: &str) -> Self {
        Self { cipher: ClusterCipher::for_gossip(secret) }
    }
}

#[async_trait]
impl Transport for EncryptedUdpTransport {
    async fn open(&self, listen_addr: SocketAddr) -> anyhow::Result<Box<dyn Socket>> {
        let socket = tokio::net::UdpSocket::bind(listen_addr)
            .await
            .with_context(|| format!("failed to bind {listen_addr}/UDP for encrypted gossip"))?;
        Ok(Box::new(EncryptedUdpSocket {
            cipher: self.cipher.clone(),
            socket,
            buf_send: Vec::with_capacity(MAX_UDP_PAYLOAD),
            buf_recv: vec![0u8; MAX_UDP_PAYLOAD].into_boxed_slice(),
            last_drop_warn: None,
            drops_since_warn: 0,
        }))
    }
}

/// UDP socket that seals on send and opens on receive.
struct EncryptedUdpSocket {
    cipher: ClusterCipher,
    socket: tokio::net::UdpSocket,
    buf_send: Vec<u8>,
    buf_recv: Box<[u8]>,
    /// When the last `warn!` for a dropped datagram was emitted.
    last_drop_warn: Option<Instant>,
    /// Datagrams dropped since the last `warn!` (logged at debug).
    drops_since_warn: u64,
}

#[async_trait]
impl Socket for EncryptedUdpSocket {
    async fn send(&mut self, to: SocketAddr, message: ChitchatMessage) -> anyhow::Result<()> {
        self.buf_send.clear();
        message.serialize(&mut self.buf_send);
        let sealed = self.cipher.seal(&self.buf_send)?;
        if sealed.len() > MAX_UDP_PAYLOAD {
            // chitchat packs messages up to the UDP payload limit; the
            // 28-byte sealing overhead can push a maximally-packed
            // datagram over it. Drop rather than fail the gossip loop —
            // the next round retransmits smaller deltas.
            tracing::warn!(
                %to,
                sealed_len = sealed.len(),
                "sealed gossip datagram exceeds UDP payload limit, dropping"
            );
            return Ok(());
        }
        self.socket
            .send_to(&sealed, to)
            .await
            .with_context(|| format!("failed to send encrypted gossip datagram to {to}"))?;
        Ok(())
    }

    async fn recv(&mut self) -> anyhow::Result<(SocketAddr, ChitchatMessage)> {
        loop {
            let (len, from) = self
                .socket
                .recv_from(&mut self.buf_recv)
                .await
                .context("error receiving encrypted gossip datagram")?;

            // A wrong-secret or plaintext peer must be invisible: drop
            // undecryptable datagrams and keep listening.
            let Some(plaintext) = self.cipher.open(&self.buf_recv[..len]) else {
                self.log_drop(from, len);
                continue;
            };

            let mut cursor = plaintext.as_slice();
            match ChitchatMessage::deserialize(&mut cursor) {
                Ok(message) => return Ok((from, message)),
                Err(e) => {
                    // Authenticated but malformed — a same-secret peer
                    // speaking an incompatible chitchat version.
                    tracing::warn!(%from, error = %e, "dropping undeserializable gossip message");
                }
            }
        }
    }
}

impl EncryptedUdpSocket {
    /// Log a dropped (undecryptable) datagram, rate-limiting `warn!` to
    /// once per [`DROP_WARN_INTERVAL`]; the rest go to debug.
    fn log_drop(&mut self, from: SocketAddr, len: usize) {
        self.drops_since_warn += 1;
        let warn_due = self.last_drop_warn.is_none_or(|t| t.elapsed() >= DROP_WARN_INTERVAL);
        if warn_due {
            tracing::warn!(
                %from,
                payload_len = len,
                dropped = self.drops_since_warn,
                "dropping undecryptable gossip datagram(s) — peer has the wrong cluster \
                 secret or is sending plaintext"
            );
            self.last_drop_warn = Some(Instant::now());
            self.drops_since_warn = 0;
        } else {
            tracing::debug!(%from, payload_len = len, "dropping undecryptable gossip datagram");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip() {
        let cipher = ClusterCipher::for_gossip("test-secret");
        let sealed = cipher.seal(b"hello cluster").unwrap();
        assert_eq!(cipher.open(&sealed).unwrap(), b"hello cluster");
    }

    #[test]
    fn seal_is_randomized() {
        let cipher = ClusterCipher::for_gossip("test-secret");
        let a = cipher.seal(b"same plaintext").unwrap();
        let b = cipher.seal(b"same plaintext").unwrap();
        assert_ne!(a, b, "each seal must use a fresh nonce");
    }

    #[test]
    fn wrong_secret_fails_to_open() {
        let sealed = ClusterCipher::for_gossip("secret-a").seal(b"data").unwrap();
        assert!(ClusterCipher::for_gossip("secret-b").open(&sealed).is_none());
    }

    #[test]
    fn domain_separation_between_planes() {
        // Same secret, different info string → different key.
        let sealed = ClusterCipher::for_gossip("shared").seal(b"data").unwrap();
        assert!(ClusterCipher::for_kv_data_plane("shared").open(&sealed).is_none());
    }

    #[test]
    fn tampered_ciphertext_fails_to_open() {
        let cipher = ClusterCipher::for_gossip("test-secret");
        let mut sealed = cipher.seal(b"data").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0x01;
        assert!(cipher.open(&sealed).is_none());
    }

    #[test]
    fn short_and_empty_inputs_are_rejected() {
        let cipher = ClusterCipher::for_gossip("test-secret");
        assert!(cipher.open(b"").is_none());
        assert!(cipher.open(b"short").is_none());
        assert!(cipher.open(&[0u8; SEAL_OVERHEAD - 1]).is_none());
    }

    #[test]
    fn empty_plaintext_roundtrip() {
        let cipher = ClusterCipher::for_kv_data_plane("test-secret");
        let sealed = cipher.seal(b"").unwrap();
        assert_eq!(sealed.len(), SEAL_OVERHEAD);
        assert_eq!(cipher.open(&sealed).unwrap(), b"");
    }

    #[test]
    fn debug_does_not_leak_key() {
        let cipher = ClusterCipher::for_gossip("super-secret-value");
        let debug = format!("{cipher:?}");
        assert!(!debug.contains("super-secret-value"));
    }

    #[tokio::test]
    async fn encrypted_udp_roundtrip() {
        let transport = EncryptedUdpTransport::new("udp-test-secret");
        let addr_a: SocketAddr = "127.0.0.1:0".parse().unwrap();

        // Bind two sockets on ephemeral ports.
        let sock_a = tokio::net::UdpSocket::bind(addr_a).await.unwrap();
        let recv_addr = sock_a.local_addr().unwrap();
        drop(sock_a); // Release so the transport can bind it.
        let mut receiver = transport.open(recv_addr).await.unwrap();

        let sock_b = tokio::net::UdpSocket::bind(addr_a).await.unwrap();
        let send_addr = sock_b.local_addr().unwrap();
        drop(sock_b);
        let mut sender = transport.open(send_addr).await.unwrap();

        sender.send(recv_addr, ChitchatMessage::BadCluster).await.unwrap();
        let (from, message) = receiver.recv().await.unwrap();
        assert_eq!(from, send_addr);
        assert_eq!(message, ChitchatMessage::BadCluster);
    }

    #[tokio::test]
    async fn plaintext_and_wrong_secret_datagrams_are_dropped() {
        let transport = EncryptedUdpTransport::new("right-secret");
        let tmp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let recv_addr = tmp.local_addr().unwrap();
        drop(tmp);
        let mut receiver = transport.open(recv_addr).await.unwrap();

        let raw = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();

        // 1. Plaintext junk — must be dropped.
        raw.send_to(b"plaintext junk", recv_addr).await.unwrap();

        // 2. A valid chitchat message sealed with the WRONG secret.
        let wrong = ClusterCipher::for_gossip("wrong-secret");
        let msg_bytes = ChitchatMessage::BadCluster.serialize_to_vec();
        let sealed_wrong = wrong.seal(&msg_bytes).unwrap();
        raw.send_to(&sealed_wrong, recv_addr).await.unwrap();

        // 3. Finally a correctly sealed message — the only one received.
        let right = ClusterCipher::for_gossip("right-secret");
        let sealed_right = right.seal(&msg_bytes).unwrap();
        raw.send_to(&sealed_right, recv_addr).await.unwrap();

        let (from, message) = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .expect("receiver should get the valid datagram")
            .unwrap();
        assert_eq!(from, raw.local_addr().unwrap());
        assert_eq!(message, ChitchatMessage::BadCluster);
    }
}
