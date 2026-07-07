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
# at github.com/ephpm/php-sdk/releases (e.g., v8.5.7). The default mirrors
# xtask::PHP_SDK_VERSIONS — keep them in sync if you bump.
ARG PHP_SDK_VERSION=8.5.7

# ── Stage 1: base — system deps + Rust toolchain ─────────────────────────────
#
# GLIBC FLOOR: the builder stage links the release binary, so its base sets the
# shipped binary's glibc floor. The manylinux_2_28 base (AlmaLinux 8, glibc
# 2.28, gcc-toolset-14) lowers that floor to glibc 2.28 — modern GCC over an
# old glibc symbol floor (the "manylinux" model). A 2.28-linked binary runs on
# RHEL8/AlmaLinux8, Ubuntu 20.04+, Debian 10+, Amazon Linux 2023, Fedora 40+.
# See docs/architecture/dynamic-baseline-design.md.

ARG BASE_IMAGE=quay.io/pypa/manylinux_2_28_x86_64
FROM ${BASE_IMAGE} AS base

# System tools needed by the ephpm Rust build (dnf / AlmaLinux 8):
#   - gcc-toolset-14: cc::Build for ephpm_wrapper.c + the gcc linker driver for
#     the glibc-dynamic (gnu) release binary. Present in the manylinux image;
#     enabled on PATH below.
#   - clang-devel/llvm-devel: bindgen needs libclang to parse PHP headers.
#   - curl, git, ca-certificates: rustup install + cargo registry.
#   - tar/xz: extracting the prebuilt PHP SDK tarball in the next stage.
RUN dnf install -y --allowerasing \
        gcc-toolset-14 clang-devel llvm-devel \
        ca-certificates curl git make pkgconfig tar xz which \
    && dnf clean all

# Enable gcc-toolset-14 (GCC 14) on PATH for every subsequent RUN. libclang
# for bindgen lives at /usr/lib64.
ENV PATH=/opt/rh/gcc-toolset-14/root/usr/bin:$PATH \
    LD_LIBRARY_PATH=/opt/rh/gcc-toolset-14/root/usr/lib64:/opt/rh/gcc-toolset-14/root/usr/lib \
    PKG_CONFIG_PATH=/opt/rh/gcc-toolset-14/root/usr/lib64/pkgconfig:/usr/lib64/pkgconfig \
    LIBCLANG_PATH=/usr/lib64

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:/opt/rh/gcc-toolset-14/root/usr/bin:$PATH

# The gnu target is the host default — no extra rustup target or cross
# toolchain needed (the musl-era CC_*/LINKER plumbing is gone with it).
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal

# ── Stage 2: php-sdk — download prebuilt SDK ─────────────────────────────────
#
# Pure shell + curl + tar — NO source COPY before the download — so the
# resulting image layer is keyed only on the base image and PHP_SDK_VERSION.
# containerd reuses it indefinitely across feature work / refactors / docs
# changes. Only a version bump invalidates it.

FROM base AS php-sdk

ARG PHP_SDK_VERSION

# Detect arch at build time (works for both x86_64 and aarch64 builders).
# Layout we extract matches xtask::php_sdk_dir_for() — Linux SDKs carry a
# `-gnu` libc suffix (glibc-linked libphp.a; the binary must dlopen()):
#   /opt/php-sdk/<version>-linux-<arch>-gnu/{lib,include}/
RUN set -eux; \
    arch=$(uname -m); \
    sdk_dir="/opt/php-sdk/${PHP_SDK_VERSION}-linux-${arch}-gnu"; \
    asset="php-sdk-${PHP_SDK_VERSION}-linux-${arch}-gnu.tar.gz"; \
    url="https://github.com/ephpm/php-sdk/releases/download/v${PHP_SDK_VERSION}/${asset}"; \
    mkdir -p "${sdk_dir}"; \
    curl -fSL "${url}" -o /tmp/sdk.tar.gz; \
    sha256sum /tmp/sdk.tar.gz; \
    tar xzf /tmp/sdk.tar.gz -C "${sdk_dir}"; \
    rm /tmp/sdk.tar.gz; \
    test -f "${sdk_dir}/lib/libphp.a"; \
    echo "==> SDK lib/ contents:"; \
    ls -la "${sdk_dir}/lib/"; \
    echo "==> libz.a archive members:"; \
    ar t "${sdk_dir}/lib/libz.a"; \
    echo "==> gz* symbols in libz.a (full):"; \
    nm "${sdk_dir}/lib/libz.a" 2>/dev/null | grep -E " (T|t|R|D|W) gz" || echo "(no gz* symbols found!)"; \
    echo "==> sanity: looking for gzerror specifically:"; \
    nm "${sdk_dir}/lib/libz.a" 2>/dev/null | grep gzerror || echo "(gzerror missing!)"

# ── Stage 3: builder — dep fetch layer then full source build ─────────────────

FROM base AS builder

ARG PHP_SDK_VERSION

# Release-time override forwarded by .github/workflows/release.yml so the
# in-container `cargo xtask release` bakes the actual tag into the binary
# (`ephpm --version`). Empty for local `docker build` and `cargo xtask e2e`,
# in which case crates/ephpm/build.rs falls back to CARGO_PKG_VERSION.
ARG EPHPM_RELEASE_VERSION=""
ENV EPHPM_RELEASE_VERSION=${EPHPM_RELEASE_VERSION}

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
COPY crates/ephpm-middleware/Cargo.toml   crates/ephpm-middleware/Cargo.toml
COPY crates/ephpm-middleware-builtins/Cargo.toml crates/ephpm-middleware-builtins/Cargo.toml
COPY crates/ephpm-middleware-cors/Cargo.toml crates/ephpm-middleware-cors/Cargo.toml
COPY crates/ephpm-middleware-jwt/Cargo.toml crates/ephpm-middleware-jwt/Cargo.toml
COPY crates/ephpm-middleware-ratelimit/Cargo.toml crates/ephpm-middleware-ratelimit/Cargo.toml
COPY crates/ephpm-middleware-security-headers/Cargo.toml crates/ephpm-middleware-security-headers/Cargo.toml
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
        crates/ephpm-middleware/src \
        crates/ephpm-middleware-builtins/src \
        crates/ephpm-middleware-cors/src \
        crates/ephpm-middleware-jwt/src \
        crates/ephpm-middleware-ratelimit/src \
        crates/ephpm-middleware-security-headers/src \
        crates/ephpm-php/src \
        crates/ephpm-query-stats/src \
        crates/ephpm-server/src \
        crates/ephpm-sqld/src && \
    printf 'fn main() {}\n' > xtask/src/main.rs && \
    printf 'fn main() {}\n' > crates/ephpm/src/main.rs && \
    for d in ephpm-cluster ephpm-config ephpm-db ephpm-kv ephpm-middleware ephpm-middleware-builtins ephpm-middleware-cors ephpm-middleware-jwt ephpm-middleware-ratelimit ephpm-middleware-security-headers ephpm-php ephpm-query-stats ephpm-server ephpm-sqld; do \
        touch crates/$d/src/lib.rs; \
    done && \
    # ephpm-php Cargo.toml declares `build = "build.rs"` explicitly, so cargo
    # errors out (rather than silently skipping) if the file is missing during
    # the stub-precompile step. Other crates with build.rs (ephpm, ephpm-sqld)
    # rely on auto-detection and tolerate absence, but we stub them too for
    # consistency — all three real build.rs files arrive with the later COPY.
    printf 'fn main() {}\n' > crates/ephpm-php/build.rs && \
    printf 'fn main() {}\n' > crates/ephpm-sqld/build.rs && \
    printf 'fn main() {}\n' > crates/ephpm/build.rs

RUN cargo fetch

# Pre-compile all third-party dependencies using the stub sources so their compiled
# .rlib artifacts are cached in this Docker layer. When real source replaces the stubs
# after COPY . ., cargo reuses all the dep .rlibs and only recompiles project crates.
#
# Must use the same --target as `cargo xtask release` (<arch>-unknown-linux-gnu) and
# the same PHP_SDK_PATH so the feature flags and cfg(php_linked) match exactly.
# The -Wl,--export-dynamic link arg is emitted by crates/ephpm/build.rs, so the
# in-container build gets it for free — nothing to mirror here.
RUN PHP_SDK_PATH=/src/php-sdk/${PHP_SDK_VERSION}-linux-$(uname -m)-gnu \
    cargo build --release --package ephpm \
        --target $(uname -m)-unknown-linux-gnu 2>&1 | grep -v "^warning:"

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
    ls -lh /ephpm

# ── Stage 4: runtime ──────────────────────────────────────────────────────────

# The ephpm binary is glibc-dynamic (it must dlopen() shared PHP extensions
# and middleware), so the runtime base has to ship glibc — a Debian slim
# image, not alpine/scratch. ca-certificates covers ACME/TLS and outbound
# HTTPS.
#
# Runtime base = debian:12-slim (bookworm, glibc 2.36). The binary's glibc
# floor is now 2.28 (php-sdk libphp.a + the ephpm link both build on the
# manylinux_2_28 base — glibc 2.28, no __isoc23_* / pidfd_* refs above 2.28),
# so any runtime with glibc >= 2.28 can host it. bookworm (2.36) comfortably
# clears that and is smaller/more-stable than trixie. (The old trixie/2.41
# requirement was an artifact of the 2.39 floor from building on ubuntu:24.04;
# see docs/architecture/dynamic-baseline-design.md §2.1.) debian:11-slim
# (bullseye, glibc 2.31) would also work if an even older runtime is wanted.
FROM debian:12-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    tini ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /ephpm /usr/local/bin/ephpm

# Test docroot — baked into image for E2E testing
COPY tests/docroot/ /var/www/html/
COPY tests/ephpm-test.toml /etc/ephpm/ephpm.toml

EXPOSE 8080

ENTRYPOINT ["tini", "--"]
CMD ["ephpm", "serve", "--config", "/etc/ephpm/ephpm.toml"]
