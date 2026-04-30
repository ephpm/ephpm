+++
title = "Feature Status"
weight = 2
+++

What ships in ePHPm today vs what's on the roadmap.

| Feature | Status |
|---------|--------|
| HTTP/1.1 + HTTP/2 serving | **Implemented** |
| Static file serving | **Implemented** |
| PHP embedding (ZTS) | **Implemented** |
| Request routing (pretty permalinks) | **Implemented** |
| Configuration (TOML + env vars) | **Implemented** |
| Embedded KV store (strings, TTL, counters) | **Implemented** |
| KV store value compression (gzip/zstd/brotli) | **Implemented** |
| KV store CLI debugging (`ephpm kv`) | **Implemented** |
| SAPI functions (`ephpm_kv_*` in PHP) | **Implemented** |
| Prometheus metrics + query stats | **Implemented** |
| Gossip clustering (SWIM via chitchat) | **Implemented** |
| Embedded SQLite — single-node (litewire + rusqlite) | **Implemented** |
| Embedded SQLite — clustered HA (litewire + sqld) | **Implemented** |
| TLS (manual cert/key + ACME/Let's Encrypt) | **Implemented** |
| Virtual hosts (directory-based, multi-tenant) | **Implemented** |
| Admin UI / API | Planned |
| OpenTelemetry export | Planned |

For longer-term direction, see the [Roadmap](/docs/roadmap/).
