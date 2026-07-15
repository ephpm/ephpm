# SSE & Realtime — Hypermedia Push on ePHPm

> **Status:** the two enabling primitives — `ephpm_kv_wait()` and
> streaming brotli compression — **shipped** (PRs
> [#183](https://github.com/ephpm/ephpm/pull/183) and
> [#184](https://github.com/ephpm/ephpm/pull/184)). The SSE hub
> described in the second half is **Planned — not yet implemented**,
> targeted at v0.6.0.

## Why realtime is suddenly ePHPm-shaped

Hypermedia frameworks built on Server-Sent Events —
[Datastar](https://data-star.dev), htmx SSE, Turbo Streams — turn the
backend into a render-and-push loop: state changes, the server
re-renders a fragment, every connected browser morphs it into the DOM.
That workload is awkward on classic PHP-FPM (no long-lived connections,
no shared state, an external Redis for pub/sub, a proxy that buffers)
and almost native on ePHPm: worker mode already carries long-lived
streamed responses (`send_response_stream`), and the in-process KV
store already gives every worker thread the same state at ~100 ns per
op. The Pixelboard demo
([ephpm/datastar-demo](https://github.com/ephpm/datastar-demo)) proved
a real multiplayer Datastar app runs on a stock v0.5.0 binary with no
server changes at all.

Two gaps kept it from being *great*. Both are now closed:

## Shipped: `ephpm_kv_wait()` — push, not poll

Before: the only fan-out pattern was version polling — every SSE
connection's worker polled a KV version key every ~100 ms
(latency floor = poll interval, idle CPU ∝ connected clients).

Now: `ephpm_kv_wait(string $key, int $last_version, int $timeout_ms)`
blocks the worker until the key is written (sub-millisecond wakeup) or
the timeout fires (keepalive tick). Idle cost is zero for the waiting
connections *and* zero for KV writers that nobody is waiting on — the
write path pays a single atomic load until the first wait in the
process. See the [KV guide](/guides/kv-from-php/) for semantics and the
SSE loop idiom.

## Shipped: streaming brotli for SSE

Before: streamed worker responses always went out identity-encoded, so
a Datastar "fat re-render" paid its full size on the wire every event.

Now: `[server.response] compression_streaming = "sse"` wraps
`text/event-stream` responses in a single brotli encoder whose window
persists for the stream's lifetime, flushed per event so every event
decodes the moment it arrives. Because successive re-renders of the
same elements are nearly identical, event N compresses against events
N−1, N−2, … and collapses to a small delta — the demo's ~5 KB grid
re-render drops to tens of bytes after the first event. Default is
`"off"`, which keeps the streamed path byte-for-byte identical to
v0.5.0. See the [configuration reference](/reference/config/).

## Planned — not yet implemented: the SSE hub (v0.6.0 target)

The remaining constraint is structural: **one SSE connection parks one
worker thread** for its whole lifetime, so `[php] worker_count` caps
concurrent viewers, and short action requests compete with streams for
the same pool. Fine for tens-to-hundreds of clients; wrong shape for
thousands of dashboard viewers who all see the *same* rendered
fragments.

A PHP execution context is thread-bound, so "detaching" a stream from
its worker is off the table. The design that fits ePHPm's architecture
is a **server-side SSE hub**:

- **Rust owns the connections.** A new SAPI surface (sketch:
  `ephpm_sse_publish(topic, event)` plus a router-level topic-subscribe
  endpoint) lets hyper hold the N client connections directly — no
  worker parked per viewer.
- **Render once, fan out.** On a KV change (via the same watch
  machinery behind `ephpm_kv_wait`), the hub asks *one* worker to
  render the fragment once, then broadcasts the resulting bytes to all
  N subscribers — through the per-connection streaming brotli encoders
  shipped above.
- **Viewers decouple from `worker_count`.** Worker threads go back to
  being a render pool; connection count becomes a hyper/file-descriptor
  problem, which Rust is good at.

Open design questions (why this is a page and not a PR): topic
registry lifecycle and auth, per-subscriber backpressure policy (drop
vs. coalesce vs. disconnect slow readers), event replay/last-event-id
semantics, and how per-vhost isolation maps onto topics in
multi-tenant mode. Nothing below this heading exists in the code today.
