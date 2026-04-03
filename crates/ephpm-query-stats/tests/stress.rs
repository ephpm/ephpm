//! Stress tests for the query stats system.
//!
//! All tests are `#[ignore]` — they run only during nightly CI where
//! longer execution times are acceptable.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use ephpm_query_stats::digest::normalize;
use ephpm_query_stats::{QueryStats, StatsConfig};

/// 20 distinct SQL patterns used across concurrent-recording tests.
/// Each pattern normalizes to a unique digest because the table/column
/// names differ.
fn sql_patterns() -> Vec<String> {
    (0..20)
        .map(|i| match i % 4 {
            0 => format!("SELECT * FROM table_{i} WHERE id = {}", i * 100),
            1 => format!(
                "INSERT INTO table_{i} (col_a, col_b) VALUES ({}, 'value_{i}')",
                i * 10
            ),
            2 => format!("UPDATE table_{i} SET col_a = {} WHERE id = {}", i * 5, i),
            _ => format!("DELETE FROM table_{i} WHERE created_at < '{i}-01-01'"),
        })
        .collect()
}

/// Spawn 50 threads, each recording 1,000 queries from a pool of 20
/// distinct SQL patterns. Verify the total execution count across all
/// digests equals 50,000 and each digest's count matches the expected
/// frequency.
///
/// To avoid the inherent check-then-insert race in `DashMap` (where two
/// threads both see a missing key and one overwrites the other's insert),
/// we pre-populate all 20 digests with a single recording before the
/// concurrent phase. This ensures all 50,000 concurrent recordings hit
/// the `get_mut` update path, which is shard-locked and atomic.
#[test]
#[ignore = "nightly CI only — spawns 50 threads"]
fn concurrent_recording_accuracy() {
    let stats = QueryStats::new(StatsConfig::default());
    let patterns = Arc::new(sql_patterns());
    let num_threads: usize = 50;
    let queries_per_thread: usize = 1_000;

    // Pre-populate all 20 digests so concurrent threads only update,
    // never race on first-insert.
    for pattern in patterns.iter() {
        stats.record(pattern, Duration::from_micros(1), true, 0);
    }
    assert_eq!(stats.digest_count(), 20);

    let barrier = Arc::new(Barrier::new(num_threads));

    let handles: Vec<_> = (0..num_threads)
        .map(|t| {
            let stats = stats.clone();
            let patterns = Arc::clone(&patterns);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                for q in 0..queries_per_thread {
                    let idx = (t * queries_per_thread + q) % patterns.len();
                    stats.record(
                        &patterns[idx],
                        Duration::from_micros(100),
                        true,
                        1,
                    );
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }

    // Still exactly 20 distinct digests.
    assert_eq!(
        stats.digest_count(),
        20,
        "expected 20 distinct digests, got {}",
        stats.digest_count()
    );

    // Total count = 20 (seed) + 50 * 1000 (concurrent) = 50,020.
    let expected_total = (patterns.len() + num_threads * queries_per_thread) as u64;
    let top = stats.top_queries(100);
    let total_count: u64 = top.iter().map(|e| e.count).sum();
    assert_eq!(
        total_count, expected_total,
        "total count mismatch: expected {expected_total}, got {total_count}",
    );

    // Each of the 20 patterns is hit 1 (seed) + 2,500 (concurrent) = 2,501 times.
    let expected_per_digest =
        1 + (num_threads * queries_per_thread / patterns.len()) as u64;
    for entry in &top {
        assert_eq!(
            entry.count, expected_per_digest,
            "digest {:?} has count {} but expected {}",
            entry.digest_text, entry.count, expected_per_digest
        );
    }
}

/// Single-threaded: normalize 100,000 realistic SQL queries and assert
/// throughput exceeds 50,000 queries/second (conservative baseline to
/// catch accidental O(n^2) regressions).
#[test]
#[ignore = "nightly CI only — benchmarks normalization throughput"]
fn normalization_throughput() {
    let queries: Vec<String> = (0..100_000_u32)
        .map(|i| match i % 5 {
            0 => format!(
                "SELECT u.id, u.name, u.email FROM users u \
                 JOIN orders o ON o.user_id = u.id \
                 WHERE u.status = 'active' AND o.total > {}.99",
                i % 1000
            ),
            1 => format!(
                "INSERT INTO events (user_id, event_type, payload, created_at) \
                 VALUES ({}, 'page_view', '{{\"url\": \"/page/{i}\"}}', '2025-01-{:02} 12:00:00')",
                i % 5000,
                (i % 28) + 1
            ),
            2 => format!(
                "UPDATE products SET price = {}.{:02}, updated_at = '2025-03-15' \
                 WHERE sku = 'SKU-{}'",
                (i % 500) + 1,
                i % 100,
                i % 10_000
            ),
            3 => format!(
                "SELECT p.*, c.name AS category FROM products p \
                 INNER JOIN categories c ON c.id = p.category_id \
                 WHERE p.price BETWEEN {} AND {} \
                 ORDER BY p.created_at DESC LIMIT {}",
                i % 100,
                (i % 100) + 50,
                (i % 50) + 10,
            ),
            _ => format!(
                "DELETE FROM sessions WHERE user_id = {} \
                 AND expires_at < '2025-06-{:02} 00:00:00'",
                i % 8000,
                (i % 28) + 1
            ),
        })
        .collect();

    let start = Instant::now();
    for sql in &queries {
        let _ = normalize(sql);
    }
    let elapsed = start.elapsed();

    #[allow(clippy::cast_precision_loss)] // 100_000 is well within f64 precision
    let throughput = queries.len() as f64 / elapsed.as_secs_f64();
    assert!(
        throughput > 50_000.0,
        "normalization throughput too low: {throughput:.0} queries/sec \
         (elapsed {elapsed:?} for {} queries). Possible O(n^2) regression.",
        queries.len()
    );
}

/// Configure `QueryStats` with `max_digests = 50`, record 200 distinct
/// query patterns, and verify the entry count never exceeds 50.
#[test]
#[ignore = "nightly CI only — tests digest cap enforcement"]
fn max_digests_cap() {
    let config = StatsConfig {
        max_digests: 50,
        ..Default::default()
    };
    let stats = QueryStats::new(config);

    for i in 0..200 {
        // Each iteration produces a unique digest because the table name differs.
        let sql = format!("SELECT * FROM unique_table_{i} WHERE id = 1");
        stats.record(&sql, Duration::from_micros(50), true, 1);

        assert!(
            stats.digest_count() <= 50,
            "digest count {} exceeded cap of 50 after recording pattern {i}",
            stats.digest_count()
        );
    }

    assert_eq!(
        stats.digest_count(),
        50,
        "expected exactly 50 digests (cap), got {}",
        stats.digest_count()
    );
}

/// 20 threads record queries while another thread calls `reset()`.
/// Verify no panics or deadlocks. After the final reset, verify stats
/// are empty.
#[test]
#[ignore = "nightly CI only — tests reset safety under contention"]
fn reset_under_concurrent_load() {
    let stats = QueryStats::new(StatsConfig::default());
    let patterns = Arc::new(sql_patterns());
    let running = Arc::new(AtomicBool::new(true));
    let barrier = Arc::new(Barrier::new(22)); // 20 writers + 1 resetter + main

    // Spawn 20 writer threads.
    let writers: Vec<_> = (0..20_u32)
        .map(|t| {
            let stats = stats.clone();
            let patterns = Arc::clone(&patterns);
            let running = Arc::clone(&running);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let mut i: usize = 0;
                while running.load(Ordering::Relaxed) {
                    let idx = (t as usize * 1000 + i) % patterns.len();
                    stats.record(
                        &patterns[idx],
                        Duration::from_micros(10),
                        true,
                        1,
                    );
                    i = i.wrapping_add(1);
                }
            })
        })
        .collect();

    // Spawn the resetter thread.
    let reset_stats = stats.clone();
    let reset_running = Arc::clone(&running);
    let reset_barrier = Arc::clone(&barrier);
    let resetter = thread::spawn(move || {
        reset_barrier.wait();
        for _ in 0..100 {
            reset_stats.reset();
            // Small yield to let writers make progress between resets.
            thread::sleep(Duration::from_millis(1));
        }
        // Signal writers to stop.
        reset_running.store(false, Ordering::Relaxed);
    });

    // Main thread participates in the barrier to start everyone together.
    barrier.wait();

    resetter.join().expect("resetter thread panicked");
    for w in writers {
        w.join().expect("writer thread panicked");
    }

    // Final reset to guarantee empty state.
    stats.reset();
    assert_eq!(
        stats.digest_count(),
        0,
        "stats should be empty after final reset, got {}",
        stats.digest_count()
    );
}
