#!/usr/bin/env bash
# Usage: formula.sh <tag> <dir>
# Prints the Homebrew formula for release <tag> (Formula/loadman.rb in the
# tap), with the checksums of the macOS and Linux archives in <dir>, named as
# package.sh names them.
set -euo pipefail
tag=$1 dir=$2
repo=${GITHUB_REPOSITORY:-tranhuuhuy297/loadman}
url() { echo "https://github.com/$repo/releases/download/$tag/loadman-$tag-$1.tar.gz"; }
sum() {
  local f="$dir/loadman-$tag-$1.tar.gz"
  if command -v sha256sum > /dev/null; then sha256sum "$f"; else shasum -a 256 "$f"; fi | cut -d' ' -f1
}
cat <<EOF
class Loadman < Formula
  desc "Load testing for Postman collections, from your terminal or CI"
  homepage "https://github.com/$repo"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    if Hardware::CPU.arm?
      url "$(url aarch64-apple-darwin)"
      sha256 "$(sum aarch64-apple-darwin)"
    else
      url "$(url x86_64-apple-darwin)"
      sha256 "$(sum x86_64-apple-darwin)"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "$(url aarch64-unknown-linux-gnu)"
      sha256 "$(sum aarch64-unknown-linux-gnu)"
    else
      url "$(url x86_64-unknown-linux-gnu)"
      sha256 "$(sum x86_64-unknown-linux-gnu)"
    end
  end

  def install
    bin.install "loadman"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/loadman --version")
    (testpath/"c.json").write <<~JSON
      {"info":{"name":"t"},"item":[{"name":"r","request":{"method":"GET","url":"http://127.0.0.1:9/"}}]}
    JSON
    assert_match "requests", shell_output("#{bin}/loadman inspect #{testpath}/c.json")
  end
end
EOF
