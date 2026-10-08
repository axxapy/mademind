#!/usr/bin/env bash
# Build release archives into dist/: one per platform, plus SHA256SUMS.
# Used by `make dist` and by .github/workflows/release.yml.
#
#   scripts/dist.sh                                  # every platform below
#   TARGETS="linux-x86_64-gnu linux-x86_64-musl" scripts/dist.sh
#   BUILD=native TARGETS=macos-arm64 scripts/dist.sh # on the platform itself
#
# How each platform builds:
#   linux-*-gnu   in a Debian bookworm container (scripts/dist-glibc.Dockerfile):
#                 GCC provides libgomp for llama.cpp's OpenMP, linked statically.
#                 Currently needs glibc >= 2.34 (Ubuntu 22.04+, RHEL 9+).
#   linux-*-musl  cross-built with zig (cargo-zigbuild); fully static, runs on
#                 any Linux.
#   macos-*, windows-*
#                 BUILD=native: plain cargo on that OS (CI runners). Windows
#                 uses MSVC, whose OpenMP needs no libgomp. Cross-building them
#                 from Linux with zig fails (no macOS SDK; no MinGW libgomp).
#
# Needs: bash 3.2+, rustup; docker (gnu); zig + cargo-zigbuild (musl);
# zip or 7z (windows).
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
OUT="dist"
ALL="linux-x86_64-gnu linux-x86_64-musl linux-arm64-gnu linux-arm64-musl macos-x86_64 macos-arm64 windows-x86_64"
TARGETS="${TARGETS:-$ALL}"
BUILD="${BUILD:-}"

# Rust target for a platform name. Cross builds of Windows use zig's GNU
# targets; native builds use MSVC.
triple() {
  case "$1" in
    linux-x86_64-gnu) echo x86_64-unknown-linux-gnu ;;
    linux-arm64-gnu) echo aarch64-unknown-linux-gnu ;;
    linux-x86_64-musl) echo x86_64-unknown-linux-musl ;;
    linux-arm64-musl) echo aarch64-unknown-linux-musl ;;
    macos-x86_64) echo x86_64-apple-darwin ;;
    macos-arm64) echo aarch64-apple-darwin ;;
    windows-x86_64) if [[ $BUILD == native ]]; then echo x86_64-pc-windows-msvc; else echo x86_64-pc-windows-gnu; fi ;;
    *) echo "unknown target $1 (known: $ALL)" >&2; return 1 ;;
  esac
}

# sqlite-vec uses BSD u_intN_t names that musl doesn't define.
MUSL_CFLAGS="-Du_int8_t=uint8_t -Du_int16_t=uint16_t -Du_int64_t=uint64_t"
export CFLAGS_x86_64_unknown_linux_musl="$MUSL_CFLAGS"
export CFLAGS_aarch64_unknown_linux_musl="$MUSL_CFLAGS"

GLIBC_IMAGE="mademind-dist-glibc"
glibc_image_built=""

# build <name>: compile, leaving the binary's path in $bin.
build() {
  local name="$1" t
  t="$(triple "$name")" || return 1
  case "$name" in
    linux-*-gnu)
      if [[ -z $glibc_image_built ]]; then
        docker build -q -t "$GLIBC_IMAGE" -f scripts/dist-glibc.Dockerfile scripts >/dev/null || return 1
        glibc_image_built=1
      fi
      local out="$OUT/.build-$name"
      rm -rf "$out" && mkdir -p "$out"
      docker run --rm -v "$PWD:/src:ro" -v mademind-dist-cache:/cache -v "$PWD/$out:/out" "$GLIBC_IMAGE" \
        sh -c "cargo build --release --locked --target $t \
               && cp /cache/target/$t/release/mademind /out/ \
               && chown $(id -u):$(id -g) /out/mademind" || return 1
      bin="$out/mademind"
      return 0
      ;;
  esac
  rustup target add "$t" >/dev/null || return 1
  if [[ $BUILD == native ]]; then
    cargo build --release --locked --target "$t" || return 1
  else
    cargo zigbuild --release --locked --target "$t" || return 1
  fi
  bin="target/$t/release/mademind"
  if [[ $name == windows-* ]]; then bin="$bin.exe"; fi
}

package() {
  local name="$1" dir="mademind-$VERSION-$1"
  rm -rf "${OUT:?}/$dir" && mkdir -p "$OUT/$dir"
  cp "$bin" LICENSE config.example.toml config.reference.toml "$OUT/$dir/"
  cp -r clients "$OUT/$dir/"
  if [[ $name == windows-* ]]; then
    rm -f "$OUT/$dir.zip"
    if command -v zip >/dev/null; then
      (cd "$OUT" && zip -qr "$dir.zip" "$dir")
    else
      (cd "$OUT" && 7z a -tzip -bd "$dir.zip" "$dir" >/dev/null)
    fi
  else
    tar -C "$OUT" -czf "$OUT/$dir.tar.gz" "$dir"
  fi
  rm -rf "${OUT:?}/$dir" "${OUT:?}/.build-$name"
}

mkdir -p "$OUT"
failed=""
for name in $TARGETS; do
  echo "==> $name"
  if build "$name"; then
    package "$name"
  else
    echo "!! $name failed" >&2
    failed="$failed $name"
  fi
done

sums() { if command -v sha256sum >/dev/null; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }
(cd "$OUT" && ls mademind-"$VERSION"-* >/dev/null 2>&1 && sums mademind-"$VERSION"-* > SHA256SUMS) || true
ls -l "$OUT"
if [[ -n $failed ]]; then
  echo "failed:$failed" >&2
  exit 1
fi
