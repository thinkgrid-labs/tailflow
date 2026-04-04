#!/usr/bin/env bash
# Build the current platform's binaries and stage them into the npm package
# so you can test `npm pack` / `npx` locally before releasing.
#
# Usage: ./scripts/pack-local.sh [version]
#   version defaults to the version in npm/tailflow/package.json
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# ── Detect platform ────────────────────────────────────────────────────────
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS-$ARCH" in
  Darwin-arm64)   PLATFORM=darwin-arm64 ; TARGET=aarch64-apple-darwin ;;
  Darwin-x86_64)  PLATFORM=darwin-x64   ; TARGET=x86_64-apple-darwin ;;
  Linux-x86_64)   PLATFORM=linux-x64    ; TARGET=x86_64-unknown-linux-gnu ;;
  Linux-aarch64)  PLATFORM=linux-arm64  ; TARGET=aarch64-unknown-linux-gnu ;;
  *)
    echo "Unsupported platform: $OS-$ARCH"
    exit 1
    ;;
esac

echo "Platform: $PLATFORM  Target: $TARGET"

# ── Build web UI ───────────────────────────────────────────────────────────
echo ""
echo "==> Building web UI …"
(cd web && npm ci && npm run build)

# ── Compile Rust binaries ──────────────────────────────────────────────────
echo ""
echo "==> Building Rust binaries …"
cargo build --release --target "$TARGET" -p tailflow-tui -p tailflow-daemon

# ── Stage into npm platform package ───────────────────────────────────────
BIN_DIR="npm/platforms/$PLATFORM/bin"
mkdir -p "$BIN_DIR"
cp "target/$TARGET/release/tailflow"        "$BIN_DIR/tailflow"
cp "target/$TARGET/release/tailflow-daemon" "$BIN_DIR/tailflow-daemon"
chmod +x "$BIN_DIR/tailflow" "$BIN_DIR/tailflow-daemon"

echo ""
echo "==> Staged binaries:"
ls -lh "$BIN_DIR"

# ── Optional: bump version ─────────────────────────────────────────────────
VERSION="${1:-}"
if [[ -n "$VERSION" ]]; then
  echo ""
  echo "==> Bumping version to $VERSION …"
  node scripts/bump-version.js "$VERSION"
fi

# ── Pack ───────────────────────────────────────────────────────────────────
echo ""
echo "==> Packing @tailflow/$PLATFORM …"
(cd "npm/platforms/$PLATFORM" && npm pack)

echo ""
echo "==> Packing tailflow (main) …"
(cd npm/tailflow && npm pack)

echo ""
echo "Done. Test with:"
echo "  npm install -g npm/tailflow/tailflow-*.tgz"
echo "  tailflow --help"
