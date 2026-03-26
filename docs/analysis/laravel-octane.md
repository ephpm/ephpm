
# Laravel Octane

- **Language:** PHP (Composer package)
- **What it is:** NOT a server. An adapter layer between Laravel and persistent server backends.
- **Supports:** FrankenPHP, RoadRunner, Swoole/OpenSwoole

---

## What It Does

- Hides server-specific protocol differences (worker loops, request/response translation)
- Manages per-request state reset (auth, session, translator, DB connections)
- Provides `Octane::table()` (backed by Swoole Table or RoadRunner KV)
- Concurrent task execution, periodic tasks

---

## What It Does NOT Do

- Fix application-level singleton/static state leaks — developer's responsibility
- Provide any infrastructure features (TLS, metrics, pooling) — delegates to the backend server

---

## Performance Benchmarks (Apple M1 Pro)

- FrankenPHP: ~0.88ms median request
- RoadRunner: ~2.61ms
- Swoole: ~4.94ms
