# Multi-stage build for ephpm with embedded PHP.
#
# Cache strategy:
#   base:    System deps + Rust toolchain — changes rarely
#   php-sdk: PHP SDK download — only invalidates when PHP_VERSION changes
#   builder: Cargo manifest fetch (cached until Cargo.lock changes) → full source build
#
# Build:
#   podman build -f docker/Dockerfile --build-arg PHP_VERSION=8.5 -t ephpm:dev .

# Global ARG so it can be referenced in FROM lines (each stage re-declares it to use it)
ARG PHP_VERSION=8.5

# ── Stage 1: base — system deps + Rust toolchain ─────────────────────────────

FROM ubuntu:24.04 AS base

ENV DEBIAN_FRONTEND=noninteractive

# System tools needed by the ephpm Rust build:
#   - musl-tools / musl-dev: musl-gcc cross-compiler for the
#     x86_64-unknown-linux-musl target (libphp.a is musl-linked).
#   - libclang-dev: bindgen needs libclang to parse PHP headers.
#   - build-essential, pkg-config: cc::Build for ephpm_wrapper.c.
#   - curl, git, ca-certificates: rustup install + cargo registry.
#   - tar: extracting the prebuilt PHP SDK tarball in the next stage.
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential ca-certificates curl git pkg-config tar \
    libclang-dev musl-tools musl-dev \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal

RUN rustup target add x86_64-unknown-linux-musl

ENV CC_x86_64_unknown_linux_musl=musl-gcc \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_CC=musl-gcc \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc

# ── Stage 2: php-sdk — download prebuilt SDK ─────────────────────────────────
#
# `cargo xtask php-sdk` downloads `libphp.a` + headers from
# github.com/ephpm/php-sdk releases. It's a ~30s pull (no compile), so this
# stage is tiny but kept separate so the Cargo.lock-keyed builder layer
# below doesn't invalidate when, say, only an .rs file changes.

FROM base AS php-sdk

ARG PHP_VERSION

WORKDIR /src

# Manifest + xtask source so `cargo xtask php-sdk` is runnable. Stub
# Cargo.toml/sources for the other workspace members keep the resolver happy.
COPY Cargo.toml Cargo.lock ./
COPY .cargo/ .cargo/
COPY xtask/ xtask/
COPY crates/ephpm/Cargo.toml          crates/ephpm/Cargo.toml
COPY crates/ephpm-config/Cargo.toml   crates/ephpm-config/Cargo.toml
COPY crates/ephpm-server/Cargo.toml   crates/ephpm-server/Cargo.toml
COPY crates/ephpm-php/Cargo.toml      crates/ephpm-php/Cargo.toml
COPY crates/ephpm-db/Cargo.toml       crates/ephpm-db/Cargo.toml
COPY crates/ephpm-kv/Cargo.toml       crates/ephpm-kv/Cargo.toml

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

# Pre-downloaded PHP SDK so xtask release skips the download step entirely
# (release_native checks for the cached SDK before fetching).
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
# `cargo xtask php-sdk` resolves "8.5" → "8.5.2-linux-x86_64" so the cache
# dir name is the full version, not the shorthand. Discover it.
RUN PHP_SDK_PATH=$(find /src/php-sdk -mindepth 1 -maxdepth 1 -type d -name '*-linux-x86_64' | head -1) \
    && test -n "$PHP_SDK_PATH" \
    && PHP_SDK_PATH=$PHP_SDK_PATH cargo build --release --package ephpm \
        --target x86_64-unknown-linux-musl 2>&1 | grep -v "^warning:"

# Real source — invalidates from here, but the dep .rlib layer above is cached.
# target/ is in .dockerignore so COPY . . does not overwrite the pre-compiled deps.
COPY . .

# Docker's COPY preserves host file mtimes, which may be older than the stub files
# created by `touch` in the pre-compile step. Cargo uses mtimes for fingerprinting,
# so it would incorrectly treat the real source as unchanged and reuse the empty stub
# artifacts. Touch all project crate sources to force mtimes newer than the stubs.
RUN find crates xtask -name '*.rs' -exec touch {} +

# Build ephpm. xtask skips the SDK download since the tarball already extracted in php-sdk stage.
# bash -o pipefail ensures cargo's exit code propagates through `tee` so a failed build
# is visible (Docker's default /bin/sh is dash and does not support pipefail).
RUN bash -euo pipefail -c 'cargo xtask release ${PHP_VERSION} 2>&1 | tee /tmp/build.log' || ( \
    echo "==> Build failed. Checking diagnostic info..." && \
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
