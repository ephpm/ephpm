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
//!
//! # TLS
//!
//! Both `http://` and `https://` endpoints work. Plaintext to a collector on
//! localhost stays the common, zero-config case — nothing here forces TLS or
//! changes how an `http://` endpoint behaves.
//!
//! The client is built by [`build_http_client`] rather than left to
//! `opentelemetry-otlp`'s default, for two reasons:
//!
//! 1. **Crypto provider.** reqwest is compiled with
//!    `rustls-tls-manual-roots-no-provider` — the only `rustls-tls-*` feature
//!    that does not also enable reqwest's `__rustls-ring` and link `ring`
//!    (issue #241). With no provider feature, reqwest's own TLS path calls
//!    `CryptoProvider::get_default()` and panics with "No provider set" when
//!    nothing has installed one — which is exactly the situation here, since
//!    the exporter is built in `main` before any listener starts. Handing it
//!    a preconfigured [`rustls::ClientConfig`] built from
//!    [`crate::tls::crypto_provider`] fixes the ordering *and* guarantees the
//!    exporter cannot drift onto a different provider than the HTTPS
//!    listener.
//! 2. **Root store.** reqwest supplies no roots in this configuration, so we
//!    choose them explicitly — see [`build_http_client`].

use std::time::Duration;

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

/// The OTLP spec's default export timeout when no env var overrides it
/// (the spec states it as 10000ms; `opentelemetry-otlp` uses the same value).
const DEFAULT_EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// Resolve the export timeout from the standard OTLP env vars.
///
/// `opentelemetry-otlp` applies this itself, but only to the client *it*
/// builds — we supply our own (see [`build_http_client`]), so the same
/// precedence is reproduced here. Both variables are in milliseconds.
fn export_timeout() -> Duration {
    ["OTEL_EXPORTER_OTLP_TRACES_TIMEOUT", "OTEL_EXPORTER_OTLP_TIMEOUT"]
        .iter()
        .find_map(|var| std::env::var(var).ok()?.parse::<u64>().ok())
        .map_or(DEFAULT_EXPORT_TIMEOUT, Duration::from_millis)
}

/// Build the exporter's HTTP client, with TLS wired up so that an `https://`
/// collector endpoint works.
///
/// `http://` endpoints are unaffected: reqwest only consults this TLS
/// configuration for `https://` URLs, so a plaintext collector on localhost
/// behaves exactly as it did before TLS was available.
///
/// # Trust anchors
///
/// The root store is the **union of the OS trust store and the bundled
/// Mozilla set**:
///
/// - **OS trust store** (`rustls-native-certs`) is what makes the realistic
///   HTTPS case work at all. A collector reached over TLS is usually an
///   internal one behind a corporate or private CA, and that CA exists *only*
///   in the platform trust store — a bundled-roots-only build could never
///   talk to it. `rustls-native-certs` also honours `SSL_CERT_FILE` /
///   `SSL_CERT_DIR`, which is why no ePHPm-specific trust-store setting is
///   introduced here: operators already have a standard way to point at a
///   private bundle. A `[server.diagnostics]` knob (a CA path, or pinning)
///   is the natural place to go if per-endpoint control is ever wanted.
/// - **Bundled Mozilla set** (`webpki-roots`) is added as a fallback so a
///   publicly trusted SaaS collector still works from a `scratch`/distroless
///   image that ships no CA bundle at all — a real deployment shape for a
///   single-binary server, and one where the OS store is simply empty.
///
/// Neither crate is new to the build; both are already in the dependency
/// graph, so this adds no packages and no second TLS stack.
///
/// The returned `String` describes the trust store for the caller to log —
/// like [`init_layer`], this runs before the tracing subscriber is installed
/// and so cannot log anything itself.
///
/// # Errors
///
/// Returns an error when the crypto provider rejects the default TLS
/// versions, or when the client cannot be constructed.
fn build_http_client() -> anyhow::Result<(reqwest::blocking::Client, String)> {
    let mut roots = rustls::RootCertStore::empty();

    // `errors` here are per-certificate parse/read failures on an otherwise
    // usable store; an empty store is the case actually worth reporting, and
    // that shows up in the description below.
    let native = rustls_native_certs::load_native_certs();
    let (native_added, _native_ignored) = roots.add_parsable_certificates(native.certs);

    let bundled = webpki_roots::TLS_SERVER_ROOTS.len();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    // Naming the provider explicitly is mandatory, not stylistic — see the
    // module docs and `crate::tls::crypto_provider`.
    let tls = rustls::ClientConfig::builder_with_provider(crate::tls::crypto_provider())
        .with_safe_default_protocol_versions()
        .context("crypto provider does not support the default TLS versions")?
        .with_root_certificates(roots)
        .with_no_client_auth();

    let timeout = export_timeout();

    // `reqwest::blocking` panics if constructed inside a tokio runtime.
    // `init_layer` runs before the runtime exists, but building on a plain
    // thread keeps that a local guarantee rather than a caller obligation —
    // it is also what the exporter's own default client does.
    let client = std::thread::spawn(move || {
        reqwest::blocking::Client::builder().timeout(timeout).use_preconfigured_tls(tls).build()
    })
    .join()
    .map_err(|_| anyhow::anyhow!("the OTLP HTTP client builder thread panicked"))?
    .context("failed to build the OTLP exporter's HTTP client")?;

    let description = format!(
        "TLS trust anchors: {native_added} from the OS store + {bundled} bundled; \
         timeout {}ms",
        timeout.as_millis()
    );
    Ok((client, description))
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
/// Returns an error when the exporter cannot be built (malformed endpoint),
/// or when its HTTP client cannot be built (see [`build_http_client`]).
pub fn init_layer<S>(
    config_endpoint: Option<&str>,
) -> anyhow::Result<Option<(impl Layer<S>, OtlpGuard, String)>>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    let env_endpoint = ["OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "OTEL_EXPORTER_OTLP_ENDPOINT"]
        .iter()
        .find_map(|var| std::env::var(var).ok().filter(|v| !v.is_empty()));

    // Resolve the endpoint before building the client: when nothing is
    // configured we return early and never touch the OS trust store.
    if env_endpoint.is_none() && config_endpoint.is_none() {
        return Ok(None);
    }

    use opentelemetry_otlp::WithHttpConfig as _;
    let (http_client, tls_description) = build_http_client()?;

    let builder = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_http_client(http_client)
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
        // Unreachable: the guard above already returned for "no endpoint
        // configured anywhere". Kept so the arm structure stays total.
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

    let description =
        format!("{endpoint_source} (service.name = {service_name}; {tls_description})");
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

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};

    use super::*;

    /// The client must speak plaintext exactly as before TLS was added.
    ///
    /// Enabling a reqwest TLS feature is the kind of change that can quietly
    /// alter the default scheme handling, and `http://` to a collector on
    /// localhost is the dominant real-world configuration — so it gets an
    /// offline test rather than being assumed.
    #[test]
    fn plain_http_endpoint_still_works() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");

        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).expect("read request");
            sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .expect("write response");
        });

        let (client, description) = build_http_client().expect("build client");
        let response = client
            .post(format!("http://{addr}/v1/traces"))
            .body(vec![0u8; 8])
            .send()
            .expect("plaintext POST must reach the listener");

        assert_eq!(response.status(), 200);
        server.join().expect("server thread");

        // The trust store is irrelevant to plaintext, but it must have been
        // built without error for the client to exist at all.
        assert!(description.contains("trust anchors"), "unexpected description: {description}");
    }

    /// A real TLS handshake against a public HTTPS endpoint.
    ///
    /// This is the whole point of the change: before it, reqwest was compiled
    /// with no TLS backend and every `https://` export failed at connect
    /// time. The assertion is deliberately about *reaching* HTTP semantics —
    /// any status code proves the handshake and certificate validation
    /// completed. A 4xx from a server that does not speak OTLP is a pass.
    ///
    /// Ignored by default: it needs outbound network access.
    #[test]
    #[ignore = "requires outbound network access"]
    fn https_endpoint_completes_a_real_tls_handshake() {
        let (client, description) = build_http_client().expect("build client");

        let response = client
            .post("https://example.com/v1/traces")
            .header("content-type", "application/x-protobuf")
            .body(vec![0u8; 8])
            .send()
            .expect("TLS handshake to a public HTTPS endpoint must succeed");

        println!("handshake OK: status {} ({description})", response.status());
    }
}
