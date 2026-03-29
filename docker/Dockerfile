# Multi-stage build for ephpm with embedded PHP.
#
# Cache strategy:
#   base:    System deps + Rust toolchain — changes rarely
#   php-sdk: PHP SDK via spc — only rebuilds when PHP_VERSION or xtask changes
#   builder: Cargo manifest fetch (cached until Cargo.lock changes) → full source build
#
# Build:
#   podman build -f docker/Dockerfile --build-arg PHP_VERSION=8.5 -t ephpm:dev .

# Global ARG so it can be referenced in FROM lines (each stage re-declares it to use it)
ARG PHP_VERSION=8.5

# ── Stage 1: base — system deps + Rust toolchain ─────────────────────────────

FROM ubuntu:24.04 AS base

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
RUN rustup target add x86_64-unknown-linux-musl

# Configure musl cross-compiler environment
ENV CC_x86_64_unknown_linux_musl=musl-gcc \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_CC=musl-gcc \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc

# ── Stage 2: php-sdk — rebuilt only when PHP_VERSION or xtask source changes ──

FROM base AS php-sdk

ARG PHP_VERSION

WORKDIR /src

# Workspace manifest + lock + xtask source (the only things needed to run
# `cargo xtask php-sdk`). Stub Cargo.toml files for every other workspace
# member keep the resolver happy without pulling in real source.
COPY Cargo.toml Cargo.lock ./
COPY .cargo/ .cargo/
COPY xtask/ xtask/
COPY crates/ephpm/Cargo.toml          crates/ephpm/Cargo.toml
COPY crates/ephpm-config/Cargo.toml   crates/ephpm-config/Cargo.toml
COPY crates/ephpm-server/Cargo.toml   crates/ephpm-server/Cargo.toml
COPY crates/ephpm-php/Cargo.toml      crates/ephpm-php/Cargo.toml
COPY crates/ephpm-db/Cargo.toml       crates/ephpm-db/Cargo.toml
COPY crates/ephpm-kv/Cargo.toml       crates/ephpm-kv/Cargo.toml

# Minimal stub src so cargo can parse (but not compile) workspace members
RUN mkdir -p \
        crates/ephpm/src \
        crates/ephpm-config/src \
        crates/ephpm-server/src \
        crates/ephpm-php/src \
        crates/ephpm-db/src \
        crates/ephpm-kv/src && \
    printf 'fn main() {}\n' > crates/ephpm/src/main.rs && \
    for d in ephpm-config ephpm-server ephpm-php ephpm-db ephpm-kv; do \
        touch crates/$d/src/lib.rs; \
    done

RUN cargo xtask php-sdk ${PHP_VERSION}

# ── Stage 3: builder — dep fetch layer then full source build ─────────────────

FROM base AS builder

ARG PHP_VERSION

WORKDIR /src

# Pre-built PHP SDK so xtask release skips the spc step entirely
# (release_linux checks for php-sdk/static-php-cli/buildroot/lib/libphp.a)
COPY --from=php-sdk /src/php-sdk /src/php-sdk

# Copy only manifest files + create stubs, then `cargo fetch`.
# This layer is invalidated only when Cargo.lock or a Cargo.toml changes,
# not on every source edit.
COPY Cargo.toml Cargo.lock ./
COPY xtask/Cargo.toml       xtask/Cargo.toml
COPY crates/ephpm/Cargo.toml          crates/ephpm/Cargo.toml
COPY crates/ephpm-config/Cargo.toml   crates/ephpm-config/Cargo.toml
COPY crates/ephpm-server/Cargo.toml   crates/ephpm-server/Cargo.toml
COPY crates/ephpm-php/Cargo.toml      crates/ephpm-php/Cargo.toml
COPY crates/ephpm-db/Cargo.toml       crates/ephpm-db/Cargo.toml
COPY crates/ephpm-kv/Cargo.toml       crates/ephpm-kv/Cargo.toml

RUN mkdir -p \
        xtask/src \
        crates/ephpm/src \
        crates/ephpm-config/src \
        crates/ephpm-server/src \
        crates/ephpm-php/src \
        crates/ephpm-db/src \
        crates/ephpm-kv/src && \
    printf 'fn main() {}\n' > xtask/src/main.rs && \
    printf 'fn main() {}\n' > crates/ephpm/src/main.rs && \
    for d in ephpm-config ephpm-server ephpm-php ephpm-db ephpm-kv; do \
        touch crates/$d/src/lib.rs; \
    done

RUN cargo fetch

# Pre-compile all third-party dependencies using the stub sources so their compiled
# .rlib artifacts are cached in this Docker layer. When real source replaces the stubs
# after COPY . ., cargo reuses all the dep .rlibs and only recompiles project crates.
#
# Must use the same --target as `cargo xtask release` (x86_64-unknown-linux-musl) and
# the same PHP_SDK_PATH so the feature flags and cfg(php_linked) match exactly.
RUN PHP_SDK_PATH=/src/php-sdk/static-php-cli/buildroot \
    cargo build --release --package ephpm \
        --target x86_64-unknown-linux-musl 2>&1 | grep -v "^warning:"

# Real source — invalidates from here, but the dep .rlib layer above is cached.
# target/ is in .dockerignore so COPY . . does not overwrite the pre-compiled deps.
COPY . .

# Docker's COPY preserves host file mtimes, which may be older than the stub files
# created by `touch` in the pre-compile step. Cargo uses mtimes for fingerprinting,
# so it would incorrectly treat the real source as unchanged and reuse the empty stub
# artifacts. Touch all project crate sources to force mtimes newer than the stubs.
RUN find crates xtask -name '*.rs' -exec touch {} +

# Verify musl toolchain can be found by the build script
RUN which musl-gcc && musl-gcc --version && echo "musl-gcc OK" || echo "musl-gcc WARNING"

# Build ephpm. xtask skips PHP SDK since libphp.a already exists from the php-sdk stage.
# bash -o pipefail ensures cargo's exit code propagates through `tee` so a failed build
# is visible (Docker's default /bin/sh is dash and does not support pipefail).
RUN bash -euo pipefail -c 'cargo xtask release ${PHP_VERSION} 2>&1 | tee /tmp/build.log' || ( \
    echo "==> Build failed. Checking diagnostic info..." && \
    find /usr -name "libgcc.a" 2>/dev/null | head -10 && \
    ls -la /usr/local/musl/lib/gcc/ 2>/dev/null || true && \
    cat /tmp/build.log && \
    exit 1 \
  )

# Find the built binary (target/<triple>/release/ephpm)
RUN cp $(find target -name ephpm -type f -path '*/release/*' | head -1) /ephpm && \
    file /ephpm

# ── Stage 4: runtime ──────────────────────────────────────────────────────────

FROM alpine:3.21

RUN apk add --no-cache tini

COPY --from=builder /ephpm /usr/local/bin/ephpm

# Test docroot — baked into image for E2E testing
COPY tests/docroot/ /var/www/html/
COPY tests/ephpm-test.toml /etc/ephpm/ephpm.toml

EXPOSE 8080

ENTRYPOINT ["tini", "--"]
CMD ["ephpm", "serve", "--config", "/etc/ephpm/ephpm.toml"]
