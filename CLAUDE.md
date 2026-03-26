# ePHPm — Embedded PHP Manager

An all-in-one PHP application server written in Rust that embeds PHP via FFI into a single binary. Runs WordPress, Laravel, etc. without external PHP-FPM.

## Build & Run

```bash
# Stub mode (no PHP, fast iteration on HTTP/routing logic)
cargo build

# Release binary with PHP linked (requires php CLI + composer installed)
cargo xtask release           # → target/release/ephpm (PHP 8.5)
cargo xtask release 8.4       # → target/release/ephpm (PHP 8.4)

# Windows .exe (cross-compiled from WSL, requires cargo-xwin)
cargo install cargo-xwin
cargo xtask release --target windows       # → target/x86_64-pc-windows-msvc/release/ephpm.exe
cargo xtask release --target windows 8.4   # with specific PHP version
```

Prerequisites for `cargo xtask release`: php CLI 8.2+, composer, git, and C build tools (autoconf, cmake, make, etc.). The xtask handles cloning and running static-php-cli automatically.

## Testing

```bash
cargo nextest run --workspace          # unit + integration tests
cargo clippy --workspace --all-targets -- -D warnings  # lint (pedantic, warnings = errors)
cargo +nightly fmt --all -- --check    # format check (nightly required for import grouping)
cargo deny check                       # license/advisory audit
```

IMPORTANT: Run single tests when possible, not the full suite. Use `cargo nextest run -p <crate> <test_name>`.

## Workspace Structure

| Crate | Purpose |
|-------|---------|
| `ephpm` | CLI binary — clap args, config loading, server startup, graceful shutdown |
| `ephpm-server` | HTTP server (hyper + tokio + tower) — routing, static file serving |
| `ephpm-php` | PHP embedding via FFI — SAPI implementation, request/response mapping |
| `ephpm-config` | Configuration (figment) — TOML + env var overrides (`EPHPM_` prefix) |
| `xtask` | Build & test tooling — `release`, `php-sdk`, `e2e`, `e2e-up`, `e2e-down` |

## Critical Conventions

- **Conditional compilation**: All PHP FFI code is gated with `#[cfg(php_linked)]`. The `php_linked` cfg is set by `ephpm-php/build.rs` when `PHP_SDK_PATH` env var is present. Stub mode must always compile and pass tests without it.
- **C wrapper required**: PHP uses setjmp/longjmp for error handling. Never call PHP functions directly from Rust without going through `ephpm_wrapper.c` and its `zend_try/zend_catch` guards — otherwise SIGSEGV.
- **NTS PHP (current MVP)**: PHP runtime is non-thread-safe. A global `Mutex<Option<PhpRuntime>>` serializes all PHP execution. Async HTTP connections are handled by tokio, but PHP calls use `spawn_blocking`.
- **MSRV**: Rust 1.85 — do not use features from newer editions without checking.
- **Clippy**: Pedantic + all warnings denied (`-D warnings`). Zero warnings policy.
- **Rustfmt**: 2024 edition style, `group_imports = "StdExternalCrate"`. Requires **nightly** toolchain (`cargo +nightly fmt`).
- **Error handling**: `thiserror` for domain errors, `anyhow` for propagation with context. Always add context to errors with `.context()`.
- **Logging**: `tracing` crate. Use appropriate levels — debug for request details, info for lifecycle events, warn/error for problems.

## Code Style

- Crate names: `ephpm-*` (kebab-case)
- Safety comments before every `unsafe` block explaining FFI invariants
- Public API documentation with `///` on all exported items
- Module-level docs with `//!` explaining purpose and design

## CI Pipeline

Runs on push/PR to main: fmt check → clippy → nextest → cargo-deny. Release builds triggered by `v*` tags across PHP 8.4/8.5 × Linux/macOS matrix.
