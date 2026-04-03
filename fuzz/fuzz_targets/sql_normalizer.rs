#![no_main]

use libfuzzer_sys::fuzz_target;

// Feed arbitrary strings into the SQL normalizer.
// Invariants: must never panic, must always return a valid String,
// output should not grow unboundedly relative to input.
fuzz_target!(|data: &[u8]| {
    // The normalizer expects UTF-8 strings. Try to interpret the bytes as
    // UTF-8; if invalid, use lossy conversion (the normalizer should handle
    // any valid &str without panicking).
    let sql = match std::str::from_utf8(data) {
        Ok(s) => s,
        Err(_) => return, // Skip non-UTF-8 inputs — normalize() takes &str
    };

    let normalized = ephpm_query_stats::digest::normalize(sql);

    // Sanity check: output should not be absurdly larger than input.
    // The normalizer replaces literals with '?' and may add ' ' for whitespace
    // collapse, but should never produce output > 2× input + small constant.
    assert!(
        normalized.len() <= sql.len() * 2 + 64,
        "normalizer output grew unexpectedly: input {} bytes → output {} bytes",
        sql.len(),
        normalized.len()
    );
});
