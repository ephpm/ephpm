+++
title = "TLS / ACME"
weight = 4
+++

ePHPm has TLS built in. Two modes: bring your own cert, or have ePHPm fetch one from Let's Encrypt automatically.

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

1. Solve a HTTP-01 challenge on the same listener (so port 80 must be reachable from the public internet for issuance, OR you can serve TLS-ALPN-01 on 443).
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

### HTTP listener for HTTP-01

If you want both an HTTP (port 80) and HTTPS (port 443) listener with automatic redirect:

```toml
[server]
listen = "0.0.0.0:80"          # HTTP — also where HTTP-01 challenges land

[server.tls]
listen = "0.0.0.0:443"         # HTTPS
domains = ["example.com"]
email   = "admin@example.com"
cache_dir = "/var/lib/ephpm/certs"
redirect_http = true
```

## Clustered ACME

In a cluster, only one node should solve the challenge — the rest read the cert from the gossip-backed KV store. ePHPm does this automatically when `[cluster] enabled = true`. Each node points at the same `cache_dir` (or a shared store) and the leader publishes the cert; replicas pick it up. See [Clustering Setup](clustering-setup/).

## What's in `cache_dir`?

- The ACME account key (created on first issuance)
- Issued certificate(s) and renewal metadata
- Per-domain state for the challenge solver

Back this directory up. Losing it means re-registering with Let's Encrypt and re-issuing certs.

## See also

- [Reference → Configuration `[server.tls]`](/reference/config/)
- [Clustering setup](clustering-setup/) — TLS in multi-node deployments
