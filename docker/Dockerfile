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

# System build tools + PHP CLI for static-php-cli
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential ca-certificates curl git pkg-config \
    autoconf automake bison cmake flex gettext libtool make re2c \
    libclang-dev musl-tools \
    software-properties-common gpg-agent \
    && add-apt-repository -y ppa:ondrej/php \
    && apt-get update && apt-get install -y --no-install-recommends \
    php8.4-cli php8.4-xml php8.4-mbstring php8.4-curl php8.4-tokenizer \
    && rm -rf /var/lib/apt/lists/*

# Composer
RUN curl -sS https://getcomposer.org/installer | php8.4 -- --install-dir=/usr/local/bin --filename=composer

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
