//! PostgreSQL wire protocol frontend e2e tests.
//!
//! Tests that the PostgreSQL wire protocol proxy (litewire) starts correctly
//! alongside the MySQL frontend when `postgres_listen` is configured under
//! `[db.sqlite.proxy]`.
//!
//! **Limitation:** Full end-to-end testing through PHP requires the `pdo_pgsql`
//! extension, which is not currently included in the static PHP build (see
//! `PHP_EXTENSIONS` in `xtask/src/main.rs`). These tests verify that enabling
//! the PostgreSQL frontend does not interfere with normal server operation.
//! Once `pdo_pgsql` is added to the extensions list, a PHP-based test should
//! be added that connects via `pdo_pgsql` to `127.0.0.1:5432` and runs queries.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

/// Verify that the server starts and serves requests normally when the
/// PostgreSQL wire protocol frontend is enabled in config.
///
/// This confirms that `postgres_listen` does not cause a startup failure
/// or interfere with HTTP serving / MySQL frontend operation.
#[tokio::test]
async fn postgres_frontend_enabled_server_healthy() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/index.php");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "index.php should return 200 when postgres frontend is enabled, got {}",
        resp.status()
    );
}

/// Verify that the MySQL frontend still works when the PostgreSQL frontend
/// is also enabled — both can coexist on different ports.
///
/// This reuses the existing `sqlite_test.php` which connects via `pdo_mysql`.
#[tokio::test]
async fn postgres_frontend_does_not_break_mysql_frontend() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/sqlite_test.php?action=setup");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "sqlite_test.php setup should succeed with both frontends enabled, got {}",
        resp.status()
    );

    let body: serde_json::Value = resp
        .json()
        .await
        .expect("sqlite_test.php should return valid JSON");

    assert_eq!(
        body["status"], "ok",
        "MySQL frontend should still work when postgres frontend is also enabled: {body}"
    );

    // Cleanup
    let cleanup_url = format!("{base_url}/sqlite_test.php?action=cleanup");
    let _ = reqwest::get(&cleanup_url).await;
}
