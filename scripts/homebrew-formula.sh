#!/bin/sh
# Prints the Homebrew formula for a release, using the .sha256 files the release built.
#
#   scripts/homebrew-formula.sh 0.2.0 dist/ > Formula/amux.rb
set -eu

version="${1#v}"
dir="$2"
base="https://github.com/quynb93/amux/releases/download/v$version"

sha() {
  file="$dir/amux-$1.tar.gz.sha256"
  [ -s "$file" ] || { echo "missing $file" >&2; exit 1; }
  cut -d' ' -f1 <"$file"
}

cat <<EOF
class Amux < Formula
  desc "Run Claude Code, Codex and Gemini CLI with many accounts over one shared home"
  homepage "https://github.com/quynb93/amux"
  license "MIT"

  on_macos do
    on_arm do
      url "$base/amux-aarch64-apple-darwin.tar.gz"
      sha256 "$(sha aarch64-apple-darwin)"
    end
    on_intel do
      url "$base/amux-x86_64-apple-darwin.tar.gz"
      sha256 "$(sha x86_64-apple-darwin)"
    end
  end

  on_linux do
    on_arm do
      url "$base/amux-aarch64-unknown-linux-musl.tar.gz"
      sha256 "$(sha aarch64-unknown-linux-musl)"
    end
    on_intel do
      url "$base/amux-x86_64-unknown-linux-musl.tar.gz"
      sha256 "$(sha x86_64-unknown-linux-musl)"
    end
  end

  def install
    bin.install "amux"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/amux --version")
    ENV["AMUX_HOME"] = testpath/".amux"
    ENV["AMUX_NO_UPDATE_CHECK"] = "1"
    assert_match "No accounts yet", shell_output("#{bin}/amux account list")
  end
end
EOF
