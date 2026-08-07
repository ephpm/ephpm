//! Shared harness for the `ephpm-db` integration tests that need a live
//! database.
//!
//! # Why this exists
//!
//! Every integration test in this crate that talks to a real server is
//! `#[ignore]` and reads a `*_TEST_URL` environment variable. Historically each
//! test read its own variable directly and returned early when it was unset:
//!
//! ```ignore
//! let Some(url) = std::env::var("MYSQL_TEST_URL").ok() else { return };
//! ```
//!
//! That is indistinguishable, in a CI summary, from the test passing. No
//! workflow ever set those variables, so the entire `ephpm-db` integration
//! suite reported green while asserting nothing — which is how issue #234 (the
//! proxy could not authenticate to a default-configured `MySQL` 8) reached a
//! release. It was found by a benchmark, not by the suite that nominally
//! covers the proxy. See issue #238.
//!
//! # The rule
//!
//! Skipping is a **local developer convenience only**. Under CI, a missing URL
//! is a hard failure: [`db_url`] panics rather than returning `None`. "No
//! database was configured" can therefore never again be reported as a pass.
//!
//! # Adding a test that needs a new database
//!
//! 1. Read its URL through [`db_url`], never through `std::env::var` directly.
//! 2. Add the variable name to [`REQUIRED_DB_URL_VARS`].
//! 3. Export it from `.github/workflows/db-integration.yml`.
//!
//! Step 2 is enforced: `tests/db_env_guard.rs` fails the DB integration job if
//! a name listed there is not exported by the workflow, so a new test cannot
//! quietly join the suite without its database being provisioned.

// Each integration test binary gets its own copy of this module and uses only
// the parts it needs, so unused-item warnings here are structural rather than
// a signal of dead code.
#![allow(dead_code)]

/// Environment variable that forces the CI behaviour on (`1`) or off (`0`).
///
/// Unset means "infer from `CI`". Setting it to `0` is the escape hatch for
/// running a subset of the suite on a CI machine without every database.
pub const REQUIRE_FLAG: &str = "EPHPM_REQUIRE_DB_TESTS";

/// Every environment variable naming a live database that some integration
/// test in this crate needs.
///
/// Keep in sync with `.github/workflows/db-integration.yml` — `db_env_guard`
/// asserts the two agree whenever [`REQUIRE_FLAG`] is set.
pub const REQUIRED_DB_URL_VARS: &[&str] = &[
    // MySQL proxy round-trip, pooling, R/W split, reset-strategy tests.
    "MYSQL_TEST_URL",
    // Admin URL against a *stock* MySQL 8 for the caching_sha2_password
    // full-auth tests (issue #234 / PR #236). Deliberately a separate server
    // from MYSQL_TEST_URL: those tests create and drop accounts and call
    // FLUSH PRIVILEGES, which perturbs the auth cache for everything else.
    "MYSQL_SHA2_TEST_URL",
    // PostgreSQL proxy handshake (SCRAM-SHA-256) and round-trip tests.
    "PG_TEST_URL",
];

/// Whether a missing database URL should fail rather than skip.
///
/// True when [`REQUIRE_FLAG`] is truthy, or when it is unset and `CI` is set —
/// i.e. the default on any CI runner is "fail loudly".
#[must_use]
pub fn require_db_tests() -> bool {
    match std::env::var(REQUIRE_FLAG) {
        Ok(v) => matches!(v.trim(), "1" | "true" | "yes"),
        Err(_) => std::env::var("CI").is_ok_and(|v| !v.is_empty()),
    }
}

/// Read a database URL, or return `None` so the caller can skip.
///
/// # Panics
///
/// Panics when the variable is unset or empty and [`require_db_tests`] is
/// true. This is the guard that turns a silent CI skip into a red build.
#[must_use]
pub fn db_url(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => {
            assert!(
                !require_db_tests(),
                "{var} is not set, but database-backed tests are required here \
                 ({REQUIRE_FLAG}/CI). Refusing to skip: a test that cannot fail \
                 is indistinguishable from a test that passes (issue #238). \
                 Provision the database and export {var}, or set \
                 {REQUIRE_FLAG}=0 to opt this run out explicitly."
            );
            println!(
                "{var} not set — skipping. Set {var} to a live server to run this test; \
                 in CI an unset {var} is a failure, not a skip."
            );
            None
        }
    }
}
