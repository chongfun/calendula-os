#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/tools/s3-toolchain.env"
case "$(uname -s)/$(uname -m)" in
  Darwin/arm64) host=aarch64-apple-darwin; digest="$S3_ESPUP_DARWIN_SHA" ;;
  Linux/x86_64) host=x86_64-unknown-linux-gnu; digest="$S3_ESPUP_LINUX_SHA" ;;
  *) echo "S3 setup currently supports Apple Silicon macOS and x86_64 Linux." >&2; exit 2 ;;
esac
mkdir -p "$ROOT/target/toolchains"
verify_sha256() {
  local file="$1"
  local actual
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$file" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$file" | awk '{print $1}')"
  else
    echo "error: neither sha256sum nor shasum found to verify $file." >&2
    exit 1
  fi
  [ "$actual" = "$digest" ]
}

installer="$ROOT/target/toolchains/espup-$S3_ESPUP_VERSION"
if [ ! -f "$installer" ]; then
  tmp="$installer.tmp.$$"
  trap 'rm -f "$tmp"' EXIT
  curl --fail --location --retry 3 \
    "https://github.com/esp-rs/espup/releases/download/v$S3_ESPUP_VERSION/espup-$host" \
    -o "$tmp"
  if ! verify_sha256 "$tmp"; then
    echo "espup checksum mismatch for downloaded installer." >&2; exit 1
  fi
  mv "$tmp" "$installer"
  trap - EXIT
elif ! verify_sha256 "$installer"; then
  echo "espup checksum mismatch: remove $installer and retry." >&2; exit 1
fi
chmod +x "$installer"
"$installer" install --name "$S3_TOOLCHAIN" --toolchain-version "$S3_RUST_VERSION" \
  --targets esp32s3 --extended-llvm \
  --export-file "$ROOT/target/toolchains/export-esp.sh"
"$ROOT/tools/cargo-s3.sh" --version
