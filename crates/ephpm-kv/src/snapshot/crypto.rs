//! Snapshot encryption: one key per file, one AEAD seal per block.
//!
//! # Why ChaCha20-Poly1305 + HKDF-SHA256
//!
//! They are the pair `ephpm-cluster`'s secure transport already uses to seal
//! gossip and data-plane frames from the same `[kv] secret`, so the snapshot
//! adds no new primitive and no new crate — and both are pure Rust, which
//! keeps the FFI-free discipline of the KV crate. AES-256-GCM through
//! `aws-lc-rs` would be faster per byte, but a snapshot is bounded by disk
//! and (for object storage) network, not by the cipher; the extra speed would
//! not be visible.
//!
//! # Keys and nonces
//!
//! `file_key = HKDF-SHA256(salt = header salt, ikm = secret, info =
//! "ephpm-kv-snapshot/v1/file-key")`. The salt is 32 fresh random bytes per
//! file, so every snapshot is sealed under its own key even though the
//! secret never changes; the `info` label is distinct from the cluster
//! transport's, so a snapshot key is never a transport key. The raw secret is
//! never written anywhere and never logged.
//!
//! Block nonces are the block's sequence number (`u64`, little-endian, then
//! four zero bytes). With a unique key per file and a counter nonce within
//! it, no (key, nonce) pair is ever reused. The key check uses a nonce no
//! block can have (`0xFF × 12`).
//!
//! Every seal takes the complete header bytes and the sequence number as
//! associated data, so a block cannot be moved, reordered, or paired with an
//! altered header.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;

use super::format::{SALT_LEN, TAG_LEN};

/// HKDF `info` for the per-file key. Distinct from every label in
/// `ephpm-cluster::secure_transport`.
const FILE_KEY_INFO: &[u8] = b"ephpm-kv-snapshot/v1/file-key";

/// The nonce of the header's key check; block nonces always end in four zero
/// bytes, so this can never collide with one.
const KEY_CHECK_NONCE: [u8; 12] = [0xFF; 12];

/// The per-file cipher.
pub struct FileCipher {
    cipher: ChaCha20Poly1305,
}

impl std::fmt::Debug for FileCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FileCipher(..)")
    }
}

impl FileCipher {
    /// Derive the cipher for a file from `[kv] secret` and the file's salt.
    ///
    /// # Panics
    ///
    /// Never in practice: HKDF-SHA256 always yields a 32-byte key.
    #[must_use]
    pub fn derive(secret: &[u8], salt: &[u8; SALT_LEN]) -> Self {
        let hk = Hkdf::<Sha256>::new(Some(salt), secret);
        let mut key = [0u8; 32];
        hk.expand(FILE_KEY_INFO, &mut key).expect("32 bytes is a valid HKDF-SHA256 output length");
        Self { cipher: ChaCha20Poly1305::new(Key::from_slice(&key)) }
    }

    /// The key-check tag for a header prefix: the Poly1305 tag of an empty
    /// message under this key, bound to the prefix.
    ///
    /// # Panics
    ///
    /// Never in practice: sealing an in-memory message cannot fail.
    #[must_use]
    pub fn key_check(&self, header_prefix: &[u8]) -> [u8; TAG_LEN] {
        let out = self
            .cipher
            .encrypt(Nonce::from_slice(&KEY_CHECK_NONCE), Payload { msg: &[], aad: header_prefix })
            .expect("ChaCha20-Poly1305 encryption of an empty message cannot fail");
        let mut tag = [0u8; TAG_LEN];
        tag.copy_from_slice(&out);
        tag
    }

    /// Whether `check` was produced by [`key_check`](Self::key_check) under
    /// this key for `header_prefix` — i.e. whether this is the right secret.
    #[must_use]
    pub fn verify_key_check(&self, header_prefix: &[u8], check: &[u8; TAG_LEN]) -> bool {
        self.cipher
            .decrypt(
                Nonce::from_slice(&KEY_CHECK_NONCE),
                Payload { msg: check, aad: header_prefix },
            )
            .is_ok()
    }

    /// Seal block `seq`. Returns ciphertext followed by the 16-byte tag.
    ///
    /// # Panics
    ///
    /// Never in practice: sealing an in-memory message cannot fail.
    #[must_use]
    pub fn seal_block(&self, header: &[u8], seq: u64, plaintext: &[u8]) -> Vec<u8> {
        let aad = block_aad(header, seq);
        self.cipher
            .encrypt(Nonce::from_slice(&block_nonce(seq)), Payload { msg: plaintext, aad: &aad })
            .expect("ChaCha20-Poly1305 encryption cannot fail for an in-memory message")
    }

    /// Open block `seq`; `None` when the tag does not verify (corruption,
    /// tampering, a moved block, or an altered header).
    #[must_use]
    pub fn open_block(&self, header: &[u8], seq: u64, ciphertext: &[u8]) -> Option<Vec<u8>> {
        if ciphertext.len() < TAG_LEN {
            return None;
        }
        let aad = block_aad(header, seq);
        self.cipher
            .decrypt(Nonce::from_slice(&block_nonce(seq)), Payload { msg: ciphertext, aad: &aad })
            .ok()
    }
}

fn block_nonce(seq: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..8].copy_from_slice(&seq.to_le_bytes());
    n
}

fn block_aad(header: &[u8], seq: u64) -> Vec<u8> {
    let mut aad = Vec::with_capacity(header.len() + 8);
    aad.extend_from_slice(header);
    aad.extend_from_slice(&seq.to_le_bytes());
    aad
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_round_trip_and_every_binding_is_enforced() {
        let c = FileCipher::derive(b"secret", &[1u8; SALT_LEN]);
        let header = b"header-bytes";
        let sealed = c.seal_block(header, 3, b"payload");
        assert_eq!(c.open_block(header, 3, &sealed).as_deref(), Some(&b"payload"[..]));
        // Wrong sequence number, altered header, flipped byte, wrong key.
        assert!(c.open_block(header, 4, &sealed).is_none());
        assert!(c.open_block(b"header-bytez", 3, &sealed).is_none());
        let mut bad = sealed.clone();
        bad[0] ^= 1;
        assert!(c.open_block(header, 3, &bad).is_none());
        let other = FileCipher::derive(b"secret", &[2u8; SALT_LEN]);
        assert!(other.open_block(header, 3, &sealed).is_none());
        assert!(c.open_block(header, 3, &sealed[..3]).is_none());
    }

    #[test]
    fn key_check_distinguishes_secrets_and_binds_the_prefix() {
        let salt = [5u8; SALT_LEN];
        let right = FileCipher::derive(b"one", &salt);
        let wrong = FileCipher::derive(b"two", &salt);
        let check = right.key_check(b"prefix");
        assert!(right.verify_key_check(b"prefix", &check));
        assert!(!wrong.verify_key_check(b"prefix", &check));
        assert!(!right.verify_key_check(b"prefiy", &check));
    }

    #[test]
    fn salt_changes_the_key() {
        let a = FileCipher::derive(b"s", &[0u8; SALT_LEN]);
        let b = FileCipher::derive(b"s", &[1u8; SALT_LEN]);
        let sealed = a.seal_block(b"h", 0, b"x");
        assert!(b.open_block(b"h", 0, &sealed).is_none());
    }
}
