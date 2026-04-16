#!/usr/bin/env bash
# local-smoke.sh — chạy smoke-test ở local, KHÔNG đụng tag, KHÔNG publish.
#
# Usage:
#   ./scripts/local-smoke.sh                   # full (Rust + binary + npm)
#   ./scripts/local-smoke.sh rust              # chỉ step 1
#   ./scripts/local-smoke.sh build             # step 1 + 2 + 3
#   ./scripts/local-smoke.sh npm               # step 4 + 5 + 6 (cần binary đã build)
#   VERSION=0.0.1-dev ./scripts/local-smoke.sh # override version
#
# Biến env:
#   VERSION         default: 0.0.1-dev
#   SKIP_CLIPPY=1   bỏ qua clippy nếu muốn chạy nhanh
#   KEEP_TEST_DIR=1 giữ /tmp/thoth-smoke sau khi chạy (để debug)

set -euo pipefail

# ───── paths & config ─────────────────────────────────────────────────────────
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${VERSION:-0.0.1-dev}"
TARGET="$(rustc -vV | awk '/^host/ {print $2}')"
TEST_DIR="/tmp/thoth-smoke"
DIST="$REPO_ROOT/dist"
NAME="thoth-${VERSION}-${TARGET}"

# map rust triple → npm platform slug
case "$TARGET" in
  aarch64-apple-darwin)        PLAT=darwin-arm64 ; NODE_OS=darwin ; NODE_CPU=arm64 ;;
  x86_64-apple-darwin)         PLAT=darwin-x64   ; NODE_OS=darwin ; NODE_CPU=x64   ;;
  aarch64-unknown-linux-gnu)   PLAT=linux-arm64  ; NODE_OS=linux  ; NODE_CPU=arm64 ;;
  x86_64-unknown-linux-gnu)    PLAT=linux-x64    ; NODE_OS=linux  ; NODE_CPU=x64   ;;
  *) echo "unsupported host triple: $TARGET" >&2 ; exit 1 ;;
esac

# ───── pretty logging ─────────────────────────────────────────────────────────
c_green=$'\e[32m'; c_cyan=$'\e[36m'; c_red=$'\e[31m'; c_yel=$'\e[33m'; c_dim=$'\e[2m'; c_rst=$'\e[0m'
step() { printf "\n${c_cyan}▶ %s${c_rst}\n" "$*"; }
ok()   { printf "${c_green}✓ %s${c_rst}\n" "$*"; }
warn() { printf "${c_yel}! %s${c_rst}\n" "$*"; }
die()  { printf "${c_red}✗ %s${c_rst}\n" "$*" >&2; exit 1; }

mode="${1:-full}"
run_rust=0; run_build=0; run_npm=0
case "$mode" in
  full)    run_rust=1; run_build=1; run_npm=1 ;;
  rust)    run_rust=1 ;;
  build)   run_rust=1; run_build=1 ;;
  npm)     run_build=1; run_npm=1 ;;
  *) die "unknown mode: $mode (full|rust|build|npm)" ;;
esac

echo "${c_dim}repo=$REPO_ROOT${c_rst}"
echo "${c_dim}target=$TARGET  platform=$PLAT  version=$VERSION${c_rst}"

cd "$REPO_ROOT"

# ───── step 1 — Rust workspace ────────────────────────────────────────────────
if [[ $run_rust -eq 1 ]]; then
  step "1/7 — cargo fmt-check"
  cargo fmt --all -- --check
  ok "fmt ok"

  if [[ "${SKIP_CLIPPY:-0}" != "1" ]]; then
    step "2/7 — cargo clippy (-D warnings)"
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    ok "clippy ok"
  else
    warn "clippy skipped (SKIP_CLIPPY=1)"
  fi

  step "3/7 — cargo test (workspace)"
  cargo test --workspace --all-features
  ok "tests ok"

  step "3b/7 — feature matrix cho thoth-domain"
  cargo check -p thoth-domain --no-default-features
  cargo check -p thoth-domain --features file
  cargo check -p thoth-domain --features "file notion asana notebooklm"
  cargo check -p thoth-cli    --features "notion asana notebooklm"
  ok "feature matrix ok"
fi

# ───── step 2+3 — build binaries cho host triple, tar, sha256, sanity ─────────
if [[ $run_build -eq 1 ]]; then
  step "4/7 — cargo build --release (host: $TARGET)"
  cargo build --release --target "$TARGET" -p thoth-cli -p thoth-mcp
  ok "binaries built"

  step "5/7 — stage + tar + sha256 (mô phỏng release.yml)"
  rm -rf "$DIST/$NAME" "$DIST/${NAME}.tar.gz" "$DIST/${NAME}.tar.gz.sha256"
  mkdir -p "$DIST/$NAME"
  cp "target/${TARGET}/release/thoth"      "$DIST/$NAME/"
  cp "target/${TARGET}/release/thoth-mcp"  "$DIST/$NAME/"
  cp "target/${TARGET}/release/thoth-gate" "$DIST/$NAME/" 2>/dev/null || warn "thoth-gate not built — skipping"
  cp README.md LICENSE-MIT LICENSE-APACHE  "$DIST/$NAME/" 2>/dev/null || true
  tar -C "$DIST" -czf "$DIST/${NAME}.tar.gz" "$NAME"
  (cd "$DIST" && shasum -a 256 "${NAME}.tar.gz" | tee "${NAME}.tar.gz.sha256")
  ok "tarball: $DIST/${NAME}.tar.gz"

  step "6/7 — sanity-check binaries"
  "$DIST/$NAME/thoth"      --version
  "$DIST/$NAME/thoth-mcp"  --version
  if [[ -x "$DIST/$NAME/thoth-gate" ]]; then
    echo '{}' | "$DIST/$NAME/thoth-gate" || warn "thoth-gate exit non-zero (check input schema)"
  fi
  ok "binaries chạy được"
fi

# ───── step 4+5+6 — npm wrapper + platform stub + local install ───────────────
if [[ $run_npm -eq 1 ]]; then
  step "7/7 — npm pack (wrapper)"
  cd "$REPO_ROOT/packaging/npm/thoth"
  rm -f unknownstudio-thoth-*.tgz
  npm pack --silent >/dev/null
  WRAPPER_TGZ="$(ls unknownstudio-thoth-*.tgz | head -1)"
  [[ -n "$WRAPPER_TGZ" ]] || die "npm pack không tạo tarball"
  ok "wrapper tarball: $WRAPPER_TGZ"
  echo "${c_dim}── file list ──${c_rst}"
  tar -tzf "$WRAPPER_TGZ" | sed 's/^/  /'

  step "7b/7 — render platform stub cho $PLAT và npm pack"
  STUB_DIR="/tmp/thoth-stub-$PLAT"
  rm -rf "$STUB_DIR"
  cp -R "$REPO_ROOT/packaging/npm/platform-stubs/template" "$STUB_DIR"

  # sed -i khác nhau giữa BSD/GNU → dùng perl cho portable
  perl -pi -e "s/\\{PLATFORM\\}/$PLAT/g;
               s/\\{VERSION\\}/$VERSION/g;
               s/\\{TRIPLE\\}/$TARGET/g;
               s/\\{OS\\}/$NODE_OS/g;
               s/\\{CPU\\}/$NODE_CPU/g" "$STUB_DIR/package.json"

  mkdir -p "$STUB_DIR/bin"
  cp "$DIST/$NAME/thoth"      "$STUB_DIR/bin/"
  cp "$DIST/$NAME/thoth-mcp"  "$STUB_DIR/bin/"
  cp "$DIST/$NAME/thoth-gate" "$STUB_DIR/bin/" 2>/dev/null || true
  chmod +x "$STUB_DIR/bin/"*

  (cd "$STUB_DIR" && npm pack --silent >/dev/null)
  STUB_TGZ="$(ls "$STUB_DIR"/unknownstudio-thoth-*.tgz | head -1)"
  [[ -n "$STUB_TGZ" ]] || die "stub npm pack không tạo tarball"
  ok "stub tarball: $STUB_TGZ"

  step "7c/7 — install wrapper + stub vào $TEST_DIR và test shim"
  rm -rf "$TEST_DIR"
  mkdir -p "$TEST_DIR"
  (cd "$TEST_DIR" && npm init -y >/dev/null)
  # install stub TRƯỚC để optionalDependencies của wrapper resolve được
  (cd "$TEST_DIR" && npm install --silent --no-audit --no-fund "$STUB_TGZ")
  (cd "$TEST_DIR" && npm install --silent --no-audit --no-fund \
      "$REPO_ROOT/packaging/npm/thoth/$WRAPPER_TGZ")

  echo "${c_dim}── installed binaries ──${c_rst}"
  "$TEST_DIR/node_modules/.bin/thoth" --version
  "$TEST_DIR/node_modules/.bin/thoth-mcp" --version
  ok "shim resolve đúng @unknownstudio/thoth-$PLAT"

  if [[ "${KEEP_TEST_DIR:-0}" != "1" ]]; then
    rm -rf "$TEST_DIR" "$STUB_DIR"
    rm -f "$REPO_ROOT/packaging/npm/thoth/unknownstudio-thoth-"*.tgz
  else
    warn "KEEP_TEST_DIR=1 — giữ $TEST_DIR và $STUB_DIR để debug"
  fi
fi

echo
printf "${c_green}════════ smoke-test passed — mode=%s ════════${c_rst}\n" "$mode"
