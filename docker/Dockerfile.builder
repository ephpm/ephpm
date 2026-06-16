FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
    bash curl git gcc g++ make cmake pkg-config \
    libclang-dev musl-tools musl-dev \
    autoconf automake libtool bison re2c \
    autopoint unzip gettext patch \
    ca-certificates xz-utils gawk \
    ninja-build python3 flex bzip2 \
    && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"
RUN rustup target add x86_64-unknown-linux-musl

RUN curl -fsSL -o /usr/local/bin/spc \
    https://dl.static-php.dev/static-php-cli/spc-bin/nightly/spc-linux-x86_64 \
    && chmod +x /usr/local/bin/spc

RUN curl -fsSL https://musl.cc/x86_64-linux-musl-cross.tgz | tar xz -C /opt/

WORKDIR /src/ephpm
