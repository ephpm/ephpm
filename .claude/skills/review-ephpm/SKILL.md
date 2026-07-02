---
name: review-ephpm
description: Repo-specific review lens for ephpm PRs - the FFI, threading, platform, and CI landmines a generic code review misses. Use when reviewing any PR touching ephpm-php, ephpm_wrapper.c, ephpm-server, config, workflows, or docs.
---

# ephpm PR review lens

Apply ON TOP of a normal correctness review. Each item has bitten this repo at least once.

## PHP embedding / wrapper (ephpm_wrapper.c, ephpm-php)

- **Per-request lifecycle invariants** (violating any caused real SIGSEGVs or cross-request leaks):
  - `SG(request_info)` fields set BEFORE `php_request_startup()`; startup owns superglobal construction. Never hand-rebuild `$_GET`/`$_POST`/`$_COOKIE` (`php_default_treat_data` on startup-built arrays = use-after-free).
  - `SG(server_context)` must be non-NULL before startup or `$_POST` silently stays empty.
  - Per-request resets after startup: `http_response_code = 200`, `headers_sent`, `no_headers`, `PG(last_error_type)` - or values leak across reused threads.
  - Per-request INI (open_basedir) must be BUFFERED and replayed AFTER startup at `STAGE_ACTIVATE` (shutdown's `zend_ini_deactivate` wipes anything applied before; RUNTIME stage rejects non-tightening open_basedir). Buffered strings need `malloc` copies (Zend per-request allocator resets across the cycle).
- All PHP calls go through the C wrapper's `zend_try`/SETJMP guards - never call PHP functions directly from Rust.
- Stub mode (`cargo build` without `PHP_SDK_PATH`) must compile and pass tests - all FFI behind `#[cfg(php_linked)]`.
- New SAPI userland functions must be registered via the MINIT shim (`ephpm_pre_init`) - post-init registration is invisible to new ZTS threads.
- Thread-local (`EPHPM_TLS`) for all per-request C state; ZTS on Linux/macOS, **NTS on Windows** (single PHP context - watch for concurrency assumptions).

## Concurrency / server

- **Never cap or block the shared tokio blocking pool to limit a subsystem** - it also runs static-file I/O. Use a scoped semaphore (the `php.workers` lesson). Remember: blocking tasks cannot be cancelled; anything slot-based leaks slots past the request 504.
- Config knobs: see the `add-config-knob` skill - reject parsed-but-unused fields.
- Section-absent serde defaults: check behavior when the whole TOML section is missing, not just the field (the `[server.security]` default-off lesson).

## Platform

- `--wrap` linker no-ops (zend signals/timeout/stack) are **Linux-only**; macOS ld64 and MSVC have no equivalent. Don't rely on them cross-platform.
- Windows: static-linked `php8embed.lib` (no DLL), `/FORCE:MULTIPLE` hack, sqld unsupported, clustered SQLite bails.
- macOS release builds pin libclang to brew `llvm@17` - a step that does `brew ... || true` will mask a missing toolchain as green.
- **ASCII-only in `.github/workflows/*`** touching PowerShell - em-dashes break the PS 5.1 tokenizer on Server Core. Grep `[^\x00-\x7F]` before approving.

## Docs & truthfulness (see CLAUDE.md "Truthfulness" section)

- Behavior/default/mechanism changes must update `site/content/`, `examples/`, `README.md` in the same PR - grep for the old claim.
- No security/durability claims without implementation; no aspirational content in `reference/`/`guides/`.
- `blocked_paths` examples need the leading `/`; KV TTL args are seconds (RESP `PEXPIRE` is the ms exception); `ephpm_kv_incr` is 1-arg (deltas = `incr_by`).

## Process

- Preserve outside contributors' authorship: prefer building on their branch/commits (merge-commit if squash would erase attribution - ask the owner).
- `ephpm-e2e` is workspace-excluded - don't add it to workspace ops; its Cargo.lock drifts by design.
- Gates: clippy pedantic `-D warnings`, `cargo +nightly fmt`, cargo-deny, MSRV 1.85.
