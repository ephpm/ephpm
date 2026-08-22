//! OTLP trace export (cargo feature `otlp`).
//!
//! Exports the router's request spans (`http.request` with children
//! `worker.queue_wait` and `php.execute`, all emitted under
//! [`crate::OTEL_TRACE_TARGET`]) to an OpenTelemetry collector, over either
//! OTLP transport — see [Transports](#transports).
//!
//! Activation is strictly opt-in at runtime, in precedence order:
//!
//! 1. `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` / `OTEL_EXPORTER_OTLP_ENDPOINT`
//!    environment variables (standard OTel semantics — the exporter builder
//!    reads them itself, so signal-path handling follows the spec).
//! 2. `[server.diagnostics] otlp_endpoint` from config (a base URL; see
//!    [`normalize_endpoint`] for the per-transport path rules).
//!
//! When none of these are set, [`init_layer`] returns `Ok(None)`: no
//! exporter is built, no background thread is spawned, and no global
//! propagator is installed — the only remaining cost is the disabled-span
//! callsite check in the router.
//!
//! # Transports
//!
//! Both OTLP transports are compiled in and the choice is made at **runtime**
//! by the standard `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` /
//! `OTEL_EXPORTER_OTLP_PROTOCOL` variables:
//!
//! | value | transport | default port |
//! |---|---|---|
//! | `http/protobuf` (default) | OTLP/HTTP, protobuf payloads | 4318 |
//! | `grpc` | OTLP/gRPC | 4317 |
//!
//! Runtime selection rather than a second cargo feature is deliberate. ePHPm
//! ships **one** release binary (`xtask`'s `RELEASE_FEATURES`), so a
//! compile-time switch would have to pick a transport on behalf of every user
//! — and since the feature would then always be on in releases, it would cost
//! exactly what compiling both costs anyway. Runtime selection is also the
//! behaviour the OTel ecosystem expects, so a user's existing
//! `OTEL_EXPORTER_OTLP_PROTOCOL` just works.
//!
//! `http/json` is *not* supported; it is rejected with a clear error rather
//! than silently falling back to a transport the operator did not ask for.
//!
//! # Runtimes
//!
//! The http/protobuf transport uses a blocking `reqwest` client driven by the
//! OTel SDK's own batch-export thread, deliberately avoiding any dependency on
//! the tokio runtime: the tracing subscriber (and therefore this layer) is
//! initialized in `main` *before* the runtime exists, because PHP must be
//! initialized single-threaded.
//!
//! The gRPC transport cannot avoid tokio — tonic is async to the core. It gets
//! its own small runtime, owned by [`OtlpGuard`], rather than borrowing the
//! server's: the server's does not exist yet at this point in `main`. See
//! [`RuntimeBoundExporter`] for how the SDK's synchronous batch thread drives
//! it.
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
    /// The gRPC transport's dedicated tokio runtime, `None` on the
    /// http/protobuf path. Declared after `provider` so it outlives the
    /// explicit `provider.shutdown()` in [`Drop`] — that shutdown performs the
    /// final flush, which for gRPC still needs this runtime alive.
    _grpc_runtime: Option<tokio::runtime::Runtime>,
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

/// What [`init_layer`] resolved, for the caller to log.
///
/// `init_layer` runs *before* the tracing subscriber is installed, so it
/// cannot log anything itself — everything it wants to say comes back here.
pub struct OtlpStartupInfo {
    /// The wire protocol in OTel's own spelling: `"http/protobuf"` or `"grpc"`.
    pub protocol: &'static str,
    /// The resolved endpoint, its source, and the transport's TLS/timeout
    /// details.
    pub description: String,
    /// Misconfigurations worth an operator's attention that are nonetheless
    /// legal, so they must not fail startup. Log each at WARN.
    pub warnings: Vec<String>,
}

/// The OTLP transport, chosen at runtime. See the module docs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Transport {
    /// OTLP/HTTP with protobuf payloads — the default, and what every
    /// pre-existing ePHPm deployment uses.
    HttpBinary,
    /// OTLP/gRPC.
    Grpc,
}

impl Transport {
    /// OTel's spelling of this transport, as used by
    /// `OTEL_EXPORTER_OTLP_PROTOCOL` and in log lines.
    const fn as_str(self) -> &'static str {
        match self {
            Self::HttpBinary => "http/protobuf",
            Self::Grpc => "grpc",
        }
    }

    /// The IANA-registered default collector port for this transport. Used
    /// only to spot an endpoint that names the *other* transport's port.
    const fn default_port(self) -> u16 {
        match self {
            Self::HttpBinary => 4318,
            Self::Grpc => 4317,
        }
    }
}

/// Resolve the transport, environment first.
///
/// Precedence, highest to lowest — the same shape as the endpoint's:
///
/// 1. `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL`
/// 2. `OTEL_EXPORTER_OTLP_PROTOCOL`
/// 3. `config_protocol` — `[server.diagnostics] otlp_protocol`, which itself
///    already reflects `EPHPM_SERVER__DIAGNOSTICS__OTLP_PROTOCOL`
///
/// All unset means `http/protobuf`, which is both the OTel default and what
/// ePHPm did before gRPC existed — so this stays additive.
///
/// # Errors
///
/// Returns an error for a value this build cannot honour, rather than falling
/// back to a transport the operator did not ask for. Silently exporting over
/// the wrong protocol is exactly the class of failure #378 is about.
fn resolve_transport(config_protocol: Option<&str>) -> anyhow::Result<Transport> {
    let from_env = ["OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "OTEL_EXPORTER_OTLP_PROTOCOL"]
        .iter()
        .find_map(|var| {
            std::env::var(var)
                .ok()
                .map(|v| ((*var).to_string(), v))
                .filter(|(_, v)| !v.trim().is_empty())
        });

    let Some((var, value)) = from_env.or_else(|| {
        config_protocol
            .filter(|v| !v.trim().is_empty())
            .map(|v| ("[server.diagnostics] otlp_protocol".to_string(), v.to_string()))
    }) else {
        return Ok(Transport::HttpBinary);
    };

    parse_transport(&var, &value)
}

/// The pure half of [`resolve_transport`], so the mapping can be tested
/// without mutating process-global environment state.
fn parse_transport(var: &str, value: &str) -> anyhow::Result<Transport> {
    match value.trim().to_ascii_lowercase().as_str() {
        "http/protobuf" | "http/proto" => Ok(Transport::HttpBinary),
        "grpc" => Ok(Transport::Grpc),
        "http/json" => Err(anyhow::anyhow!(
            "{var}=http/json is not supported by ePHPm's OTLP exporter; use \
             `grpc` or `http/protobuf`"
        )),
        other => Err(anyhow::anyhow!(
            "{var}={other} is not a valid OTLP protocol; expected `grpc` or \
             `http/protobuf`"
        )),
    }
}

/// Normalize `[server.diagnostics] otlp_endpoint` for the chosen transport.
///
/// The two transports disagree about paths, and getting this wrong is silent:
///
/// - **http/protobuf** takes a *signal* URL, so `/v1/traces` is appended when
///   the configured value does not already end with it.
/// - **gRPC** takes a *base* URL and no signal path — the signal is the gRPC
///   method name. Appending `/v1/traces` here produces a 404-equivalent from
///   the collector and no spans.
fn normalize_endpoint(transport: Transport, endpoint: &str) -> String {
    let trimmed = endpoint.trim_end_matches('/');
    match transport {
        Transport::Grpc => trimmed.to_string(),
        Transport::HttpBinary => {
            if endpoint.ends_with("/v1/traces") {
                endpoint.to_string()
            } else {
                format!("{trimmed}/v1/traces")
            }
        }
    }
}

/// Warn when an endpoint names the *other* transport's default port.
///
/// This is the single most likely OTLP misconfiguration: 4317 and 4318 are one
/// character apart and a collector accepts both, on different protocols. The
/// symptom is silence, which is precisely what #378 is about.
///
/// Deliberately a **warning, not an error**. Nothing stops an operator running
/// a gRPC receiver on 4318, so failing startup here would reject a legal
/// configuration. The goal is that the log says why, not that the server
/// refuses to run.
fn port_mismatch_warning(transport: Transport, endpoint: &str) -> Option<String> {
    let other = match transport {
        Transport::HttpBinary => Transport::Grpc,
        Transport::Grpc => Transport::HttpBinary,
    };
    let port = endpoint.parse::<http::Uri>().ok()?.port_u16()?;
    if port != other.default_port() {
        return None;
    }

    Some(format!(
        "OTLP endpoint {endpoint} uses port {port}, the conventional port for \
         {other_proto}, but the exporter is configured for {this_proto} (the \
         conventional port for which is {this_port}). If no spans arrive, \
         either set OTEL_EXPORTER_OTLP_PROTOCOL={other_proto} or point the \
         endpoint at port {this_port}.",
        other_proto = other.as_str(),
        this_proto = transport.as_str(),
        this_port = transport.default_port(),
    ))
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

/// Adapts an async [`SpanExporter`] to the SDK's synchronous batch thread.
///
/// [`opentelemetry_sdk::trace::BatchSpanProcessor`] runs on a plain
/// `std::thread` and drives exports with `futures_executor::block_on`
/// (`opentelemetry_sdk-0.31.0/src/trace/span_processor.rs`, the thread at
/// `:316` and the `block_on` at `:507`). That executor provides no tokio
/// reactor, so tonic's hyper connection would fail with "no reactor running".
///
/// So the export future is handed to our own runtime with
/// [`tokio::runtime::Handle::block_on`], which polls it on the calling thread
/// while the runtime's worker drives the IO. Blocking that thread is correct
/// and not a regression: it is the batch exporter's own dedicated thread, and
/// the http/protobuf transport already blocks it the same way via
/// `reqwest::blocking`.
///
/// `Handle::block_on` panics only when called from *inside* a runtime, which
/// the paragraph above rules out — the batch processor never runs on a tokio
/// worker. Keeping `inner` by value (rather than behind an `Arc`, as a
/// `Handle::spawn` implementation would require) is what lets the `&mut`
/// methods below forward directly; `set_resource` in particular carries
/// `service.name`, and silently dropping it would be a real defect.
#[derive(Debug)]
struct RuntimeBoundExporter<E> {
    inner: E,
    handle: tokio::runtime::Handle,
}

impl<E: opentelemetry_sdk::trace::SpanExporter> opentelemetry_sdk::trace::SpanExporter
    for RuntimeBoundExporter<E>
{
    fn export(
        &self,
        batch: Vec<opentelemetry_sdk::trace::SpanData>,
    ) -> impl std::future::Future<Output = opentelemetry_sdk::error::OTelSdkResult> + Send {
        let handle = self.handle.clone();
        let export = self.inner.export(batch);
        async move { handle.block_on(export) }
    }

    fn shutdown_with_timeout(
        &mut self,
        timeout: std::time::Duration,
    ) -> opentelemetry_sdk::error::OTelSdkResult {
        let _entered = self.handle.enter();
        self.inner.shutdown_with_timeout(timeout)
    }

    fn force_flush(&mut self) -> opentelemetry_sdk::error::OTelSdkResult {
        let _entered = self.handle.enter();
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &opentelemetry_sdk::Resource) {
        self.inner.set_resource(resource);
    }
}

/// Make [`crate::tls::crypto_provider`] the process-default rustls provider.
///
/// Unlike reqwest, tonic offers no `use_preconfigured_tls` escape hatch — it
/// builds the [`rustls::ClientConfig`] itself. What it *does* do is consult
/// `CryptoProvider::get_default()` **first**, ahead of any crate-feature
/// fallback (`tonic-0.14.5/src/transport/channel/service/tls.rs`, the `match`
/// at the top of `TlsConnector::new`). Installing our provider as the process
/// default is therefore the supported way to keep gRPC on the same provider as
/// the HTTPS listener.
///
/// Without this, the `tls-provider-agnostic` build reaches tonic's final arm,
/// a bare `ClientConfig::builder()`, which resolves via
/// `from_crate_features()` — `None` in a binary that has had two providers
/// linked — and panics. That is the same trap #371 hit with reqwest.
///
/// Installing a default is inert for the rest of the server: every other call
/// site names its provider explicitly with `builder_with_provider`.
///
/// Returns a warning when some *other* provider was already installed, which
/// would mean gRPC TLS silently uses it.
fn install_default_crypto_provider() -> Option<String> {
    let ours = crate::tls::crypto_provider();

    if rustls::crypto::CryptoProvider::install_default((*ours).clone()).is_ok() {
        return None;
    }

    // Err means one was already installed. Ours is installed exactly here, so
    // an existing one is either ours (idempotent, fine) or a foreign one.
    let installed = rustls::crypto::CryptoProvider::get_default();
    match installed {
        Some(p) if std::ptr::eq(std::ptr::from_ref(&**p), std::ptr::from_ref(&*ours)) => None,
        _ => Some(
            "a rustls crypto provider was already installed as the process \
             default before the OTLP gRPC exporter was built; gRPC TLS will \
             use that provider, which may differ from the HTTPS listener's"
                .to_string(),
        ),
    }
}

/// Build the OTLP/gRPC exporter together with the runtime that drives it.
///
/// `endpoint` is the already-normalized endpoint, or `None` to let
/// `opentelemetry-otlp` apply its own `OTEL_EXPORTER_OTLP_*` handling.
///
/// # TLS
///
/// The trust anchors match the http/protobuf transport's policy — the union of
/// the OS trust store and the bundled Mozilla set — so switching protocol does
/// not silently change which collectors are reachable. The two are configured
/// differently only because tonic owns its `ClientConfig`: it reads the OS
/// store itself (honouring `SSL_CERT_FILE` / `SSL_CERT_DIR` via the same
/// `rustls-native-certs`) rather than being handed one.
///
/// Native roots are requested only when the OS store actually has certificates
/// in it: tonic's `with_native_roots` fails the whole handshake with
/// `NativeCertsNotFound` on an empty store, which would make a
/// `scratch`/distroless image unable to reach even a publicly trusted
/// collector that the bundled roots cover.
///
/// # Errors
///
/// Returns an error when the runtime or the exporter cannot be built.
fn build_grpc_exporter(
    endpoint: Option<&str>,
) -> anyhow::Result<(
    // `use<>`: the exporter captures nothing from `endpoint` (the builder
    // copies it), and the SDK requires `'static` to hand it to the batch
    // processor. Without this, Rust 2024's capture rules infer a borrow.
    impl opentelemetry_sdk::trace::SpanExporter + use<>,
    tokio::runtime::Runtime,
    String,
)> {
    use opentelemetry_otlp::{WithExportConfig as _, WithTonicConfig as _};

    let provider_warning = install_default_crypto_provider();

    let native_count = rustls_native_certs::load_native_certs().certs.len();
    let bundled = webpki_roots::TLS_SERVER_ROOTS.len();

    let mut tls = tonic::transport::ClientTlsConfig::new().with_webpki_roots();
    if native_count > 0 {
        tls = tls.with_native_roots();
    }

    let timeout = export_timeout();

    // One worker is plenty: this runtime carries a single gRPC stream of
    // batched spans, and every export is serialized by the SDK anyway ("this
    // function will never be called concurrently for the same exporter
    // instance"). It costs one thread — the same order as the
    // `reqwest-internal-sync-runtime` thread the http transport spawns.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .thread_name("ephpm-otlp-grpc")
        .build()
        .context("failed to build the OTLP gRPC exporter's tokio runtime")?;

    // tonic's `connect_lazy` builds the channel without connecting, but it
    // still expects to be inside a runtime context. Building here rather than
    // relying on that being optional keeps it correct by construction.
    let exporter = {
        let _entered = runtime.enter();

        let mut builder =
            opentelemetry_otlp::SpanExporter::builder().with_tonic().with_tls_config(tls);
        if let Some(endpoint) = endpoint {
            builder = builder.with_endpoint(endpoint);
        }
        builder
            .with_timeout(timeout)
            .build()
            .context("failed to build the OTLP gRPC span exporter")?
    };

    let handle = runtime.handle().clone();
    let mut description = format!(
        "TLS trust anchors: {native_count} from the OS store + {bundled} bundled; timeout {}ms",
        timeout.as_millis()
    );
    if let Some(warning) = provider_warning {
        description.push_str("; ");
        description.push_str(&warning);
    }

    Ok((RuntimeBoundExporter { inner: exporter, handle }, runtime, description))
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
/// The returned [`OtlpStartupInfo`] describes the resolved transport,
/// endpoint and its source for the caller to log — this function runs *before*
/// the tracing subscriber is installed, so it cannot log anything itself. Its
/// `warnings` must be logged at WARN.
///
/// # Errors
///
/// Returns an error when `OTEL_EXPORTER_OTLP_PROTOCOL` names a protocol this
/// build cannot honour, when the exporter cannot be built (malformed
/// endpoint), or when its transport client cannot be built (see
/// [`build_http_client`] and [`build_grpc_exporter`]).
pub fn init_layer<S>(
    config_endpoint: Option<&str>,
    config_protocol: Option<&str>,
) -> anyhow::Result<Option<(impl Layer<S>, OtlpGuard, OtlpStartupInfo)>>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    let env_endpoint = ["OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "OTEL_EXPORTER_OTLP_ENDPOINT"]
        .iter()
        .find_map(|var| std::env::var(var).ok().filter(|v| !v.is_empty()));

    // Resolve the endpoint before building anything: when nothing is
    // configured we return early and never touch the OS trust store.
    if env_endpoint.is_none() && config_endpoint.is_none() {
        return Ok(None);
    }

    // Resolved before any client is built so an unsupported protocol fails
    // fast, with a message, instead of exporting over the wrong one.
    let transport = resolve_transport(config_protocol)?;

    // The endpoint the *exporter* will use, for the port sanity check. When it
    // comes from the environment we leave the actual value to the builder's own
    // env handling (standard OTel semantics) and only inspect it here.
    let effective_endpoint = env_endpoint
        .clone()
        .or_else(|| config_endpoint.map(|ep| normalize_endpoint(transport, ep)));
    let mut warnings = Vec::new();
    if let Some(ref endpoint) = effective_endpoint
        && let Some(warning) = port_mismatch_warning(transport, endpoint)
    {
        warnings.push(warning);
    }

    let endpoint_source = match (&env_endpoint, &effective_endpoint) {
        (Some(env_ep), _) => format!("env: {env_ep}"),
        (None, Some(endpoint)) => format!("config: {endpoint}"),
        // Unreachable: the guard above already returned for "no endpoint
        // configured anywhere". Kept so the arm structure stays total.
        (None, None) => return Ok(None),
    };

    // `with_endpoint` is applied only for the config source: for the env
    // source the builder reads the variables itself, which is what keeps the
    // standard semantics (`OTEL_EXPORTER_OTLP_ENDPOINT` is a base URL that
    // gets `/v1/traces` appended on http/protobuf and is used as-is on gRPC;
    // the TRACES variant is verbatim on both).
    let builder_endpoint =
        if env_endpoint.is_some() { None } else { effective_endpoint.as_deref() };

    let service_name = std::env::var("OTEL_SERVICE_NAME")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "ephpm".to_string());

    let resource =
        opentelemetry_sdk::Resource::builder().with_service_name(service_name.clone()).build();

    // The two transports produce different exporter types, so each branch
    // builds its own provider and they converge on `SdkTracerProvider`.
    let (provider, grpc_runtime, transport_description) = match transport {
        Transport::Grpc => {
            let (exporter, runtime, description) = build_grpc_exporter(builder_endpoint)?;
            let provider = SdkTracerProvider::builder()
                .with_batch_exporter(exporter)
                .with_resource(resource)
                .build();
            (provider, Some(runtime), description)
        }
        Transport::HttpBinary => {
            use opentelemetry_otlp::{WithExportConfig as _, WithHttpConfig as _};

            let (http_client, description) = build_http_client()?;
            let mut builder = opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .with_http_client(http_client)
                .with_protocol(opentelemetry_otlp::Protocol::HttpBinary);
            if let Some(endpoint) = builder_endpoint {
                builder = builder.with_endpoint(endpoint);
            }
            let exporter = builder.build().context("failed to build the OTLP span exporter")?;
            let provider = SdkTracerProvider::builder()
                .with_batch_exporter(exporter)
                .with_resource(resource)
                .build();
            (provider, None, description)
        }
    };

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
        format!("{endpoint_source} (service.name = {service_name}; {transport_description})");
    let info = OtlpStartupInfo { protocol: transport.as_str(), description, warnings };
    Ok(Some((layer, OtlpGuard { provider, _grpc_runtime: grpc_runtime }, info)))
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

    /// Every protocol spelling the OTel spec allows, plus the rejections.
    ///
    /// Pure over `parse_transport` rather than the env-reading wrapper: the
    /// test suite shares one process, so mutating `OTEL_EXPORTER_OTLP_*` here
    /// would leak into any sibling test that builds an exporter.
    #[test]
    fn protocol_values_map_to_transports() {
        let var = "OTEL_EXPORTER_OTLP_PROTOCOL";
        assert_eq!(parse_transport(var, "grpc").unwrap(), Transport::Grpc);
        assert_eq!(parse_transport(var, "http/protobuf").unwrap(), Transport::HttpBinary);
        // Case and surrounding whitespace are not the operator's problem.
        assert_eq!(parse_transport(var, "  GRPC \n").unwrap(), Transport::Grpc);

        // http/json is a real OTLP protocol we deliberately do not implement,
        // so it must say so rather than silently exporting over another one.
        let err = parse_transport(var, "http/json").unwrap_err().to_string();
        assert!(err.contains("not supported"), "unexpected error: {err}");
        assert!(err.contains("http/protobuf"), "error must name the alternatives: {err}");

        let err = parse_transport(var, "gprc").unwrap_err().to_string();
        assert!(err.contains("not a valid OTLP protocol"), "unexpected error: {err}");
    }

    /// The config knob is honoured, and an invalid one still errors.
    ///
    /// Only the config path is exercised here: asserting that
    /// `OTEL_EXPORTER_OTLP_PROTOCOL` beats it would require setting a
    /// process-global variable that any concurrently running test could
    /// observe. The precedence itself is a one-line `or_else` in
    /// `resolve_transport`, and the env-vs-config order is verified end to end
    /// in the lab rather than here.
    #[test]
    fn config_protocol_is_used_when_no_env_var_is_set() {
        // The env vars are not set in a normal test run; if a future test
        // leaks one, this assertion is what will catch it.
        for var in ["OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "OTEL_EXPORTER_OTLP_PROTOCOL"] {
            assert!(
                std::env::var(var).is_err(),
                "{var} leaked into the test process; this test cannot be trusted"
            );
        }

        assert_eq!(resolve_transport(Some("grpc")).unwrap(), Transport::Grpc);
        assert_eq!(resolve_transport(Some("http/protobuf")).unwrap(), Transport::HttpBinary);
        // Unset and empty both mean "the default", never an error.
        assert_eq!(resolve_transport(None).unwrap(), Transport::HttpBinary);
        assert_eq!(resolve_transport(Some("   ")).unwrap(), Transport::HttpBinary);

        let err = resolve_transport(Some("http/json")).unwrap_err().to_string();
        assert!(err.contains("otlp_protocol"), "error must name the config key: {err}");
    }

    /// gRPC takes a base URL; http/protobuf takes a signal URL.
    ///
    /// Appending `/v1/traces` on the gRPC path is silent — the collector
    /// simply never sees a recognised method — so the asymmetry is pinned.
    #[test]
    fn endpoint_normalization_differs_by_transport() {
        let base = "http://127.0.0.1:4317";
        assert_eq!(normalize_endpoint(Transport::Grpc, base), base);
        assert_eq!(normalize_endpoint(Transport::Grpc, "http://127.0.0.1:4317/"), base);

        assert_eq!(
            normalize_endpoint(Transport::HttpBinary, "http://127.0.0.1:4318"),
            "http://127.0.0.1:4318/v1/traces"
        );
        assert_eq!(
            normalize_endpoint(Transport::HttpBinary, "http://127.0.0.1:4318/"),
            "http://127.0.0.1:4318/v1/traces"
        );
        // Already-suffixed values must not be doubled.
        assert_eq!(
            normalize_endpoint(Transport::HttpBinary, "http://127.0.0.1:4318/v1/traces"),
            "http://127.0.0.1:4318/v1/traces"
        );
    }

    /// The 4317/4318 mix-up must produce a message, and nothing else must.
    #[test]
    fn port_mismatch_is_reported_only_when_it_is_a_mismatch() {
        let warning = port_mismatch_warning(Transport::Grpc, "http://127.0.0.1:4318")
            .expect("gRPC pointed at 4318 must warn");
        assert!(warning.contains("4318"), "warning must name the port: {warning}");
        assert!(warning.contains("grpc"), "warning must name the configured protocol: {warning}");

        let warning =
            port_mismatch_warning(Transport::HttpBinary, "http://127.0.0.1:4317/v1/traces")
                .expect("http/protobuf pointed at 4317 must warn");
        assert!(warning.contains("http/protobuf"), "unexpected warning: {warning}");

        // Matching ports, and any unrelated port, are silent — an operator is
        // free to run either transport anywhere.
        assert!(port_mismatch_warning(Transport::Grpc, "http://127.0.0.1:4317").is_none());
        assert!(
            port_mismatch_warning(Transport::HttpBinary, "http://127.0.0.1:4318/v1/traces")
                .is_none()
        );
        assert!(port_mismatch_warning(Transport::Grpc, "http://collector:9999").is_none());
        // A port-less endpoint has nothing to compare.
        assert!(port_mismatch_warning(Transport::Grpc, "https://collector.example").is_none());
    }

    /// The gRPC exporter must build offline, with its own runtime.
    ///
    /// This is the guard on the crypto-provider wiring: with
    /// `tls-provider-agnostic` and nothing installed as the process default,
    /// tonic falls through to a bare `ClientConfig::builder()` and **panics**
    /// in a binary that has had two providers linked (#241). Building the
    /// exporter is enough to exercise that path because tonic's
    /// `connect_lazy` constructs the TLS connector eagerly.
    #[test]
    fn grpc_exporter_builds_with_its_own_runtime() {
        let (_exporter, runtime, description) =
            build_grpc_exporter(Some("http://127.0.0.1:4317")).expect("build gRPC exporter");

        assert!(description.contains("trust anchors"), "unexpected description: {description}");
        // A foreign provider would mean gRPC TLS silently diverges from the
        // HTTPS listener; the description carries that warning when it happens.
        assert!(
            !description.contains("already installed"),
            "another crypto provider won the process default: {description}"
        );

        // The runtime is real and usable — this is what drives every export.
        assert_eq!(runtime.block_on(async { 1 + 1 }), 2);
    }

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
