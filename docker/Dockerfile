# Multi-stage build for ephpm with embedded PHP.
#
# Cache strategy:
#   base:    System deps + Rust toolchain — changes rarely.
#   php-sdk: Single RUN that downloads + extracts the prebuilt PHP SDK from
#            github.com/ephpm/php-sdk releases. Keyed only on PHP_SDK_VERSION,
#            so containerd's image layer cache reuses it across all source
#            changes. Bumping PHP_SDK_VERSION (e.g., 8.5.2 → 8.5.3) is the
#            only thing that invalidates this ~80 MB layer.
#   builder: Cargo manifest fetch (cached until Cargo.lock changes) → full source build.
#
# Build:
#   podman build -f docker/Dockerfile --build-arg PHP_SDK_VERSION=8.5.2 -t ephpm:dev .
#
# `cargo xtask e2e` resolves the shorthand (e.g. "8.5") to the pinned full
# version via xtask::PHP_SDK_VERSIONS and passes it as PHP_SDK_VERSION.

# Pinned full version of the PHP SDK to download. Must match a release tag
# at github.com/ephpm/php-sdk/releases (e.g., v8.5.2). The default mirrors
# xtask::PHP_SDK_VERSIONS — keep them in sync if you bump.
ARG PHP_SDK_VERSION=8.5.2

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
# Pure shell + curl + tar — NO source COPY before the download — so the
# resulting image layer is keyed only on the base image and PHP_SDK_VERSION.
# containerd reuses it indefinitely across feature work / refactors / docs
# changes. Only a version bump invalidates it.

FROM base AS php-sdk

ARG PHP_SDK_VERSION

# Detect arch at build time (works for both x86_64 and aarch64 builders).
# Layout we extract matches xtask::php_sdk_dir_for():
#   /opt/php-sdk/<version>-linux-<arch>/{lib,include}/
RUN set -eux; \
    arch=$(uname -m); \
    sdk_dir="/opt/php-sdk/${PHP_SDK_VERSION}-linux-${arch}"; \
    asset="php-sdk-${PHP_SDK_VERSION}-linux-${arch}.tar.gz"; \
    url="https://github.com/ephpm/php-sdk/releases/download/v${PHP_SDK_VERSION}/${asset}"; \
    mkdir -p "${sdk_dir}"; \
    curl -fSL "${url}" | tar xz -C "${sdk_dir}"; \
    test -f "${sdk_dir}/lib/libphp.a"

# ── Stage 3: builder — dep fetch layer then full source build ─────────────────

FROM base AS builder

ARG PHP_SDK_VERSION

WORKDIR /src

# Pull the SDK in from the cached php-sdk stage. xtask::release_native
# checks PHP_SDK_PATH and skips downloading if the directory already
# contains lib/libphp.a, so the release build below is a pure cargo build.
COPY --from=php-sdk /opt/php-sdk /src/php-sdk

# Copy only manifest files + create stubs, then `cargo fetch`.
# This layer is invalidated only when Cargo.lock or a Cargo.toml changes,
# not on every source edit.
COPY Cargo.toml Cargo.lock ./
COPY xtask/Cargo.toml                     xtask/Cargo.toml
COPY crates/ephpm/Cargo.toml              crates/ephpm/Cargo.toml
COPY crates/ephpm-cluster/Cargo.toml      crates/ephpm-cluster/Cargo.toml
COPY crates/ephpm-config/Cargo.toml       crates/ephpm-config/Cargo.toml
COPY crates/ephpm-db/Cargo.toml           crates/ephpm-db/Cargo.toml
COPY crates/ephpm-kv/Cargo.toml           crates/ephpm-kv/Cargo.toml
COPY crates/ephpm-php/Cargo.toml          crates/ephpm-php/Cargo.toml
COPY crates/ephpm-query-stats/Cargo.toml  crates/ephpm-query-stats/Cargo.toml
COPY crates/ephpm-server/Cargo.toml       crates/ephpm-server/Cargo.toml
COPY crates/ephpm-sqld/Cargo.toml         crates/ephpm-sqld/Cargo.toml

RUN mkdir -p \
        xtask/src \
        crates/ephpm/src \
        crates/ephpm-cluster/src \
        crates/ephpm-config/src \
        crates/ephpm-db/src \
        crates/ephpm-kv/src \
        crates/ephpm-php/src \
        crates/ephpm-query-stats/src \
        crates/ephpm-server/src \
        crates/ephpm-sqld/src && \
    printf 'fn main() {}\n' > xtask/src/main.rs && \
    printf 'fn main() {}\n' > crates/ephpm/src/main.rs && \
    for d in ephpm-cluster ephpm-config ephpm-db ephpm-kv ephpm-php ephpm-query-stats ephpm-server ephpm-sqld; do \
        touch crates/$d/src/lib.rs; \
    done

RUN cargo fetch

# Pre-compile all third-party dependencies using the stub sources so their compiled
# .rlib artifacts are cached in this Docker layer. When real source replaces the stubs
# after COPY . ., cargo reuses all the dep .rlibs and only recompiles project crates.
#
# Must use the same --target as `cargo xtask release` (x86_64-unknown-linux-musl) and
# the same PHP_SDK_PATH so the feature flags and cfg(php_linked) match exactly.
RUN PHP_SDK_PATH=/src/php-sdk/${PHP_SDK_VERSION}-linux-$(uname -m) \
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

# Build ephpm. xtask skips the SDK download since the tarball is already
# extracted in /src/php-sdk/. We pass the full PHP_SDK_VERSION as the version
# argument so xtask uses the same cache dir as the php-sdk stage.
# bash -o pipefail ensures cargo's exit code propagates through `tee` so a failed
# build is visible (Docker's default /bin/sh is dash and does not support pipefail).
RUN bash -euo pipefail -c 'cargo xtask release ${PHP_SDK_VERSION} 2>&1 | tee /tmp/build.log' || ( \
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
