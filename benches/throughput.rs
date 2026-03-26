//! Throughput benchmarks for ePHPm.
//!
//! Uses criterion to measure requests/sec for:
//! - Static file serving
//! - PHP script execution
//! - WordPress page rendering
//!
//! These benchmarks require libphp to be linked and a PHP document root
//! to be configured. They are skipped in CI unless the PHP environment
//! is available.

fn main() {
    // Criterion benchmarks will be added when the server is functional
    // with a real PHP runtime linked.
    println!("Benchmarks require libphp — skipping in stub mode.");
}
