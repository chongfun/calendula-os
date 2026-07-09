#!/usr/bin/env bash
# Build the browser emulator wasm for every supported board and stage the
# static site (web assets + one wasm per board) into OUT_DIR.
#
# The panel geometry is compile-time in the firmware, so each board is a
# separate wasm build. The crate always emits x4_web_emulator.wasm, so each
# board is built in turn and copied under a per-board name; index.html loads
# the right one from the ?board= query parameter (default x4).
#
# Usage: tools/build-web.sh [OUT_DIR]     (default: _site)
set -euo pipefail

OUT_DIR="${1:-_site}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MANIFEST="$ROOT/tools/web-emulator/Cargo.toml"
WASM="$ROOT/tools/web-emulator/target/wasm32-unknown-unknown/release/x4_web_emulator.wasm"

mkdir -p "$OUT_DIR"
cp -R "$ROOT/web/." "$OUT_DIR/"

build() {
  local board="$1"; shift
  cargo build --manifest-path "$MANIFEST" \
    --target wasm32-unknown-unknown --release "$@"
  cp "$WASM" "$OUT_DIR/${board}_web_emulator.wasm"
}

build x4
build x3 --features device-x3

echo "Wrote web assets and board wasm files to $OUT_DIR"
