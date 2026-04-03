//! TDS (SQL Server) wire protocol frontend e2e tests.
//!
//! Tests that the TDS wire protocol proxy (litewire) starts correctly
//! alongside the MySQL and PostgreSQL frontends when `tds_listen` is
//! configured under `[db.sqlite.proxy]`.
//!
//! **Limitation:** Full end-to-end testing through PHP requires a TDS client
//! extension (e.g. `pdo_dblib`), which is not currently included in the
//! static PHP build. These tests verify that enabling the TDS frontend does
//! not interfere with normal server operation. Once a TDS PHP extension is
//! available, a PHP-based test should be added that connects via TDS to
//! `127.0.0.1:1433` and runs queries.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

/// Verify that the server starts and serves requests normally when the
/// TDS wire protocol frontend is enabled in config.
///
/// This confirms that `tds_listen` does not cause a startup failure
/// or interfere with HTTP serving / MySQL frontend operation.
#[tokio::test]
async fn tds_frontend_enabled_server_healthy() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/index.php");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "index.php should return 200 when TDS frontend is enabled, got {}",
        resp.status()
    );
}

/// Verify that the MySQL frontend still works when the TDS frontend
/// is also enabled — both can coexist on different ports.
///
/// This reuses the existing `sqlite_test.php` which connects via `pdo_mysql`.
#[tokio::test]
async fn tds_frontend_does_not_break_mysql_frontend() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/sqlite_test.php?action=setup");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "sqlite_test.php setup should succeed with TDS frontend enabled, got {}",
        resp.status()
    );

    let body: serde_json::Value = resp
        .json()
        .await
        .expect("sqlite_test.php should return valid JSON");

    assert_eq!(
        body["status"], "ok",
        "MySQL frontend should still work when TDS frontend is also enabled: {body}"
    );

    // Cleanup
    let cleanup_url = format!("{base_url}/sqlite_test.php?action=cleanup");
    let _ = reqwest::get(&cleanup_url).await;
}
