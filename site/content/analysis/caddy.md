# Caddy Server

- **Language:** Go
- **Architecture:** General-purpose web server and reverse proxy. Foundation that FrankenPHP builds upon.
- **Maturity:** Very high. v2.11.x (early 2026). Production-grade, widely deployed. 70.8k GitHub stars.
- **License:** Apache 2.0

---

## What Caddy Does Well

- Best-in-class automatic TLS: zero-config ACME, auto-renewal, OCSP stapling, ECH, post-quantum key exchange (x25519mlkem768 default as of v2.10)
- On-Demand TLS (cert per first connection)
- Support for Let's Encrypt and ZeroSSL as default CAs
- ACME profiles for 6-day certs
- Prometheus/OpenMetrics endpoint
- Extensible via Go modules (how FrankenPHP integrates)

**Does not execute PHP on its own** — requires FrankenPHP module or reverse proxy to FPM/RoadRunner.

---

## Relevance to ePHPm

Caddy was the original candidate for ePHPm's HTTP/TLS layer (see architecture options A and C in [ephpm-architecture.md](../architecture/ephpm-architecture.md)). Both were rejected:

- **Option A (Caddy module):** Caddy owns `main()` and the CLI. ePHPm's scope extends far beyond HTTP serving.
- **Option C (embed Caddy as library):** Caddy was designed to own the process. Its maintainers describe library usage as "unwieldy."

With the Rust decision, Caddy is no longer relevant as a dependency. The features ePHPm needs from Caddy's stack are available natively in Rust:

| Caddy Feature | Rust Equivalent |
|---|---|
| HTTP/1.1 + HTTP/2 | `hyper` + `tokio` |
| HTTP/3 (QUIC) | `quinn` |
| TLS | `rustls` (no OpenSSL dependency, pure Rust) |
| Automatic ACME / Let's Encrypt | `rustls-acme` or `instant-acme` (see [certmagic.md](certmagic.md)) |
| Reverse proxy | Custom implementation on `hyper` |
| Caddyfile config | `toml` / `serde` (ePHPm's own config format) |

### What We Learned from Caddy

Caddy's architecture validated several principles ePHPm follows:

1. **Automatic HTTPS should be zero-config.** Caddy proved that developers will adopt a server that just handles TLS. ePHPm must match this — no manual cert management.
2. **Single binary distribution works.** Caddy's single binary (with xcaddy for plugins) showed that ops teams prefer `scp binary && run` over package managers and config file sprawl.
3. **CertMagic as a separable library was the right design.** The fact that Caddy's TLS was extracted as a standalone Go library (`certmagic`) validates the principle of layered architecture. ePHPm follows the same principle in Rust — `rustls` + `rustls-acme` are independent libraries, not tied to any server.
