#!/usr/bin/env bash
# Fetch SHA256s from a released tag and stamp them into thoth.rb.
#
# Usage:
#   ./packaging/homebrew/bump.sh v0.0.1
#
# Prints a ready-to-commit formula to stdout.
set -euo pipefail

TAG="${1:?usage: bump.sh vX.Y.Z}"
VERSION="${TAG#v}"
REPO="unknown-studio-dev/thoth"
BASE="https://github.com/${REPO}/releases/download/${TAG}"

targets=(
  "aarch64-apple-darwin"
  "x86_64-apple-darwin"
  "aarch64-unknown-linux-gnu"
  "x86_64-unknown-linux-gnu"
)

declare -A SHA
for t in "${targets[@]}"; do
  url="${BASE}/thoth-${VERSION}-${t}.tar.gz.sha256"
  echo "fetching $url" >&2
  SHA[$t]=$(curl -fsSL "$url" | awk '{print $1}')
done

sed \
  -e "s|^  version \".*\"|  version \"${VERSION}\"|" \
  -e "0,/REPLACE_WITH_SHA256_FROM_RELEASE_ASSET/s||${SHA[aarch64-apple-darwin]}|" \
  -e "0,/REPLACE_WITH_SHA256_FROM_RELEASE_ASSET/s||${SHA[x86_64-apple-darwin]}|" \
  -e "0,/REPLACE_WITH_SHA256_FROM_RELEASE_ASSET/s||${SHA[aarch64-unknown-linux-gnu]}|" \
  -e "0,/REPLACE_WITH_SHA256_FROM_RELEASE_ASSET/s||${SHA[x86_64-unknown-linux-gnu]}|" \
  "$(dirname "$0")/thoth.rb"
