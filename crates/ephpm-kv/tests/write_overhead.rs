//! Manual microbenchmark for the KV hot paths (`set` / `get` / `incr_by`).
//!
//! Used as the before/after harness for changes that touch the write path
//! (e.g. the `ephpm_kv_wait` watch hooks): the file only uses the public
//! `Store` API, so the *identical* file can be dropped into a baseline
//! checkout of `main` and compared same-box, same-toolchain.
//!
//! Not a unit test — ignored by default. Run with:
//!
//! ```sh
//! cargo test -p ephpm-kv --release --test write_overhead -- --ignored --nocapture
//! ```
//!
//! Numbers are single-threaded ns/op; compare medians of the printed
//! rounds, not absolute values across machines.

use std::time::Instant;

use ephpm_kv::store::{Store, StoreConfig};

const ROUNDS: usize = 5;
const KEYS: usize = 512;
const SET_ITERS: usize = 2_000_000;
const GET_ITERS: usize = 4_000_000;
const INCR_ITERS: usize = 2_000_000;

fn keyset() -> Vec<String> {
    (0..KEYS).map(|i| format!("bench:key:{i}")).collect()
}

#[allow(clippy::cast_precision_loss)]
fn report(label: &str, iters: usize, elapsed: std::time::Duration) {
    let ns_per_op = elapsed.as_nanos() as f64 / iters as f64;
    let mops = iters as f64 / elapsed.as_secs_f64() / 1_000_000.0;
    println!("{label}: {iters} iters in {elapsed:?} -> {ns_per_op:.1} ns/op ({mops:.2} Mops/s)");
}

#[test]
#[ignore = "manual microbenchmark — run with --release --ignored --nocapture"]
fn set_get_incr_throughput() {
    let store = Store::new(StoreConfig { memory_limit: 0, ..StoreConfig::default() });
    let keys = keyset();
    let value = b"0123456789abcdef0123456789abcdef"; // 32 B, below compress min

    // Warmup: populate every key and fault in the map shards.
    for key in &keys {
        store.set(key.clone(), value.to_vec(), None);
    }

    for round in 1..=ROUNDS {
        println!("── round {round}/{ROUNDS} ──");

        let start = Instant::now();
        for i in 0..SET_ITERS {
            let key = &keys[i % KEYS];
            store.set(key.clone(), value.to_vec(), None);
        }
        report("set (overwrite)", SET_ITERS, start.elapsed());

        let start = Instant::now();
        let mut hits = 0usize;
        for i in 0..GET_ITERS {
            let key = &keys[i % KEYS];
            if store.get(key).is_some() {
                hits += 1;
            }
        }
        assert_eq!(hits, GET_ITERS, "every get must hit");
        report("get (hit)", GET_ITERS, start.elapsed());

        let start = Instant::now();
        for _ in 0..INCR_ITERS {
            store.incr_by("bench:counter", 1).expect("counter must stay an integer");
        }
        report("incr_by", INCR_ITERS, start.elapsed());
    }
}
