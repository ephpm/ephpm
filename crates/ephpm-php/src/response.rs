//! PHP output to HTTP response mapping.
//!
//! Converts the output captured from PHP's SAPI callbacks into an HTTP response.

/// Response from a PHP script execution.
///
/// Built from the data captured by SAPI callbacks during PHP execution:
/// - `ub_write` → body
/// - `send_header` → headers
/// - PHP's response code → status
#[derive(Debug)]
pub struct PhpResponse {
    /// HTTP status code.
    pub status: u16,

    /// Response headers as (name, value) pairs.
    pub headers: Vec<(String, String)>,

    /// Response body bytes.
    pub body: Vec<u8>,
}
