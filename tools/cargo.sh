#!/usr/bin/env bash
# Run Cargo with the toolchain rust-toolchain.toml pins and matching compiler tools.
#
# Some development machines have another cargo/rustc/rustdoc earlier on PATH.
# That can make firmware builds fail with "can't find crate for `core`" even
# when rustup has installed riscv32imc-unknown-none-elf. Force Cargo, rustc,
# and rustdoc to come from the same rustup toolchain.
set -euo pipefail

# The pin in rust-toolchain.toml is the default, so this script and a bare
# `cargo` in the repo agree on the compiler, and so does CI. The match
# tolerates TOML spacing, either quote style, and a trailing comment; if no
# channel parses at all, stop rather than quietly drift onto floating stable.
TOOLCHAIN_FILE="$(dirname "$0")/../rust-toolchain.toml"
PINNED="$(sed -n -E 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*["'"'"']([^"'"'"']+)["'"'"'].*$/\1/p' "$TOOLCHAIN_FILE" | head -n 1)"
if [ -z "${RUSTUP_TOOLCHAIN:-}" ] && [ -z "$PINNED" ]; then
  echo "error: no channel found in $TOOLCHAIN_FILE; set RUSTUP_TOOLCHAIN to override." >&2
  exit 1
fi
TOOLCHAIN="${RUSTUP_TOOLCHAIN:-$PINNED}"

if ! command -v rustup >/dev/null 2>&1; then
  cat >&2 <<'EOF'
error: rustup is required to build CalendulaOS firmware.

Install Rust with rustup, then install this repo's firmware target:
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  rustup target add riscv32imc-unknown-none-elf
EOF
  exit 127
fi

if ! CARGO="$(rustup which --toolchain "$TOOLCHAIN" cargo 2>/dev/null)"; then
  cat >&2 <<EOF
error: cargo is not installed for the '$TOOLCHAIN' rustup toolchain.

Install or repair the toolchain, then retry:
  rustup toolchain install $TOOLCHAIN --profile default
  rustup target add --toolchain $TOOLCHAIN riscv32imc-unknown-none-elf
EOF
  exit 127
fi

RUSTC="$(rustup which --toolchain "$TOOLCHAIN" rustc)"
RUSTDOC="$(rustup which --toolchain "$TOOLCHAIN" rustdoc)"
export RUSTC
export RUSTDOC

exec "$CARGO" "$@"
