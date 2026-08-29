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

1. Solve a TLS-ALPN-01 challenge on the HTTPS listener itself — the default challenge type. Port 443 must be reachable from the public internet for issuance; port 80 is never used for ACME. (For wildcards, use the [DNS-01 challenge](#dns-01-challenge-wildcards) instead.)
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

## DNS-01 challenge (wildcards)

TLS-ALPN-01 cannot obtain a **wildcard** certificate (`*.example.com`): the CA has no single hostname to connect to. For wildcards — and for hosts that never accept inbound TLS — set `challenge = "dns-01"`, which proves control by publishing a `_acme-challenge` TXT record through a DNS provider. Only **Cloudflare** is implemented today.

```toml
[server]
listen = "0.0.0.0:443"

[server.tls]
domains = ["*.preview.example.com", "preview.example.com"]
email   = "admin@example.com"
cache_dir = "/var/lib/ephpm/certs"
challenge = "dns-01"
dns_provider = "cloudflare"
# Prefer a file or the environment over inlining the secret:
cloudflare_api_token_file = "/run/secrets/cf-token"
# ...or: EPHPM_SERVER__TLS__CLOUDFLARE_API_TOKEN=<token>
```

The token must be a **zone-scoped Cloudflare API token** with the `Zone.DNS:Edit` permission on the zone that holds the records. If you also set `cloudflare_zone_id`, the token needs nothing more; otherwise ePHPm resolves the zone from the FQDN, which additionally needs `Zone:Read`.

**Why wildcards matter here.** Let's Encrypt limits you to 50 certificates per registered domain per week. A fleet of ephemeral preview subdomains (`pr-123.preview.example.com`, …) would burn through that quickly with per-subdomain issuance; one `*.preview.example.com` certificate covers them all under a single order.

For each order ePHPm publishes the challenge TXT records, waits for propagation, asks the CA to validate, then finalizes and retracts the records. The issued certificate is hot-swapped into the running TLS listener — no restart — and renewed automatically (~30 days before expiry). DNS-01 and TLS-ALPN-01 are mutually exclusive per server.

## Clustered ACME

In a cluster, only one node should solve the challenge — the rest read the cert from the KV store. ePHPm does this automatically when `[cluster] enabled = true`. Each node points at the same `cache_dir` (or a shared store) and the leader publishes the cert; replicas pick it up. Both challenge lanes share the same `acme:leader` election and KV distribution. See [Clustering Setup](clustering-setup/).

Certificates and ACME account material are written to the KV store's **broadcast** tier, so every node gets its own local copy. This is deliberate and distinct from an ordinary KV write: the clustered store routes values larger than `[cluster.kv] small_key_threshold` to a *sharded* tier held on `replication_factor` nodes, and a certificate chain is comfortably over that threshold. Every node terminates TLS, so every node needs the certificate — sharding it leaves the rest of the cluster unable to complete a handshake at all. A follower that missed the broadcast (it joined, or was down, after issuance) fetches the certificate from a peer on its next poll rather than waiting for the next renewal.

The two limitations below apply to the **TLS-ALPN-01** lane. The **DNS-01** lane avoids both — the challenge is answered over DNS by the leader (nothing needs to reach a specific node), and its certificate resolver is hot-swappable, so a follower installs the leader's *renewed* certificate from the KV store without a restart. That makes DNS-01 the better fit for clustered deployments.

Two limitations of the TLS-ALPN-01 lane you must plan around:

- **Challenge traffic has to reach the ACME leader.** Sharing challenge
  tokens between nodes is **not implemented**. A follower can serve
  `/.well-known/acme-challenge/<token>` out of the KV store, but nothing
  populates those keys, and the TLS-ALPN-01 challenge material lives only in
  the leader's in-memory resolver. If your load balancer can send validation
  traffic to any node, issuance will fail intermittently.
- **Followers do not pick up renewed certificates while running.** This is
  **not implemented** for TLS-ALPN-01: `rustls-acme` consults its certificate
  cache once per state machine, so a renewal published by the leader is not
  injected into a running follower. **A follower serves the certificate it
  loaded at startup until it restarts.** On a 90-day Let's Encrypt cert this
  means a rolling restart inside the renewal window, or followers will
  eventually serve an expired certificate. Watch for it. (DNS-01 does not have
  this limitation.)

## What's in `cache_dir`?

- The ACME account key (created on first issuance)
- Issued certificate(s) and renewal metadata
- Per-domain state for the challenge solver

Back this directory up. Losing it means re-registering with Let's Encrypt and re-issuing certs.

## See also

- [Reference → Configuration `[server.tls]`](/reference/config/)
- [Clustering setup](clustering-setup/) — TLS in multi-node deployments
