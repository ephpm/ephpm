# ephpm extension builder image
# Ubuntu for bindgen/libclang compatibility; spc doctor installs musl toolchain

FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential ca-certificates curl git pkg-config tar xz-utils \
    libclang-dev musl-tools musl-dev \
    autoconf automake libtool bison re2c flex cmake ninja-build \
    libssl-dev zlib1g-dev autopoint unzip gettext \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal \
    && rustup target add x86_64-unknown-linux-musl

ENV CC_x86_64_unknown_linux_musl=musl-gcc \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_CC=musl-gcc \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc

RUN curl -fsSL -o /usr/local/bin/spc \
    https://dl.static-php.dev/static-php-cli/spc-bin/nightly/spc-linux-x86_64 \
    && chmod +x /usr/local/bin/spc

# Download full musl cross-toolchain with g++/libstdc++ (needed for C++ extensions like intl)
RUN curl -fsSL https://musl.cc/x86_64-linux-musl-cross.tgz | tar xz -C /opt/

WORKDIR /build

COPY docker/builder-entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

ENTRYPOINT ["/entrypoint.sh"]
