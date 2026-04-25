+++
title = "Comparison"
weight = 1
+++

How ePHPm compares to other ways of running PHP on a server.

| | ePHPm | FrankenPHP | RoadRunner | Swoole | Apache + mod_php | Nginx + php-fpm |
|---|---|---|---|---|---|---|
| Language | Rust | Go (CGO) | Go | PHP + C | C | C + PHP |
| PHP FFI overhead | Zero (native C call) | ~2.2μs/req (11+ CGO crossings) | N/A (worker mode) | N/A (native) | N/A (in-process) | IPC (FastCGI) |
| Server GC pauses | None | Go GC | Go GC | None | None | None |
| Binary | Single static binary | Caddy module | Go binary + PHP workers | PHP + extension | Apache + modules | Nginx + separate FPM |
| DB proxy + connection pooling | Built-in (MySQL wire, R/W split) | No | No | No | No | No |
| Embedded DB | SQLite via litewire | No | No | No | No | No |
| Built-in KV store | Yes (RESP compatible, in-process) | No | No | No | No | No |
| Query stats (Prometheus) | Built-in | No | No | No | No | No |
| Auto TLS (ACME) | Built-in | Via Caddy | No | No | No | No |
| Clustering | Gossip (SWIM) | No | No | Built-in | No | No |
| Virtual hosts | Built-in (directory-based) | Via Caddy | No | No | `<VirtualHost>` | `server` blocks |
| PHP compatibility | Drop-in (embed SAPI) | Drop-in (worker SAPI) | Requires PSR-7 packages | Requires async code | Native (100%) | Native (100%) |
| Deployment | Single binary | Requires Caddy | Multi-process | Requires PHP + Swoole extension | Apache + modules | Separate services |
| Container-friendly | ✓ (single binary) | ✓ (Caddy module) | ✓ | ⚠️ (PHP + extension) | ⚠️ (heavier) | ⚠️ (two services) |

For a deeper look at each alternative, see the [Analysis](/docs/analysis/) section.
