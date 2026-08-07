//! Guard: the DB integration job must actually provision every database the
//! `ephpm-db` integration suite needs.
//!
//! [`common::db_url`] already turns a missing URL into a failure at the point
//! of use — but only for tests that happen to run. This file closes the
//! complementary hole: a test added with a *new* `*_TEST_URL` that the
//! workflow never exports would otherwise be `#[ignore]`d into invisibility,
//! and nothing would notice that its database was never started.
//!
//! Unlike everything else in this suite, this test is **not** `#[ignore]`d, so
//! it also runs in the ordinary `cargo nextest run --workspace` job. There it
//! is inert: [`common::require_db_tests`] is false unless `CI` is set together
//! with a deliberate `EPHPM_REQUIRE_DB_TESTS=1`, which only
//! `.github/workflows/db-integration.yml` does.
//!
//! See issue #238.

mod common;

/// Every name in [`common::REQUIRED_DB_URL_VARS`] must be exported by whatever
/// provisioned this run.
#[test]
fn ci_provisions_every_required_database() {
    // Opt-in: only the DB integration job asserts this. Elsewhere (local runs,
    // the plain workspace test job) there is nothing to check.
    if !matches!(std::env::var(common::REQUIRE_FLAG).as_deref(), Ok("1" | "true" | "yes")) {
        println!(
            "{} not set — nothing to verify (this test only asserts inside the DB \
             integration job)",
            common::REQUIRE_FLAG
        );
        return;
    }

    let missing: Vec<&str> = common::REQUIRED_DB_URL_VARS
        .iter()
        .copied()
        .filter(|var| !std::env::var(var).is_ok_and(|v| !v.trim().is_empty()))
        .collect();

    assert!(
        missing.is_empty(),
        "the DB integration job did not provision: {}.\n\
         Every name in common::REQUIRED_DB_URL_VARS must be started and exported by \
         .github/workflows/db-integration.yml. If a test no longer needs one of these, \
         remove it from REQUIRED_DB_URL_VARS in the same change — do not leave it \
         unexported, because the tests reading it would then skip silently (issue #238).",
        missing.join(", ")
    );
}
