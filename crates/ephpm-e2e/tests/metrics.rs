//! Metrics endpoint (`/metrics`) tests.
//!
//! Validates:
//! - Prometheus endpoint returns 200 with correct content-type
//! - `ephpm_build_info` gauge present with version label
//! - HTTP request counters increment after traffic
//! - Handler labels (`php`, `static`, `error`) appear correctly
//! - PHP execution metrics recorded after hitting a PHP endpoint
//! - Request/response body size histograms recorded
//! - In-flight gauge present
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)
//!
//! Requires `server.metrics.enabled = true` in the ephpm configuration.

use ephpm_e2e::required_env;

/// Scrape `/metrics` and return the body as a string.
async fn scrape_metrics(base_url: &str) -> String {
    let url = format!("{base_url}/metrics");
    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));
    assert_eq!(
        resp.status().as_u16(),
        200,
        "expected 200 from /metrics, got {}",
        resp.status()
    );
    resp.text().await.expect("failed to read /metrics body")
}

#[tokio::test]
async fn metrics_endpoint_returns_prometheus_format() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/metrics");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "expected 200 from /metrics, got {}",
        resp.status()
    );

    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("text/plain"),
        "expected text/plain content-type for Prometheus format, got: {ct}"
    );

    let body = resp.text().await.expect("failed to read body");
    assert!(
        !body.is_empty(),
        "/metrics response body must not be empty"
    );
}

#[tokio::test]
async fn metrics_contains_build_info() {
    let base_url = required_env("EPHPM_URL");
    let body = scrape_metrics(&base_url).await;

    assert!(
        body.contains("ephpm_build_info"),
        "metrics must contain ephpm_build_info gauge:\n{body}"
    );
    // Build info should have a version label
    assert!(
        body.contains("version="),
        "ephpm_build_info must include a version label:\n{body}"
    );
}

#[tokio::test]
async fn metrics_contains_http_request_counters() {
    let base_url = required_env("EPHPM_URL");
    let client = reqwest::Client::new();

    // Generate some traffic first
    let _ = client
        .get(format!("{base_url}/index.php"))
        .send()
        .await;
    let _ = client
        .get(format!("{base_url}/test.html"))
        .send()
        .await;

    let body = scrape_metrics(&base_url).await;

    assert!(
        body.contains("ephpm_http_requests_total"),
        "metrics must contain ephpm_http_requests_total counter:\n{body}"
    );
    assert!(
        body.contains("ephpm_http_request_duration_seconds"),
        "metrics must contain ephpm_http_request_duration_seconds histogram:\n{body}"
    );
}

#[tokio::test]
async fn metrics_handler_labels_correct() {
    let base_url = required_env("EPHPM_URL");
    let client = reqwest::Client::new();

    // Hit PHP endpoint
    let _ = client
        .get(format!("{base_url}/index.php"))
        .send()
        .await;

    // Hit static endpoint
    let _ = client
        .get(format!("{base_url}/test.html"))
        .send()
        .await;

    // Hit a missing path (error handler)
    let _ = client
        .get(format!("{base_url}/nonexistent.txt"))
        .send()
        .await;

    let body = scrape_metrics(&base_url).await;

    assert!(
        body.contains("handler=\"php\""),
        "metrics must contain handler=\"php\" label after hitting a PHP endpoint:\n{body}"
    );
    assert!(
        body.contains("handler=\"static\""),
        "metrics must contain handler=\"static\" label after hitting a static file:\n{body}"
    );
    assert!(
        body.contains("handler=\"error\""),
        "metrics must contain handler=\"error\" label after hitting a missing path:\n{body}"
    );
}

#[tokio::test]
async fn metrics_contains_php_execution_metrics() {
    let base_url = required_env("EPHPM_URL");

    // Trigger a PHP execution
    let _ = reqwest::get(format!("{base_url}/index.php")).await;

    let body = scrape_metrics(&base_url).await;

    assert!(
        body.contains("ephpm_php_executions_total"),
        "metrics must contain ephpm_php_executions_total counter:\n{body}"
    );
    assert!(
        body.contains("ephpm_php_execution_duration_seconds"),
        "metrics must contain ephpm_php_execution_duration_seconds histogram:\n{body}"
    );
    // At least one successful execution
    assert!(
        body.contains("status=\"ok\""),
        "metrics must show status=\"ok\" after a successful PHP execution:\n{body}"
    );
}

#[tokio::test]
async fn metrics_contains_in_flight_gauge() {
    let base_url = required_env("EPHPM_URL");
    let body = scrape_metrics(&base_url).await;

    assert!(
        body.contains("ephpm_http_requests_in_flight"),
        "metrics must contain ephpm_http_requests_in_flight gauge:\n{body}"
    );
}

#[tokio::test]
async fn metrics_contains_body_size_histograms() {
    let base_url = required_env("EPHPM_URL");
    let client = reqwest::Client::new();

    // POST with a body to trigger request body bytes recording
    let _ = client
        .post(format!("{base_url}/test.php"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("data=hello")
        .send()
        .await;

    let body = scrape_metrics(&base_url).await;

    assert!(
        body.contains("ephpm_http_request_body_bytes"),
        "metrics must contain ephpm_http_request_body_bytes histogram:\n{body}"
    );
    assert!(
        body.contains("ephpm_http_response_body_bytes"),
        "metrics must contain ephpm_http_response_body_bytes histogram:\n{body}"
    );
}

#[tokio::test]
async fn metrics_endpoint_counts_itself_as_metrics_handler() {
    let base_url = required_env("EPHPM_URL");

    // Hit /metrics twice — the first scrape is the request, the second captures it
    let _ = scrape_metrics(&base_url).await;
    let body = scrape_metrics(&base_url).await;

    assert!(
        body.contains("handler=\"metrics\""),
        "scraping /metrics should produce handler=\"metrics\" label:\n{body}"
    );
}

#[tokio::test]
async fn metrics_status_codes_recorded() {
    let base_url = required_env("EPHPM_URL");
    let client = reqwest::Client::new();

    // 200
    let _ = client
        .get(format!("{base_url}/test.html"))
        .send()
        .await;
    // 404
    let _ = client
        .get(format!("{base_url}/does_not_exist.xyz"))
        .send()
        .await;

    let body = scrape_metrics(&base_url).await;

    assert!(
        body.contains("status=\"200\""),
        "metrics must record status=\"200\" after a successful request:\n{body}"
    );
    assert!(
        body.contains("status=\"404\""),
        "metrics must record status=\"404\" after a missing-file request:\n{body}"
    );
}
