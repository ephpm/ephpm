# Multi-stage build for ephpm with embedded PHP.
#
# Cache strategy:
#   base:    System deps + Rust toolchain — changes rarely
#   php-sdk: Pre-built tarball from build context, or built from source via spc
#   builder: Cargo manifest fetch (cached until Cargo.lock changes) → full source build
#
# Fast build (pre-built SDK, download it first):
#   gh release download 8.5.2 -R ephpm/php-sdk -p 'php-sdk-*-linux-x86_64.tar.gz' -D php-sdk/
#   podman build -f docker/Dockerfile --build-arg PHP_VERSION=8.5.2 -t ephpm:dev .
#
# Source build (no pre-built SDK, ~15 min longer):
#   podman build -f docker/Dockerfile --build-arg PHP_VERSION=8.5 -t ephpm:dev .

ARG PHP_VERSION=8.5.2

# ── Stage 1: base — system deps + Rust toolchain ─────────────────────────────

FROM ubuntu:24.04 AS base

ENV DEBIAN_FRONTEND=noninteractive

# System build tools for static-php-cli (spc) and musl cross-compilation.
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

# ── Stage 2: php-sdk — use pre-built tarball or build from source ────────────
#
# If php-sdk/*.tar.gz exists in the build context (downloaded by the workflow
# or manually via `gh release download`), extract it. Otherwise build from
# source via `cargo xtask php-sdk` (~15 min).

FROM base AS php-sdk

ARG PHP_VERSION

WORKDIR /src

# Copy any pre-downloaded SDK tarballs from the build context.
# The directory may be empty — that's fine, we check in the RUN step.
COPY php-sdk/ /tmp/php-sdk-dl/

# Layout that cargo xtask release expects:
#   php-sdk/static-php-cli/buildroot/lib/libphp.a
#   php-sdk/static-php-cli/buildroot/include/php/

# Try to find a matching tarball; if none, fall through to source build.
RUN ARCH=$(uname -m) && \
    TARBALL=$(find /tmp/php-sdk-dl -name "php-sdk-*-linux-${ARCH}.tar.gz" 2>/dev/null | head -1) && \
    if [ -n "${TARBALL}" ]; then \
        echo "==> Extracting pre-built PHP SDK from ${TARBALL}..." && \
        mkdir -p php-sdk/static-php-cli/buildroot && \
        tar xzf "${TARBALL}" -C php-sdk/static-php-cli/buildroot && \
        ls -lh php-sdk/static-php-cli/buildroot/lib/libphp.a && \
        echo "==> PHP SDK ready (pre-built)"; \
    else \
        echo "==> No pre-built SDK tarball found, will build from source..."; \
    fi && \
    rm -rf /tmp/php-sdk-dl

# Source build fallback — only runs if libphp.a doesn't exist yet.
COPY Cargo.toml Cargo.lock ./
COPY .cargo/ .cargo/
COPY xtask/ xtask/
COPY crates/ephpm/Cargo.toml          crates/ephpm/Cargo.toml
COPY crates/ephpm-config/Cargo.toml   crates/ephpm-config/Cargo.toml
COPY crates/ephpm-server/Cargo.toml   crates/ephpm-server/Cargo.toml
COPY crates/ephpm-php/Cargo.toml      crates/ephpm-php/Cargo.toml
COPY crates/ephpm-db/Cargo.toml       crates/ephpm-db/Cargo.toml
COPY crates/ephpm-kv/Cargo.toml       crates/ephpm-kv/Cargo.toml

RUN if [ ! -f php-sdk/static-php-cli/buildroot/lib/libphp.a ]; then \
        mkdir -p \
            crates/ephpm/src \
            crates/ephpm-config/src \
            crates/ephpm-server/src \
            crates/ephpm-php/src \
            crates/ephpm-db/src \
            crates/ephpm-kv/src && \
        printf 'fn main() {}\n' > crates/ephpm/src/main.rs && \
        for d in ephpm-config ephpm-server ephpm-php ephpm-db ephpm-kv; do \
            touch crates/$d/src/lib.rs; \
        done && \
        cargo xtask php-sdk ${PHP_VERSION}; \
    fi

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
