#!/usr/bin/env bash
# Publish Thoth to npm from a local build (host platform only).
#
# Use when you want a quick alpha/beta release without waiting on
# release.yml. Builds the host target, repacks it as a platform
# subpackage, and publishes it + the wrapper. The other 3 platform
# subpackages are NOT published — on those hosts the wrapper's
# optionalDependencies will fail to resolve and postinstall.js will
# fall through to its fallback path.
#
# Usage:
#   ./packaging/npm/publish-local.sh 0.0.1-alpha
#   DRY_RUN=1 ./packaging/npm/publish-local.sh 0.0.1-alpha
#   NPM_TAG=alpha ./packaging/npm/publish-local.sh 0.0.1-alpha
set -euo pipefail

VERSION="${1:?usage: publish-local.sh <version e.g. 0.0.1-alpha>}"
DRY_RUN="${DRY_RUN:-0}"
# Prereleases should NOT go to `latest`. Default to `alpha` when the
# version has a `-` suffix, else `latest`.
if [[ "$VERSION" == *-* ]]; then
  NPM_TAG="${NPM_TAG:-alpha}"
else
  NPM_TAG="${NPM_TAG:-latest}"
fi

cd "$(dirname "$0")/../.."
REPO_ROOT="$(pwd)"

# Detect host triple + npm platform/cpu.
HOST_OS="$(uname -s)"
HOST_ARCH="$(uname -m)"
case "$HOST_OS-$HOST_ARCH" in
  Darwin-arm64)   TRIPLE="aarch64-apple-darwin";       PLAT="darwin-arm64"; OS="darwin"; CPU="arm64" ;;
  Darwin-x86_64)  TRIPLE="x86_64-apple-darwin";        PLAT="darwin-x64";   OS="darwin"; CPU="x64"   ;;
  Linux-aarch64)  TRIPLE="aarch64-unknown-linux-gnu";  PLAT="linux-arm64";  OS="linux";  CPU="arm64" ;;
  Linux-x86_64)   TRIPLE="x86_64-unknown-linux-gnu";   PLAT="linux-x64";    OS="linux";  CPU="x64"   ;;
  *) echo "unsupported host: $HOST_OS-$HOST_ARCH" >&2; exit 1 ;;
esac

echo "→ host: $TRIPLE ($PLAT)  version: $VERSION  tag: $NPM_TAG  dry_run: $DRY_RUN" >&2

# 1. Build host release binaries.
echo "→ cargo build --release -p thoth-cli -p thoth-mcp" >&2
cargo build --release --target "$TRIPLE" -p thoth-cli -p thoth-mcp
BIN_DIR="$REPO_ROOT/target/$TRIPLE/release"
for b in thoth thoth-mcp thoth-gate; do
  [ -x "$BIN_DIR/$b" ] || { echo "missing binary: $BIN_DIR/$b" >&2; exit 1; }
done

# 2. Assemble platform subpackage.
WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

pkg="$WORKDIR/thoth-$PLAT"
mkdir -p "$pkg/bin"
cp "$BIN_DIR/thoth" "$BIN_DIR/thoth-mcp" "$BIN_DIR/thoth-gate" "$pkg/bin/"
chmod +x "$pkg/bin/"*

sed \
  -e "s|{PLATFORM}|$PLAT|g" \
  -e "s|{TRIPLE}|$TRIPLE|g" \
  -e "s|{OS}|$OS|g" \
  -e "s|{CPU}|$CPU|g" \
  packaging/npm/platform-stubs/template/package.json > "$pkg/package.json"
jq --arg v "$VERSION" '.version = $v' "$pkg/package.json" > "$pkg/package.json.tmp"
mv "$pkg/package.json.tmp" "$pkg/package.json"

# 3. Assemble wrapper package.
wrapper="$WORKDIR/thoth"
cp -R packaging/npm/thoth "$wrapper"
jq --arg v "$VERSION" '
  .version = $v
  | .optionalDependencies |= (to_entries | map(.value = $v) | from_entries)
' "$wrapper/package.json" > "$wrapper/package.json.tmp"
mv "$wrapper/package.json.tmp" "$wrapper/package.json"

publish() {
  local dir="$1"
  local name
  name=$(jq -r .name "$dir/package.json")
  echo "→ publishing $name@$VERSION ($NPM_TAG)" >&2
  if [ "$DRY_RUN" = "1" ]; then
    (cd "$dir" && npm pack --dry-run >&2)
  else
    (cd "$dir" && npm publish --access public --tag "$NPM_TAG")
  fi
}

publish "$pkg"
publish "$wrapper"

echo "✓ done: @unknownstudio/thoth@$VERSION + @unknownstudio/thoth-$PLAT@$VERSION" >&2
