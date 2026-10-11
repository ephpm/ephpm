+++
title = "ratelimit"
weight = 1
+++

Reference for the built-in `ratelimit` middleware: fixed-window, per-client
request limiting backed by the embedded KV store. This is the page ePHPm's
cluster startup warning names. For a walkthrough of mounting middleware, see
the [Native Middleware guide](/guides/native-middleware/#ratelimit).

Source: [`crates/ephpm-middleware-builtins/src/ratelimit.rs`](https://github.com/ephpm/ephpm/blob/main/crates/ephpm-middleware-builtins/src/ratelimit.rs).

```toml
[[middleware]]
library = "ratelimit"     # also "rate-limit", "ephpm-middleware-ratelimit"
match   = "/api/*"
order   = 40
config  = { per_ip_rps = 5, burst = 10 }
```

## Configuration

| key | type | default | meaning |
|---|---|---|---|
| `per_ip_rps` | integer | **required**, > 0 | sustained requests per second per client |
| `burst` | integer | `per_ip_rps` | extra requests allowed on top of each window's allowance |
| `key_headers` | array of strings | unset | identify the client by the **first present** of these request headers (e.g. `["X-Api-Key"]`) instead of by IP |

Any other key fails startup, naming the key and the nearest accepted spelling.

## Algorithm

- Fixed **10-second** windows. Each window allows `per_ip_rps × 10 + burst`
  requests per client.
- The counter key is `mw:rl:<site>:<client>:<window>`, where `<site>` is the
  request's canonical site key (`_UNMATCHED` for a request whose `Host`
  matched no vhost — every request on a single-site node), `<client>` is the
  client identity below, and `<window>` is `unix_time / 10`.
- Each request bumps the counter with one atomic increment that sets a
  30-second TTL only when it creates the key, so a counter can never outlive
  its window by more than that.
- Over the allowance: `429 rate limit exceeded` with `Retry-After` set to the
  seconds left in the current window. PHP does not run.
- A full window's allowance can be spent instantly at a window boundary — it
  is a fixed window, not a sliding one or a token bucket.

## Client identity

The client is the value of the first `key_headers` header present on the
request, or — when none is configured or present — the request's client IP.

The client IP is decided by the router, not by this module:

- It is the **TCP peer address** of the connection,
- **unless** that peer falls inside `[server.security] trusted_proxies`
  (CIDR list, default empty). Then it is the **rightmost** address in
  `X-Forwarded-For` that is not itself a trusted proxy (if every entry is
  trusted, the leftmost).

Only `X-Forwarded-For` is consulted; `Forwarded` (RFC 7239) and `X-Real-IP`
are ignored. With no `X-Forwarded-For` header, a trusted peer is counted by
its own address.

### Behind a load balancer or reverse proxy

If ePHPm sits behind a load balancer, CDN or reverse proxy and
`trusted_proxies` is **not** set, every request's peer is the proxy, so **all
clients share one budget** — a modest `per_ip_rps` then caps the whole site.
Fix it by listing the proxy tier's addresses:

```toml
[server.security]
trusted_proxies = ["10.0.0.0/8"]   # your LB / proxy subnet — not 0.0.0.0/0
```

Only list addresses you control. A peer inside `trusted_proxies` is believed
about `X-Forwarded-For`, so trusting a range a client can connect from lets
that client choose its own IP — and therefore a fresh budget per request.
For the same reason, never put `X-Forwarded-For` (or any other header a client
sets freely) in `key_headers`: a header-keyed limit is only as strong as the
header's provenance. Keying on a credential such as `X-Api-Key` is fine when
an auth module in front of it (for example [`api-key`](/guides/native-middleware/#api-key))
rejects unknown keys.

The same resolved address is what `$_SERVER['REMOTE_ADDR']`, `ip-allowlist`,
`maintenance-mode`'s `bypass_ips` and the `[server.limits]` `per_ip_rate`
request limiter see, so setting `trusted_proxies` fixes all of them together.
(`per_ip_max_connections` is a separate, connection-level limit and is not
covered by this page.)

## Failure behaviour

**Fail-open.** If the KV increment fails (store unavailable or out of
memory), the request is allowed through and a `WARN` is logged under the
`ephpm_middleware` target:
`ratelimit: KV store unavailable — failing open for client <client>`.
A rate limiter that hard-failed would turn a KV hiccup into an outage; do not
use `ratelimit` as an access-control gate.

## Multi-tenancy

The budget is per tenant. The counter key carries the canonical site key, and
on a multi-tenant node (`[server] sites_dir`) the counter is stored in that
site's own KV keyspace, so one tenant's traffic cannot spend another tenant's
budget, and a client cannot get a fresh budget by inventing `Host` values
(all unmatched hosts share the `_UNMATCHED` bucket).

## In a cluster

With `[cluster].enabled = true` the counters live in the replicated KV store.
The increment is atomic **on the node that performs it**; its resulting value
is then replicated to the other nodes as an ordinary last-arrival-wins write
(this replication of `INCR` results shipped with #416). Consequences:

- Nodes converge on a shared count for each client and window, so a client
  whose requests are spread across nodes is limited roughly cluster-wide when
  traffic is light.
- Concurrent increments on different nodes are **not** summed: two nodes can
  each publish their own total and the later arrival wins, which undercounts.
  Under concurrent cross-node load a client can exceed the allowance — in the
  worst case up to N × the allowance across N nodes.
- Replication is asynchronous, so a burst landing on several nodes inside the
  replication delay is counted per node.

Treat the clustered limit as approximate protection, not a strict cluster-wide
quota. Pinning each client to one node at the load balancer (session
affinity by client IP) makes the per-node count exact for that client.

### The cluster startup warning

At startup, ePHPm logs this `WARN` when a mount whose `library` is exactly
`ratelimit` is loaded with `[cluster].enabled = true`:

```
[middleware] ratelimit mounted with [cluster].enabled = true — KV INCR is not
yet replicated across nodes (SET/DEL/EXPIRE now do), so rate limits are
enforced PER NODE. A client hitting N nodes gets up to N × the configured
allowance. See site/content/reference/middleware/ratelimit.md for the current
status.
```

Read it with two corrections, both known gaps in the code rather than in your
config:

- **The text is out of date.** `INCR` results *are* replicated now (see
  [In a cluster](#in-a-cluster)). "Up to N × the allowance" remains the right
  worst-case bound, but "enforced per node" understates the convergence that
  does happen.
- **It only fires for the spelling `ratelimit`.** A mount written
  `rate-limit`, `rate_limit`, `ephpm-middleware-ratelimit` or
  `ephpm_middleware_ratelimit` loads the same module, behaves identically in a
  cluster, and logs no warning.
