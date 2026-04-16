#!/usr/bin/env bash
# install-local.sh — build & cài Thoth lên máy để test thử.
#
# 2 chế độ:
#
#   cargo  : `cargo install --path ...` → binary vào ~/.cargo/bin/
#            Nhanh nhất, không cần npm. Khuyến nghị cho dev loop.
#
#   npm    : build binary host → render platform stub + wrapper →
#            `npm install -g` cả 2 tarball → binary vào prefix của npm.
#            Dùng khi muốn test đúng con đường user thật sẽ đi qua
#            (`npm i -g @unknownstudio/thoth`).
#
# Usage:
#   ./scripts/install-local.sh cargo           # mặc định
#   ./scripts/install-local.sh npm
#   ./scripts/install-local.sh uninstall       # gỡ cả 2 đường
#
# Env:
#   VERSION=0.0.1-dev      # chỉ dùng cho chế độ npm

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${VERSION:-0.0.1-dev}"
TARGET="$(rustc -vV | awk '/^host/ {print $2}')"

case "$TARGET" in
  aarch64-apple-darwin)        PLAT=darwin-arm64 ; NODE_OS=darwin ; NODE_CPU=arm64 ;;
  x86_64-apple-darwin)         PLAT=darwin-x64   ; NODE_OS=darwin ; NODE_CPU=x64   ;;
  aarch64-unknown-linux-gnu)   PLAT=linux-arm64  ; NODE_OS=linux  ; NODE_CPU=arm64 ;;
  x86_64-unknown-linux-gnu)    PLAT=linux-x64    ; NODE_OS=linux  ; NODE_CPU=x64   ;;
  *) echo "unsupported host triple: $TARGET" >&2 ; exit 1 ;;
esac

c_green=$'\e[32m'; c_cyan=$'\e[36m'; c_yel=$'\e[33m'; c_dim=$'\e[2m'; c_rst=$'\e[0m'
step() { printf "\n${c_cyan}▶ %s${c_rst}\n" "$*"; }
ok()   { printf "${c_green}✓ %s${c_rst}\n" "$*"; }
warn() { printf "${c_yel}! %s${c_rst}\n" "$*"; }

mode="${1:-cargo}"
cd "$REPO_ROOT"

# ──────────────────────────────────────────────────────────────────────────────
install_cargo() {
  step "cargo install thoth-cli (bin: thoth) + thoth-mcp (bins: thoth-mcp, thoth-gate)"
  cargo install --path crates/thoth-cli --force --locked
  # thoth-mcp có 2 bin: thoth-mcp + thoth-gate → cài cả hai qua --bins
  cargo install --path crates/thoth-mcp --force --locked --bins

  BIN_DIR="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin"

  ok "binary đã cài vào: $BIN_DIR"
  step "sanity check"
  "$BIN_DIR/thoth" --version
  # thoth-mcp là stdio server, không handle --version → chỉ kiểm tra file tồn tại
  [[ -x "$BIN_DIR/thoth-mcp"  ]] && ok "$BIN_DIR/thoth-mcp đã cài"
  [[ -x "$BIN_DIR/thoth-gate" ]] && ok "$BIN_DIR/thoth-gate đã cài"

  cat <<EOF

${c_green}✓ xong — để dùng:${c_rst}
    export PATH="$BIN_DIR:\$PATH"   # nếu chưa có
    thoth --version

Trên một project bất kỳ:
    cd /path/to/your/project
    thoth setup                # wizard: hooks + MCP + skills + .thoth/
    thoth index .              # index codebase
    thoth query "hybrid recall"

Gỡ:   ./scripts/install-local.sh uninstall  (hoặc: cargo uninstall thoth-cli thoth-mcp)
EOF
}

uninstall_cargo() {
  step "cargo uninstall thoth-cli / thoth-mcp / thoth-gate"
  cargo uninstall thoth-cli  || true
  cargo uninstall thoth-mcp  || true
  cargo uninstall thoth-gate || true
  ok "cargo binary đã gỡ"
}

# ──────────────────────────────────────────────────────────────────────────────
install_npm() {
  step "build release binary cho host ($TARGET)"
  # `-p thoth-mcp` build cả 2 bin: thoth-mcp + thoth-gate
  cargo build --release --target "$TARGET" -p thoth-cli -p thoth-mcp

  TGT_DIR="target/${TARGET}/release"

  step "render platform stub @unknownstudio/thoth-$PLAT và pack"
  STUB_DIR="/tmp/thoth-stub-$PLAT"
  rm -rf "$STUB_DIR"
  cp -R packaging/npm/platform-stubs/template "$STUB_DIR"

  perl -pi -e "s/\\{PLATFORM\\}/$PLAT/g;
               s/\\{VERSION\\}/$VERSION/g;
               s/\\{TRIPLE\\}/$TARGET/g;
               s/\\{OS\\}/$NODE_OS/g;
               s/\\{CPU\\}/$NODE_CPU/g" "$STUB_DIR/package.json"

  mkdir -p "$STUB_DIR/bin"
  cp "$TGT_DIR/thoth"      "$STUB_DIR/bin/"
  cp "$TGT_DIR/thoth-mcp"  "$STUB_DIR/bin/"
  cp "$TGT_DIR/thoth-gate" "$STUB_DIR/bin/" 2>/dev/null || true
  chmod +x "$STUB_DIR/bin/"*

  (cd "$STUB_DIR" && npm pack --silent >/dev/null)
  STUB_TGZ="$(ls "$STUB_DIR"/unknownstudio-thoth-*.tgz | head -1)"

  step "pack wrapper @unknownstudio/thoth@$VERSION"
  # tạm thời sync version của wrapper cho khớp nếu user override VERSION
  WRAPPER_SRC="packaging/npm/thoth"
  WRAPPER_TMP="/tmp/thoth-wrapper"
  rm -rf "$WRAPPER_TMP"
  cp -R "$WRAPPER_SRC" "$WRAPPER_TMP"
  perl -pi -e 's/"version":\s*"[^"]+"/"version": "'"$VERSION"'"/' "$WRAPPER_TMP/package.json"
  perl -pi -e 's/"@unknownstudio\/thoth-([a-z0-9-]+)":\s*"[^"]+"/"@unknownstudio\/thoth-$1": "'"$VERSION"'"/g' "$WRAPPER_TMP/package.json"
  (cd "$WRAPPER_TMP" && npm pack --silent >/dev/null)
  WRAPPER_TGZ="$(ls "$WRAPPER_TMP"/unknownstudio-thoth-*.tgz | head -1)"

  step "npm install -g stub + wrapper"
  # stub trước để optionalDependency resolve được local
  npm install -g --no-audit --no-fund "$STUB_TGZ"
  npm install -g --no-audit --no-fund "$WRAPPER_TGZ"

  NPM_BIN="$(npm bin -g 2>/dev/null || npm prefix -g)/bin"
  # một số setup (nvm, volta) trả path khác — thử cả 2
  [[ -x "$NPM_BIN/thoth" ]] || NPM_BIN="$(npm prefix -g)/bin"

  ok "installed globally — bin dir: $NPM_BIN"
  step "sanity check"
  thoth --version
  # thoth-mcp là stdio server, không có --version → check file tồn tại + executable
  [[ -x "$NPM_BIN/thoth-mcp"  ]] && ok "thoth-mcp đã cài ($NPM_BIN/thoth-mcp)"
  [[ -x "$NPM_BIN/thoth-gate" ]] && ok "thoth-gate đã cài ($NPM_BIN/thoth-gate)"

  cat <<EOF

${c_green}✓ xong — \`thoth\` đã global thật sự:${c_rst}
    thoth --version
    which thoth        # $NPM_BIN/thoth

Trên một project bất kỳ:
    cd /path/to/your/project
    thoth setup                # wizard: hooks + MCP + skills + .thoth/
    thoth index .              # index codebase

Gỡ:   ./scripts/install-local.sh uninstall
EOF
}

uninstall_npm() {
  step "npm uninstall -g @unknownstudio/thoth + các platform stub"
  npm uninstall -g @unknownstudio/thoth 2>/dev/null || true
  for p in darwin-arm64 darwin-x64 linux-arm64 linux-x64; do
    npm uninstall -g "@unknownstudio/thoth-$p" 2>/dev/null || true
  done
  # legacy thoth-cc cleanup (best-effort, in case máy còn vết cài cũ)
  npm uninstall -g @unknownstudio/thoth-cc 2>/dev/null || true
  for p in darwin-arm64 darwin-x64 linux-arm64 linux-x64; do
    npm uninstall -g "@unknownstudio/thoth-cc-$p" 2>/dev/null || true
  done
  rm -rf /tmp/thoth-stub-* /tmp/thoth-wrapper
  ok "npm global đã gỡ"
}

# ──────────────────────────────────────────────────────────────────────────────
case "$mode" in
  cargo)     install_cargo ;;
  npm)       install_npm   ;;
  uninstall) uninstall_cargo ; uninstall_npm ;;
  *) echo "usage: $0 [cargo|npm|uninstall]" >&2 ; exit 1 ;;
esac
