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
# is preferred and checked against the pin before anything uses it.
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

        # The durability campaign's firmware. Never shipped, so nothing above
        # compiles it -- and a bench operator finding it broken is finding it
        # at the worst possible moment, with the device already open and the
        # card already staged. X3 only: it is the board the campaign runs on.
        echo "Running firmware clippy for the powercut campaign for X3..."
        tools/cargo.sh clippy -p fw --features device-x3,powercut-selftest -- -D warnings
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
    test-tools)
        # Operator tooling whose *analysis* decides a pass or a fail, and so
        # cannot be left to be exercised only on a bench. The powercut
        # campaign's landing logic is the whole of what turns an installer
        # durability defect into a reported failure; a bug there reports
        # green on a broken card.
        require_python
        echo "Running device tool tests..."
        "$PYTHON" -m unittest tools/powercut_campaign.py
        ;;
    ruff)
        # Python lint and formatting, configured in ruff.toml at the repo
        # root. Refuses to run rather than skipping when ruff is missing, for
        # the same reason --strict refuses when it cannot load its budgets: a
        # check that quietly does nothing is worse than none, because the
        # green tick still appears.
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
        # The six stages are independent commands, so they start together and
        # are waited on as a group. The three cargo stages do contend: they
        # share one build directory, and cargo prints "Blocking waiting for
        # file lock" while it serialises their compiles. The win is that a
        # stage's *test execution* happens outside that lock, so it overlaps
        # the next stage's compile. Measured on a ten-core host over six
        # interleaved trials, this target went from ~22.5s serial to ~16s
        # after a one-file edit, and from ~5s to ~2s with nothing to rebuild.
        # The absolute numbers move with machine load, so they are only worth
        # comparing against a serial run taken alongside; the ratio held near
        # 0.7 across loads from 1 to 8.
        #
        # They share the one target dir on purpose. Giving each stage its own
        # would drop the lock waits entirely, but target/ already runs to tens
        # of gigabytes and a copy per stage costs far more disk than the
        # remaining contention costs time.
        #
        # Output is captured per stage and replayed below in a fixed order
        # rather than interleaved live: six concurrent cargo jobs sharing one
        # terminal are unreadable, and -- the part that matters -- when one
        # fails you cannot tell which. The cheap stages are listed first so
        # that something appears while the cargo ones are still running.
        #
        # Only pre-push and `all` reach this. CI invokes the individual
        # targets on separate runners and never goes through here.
        FAST_STAGES=(fmt ruff test-bench test-tools clippy-host test-host test-host-x3)

        # Job control, for cancellation. Two things follow from `set -m` that
        # this arm depends on. Without it, POSIX has the shell set SIGINT to
        # ignore in every command started with `&`, and that disposition
        # survives exec, so Ctrl-C would leave the stages *and* their cargo
        # descendants running while the script itself died -- orphaned builds
        # holding the target-directory lock that the next run then blocks on.
        # With it, each stage instead leads its own process group, so one
        # `kill` on the negated pid reaches the whole tree rather than just
        # the wrapper. Verified by sending SIGINT to the group: six survivors
        # before, zero after.
        set -m

        FAST_TMP="$(mktemp -d)"
        FAST_PIDS=()
        # Stages sit in their own process groups now, which also means the
        # terminal's Ctrl-C no longer reaches them on its own -- only the
        # foreground group gets it. Tearing them down here is therefore
        # required, not belt-and-braces. INT/TERM/HUP exit into the EXIT trap
        # so that one teardown path serves every exit.
        fast_cleanup() {
            local p
            for p in "${FAST_PIDS[@]}"; do
                if [ -n "$p" ]; then
                    kill -TERM "-$p" 2>/dev/null || true
                fi
            done
            wait 2>/dev/null || true
            rm -rf "$FAST_TMP"
        }
        trap fast_cleanup EXIT
        trap 'exit 130' INT
        trap 'exit 143' TERM
        trap 'exit 129' HUP

        for stage in "${FAST_STAGES[@]}"; do
            "$0" "$stage" > "$FAST_TMP/$stage.log" 2>&1 &
            FAST_PIDS+=($!)
        done
        echo "Running ${#FAST_STAGES[@]} checks in parallel..."
        echo

        # Waiting in declaration order rather than completion order keeps the
        # transcript identical from run to run, which matters when comparing a
        # failure against a previous one.
        FAST_FAILED=()
        for ((i = 0; i < ${#FAST_STAGES[@]}; i++)); do
            stage="${FAST_STAGES[$i]}"
            if wait "${FAST_PIDS[$i]}"; then
                echo "--- $stage: ok ---"
            else
                echo "--- $stage: FAILED ---"
                FAST_FAILED+=("$stage")
            fi
            # Reaped, so drop it: the pid is free for reuse from here, and
            # cleanup must not signal a group that some unrelated process has
            # since been given. Stages still running keep their entry.
            FAST_PIDS[$i]=""
            cat "$FAST_TMP/$stage.log"
            echo
        done

        if [ ${#FAST_FAILED[@]} -ne 0 ]; then
            echo "check.sh fast: ${#FAST_FAILED[@]} of ${#FAST_STAGES[@]} failed: ${FAST_FAILED[*]}" >&2
            exit 1
        fi
        echo "check.sh fast: all ${#FAST_STAGES[@]} checks passed."
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
        echo "Usage: $0 {fmt|clippy-host|clippy-firmware|test-host|test-host-x3|test-bench|test-tools|ruff|golden-frames|test-emulator|build-firmware|stack-frames|fast|emulator|firmware|all}"
        echo "  'all' runs all required root/firmware verification."
        exit 1
        ;;
esac
