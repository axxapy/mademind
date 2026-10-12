#!/bin/bash
# Build the Arch package (packaging/arch/mademind-cuda) from this checkout's
# HEAD commit in a clean archlinux container, the way CI does. Uncommitted
# changes are not included.
#
#   scripts/arch-package.sh [out-dir]          # default out-dir: dist/
#   CUDA_ARCHS="75;86;89;120" scripts/arch-package.sh
#
# CUDA_ARCHS: GPU architectures to build kernels for (default 89 = RTX 40xx;
# each one adds several minutes of nvcc time).
set -euo pipefail

cd "$(dirname "$0")/.."
out="$(realpath -m "${1:-dist}")"
mkdir -p "$out"

docker run --rm \
  -v "$PWD:/src:ro" -v "$out:/out" \
  -e COMMIT="$(git rev-parse HEAD)" -e CUDA_ARCHS="${CUDA_ARCHS:-89}" \
  archlinux:base-devel bash -euo pipefail -c '
    # build deps only: --nodeps below skips the runtime nvidia-utils (driver)
    pacman -Syu --noconfirm --needed cuda gcc15 cmake clang git rust >/dev/null
    useradd -m builder
    # the recipe only, not leftovers of a local makepkg run (src/, pkg/, clone)
    mkdir /home/builder/pkg
    cp /src/packaging/arch/mademind-cuda/PKGBUILD /src/packaging/arch/mademind-cuda/*.service /home/builder/pkg/
    chown -R builder: /home/builder/pkg /out
    su builder -c "
      git config --global --add safe.directory \"*\"
      cd ~/pkg
      _mademind_src=git+file:///src#commit=$COMMIT _cuda_archs=\"$CUDA_ARCHS\" \
        PKGDEST=/out makepkg --noconfirm --nodeps
    "
  '
ls -la "$out"/mademind-cuda-*.pkg.tar.zst
