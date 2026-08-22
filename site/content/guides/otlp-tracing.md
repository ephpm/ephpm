+++
title = "Distributed Tracing with OTLP (Local Dev Loop)"
weight = 20
+++

ePHPm can export a span per HTTP request to any OpenTelemetry collector over
OTLP **http/protobuf**. This guide sets up a local trace UI, points ePHPm at
it, and covers what your IDE can and cannot do for you.

Set expectations first, because "tracing" means different things:

- ePHPm exports **two or three spans per request** describing its own request
  handling. It does **not** auto-instrument your PHP code — there are no
  spans for individual functions, database queries or HTTP clients. For
  per-query visibility use [Query Stats with
  Prometheus](/guides/query-stats-prometheus/), which is a different and
  more detailed tool.
- ePHPm is a trace **producer**, not a collector. It cannot receive OTLP from
  your PHP OpenTelemetry SDK. (An in-process OTLP receiver appears in
  `/architecture/` as a design goal — it is **not implemented**.)

What you do get is the piece nothing else can give you: where the time in a
request actually went, split between ePHPm's own handling and PHP execution,
stitched into a trace that continues from whatever called you.

## What ePHPm exports

| Span | Emitted | Attributes |
|------|---------|------------|
| `http.request` | every request | `http.request.method`, `url.path`, `http.response.status_code` |
| `php.execute` | requests that run PHP | none |
| `worker.queue_wait` | worker mode only | none |

`php.execute` and `worker.queue_wait` are children of `http.request`. On the
default fpm path there is no dispatch queue, so you get a two-span tree; in
worker mode (`[php] mode = "worker"`) `worker.queue_wait` appears as a sibling
of `php.execute` and measures how long the request waited for a free worker.
A static file or a 404 produces `http.request` alone.

Note what is **not** on a span: no `Host` header, no query string, and no
site/vhost identifier. Query strings routinely carry tokens, so their absence
is deliberate. The consequence in multi-tenant mode (`[server] sites_dir`) is
that traces from different vhosts are currently indistinguishable — see
[Known rough edges](#known-rough-edges).

## Step 1: run a collector

Two options. Both were verified against ePHPm while writing this guide.

### Jaeger — a real trace UI, zero configuration

Jaeger v2 listens for OTLP on 4317 (gRPC) and 4318 (http/protobuf) out of the
box and serves its UI on 16686. No environment variables, no config file:

```bash
docker run --rm --name jaeger \
  -p 16686:16686 -p 4317:4317 -p 4318:4318 \
  cr.jaegertracing.io/jaegertracing/jaeger:2.20.0
```

Storage is in-memory and is lost when the container stops, which is exactly
what you want for a dev loop.

### OpenTelemetry Collector — when you want the raw spans

No UI, but it will write every span to a file as JSON, which is the fastest
way to answer "what exactly is ePHPm sending?". Save as `otelcol.yaml`:

```yaml
receivers:
  otlp:
    protocols:
      http:
        endpoint: 0.0.0.0:4318

exporters:
  file:
    path: /out/spans.json
  debug:
    verbosity: basic

service:
  pipelines:
    traces:
      receivers: [otlp]
      exporters: [file, debug]
```

```bash
docker run --rm -p 4318:4318 \
  -v "$PWD/otelcol.yaml:/etc/otelcol/config.yaml" -v "$PWD/out:/out" \
  otel/opentelemetry-collector-contrib:0.135.0 --config /etc/otelcol/config.yaml
```

You can also run both: add an `otlphttp` exporter pointing at Jaeger and get
the UI and the JSON from one pipeline.

## Step 2: point ePHPm at it

```toml
[server.diagnostics]
otlp_endpoint = "http://127.0.0.1:4318"
```

`/v1/traces` is appended when missing, so the base URL is enough. Then:

```bash
ephpm serve --config ephpm.toml
```

Startup confirms it, and this line is your first checkpoint — if it is absent,
nothing downstream matters:

```text
INFO ephpm: OTLP trace export enabled (http/protobuf)
     endpoint=config: http://127.0.0.1:4318/v1/traces (service.name = ephpm; ...)
```

Drive a request or two, wait a few seconds for the batch to flush, and open
<http://localhost:16686>.

### Environment variables

The standard OTel variables work and **take precedence over the config knob**:

| Variable | Effect |
|----------|--------|
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | Used verbatim. Highest precedence. |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Base URL; `/v1/traces` appended. Beats the config knob. |
| `OTEL_SERVICE_NAME` | Service name in the trace UI. Default `ephpm`. |
| `OTEL_TRACES_SAMPLER` / `OTEL_TRACES_SAMPLER_ARG` | Sampler. Default `parentbased_always_on`. |
| `OTEL_EXPORTER_OTLP_TRACES_TIMEOUT` / `OTEL_EXPORTER_OTLP_TIMEOUT` | Export timeout in ms. Default `10000`. |

Set `OTEL_SERVICE_NAME` per project — it is what separates your app from
everything else in the UI's service dropdown.

### Do you need to build from source?

**No.** Official release binaries and the Docker image are built with the
`otlp` cargo feature (`cargo xtask release` enables it). A plain
`cargo build` does **not** — if you are running a locally compiled binary and
see this at startup, that is why:

```text
WARN ephpm: OTLP trace export requested ... but this binary was built without
     the `otlp` cargo feature — no spans are exported.
```

Rebuild with `cargo build --release --features otlp`, or use a release binary.

The feature is inert until an endpoint is configured: with `otlp_endpoint`
unset, no exporter is built and no background thread is spawned (measured —
thread count is identical to a binary without the feature).

## Step 3: continue a trace from upstream

ePHPm honours the W3C `traceparent` header. If a proxy, a browser SDK, or an
upstream service sends one, ePHPm's `http.request` span becomes a child of
that span instead of a new root — the trace continues across the hop:

```bash
curl -H 'traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01' \
     http://localhost:8080/
```

That request lands in the UI under trace `4bf92f35…`, parented to span
`00f067aa0ba902b7`.

**The sampled flag is obeyed.** A `traceparent` ending in `-00` marks the
trace as not sampled, and ePHPm exports **nothing** for that request. This is
correct behaviour and the most common cause of "my traces stopped appearing
after I put a load balancer in front" — the symptom is indistinguishable from
a broken exporter.

## HTTPS collectors

An `https://` endpoint works with no extra configuration. TLS is rustls (the
same crypto provider the HTTPS listener uses), and the trust anchors are the
union of the OS trust store and a bundled Mozilla root set.

For a collector behind a private CA, point the standard `SSL_CERT_FILE` (a PEM
bundle) or `SSL_CERT_DIR` at it. Note that doing so **replaces** the OS trust
store rather than adding to it, so the bundle must contain every CA the
exporter needs. There is no ePHPm-specific trust-store setting. See the
[`otlp_endpoint` reference](/reference/config/#serverdiagnostics) for the full
description.

## PhpStorm

### The in-IDE trace viewer, and why it needs a bridge

JetBrains ships a **free, first-party** plugin called
[OpenTelemetry](https://plugins.jetbrains.com/plugin/27488-opentelemetry)
(plugin ID 27488) that is listed as compatible with **PhpStorm 2026.2+**. It
is a genuine trace viewer: a Traces tab, and *"Double-click a trace, or select
it and click Examine Trace, to open its span hierarchy."*

There is a catch that matters here. Per
[JetBrains' documentation](https://www.jetbrains.com/help/rider/OpenTelemetry.html):
*"The built-in receiver accepts OTLP over gRPC."* **ePHPm only speaks OTLP
http/protobuf**, so it cannot feed the plugin directly. To use it you need a
local OpenTelemetry Collector receiving http/protobuf on 4318 and re-exporting
over gRPC to the plugin's port (enable *Use fixed OTLP server port* in
Settings → Tools → OpenTelemetry; the documented default is 17011):

```yaml
receivers:
  otlp:
    protocols:
      http:
        endpoint: 0.0.0.0:4318
exporters:
  otlp/ide:
    endpoint: localhost:17011
    tls:
      insecure: true
service:
  pipelines:
    traces:
      receivers: [otlp]
      exporters: [otlp/ide]
```

Also note the plugin has no PHP module — its automatic
`OTEL_EXPORTER_OTLP_*` injection covers other languages' run configurations,
not PHP. For ePHPm you set the endpoint in `ephpm.toml` anyway, so this costs
you nothing.

If you would rather not run the bridge, use Jaeger in a browser tab. That is
the simpler setup and loses nothing but the tab.

### What PhpStorm contributes either way

- A **Docker Compose run configuration** for your collector stack, so
  `docker-compose.otel.yml` starts from the Run menu.
- A **compound run configuration** that starts the collector and ePHPm
  together as one action.
- A **"Before launch → Run Another Configuration"** step on the ePHPm
  configuration, so the collector is always up first.

A minimal `docker-compose.otel.yml` to point those at:

```yaml
services:
  jaeger:
    image: cr.jaegertracing.io/jaegertracing/jaeger:2.20.0
    ports: ["16686:16686", "4318:4318"]
```

## VS Code

**There is no credible in-editor OTLP trace viewer for VS Code.** As of this
writing the marketplace has OpenTelemetry Collector *config* extensions (YAML
schema and OTTL support), vendor extensions that deep-link into a hosted
product, and a handful of hobby projects with two-digit install counts. None
of them is something to build a workflow on. The trace UI lives in a browser
tab; VS Code's job is starting things and getting you there.

`tasks.json` — bring the collector up, and tear it down:

```json
{
  "version": "2.0.0",
  "tasks": [
    {
      "label": "otel: up",
      "type": "shell",
      "command": "docker compose -f docker-compose.otel.yml up -d",
      "problemMatcher": []
    },
    {
      "label": "otel: down",
      "type": "shell",
      "command": "docker compose -f docker-compose.otel.yml down",
      "problemMatcher": []
    }
  ]
}
```

`launch.json` — gate the app on that task with `preLaunchTask`:

```json
{
  "version": "0.2.0",
  "configurations": [
    {
      "name": "ephpm serve (traced)",
      "type": "node-terminal",
      "request": "launch",
      "command": "ephpm serve --config ephpm.toml",
      "preLaunchTask": "otel: up"
    }
  ]
}
```

Two more genuinely useful bits:

- **Simple Browser** (`Ctrl/Cmd+Shift+P` → *Simple Browser: Show*) renders
  <http://localhost:16686> in an editor tab, which is as close to an in-editor
  trace viewer as VS Code gets.
- If you develop in a **Dev Container**, add the UI port to
  `forwardPorts` in `devcontainer.json` so the link works from the host:

  ```json
  { "forwardPorts": [8080, 16686] }
  ```

Xdebug and this are complementary, not alternatives: Xdebug tells you what a
single request did line by line; a trace tells you which requests are worth
opening Xdebug on.

## Known rough edges

Verified behaviour you should know about before you go hunting for a bug in
your own setup.

**Spans are lost on a hard kill.** A graceful shutdown (`SIGTERM`, `Ctrl+C`)
flushes the batch queue before exiting — verified. `SIGKILL` does not, and the
last few seconds of spans are gone. If your last request never shows up, check
how you stopped the server before you check anything else.

**Export failures are silent.** If the endpoint is wrong, unreachable, or
presents a certificate ePHPm does not trust, nothing is logged — the startup
line still says export is enabled and then the server goes quiet. Check the
collector's own log. Tracked in
[#378](https://github.com/ephpm/ephpm/issues/378).

**Spans are `SPAN_KIND_INTERNAL`, including `http.request`.** Backends that
build service graphs or RED metrics from span kind will not recognise ePHPm as
a server. A 5xx response also leaves the span status `UNSET`, so failed
requests are not highlighted as errors — the `http.response.status_code`
attribute is correct, but nothing else marks them. Tracked in
[#379](https://github.com/ephpm/ephpm/issues/379).

**No per-tenant attribution.** In multi-site mode every span looks the same
regardless of which vhost served the request. Tracked in
[#380](https://github.com/ephpm/ephpm/issues/380).

## See also

- [`[server.diagnostics]` config reference](/reference/config/#serverdiagnostics)
- [Query Stats with Prometheus](/guides/query-stats-prometheus/) — per-query
  timing and error rates, which tracing deliberately does not cover
- `GET /_ephpm/requests` — the last 256 requests as JSON, with the same
  total/queue-wait/PHP split, and no collector required
  ([`request_log`](/reference/config/#serverdiagnostics))
