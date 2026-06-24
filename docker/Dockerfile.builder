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
# Add the musl target matching the build host's architecture. The builder
# image is built natively per-arch (mirroring docker/Dockerfile.gha), so only
# the host's own target is needed. uname -m reports x86_64 / aarch64.
RUN case "$(uname -m)" in \
      x86_64)  rustup target add x86_64-unknown-linux-musl ;; \
      aarch64) rustup target add aarch64-unknown-linux-musl ;; \
      *) echo "unsupported arch $(uname -m)" >&2; exit 1 ;; \
    esac

# spc's nightly binaries are named spc-linux-<uname-m> (spc-linux-x86_64 /
# spc-linux-aarch64), so the host arch token drops straight in.
RUN ARCH="$(uname -m)" \
    && curl -fsSL -o /usr/local/bin/spc \
       "https://dl.static-php.dev/static-php-cli/spc-bin/nightly/spc-linux-${ARCH}" \
    && chmod +x /usr/local/bin/spc

# musl.cc cross toolchains are named <uname-m>-linux-musl-cross.tgz and extract
# to /opt/<uname-m>-linux-musl-cross/ — build.rs derives the same paths from
# the cargo target arch.
RUN ARCH="$(uname -m)" \
    && curl -fsSL "https://musl.cc/${ARCH}-linux-musl-cross.tgz" | tar xz -C /opt/

WORKDIR /src/ephpm

COPY docker/builder-entrypoint.sh /builder-entrypoint.sh
RUN chmod +x /builder-entrypoint.sh
ENTRYPOINT ["/bin/bash", "/builder-entrypoint.sh"]
