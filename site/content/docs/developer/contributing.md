+++
title = "Contributing"
weight = 4
+++

Contributions are welcome. The bar is high but the path is short — most changes follow the same shape.

## Prerequisites

- **Rust 1.85+** via [rustup](https://rustup.rs). On Windows, also install [C++ Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/).
- **Nightly toolchain** — `rustup toolchain install nightly` (used for `cargo +nightly fmt` only)
- **cargo-nextest** — `cargo install cargo-nextest --locked`
- **cargo-deny** — `cargo install cargo-deny --locked`
- **WSL + Ubuntu** (Windows only) — needed for `cargo xtask release` because the PHP SDK build needs a Unix toolchain. The xtask re-invokes itself inside WSL automatically.

See [Getting Started](../getting-started/) for the full development environment walkthrough.

## Workflow

Most development happens in **stub mode** — no PHP SDK or container engine needed:

```bash
cargo build                                                   # build
cargo nextest run -p <crate>                                  # test
cargo clippy --workspace --all-targets -- -D warnings         # lint
cargo +nightly fmt --all                                      # format
cargo deny check                                              # license/advisory audit
```

Run a single test instead of the whole suite when iterating:

```bash
cargo test -p ephpm-server my_specific_test
```

The full test suite hits OpenSSL via the e2e crate and may fail on hosts without `openssl-dev`. The `ephpm-e2e` crate is **excluded from the workspace** and runs inside Docker via `cargo xtask e2e`.

## Build & test tooling (xtask)

```bash
cargo xtask release      # build PHP SDK + ephpm binary (release mode)
cargo xtask php-sdk      # only the static PHP SDK (~15 min first time, then cached)
cargo xtask e2e-install  # download kind, tilt, kubectl into ./bin/
cargo xtask e2e          # run E2E suite (Kind + Tilt CI)
cargo xtask e2e-up       # start E2E dev environment (Tilt dashboard at :10350)
cargo xtask e2e-down     # tear down Kind cluster
cargo xtask docs serve   # run the docs site locally on :1313
cargo xtask docs build   # build the docs site to site/public/
cargo xtask docs install # download pinned hugo extended into ./bin/
```

E2E commands require Podman or Docker. `cargo xtask e2e-install` downloads kind/tilt/kubectl to `./bin/` so you don't need a global install. See [Testing](../testing/) for details.

## Code conventions

- **Clippy**: pedantic + all warnings denied. Zero warnings policy.
- **Formatting**: 2024 edition style, grouped imports (`group_imports = "StdExternalCrate"`). Run `cargo +nightly fmt --all` before pushing.
- **Error handling**: `thiserror` in library crates, `anyhow` in the binary. Always add `.context()` when propagating.
- **Logging**: `tracing` crate. Use levels appropriately — debug for request details, info for lifecycle events, warn/error for problems.
- **Unsafe code**: Add `// SAFETY:` before every `unsafe` block explaining the FFI invariants you're upholding.
- **Documentation**: `///` on all exported items, `//!` at module level explaining purpose and design.

## Critical conventions

- All PHP FFI code is gated with `#[cfg(php_linked)]`. **Stub mode must always compile and pass tests** without a PHP SDK.
- PHP uses `setjmp`/`longjmp` for error handling. Never call PHP functions directly from Rust — always go through `ephpm_wrapper.c` and its `zend_try`/`zend_catch` guards.
- Crate names are kebab-case (`ephpm-*`).
- MSRV is Rust 1.85. Don't use features from newer editions without checking.

## Pull request flow

1. Branch off `main` (`feat/...`, `fix/...`, `docs/...`, `chore/...`).
2. Make your change. Keep it focused — one PR per concern.
3. Run `cargo clippy --workspace --all-targets -- -D warnings` and `cargo +nightly fmt --all`.
4. Add or update tests for behavior changes.
5. Push, open a PR. CI runs fmt → clippy → test → cargo-deny.
6. Address review comments by pushing follow-up commits (don't force-push during review unless asked).

## Where to look

- [Workspace structure](../architecture-overview/) — repo layout, crate responsibilities
- [Architecture](/docs/architecture/) — design rationale for every component
- [Testing](../testing/) — what each test tier covers
- [GitHub Issues](https://github.com/ephpm/ephpm/issues) — open work
