+++
title = "Introduction"
type = "docs"
weight = 1
+++

ePHPm is an all-in-one PHP application server written in Rust. It embeds PHP via FFI into a single binary alongside an [HTTP server](/architecture/http/), [database layer](/architecture/database/) (MySQL/PostgreSQL-compatible wire protocol over SQLite), [KV store](/architecture/kv-store/), [gossip clustering](/architecture/clustering/), and [ACME TLS](/guides/tls-acme/). The same binary runs on your laptop, in CI, and in production.

If you want to actually run something, jump to [Getting Started](/getting-started/).

## Comparison

How ePHPm compares to other ways of running PHP with a webserver.

| | ePHPm | FrankenPHP | RoadRunner | Swoole | Apache + mod_php | Nginx + php-fpm |
|---|---|---|---|---|---|---|
| Language | Rust | Go (CGO) | Go | PHP + C | C | C |
| Dispatch to PHP | <1 μs (in-process C call) | ~2–3 μs (CGO crossings) | ~10–50 μs (IPC to worker) | <1 μs (in-process) | <1 μs (in-process) | ~50–100 μs (FastCGI socket) |
| Server GC pauses | None | Go GC | Go GC | None | None | None |
| Binary | Single static binary | Caddy module | Go binary + PHP workers | PHP + extension | Apache + modules | Nginx + separate FPM |
| DB proxy + connection pooling | Built-in (MySQL wire, R/W split) | No | No | No | No | No |
| Embedded DB | SQLite via litewire | No | No | No | No | No |
| Built-in KV store | Yes (RESP compatible, in-process) | No | No | No | No | No |
| Query stats (Prometheus) | Built-in | No | No | No | No | No |
| Auto TLS (ACME) | Built-in | Via Caddy | No | No | No | No |
| Clustering | Gossip (SWIM) | No | No | Multi-process (single node) | No | No |
| Virtual hosts | Built-in (directory-based) | Via Caddy | No | No | `<VirtualHost>` | `server` blocks |
| Install size | ~40 MB (varies by PHP extensions) | ~150 MB | ~60–70 MB (rr + PHP) | ~35–45 MB (PHP + .so) | ~50–60 MB (Apache + PHP) | ~40–50 MB (Nginx + PHP) |
| PHP compatibility | Drop-in | Drop-in | Drop-in (worker mode requires PSR-7) | Drop-in (async features require rewrite) | Native (100%) | Native (100%) |
| Deployment | Single binary | Requires Caddy | Multi-process | Requires PHP + Swoole extension | Apache + modules | Separate services |
| Container-friendly | ✓ (single binary) | ✓ (Caddy module) | ✓ | ⚠️ (PHP + extension) | ⚠️ (heavier) | ⚠️ (two services) |

For a deeper look at each alternative, see the [Analysis](/analysis/) section.
