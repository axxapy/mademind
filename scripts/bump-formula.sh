#!/bin/bash
# Point Formula/mademind.rb at a published release: the version in the four
# download URLs and their sha256 values (from the release's SHA256SUMS).
#
#   scripts/bump-formula.sh 0.2.2
set -euo pipefail

v="${1:?usage: $0 <version>}"
cd "$(dirname "$0")/.."
f=Formula/mademind.rb
sums="$(curl -fsSL "https://github.com/axxapy/mademind/releases/download/v$v/SHA256SUMS")"

sed -E -i.bak "s#/download/v[0-9][^/]*/mademind-[^-]+-#/download/v$v/mademind-$v-#" "$f"
for t in macos-arm64 macos-x86_64 linux-arm64-musl linux-x86_64-musl; do
  sum="$(awk -v n="mademind-$v-$t.tar.gz" '$2 == n { print $1 }' <<<"$sums")"
  [[ -n $sum ]] || { echo "no checksum for $t in v$v SHA256SUMS" >&2; exit 1; }
  # the sha256 line right after this target's url line
  sed -i.bak "/-$t\.tar\.gz\"/{n;s/sha256 \"[0-9a-f]*\"/sha256 \"$sum\"/;}" "$f"
done
rm -f "$f.bak"
git diff --stat -- "$f"
