//! Prometheus metrics exporter.
//!
//! Initializes the global [`metrics`] facade recorder and exposes a handler
//! that renders the current scrape payload in `OpenMetrics` text format.
//!
//! Call [`init`] once at startup. Pass the returned [`PrometheusHandle`] into
//! [`Router`](crate::router::Router) so the `/metrics` endpoint can serve it.

use ::metrics::gauge;
use anyhow::Context;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::{Response, StatusCode};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

/// Duration histogram buckets tuned for PHP workloads.
///
/// Covers the typical p50 range (10-50 ms) through worst-case p99 (up to 10 s).
const PHP_DURATION_BUCKETS: &[f64] =
    &[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

/// Tighter buckets for mutex contention — should stay well under 10 ms in
/// healthy deployments. High values indicate NTS serialization pressure.
const MUTEX_WAIT_BUCKETS: &[f64] =
    &[0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5];

/// Body size buckets in bytes: 100B to 10MB. Covers typical HTML (5-50KB),
/// API JSON (1-100KB), and larger asset responses.
const BODY_BYTES_BUCKETS: &[f64] = &[
    100.0,
    1_000.0,
    10_000.0,
    50_000.0,
    100_000.0,
    500_000.0,
    1_000_000.0,
    5_000_000.0,
    10_000_000.0,
];

/// Compression ratio buckets (compressed / original). Values close to 0 mean
/// excellent compression; values close to 1 mean negligible savings.
const COMPRESSION_RATIO_BUCKETS: &[f64] =
    &[0.05, 0.1, 0.15, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9];

/// Install the Prometheus recorder and return a scrape handle.
///
/// Must be called once at process startup, before any `metrics::counter!` /
/// `metrics::histogram!` / `metrics::gauge!` calls are made.
///
/// # Errors
///
/// Returns an error if the recorder cannot be installed (e.g. a recorder has
/// already been installed globally, or the bucket configuration is invalid).
pub fn init() -> anyhow::Result<PrometheusHandle> {
    let handle = PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full("ephpm_http_request_duration_seconds".to_string()),
            PHP_DURATION_BUCKETS,
        )
        .context("failed to configure http_request_duration buckets")?
        .set_buckets_for_metric(
            Matcher::Full("ephpm_php_execution_duration_seconds".to_string()),
            PHP_DURATION_BUCKETS,
        )
        .context("failed to configure php_execution_duration buckets")?
        .set_buckets_for_metric(
            Matcher::Full("ephpm_php_mutex_wait_seconds".to_string()),
            MUTEX_WAIT_BUCKETS,
        )
        .context("failed to configure php_mutex_wait buckets")?
        .set_buckets_for_metric(
            Matcher::Full("ephpm_http_request_body_bytes".to_string()),
            BODY_BYTES_BUCKETS,
        )
        .context("failed to configure request_body_bytes buckets")?
        .set_buckets_for_metric(
            Matcher::Full("ephpm_http_response_body_bytes".to_string()),
            BODY_BYTES_BUCKETS,
        )
        .context("failed to configure response_body_bytes buckets")?
        .set_buckets_for_metric(
            Matcher::Full("ephpm_php_output_bytes".to_string()),
            BODY_BYTES_BUCKETS,
        )
        .context("failed to configure php_output_bytes buckets")?
        .set_buckets_for_metric(
            Matcher::Full("ephpm_http_compression_ratio".to_string()),
            COMPRESSION_RATIO_BUCKETS,
        )
        .context("failed to configure compression_ratio buckets")?
        .install_recorder()
        .context("failed to install Prometheus recorder")?;

    // Static build-info gauge — always 1, carries version labels.
    gauge!(
        "ephpm_build_info",
        "version" => env!("CARGO_PKG_VERSION")
    )
    .set(1.0);

    Ok(handle)
}

/// Render the current scrape payload as an HTTP response.
///
/// Returns `text/plain; version=0.0.4` — the standard Prometheus text format
/// understood by Prometheus, Grafana Agent, and `OpenTelemetry` collectors.
///
/// # Panics
///
/// Panics if the HTTP response builder fails (should never happen with static headers).
#[must_use]
pub fn render(handle: &PrometheusHandle) -> Response<crate::body::ServerBody> {
    let body = handle.render();
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain; version=0.0.4; charset=utf-8")
        .body(crate::body::buffered(Full::new(Bytes::from(body))))
        .expect("static metrics response builder is always valid")
}
