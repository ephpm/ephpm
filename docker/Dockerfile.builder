FROM ubuntu:24.04

# Install build dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    bash curl git gcc g++ make cmake pkg-config \
    libclang-dev musl-tools musl-dev \
    autopoint unzip gettext ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Install latest Rust via rustup with musl target
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"
RUN rustup target add x86_64-unknown-linux-musl

# Install static-php-cli (spc)
RUN curl -fsSL -o /usr/local/bin/spc \
    https://dl.static-php.dev/static-php-cli/spc-bin/nightly/spc-linux-x86_64 \
    && chmod +x /usr/local/bin/spc

# Download full musl cross-toolchain with g++/libstdc++ (needed for C++ extensions like intl)
RUN curl -fsSL https://musl.cc/x86_64-linux-musl-cross.tgz | tar xz -C /opt/

WORKDIR /src/ephpm
