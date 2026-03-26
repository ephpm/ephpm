# ePHPm Project Memory

## Project Overview
- Cargo workspace: 4 crates (ephpm, ephpm-config, ephpm-php, ephpm-server)
- Edition 2024, MSRV 1.85, resolver = "3"
- Virtual manifest in root Cargo.toml

## Crate Structure
- `ephpm` (binary): clap CLI, tokio main, wires config+php+server
- `ephpm-config`: figment + serde, TOML loading, EPHPM_ env prefix
- `ephpm-php`: FFI via bindgen + cc (ephpm_wrapper.c), `#[cfg(php_linked)]` gate, Mutex<Option<PhpRuntime>>
- `ephpm-server`: hyper 1.x HTTP/1.1, router with PHP/static dispatch, spawn_blocking for PHP

## Build System
- `PHP_SDK_PATH` env var triggers php_linked cfg in build.rs
- C wrapper (ephpm_wrapper.c) compiled via `cc` crate for zend_try/zend_catch safety
- static-php-cli builds libphp.a with selected extensions
- Stub mode compiles and tests without PHP SDK

## Key Patterns
- Error handling: thiserror for domain errors, anyhow for propagation
- Logging: tracing crate
- Clippy: pedantic + all = warn, -D warnings in CI
- Rustfmt: nightly required for style_edition=2024, group_imports=StdExternalCrate
- INI overrides stored as Vec<[String; 2]> (not Vec<(String, String)> as docs suggest)

## CI/CD
- GitHub Actions: fmt (nightly) -> clippy -> nextest (ubuntu+macos) -> cargo-deny
- Release: v* tags, PHP 8.3/8.4 x ubuntu/macos matrix via static-php-cli

## Documentation
- Extensive analysis docs in docs/analysis/ (competitor research)
- Architecture docs in docs/architecture/ (architecture.md, cli.md, implementation.md)
- No README.md exists yet
