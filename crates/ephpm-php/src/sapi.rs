//! SAPI bridge between Rust HTTP and PHP execution.
//!
//! The Server API (SAPI) is PHP's interface for communicating with the
//! hosting environment (web server, CLI, etc.). Our SAPI implementation
//! lives in `ephpm_wrapper.c` and provides:
//!
//! - **`ub_write`** — Captures PHP output (echo/print) into a buffer
//!   instead of writing to stdout.
//! - **`read_post`** — Feeds HTTP POST body data to PHP's input stream.
//! - **`read_cookies`** — Provides the Cookie header to PHP.
//! - **`register_server_variables`** — Populates `$_SERVER` with request
//!   metadata (method, URI, headers, etc.).
//! - **`send_headers`** — Captures response headers.
//! - **`log_message`** — Routes PHP error messages to stderr.
//!
//! The callbacks are installed by [`ephpm_install_sapi()`] (called once
//! during runtime initialization) and invoked by PHP during request
//! processing. Per-request data is passed from Rust to C via
//! [`ephpm_request_set_info()`] and [`ephpm_request_add_server_var()`],
//! and response data is retrieved via [`ephpm_get_output_buf()`],
//! [`ephpm_get_response_code()`], and [`ephpm_get_response_headers()`].
//!
//! See `ephpm_wrapper.c` for the C implementation and `lib.rs` for the
//! Rust-side orchestration in [`PhpRuntime::execute_php()`].
