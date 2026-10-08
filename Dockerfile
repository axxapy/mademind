# syntax=docker/dockerfile:1
#
# Default image (slim): one binary — mademind with rqmd linked in (builtin
# engine). No Bun, no Node, no separate engine process.
#
#   stage build : rust:slim-trixie + cmake/clang (llama.cpp is compiled from
#                 source inside llama-cpp-sys-2; CPU-only by design)
#   final       : debian:trixie-slim + the binary
#
# For the original Bun qmd engine use Dockerfile.classic.

# Pinned to trixie: must match the runtime glibc (rust:slim floats).
FROM rust:slim-trixie AS build
RUN apt-get update -qq \
 && apt-get install -y -qq cmake build-essential pkg-config perl libclang-dev \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:trixie-slim
# libgomp1/libstdc++6: llama.cpp (OpenMP, C++). OpenSSL is linked in, so
# model downloads (Hugging Face, on first use) need only the CA bundle —
# copied from the build stage rather than installing ca-certificates, which
# would pull in the openssl packages.
RUN apt-get update -qq \
 && apt-get install -y -qq libgomp1 libstdc++6 \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /build/target/release/mademind /usr/local/bin/mademind
# Smoke assertion: fails the build if a shared lib is missing.
RUN mademind healthcheck; test $? -eq 1
# Index + models (~0.3GB embed model; more for rerank/expansion) live here.
ENV MADEMIND_CACHE_DIR=/root/.cache/mademind
VOLUME ["/root/.cache/mademind"]
WORKDIR /root
EXPOSE 8888
HEALTHCHECK --interval=60s --timeout=10s CMD ["mademind", "healthcheck"]
# Config: /config/config.toml (see config.example.toml / config.reference.toml).
CMD ["mademind", "serve"]
