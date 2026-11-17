#!/bin/sh
# Repackages the release archives in <dir> as Homebrew bottles, each with a .sha256 file:
#
#   scripts/homebrew-bottles.sh 0.2.0 dist/
#
# With bottles, `brew install` pours the binary. Without them Homebrew treats the formula
# as a source build and refuses to run when the Command Line Tools are older than it wants,
# although nothing gets compiled.
set -eu

version="${1#v}"
dir="$2"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# The oldest macOS tag Homebrew knows for each architecture; it pours that bottle on every
# newer macOS. The Rust targets build for macOS 11 (arm64) and 10.12 (x86_64).
for pair in \
  aarch64-apple-darwin:arm64_big_sur \
  x86_64-apple-darwin:big_sur \
  aarch64-unknown-linux-musl:arm64_linux \
  x86_64-unknown-linux-musl:x86_64_linux; do
  target="${pair%%:*}"
  tag="${pair#*:}"
  archive="$dir/amux-$target.tar.gz"
  [ -s "$archive" ] || { echo "missing $archive" >&2; exit 1; }
  rm -rf "$work/amux"
  mkdir -p "$work/amux/$version/bin"
  tar -xzf "$archive" -C "$work/amux/$version/bin"
  chmod 755 "$work/amux/$version/bin/amux"
  bottle="amux-$version.$tag.bottle.tar.gz"
  tar -czf "$dir/$bottle" -C "$work" amux
  (cd "$dir" && { sha256sum "$bottle" 2>/dev/null || shasum -a 256 "$bottle"; } >"$bottle.sha256")
done
