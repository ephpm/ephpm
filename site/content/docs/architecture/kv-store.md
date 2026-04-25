+++
title = "KV Store"
weight = 6
+++

> **Stub** — not yet written. The implementation lives in `crates/ephpm-kv/`.

## What this will cover

- Why a built-in KV (vs requiring an external Redis)
- DashMap-backed string and hash entries
- RESP2 protocol compatibility (so `phpredis` / `predis` connect unchanged)
- TTL and expiry sweeps
- Transparent compression (gzip / zstd / brotli) with size threshold
- Eviction (LRU + random)
- Multi-tenant isolation (per-vhost passwords)
- Clustered mode: gossip-tier for small values, TCP data plane for large, hot-key promotion
