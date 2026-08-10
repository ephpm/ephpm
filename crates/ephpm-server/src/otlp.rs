//! OTLP trace export (cargo feature `otlp`).
//!
//! Exports the router's request spans (`http.request` with children
//! `worker.queue_wait` and `php.execute`, all emitted under
//! [`crate::OTEL_TRACE_TARGET`]) to an OpenTelemetry collector over
//! OTLP/HTTP with protobuf payloads.
//!
//! Activation is strictly opt-in at runtime, in precedence order:
//!
//! 1. `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` / `OTEL_EXPORTER_OTLP_ENDPOINT`
//!    environment variables (standard OTel semantics — the exporter builder
//!    reads them itself, so `.../v1/traces` handling follows the spec).
//! 2. `[server.diagnostics] otlp_endpoint` from config (a base URL;
//!    `/v1/traces` is appended when missing).
//!
//! When none of these are set, [`init_layer`] returns `Ok(None)`: no
//! exporter is built, no background thread is spawned, and no global
//! propagator is installed — the only remaining cost is the disabled-span
//! callsite check in the router.
//!
//! The exporter uses a blocking `reqwest` client driven by the OTel SDK's
//! own batch-export thread, deliberately avoiding any dependency on the
//! tokio runtime: the tracing subscriber (and therefore this layer) is
//! initialized in `main` *before* the runtime exists, because PHP must be
//! initialized single-threaded.

use anyhow::Context as _;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::Layer;
use tracing_subscriber::registry::LookupSpan;

/// Keeps the tracer provider alive and flushes it on drop.
///
/// Dropping the guard shuts the provider down, which flushes any spans still
/// sitting in the batch queue — hold it for the whole server lifetime.
pub struct OtlpGuard {
    provider: SdkTracerProvider,
}

impl Drop for OtlpGuard {
    fn drop(&mut self) {
        if let Err(e) = self.provider.shutdown() {
            // Shutdown races the collector going away; a lost final batch is
            // not worth a noisy exit.
            tracing::debug!(error = %e, "OTLP tracer provider shutdown reported an error");
        }
    }
}

/// Resolve the endpoint and build the OTLP tracing layer.
///
/// `config_endpoint` is `[server.diagnostics] otlp_endpoint`. The standard
/// `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` / `OTEL_EXPORTER_OTLP_ENDPOINT`
/// environment variables take precedence over it. Returns `Ok(None)` when no
/// endpoint is configured anywhere — the caller then installs no layer.
///
/// The returned layer is already filtered to [`crate::OTEL_TRACE_TARGET`] at
/// DEBUG, so it exports exactly the request spans and none of the log events.
///
/// The service name is `OTEL_SERVICE_NAME` when set, else `"ephpm"`.
///
/// The returned `String` describes the resolved endpoint and its source for
/// the caller to log — this function runs *before* the tracing subscriber is
/// installed, so it cannot log anything itself.
///
/// # Errors
///
/// Returns an error when the exporter cannot be built (malformed endpoint).
pub fn init_layer<S>(
    config_endpoint: Option<&str>,
) -> anyhow::Result<Option<(impl Layer<S>, OtlpGuard, String)>>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    let env_endpoint = ["OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "OTEL_EXPORTER_OTLP_ENDPOINT"]
        .iter()
        .find_map(|var| std::env::var(var).ok().filter(|v| !v.is_empty()));

    let builder = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_protocol(opentelemetry_otlp::Protocol::HttpBinary);

    let (builder, endpoint_source) = if let Some(ref env_ep) = env_endpoint {
        // Leave the endpoint to the builder's own env handling so the
        // standard semantics apply (OTEL_EXPORTER_OTLP_ENDPOINT is a base
        // URL that gets `/v1/traces` appended; the TRACES variant is used
        // verbatim).
        (builder, format!("env: {env_ep}"))
    } else if let Some(cfg_ep) = config_endpoint {
        let url = if cfg_ep.ends_with("/v1/traces") {
            cfg_ep.to_string()
        } else {
            format!("{}/v1/traces", cfg_ep.trim_end_matches('/'))
        };
        (builder.with_endpoint(&url), format!("config: {url}"))
    } else {
        return Ok(None);
    };

    use opentelemetry_otlp::WithExportConfig as _;
    let exporter = builder.build().context("failed to build the OTLP span exporter")?;

    let service_name = std::env::var("OTEL_SERVICE_NAME")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "ephpm".to_string());

    let resource =
        opentelemetry_sdk::Resource::builder().with_service_name(service_name.clone()).build();

    let provider =
        SdkTracerProvider::builder().with_batch_exporter(exporter).with_resource(resource).build();

    // W3C trace-context propagation: the router extracts an incoming
    // `traceparent` through the global propagator, which defaults to a
    // no-op — install the real one only when export is actually on.
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    let tracer = provider.tracer("ephpm");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer).with_filter(
        tracing_subscriber::filter::Targets::new()
            .with_target(crate::OTEL_TRACE_TARGET, tracing::level_filters::LevelFilter::DEBUG),
    );

    let description = format!("{endpoint_source} (service.name = {service_name})");
    Ok(Some((layer, OtlpGuard { provider }, description)))
}

/// Parent `span` to the trace context carried in an incoming W3C
/// `traceparent` header, if present.
///
/// Uses the global text-map propagator, which [`init_layer`] sets to the W3C
/// `TraceContext` propagator when export is enabled; without it (the no-op
/// default) extraction yields an empty context and this is a no-op.
pub(crate) fn set_span_parent_from_headers(span: &tracing::Span, headers: &hyper::HeaderMap) {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    if !headers.contains_key("traceparent") {
        return;
    }

    /// `Extractor` view over hyper's `HeaderMap` — a local shim so the
    /// `opentelemetry-http` crate isn't pulled in for two methods.
    struct HeaderMapExtractor<'a>(&'a hyper::HeaderMap);

    impl opentelemetry::propagation::Extractor for HeaderMapExtractor<'_> {
        fn get(&self, key: &str) -> Option<&str> {
            self.0.get(key).and_then(|v| v.to_str().ok())
        }

        fn keys(&self) -> Vec<&str> {
            self.0.keys().map(hyper::header::HeaderName::as_str).collect()
        }
    }

    let parent = opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderMapExtractor(headers))
    });
    if let Err(e) = span.set_parent(parent) {
        // A span the subscriber never enabled (or a malformed header) is not
        // worth more than a debug line on a per-request path.
        tracing::debug!(error = %e, "failed to parent request span to incoming traceparent");
    }
}
