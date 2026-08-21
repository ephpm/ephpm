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

> Single-node SQLite (the in-process Turso engine), the MySQL/Postgres proxy, and everything else work normally on Windows. Clustered SQLite (Turso CDC replication) is untested on Windows.

### TAILCALL build — experimental, PHP 8.5 only

A release may additionally carry a second Windows archive:

```text
ephpm-vX.Y.Z+php8.5.7-windows-x86_64-tailcall.tar.gz
```

Every PHP built with MSVC — including the default ePHPm Windows binary — falls back to the interpreter's slow `CALL` VM. The `-tailcall` archive contains the same `ephpm.exe`, but with PHP 8.5 compiled by clang-cl so the interpreter is the new TAILCALL VM (`[[clang::musttail]]` + `preserve_none`), which recovers roughly the performance of the HYBRID VM that Linux (GCC) builds get. macOS is a clang build, not GCC, so it never gets HYBRID: PHP 8.5 there runs the TAILCALL VM, while 8.3/8.4 fall back to the same slow `CALL` VM.

Measured against the default MSVC Windows binary, end-to-end through ePHPm (Ryzen 9 5950X):

- **1.6–1.7x faster on CPU-bound PHP** (reference loop 2.73 ms vs 4.43 ms).
- **~3% on the Symfony demo app** — real applications are dominated by the filesystem tier, matching upstream's ~5.5% Symfony figure for the VM-kind delta. If your workload is I/O-bound, expect the small number, not the big one.

Status and caveats:

- **Experimental.** Its release-CI leg is non-gating, so a given release can ship without this archive. The MSVC binary remains the default and supported Windows build.
- **PHP 8.5 only** — the TAILCALL VM does not exist in PHP 8.3/8.4.
- Install is identical: extract and run `.\ephpm.exe install`.
- The build pipeline hard-gates on the VM kind (it disassembles `zend_vm_kind()` out of the PHP static lib and requires `ZEND_VM_KIND_TAILCALL`), so an archive named `-tailcall` cannot silently contain the CALL VM.

## Manage the service

After `install`, the same commands work on every platform — they wrap systemd / launchd / the Windows service controller:

```bash
sudo ephpm start          # start the service
sudo ephpm stop           # stop the service
sudo ephpm restart        # restart (after editing the config)
sudo ephpm status         # service name, PID, uptime, listen address, config path
sudo ephpm logs           # tail the service log
sudo ephpm logs --follow  # follow new log lines
```

To run the server in the foreground without registering a service (useful for debugging):

```bash
sudo ephpm serve --config /etc/ephpm/ephpm.toml
```

## Upgrading from 0.6.x

v0.7.0 replaced the embedded SQLite engine: the rusqlite (SQLite C engine)
backend and the sqld sidecar were removed, and the in-process
[Turso engine](/architecture/database/engines/) is now the only embedded
engine. For an upgrading node:

- **Your `.db` files open in place** — a cleanly-shut-down 0.6.x database
  (WAL or rollback journal) needs no dump/reload. **Stop the old node cleanly
  before upgrading**: a hot `-wal` left by a hard crash was not verified to
  replay through Turso.
- **`[db.sqlite] engine = "sqlite"` (or `"rusqlite"`) is now a hard startup
  error** with a migration message — remove the `engine` key or set it to
  `"turso"`. The removed `[db.sqlite.sqld]` block and `cdc_experimental` flag
  log deprecation warnings and have no effect.

Details and caveats (including non-UTF-8 `TEXT`):
[Database Engines → Opening existing `.db` files](/architecture/database/engines/#opening-existing-sqlite--rusqlite-db-files).

## Uninstall

```bash
sudo ephpm uninstall
```

Stops the service, removes the binary, the service unit, and `/var/lib/ephpm/`. Pass `--keep-data` to preserve the config file and any SQLite databases:

```bash
sudo ephpm uninstall --keep-data
```

## Build from source

For contributors or custom builds. Requires Rust 1.88+.

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

The SDK download is the same php-sdk release, with a prebuilt static `php8embed.lib` for Windows. It links statically, so `ephpm.exe` is a single self-contained binary — there is no DLL to deploy alongside it.

To build the experimental [TAILCALL variant](#tailcall-build--experimental-php-85-only) from source (PHP 8.5 only):

```powershell
cargo xtask release --target windows --variant clang
```

This downloads the clang-cl-built SDK asset (`php-sdk-<ver>-windows-x86_64-clang.tar.gz`, cached at `php-sdk/<ver>-windows-x86_64-clang/` next to the default SDK) and verifies the SDK's interpreter is the TAILCALL VM before linking. `cargo xtask php-sdk 8.5 --target windows --variant clang` fetches the SDK alone.

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
