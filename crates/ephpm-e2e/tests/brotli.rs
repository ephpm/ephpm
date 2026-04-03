//! Brotli compression negotiation test.
//!
//! Validates that ePHPm returns a brotli-compressed response when the client
//! sends `Accept-Encoding: br`, and that the compressed payload is valid brotli
//! that decompresses to the expected PHP output.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

#[tokio::test]
async fn brotli_response_is_compressed() {
    let base_url = required_env("EPHPM_URL");
    // info.php generates a large phpinfo() page — well above the 1 KiB compression threshold
    let url = format!("{base_url}/info.php");

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("Accept-Encoding", "br")
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(resp.status().as_u16(), 200);
    let encoding = resp
        .headers()
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        encoding.contains("br"),
        "expected Content-Encoding: br when Accept-Encoding: br was sent, got: {encoding:?}"
    );

    // reqwest has default-features = false (no auto-decompression), so body is raw brotli bytes
    let body = resp.bytes().await.expect("failed to read compressed body");
    assert!(!body.is_empty(), "compressed response body must not be empty");

    // Decompress and verify the output is valid brotli containing phpinfo() HTML
    let mut decompressed = Vec::new();
    let mut reader = brotli::Decompressor::new(body.as_ref(), 4096);
    std::io::Read::read_to_end(&mut reader, &mut decompressed)
        .expect("brotli decompression failed — response was not valid brotli");
    assert!(
        decompressed.len() > body.len(),
        "decompressed size ({}) should be larger than compressed size ({})",
        decompressed.len(),
        body.len()
    );

    let text = String::from_utf8_lossy(&decompressed);
    assert!(
        text.contains("PHP Version") || text.contains("phpinfo"),
        "decompressed body should contain phpinfo() output"
    );
}
