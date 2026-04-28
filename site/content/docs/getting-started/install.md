+++
title = "Install"
weight = 1
+++

## Linux / macOS

```bash
curl -fsSL https://raw.githubusercontent.com/ephpm/ephpm/main/install.sh | sh
```

Installs the binary, creates a default config at `/etc/ephpm/ephpm.toml`, sets up a systemd service, and starts serving on `http://your-server:8080`.

Variants:

```bash
# Pin to a specific version
curl -fsSL https://raw.githubusercontent.com/ephpm/ephpm/main/install.sh | EPHPM_VERSION=0.1.0 sh

# Binary only (no systemd unit, no config)
curl -fsSL https://raw.githubusercontent.com/ephpm/ephpm/main/install.sh | sh -s -- --no-systemd --no-config

# Uninstall
curl -fsSL https://raw.githubusercontent.com/ephpm/ephpm/main/install.sh | sh -s -- --uninstall
```

## Windows

PowerShell, run as Administrator:

```powershell
irm https://raw.githubusercontent.com/ephpm/ephpm/main/install.ps1 | iex
```

Installs to `C:\Program Files\ephpm\`, adds to `PATH`, creates a Windows service, and starts serving.

```powershell
# Binary only (no service)
irm https://raw.githubusercontent.com/ephpm/ephpm/main/install.ps1 | iex -Args "--no-service"

# Uninstall
irm https://raw.githubusercontent.com/ephpm/ephpm/main/install.ps1 | iex -Args "--uninstall"
```

> Clustered SQLite (sqld) isn't available on Windows — Turso doesn't publish a Windows binary. Single-node SQLite, the MySQL/Postgres proxy, and everything else work normally.

## Build from source

For contributors or custom builds. Requires Rust 1.85+.

```bash
# Stub mode — no PHP, fast iteration on HTTP/routing logic
cargo build
cargo run -- --config ephpm.toml
```

```bash
# Release binary with PHP embedded.
# Prerequisites: php-cli 8.2+, composer, git, build-essential, autoconf, cmake,
# pkg-config, re2c, libssl-dev (libssl-devel/openssl-devel on RHEL/Fedora).
cargo xtask release       # → target/release/ephpm
cargo xtask release 8.4   # use PHP 8.4 instead of 8.5
```

On Windows, `cargo xtask release` re-invokes itself inside WSL automatically (the PHP SDK build needs a Unix toolchain). Cross-compiled `.exe` builds work via [`cargo-xwin`](https://github.com/rust-cross/cargo-xwin):

```bash
cargo install cargo-xwin
cargo xtask release --target windows
```

The first `cargo xtask release` is slow (it builds a fully static PHP via [static-php-cli](https://github.com/crazywhalecc/static-php-cli) — about 15 minutes). It's cached at `php-sdk/static-php-cli/buildroot/`; delete that to force a rebuild.

## Verify

```bash
ephpm --version
ephpm --help
```
