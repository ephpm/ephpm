//! Header size limit tests.
//!
//! Validates:
//! - Requests with headers exceeding `max_header_size` are rejected
//! - Requests with normal-sized headers succeed
//!
//! The test config sets `max_header_size = 4096` (4 KiB). hyper enforces
//! this via `max_buf_size` and responds with 431 Request Header Fields Too
//! Large (or drops the connection entirely for extremely large headers).
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

#[tokio::test]
async fn oversized_header_is_rejected() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/test.html");

    // Build a header value that pushes the total header block well past 4096 bytes.
    // The request line + Host header already consume ~100 bytes, so an 8 KiB
    // custom header guarantees we exceed the limit.
    let large_value = "X".repeat(8 * 1024);

    let client = reqwest::Client::new();
    let result = client
        .get(&url)
        .header("X-Large-Header", &large_value)
        .send()
        .await;

    match result {
        Ok(resp) => {
            // hyper may return 431 if it can parse enough of the request to
            // send a response before closing.
            let status = resp.status().as_u16();
            assert!(
                status == 431 || status == 400,
                "expected 431 Request Header Fields Too Large (or 400) for oversized headers, got {status}"
            );
        }
        Err(_) => {
            // hyper may also just drop the connection when the buffer
            // overflows during header parsing — this is acceptable behavior
            // for an oversized request.
        }
    }
}

#[tokio::test]
async fn normal_headers_succeed() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/test.html");

    // A modest custom header well under the 4096-byte limit.
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("X-Small-Header", "hello")
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} with small header failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "requests with normal-sized headers must succeed, got {}",
        resp.status()
    );
}
