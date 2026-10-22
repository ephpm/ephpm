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

/// Worker framework-boot duration buckets (seconds): 10 ms up to 30 s. Heavy
/// frameworks (Laravel, WordPress) can take seconds to bootstrap once.
const WORKER_BOOT_BUCKETS: &[f64] = &[0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 20.0, 30.0];

/// CDC batch-apply duration buckets (seconds): 100 µs up to 5 s. A batch
/// is one replicated transaction, so the healthy range is sub-millisecond
/// and the tail matters more than the median.
const CDC_APPLY_BUCKETS: &[f64] =
    &[0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.5, 1.0, 5.0];

/// Snapshot bootstrap transfer buckets (seconds): 10 ms up to 5 min. A
/// logical dump of a large database is minutes, not milliseconds, and it
/// blocks a cold replica's startup — the wide top end is the point.
const CDC_SNAPSHOT_BUCKETS: &[f64] =
    &[0.01, 0.05, 0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0];

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
        // HTTP/3 gets its own duration series so h3-vs-h2 latency is
        // comparable; same buckets, so the two are directly overlayable.
        .set_buckets_for_metric(
            Matcher::Full("ephpm_http3_request_duration_seconds".to_string()),
            PHP_DURATION_BUCKETS,
        )
        .context("failed to configure http3_request_duration buckets")?
        .set_buckets_for_metric(
            Matcher::Full("ephpm_php_execution_duration_seconds".to_string()),
            PHP_DURATION_BUCKETS,
        )
        .context("failed to configure php_execution_duration buckets")?
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
        .set_buckets_for_metric(
            // Framework boot can take seconds (Laravel/WordPress) — wider range.
            Matcher::Full("ephpm_worker_boot_duration_seconds".to_string()),
            WORKER_BOOT_BUCKETS,
        )
        .context("failed to configure worker_boot_duration buckets")?
        .set_buckets_for_metric(
            Matcher::Full("ephpm_worker_request_wait_seconds".to_string()),
            PHP_DURATION_BUCKETS,
        )
        .context("failed to configure worker_request_wait buckets")?
        .set_buckets_for_metric(
            Matcher::Full(crate::turso_cdc_metrics::METRIC_APPLY_DURATION.to_string()),
            CDC_APPLY_BUCKETS,
        )
        .context("failed to configure cdc_apply_duration buckets")?
        .set_buckets_for_metric(
            Matcher::Full(crate::turso_cdc_metrics::METRIC_SNAPSHOT_DURATION.to_string()),
            CDC_SNAPSHOT_BUCKETS,
        )
        .context("failed to configure cdc_snapshot_duration buckets")?
        // KV warm-restart snapshots: a save or load is seconds for a large
        // store, so the same wide range as the CDC snapshot bootstrap.
        .set_buckets_for_metric(
            Matcher::Full(crate::kv_snapshot::METRIC_SAVE_DURATION.to_string()),
            CDC_SNAPSHOT_BUCKETS,
        )
        .context("failed to configure kv_snapshot_save_duration buckets")?
        .set_buckets_for_metric(
            Matcher::Full(crate::kv_snapshot::METRIC_LOAD_DURATION.to_string()),
            CDC_SNAPSHOT_BUCKETS,
        )
        .context("failed to configure kv_snapshot_load_duration buckets")?
        .set_buckets_for_metric(
            Matcher::Full(crate::kv_snapshot::METRIC_HANDOFF_DURATION.to_string()),
            CDC_SNAPSHOT_BUCKETS,
        )
        .context("failed to configure kv_handoff_duration buckets")?
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

/// Render the full scrape payload as text: everything recorded through the
/// [`metrics`] facade, plus the per-site CDC gauges that live outside it so
/// they can be removed when a site stops replicating here (see
/// [`crate::turso_cdc_metrics::render_site_gauges`]).
#[must_use]
pub fn render_text(handle: &PrometheusHandle) -> String {
    let mut body = handle.render();
    crate::turso_cdc_metrics::render_site_gauges(&mut body);
    body
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
    let body = render_text(handle);
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain; version=0.0.4; charset=utf-8")
        .body(crate::body::buffered(Full::new(Bytes::from(body))))
        .expect("static metrics response builder is always valid")
}

/// Holds a gauge at +1 for exactly as long as it lives.
///
/// For "currently doing X" gauges whose X spans `.await` points
/// (`ephpm_http_requests_in_flight`, `ephpm_worker_busy`). A bare
/// increment/decrement pair around an await leaks the increment whenever the
/// future is dropped in between — the request timeout firing, or hyper
/// dropping the service future when the client disconnects — and the gauge
/// then drifts upward forever. The decrement lives in `Drop`, which runs on
/// cancellation too.
#[must_use = "the gauge is decremented as soon as the guard is dropped"]
pub(crate) struct GaugeGuard(&'static str);

impl GaugeGuard {
    /// Increment `name` and return the guard that decrements it.
    pub(crate) fn enter(name: &'static str) -> Self {
        gauge!(name).increment(1.0);
        Self(name)
    }
}

impl Drop for GaugeGuard {
    fn drop(&mut self) {
        gauge!(self.0).decrement(1.0);
    }
}

/// Render what `f` records into a private Prometheus recorder, for tests that
/// assert on metric values without touching the process-global recorder.
///
/// `f` runs on a fresh current-thread runtime on this thread (with a paused
/// clock), so everything it records — including from tasks it spawns — lands
/// in the local recorder. Records made on other OS threads (pool threads) go to
/// the global recorder and are not seen.
#[cfg(test)]
pub(crate) fn record_locally<F, Fut>(f: F) -> (Fut::Output, String)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future,
{
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    let out = ::metrics::with_local_recorder(&recorder, || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("test runtime")
            .block_on(f())
    });
    (out, handle.render())
}

/// The value of the series `series` (name plus any `{labels}`, exactly as
/// rendered) in a Prometheus text payload, if present.
#[cfg(test)]
pub(crate) fn rendered_value(payload: &str, series: &str) -> Option<f64> {
    payload.lines().find_map(|line| {
        let rest = line.strip_prefix(series)?;
        rest.strip_prefix(' ')?.trim().parse().ok()
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// The gauge comes back down when the future holding the guard is dropped
    /// mid-await — the cancellation a bare increment/decrement pair leaked.
    #[test]
    fn gauge_guard_survives_cancellation() {
        let ((), payload) = record_locally(|| async {
            let pending = async {
                let _guard = GaugeGuard::enter("ephpm_test_in_flight");
                std::future::pending::<()>().await;
            };
            assert!(tokio::time::timeout(Duration::from_millis(10), pending).await.is_err());
        });
        assert_eq!(rendered_value(&payload, "ephpm_test_in_flight"), Some(0.0), "{payload}");
    }

    #[test]
    fn gauge_guard_counts_while_held() {
        let ((), payload) = record_locally(|| async {
            let a = GaugeGuard::enter("ephpm_test_busy");
            let b = GaugeGuard::enter("ephpm_test_busy");
            drop(a);
            std::mem::forget(b);
        });
        assert_eq!(rendered_value(&payload, "ephpm_test_busy"), Some(1.0), "{payload}");
    }
}
