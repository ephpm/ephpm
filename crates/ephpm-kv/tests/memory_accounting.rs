//! `memory_limit` accounting versus the heap the store really holds.
//!
//! A counting global allocator measures the net bytes a workload leaves
//! allocated, and the test compares that with the store's own `mem_used()`.
//! The stress run found real memory 2-4x the configured limit (F-12); part of
//! that was the accounting under-counting what the store holds. This pins the
//! accounting to within a tolerance of the live heap for the shapes that
//! matter: small and large values, TTL'd keys, hashes, and every compression
//! algorithm.
//!
//! What it deliberately does **not** measure is allocator overhead outside the
//! heap the store requests (size-class rounding inside the allocator, freed
//! pages not yet returned to the OS, fragmentation). Those show up in RSS but
//! are not something a byte counter in the store can see; the KV architecture
//! docs say so.
//!
//! One `#[test]` running the scenarios in sequence, alone in this binary, so
//! no other test's allocations land in the measurement.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Duration;

use ephpm_kv::store::{CompressionAlgo, CompressionConfig, EvictionPolicy, Store, StoreConfig};

/// Net bytes currently allocated through the global allocator.
static LIVE: AtomicIsize = AtomicIsize::new(0);

struct Counting;

// SAFETY: every method forwards to `System` unchanged and only updates a
// counter; the layout contract is the caller's, exactly as for `System`.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size().cast_signed(), Ordering::Relaxed);
        // SAFETY: forwarded verbatim.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size().cast_signed(), Ordering::Relaxed);
        // SAFETY: forwarded verbatim.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size().cast_signed(), Ordering::Relaxed);
        // SAFETY: forwarded verbatim.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_add(new_size.cast_signed() - layout.size().cast_signed(), Ordering::Relaxed);
        // SAFETY: forwarded verbatim.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn live() -> isize {
    LIVE.load(Ordering::Relaxed)
}

/// Run `fill` against a fresh unlimited store and return
/// `(heap bytes the store holds, bytes it accounts)`.
fn measure(compression: CompressionConfig, fill: impl Fn(&Store)) -> (f64, f64) {
    let store = Store::new(StoreConfig {
        memory_limit: 0,
        eviction_policy: EvictionPolicy::AllKeysLru,
        compression,
    });
    let before = live();
    fill(&store);
    let held = live() - before;
    #[allow(clippy::cast_precision_loss)]
    (held as f64, store.mem_used() as f64)
}

fn none() -> CompressionConfig {
    CompressionConfig::default()
}

fn algo(algo: CompressionAlgo) -> CompressionConfig {
    CompressionConfig { algo, level: 6, min_size: 1024 }
}

/// A compressible but not trivial value: repeated text with a counter.
fn text(i: usize, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut n = i;
    while v.len() < len {
        v.extend_from_slice(
            format!("user:{n} logged in from 10.0.{}.{} ", n % 250, n % 7).as_bytes(),
        );
        n += 1;
    }
    v.truncate(len);
    v
}

/// A named workload and the compression it runs under.
type Case = (&'static str, CompressionConfig, Box<dyn Fn(&Store)>);

#[test]
fn accounted_memory_tracks_the_heap_the_store_holds() {
    let ttl = Some(Duration::from_secs(3_600));
    let cases: Vec<Case> = vec![
        (
            "100 B values",
            none(),
            Box::new(|s| {
                for i in 0..50_000 {
                    s.set(format!("key:{i}"), vec![b'v'; 100], None);
                }
            }),
        ),
        (
            "100 B values with TTL",
            none(),
            Box::new(move |s| {
                for i in 0..50_000 {
                    s.set(format!("key:{i}"), vec![b'v'; 100], ttl);
                }
            }),
        ),
        (
            "16 B values",
            none(),
            Box::new(|s| {
                for i in 0..50_000 {
                    s.set(format!("k{i}"), vec![b'v'; 16], None);
                }
            }),
        ),
        (
            "8 KiB values",
            none(),
            Box::new(|s| {
                for i in 0..2_000 {
                    s.set(format!("key:{i}"), vec![b'v'; 8 * 1024], None);
                }
            }),
        ),
        (
            "hash fields",
            none(),
            Box::new(|s| {
                for i in 0..20_000 {
                    let _ =
                        s.hset(&format!("h:{}", i % 100), &format!("field:{i}"), vec![b'v'; 50]);
                }
            }),
        ),
        (
            "gzip 16 KiB",
            algo(CompressionAlgo::Gzip),
            Box::new(|s| {
                for i in 0..1_000 {
                    s.set(format!("key:{i}"), text(i, 16 * 1024), None);
                }
            }),
        ),
        (
            "brotli 16 KiB",
            algo(CompressionAlgo::Brotli),
            Box::new(|s| {
                for i in 0..1_000 {
                    s.set(format!("key:{i}"), text(i, 16 * 1024), None);
                }
            }),
        ),
        (
            "zstd 16 KiB",
            algo(CompressionAlgo::Zstd),
            Box::new(|s| {
                for i in 0..1_000 {
                    s.set(format!("key:{i}"), text(i, 16 * 1024), None);
                }
            }),
        ),
    ];

    let mut failures = Vec::new();
    for (name, compression, fill) in &cases {
        let (held, accounted) = measure(*compression, fill);
        let ratio = held / accounted;
        println!(
            "{name:>24}: held {held:>12.0} accounted {accounted:>12.0} held/accounted {ratio:.2}"
        );
        if !(0.8..=1.25).contains(&ratio) {
            failures.push(format!("{name}: held/accounted = {ratio:.2}"));
        }
    }
    assert!(failures.is_empty(), "accounting is off: {failures:#?}");
}
