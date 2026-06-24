#!/bin/sh
set -e
EXTENSIONS="${EXTENSIONS:-json,pcre,mbstring,openssl,curl,xml,zip,zlib,session,fileinfo,filter,dom,phar,tokenizer,sodium}"
PHP_VERSION="${PHP_VERSION:-8.5}"
OUTPUT="${OUTPUT:-/output/ephpm}"
echo ""
echo "  Building libphp.a"
echo "    PHP:        ${PHP_VERSION}"
echo "    Extensions: ${EXTENSIONS}"
echo ""
cd /build

# Remove any stale musl toolchain so doctor installs spc's own proper one
rm -rf /usr/local/musl

# Run doctor to install deps including spc's musl toolchain (do NOT swallow errors)
spc doctor --auto-fix

# Resolve the rust target triple for this host's architecture. uname -m
# reports x86_64 / aarch64, which match both the rust arch token and the
# musl-cross toolchain naming (mirrors docker/Dockerfile.gha).
ARCH="$(uname -m)"
case "${ARCH}" in
    x86_64)  RUST_TARGET="x86_64-unknown-linux-musl" ;;
    aarch64) RUST_TARGET="aarch64-unknown-linux-musl" ;;
    *) echo "  Unsupported architecture: ${ARCH}" >&2; exit 1 ;;
esac
echo "    Arch:       ${ARCH} (${RUST_TARGET})"

# Point Rust linker at spc's musl toolchain for this arch.
MUSL_GCC="/usr/local/musl/bin/${ARCH}-linux-musl-gcc"
if [ -f "${MUSL_GCC}" ]; then
    case "${ARCH}" in
        x86_64)
            export CC_x86_64_unknown_linux_musl="${MUSL_GCC}"
            export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_CC="${MUSL_GCC}"
            export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="${MUSL_GCC}"
            ;;
        aarch64)
            export CC_aarch64_unknown_linux_musl="${MUSL_GCC}"
            export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_CC="${MUSL_GCC}"
            export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="${MUSL_GCC}"
            ;;
    esac
    echo "  Using spc musl toolchain at /usr/local/musl (${RUST_TARGET})"
fi

# Configure GitHub token for spc if provided
if [ -n "${GITHUB_TOKEN}" ]; then
    export GITHUB_TOKEN="${GITHUB_TOKEN}"
fi

SDK_PATH="${SPC_BUILD_PATH:-/build/buildroot}"
export PHP_SDK_PATH="${SDK_PATH}"

spc download \
    --with-php="${PHP_VERSION}" \
    --for-extensions="${EXTENSIONS}" \
    --prefer-pre-built \
    --no-alt \
    php-src,micro,frankenphp

spc build "${EXTENSIONS}" --build-embed --enable-zts

echo ""
echo "  Copying output"
cd /src/ephpm
cargo build --release --package ephpm --target "${RUST_TARGET}"

OUTPUT_FILE="${OUTPUT}"
OUTPUT_DIR=$(dirname "${OUTPUT_FILE}")
mkdir -p "${OUTPUT_DIR}"
cp "target/${RUST_TARGET}/release/ephpm" "${OUTPUT_FILE}"

EXT_COUNT=$("./target/${RUST_TARGET}/release/ephpm" php -m 2>/dev/null | grep -v '^\[' | grep -v '^$' | wc -l | tr -d ' ')
echo ""
echo "  Done - ${EXT_COUNT} extensions compiled in"
echo "    Binary: /output/$(basename "${OUTPUT_FILE}")"
echo "  Build complete"
echo "    Verify: ./$(basename "${OUTPUT_FILE}") ext list"
