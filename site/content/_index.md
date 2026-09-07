+++
title = "ePHPm"
toc = false
type = "docs"

[cascade]
  type = "docs"
+++

**A PHP application server that embeds the runtime and its dependencies in a single binary.**

ePHPm is an application server for PHP, written in Rust. The Zend engine is compiled in as a static library and executed via FFI on the server's own worker threads — there is no PHP-FPM pool and no FastCGI socket between the web server and the interpreter. HTTP is served by hyper on tokio; the PHP context lives on the same threads that accept the connection.

It is a drop-in replacement for that stack. Point it at a document root and your application runs unmodified — the same code, the same drivers, the same framework configuration. There is nothing to port and no ePHPm-specific API you have to adopt.

The services a PHP application normally reaches over a network are compiled in beside it. The database is **Turso**, the pure-Rust rewrite of SQLite, with MVCC and concurrent writers. The cache is a **DashMap**. Both live in the server process, and there are two ways to reach them.

Existing code keeps its drivers. **litewire** puts Turso behind the MySQL, PostgreSQL, Hrana and TDS wire protocols, so a `pdo_mysql` connection to `127.0.0.1:3306` works untouched, and the cache answers RESP2, so any Redis client library connects and talks the protocol it expects. Nothing has to change to run.

Or skip the protocol entirely. `ephpm_db_query()`, `ephpm_kv_get()` and `ephpm_ws_broadcast()` are native functions in the SAPI — thirty-plus of them — that call straight into the engine on the calling thread. No socket, no wire format to encode, no connection to pool. This is the fast path, and it is what the [WordPress and Laravel integrations](/guides/) use.

## Get started

```bash
docker run -p 8080:8080 ephpm/ephpm:latest
```

Or download an archive from [Releases](https://github.com/ephpm/ephpm/releases) and let the binary install itself:

```bash
sudo ./ephpm install
```

`install` places the binary, writes a default config, registers a systemd unit or launchd plist, and starts the service. There is no install script and no package repository to add.

## What's inside

| | |
|---|---|
| **PHP** | Zend engine, statically linked, thread-safe on every platform including Windows |
| **Turso** | SQLite-compatible storage engine, MVCC, no external process |
| **litewire** | MySQL / PostgreSQL / Hrana / TDS wire protocols translated to SQL against Turso |
| **DashMap** | in-process key-value store over RESP2 — PSR-16/PSR-6 cache, sessions, object cache |
| **SAPI bridge** | `ephpm_db_*`, `ephpm_kv_*`, `ephpm_ws_*` — native functions that bypass the wire protocols entirely |
| **rustls** | TLS with ACME issuance, including DNS-01 wildcards across five providers |
| **chitchat** | SWIM gossip for membership, failure detection and replication |
| **hyper / tokio** | HTTP/1.1 and HTTP/2, async I/O, per-vhost routing |

Multi-tenant hosting gives every virtual host its own Turso database, cache keyspace and session state. Native WebSockets are held by the server and dispatched to PHP per event. Two execution modes: a worker mode that boots your framework once, and a per-request mode that behaves like PHP-FPM.

## Preview environments

Open a pull request and it deploys as a running site at its own hostname, with its own database, cache keyspace and session state — the same isolation every virtual host gets. Merge or close the PR and the site is removed, database included.

Two pieces do it: a daemon that fetches the branch, builds it and installs it as a vhost, and a small PHP application that receives the GitHub webhooks — itself running on ePHPm. You run both on your own cluster; there is no hosted service.

[PR previews for your app →](/guides/pr-previews/) · [Running the preview bot →](/guides/preview-bot/)

## Runs

WordPress · Laravel · Symfony · any PSR-15 application

[Get Started →](/getting-started/)
[Architecture →](/architecture/)
[Credits →](/credits/)
[GitHub](https://github.com/ephpm/ephpm)
