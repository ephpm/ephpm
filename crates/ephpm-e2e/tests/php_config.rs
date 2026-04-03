//! PHP configuration tests.
//!
//! Validates:
//! - `ini_overrides` from config take effect inside PHP
//! - Concurrent PHP requests all succeed (ZTS worker threading)
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

#[tokio::test]
async fn ini_overrides_take_effect() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/ini_check.php");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "expected 200 from /ini_check.php, got {}",
        resp.status()
    );

    let body: serde_json::Value = resp
        .json()
        .await
        .expect("ini_check.php must return valid JSON");

    // The test config sets display_errors = On
    let display_errors = body["display_errors"]
        .as_str()
        .expect("display_errors must be a string");
    assert!(
        display_errors == "1" || display_errors.eq_ignore_ascii_case("on"),
        "ini_override display_errors = On must be active, got: {display_errors}"
    );

    // The test config sets error_reporting = E_ALL
    let error_reporting = body["error_reporting"]
        .as_str()
        .expect("error_reporting must be a string");
    let er_value: i64 = error_reporting
        .parse()
        .unwrap_or_else(|_| panic!("error_reporting must be numeric, got: {error_reporting}"));
    // E_ALL is 32767 on PHP 8.x
    assert!(
        er_value > 0,
        "ini_override error_reporting = E_ALL must produce a positive value, got: {er_value}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_concurrency() {
    let base_url = required_env("EPHPM_URL");

    // Send 5 concurrent requests to a PHP page that does a small sleep.
    // We don't assert parallelism timing — just that all complete without
    // deadlocking or returning errors.
    let handles: Vec<_> = (0..5)
        .map(|i| {
            let base_url = base_url.clone();
            tokio::spawn(async move {
                let url = format!("{base_url}/sleep.php?seconds=0.1");
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .build()
                    .expect("failed to build reqwest client");
                let resp = client
                    .get(&url)
                    .send()
                    .await
                    .unwrap_or_else(|e| panic!("request {i} failed: {e}"));
                (i, resp.status().as_u16())
            })
        })
        .collect();

    for handle in handles {
        let (i, status) = handle
            .await
            .unwrap_or_else(|e| panic!("task panicked: {e}"));
        assert_eq!(
            status, 200,
            "concurrent request {i} expected 200, got {status} — \
             possible deadlock or worker exhaustion"
        );
    }
}
