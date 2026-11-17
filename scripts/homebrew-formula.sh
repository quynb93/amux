#!/bin/sh
# Prints the Homebrew formula for a release, using the .sha256 files the release built.
#
#   scripts/homebrew-formula.sh 0.2.0 dist/ > Formula/amux.rb
set -eu

version="${1#v}"
dir="$2"
base="https://github.com/quynb93/amux/releases/download/v$version"

# Fail before printing anything when a checksum is missing: inside $(...) an exit
# would only end the subshell and leave an empty sha256 in the formula.
for f in \
  "amux-$version.arm64_big_sur.bottle.tar.gz" "amux-$version.big_sur.bottle.tar.gz" \
  "amux-$version.arm64_linux.bottle.tar.gz" "amux-$version.x86_64_linux.bottle.tar.gz" \
  amux-aarch64-apple-darwin.tar.gz amux-x86_64-apple-darwin.tar.gz \
  amux-aarch64-unknown-linux-musl.tar.gz amux-x86_64-unknown-linux-musl.tar.gz; do
  [ -s "$dir/$f.sha256" ] || { echo "missing $dir/$f.sha256 (run scripts/homebrew-bottles.sh first)" >&2; exit 1; }
done

sha() {
  file="$dir/$1.sha256"
  [ -s "$file" ] || { echo "missing $file" >&2; exit 1; }
  cut -d' ' -f1 <"$file"
}

# Bottles from scripts/homebrew-bottles.sh, so brew pours the binary instead of treating
# the formula as a source build (which needs up-to-date Command Line Tools).
bottle() {
  echo "    sha256 cellar: :any_skip_relocation, $1: \"$(sha "amux-$version.$1.bottle.tar.gz")\""
}

cat <<EOF
class Amux < Formula
  desc "Run Claude Code, Codex and Gemini CLI with many accounts over one shared home"
  homepage "https://github.com/quynb93/amux"
  license "MIT"

  bottle do
    root_url "$base"
$(bottle arm64_big_sur)
$(bottle big_sur)
$(bottle arm64_linux)
$(bottle x86_64_linux)
  end

  on_macos do
    on_arm do
      url "$base/amux-aarch64-apple-darwin.tar.gz"
      sha256 "$(sha amux-aarch64-apple-darwin.tar.gz)"
    end
    on_intel do
      url "$base/amux-x86_64-apple-darwin.tar.gz"
      sha256 "$(sha amux-x86_64-apple-darwin.tar.gz)"
    end
  end

  on_linux do
    on_arm do
      url "$base/amux-aarch64-unknown-linux-musl.tar.gz"
      sha256 "$(sha amux-aarch64-unknown-linux-musl.tar.gz)"
    end
    on_intel do
      url "$base/amux-x86_64-unknown-linux-musl.tar.gz"
      sha256 "$(sha amux-x86_64-unknown-linux-musl.tar.gz)"
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
