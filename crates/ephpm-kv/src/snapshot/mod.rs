//! Warm-restart snapshots: write the live store to an encrypted file at
//! shutdown (or on demand), load it at the next start.
//!
//! A snapshot is a **cache warm-up, not durability**. It is written on a
//! best-effort basis, it is not atomic across keys (each shard is copied at
//! a slightly different moment), keys written after it was taken are not in
//! it, and a node that loads one may briefly serve values a peer has since
//! overwritten or deleted. What it guarantees is narrower and exact: a file
//! that loads is byte-for-byte what this node's stores held when it was
//! written, every entry lands in the store of the site it came from and no
//! other, entries past their expiry are never restored, and a file that was
//! truncated, altered, written by a newer format, or sealed under a different
//! secret loads **nothing** rather than something wrong.
//!
//! * [`format`] — the on-disk layout and its codec.
//! * [`crypto`] — per-file HKDF key, per-block ChaCha20-Poly1305.
//! * [`writer`] — streaming, bounded-memory, atomic-replace writer.
//! * [`reader`] — parallel loader, `inspect`, `verify`.
//!
//! Everything in here is synchronous and blocking by design: the server
//! runs it on a blocking thread, never on an async worker.

pub mod crypto;
pub mod format;
pub mod reader;
pub mod writer;

use std::time::{SystemTime, UNIX_EPOCH};

pub use format::{FORMAT_VERSION, Header};
pub use reader::{
    ColdReason, Inspection, LoadOptions, LoadOutcome, LoadReport, PartialReason, SiteResolver,
    SiteSummary, VerifyReport, inspect, load_snapshot, verify,
};
pub use writer::{DEFAULT_BLOCK_TARGET, SaveReport, WriteOptions, write_snapshot};

/// Why a snapshot could not be written or read.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// The filesystem said no.
    #[error("snapshot I/O: {0}")]
    Io(#[from] std::io::Error),
    /// The file is (or would be) larger than the configured limit.
    #[error("snapshot is {size} bytes, over the {max}-byte limit")]
    TooLarge {
        /// Actual or projected size.
        size: u64,
        /// The limit.
        max: u64,
    },
    /// The bytes are not a well-formed snapshot (truncated, damaged, or not
    /// a snapshot at all).
    #[error("snapshot is not readable: {0}")]
    Corrupt(String),
    /// Written by a newer format than this binary reads.
    #[error("snapshot format version {0} is newer than this binary supports (v{FORMAT_VERSION})")]
    UnsupportedVersion(u16),
    /// Sealed under a different `[kv] secret`.
    #[error("snapshot was sealed with a different [kv] secret")]
    WrongKey,
}

/// `t` as Unix milliseconds (0 for a time before the epoch).
#[must_use]
pub fn unix_ms_for(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use bytes::Bytes;
    use proptest::prelude::*;

    use super::*;
    use crate::store::{CompressionAlgo, CompressionConfig, EvictionPolicy, Store, StoreConfig};

    const SECRET: &[u8] = b"test-secret";

    fn store(algo: CompressionAlgo) -> Arc<Store> {
        Store::new(StoreConfig {
            memory_limit: 0,
            eviction_policy: EvictionPolicy::NoEviction,
            compression: CompressionConfig { algo, level: 3, min_size: 64 },
        })
    }

    fn opts() -> WriteOptions {
        WriteOptions { node_id: "test-node".into(), sealers: 2, ..WriteOptions::default() }
    }

    /// `n` incompressible bytes (xorshift), so size assertions are not
    /// defeated by LZ4.
    fn noise(seed: u64, n: usize) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    fn resolver(stores: Vec<(String, Arc<Store>)>) -> impl Fn(&str) -> Option<Arc<Store>> + Sync {
        move |site: &str| stores.iter().find(|(s, _)| s == site).map(|(_, st)| Arc::clone(st))
    }

    #[test]
    fn round_trip_strings_hashes_ttls_and_sites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.snap");
        let default = store(CompressionAlgo::None);
        let alice = store(CompressionAlgo::None);
        default.set("k1".into(), b"v1".to_vec(), None);
        default.set("ttl".into(), b"soon".to_vec(), Some(Duration::from_secs(60)));
        default.set("gone".into(), b"x".to_vec(), Some(Duration::from_millis(1)));
        default.hset("h", "f1", b"a".to_vec());
        default.hset("h", "f2", b"b".to_vec());
        alice.set("k1".into(), b"alice".to_vec(), None);
        std::thread::sleep(Duration::from_millis(5));

        let stores =
            vec![(String::new(), Arc::clone(&default)), ("alice".into(), Arc::clone(&alice))];
        let report = write_snapshot(&path, SECRET, &stores, &opts()).unwrap();
        assert_eq!(report.strings, 3, "the already-expired key is not written");
        assert_eq!(report.hashes, 1);
        assert_eq!(report.sites, 2);
        assert!(report.bytes > 0);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), report.bytes);

        let d2 = store(CompressionAlgo::None);
        let a2 = store(CompressionAlgo::None);
        let res =
            resolver(vec![(String::new(), Arc::clone(&d2)), ("alice".into(), Arc::clone(&a2))]);
        let load = load_snapshot(&path, SECRET, &res, &LoadOptions::default());
        assert_eq!(load.outcome, LoadOutcome::Ok, "{load:?}");
        assert_eq!(load.restored, 4);
        assert_eq!(d2.get("k1").as_deref(), Some(&b"v1"[..]));
        assert_eq!(d2.get("ttl").as_deref(), Some(&b"soon"[..]));
        let ttl = d2.pttl("ttl").unwrap();
        assert!((55_000..=60_000).contains(&ttl), "ttl carried across: {ttl}");
        assert_eq!(d2.get("gone"), None);
        assert_eq!(d2.hget("h", "f2").as_deref(), Some(&b"b"[..]));
        assert_eq!(a2.get("k1").as_deref(), Some(&b"alice"[..]), "site data stays in its site");
        assert_eq!(
            d2.len(),
            3,
            "two strings + one hash, no cross-site leak into the default store"
        );
    }

    #[test]
    fn compressed_values_are_restored_as_stored_and_recoded_across_algorithms() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.snap");
        let src = store(CompressionAlgo::Zstd);
        let big = vec![b'z'; 4096];
        src.set("big".into(), big.clone(), None);
        let stores = vec![(String::new(), Arc::clone(&src))];
        write_snapshot(&path, SECRET, &stores, &opts()).unwrap();

        // Same algorithm: bytes go in as stored.
        let same = store(CompressionAlgo::Zstd);
        let res = resolver(vec![(String::new(), Arc::clone(&same))]);
        assert_eq!(
            load_snapshot(&path, SECRET, &res, &LoadOptions::default()).outcome,
            LoadOutcome::Ok
        );
        assert_eq!(same.get("big").as_deref(), Some(&big[..]));
        assert!(same.mem_used() < 4096, "stored compressed, not re-inflated");

        // Different algorithm: decoded then re-encoded by the target store.
        let other = store(CompressionAlgo::Gzip);
        let res = resolver(vec![(String::new(), Arc::clone(&other))]);
        assert_eq!(
            load_snapshot(&path, SECRET, &res, &LoadOptions::default()).outcome,
            LoadOutcome::Ok
        );
        assert_eq!(other.get("big").as_deref(), Some(&big[..]));
    }

    #[test]
    fn wrong_secret_truncation_and_tampering_all_load_cold() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.snap");
        let src = store(CompressionAlgo::None);
        for i in 0..2000u32 {
            src.set(format!("k{i}"), vec![i as u8; 200], None);
        }
        let stores = vec![(String::new(), Arc::clone(&src))];
        let report = write_snapshot(&path, SECRET, &stores, &opts()).unwrap();
        assert!(report.blocks >= 1);
        let bytes = std::fs::read(&path).unwrap();

        let fresh = || {
            let s = store(CompressionAlgo::None);
            (Arc::clone(&s), s)
        };
        let cold_with = |p: &std::path::Path, reason: ColdReason| {
            let (s, _keep) = fresh();
            let res = resolver(vec![(String::new(), Arc::clone(&s))]);
            let r = load_snapshot(p, SECRET, &res, &LoadOptions::default());
            assert_eq!(r.outcome, LoadOutcome::Cold(reason), "{r:?}");
            assert_eq!(s.len(), 0, "a cold load restores nothing");
        };

        // Wrong key.
        {
            let (s, _keep) = fresh();
            let res = resolver(vec![(String::new(), Arc::clone(&s))]);
            let r = load_snapshot(&path, b"other-secret", &res, &LoadOptions::default());
            assert_eq!(r.outcome, LoadOutcome::Cold(ColdReason::WrongKey));
            assert_eq!(s.len(), 0);
        }
        // Missing.
        cold_with(&dir.path().join("nope.snap"), ColdReason::Missing);
        // Truncated anywhere: no trailer → corrupt, nothing restored.
        for cut in [bytes.len() - 1, bytes.len() - 24, bytes.len() / 2, 70, 10] {
            let p = dir.path().join("cut.snap");
            std::fs::write(&p, &bytes[..cut]).unwrap();
            cold_with(&p, ColdReason::Corrupt);
        }
        // Header tamper → CRC.
        let mut t = bytes.clone();
        t[16] ^= 0x01;
        let p = dir.path().join("hdr.snap");
        std::fs::write(&p, &t).unwrap();
        cold_with(&p, ColdReason::Corrupt);
        // Future version.
        let mut t = bytes.clone();
        t[8] = 0x02;
        std::fs::write(&p, &t).unwrap();
        cold_with(&p, ColdReason::Version);
        // Index tamper → corrupt.
        let mut t = bytes.clone();
        let trailer = format::Trailer::decode(&bytes[bytes.len() - 24..]).unwrap();
        t[trailer.index_offset as usize + 10] ^= 0xff;
        std::fs::write(&p, &t).unwrap();
        cold_with(&p, ColdReason::Corrupt);
        // Too large.
        let (s, _keep) = fresh();
        let res = resolver(vec![(String::new(), Arc::clone(&s))]);
        let r = load_snapshot(
            &path,
            SECRET,
            &res,
            &LoadOptions { max_size: 100, ..LoadOptions::default() },
        );
        assert_eq!(r.outcome, LoadOutcome::Cold(ColdReason::TooLarge));
    }

    #[test]
    fn a_damaged_data_block_is_skipped_and_the_rest_restored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.snap");
        let src = store(CompressionAlgo::None);
        for i in 0..3000 {
            src.set(format!("k{i}"), vec![b'x'; 1000], None);
        }
        let stores = vec![(String::new(), Arc::clone(&src))];
        let o = WriteOptions { block_target: 64 * 1024, ..opts() };
        let report = write_snapshot(&path, SECRET, &stores, &o).unwrap();
        assert!(report.blocks >= 3, "{report:?}");
        let mut bytes = std::fs::read(&path).unwrap();
        // Flip a byte inside the second data block's ciphertext.
        let insp = inspect(&path, Some(SECRET), u64::MAX).unwrap();
        let _ = insp;
        let trailer = format::Trailer::decode(&bytes[bytes.len() - 24..]).unwrap();
        let header_len = format::Header::declared_len(&bytes[..12]).unwrap();
        let first_len =
            u32::from_le_bytes(bytes[header_len + 1..header_len + 5].try_into().unwrap()) as usize;
        let second = header_len + 5 + first_len;
        assert!(second + 20 < trailer.index_offset as usize);
        bytes[second + 20] ^= 0x55;
        std::fs::write(&path, &bytes).unwrap();

        let v = verify(&path, SECRET, u64::MAX).unwrap();
        assert_eq!(v.bad_blocks, vec![1]);
        assert!(!v.is_clean());

        let dst = store(CompressionAlgo::None);
        let res = resolver(vec![(String::new(), Arc::clone(&dst))]);
        let r = load_snapshot(&path, SECRET, &res, &LoadOptions::default());
        assert_eq!(r.outcome, LoadOutcome::Partial(PartialReason::CorruptBlocks));
        assert_eq!(r.bad_blocks, 1);
        assert_eq!(r.restored + u64::from(0u32), dst.len() as u64);
        assert!(dst.len() < 3000 && !dst.is_empty());
    }

    #[test]
    fn deadline_stops_the_load_and_reports_partial() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.snap");
        let src = store(CompressionAlgo::None);
        for i in 0..20_000 {
            src.set(format!("k{i}"), vec![b'x'; 100], None);
        }
        let stores = vec![(String::new(), Arc::clone(&src))];
        let o = WriteOptions { block_target: 16 * 1024, ..opts() };
        let report = write_snapshot(&path, SECRET, &stores, &o).unwrap();
        assert!(report.blocks > 10);
        let dst = store(CompressionAlgo::None);
        let res = resolver(vec![(String::new(), Arc::clone(&dst))]);
        // A deadline already in the past: the open still verifies the file,
        // then no block is applied.
        let r = load_snapshot(
            &path,
            SECRET,
            &res,
            &LoadOptions {
                deadline: Some(Instant::now() - Duration::from_secs(1)),
                workers: 1,
                ..LoadOptions::default()
            },
        );
        assert_eq!(r.outcome, LoadOutcome::Partial(PartialReason::Timeout), "{r:?}");
        assert_eq!(r.unreached_blocks, r.blocks);
        assert_eq!(dst.len(), 0);
    }

    #[test]
    fn memory_limit_refusals_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.snap");
        let src = store(CompressionAlgo::None);
        for i in 0..200 {
            src.set(format!("k{i}"), vec![b'x'; 1000], None);
        }
        let stores = vec![(String::new(), Arc::clone(&src))];
        write_snapshot(&path, SECRET, &stores, &opts()).unwrap();
        let small = Store::new(StoreConfig {
            memory_limit: 50_000,
            eviction_policy: EvictionPolicy::NoEviction,
            compression: CompressionConfig::default(),
        });
        let res = resolver(vec![(String::new(), Arc::clone(&small))]);
        let r = load_snapshot(&path, SECRET, &res, &LoadOptions::default());
        assert_eq!(r.outcome, LoadOutcome::Partial(PartialReason::MemoryLimit));
        assert!(r.refused > 0 && r.restored > 0, "{r:?}");
    }

    #[test]
    fn unknown_sites_are_skipped_not_merged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.snap");
        let default = store(CompressionAlgo::None);
        let bob = store(CompressionAlgo::None);
        default.set("d".into(), b"1".to_vec(), None);
        bob.set("secret".into(), b"bob".to_vec(), None);
        let stores = vec![(String::new(), Arc::clone(&default)), ("bob".into(), Arc::clone(&bob))];
        write_snapshot(&path, SECRET, &stores, &opts()).unwrap();
        let only_default = store(CompressionAlgo::None);
        let res = resolver(vec![(String::new(), Arc::clone(&only_default))]);
        let r = load_snapshot(&path, SECRET, &res, &LoadOptions::default());
        assert_eq!(r.outcome, LoadOutcome::Partial(PartialReason::UnknownSite));
        assert_eq!(r.unknown_site, 1);
        assert_eq!(only_default.get("secret"), None);
        assert_eq!(only_default.len(), 1);
    }

    #[test]
    fn max_size_aborts_the_write_and_keeps_the_previous_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.snap");
        let src = store(CompressionAlgo::None);
        src.set("k".into(), b"v".to_vec(), None);
        let stores = vec![(String::new(), Arc::clone(&src))];
        write_snapshot(&path, SECRET, &stores, &opts()).unwrap();
        let before = std::fs::read(&path).unwrap();

        for i in 0..5000u64 {
            src.set(format!("k{i}"), noise(i, 500), None);
        }
        let o = WriteOptions { max_size: 100_000, ..opts() };
        let err = write_snapshot(&path, SECRET, &stores, &o).unwrap_err();
        assert!(matches!(err, SnapshotError::TooLarge { .. }), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), before, "previous snapshot untouched");
        let leftovers: Vec<_> =
            std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(leftovers.len(), 1, "no temp file left behind: {leftovers:?}");
    }

    #[test]
    fn inspect_without_the_secret_reads_the_header_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.snap");
        let src = store(CompressionAlgo::None);
        src.set("k".into(), b"v".to_vec(), None);
        let stores = vec![("site-a".to_string(), Arc::clone(&src))];
        write_snapshot(&path, SECRET, &stores, &opts()).unwrap();
        let i = inspect(&path, None, u64::MAX).unwrap();
        assert_eq!(i.header.node_id, "test-node");
        assert!(i.sites.is_none());
        let i = inspect(&path, Some(SECRET), u64::MAX).unwrap();
        let sites = i.sites.unwrap();
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].site, "site-a");
        assert_eq!(sites[0].entries, 1);
        assert!(matches!(inspect(&path, Some(b"nope"), u64::MAX), Err(SnapshotError::WrongKey)));
    }

    #[test]
    fn an_empty_store_writes_a_loadable_empty_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.snap");
        let src = store(CompressionAlgo::None);
        let stores = vec![(String::new(), Arc::clone(&src))];
        let report = write_snapshot(&path, SECRET, &stores, &opts()).unwrap();
        assert_eq!(report.blocks, 0);
        let res = resolver(vec![(String::new(), Arc::clone(&src))]);
        let r = load_snapshot(&path, SECRET, &res, &LoadOptions::default());
        assert_eq!(r.outcome, LoadOutcome::Ok);
        assert_eq!(r.restored, 0);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]

        /// Any set of keys/values/TTLs across any set of sites round-trips
        /// exactly, with expired entries dropped and sites kept apart.
        #[test]
        fn prop_round_trip(
            entries in prop::collection::vec(
                (
                    prop::sample::select(vec!["", "a.test", "b.test"]),
                    "[a-z0-9:_-]{1,24}",
                    prop::collection::vec(any::<u8>(), 0..300),
                    prop::option::of(1_000u64..100_000),
                ),
                0..200,
            ),
            block_target in 4096usize..65536,
        ) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("kv.snap");
            let sites = ["", "a.test", "b.test"];
            let src: Vec<(String, Arc<Store>)> =
                sites.iter().map(|s| (s.to_string(), store(CompressionAlgo::Gzip))).collect();
            let mut expect: std::collections::HashMap<(String, String), Vec<u8>> =
                std::collections::HashMap::default();
            for (site, key, value, ttl_ms) in &entries {
                let st = &src.iter().find(|(s, _)| s == site).unwrap().1;
                st.set(key.clone(), value.clone(), ttl_ms.map(Duration::from_millis));
                expect.insert((site.to_string(), key.clone()), value.clone());
            }
            let o = WriteOptions { block_target, ..opts() };
            write_snapshot(&path, SECRET, &src, &o).unwrap();
            let dst: Vec<(String, Arc<Store>)> =
                sites.iter().map(|s| (s.to_string(), store(CompressionAlgo::Gzip))).collect();
            let res = resolver(dst.clone());
            let r = load_snapshot(&path, SECRET, &res, &LoadOptions { workers: 3, ..LoadOptions::default() });
            prop_assert_eq!(r.outcome, LoadOutcome::Ok);
            for ((site, key), value) in &expect {
                let st = &dst.iter().find(|(s, _)| s == site).unwrap().1;
                let got = st.get(key);
                prop_assert_eq!(got.as_deref(), Some(&value[..]), "{}/{}", site, key);
                for (other, ost) in &dst {
                    if other != site && !expect.contains_key(&(other.clone(), key.clone())) {
                        prop_assert!(ost.get(key).is_none(), "{key} leaked into {other}");
                    }
                }
            }
            let total: usize = dst.iter().map(|(_, s)| s.len()).sum();
            prop_assert_eq!(total, expect.len());
        }

        /// Random byte damage anywhere in the file never panics and never
        /// restores a wrong value: either cold, or every restored value is
        /// one that was written.
        #[test]
        fn prop_damage_never_restores_wrong_data(
            flips in prop::collection::vec((0usize..10_000, 1u8..=255), 1..8),
        ) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("kv.snap");
            let src = store(CompressionAlgo::None);
            for i in 0..100 {
                src.set(format!("key{i}"), format!("value{i}").into_bytes(), None);
            }
            let stores = vec![(String::new(), Arc::clone(&src))];
            let o = WriteOptions { block_target: 4096, ..opts() };
            write_snapshot(&path, SECRET, &stores, &o).unwrap();
            let mut bytes = std::fs::read(&path).unwrap();
            for (at, x) in flips {
                let i = at % bytes.len();
                bytes[i] ^= x;
            }
            std::fs::write(&path, &bytes).unwrap();
            let dst = store(CompressionAlgo::None);
            let res = resolver(vec![(String::new(), Arc::clone(&dst))]);
            let r = load_snapshot(&path, SECRET, &res, &LoadOptions::default());
            for key in dst.keys("*") {
                let v = dst.get(&key).unwrap();
                let n: usize = key.strip_prefix("key").unwrap().parse().unwrap();
                let want = format!("value{n}");
                prop_assert_eq!(&v[..], want.as_bytes());
            }
            if matches!(r.outcome, LoadOutcome::Cold(_)) {
                prop_assert_eq!(dst.len(), 0);
            }
        }
    }

    #[test]
    fn bytes_helper() {
        assert_eq!(unix_ms_for(UNIX_EPOCH), 0);
        assert!(unix_ms_for(SystemTime::now()) > 1_600_000_000_000);
        let _ = Bytes::new();
    }
}
