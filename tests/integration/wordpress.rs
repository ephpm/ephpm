//! WordPress integration smoke tests.
//!
//! These tests verify that ePHPm can serve a WordPress site correctly.
//! They require:
//! - libphp linked (real PHP runtime)
//! - A WordPress installation at the configured document root
//! - A running MySQL database
//!
//! Run with: `cargo test --test wordpress -- --ignored`

#[test]
#[ignore = "requires libphp and WordPress installation"]
fn wordpress_install_wizard_loads() {
    // TODO: Start ephpm server, GET /, verify install page HTML
}

#[test]
#[ignore = "requires libphp and WordPress installation"]
fn wordpress_static_assets_load() {
    // TODO: Verify CSS/JS/images return correct MIME types and 200 status
}

#[test]
#[ignore = "requires libphp and WordPress installation"]
fn wordpress_admin_login() {
    // TODO: POST to wp-login.php, verify session cookie is set
}

#[test]
#[ignore = "requires libphp and WordPress installation"]
fn wordpress_permalinks() {
    // TODO: GET /sample-page/, verify PHP handles the pretty permalink
}
