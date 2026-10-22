//! On-disk layout of a KV snapshot — format version 1.
//!
//! Everything here is a pure codec: no I/O, no crypto. The writer and reader
//! modules compose these pieces into files. All integers are little-endian.
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────┐
//! │ HEADER (plaintext, CRC-32; used as AEAD associated data)      │
//! ├──────────────────────────────────────────────────────────────┤
//! │ BLOCK 0 … BLOCK n-1   kind=DATA   (sealed: AEAD per block)    │
//! ├──────────────────────────────────────────────────────────────┤
//! │ BLOCK n               kind=INDEX  (sealed)                    │
//! ├──────────────────────────────────────────────────────────────┤
//! │ TRAILER (plaintext, CRC-32): index offset + length            │
//! └──────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Header
//!
//! | Offset | Size | Field |
//! |-------:|-----:|-------|
//! | 0  | 8 | magic `EPKVSNAP` |
//! | 8  | 2 | format version (`1`) |
//! | 10 | 2 | total header length, CRC included |
//! | 12 | 4 | flags ([`FLAG_ENCRYPTED`], [`FLAG_MULTI_TENANT`]) |
//! | 16 | 8 | created-at, Unix milliseconds |
//! | 24 | 1 | cipher id ([`CIPHER_CHACHA20_POLY1305`]) |
//! | 25 | 1 | block compression id ([`BLOCK_COMPRESSION_LZ4`]) |
//! | 26 | 1 | key-derivation id ([`KDF_HKDF_SHA256`]) |
//! | 27 | 1 | reserved (0) |
//! | 28 | 32 | KDF salt, fresh random per file |
//! | 60 | 1+n | node id (length-prefixed UTF-8, at most 255 bytes) |
//! | … | 16 | key check: AEAD tag of an empty message under the file key |
//! | … | 4 | CRC-32 of every preceding header byte |
//!
//! The header is the **associated data** of every sealed block (together
//! with the block's sequence number), so none of it can be altered without
//! every block failing to open. The key check lets a reader tell "wrong
//! secret" from "corrupt file" before touching any data.
//!
//! # Blocks
//!
//! `kind: u8 | len: u32 | ciphertext[len]`. The ciphertext opens (under
//! nonce = sequence number) to `comp: u8 | raw_len: u32 | payload`, where
//! `payload` is `raw_len` bytes once decompressed. A **data** block's payload
//! is one site's entries — a block never mixes sites, so a site's keys cannot
//! land in another site's store:
//!
//! `site_len: u16 | site | value_compression: u8 | count: u32 | entry…`
//!
//! # Entries
//!
//! `type: u8 | flags: u8 | ext_len: u16 | ext[ext_len] | expires_at_ms: u64 |
//! key_len: u32 | key | payload_len: u32 | payload`
//!
//! `ext` is reserved for future per-entry metadata (an LRU hint, a cluster
//! stamp); a version-1 reader skips it. `payload_len` lets a reader skip an
//! entry type it does not know. `expires_at_ms = 0` means no expiry.
//!
//! * [`ENTRY_STRING`]: payload = `value` (the bytes as stored; flag
//!   [`ENTRY_FLAG_VALUE_COMPRESSED`] says they are compressed with the block's
//!   `value_compression` algorithm).
//! * [`ENTRY_HASH`]: payload = `count: u32 | field…` with each field
//!   `[expires_at_ms: u64 if ENTRY_FLAG_FIELD_EXPIRY] | name_len: u32 | name |
//!   value_len: u32 | value`.
//!
//! # Index and trailer
//!
//! The index block (`count: u32`, then per data block `offset: u64 | len: u32
//! | site_len: u16 | site | entries: u32 | plain_bytes: u64`) is what lets a
//! loader verify and apply blocks in parallel. The trailer, the last
//! [`TRAILER_LEN`] bytes of the file, is `index_offset: u64 | index_len: u32 |
//! crc32: u32 | magic EPKVEND\0`. A file whose trailer is missing or whose CRC
//! fails was truncated: it never loads, partially or otherwise.

use bytes::Bytes;

use crate::store::CompressionAlgo;

/// Leading magic of a snapshot file.
pub const MAGIC: [u8; 8] = *b"EPKVSNAP";
/// Trailing magic (last 8 bytes of a complete file).
pub const END_MAGIC: [u8; 8] = *b"EPKVEND\0";
/// The one format version this binary writes and reads.
pub const FORMAT_VERSION: u16 = 1;

/// Header flag: the blocks are AEAD-sealed (always set in version 1).
pub const FLAG_ENCRYPTED: u32 = 1;
/// Header flag: the writer ran with per-site stores (`sites_dir`).
pub const FLAG_MULTI_TENANT: u32 = 1 << 1;

/// Cipher id: ChaCha20-Poly1305, 12-byte nonce = block sequence number.
pub const CIPHER_CHACHA20_POLY1305: u8 = 1;
/// KDF id: HKDF-SHA256 over `[kv] secret` with the header salt.
pub const KDF_HKDF_SHA256: u8 = 1;
/// Block compression id: none.
pub const BLOCK_COMPRESSION_NONE: u8 = 0;
/// Block compression id: LZ4 block format (`lz4_flex`), raw length carried
/// separately.
pub const BLOCK_COMPRESSION_LZ4: u8 = 1;

/// Block kind: one site's entries.
pub const BLOCK_KIND_DATA: u8 = 1;
/// Block kind: the index of every data block.
pub const BLOCK_KIND_INDEX: u8 = 2;
/// Bytes of a block's on-disk prefix (`kind` + `len`).
pub const BLOCK_PREFIX_LEN: usize = 5;

/// Entry type: a string value.
pub const ENTRY_STRING: u8 = 1;
/// Entry type: a hash (field → value map).
pub const ENTRY_HASH: u8 = 2;
/// Entry flag: the string value is stored compressed (algorithm = the block's
/// `value_compression`).
pub const ENTRY_FLAG_VALUE_COMPRESSED: u8 = 1;
/// Entry flag: every field of the hash carries its own `expires_at_ms`.
pub const ENTRY_FLAG_FIELD_EXPIRY: u8 = 1 << 1;

/// Bytes of the per-file KDF salt.
pub const SALT_LEN: usize = 32;
/// Bytes of a Poly1305 tag — the key check and every block's tag.
pub const TAG_LEN: usize = 16;
/// Bytes of the trailer.
pub const TRAILER_LEN: usize = 24;
/// Header bytes before the variable-length node id.
pub const HEADER_FIXED_LEN: usize = 60;
/// Largest header the format allows (255-byte node id).
pub const MAX_HEADER_LEN: usize = HEADER_FIXED_LEN + 1 + 255 + TAG_LEN + 4;
/// Smallest header the format allows (empty node id).
pub const MIN_HEADER_LEN: usize = HEADER_FIXED_LEN + 1 + TAG_LEN + 4;

/// Value-compression id stored per data block: none.
pub const VALUE_COMPRESSION_NONE: u8 = 0;
/// Value-compression id: gzip.
pub const VALUE_COMPRESSION_GZIP: u8 = 1;
/// Value-compression id: brotli.
pub const VALUE_COMPRESSION_BROTLI: u8 = 2;
/// Value-compression id: zstd.
pub const VALUE_COMPRESSION_ZSTD: u8 = 3;

/// A decoding failure: the bytes are not a well-formed snapshot structure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FormatError {
    /// The input ended before the structure did.
    #[error("truncated: needed {needed} more bytes at offset {at}")]
    Truncated {
        /// Offset where the read started.
        at: usize,
        /// Bytes the read needed.
        needed: usize,
    },
    /// A field holds a value the format does not allow.
    #[error("malformed: {0}")]
    Malformed(&'static str),
    /// The file is not a snapshot at all (wrong magic).
    #[error("not a snapshot file (bad magic)")]
    BadMagic,
    /// The format version is one this binary does not read.
    #[error("unsupported snapshot format version {0}")]
    UnsupportedVersion(u16),
    /// A plaintext CRC-32 did not match.
    #[error("checksum mismatch in the {0}")]
    Checksum(&'static str),
}

/// Map the store's value compression to its on-disk id.
#[must_use]
pub fn value_compression_id(algo: CompressionAlgo) -> u8 {
    match algo {
        CompressionAlgo::None => VALUE_COMPRESSION_NONE,
        CompressionAlgo::Gzip => VALUE_COMPRESSION_GZIP,
        CompressionAlgo::Brotli => VALUE_COMPRESSION_BROTLI,
        CompressionAlgo::Zstd => VALUE_COMPRESSION_ZSTD,
    }
}

/// Map an on-disk value-compression id back to the store's algorithm.
#[must_use]
pub fn value_compression_algo(id: u8) -> Option<CompressionAlgo> {
    match id {
        VALUE_COMPRESSION_NONE => Some(CompressionAlgo::None),
        VALUE_COMPRESSION_GZIP => Some(CompressionAlgo::Gzip),
        VALUE_COMPRESSION_BROTLI => Some(CompressionAlgo::Brotli),
        VALUE_COMPRESSION_ZSTD => Some(CompressionAlgo::Zstd),
        _ => None,
    }
}

// ── Byte helpers ─────────────────────────────────────────────────────────────

/// Append little-endian integers and length-prefixed byte strings.
pub(crate) trait PutLe {
    fn put_u8(&mut self, v: u8);
    fn put_u16(&mut self, v: u16);
    fn put_u32(&mut self, v: u32);
    fn put_u64(&mut self, v: u64);
}

impl PutLe for Vec<u8> {
    fn put_u8(&mut self, v: u8) {
        self.push(v);
    }
    fn put_u16(&mut self, v: u16) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    fn put_u32(&mut self, v: u32) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    fn put_u64(&mut self, v: u64) {
        self.extend_from_slice(&v.to_le_bytes());
    }
}

/// A bounds-checked cursor over a byte slice: every read either returns the
/// bytes or a [`FormatError::Truncated`], never panics.
pub struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    /// Start at the beginning of `buf`.
    #[must_use]
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes not yet consumed.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], FormatError> {
        if self.remaining() < n {
            return Err(FormatError::Truncated { at: self.pos, needed: n - self.remaining() });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, FormatError> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, FormatError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, FormatError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, FormatError> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    /// A `u32`-length-prefixed byte string.
    pub(crate) fn bytes32(&mut self) -> Result<&'a [u8], FormatError> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    /// A `u16`-length-prefixed UTF-8 string.
    pub(crate) fn str16(&mut self) -> Result<&'a str, FormatError> {
        let n = self.u16()? as usize;
        std::str::from_utf8(self.take(n)?).map_err(|_| FormatError::Malformed("non-UTF-8 string"))
    }
}

/// CRC-32 (IEEE) of `bytes`, as every plaintext checksum in the format.
#[must_use]
pub fn crc32(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
}

// ── Header ───────────────────────────────────────────────────────────────────

/// The plaintext file header. See the module docs for the layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// Format version — always [`FORMAT_VERSION`] for a header this binary
    /// decodes successfully.
    pub version: u16,
    /// [`FLAG_ENCRYPTED`] | [`FLAG_MULTI_TENANT`].
    pub flags: u32,
    /// When the snapshot was written, Unix milliseconds.
    pub created_at_ms: u64,
    /// Cipher id.
    pub cipher: u8,
    /// Block compression id.
    pub block_compression: u8,
    /// Key-derivation id.
    pub kdf: u8,
    /// Per-file KDF salt.
    pub salt: [u8; SALT_LEN],
    /// Id of the node that wrote the file (informational).
    pub node_id: String,
}

/// A decoded header plus the pieces a reader needs to verify it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedHeader {
    /// The header fields.
    pub header: Header,
    /// The key check tag stored in the header.
    pub key_check: [u8; TAG_LEN],
    /// The exact header bytes (CRC included): the AEAD associated data.
    pub bytes: Vec<u8>,
}

impl Header {
    /// Serialise every field up to (not including) the key check: the bytes
    /// the key check authenticates.
    ///
    /// # Panics
    ///
    /// If `node_id` is longer than 255 bytes (the writer truncates it first).
    #[must_use]
    pub fn encode_prefix(&self) -> Vec<u8> {
        let node_id = self.node_id.as_bytes();
        debug_assert!(node_id.len() <= 255, "node id is clamped before encoding");
        let total = HEADER_FIXED_LEN + 1 + node_id.len() + TAG_LEN + 4;
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&MAGIC);
        out.put_u16(self.version);
        out.put_u16(u16::try_from(total).expect("header length fits u16"));
        out.put_u32(self.flags);
        out.put_u64(self.created_at_ms);
        out.put_u8(self.cipher);
        out.put_u8(self.block_compression);
        out.put_u8(self.kdf);
        out.put_u8(0);
        out.extend_from_slice(&self.salt);
        out.put_u8(u8::try_from(node_id.len()).expect("node id clamped to 255 bytes"));
        out.extend_from_slice(node_id);
        out
    }

    /// The complete header bytes: prefix, key check, CRC-32.
    #[must_use]
    pub fn encode(&self, key_check: &[u8; TAG_LEN]) -> Vec<u8> {
        let mut out = self.encode_prefix();
        out.extend_from_slice(key_check);
        let crc = crc32(&out);
        out.put_u32(crc);
        out
    }

    /// Total header length declared by the first [`HEADER_FIXED_LEN`] bytes of
    /// a file, after checking the magic and version.
    ///
    /// # Errors
    ///
    /// [`FormatError::BadMagic`], [`FormatError::UnsupportedVersion`], or a
    /// malformed length.
    pub fn declared_len(prefix: &[u8]) -> Result<usize, FormatError> {
        if prefix.len() < 12 {
            return Err(FormatError::Truncated { at: 0, needed: 12 - prefix.len() });
        }
        if prefix[..8] != MAGIC {
            return Err(FormatError::BadMagic);
        }
        let version = u16::from_le_bytes([prefix[8], prefix[9]]);
        if version != FORMAT_VERSION {
            return Err(FormatError::UnsupportedVersion(version));
        }
        let len = u16::from_le_bytes([prefix[10], prefix[11]]) as usize;
        if !(MIN_HEADER_LEN..=MAX_HEADER_LEN).contains(&len) {
            return Err(FormatError::Malformed("header length out of range"));
        }
        Ok(len)
    }

    /// Decode and CRC-check a complete header (`bytes.len()` must equal the
    /// declared length).
    ///
    /// # Errors
    ///
    /// Any [`FormatError`]; a CRC mismatch is [`FormatError::Checksum`].
    pub fn decode(bytes: &[u8]) -> Result<DecodedHeader, FormatError> {
        let len = Self::declared_len(bytes)?;
        if bytes.len() != len {
            return Err(FormatError::Malformed("header length does not match its declaration"));
        }
        let (body, crc_bytes) = bytes.split_at(len - 4);
        let stored = u32::from_le_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
        if crc32(body) != stored {
            return Err(FormatError::Checksum("header"));
        }
        let mut c = Cursor::new(body);
        c.take(8)?; // magic, checked above
        let version = c.u16()?;
        c.u16()?; // length, checked above
        let flags = c.u32()?;
        let created_at_ms = c.u64()?;
        let cipher = c.u8()?;
        let block_compression = c.u8()?;
        let kdf = c.u8()?;
        c.u8()?; // reserved
        let mut salt = [0u8; SALT_LEN];
        salt.copy_from_slice(c.take(SALT_LEN)?);
        let node_len = c.u8()? as usize;
        let node_id = std::str::from_utf8(c.take(node_len)?)
            .map_err(|_| FormatError::Malformed("node id is not UTF-8"))?
            .to_string();
        let mut key_check = [0u8; TAG_LEN];
        key_check.copy_from_slice(c.take(TAG_LEN)?);
        if c.remaining() != 0 {
            return Err(FormatError::Malformed("trailing bytes in header"));
        }
        if flags & FLAG_ENCRYPTED == 0 {
            return Err(FormatError::Malformed("unencrypted snapshots are not supported"));
        }
        if cipher != CIPHER_CHACHA20_POLY1305 {
            return Err(FormatError::Malformed("unknown cipher id"));
        }
        if kdf != KDF_HKDF_SHA256 {
            return Err(FormatError::Malformed("unknown key-derivation id"));
        }
        if block_compression > BLOCK_COMPRESSION_LZ4 {
            return Err(FormatError::Malformed("unknown block compression id"));
        }
        Ok(DecodedHeader {
            header: Header {
                version,
                flags,
                created_at_ms,
                cipher,
                block_compression,
                kdf,
                salt,
                node_id,
            },
            key_check,
            bytes: bytes.to_vec(),
        })
    }
}

// ── Trailer ──────────────────────────────────────────────────────────────────

/// The plaintext trailer: where the index block is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trailer {
    /// File offset of the index block's `kind` byte.
    pub index_offset: u64,
    /// Length of the index block's ciphertext.
    pub index_len: u32,
}

impl Trailer {
    /// Serialise to exactly [`TRAILER_LEN`] bytes.
    #[must_use]
    pub fn encode(&self) -> [u8; TRAILER_LEN] {
        let mut out = Vec::with_capacity(TRAILER_LEN);
        out.put_u64(self.index_offset);
        out.put_u32(self.index_len);
        let crc = crc32(&out);
        out.put_u32(crc);
        out.extend_from_slice(&END_MAGIC);
        let mut arr = [0u8; TRAILER_LEN];
        arr.copy_from_slice(&out);
        arr
    }

    /// Decode the last [`TRAILER_LEN`] bytes of a file.
    ///
    /// # Errors
    ///
    /// Bad magic or CRC means the file is incomplete or not a snapshot.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        if bytes.len() != TRAILER_LEN {
            return Err(FormatError::Malformed("trailer length"));
        }
        if bytes[16..] != END_MAGIC {
            return Err(FormatError::Malformed("file does not end with the snapshot trailer"));
        }
        let stored = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        if crc32(&bytes[..12]) != stored {
            return Err(FormatError::Checksum("trailer"));
        }
        let mut c = Cursor::new(&bytes[..12]);
        Ok(Self { index_offset: c.u64()?, index_len: c.u32()? })
    }
}

// ── Sealed block plaintext ───────────────────────────────────────────────────

/// Wrap a block payload for sealing: compress with LZ4 when that makes it
/// smaller, prefixing the compression id and the raw length.
///
/// # Panics
///
/// If `payload` is 4 GiB or more (the writer never builds such a block).
#[must_use]
pub fn pack_block(payload: &[u8], compression: u8) -> Vec<u8> {
    let raw_len = u32::try_from(payload.len()).expect("block payload fits u32");
    if compression == BLOCK_COMPRESSION_LZ4 && payload.len() >= 64 {
        let compressed = lz4_flex::block::compress(payload);
        if compressed.len() < payload.len() {
            let mut out = Vec::with_capacity(5 + compressed.len());
            out.put_u8(BLOCK_COMPRESSION_LZ4);
            out.put_u32(raw_len);
            out.extend_from_slice(&compressed);
            return out;
        }
    }
    let mut out = Vec::with_capacity(5 + payload.len());
    out.put_u8(BLOCK_COMPRESSION_NONE);
    out.put_u32(raw_len);
    out.extend_from_slice(payload);
    out
}

/// Undo [`pack_block`]: the raw payload of an opened block.
///
/// # Errors
///
/// Unknown compression id, a raw length that does not match, or a payload
/// larger than `max_raw` (a decompression bomb guard).
pub fn unpack_block(opened: &[u8], max_raw: usize) -> Result<Vec<u8>, FormatError> {
    let mut c = Cursor::new(opened);
    let comp = c.u8()?;
    let raw_len = c.u32()? as usize;
    if raw_len > max_raw {
        return Err(FormatError::Malformed("block raw length exceeds the limit"));
    }
    let body = c.take(c.remaining())?;
    match comp {
        BLOCK_COMPRESSION_NONE => {
            if body.len() != raw_len {
                return Err(FormatError::Malformed("raw block length mismatch"));
            }
            Ok(body.to_vec())
        }
        BLOCK_COMPRESSION_LZ4 => {
            let out = lz4_flex::block::decompress(body, raw_len)
                .map_err(|_| FormatError::Malformed("LZ4 block does not decompress"))?;
            if out.len() != raw_len {
                return Err(FormatError::Malformed("LZ4 raw length mismatch"));
            }
            Ok(out)
        }
        _ => Err(FormatError::Malformed("unknown block compression id")),
    }
}

// ── Entries ──────────────────────────────────────────────────────────────────

/// One field of a hash entry as it appears in a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldRecord {
    /// Field name.
    pub name: String,
    /// Field value.
    pub value: Bytes,
    /// The field's own expiry, Unix milliseconds; `0` = none.
    pub expires_at_ms: u64,
}

/// One entry as it appears in a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryRecord {
    /// A string value.
    String {
        /// Key.
        key: String,
        /// Value bytes as stored (compressed when `compressed`).
        data: Bytes,
        /// Whether `data` is compressed with the block's value algorithm.
        compressed: bool,
        /// Expiry, Unix milliseconds; `0` = none.
        expires_at_ms: u64,
    },
    /// A hash.
    Hash {
        /// Key.
        key: String,
        /// Fields.
        fields: Vec<FieldRecord>,
        /// The hash's expiry, Unix milliseconds; `0` = none.
        expires_at_ms: u64,
    },
}

impl EntryRecord {
    /// The entry's key.
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Self::String { key, .. } | Self::Hash { key, .. } => key,
        }
    }
}

/// Serialise one entry onto `out`.
///
/// # Panics
///
/// If a key, value, field or payload is 4 GiB or more; the writer skips such
/// entries before encoding (see [`entry_fits`]).
pub fn encode_entry(out: &mut Vec<u8>, entry: &EntryRecord) {
    match entry {
        EntryRecord::String { key, data, compressed, expires_at_ms } => {
            out.put_u8(ENTRY_STRING);
            out.put_u8(if *compressed { ENTRY_FLAG_VALUE_COMPRESSED } else { 0 });
            out.put_u16(0);
            out.put_u64(*expires_at_ms);
            out.put_u32(u32::try_from(key.len()).expect("key fits u32"));
            out.extend_from_slice(key.as_bytes());
            out.put_u32(u32::try_from(data.len()).expect("value fits u32"));
            out.extend_from_slice(data);
        }
        EntryRecord::Hash { key, fields, expires_at_ms } => {
            let field_expiry = fields.iter().any(|f| f.expires_at_ms != 0);
            out.put_u8(ENTRY_HASH);
            out.put_u8(if field_expiry { ENTRY_FLAG_FIELD_EXPIRY } else { 0 });
            out.put_u16(0);
            out.put_u64(*expires_at_ms);
            out.put_u32(u32::try_from(key.len()).expect("key fits u32"));
            out.extend_from_slice(key.as_bytes());
            let payload_len: usize = 4 + fields
                .iter()
                .map(|f| usize::from(field_expiry) * 8 + 4 + f.name.len() + 4 + f.value.len())
                .sum::<usize>();
            out.put_u32(u32::try_from(payload_len).expect("hash payload fits u32"));
            out.put_u32(u32::try_from(fields.len()).expect("field count fits u32"));
            for f in fields {
                if field_expiry {
                    out.put_u64(f.expires_at_ms);
                }
                out.put_u32(u32::try_from(f.name.len()).expect("field name fits u32"));
                out.extend_from_slice(f.name.as_bytes());
                out.put_u32(u32::try_from(f.value.len()).expect("field value fits u32"));
                out.extend_from_slice(&f.value);
            }
        }
    }
}

/// Whether every length in `entry` fits the format's `u32` length fields.
#[must_use]
pub fn entry_fits(entry: &EntryRecord) -> bool {
    const LIMIT: usize = u32::MAX as usize;
    match entry {
        EntryRecord::String { key, data, .. } => key.len() <= LIMIT && data.len() <= LIMIT,
        EntryRecord::Hash { key, fields, .. } => {
            key.len() <= LIMIT
                && fields.len() <= LIMIT
                && fields
                    .iter()
                    .try_fold(4usize, |acc, f| {
                        (f.name.len() <= LIMIT && f.value.len() <= LIMIT)
                            .then(|| acc.checked_add(8 + 4 + f.name.len() + 4 + f.value.len()))
                            .flatten()
                    })
                    .is_some_and(|total| total <= LIMIT)
        }
    }
}

/// Decode one entry from the cursor. `Ok(None)` means the entry is of a type
/// this reader does not know and was skipped.
///
/// # Errors
///
/// A truncated or malformed entry.
pub fn decode_entry(c: &mut Cursor<'_>) -> Result<Option<EntryRecord>, FormatError> {
    let kind = c.u8()?;
    let flags = c.u8()?;
    let ext_len = c.u16()? as usize;
    c.take(ext_len)?;
    let expires_at_ms = c.u64()?;
    let key = std::str::from_utf8(c.bytes32()?)
        .map_err(|_| FormatError::Malformed("key is not UTF-8"))?
        .to_string();
    let payload = c.bytes32()?;
    match kind {
        ENTRY_STRING => Ok(Some(EntryRecord::String {
            key,
            data: Bytes::copy_from_slice(payload),
            compressed: flags & ENTRY_FLAG_VALUE_COMPRESSED != 0,
            expires_at_ms,
        })),
        ENTRY_HASH => {
            let mut p = Cursor::new(payload);
            let count = p.u32()? as usize;
            let mut fields = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                let field_expiry = if flags & ENTRY_FLAG_FIELD_EXPIRY != 0 { p.u64()? } else { 0 };
                let name = std::str::from_utf8(p.bytes32()?)
                    .map_err(|_| FormatError::Malformed("field name is not UTF-8"))?
                    .to_string();
                let value = Bytes::copy_from_slice(p.bytes32()?);
                fields.push(FieldRecord { name, value, expires_at_ms: field_expiry });
            }
            if p.remaining() != 0 {
                return Err(FormatError::Malformed("trailing bytes in hash payload"));
            }
            Ok(Some(EntryRecord::Hash { key, fields, expires_at_ms }))
        }
        _ => Ok(None),
    }
}

// ── Data block body ──────────────────────────────────────────────────────────

/// The per-block prefix before the entries: site, value compression, count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataBlockHead {
    /// Site key (`""` = the default store).
    pub site: String,
    /// Value-compression id of this block's entries.
    pub value_compression: u8,
    /// Number of entries that follow.
    pub count: u32,
}

impl DataBlockHead {
    /// Serialise onto `out`.
    ///
    /// # Panics
    ///
    /// If `site` is longer than 64 KiB (site keys are hostnames).
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.put_u16(u16::try_from(self.site.len()).expect("site key fits u16"));
        out.extend_from_slice(self.site.as_bytes());
        out.put_u8(self.value_compression);
        out.put_u32(self.count);
    }

    /// Decode from the cursor, leaving it at the first entry.
    ///
    /// # Errors
    ///
    /// A truncated or malformed head.
    pub fn decode(c: &mut Cursor<'_>) -> Result<Self, FormatError> {
        let site = c.str16()?.to_string();
        let value_compression = c.u8()?;
        if value_compression_algo(value_compression).is_none() {
            return Err(FormatError::Malformed("unknown value compression id"));
        }
        let count = c.u32()?;
        Ok(Self { site, value_compression, count })
    }
}

// ── Index ────────────────────────────────────────────────────────────────────

/// One data block as described by the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    /// File offset of the block's `kind` byte.
    pub offset: u64,
    /// Length of the block's ciphertext.
    pub len: u32,
    /// Site key of every entry in the block.
    pub site: String,
    /// Number of entries in the block.
    pub entries: u32,
    /// Plaintext payload bytes of the block (before compression).
    pub plain_bytes: u64,
}

/// Serialise the index block payload.
///
/// # Panics
///
/// If there are 2^32 or more blocks, or a site key is longer than 64 KiB.
#[must_use]
pub fn encode_index(entries: &[IndexEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + entries.len() * 40);
    out.put_u32(u32::try_from(entries.len()).expect("block count fits u32"));
    for e in entries {
        out.put_u64(e.offset);
        out.put_u32(e.len);
        out.put_u16(u16::try_from(e.site.len()).expect("site key fits u16"));
        out.extend_from_slice(e.site.as_bytes());
        out.put_u32(e.entries);
        out.put_u64(e.plain_bytes);
    }
    out
}

/// Decode the index block payload.
///
/// # Errors
///
/// A truncated or malformed index.
pub fn decode_index(payload: &[u8]) -> Result<Vec<IndexEntry>, FormatError> {
    let mut c = Cursor::new(payload);
    let count = c.u32()? as usize;
    let mut out = Vec::with_capacity(count.min(1 << 16));
    for _ in 0..count {
        let offset = c.u64()?;
        let len = c.u32()?;
        let site = c.str16()?.to_string();
        let entries = c.u32()?;
        let plain_bytes = c.u64()?;
        out.push(IndexEntry { offset, len, site, entries, plain_bytes });
    }
    if c.remaining() != 0 {
        return Err(FormatError::Malformed("trailing bytes in index"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Header {
        Header {
            version: FORMAT_VERSION,
            flags: FLAG_ENCRYPTED | FLAG_MULTI_TENANT,
            created_at_ms: 1_700_000_000_000,
            cipher: CIPHER_CHACHA20_POLY1305,
            block_compression: BLOCK_COMPRESSION_LZ4,
            kdf: KDF_HKDF_SHA256,
            salt: [7u8; SALT_LEN],
            node_id: "node-1".into(),
        }
    }

    #[test]
    fn header_round_trips_and_detects_a_flipped_bit() {
        let h = header();
        let bytes = h.encode(&[9u8; TAG_LEN]);
        assert_eq!(Header::declared_len(&bytes[..12]).unwrap(), bytes.len());
        let d = Header::decode(&bytes).unwrap();
        assert_eq!(d.header, h);
        assert_eq!(d.key_check, [9u8; TAG_LEN]);
        assert_eq!(d.bytes, bytes);

        let mut bad = bytes.clone();
        bad[20] ^= 1;
        assert_eq!(Header::decode(&bad), Err(FormatError::Checksum("header")));
    }

    #[test]
    fn header_rejects_wrong_magic_and_future_versions() {
        let mut bytes = header().encode(&[0u8; TAG_LEN]);
        bytes[0] = b'X';
        assert_eq!(Header::declared_len(&bytes), Err(FormatError::BadMagic));
        let mut bytes = header().encode(&[0u8; TAG_LEN]);
        bytes[8] = 2;
        assert_eq!(Header::declared_len(&bytes), Err(FormatError::UnsupportedVersion(2)));
    }

    #[test]
    fn trailer_round_trips_and_detects_corruption() {
        let t = Trailer { index_offset: 123_456, index_len: 789 };
        let bytes = t.encode();
        assert_eq!(Trailer::decode(&bytes).unwrap(), t);
        let mut bad = bytes;
        bad[3] ^= 0x80;
        assert_eq!(Trailer::decode(&bad), Err(FormatError::Checksum("trailer")));
        let mut cut = bytes;
        cut[16] = b'X';
        assert!(matches!(Trailer::decode(&cut), Err(FormatError::Malformed(_))));
    }

    #[test]
    fn entries_round_trip_including_per_field_expiry() {
        let entries = vec![
            EntryRecord::String {
                key: "a".into(),
                data: Bytes::from_static(b"hello"),
                compressed: true,
                expires_at_ms: 42,
            },
            EntryRecord::Hash {
                key: "h".into(),
                fields: vec![
                    FieldRecord {
                        name: "f1".into(),
                        value: Bytes::from_static(b"v1"),
                        expires_at_ms: 0,
                    },
                    FieldRecord {
                        name: "f2".into(),
                        value: Bytes::from_static(b""),
                        expires_at_ms: 99,
                    },
                ],
                expires_at_ms: 0,
            },
            EntryRecord::Hash { key: "plain".into(), fields: vec![], expires_at_ms: 5 },
        ];
        let mut buf = Vec::new();
        for e in &entries {
            encode_entry(&mut buf, e);
        }
        let mut c = Cursor::new(&buf);
        let mut got = Vec::new();
        while c.remaining() > 0 {
            got.push(decode_entry(&mut c).unwrap().unwrap());
        }
        assert_eq!(got, entries);
    }

    #[test]
    fn unknown_entry_types_are_skipped_by_payload_length() {
        let mut buf = Vec::new();
        buf.put_u8(200); // unknown type
        buf.put_u8(0);
        buf.put_u16(3);
        buf.extend_from_slice(b"ext");
        buf.put_u64(0);
        buf.put_u32(1);
        buf.push(b'k');
        buf.put_u32(4);
        buf.extend_from_slice(b"junk");
        encode_entry(
            &mut buf,
            &EntryRecord::String {
                key: "after".into(),
                data: Bytes::from_static(b"x"),
                compressed: false,
                expires_at_ms: 0,
            },
        );
        let mut c = Cursor::new(&buf);
        assert!(decode_entry(&mut c).unwrap().is_none());
        assert_eq!(decode_entry(&mut c).unwrap().unwrap().key(), "after");
        assert_eq!(c.remaining(), 0);
    }

    #[test]
    fn truncated_entry_is_an_error_not_a_panic() {
        let mut buf = Vec::new();
        encode_entry(
            &mut buf,
            &EntryRecord::String {
                key: "k".into(),
                data: Bytes::from_static(b"value"),
                compressed: false,
                expires_at_ms: 0,
            },
        );
        for cut in 0..buf.len() {
            let mut c = Cursor::new(&buf[..cut]);
            assert!(
                matches!(decode_entry(&mut c), Err(FormatError::Truncated { .. })),
                "cut {cut}"
            );
        }
    }

    #[test]
    fn block_packing_compresses_when_it_helps_and_bounds_the_raw_length() {
        let payload = vec![b'a'; 4096];
        let packed = pack_block(&payload, BLOCK_COMPRESSION_LZ4);
        assert_eq!(packed[0], BLOCK_COMPRESSION_LZ4);
        assert!(packed.len() < payload.len());
        assert_eq!(unpack_block(&packed, 1 << 20).unwrap(), payload);
        assert!(matches!(unpack_block(&packed, 100), Err(FormatError::Malformed(_))));

        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let random: Vec<u8> = (0..300)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect();
        let packed = pack_block(&random, BLOCK_COMPRESSION_LZ4);
        assert_eq!(packed[0], BLOCK_COMPRESSION_NONE);
        assert_eq!(unpack_block(&packed, 1 << 20).unwrap(), random);
    }

    #[test]
    fn index_and_block_head_round_trip() {
        let idx = vec![
            IndexEntry { offset: 100, len: 50, site: String::new(), entries: 3, plain_bytes: 400 },
            IndexEntry {
                offset: 155,
                len: 60,
                site: "alice.test".into(),
                entries: 1,
                plain_bytes: 9,
            },
        ];
        assert_eq!(decode_index(&encode_index(&idx)).unwrap(), idx);
        let head =
            DataBlockHead { site: "s".into(), value_compression: VALUE_COMPRESSION_ZSTD, count: 7 };
        let mut buf = Vec::new();
        head.encode(&mut buf);
        assert_eq!(DataBlockHead::decode(&mut Cursor::new(&buf)).unwrap(), head);
    }
}
