//! WordPress integration smoke tests.
//!
//! These tests require a full ePHPm release build with `libphp` linked and
//! a WordPress installation at the configured document root. They cannot
//! run in CI without significant infrastructure (PHP SDK, WordPress files,
//! database).
//!
//! Run with: `cargo test --test wordpress -- --ignored`

#[test]
#[ignore = "requires release build with libphp, WordPress install, and MySQL database — see module docs"]
fn wordpress_integration_placeholder() {
    // This test exists as a reminder that WordPress integration testing
    // requires a full environment:
    //
    // 1. Build with `cargo xtask release` to embed libphp
    // 2. Install WordPress at the document root
    // 3. Configure a MySQL database (or use ephpm's embedded SQLite via litewire)
    // 4. Start ephpm and verify:
    //    - GET / returns the install wizard (or front page if configured)
    //    - Static assets (CSS/JS/images) return correct MIME types
    //    - POST to wp-login.php sets a session cookie
    //    - Pretty permalinks route through PHP correctly
    //
    // These scenarios are covered by the ephpm-e2e crate which runs in a
    // Kind cluster via `cargo xtask e2e`.
}
