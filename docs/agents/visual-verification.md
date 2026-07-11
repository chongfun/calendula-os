# Visual & Layout Changes Verification

Golden frames are the fast oracle for visual, layout, rendering, and typography
changes: they run on a plain host, cover both boards, and catch regressions no
unit test will. Work through the steps below before trusting such a change.

## Host target is mandatory

`.cargo/config.toml` pins `[build] target = "riscv32imc-unknown-none-elf"`, so
every host-side `cargo` command needs an explicit `--target`. Omit it and the
emulator builds for the `no_std` firmware target and fails deep in `serde` with
thousands of "cannot find `Some` in this scope" errors.

```sh
HOST="$(rustc -vV | sed -n 's/^host: //p')"   # aarch64-apple-darwin on an Apple-silicon Mac
```

CI passes the same thing as `$HOST_TARGET` (`x86_64-unknown-linux-gnu`). Alternatively,
use the `tools/check.sh` script which automatically determines your host target.

## Format and lint

```sh
tools/check.sh fmt
tools/check.sh clippy-host
tools/check.sh clippy-firmware
```

The host clippy set is exactly CI's `--workspace --exclude hal-ext --exclude fw`.
The firmware clippy runs lint on `hal-ext` and the shared crates on the RISC-V target as a
side effect of compiling the firmware.

`cargo fmt --all` and `cargo clippy --workspace` only reach the seven root-workspace
crates (including `upload-store`). `tools/emulator`, `tools/preview`, and `tools/web-emulator` each declare
their own `[workspace]`, sit outside CI's fmt and clippy jobs, and are not
fmt-clean today. Match local style when editing them; do not run `cargo fmt`
across those trees or you will bury the real change in reformatting noise.

## Golden frames

X4 and X3 frames share `fixtures/golden/`; X3 files carry an `-x3` suffix. Check
both boards:

```sh
tools/check.sh golden-frames
```

When a frame changes on purpose, point `--dump` straight at the golden directory
for each board — it names every file from the scenario stem plus the board
suffix, and rewrites unchanged frames byte-for-byte, so `git diff` shows exactly
the frames the change touched. Review those images, then re-run `tools/check.sh golden-frames`.

```sh
cargo run --manifest-path tools/emulator/Cargo.toml --target "$HOST" \
  --no-default-features -- --scenario fixtures/scenarios --dump fixtures/golden
cargo run --manifest-path tools/emulator/Cargo.toml --target "$HOST" \
  --no-default-features --features device-x3 -- --scenario fixtures/scenarios --dump fixtures/golden
```

`--check` compares decoded pixels with strict equality, so goldens must come
from `--dump`. Never hand-edit one or round-trip it through an image editor.
`--present-dump` writes the panel-presented image instead of the framebuffer —
useful when debugging refresh and ghosting, never a golden.

Agents must not update goldens merely to make a test green; they must inspect the pixel changes and explain why they are intentional.

### Per-board target dirs when alternating boards

Both boards share `tools/emulator/target`, and the `device-x3` feature flip
recompiles `display` → `ui` → emulator (~2–6 s each way), so alternating X4/X3
checks pay that rebuild on every switch. `tools/check.sh` already gives each board
its own artifact cache with `--target-dir` so both directions stay warm.

To eyeball a single scenario, pass a `.toml` file to `--scenario` and a `.png` to
`--dump`; `--gui` needs `--features gui`.

## Reading-page goldens

Typography and reading-layout changes also need the reading-page goldens, which
`--scenario` does not cover. These checks run in the CI `golden-frames` job alongside emulator unit tests. Check them locally:

```sh
tools/check.sh test-emulator
```

Regenerate after an intentional typography change by running:

```sh
REGEN_READING_GOLDEN=1 ./tools/check.sh test-emulator
./tools/check.sh test-emulator
```

The second invocation verifies that the regenerated fixtures pass without regeneration enabled.

## Browser emulator

`ui` and `display` changes also feed the wasm emulator, and the device chrome
(bezel, rocker buttons) lives in `web/`. If the change is visible there, build
and look at both boards:

```sh
rustup target add wasm32-unknown-unknown
tools/build-web.sh _site
python3 -m http.server -d _site 8000    # then open http://localhost:8000/?board=x3
```

## Hardware

Golden frames prove the render, not the panel. After display flush, refresh-plan,
or sleep-screen changes, follow up with the short hardware benches in
`docs/agents/bench.md`.
