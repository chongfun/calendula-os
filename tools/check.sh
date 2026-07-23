#!/usr/bin/env bash
set -eo pipefail

if [ -z "${HOST_TARGET:-}" ]; then
    if ! HOST_TARGET="$(rustc -vV | sed -n 's/^host: //p')" ||
       [ -z "$HOST_TARGET" ]; then
        echo "Error: Failed to detect HOST_TARGET. Is rustc installed?" >&2
        exit 1
    fi
fi

COMMAND="$1"
shift || true

case "$COMMAND" in
    fmt)
        echo "Running formatting checks..."
        cargo fmt --all -- --check
        ;;
    clippy-host)
        echo "Running host clippy..."
        cargo clippy --workspace --exclude hal-ext --exclude fw \
            --target "$HOST_TARGET" --all-targets -- -D warnings
        ;;
    clippy-firmware)
        echo "Running firmware clippy for X4..."
        tools/cargo.sh clippy -p fw -- -D warnings
        
        echo "Running firmware clippy for X3..."
        tools/cargo.sh clippy -p fw --features device-x3 -- -D warnings
        ;;
    test-host)
        echo "Running host tests..."
        cargo test --workspace --exclude hal-ext --exclude fw --target "$HOST_TARGET"
        ;;
    test-host-x3)
        # The UC8253 panel driver is behind `device-x3`, so its tests -- the
        # controller step plans among them -- are invisible to the run above.
        # Scoped to `display` on purpose: `device-x3` flips the whole workspace
        # to X3 geometry, which the golden frames are not written for.
        echo "Running X3 host tests..."
        cargo test -p display --features device-x3 --target "$HOST_TARGET"
        ;;
    golden-frames)
        echo "Checking emulator golden frames for X4..."
        cargo run --manifest-path tools/emulator/Cargo.toml --target "$HOST_TARGET" \
            --target-dir tools/emulator/target/x4 \
            --no-default-features -- --scenario fixtures/scenarios --check fixtures/golden
        
        echo "Checking emulator golden frames for X3..."
        cargo run --manifest-path tools/emulator/Cargo.toml --target "$HOST_TARGET" \
            --target-dir tools/emulator/target/x3 \
            --no-default-features --features device-x3 -- --scenario fixtures/scenarios --check fixtures/golden
        ;;
    test-emulator)
        echo "Running emulator tests (including reading goldens) for X4..."
        cargo test --manifest-path tools/emulator/Cargo.toml --target "$HOST_TARGET" \
            --target-dir tools/emulator/target/x4 \
            --no-default-features
            
        echo "Running emulator tests (including reading goldens) for X3..."
        cargo test --manifest-path tools/emulator/Cargo.toml --target "$HOST_TARGET" \
            --target-dir tools/emulator/target/x3 \
            --no-default-features --features device-x3
        ;;
    build-firmware)
        echo "Building firmware for X4..."
        tools/cargo.sh build -p fw --release
        
        echo "Building firmware for X3..."
        tools/cargo.sh build -p fw --release --features device-x3
        ;;
    fast)
        "$0" fmt
        "$0" clippy-host
        "$0" test-host
        "$0" test-host-x3
        ;;
    emulator)
        "$0" golden-frames
        "$0" test-emulator
        ;;
    firmware)
        "$0" clippy-firmware
        "$0" build-firmware
        ;;
    all)
        "$0" fast
        "$0" emulator
        "$0" firmware
        ;;
    *)
        echo "Usage: $0 {fmt|clippy-host|clippy-firmware|test-host|test-host-x3|golden-frames|test-emulator|build-firmware|fast|emulator|firmware|all}"
        echo "  'all' runs all required root/firmware verification."
        exit 1
        ;;
esac
