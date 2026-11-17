#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/tools/s3-toolchain.env"
exports="$ROOT/target/toolchains/export-esp.sh"
if [ ! -f "$exports" ]; then
  echo "S3 environment missing; run tools/setup-s3.sh." >&2; exit 1
fi
source "$exports"
if ! compiler="$(rustup run "$S3_TOOLCHAIN" rustc -vV)"; then
  echo "S3 compiler missing; run tools/setup-s3.sh." >&2; exit 1
fi
if ! printf '%s\n' "$compiler" | grep -qx "commit-hash: $S3_RUST_COMMIT"; then
  echo "S3 compiler does not match tools/s3-toolchain.env; run tools/setup-s3.sh." >&2; exit 1
fi
if [ ! -x "$(dirname "$CLANG_PATH")/llvm-objdump" ]; then
  echo "Xtensa LLVM tools missing; rerun tools/setup-s3.sh (--extended-llvm)." >&2; exit 1
fi
if [ "${1:-}" = --version ]; then
  printf '%s\n' "$compiler"
  exit 0
fi
COMMAND="${1:?usage: tools/cargo-s3.sh <build|check|clippy|metadata|rustc|tree> [cargo arguments]}"
shift
case "$COMMAND" in
  build|check|clippy|metadata|rustc|tree) ;;
  *) echo "unsupported S3 command: $COMMAND" >&2; exit 2 ;;
esac
export RUSTUP_TOOLCHAIN="$S3_TOOLCHAIN"
export CARGO_UNSTABLE_BUILD_STD="core,alloc"
if [ "$COMMAND" = "metadata" ]; then
  exec "$ROOT/tools/cargo.sh" --config "$ROOT/fw-s3/cargo-config.toml" \
    "$COMMAND" --manifest-path "$ROOT/fw-s3/Cargo.toml" "$@"
fi
exec "$ROOT/tools/cargo.sh" --config "$ROOT/fw-s3/cargo-config.toml" \
  "$COMMAND" --manifest-path "$ROOT/fw-s3/Cargo.toml" \
  --target xtensa-esp32s3-none-elf "$@"
