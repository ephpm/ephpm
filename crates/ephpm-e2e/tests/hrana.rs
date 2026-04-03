//! Hrana HTTP API smoke tests.
//!
//! Validates that the optional `hrana_listen` configuration under
//! `[db.sqlite.proxy]` is accepted by the server and that the Hrana
//! endpoint responds to requests.
//!
//! The test config sets `hrana_listen = "0.0.0.0:8081"` so litewire
//! exposes its HTTP API alongside the MySQL wire protocol frontend.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)
//! - `EPHPM_HRANA_URL` — base URL of the Hrana HTTP endpoint (e.g. `http://ephpm:8081`).
//!   If not set, Hrana-specific endpoint tests are skipped (the server may
//!   be behind a network boundary that the test runner cannot reach).

use ephpm_e2e::required_env;

/// Verify the main server starts successfully when `hrana_listen` is configured.
///
/// If the Hrana listener fails to bind or panics during startup, the
/// ephpm process will exit and this request will fail to connect.
#[tokio::test]
async fn server_starts_with_hrana_configured() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/index.php");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed — server may not have started: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "index.php must return 200 when hrana_listen is configured, got {}",
        resp.status()
    );
    let body = resp.text().await.expect("failed to read body");
    assert!(
        body.contains("Hello from ePHPm"),
        "index.php output must be served normally with Hrana enabled:\n{body}"
    );
}

/// Verify PHP can still execute SQL through the MySQL wire protocol while
/// the Hrana HTTP API is also listening on a separate port. This catches
/// port-binding conflicts or shared-state corruption between the two
/// frontends.
#[tokio::test]
async fn php_sqlite_query_works_with_hrana_enabled() {
    let base_url = required_env("EPHPM_URL");

    // The sqlite.rs e2e tests create tables via /db.php — we just need to
    // confirm a simple query round-trips. Use a standalone CREATE + SELECT
    // that does not depend on pre-existing state.
    let client = reqwest::Client::new();

    // Create a throwaway table.
    let create_url = format!(
        "{base_url}/db.php?sql={}",
        urlencoding::encode(
            "CREATE TABLE IF NOT EXISTS hrana_smoke (id INTEGER PRIMARY KEY, val TEXT)"
        )
    );
    let resp = client
        .get(&create_url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {create_url} failed: {e}"));
    assert_eq!(
        resp.status().as_u16(),
        200,
        "CREATE TABLE must succeed, got {}",
        resp.status()
    );

    // Insert a row.
    let insert_url = format!(
        "{base_url}/db.php?sql={}",
        urlencoding::encode("INSERT OR IGNORE INTO hrana_smoke (id, val) VALUES (1, 'hrana_ok')")
    );
    let resp = client
        .get(&insert_url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {insert_url} failed: {e}"));
    assert_eq!(
        resp.status().as_u16(),
        200,
        "INSERT must succeed, got {}",
        resp.status()
    );

    // Select it back.
    let select_url = format!(
        "{base_url}/db.php?sql={}",
        urlencoding::encode("SELECT val FROM hrana_smoke WHERE id = 1")
    );
    let resp = client
        .get(&select_url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {select_url} failed: {e}"));
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("failed to read body");
    assert!(
        body.contains("hrana_ok"),
        "SELECT must return the inserted value when Hrana is co-running:\n{body}"
    );
}

/// If the `EPHPM_HRANA_URL` environment variable is set, probe the Hrana
/// HTTP endpoint directly.
///
/// The Hrana v3 protocol responds to `POST /v3/pipeline` but also exposes
/// a basic health surface. We send a simple pipeline request and check
/// that the endpoint does not return a connection error or 404.
#[tokio::test]
async fn hrana_endpoint_responds() {
    let base_url = required_env("EPHPM_URL");

    // Ensure the server is up first.
    let resp = reqwest::get(format!("{base_url}/index.php"))
        .await
        .unwrap_or_else(|e| panic!("server health check failed: {e}"));
    assert_eq!(resp.status().as_u16(), 200);

    let hrana_url = match std::env::var("EPHPM_HRANA_URL") {
        Ok(url) => url,
        Err(_) => {
            // Hrana endpoint not reachable from the test runner — skip.
            eprintln!("EPHPM_HRANA_URL not set, skipping direct Hrana probe");
            return;
        }
    };

    // Send a minimal Hrana v3 pipeline request.
    let client = reqwest::Client::new();
    let pipeline_url = format!("{hrana_url}/v3/pipeline");
    let body = serde_json::json!({
        "requests": [
            {
                "type": "execute",
                "stmt": {
                    "sql": "SELECT 1 AS ping"
                }
            },
            {
                "type": "close"
            }
        ]
    });

    let resp = client
        .post(&pipeline_url)
        .json(&body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("POST {pipeline_url} failed: {e}"));

    // Accept any 2xx — the exact response format depends on the litewire
    // Hrana implementation version.
    assert!(
        resp.status().is_success(),
        "Hrana pipeline endpoint must return 2xx, got {}",
        resp.status()
    );

    let resp_body = resp.text().await.expect("failed to read Hrana response");
    assert!(
        !resp_body.is_empty(),
        "Hrana pipeline response must not be empty"
    );
}
