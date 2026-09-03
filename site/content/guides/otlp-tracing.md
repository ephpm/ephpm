+++
title = "Distributed Tracing with OTLP (Local Dev Loop)"
weight = 20
+++

ePHPm can export a span per HTTP request to any OpenTelemetry collector over
OTLP — either **gRPC** or **http/protobuf**. This guide sets up a local trace
UI, points ePHPm at it, and covers what your IDE can and cannot do for you.

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

| Span | Kind | Emitted | Attributes |
|------|------|---------|------------|
| `http.request` | `SERVER` | every request | `http.request.method`, `url.path`, `http.response.status_code`; `error.type` on a 5xx; `ephpm.site` in multi-site mode |
| `php.execute` | `INTERNAL` | requests that run PHP | none |
| `worker.queue_wait` | `INTERNAL` | worker mode only | none |

`php.execute` and `worker.queue_wait` are children of `http.request`. On the
default per-request path no queue-wait span is emitted, so you get a two-span
tree; in worker mode (`[php] mode = "worker"`) `worker.queue_wait` appears as a
sibling of `php.execute` and measures how long the request waited for a free
worker.
A static file or a 404 produces `http.request` alone.

Attribute names follow the [OpenTelemetry HTTP semantic
conventions](https://opentelemetry.io/docs/specs/semconv/http/http-spans/)
(HTTP spans, stable since semconv **v1.23.0**). ePHPm has never used the
deprecated `http.method` / `http.url` spellings, so there is nothing to
migrate.

**Span status.** A 5xx sets the span status to `ERROR` and records
`error.type` (the status code). A **4xx does not** — semconv is explicit that
on a `SERVER` span a 4xx is left unset, because a 404 or a 401 is a normal
outcome of serving rather than a server fault. Without that rule every bot
probe would show up red.

**Span name.** `http.request`, not the semconv-preferred `{method} {route}`.
ePHPm has no route concept, so the only thing it could substitute is the raw
path — which explodes cardinality on any app with IDs in its URLs. The method
and path are both available as attributes.

Note what is **not** on a span: no `Host` header (under `server.address` or any
other name), and no query string. Both are deliberate. The `Host` header is
attacker-controlled and arrives before any tenancy decision, so what is
exported instead is `ephpm.site` — see below. Query strings routinely carry
tokens and PII; `url.path` excludes the query by construction.

### Per-tenant attribution (`ephpm.site`)

In multi-site mode (`[server] sites_dir`) the `http.request` span carries
`ephpm.site`: the **canonical site key** ePHPm resolved for the request — the
same key that selects the vhost's database file, its KV keyspace and its
`pdo_mysql` credential. Filter or group by it to get per-tenant traces,
latency and error rates out of one process.

Two properties worth knowing:

- It is the *resolved* key, never a re-spelling of the `Host` header. With
  `sites_domain_suffix = ".local"`, `Host: shop.local` and `Host: shop` both
  export `ephpm.site="shop"`, because they are one tenant.
- A request whose `Host` matched **no** vhost carries **no** `ephpm.site` at
  all. An absent attribute is the honest representation of "no tenant", and it
  keeps unknown, attacker-supplied hostnames off the wire entirely.

In single-site mode the attribute is absent — there is one tenant and
`OTEL_SERVICE_NAME` already distinguishes the deployment.

`ephpm.` is a deliberately ePHPm-owned namespace. OTel semantic conventions
have no multi-tenant attribute, and no reserved namespace (`service.*`,
`server.*`, `host.*`) means "which of this process's vhosts served this", so
squatting on one would be wrong.

## Choosing a transport

OTLP has two wire protocols and ePHPm speaks both. The choice is made at
runtime by the standard environment variable — there is no rebuild and no
cargo feature involved:

| `OTEL_EXPORTER_OTLP_PROTOCOL` | Transport | Conventional port | Endpoint form |
|---|---|---|---|
| unset (default) | http/protobuf | 4318 | base URL; `/v1/traces` appended |
| `http/protobuf` | http/protobuf | 4318 | base URL; `/v1/traces` appended |
| `grpc` | OTLP/gRPC | 4317 | base URL, **no path appended** |

`OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` overrides it for traces specifically, the
same way the `_TRACES_ENDPOINT` variable overrides the endpoint.

If you would rather keep everything in one file, there is a config knob with
the same meaning — the environment variables above win over it:

```toml
[server.diagnostics]
otlp_endpoint = "http://127.0.0.1:4317"
otlp_protocol = "grpc"
```

**Which should you use?** For a collector, either — they carry identical data.
Pick gRPC when the consumer only accepts gRPC, which is the case for
[PhpStorm's OpenTelemetry plugin](#phpstorm). Pick http/protobuf when
something between you and the collector is an HTTP proxy that does not speak
HTTP/2. `http/json` is not supported, and asking for it is a startup error
rather than a silent fallback.

**Mind the port.** 4317 and 4318 differ by one character and a collector
usually listens on both, so pointing gRPC at 4318 is the single most common
way to end up with no traces. ePHPm warns when the endpoint uses the other
transport's conventional port:

```text
WARN ephpm: OTLP endpoint http://127.0.0.1:4318 uses port 4318, the
     conventional port for http/protobuf, but the exporter is configured for
     grpc (the conventional port for which is 4317). ...
```

It is a warning, not an error — running either transport on any port is legal.

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
      grpc:
        endpoint: 0.0.0.0:4317
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
docker run --rm -p 4317:4317 -p 4318:4318 \
  -v "$PWD/otelcol.yaml:/etc/otelcol/config.yaml" -v "$PWD/out:/out" \
  otel/opentelemetry-collector-contrib:0.135.0 --config /etc/otelcol/config.yaml
```

With both receivers enabled you can switch `OTEL_EXPORTER_OTLP_PROTOCOL`
between `grpc` and `http/protobuf` (and the endpoint between 4317 and 4318)
without touching the collector.

You can also run both: add an `otlphttp` exporter pointing at Jaeger and get
the UI and the JSON from one pipeline.

## Step 2: point ePHPm at it

For **http/protobuf** (the default), a base URL is enough — `/v1/traces` is
appended when missing:

```toml
[server.diagnostics]
otlp_endpoint = "http://127.0.0.1:4318"
```

For **gRPC**, use the gRPC port and set the protocol. No path is appended,
because on gRPC the signal is the method name, not the URL:

```toml
[server.diagnostics]
otlp_endpoint = "http://127.0.0.1:4317"
```

```bash
OTEL_EXPORTER_OTLP_PROTOCOL=grpc ephpm serve --config ephpm.toml
```

Startup confirms it, and this line is your first checkpoint — if it is absent,
nothing downstream matters. Note it names the transport in use:

```text
INFO ephpm: OTLP trace export enabled
     endpoint=config: http://127.0.0.1:4317 (service.name = ephpm; ...) protocol="grpc"
```

Drive a request or two, wait a few seconds for the batch to flush, and open
<http://localhost:16686>.

### Environment variables

The standard OTel variables work and **take precedence over the config knob**:

| Variable | Effect |
|----------|--------|
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | Used verbatim. Highest precedence. |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Base URL; `/v1/traces` appended on http/protobuf, used as-is on gRPC. Beats the config knob. |
| `OTEL_EXPORTER_OTLP_PROTOCOL` | `grpc` or `http/protobuf`. Default `http/protobuf`. |
| `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` | Same, traces only. Highest precedence. |
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

Rebuild with `cargo build --release -p ephpm --features otlp`, or use a
release binary.

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

## When export fails

A wrong endpoint, an unreachable collector, or a certificate ePHPm does not
trust used to be **completely silent**: the startup line appeared, no spans
arrived, and nothing in the log said why
([#378](https://github.com/ephpm/ephpm/issues/378)). Now the failure is
reported — and deliberately rate-limited, because a collector that is down for
an hour must not produce hundreds of identical lines.

The first failed batch is loud:

```text
WARN ephpm_server::otlp: OTLP span export failed; traces are not reaching the
     collector. Requests are unaffected. Repeats are summarized rather than
     logged individually.
     error="Operation failed: ... tcp connect error ... Connection refused"
     summary_interval_secs=60
```

While it keeps failing you get one summary a minute, carrying how bad it is:

```text
WARN ephpm_server::otlp: OTLP span export is still failing
     consecutive_failures=13 failing_for_secs=60 error="..."
```

And the recovery is logged too, so "traces came back" is not something you
have to infer:

```text
INFO ephpm_server::otlp: OTLP span export recovered
     consecutive_failures=14 was_failing_for_secs=120
```

Read the `error=` value first — it distinguishes the four failure modes that
otherwise look identical from the collector side: a refused connection (wrong
port, collector down), a TLS error (`unknown certificate authority` → see
[HTTPS collectors](#https-collectors)), an HTTP status from the collector
(wrong path, or gRPC pointed at an http/protobuf receiver), and a timeout.

**Serving is never affected.** A failing exporter does not slow, block or fail
requests; export runs on the SDK's own batch thread.

## HTTPS collectors

An `https://` endpoint works with no extra configuration, on **both**
transports. TLS is rustls (the same crypto provider the HTTPS listener uses —
never OpenSSL and never a second TLS stack), and the trust anchors are the
union of the OS trust store and a bundled Mozilla root set, identically for
gRPC and http/protobuf.

For a collector behind a private CA, point the standard `SSL_CERT_FILE` (a PEM
bundle) or `SSL_CERT_DIR` at it. Note that doing so **replaces** the OS trust
store rather than adding to it, so the bundle must contain every CA the
exporter needs. There is no ePHPm-specific trust-store setting. See the
[`otlp_endpoint` reference](/reference/config/#serverdiagnostics) for the full
description.

## PhpStorm

### The in-IDE trace viewer — no bridge needed

JetBrains ships a **free, first-party** plugin called
[OpenTelemetry](https://plugins.jetbrains.com/plugin/27488-opentelemetry)
(plugin ID 27488) that is listed as compatible with **PhpStorm 2026.2+**. It
is a genuine trace viewer: a Traces tab, and *"Double-click a trace, or select
it and click Examine Trace, to open its span hierarchy."*

Per [JetBrains' documentation](https://www.jetbrains.com/help/rider/OpenTelemetry.html),
*"the built-in receiver accepts OTLP over gRPC."* ePHPm speaks gRPC, so you
can point it straight at the IDE — **no OpenTelemetry Collector in between**.

1. **Install the plugin.** Settings → Plugins → Marketplace → search
   *OpenTelemetry* (vendor JetBrains) → Install → restart.
2. **Pin the receiver port.** Settings → Tools → OpenTelemetry → enable
   *Use fixed OTLP server port*. The documented default is **17011**. Without
   this the IDE picks a fresh port per run and there is nothing stable to
   configure ePHPm against.
3. **Point ePHPm at the IDE:**

   ```toml
   [server.diagnostics]
   otlp_endpoint = "http://127.0.0.1:17011"
   ```

   ```bash
   OTEL_EXPORTER_OTLP_PROTOCOL=grpc OTEL_SERVICE_NAME=my-app \
     ephpm serve --config ephpm.toml
   ```

   `http://`, not `https://` — the IDE receiver is plaintext on loopback.

4. **Check the startup line** says `protocol="grpc"` and names port 17011.
5. **Drive a request**, wait a few seconds for the batch to flush, and open
   the **OpenTelemetry** tool window → *Traces*. Double-click a trace (or
   *Examine Trace*) to get the span hierarchy: `http.request` with
   `php.execute` — and `worker.queue_wait` too, in worker mode — nested under
   it.

Set `OTEL_SERVICE_NAME` per project; it is what the plugin labels traces with.

Note the plugin has no PHP module — its automatic `OTEL_EXPORTER_OTLP_*`
injection covers other languages' run configurations, not PHP. For ePHPm you
set the endpoint in `ephpm.toml` anyway, so this costs you nothing.

If the Traces tab stays empty, work down this list: is the startup line
present and does it say `grpc`; does it name 17011; is *Use fixed OTLP server
port* actually enabled; and is there a `WARN ... OTLP span export failed` line
in the ePHPm log (a refused connection means the IDE is not listening on that
port). See [When export fails](#when-export-fails).

Jaeger in a browser tab remains a perfectly good alternative, and is what to
fall back to if the plugin misbehaves.

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

**`busy_ns` and `idle_ns` are `0` on `php.execute` and `worker.queue_wait`.**
Those spans are created and dropped but never *entered*, so
`tracing-opentelemetry`'s busy/idle accounting sees nothing to attribute. The
span durations themselves are correct (a span records create→close), and
`http.request` carries real values. Cosmetic, but do not read the zeros as
"this took no time".

## See also

- [`[server.diagnostics]` config reference](/reference/config/#serverdiagnostics)
- [Query Stats with Prometheus](/guides/query-stats-prometheus/) — per-query
  timing and error rates, which tracing deliberately does not cover
- `GET /_ephpm/requests` — the last 256 requests as JSON, with the same
  total/queue-wait/PHP split, and no collector required
  ([`request_log`](/reference/config/#serverdiagnostics))
