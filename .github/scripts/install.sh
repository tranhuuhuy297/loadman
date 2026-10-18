#!/usr/bin/env bash
# Installs a loadman release binary for the current runner and adds it to PATH.
# Used by action.yml. Env: VERSION (tag or empty), ACTION_REF, ACTION_REPO, GH_TOKEN.
set -euo pipefail
repo=${ACTION_REPO:-tranhuuhuy297/loadman}
tag=${VERSION:-}
if [ -z "$tag" ] && [[ "${ACTION_REF:-}" =~ ^v[0-9] ]]; then tag=$ACTION_REF; fi
if [ -z "$tag" ]; then tag=$(gh release view -R "$repo" --json tagName --jq .tagName); fi

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-gnu ;;
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  MINGW* | MSYS* | CYGWIN*) target=x86_64-pc-windows-msvc ;;
  *) echo "loadman: no release binary for $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac

dir="$RUNNER_TEMP/loadman-$tag"
mkdir -p "$dir"
cd "$dir"
name="loadman-$tag-$target"
if [[ "$target" == *windows* ]]; then
  gh release download "$tag" -R "$repo" -p "$name.zip" --clobber
  unzip -oq "$name.zip"
else
  gh release download "$tag" -R "$repo" -p "$name.tar.gz" --clobber
  tar xzf "$name.tar.gz"
fi
echo "$dir/$name" >> "$GITHUB_PATH"
"$dir/$name/loadman" --version
