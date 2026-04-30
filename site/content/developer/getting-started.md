# Getting Started

Everything you need to set up a development environment for ePHPm.

---

## Prerequisites

### Rust (required)

ePHPm requires **Rust 1.85+** (edition 2024). Install via [rustup](https://rustup.rs):

**Linux / macOS / WSL:**

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

**Windows:**

Download and run the installer from https://rustup.rs. You'll also need the [Visual Studio C++ Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) (select "Desktop development with C++").

```bash
# Verify installation
rustc --version   # should be >= 1.85
cargo --version
```

The project includes a `rust-toolchain.toml` that pins the stable channel, so rustup will automatically install the correct version when you first build.

### Nightly Rust (required for formatting)

The formatter uses 2024-edition features (`group_imports`, `imports_granularity`) that require nightly:

```bash
rustup toolchain install nightly
```

You only need nightly for `cargo +nightly fmt`. All other commands use stable.

### cargo-nextest (recommended)

The project uses [nextest](https://nexte.st) as its test runner for faster, more readable output:

```bash
cargo install cargo-nextest --locked
```

### cargo-deny (recommended)

Used for dependency license and advisory auditing:

```bash
cargo install cargo-deny --locked
```

---

## Building

### Stub mode (no PHP)

This compiles the HTTP server and routing logic without linking PHP. Fast iteration, works everywhere, no container engine needed:

```bash
cargo build
```

The binary will serve static files and return a placeholder response for `.php` routes. This is the default development workflow — most HTTP/routing work doesn't need PHP linked.

### Full build with PHP (xtask)

The `cargo xtask release` command builds the static PHP SDK via [static-php-cli](https://github.com/crazywhalecc/static-php-cli) and then compiles the release binary with PHP linked. The first build takes ~15 minutes; subsequent builds are cached.

**Linux / macOS:**

Install the prerequisites, then run xtask:

```bash
# Prerequisites (Ubuntu/Debian)
sudo apt install php-cli composer git build-essential autoconf cmake pkg-config re2c

# Prerequisites (macOS)
brew install php composer autoconf cmake pkg-config re2c

# Build
cargo xtask release          # PHP 8.5 (default)
cargo xtask release 8.4      # PHP 8.4

# Binary is at target/release/ephpm
```

**Windows (via WSL):**

Building the PHP SDK requires a Unix C toolchain (autoconf, make, gcc). On Windows, the xtask **automatically detects Windows and re-invokes itself inside WSL** — you just need WSL set up with the right tools.

1. **Install WSL and Ubuntu** (if you haven't already):

   ```powershell
   # Run in PowerShell as Administrator
   wsl --install
   ```

   This installs WSL 2 and [Ubuntu from the Microsoft Store](https://apps.microsoft.com/detail/9PDXGNCFSCZV). Restart your machine when prompted, then launch Ubuntu from the Start menu to finish setup (username + password).

2. **Install Rust inside WSL:**

   ```bash
   # Run inside WSL (launch "Ubuntu" from Start menu, or type `wsl` in terminal)
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   source "$HOME/.cargo/env"
   ```

3. **Install build dependencies inside WSL:**

   ```bash
   sudo apt update && sudo apt install -y \
       php-cli composer git build-essential \
       autoconf cmake pkg-config re2c libclang-dev
   ```

4. **Run xtask from your Windows terminal:**

   ```bash
   cargo xtask release
   ```

   The xtask detects Windows, calls `wsl -- bash -c 'cargo xtask release'` automatically, and builds inside WSL. Your project directory is shared between Windows and WSL via `/mnt/c/...`, so the output binary lands in your normal `target/release/` directory.

You can also build the PHP SDK separately without building the binary:

```bash
cargo xtask php-sdk
# Then build locally against the SDK
PHP_SDK_PATH=./php-sdk/static-php-cli/buildroot cargo build --release
```

### Full build with PHP (container)

Alternatively, build everything inside a container without any local toolchain setup:

```bash
# Build a container image with the binary inside
podman build -f docker/Dockerfile --build-arg PHP_VERSION=8.5 -t ephpm:latest .

# Run it
podman run --rm -p 8080:8080 ephpm:latest
```

This requires [Podman](https://podman.io/installation) (or Docker — substitute `docker` for `podman`).

- Windows: `winget install RedHat.Podman`
- macOS: `brew install podman`
- Linux: Available in most distro package managers

---

## Testing

```bash
# Run all tests
cargo nextest run --workspace

# Run a single crate's tests (preferred)
cargo nextest run -p ephpm-server

# Run a single test by name
cargo nextest run -p ephpm-server test_name

# Lint (pedantic, zero warnings)
cargo clippy --workspace --all-targets -- -D warnings

# Format check (requires nightly)
cargo +nightly fmt --all -- --check

# Format fix
cargo +nightly fmt --all

# Dependency audit
cargo deny check
```

---

## Project Layout

```
ephpm/
├── crates/
│   ├── ephpm/           # CLI binary (clap, config loading, server boot)
│   ├── ephpm-server/    # HTTP server (hyper + tokio)
│   ├── ephpm-php/       # PHP FFI embedding (SAPI, request/response)
│   └── ephpm-config/    # Configuration (figment, TOML + env vars)
├── xtask/               # Build tooling (cargo xtask release / php-sdk)
├── docker/
│   ├── Dockerfile           # Full multi-stage build (PHP SDK + Rust binary)
│   └── Dockerfile.php-sdk   # PHP SDK only (extract for local builds)
├── docs/
│   ├── analysis/        # Competitive analysis
│   ├── architecture/    # Architecture decisions, security model
│   └── developer/       # You are here
└── tests/               # Integration tests
```

---

## IDE Setup

### VS Code / Cursor

Install [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer). The workspace will be detected automatically from `Cargo.toml`.

### IntelliJ / RustRover

Install the Rust plugin or use [RustRover](https://www.jetbrains.com/rust/). Open the project root — the workspace manifest will be detected.

### cfg visibility

In stub mode (no `PHP_SDK_PATH`), code inside `#[cfg(php_linked)]` blocks will be grayed out by rust-analyzer. This is expected — that code only compiles when the PHP SDK is present.
