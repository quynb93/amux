#!/bin/sh
# Installs the amux binary for this machine from GitHub releases.
#
#   curl -fsSL https://github.com/quynb93/amux/releases/latest/download/install.sh | sh
#
# Environment:
#   AMUX_VERSION      version to install, e.g. 0.2.0 (default: latest)
#   AMUX_INSTALL_DIR  where to put the binary (default: ~/.local/bin)
set -eu

repo="quynb93/amux"
dir="${AMUX_INSTALL_DIR:-$HOME/.local/bin}"

fail() {
  echo "amux install: $*" >&2
  exit 1
}

case "$(uname -s)" in
  Darwin) os="apple-darwin" ;;
  Linux) os="unknown-linux-musl" ;;
  *) fail "unsupported OS $(uname -s). On Windows, use WSL." ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch="aarch64" ;;
  x86_64 | amd64) arch="x86_64" ;;
  *) fail "unsupported CPU $(uname -m)." ;;
esac
target="$arch-$os"

if [ -n "${AMUX_VERSION:-}" ]; then
  base="https://github.com/$repo/releases/download/v${AMUX_VERSION#v}"
else
  base="https://github.com/$repo/releases/latest/download"
fi

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
else
  fail "needs curl or wget."
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
archive="amux-$target.tar.gz"
echo "Downloading ${archive}..."
fetch "$base/$archive" "$tmp/$archive" || fail "download failed: $base/$archive"
fetch "$base/$archive.sha256" "$tmp/$archive.sha256" || fail "checksum download failed"

expected="$(cut -d' ' -f1 <"$tmp/$archive.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp/$archive" | cut -d' ' -f1)"
else
  actual="$(shasum -a 256 "$tmp/$archive" | cut -d' ' -f1)"
fi
[ "$expected" = "$actual" ] || fail "checksum mismatch for $archive"

tar -xzf "$tmp/$archive" -C "$tmp"
mkdir -p "$dir"
install -m 755 "$tmp/amux" "$dir/amux"
echo "Installed $("$dir/amux" --version | sed 's/^/amux /') to $dir/amux"

case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "Add $dir to your PATH, e.g.: echo 'export PATH=\"$dir:\$PATH\"' >> ~/.zshrc" ;;
esac
