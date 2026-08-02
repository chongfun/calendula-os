#!/usr/bin/env bash
set -eo pipefail

if [ -z "${HOST_TARGET:-}" ]; then
    if ! HOST_TARGET="$(rustc -vV | sed -n 's/^host: //p')" ||
       [ -z "$HOST_TARGET" ]; then
        echo "Error: Failed to detect HOST_TARGET. Is rustc installed?" >&2
        exit 1
    fi
fi

# The one interpreter this repo's Python runs on, from .python-version. There
# is no fallback path any more: the bench harness imports `tomllib` directly,
# so an older python does not degrade, it fails at import. `python3` is not
# assumed to be it -- macOS ships 3.9 under that name -- so a versioned binary
# is preferred and the exact version is checked before anything uses it.
#
# Called only by the targets that run Python. Resolving it up front made every
# Rust-only target -- fmt, both clippys, the host tests, the golden frames --
# refuse to start without 3.14 installed, which broke five of the seven CI
# jobs, none of which have any Python in them.
require_python() {
    [ -n "${PYTHON_CHECKED:-}" ] && return 0
    PYTHON_VERSION="$(tr -d '[:space:]' < "$(dirname "$0")/../.python-version")"
    PYTHON_SERIES="$(printf '%s' "$PYTHON_VERSION" | cut -d. -f1,2)"
    # However many components the pin names is how many are compared, so
    # `3.14` is a series contract and `3.14.6` would be an exact one, without
    # this needing to know which was chosen.
    PYTHON_PARTS="$(printf '%s' "$PYTHON_VERSION" | awk -F. '{print NF}')"
    if [ -n "${PYTHON:-}" ]; then
        :
    elif command -v "python${PYTHON_SERIES}" >/dev/null 2>&1; then
        PYTHON="python${PYTHON_SERIES}"
    else
        PYTHON="python3"
    fi
    FOUND="$("$PYTHON" -c "import sys; print('.'.join(str(p) for p in sys.version_info[:$PYTHON_PARTS]))" 2>/dev/null)" || FOUND=""
    if [ "$FOUND" != "$PYTHON_VERSION" ]; then
        echo "Error: this repo needs Python $PYTHON_VERSION (.python-version)." >&2
        echo "  '$PYTHON' is ${FOUND:-not runnable}." >&2
        echo "  Install it and put python${PYTHON_SERIES} on PATH, or set PYTHON=/path/to/python." >&2
        echo "  With a version manager: 'uv python install $PYTHON_VERSION' or 'pyenv install $PYTHON_VERSION'." >&2
        exit 1
    fi
    PYTHON_CHECKED=1
}

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

        # The shipped untethered build: `serial-log` off, so every bench_log!
        # takes its disabled expansion. That path is invisible to both runs
        # above, and a telemetry line that only compiles with the chatter on
        # is a build break nobody sees until release day. Both devices,
        # because the two panel drivers carry their own bench_log! sites.
        echo "Running firmware clippy with serial-log disabled for X4..."
        tools/cargo.sh clippy -p fw --no-default-features -- -D warnings

        echo "Running firmware clippy with serial-log disabled for X3..."
        tools/cargo.sh clippy -p fw --no-default-features --features device-x3 -- -D warnings
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
    stack-frames)
        # Builds and checks one device at a time: both share a target dir, so
        # the second build overwrites the first binary. See tools/stack_frames.py
        # for what this catches and why the source diff never shows it.
        require_python
        echo "Running stack frame analyzer unit tests..."
        "$PYTHON" -m unittest tools/stack_frames.py

        echo "Checking firmware stack frames for X4..."
        tools/cargo.sh build -p fw --release
        "$PYTHON" tools/stack_frames.py target/riscv32imc-unknown-none-elf/release/fw
        
        echo "Checking firmware stack frames for X3..."
        tools/cargo.sh build -p fw --release --features device-x3
        "$PYTHON" tools/stack_frames.py target/riscv32imc-unknown-none-elf/release/fw
        ;;
    test-bench)
        # The bench harness produces every device performance number this
        # project has. It runs on the one interpreter resolved above, which is
        # also the one an operator captures with -- a single version, so a
        # budget cannot be enforced in CI and silently skipped on the bench.
        require_python
        echo "Running bench harness tests..."
        "$PYTHON" -m unittest discover -s tools/bench -p 'test_*.py'
        ;;
    ruff)
        # Python lint and formatting, configured in ruff.toml at the repo
        # root. Refuses to run rather than skipping when ruff is missing, for
        # the same reason --strict refuses without a TOML parser: a check that
        # quietly does nothing is worse than none, because the green tick
        # still appears.
        require_python
        echo "Running Python lint and format checks..."
        if command -v ruff >/dev/null 2>&1; then
            RUFF=(ruff)
        elif "$PYTHON" -c "import ruff" >/dev/null 2>&1; then
            RUFF=("$PYTHON" -m ruff)
        else
            echo "Error: ruff not found. Install the pinned version:" >&2
            echo "  uv tool install ruff==0.16.1" >&2
            echo "  pipx install ruff==0.16.1" >&2
            echo "  pip install ruff==0.16.1" >&2
            echo "(ruff.toml pins the version; brew installs whichever" >&2
            echo " release is current, which may not match.)" >&2
            exit 1
        fi
        "${RUFF[@]}" check .
        # `--check` only reports; `ruff format .` is what fixes it.
        "${RUFF[@]}" format --check .
        ;;
    fast)
        "$0" fmt
        "$0" clippy-host
        "$0" test-host
        "$0" test-host-x3
        "$0" test-bench
        "$0" ruff
        ;;
    emulator)
        "$0" golden-frames
        "$0" test-emulator
        ;;
    firmware)
        "$0" clippy-firmware
        "$0" stack-frames
        ;;
    all)
        "$0" fast
        "$0" emulator
        "$0" firmware
        ;;
    *)
        echo "Usage: $0 {fmt|clippy-host|clippy-firmware|test-host|test-host-x3|test-bench|ruff|golden-frames|test-emulator|build-firmware|stack-frames|fast|emulator|firmware|all}"
        echo "  'all' runs all required root/firmware verification."
        exit 1
        ;;
esac
