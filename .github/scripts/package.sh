#!/usr/bin/env bash
# Usage: package.sh <tag> <target>
# Packs target/<target>/release/loadman[.exe] with the README and licenses
# into loadman-<tag>-<target>.zip (Windows) or .tar.gz (everything else).
set -euo pipefail
tag=$1 target=$2
name="loadman-$tag-$target"
mkdir "$name"
cp README.md LICENSE-MIT LICENSE-APACHE "$name"/
if [[ "$target" == *windows* ]]; then
  cp "target/$target/release/loadman.exe" "$name"/
  7z a "$name.zip" "$name" > /dev/null
else
  cp "target/$target/release/loadman" "$name"/
  tar czf "$name.tar.gz" "$name"
fi
