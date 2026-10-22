//! Throughput of the warm-restart snapshot writer and loader on a 1M-key
//! store. Ignored by default: it allocates a few hundred MiB and takes
//! seconds. Run with
//!
//! ```text
//! cargo test -p ephpm-kv --release --test snapshot_bench -- --ignored --nocapture
//! ```
//!
//! Prints keys/s and MiB/s for save and load so the numbers can be quoted in
//! the architecture page; the only assertion is that the round trip is exact.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ephpm_kv::snapshot::{LoadOptions, LoadOutcome, WriteOptions, load_snapshot, write_snapshot};
use ephpm_kv::store::{CompressionAlgo, CompressionConfig, EvictionPolicy, Store, StoreConfig};

const KEYS: usize = 1_000_000;
const VALUE_LEN: usize = 200;

fn store() -> Arc<Store> {
    Store::new(StoreConfig {
        memory_limit: 0,
        eviction_policy: EvictionPolicy::NoEviction,
        compression: CompressionConfig { algo: CompressionAlgo::None, level: 1, min_size: 1 << 20 },
    })
}

/// Values a PHP app would store: JSON-ish text with per-key variation, so
/// LZ4 has something realistic to work on (neither all-zero nor random).
fn value(i: usize) -> Vec<u8> {
    let mut v = format!(
        "{{\"user_id\":{i},\"name\":\"user-{i}\",\"roles\":[\"editor\",\"author\"],\"last_seen\":{},\"prefs\":{{\"theme\":\"dark\",\"lang\":\"en\"}},\"token\":\"{:016x}\"",
        1_700_000_000 + i,
        (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
    )
    .into_bytes();
    while v.len() < VALUE_LEN {
        v.push(b' ');
    }
    v.truncate(VALUE_LEN);
    v.extend_from_slice(b"}");
    v
}

#[test]
#[ignore = "benchmark: allocates ~400 MiB and takes seconds; run with --ignored --nocapture"]
fn snapshot_1m_keys_save_and_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bench.kvsnap");
    let src = store();
    let fill = Instant::now();
    for i in 0..KEYS {
        let ttl = (i % 4 == 0).then(|| Duration::from_secs(3600));
        src.set(format!("sess:{i:08}"), value(i), ttl);
    }
    println!(
        "fill:  {} keys in {:?} ({} MiB accounted)",
        KEYS,
        fill.elapsed(),
        src.mem_used() >> 20
    );

    let stores = vec![(String::new(), Arc::clone(&src))];
    let report = write_snapshot(&path, b"bench-secret", &stores, &WriteOptions::default()).unwrap();
    let secs = report.elapsed.as_secs_f64();
    let plain_mib = (KEYS * (VALUE_LEN + 1 + 13)) as f64 / (1024.0 * 1024.0);
    println!(
        "save:  {} keys, {} blocks, {:.1} MiB on disk ({:.1} MiB plaintext) in {:.2}s = {:.0} keys/s, {:.0} MiB/s plaintext, {:.0} MiB/s written",
        report.keys(),
        report.blocks,
        report.bytes as f64 / (1024.0 * 1024.0),
        plain_mib,
        secs,
        report.keys() as f64 / secs,
        plain_mib / secs,
        report.bytes as f64 / (1024.0 * 1024.0) / secs
    );
    drop(src);

    let dst = store();
    let resolve = {
        let dst = Arc::clone(&dst);
        move |_: &str| Some(Arc::clone(&dst))
    };
    let load = load_snapshot(&path, b"bench-secret", &resolve, &LoadOptions::default());
    assert_eq!(load.outcome, LoadOutcome::Ok, "{load:?}");
    let secs = load.elapsed.as_secs_f64();
    println!(
        "load:  {} keys in {:.2}s = {:.0} keys/s, {:.0} MiB/s plaintext ({} workers)",
        load.restored,
        secs,
        load.restored as f64 / secs,
        plain_mib / secs,
        std::thread::available_parallelism().map_or(1, |n| n.get().min(8))
    );
    assert_eq!(load.restored as usize, KEYS);
    assert_eq!(dst.get("sess:00000042").unwrap().as_ref(), value(42).as_slice());

    // Single-threaded load, for the per-core number.
    let dst1 = store();
    let resolve1 = {
        let dst1 = Arc::clone(&dst1);
        move |_: &str| Some(Arc::clone(&dst1))
    };
    let load1 = load_snapshot(
        &path,
        b"bench-secret",
        &resolve1,
        &LoadOptions { workers: 1, ..LoadOptions::default() },
    );
    assert_eq!(load1.outcome, LoadOutcome::Ok);
    let secs = load1.elapsed.as_secs_f64();
    println!(
        "load1: {} keys in {:.2}s = {:.0} keys/s, {:.0} MiB/s plaintext (1 worker)",
        load1.restored,
        secs,
        load1.restored as f64 / secs,
        plain_mib / secs
    );
}
