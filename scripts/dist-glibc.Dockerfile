# Build environment for the glibc release binaries (scripts/dist.sh).
#
# zig (used for the other targets) ships no libgomp, which llama.cpp's OpenMP
# needs on glibc; GCC does. Built on Debian bookworm (glibc 2.36), the
# binaries currently use symbols up to GLIBC_2.34: Ubuntu 22.04+, RHEL 9+,
# Debian 12+ (older distros: the static musl builds). x86_64 builds natively,
# arm64 with Debian's cross toolchain. libgomp is linked statically (below).

FROM debian:bookworm
RUN apt-get update -qq \
 && apt-get install -y -qq --no-install-recommends \
      ca-certificates curl build-essential cmake perl pkg-config clang libclang-dev git \
      gcc-aarch64-linux-gnu g++-aarch64-linux-gnu libc6-dev-arm64-cross \
 && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup PATH=/usr/local/cargo/bin:$PATH
ARG RUST_VERSION=1.98.1
RUN curl -sSf https://sh.rustup.rs | CARGO_HOME=/usr/local/cargo sh -s -- -y -q \
      --profile minimal --default-toolchain "$RUST_VERSION" --target aarch64-unknown-linux-gnu

# Static libgomp: directories holding only libgomp.a, searched first. ld
# looks for .so then .a in each directory before the next, so -lgomp
# resolves to the archive and users need no libgomp installed.
RUN mkdir -p /opt/gomp/x86_64 /opt/gomp/aarch64 \
 && cp "$(gcc -print-file-name=libgomp.a)" /opt/gomp/x86_64/ \
 && cp "$(aarch64-linux-gnu-gcc -print-file-name=libgomp.a)" /opt/gomp/aarch64/

ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-L native=/opt/gomp/x86_64" \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-L native=/opt/gomp/aarch64" \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ \
    AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar \
    BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_gnu="--sysroot=/usr/aarch64-linux-gnu" \
    CARGO_HOME=/cache/cargo CARGO_TARGET_DIR=/cache/target
WORKDIR /src
