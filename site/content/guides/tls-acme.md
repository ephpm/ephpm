+++
title = "TLS / ACME"
weight = 4
+++

ePHPm has TLS built in. Two modes: bring your own cert, or have ePHPm fetch one from Let's Encrypt automatically.

> **Historical note — manual `cert` + `key` did not start on v0.1.0 through
> v0.6.1. Fixed in v0.6.2.**
>
> On releases up to and including v0.6.1, starting ePHPm with
> `[server.tls] cert` and `key` logged `TLS enabled (manual)` and then
> **panicked before binding a listener**:
>
> ```
> Could not automatically determine the process-level CryptoProvider
> ```
>
> The process exited with status 101 and served nothing. This was not a
> regression — it had never worked in any release; the bug dated to v0.1.0
> and was found in August 2026 while adding HTTP/3. ACME mode was unaffected.
>
> **Both modes work on v0.6.2 and newer**
> ([#243](https://github.com/ephpm/ephpm/pull/243)). If you are still on
> v0.6.1 or older, upgrade rather than working around it.

## Manual cert + key

Point at PEM-encoded files:

```toml
[server]
listen = "0.0.0.0:443"

[server.tls]
cert = "/etc/ssl/ephpm/fullchain.pem"
key  = "/etc/ssl/ephpm/privkey.pem"
```

If you also want HTTP on port 80 with an automatic redirect to HTTPS:

```toml
[server]
listen = "0.0.0.0:80"          # HTTP

[server.tls]
listen = "0.0.0.0:443"         # HTTPS — separate listener
cert = "/etc/ssl/ephpm/fullchain.pem"
key  = "/etc/ssl/ephpm/privkey.pem"
redirect_http = true           # 301 every HTTP request to its HTTPS equivalent
```

Manual mode never reaches out to the network.

## Automatic via ACME (Let's Encrypt)

Point at domains, give a contact email, and pick a cache directory:

```toml
[server]
listen = "0.0.0.0:443"

[server.tls]
domains = ["example.com", "www.example.com"]
email   = "admin@example.com"
cache_dir = "/var/lib/ephpm/certs"
```

ePHPm will:

1. Solve a TLS-ALPN-01 challenge on the HTTPS listener itself — the only challenge type implemented. Port 443 must be reachable from the public internet for issuance; port 80 is never used for ACME.
2. Save the issued certificate and account key under `cache_dir`.
3. Renew automatically before expiry.

> **Always set `cache_dir` in production.** Without it, certificates are re-fetched on every restart, which can hit Let's Encrypt's rate limit (50 certificates per registered domain per week).

### Test against staging first

Production Let's Encrypt has tight rate limits. Use the staging environment to dry-run:

```toml
[server.tls]
domains = ["example.com"]
email   = "admin@example.com"
cache_dir = "/var/lib/ephpm/certs-staging"
staging  = true                # untrusted certs, generous rate limits
```

Browsers will warn — that's expected. Once it works, drop `staging = true` and clear `cache_dir`.

### Optional HTTP listener with redirect

If you want both an HTTP (port 80) and HTTPS (port 443) listener with automatic redirect:

```toml
[server]
listen = "0.0.0.0:80"          # HTTP — serves traffic or 301-redirects, never ACME

[server.tls]
listen = "0.0.0.0:443"         # HTTPS — ACME challenges (TLS-ALPN-01) happen here
domains = ["example.com"]
email   = "admin@example.com"
cache_dir = "/var/lib/ephpm/certs"
redirect_http = true
```

The plain-HTTP listener only serves regular traffic (or 301-redirects when `redirect_http = true`). ACME challenges are always solved on the HTTPS listener via TLS-ALPN-01 — HTTP-01 is not implemented, so port 80 is never required for certificate issuance.

## Clustered ACME

In a cluster, only one node should solve the challenge — the rest read the cert from the gossip-backed KV store. ePHPm does this automatically when `[cluster] enabled = true`. Each node points at the same `cache_dir` (or a shared store) and the leader publishes the cert; replicas pick it up. See [Clustering Setup](clustering-setup/).

Two limitations you must plan around:

- **Challenge traffic has to reach the ACME leader.** Sharing challenge
  tokens between nodes is **not implemented**. A follower can serve
  `/.well-known/acme-challenge/<token>` out of the KV store, but nothing
  populates those keys, and the TLS-ALPN-01 challenge material lives only in
  the leader's in-memory resolver. If your load balancer can send validation
  traffic to any node, issuance will fail intermittently.
- **Followers do not pick up renewed certificates while running.** This is
  **not implemented**: `rustls-acme` consults its certificate cache once per
  state machine, so a renewal published by the leader is not injected into a
  running follower. **A follower serves the certificate it loaded at startup
  until it restarts.** On a 90-day Let's Encrypt cert this means a rolling
  restart inside the renewal window, or followers will eventually serve an
  expired certificate. Watch for it.

## What's in `cache_dir`?

- The ACME account key (created on first issuance)
- Issued certificate(s) and renewal metadata
- Per-domain state for the challenge solver

Back this directory up. Losing it means re-registering with Let's Encrypt and re-issuing certs.

## See also

- [Reference → Configuration `[server.tls]`](/reference/config/)
- [Clustering setup](clustering-setup/) — TLS in multi-node deployments
