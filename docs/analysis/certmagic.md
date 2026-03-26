# TLS & ACME: From CertMagic (Go) to Rust Equivalents

## Background: CertMagic

CertMagic (`github.com/caddyserver/certmagic`) is the Go library that powers Caddy's automatic HTTPS. It was originally considered for ePHPm when the project was Go-based. With the decision to build ePHPm in Rust, CertMagic is no longer a dependency — but its design principles informed the Rust approach.

**What CertMagic does (for reference):**
- ACME account creation
- Domain ownership verification (HTTP-01, TLS-ALPN-01, or DNS-01 challenges)
- Certificate issuance from Let's Encrypt (or any ACME CA)
- Automatic renewal (starting 30 days before expiry)
- OCSP stapling
- HTTP-to-HTTPS redirect
- Pluggable certificate storage (filesystem, Redis, etcd, S3)

**Underlying Go libraries:**
- **ACMEz** (`github.com/mholt/acmez`) — pure-Go, RFC 8555 ACME client
- **libdns** — pluggable DNS provider interface for DNS-01 challenges (Cloudflare, Route53, etc.)

---

## Rust Equivalents

The Rust ecosystem has mature equivalents for every layer of CertMagic's stack.

### TLS: rustls

| | CertMagic (Go) | rustls (Rust) |
|---|---|---|
| Library | `crypto/tls` (Go stdlib) + CertMagic | `rustls` |
| OpenSSL dependency | No (pure Go) | No (pure Rust) |
| TLS 1.2 / 1.3 | Yes | Yes |
| FIPS compliance | Via BoringSSL | Via `aws-lc-rs` backend |
| Post-quantum | Caddy v2.10+ (x25519mlkem768) | Via `rustls` post-quantum feature |
| OCSP stapling | CertMagic handles it | `rustls` supports it, manual or via ACME crate |
| Stars / maturity | N/A (stdlib) | `rustls`: 6.2k stars, actively maintained, used by Cloudflare (Pingora), AWS, Deno |

`rustls` is the standard Rust TLS library. No OpenSSL dependency, memory-safe, audited. Used in production by Cloudflare's Pingora proxy framework, AWS SDK, Deno runtime, and hundreds of other projects.

### ACME: Three Options

#### 1. `rustls-acme` (Recommended for ePHPm)

- **Stars:** 184 | **Downloads:** ~15k/month | **License:** Apache-2.0 OR MIT
- **What it does:** Full automatic HTTPS — accepts TLS connections, handles ACME challenges inline, manages cert renewal. "Like CertMagic but for Rust."
- **Challenge types:** TLS-ALPN-01 (default, recommended) and HTTP-01
- **Runtime:** Agnostic — works with Tokio, async-std, or any runtime
- **Storage:** Pluggable via `Cache` trait. Default `DirCache` for filesystem. ePHPm implements this trait against the clustered KV store.
- **Why it fits:** Provides the same "one function call for automatic HTTPS" experience as CertMagic. Handles the full lifecycle (account, challenges, issuance, renewal, serving) without spawning background tasks.

```rust
use rustls_acme::AcmeConfig;
use tokio_stream::StreamExt;

let mut acme_state = AcmeConfig::new(["example.com"])
    .contact(["mailto:admin@example.com"])
    .cache(EphpmKvCache::new(kv_store))  // backed by clustered KV
    .directory_lets_encrypt(true)
    .state();

let rustls_config = acme_state.default_rustls_config();
let acceptor = acme_state.axum_acceptor(Arc::new(rustls_config));

// Spawn cert management as a background task
tokio::spawn(async move {
    while let Some(event) = acme_state.next().await {
        match event {
            Ok(ok) => log::info!("ACME event: {:?}", ok),
            Err(err) => log::error!("ACME error: {:?}", err),
        }
    }
});
```

#### 2. `instant-acme` (Lower-level alternative)

- **Stars:** 195 | **Downloads:** ~66k/month | **License:** Apache-2.0
- **What it does:** Pure ACME client — handles the protocol (account creation, orders, challenges, finalization) but does NOT manage TLS serving or renewal. You wire it up yourself.
- **Challenge types:** All (HTTP-01, DNS-01, TLS-ALPN-01) — you implement the challenge response
- **Used in production** at Instant Domain Search
- **Why you'd use it:** If you need DNS-01 challenges (wildcard certs) or want full control over the ACME flow. More work to integrate, but more flexible.

#### 3. `tokio-rustls-acme` (Tokio-specific fork)

- **Stars:** 40 | **Downloads:** smaller | **License:** Apache-2.0 OR MIT
- **What it does:** Fork of `rustls-acme` specifically for Tokio. Actively maintained (last push Mar 2026) by n0-computer (Iroh/IPFS team).
- **Why you'd use it:** If you want the `rustls-acme` experience but tightly coupled to Tokio (which ePHPm uses). May be simpler integration.

### Recommendation for ePHPm

**Primary: `rustls-acme`** for the core automatic HTTPS flow (TLS-ALPN-01 and HTTP-01 challenges). This covers 90% of use cases — single-domain and multi-domain certs with zero config.

**Secondary: `instant-acme`** for DNS-01 challenge support (needed for wildcard certs like `*.example.com`). DNS-01 requires calling DNS provider APIs (Cloudflare, Route53, etc.) to create TXT records. The Rust ecosystem doesn't have a `libdns` equivalent (Go's pluggable DNS provider library), so ePHPm would need thin wrappers for each provider:

```rust
/// DNS provider trait for DNS-01 challenges
trait DnsProvider: Send + Sync {
    async fn create_txt_record(&self, domain: &str, value: &str) -> Result<()>;
    async fn delete_txt_record(&self, domain: &str, value: &str) -> Result<()>;
}

/// Cloudflare implementation
struct CloudflareDns { api_token: String, zone_id: String }

/// Route53 implementation
struct Route53Dns { /* AWS credentials */ }
```

This is ~100-200 lines per provider. Start with Cloudflare (most common), add others based on demand.

---

## CertMagic vs. Rust Stack Comparison

| Feature | CertMagic (Go) | ePHPm Rust Stack |
|---|---|---|
| Automatic HTTPS | One function call | `rustls-acme` — same experience |
| TLS library | Go `crypto/tls` | `rustls` (pure Rust, no OpenSSL) |
| ACME protocol | Built-in (via ACMEz) | `rustls-acme` + `instant-acme` |
| HTTP-01 challenge | Yes | Yes (`rustls-acme`) |
| TLS-ALPN-01 challenge | Yes | Yes (`rustls-acme`, default) |
| DNS-01 challenge | Yes (via libdns providers) | `instant-acme` + custom provider wrappers |
| Wildcard certs | Yes | Yes (via DNS-01) |
| OCSP stapling | Yes | Yes (`rustls`) |
| Cert storage | Pluggable (`certmagic.Storage`) | Pluggable (`Cache` trait) → backed by clustered KV |
| Cert sharing across nodes | Via external storage | Via clustered KV — built-in, no external deps |
| Let's Encrypt | Yes | Yes |
| ZeroSSL / other CAs | Yes | Yes (any ACME-compatible CA) |
| On-Demand TLS | Yes | Implementable (issue cert on first connection) |
| Dependency on server | None (standalone library) | None (standalone crates) |

### Key Advantage Over CertMagic

CertMagic's `certmagic.Storage` interface requires an external backend for multi-node cert sharing (Redis, etcd, S3, etc.). Additionally, CertMagic's `Locker` interface requires an external distributed lock (etcd, Consul, DynamoDB, etc.) to prevent multiple nodes from requesting certs simultaneously.

ePHPm replaces both with the **built-in clustered KV store**:

| CertMagic needs | ePHPm equivalent | External dependency? |
|---|---|---|
| `Storage` (cert persistence) | KV store (`certs:*` keys) | **No** — built-in |
| `Locker` (distributed lock) | KV store (`acme:lock:*` keys with TTL) | **No** — built-in |
| Challenge token sharing | KV store (`acme:challenge:*` keys) | **No** — built-in |
| Leader election for renewal | KV store (`acme:leader` key with TTL heartbeat) | **No** — built-in |

Certificates, locks, challenge tokens, and leader election all replicate to all nodes automatically via gossip. Zero external dependencies for clustered HTTPS.

See `architecture.md` → "TLS Certificate Management in a Cluster" for the full HA design covering race conditions, challenge routing, and renewal stampede prevention.
