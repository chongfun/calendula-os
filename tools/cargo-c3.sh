#!/usr/bin/env bash
# Select C3 target and memory policy without leaking either into host/S3 builds.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
COMMAND="${1:?usage: tools/cargo-c3.sh <build|check|clippy|run|rustc> [cargo arguments]}"
shift
case "$COMMAND" in
  build|check|clippy|run|rustc) ;;
  *) echo "unsupported C3 command: $COMMAND" >&2; exit 2 ;;
esac

# Cargo's external clippy subcommand does not retain a target supplied only
# by --config. Pass it to the command itself, as CI and direct builds must
# agree even when the workspace's default is the host.
exec "$ROOT/tools/cargo.sh" --config "$ROOT/fw-c3/cargo-config.toml" \
  "$COMMAND" --target riscv32imc-unknown-none-elf "$@"
