#!/usr/bin/env bash
# Publish @unknownstudio/thoth-cc + platform subpackages to npm.
#
# Run after a GitHub release is published. Downloads the release tarballs,
# re-packs them as npm packages, and publishes all four platform subpackages
# plus the main wrapper. All packages are scoped under `@unknownstudio`;
# `--access public` is required to publish a scoped package publicly.
#
# Requires: gh, npm (logged in, with publish rights to @unknownstudio),
# jq, tar.
#
# Usage:
#   ./packaging/npm/publish.sh v0.2.0
#   NPM_TAG=latest ./packaging/npm/publish.sh v0.2.0         # default
#   DRY_RUN=1      ./packaging/npm/publish.sh v0.2.0         # no publish
set -euo pipefail

TAG="${1:?usage: publish.sh vX.Y.Z}"
VERSION="${TAG#v}"
REPO="unknown-studio-dev/thoth"
NPM_TAG="${NPM_TAG:-latest}"
DRY_RUN="${DRY_RUN:-0}"

cd "$(dirname "$0")"
WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

# (platform, triple, os, cpu)
platforms=(
  "darwin-arm64:aarch64-apple-darwin:darwin:arm64"
  "darwin-x64:x86_64-apple-darwin:darwin:x64"
  "linux-arm64:aarch64-unknown-linux-gnu:linux:arm64"
  "linux-x64:x86_64-unknown-linux-gnu:linux:x64"
)

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

# 1. Platform subpackages.
for entry in "${platforms[@]}"; do
  IFS=: read -r plat triple os cpu <<<"$entry"
  # Directory name uses a flat form so mktemp/path lookups stay simple;
  # the package's true name (set via template substitution below) is the
  # scoped `@unknownstudio/thoth-cc-<plat>`.
  pkg="thoth-cc-${plat}"
  out="$WORKDIR/$pkg"
  mkdir -p "$out/bin"

  tarball="thoth-${VERSION}-${triple}.tar.gz"
  echo "downloading $tarball" >&2
  gh release download "$TAG" --repo "$REPO" --pattern "$tarball" --dir "$WORKDIR"
  tar -xzf "$WORKDIR/$tarball" -C "$WORKDIR"
  extracted="$WORKDIR/thoth-${VERSION}-${triple}"
  cp "$extracted/thoth" "$extracted/thoth-mcp" "$extracted/thoth-gate" "$out/bin/"
  chmod +x "$out/bin/"*

  sed \
    -e "s|{PLATFORM}|${plat}|g" \
    -e "s|{TRIPLE}|${triple}|g" \
    -e "s|{OS}|${os}|g" \
    -e "s|{CPU}|${cpu}|g" \
    platform-stubs/template/package.json > "$out/package.json"
  # bump version to match TAG (template hardcodes 0.2.0 — override).
  jq --arg v "$VERSION" '.version = $v' "$out/package.json" > "$out/package.json.tmp"
  mv "$out/package.json.tmp" "$out/package.json"

  publish "$out"
done

# 2. Main wrapper package — update version and publish.
wrapper="$WORKDIR/thoth-cc"
cp -R thoth-cc "$wrapper"
jq --arg v "$VERSION" '
  .version = $v
  | .optionalDependencies |= (to_entries | map(.value = $v) | from_entries)
' "$wrapper/package.json" > "$wrapper/package.json.tmp"
mv "$wrapper/package.json.tmp" "$wrapper/package.json"

publish "$wrapper"

echo "✓ published @unknownstudio/thoth-cc@${VERSION} + 4 platform packages" >&2
