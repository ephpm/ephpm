+++
title = "Introduction"
weight = 1
+++

ePHPm is an all-in-one PHP application server written in Rust. It embeds PHP via FFI into a single binary alongside an HTTP server, MySQL/SQLite database layer, KV store, gossip clustering, and ACME TLS. The same binary runs on your laptop, in CI, and in production.

This section answers *what is ePHPm* and *why does it exist*. If you want to actually run something, jump to [Getting Started](/docs/getting-started/).

- **[Comparison](comparison/)** — feature parity vs FrankenPHP, RoadRunner, Swoole, Apache, Nginx.
- **[Feature Status](feature-status/)** — implemented today vs planned.
