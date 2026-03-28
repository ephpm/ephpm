# Multi-stage build for ephpm with embedded PHP.
#
# Build:
#   podman build -f docker/Dockerfile --build-arg PHP_VERSION=8.5 -t ephpm:dev .
#
# The resulting image contains only the static ephpm binary + a test docroot.

# ── Stage 1: Build ───────────────────────────────────────────────────────────

FROM ubuntu:24.04 AS builder

ARG PHP_VERSION=8.5

ENV DEBIAN_FRONTEND=noninteractive

# System build tools for static-php-cli (spc) and musl cross-compilation.
#
# The standalone spc binary bundles its own PHP — no system PHP or Composer
# is needed. `spc doctor --auto-fix` may install additional packages, so we
# keep apt metadata around (no `rm -rf /var/lib/apt/lists/*`).
#
# Key packages:
#   unzip           — required by spc for extracting downloads
#   gettext         — provides `autopoint`, checked by spc doctor
#   musl-tools      — provides musl-gcc cross-compiler
#   musl-dev        — musl development files and static libraries
#   gcc-multilib    — 32-bit support for cross-compilation
#   libstdc++-12-dev — needed for C++ standard library (musl)
#   libgcc-12-dev   — GCC runtime library (musl)
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential ca-certificates curl git pkg-config sudo \
    autoconf automake bison cmake flex gettext libtool make re2c unzip \
    libclang-dev musl-tools musl-dev gcc-multilib libstdc++-12-dev libgcc-12-dev

# Rust stable
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal

# Install musl target for static linking
RUN /usr/local/cargo/bin/rustup target add x86_64-unknown-linux-musl

# Configure musl cross-compiler environment
ENV CC_x86_64_unknown_linux_musl=musl-gcc \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_CC=musl-gcc \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc

WORKDIR /src
COPY . .

# Verify musl toolchain can be found by the build script
# This helps diagnose libgcc lookup issues early
RUN which musl-gcc && musl-gcc --version && echo "musl-gcc OK" || echo "musl-gcc WARNING"

# Pre-warm Cargo cache by building dependencies
RUN cargo fetch

# Build PHP SDK and ephpm binary via xtask
# Runs `spc doctor --auto-fix` automatically to install missing build dependencies.
# Save output for debugging if build fails.
RUN cargo xtask release 2>&1 | tee /tmp/build.log || ( \
    echo "==> Build failed. Checking diagnostic info..." && \
    find /usr -name "libgcc.a" 2>/dev/null | head -10 && \
    ls -la /usr/local/musl/lib/gcc/ 2>/dev/null || true && \
    exit 1 \
  )

# Find the built binary (target/<triple>/release/ephpm)
RUN cp $(find target -name ephpm -type f -path '*/release/*' | head -1) /ephpm && \
    file /ephpm

# ── Stage 2: Runtime ─────────────────────────────────────────────────────────

FROM alpine:3.21

RUN apk add --no-cache tini

COPY --from=builder /ephpm /usr/local/bin/ephpm

# Test docroot — baked into image for E2E testing
COPY tests/docroot/ /var/www/html/
COPY tests/ephpm-test.toml /etc/ephpm/ephpm.toml

EXPOSE 8080

ENTRYPOINT ["tini", "--"]
CMD ["ephpm", "serve", "--config", "/etc/ephpm/ephpm.toml"]
