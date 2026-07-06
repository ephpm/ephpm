+++
title = "Install"
weight = 1
+++

ePHPm ships as a single binary that manages itself. There's no install script — the binary registers and controls its own system service. For trying it out without touching the host, a Docker image is also published.

## Docker

```bash
docker run -p 8080:8080 ephpm/ephpm:latest
```

That starts ePHPm with default settings on `http://localhost:8080`. Mount your document root at `/var/www/html` and your config at `/etc/ephpm/ephpm.toml` to serve a real site:

```bash
docker run -p 8080:8080 \
  -v /path/to/site:/var/www/html \
  -v /path/to/ephpm.toml:/etc/ephpm/ephpm.toml \
  ephpm/ephpm:latest
```

### Tags

| Tag | What it tracks |
|-----|----------------|
| `ephpm/ephpm:latest` | Rolling latest release with the default PHP minor |
| `ephpm/ephpm:8.5` / `ephpm/ephpm:8.4` | Rolling latest release pinned to a PHP minor |
| `ephpm/ephpm:vX.Y.Z` | Pinned ePHPm release with the default PHP minor |
| `ephpm/ephpm:vX.Y.Z-php8.5` | Pinned release × rolling PHP minor |
| `ephpm/ephpm:vX.Y.Z-php8.5.7` | Pinned release × pinned PHP patch (fully reproducible) |

Real SemVer build metadata uses `+` (`v0.2.0+php8.5.7`), but OCI tags reject `+`, so Docker tags substitute `-` while the upstream `+` form is preserved on each image's `org.opencontainers.image.version` label — the same trade-off k3s and rke2 make.

For the standalone binary install path (single-binary, self-installing, no container runtime needed), grab an archive from [Releases](https://github.com/ephpm/ephpm/releases) and continue with the Linux / macOS or Windows section below.

## Linux / macOS

Download the latest binary from [Releases](https://github.com/ephpm/ephpm/releases) and unpack it, then run:

```bash
sudo ./ephpm install
```

`install` copies the binary to `/usr/local/bin/ephpm`, writes a default config to `/etc/ephpm/ephpm.toml`, registers a systemd service (Linux) or launchd plist (macOS), and starts it. By default the server listens on `http://localhost:8080`.

## Windows

Download the Windows `.tar.gz` archive from [Releases](https://github.com/ephpm/ephpm/releases) and extract `ephpm.exe` from it. In an Administrator PowerShell:

```powershell
.\ephpm.exe install
```

Installs to `C:\Program Files\ephpm\`, adds the directory to the system `PATH`, registers a Windows service, and starts it.

> Clustered SQLite (sqld) isn't available on Windows — Turso doesn't publish a Windows binary. Single-node SQLite, the MySQL/Postgres proxy, and everything else work normally.

## Manage the service

After `install`, the same commands work on every platform — they wrap systemd / launchd / the Windows service controller:

```bash
sudo ephpm start          # start the service
sudo ephpm stop           # stop the service
sudo ephpm restart        # restart (after editing the config)
sudo ephpm status         # PID, uptime, last exit code, listen address
sudo ephpm logs           # tail the service log
sudo ephpm logs --follow  # follow new log lines
```

To run the server in the foreground without registering a service (useful for debugging):

```bash
sudo ephpm serve --config /etc/ephpm/ephpm.toml
```

## Uninstall

```bash
sudo ephpm uninstall
```

Stops the service, removes the binary, the service unit, and `/var/lib/ephpm/`. Pass `--keep-data` to preserve the config file and any SQLite databases:

```bash
sudo ephpm uninstall --keep-data
```

## Build from source

For contributors or custom builds. Requires Rust 1.85+.

```bash
# Stub mode — no PHP, fast iteration on HTTP/routing logic
cargo build
cargo run -- serve --config ephpm.toml
```

```bash
# Release binary with PHP embedded.
# Prerequisites: git, curl, tar, build-essential, pkg-config, libclang-dev.
cargo xtask release       # → target/release/ephpm
cargo xtask release 8.4   # use PHP 8.4 instead of 8.5
```

`cargo xtask release` doesn't build PHP locally — it downloads a prebuilt PHP SDK (`libphp.a` plus headers; on Linux the glibc-linked `-gnu` variant) from [github.com/ephpm/php-sdk](https://github.com/ephpm/php-sdk) releases. No PHP CLI, Composer, or static-php-cli toolchain is required. The SDK is cached at `php-sdk/<version>-<os>-<arch>[-gnu]/`; delete that directory to force a re-download. On Linux the result is a single glibc-dynamic binary (gnu target) that can load shared PHP extensions and middleware via `dlopen`.

Windows builds run natively (no cross-compile, no `cargo-xwin`) — you just need the MSVC build tools:

```powershell
cargo xtask release --target windows   # → target\x86_64-pc-windows-msvc\release\ephpm.exe
```

The SDK download is the same php-sdk release, with prebuilt `php8embed.dll`/`.lib` for Windows.

A binary built from source can also self-install:

```bash
sudo ./target/release/ephpm install
```

## Verify

```bash
ephpm --version
ephpm --help
ephpm status
```
