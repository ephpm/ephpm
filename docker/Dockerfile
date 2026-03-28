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

# System build tools for static-php-cli (spc).
#
# The standalone spc binary bundles its own PHP — no system PHP or Composer
# is needed. `spc doctor --auto-fix` may install additional packages, so we
# keep apt metadata around (no `rm -rf /var/lib/apt/lists/*`).
#
# Key packages:
#   unzip     — required by spc for extracting downloads
#   gettext   — provides `autopoint`, checked by spc doctor
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential ca-certificates curl git pkg-config sudo \
    autoconf automake bison cmake flex gettext libtool make re2c unzip \
    libclang-dev musl-tools

# Rust stable
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal

WORKDIR /src
COPY . .

# Build PHP SDK and ephpm binary via xtask
RUN cargo xtask release ${PHP_VERSION}

# Find the built binary (target/<triple>/release/ephpm)
RUN cp $(find target -name ephpm -type f -path '*/release/*' | head -1) /ephpm

# ── Stage 2: Runtime ─────────────────────────────────────────────────────────

FROM alpine:3.21

RUN apk add --no-cache tini

COPY --from=builder /ephpm /usr/local/bin/ephpm

# Test docroot — baked into image for E2E testing
COPY tests/docroot/ /var/www/html/
COPY tests/ephpm-test.toml /etc/ephpm/ephpm.toml

EXPOSE 8080

ENTRYPOINT ["tini", "--"]
CMD ["ephpm", "--config", "/etc/ephpm/ephpm.toml"]
