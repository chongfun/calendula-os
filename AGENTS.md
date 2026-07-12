# Working contract

Before changing code:

- Read `docs/ARCHITECTURE.md`, `docs/IMPLEMENTATION_PLAN.md`, and any relevant ADRs or files under `docs/agents/`.
- Inspect the existing implementation and nearby tests before designing a replacement.
- Keep changes scoped to the requested work. Do not rewrite unrelated code or regenerate golden files unless the change intentionally affects them.
- The repository defaults Cargo to the firmware target. Determine the local host with `rustc -vV` and pass an explicit `--target` to every host-side Cargo command.

## Definition of done

Do not declare a task complete until you have:

1. Run the checks applicable to the changed code.
2. Fixed failures introduced by your change.
3. Reviewed the final diff for accidental files, formatting churn, generated artifacts, and unrelated edits.
4. Reported every verification command you ran and whether it passed.
5. Explicitly reported any applicable check you could not run and the reason.

Never say that tests, lint, builds, or visual checks pass unless you actually ran them.

Use the repository verification entry points:

- `tools/check.sh fmt` for formatting.
- `tools/check.sh fast` for normal Rust changes.
- `tools/check.sh emulator` for UI, layout, rendering, typography, reader-state, or golden-frame changes.
- `tools/check.sh firmware` for firmware, HAL, board-specific, feature-gated, or release-sensitive changes.
- `tools/check.sh all` before a pull request is considered ready.

Changes affecting boards, rendering, or firmware must be checked for both X4 and X3.

Do not push, tag, publish a release, rewrite history, or discard user changes without explicit permission.

## Rust and embedded constraints

The firmware is `#![no_std]` on an ESP32-C3: 400 KB SRAM, no PSRAM, ~43 KB of
usable stack, one 48 KB framebuffer, `panic = "abort"` in release.
`docs/ARCHITECTURE.md` (Rules, Data-oriented design) is the authority on the
invariants; a change that compiles, passes tests, and still violates one of
these is wrong:

- No heap outside the wireless session. The only allocator is the Wi-Fi
  session's one-way memory loan (`fw::sync_mem`), which ends in a software
  reset. Do not introduce `alloc`, `Vec`, `String`, or `Box` anywhere else in
  firmware; the reading path stays allocation-free.
- Memory is budgeted, not grown. Use bounded collections (`heapless`, fixed
  arrays) and caller-owned buffers. Raising a capacity constant, widening a
  struct used in caches, messages, or parser state, adding a large stack
  local, or adding recursion is a RAM/stack decision: state the size delta
  and why it fits. The EPUB-open chain must stay inside the stack region
  noted in the root `Cargo.toml` profile comments.
- Respect the single-writer owners. Only the board I/O task touches the EPD
  bus, SD chip select, `ReaderStore`, and framebuffer; only `app_task`
  mutates `ReaderState`; the power task requests sleep through the display
  task and never touches SPI. Tasks exchange small `Copy` messages, and bulk
  bytes move through caller-owned or loaned buffers. Do not bypass an owner
  or add a second bus-touching task to shorten an implementation.
- Embassy waits are cooperative. Do not busy-spin, block for long stretches,
  or hold a lock, critical section, or shared-resource guard across an
  `.await` unless nearby code already documents that pattern.
- Release builds abort on panic. Malformed EPUBs, SD/FAT failures, network
  input, and hardware flakiness are recoverable: handle them through
  `Result`/`Option` and the existing retry/fallback paths, never through
  `unwrap`, `expect`, or indexing that can panic on externally influenced
  data. Panicking is acceptable only for boot bring-up (peripheral, DMA, and
  task-spawn setup) and locally proven invariants, with an `expect` message
  stating the invariant (existing style: `"record slice is exactly one
  record"`).
- `unsafe` is fenced by the compiler: library crates `forbid(unsafe_code)`,
  and `fw` denies it with narrow per-item `#[allow(unsafe_code)]` opt-ins.
  Keep unsafe blocks minimal, and give each one a `// SAFETY:` comment
  stating the soundness argument (workspace-lint enforced).
- Fix lint findings; don't silence them. When suppression is genuinely
  right, use the narrowest scope and put the reason next to it (existing
  style: `#[allow(clippy::manual_div_ceil)] // False positive inside
  esp_hal::dma_buffers!.`). Use `#[expect]` only where the lint fires in
  every built configuration: X4 and X3 both compile with `-D warnings`, and
  an expectation unfulfilled in one build fails it. Never add crate- or
  workspace-wide allows to get CI green.
- Features form a fixed matrix, not a lattice. Default is the X4;
  `device-x3` deliberately flips the whole workspace to X3 geometry;
  `builtin-custom-font` compiles in a generated typeface; `ota-selftest` is
  an on-device flash exerciser that must never reach a release.
  `--all-features` builds a nonsense hybrid and verifies nothing — use the
  `tools/check.sh` entry points, which already cover both devices. Keep new
  features additive and document their interactions in Cargo.toml comments
  as the existing ones do.
- Prefer crates already in the tree. A new dependency must support the
  firmware target as `no_std`, with default features trimmed, and be weighed
  for flash and RAM cost. Pin Git dependencies to a commit and record why
  that commit (see the `embedded-sdmmc` pin in `fw/Cargo.toml`). Do not
  change `Cargo.lock`, `rust-toolchain.toml`, dependency versions, features,
  or `[profile.*]` as a side effect of unrelated work; profile changes
  require re-measuring stack use per the root `Cargo.toml` notes.
- Keep pure logic host-testable. State reduction, parsing, layout, encoding,
  and protocol code lives in `app-core`, `proto`, `ui`, `display`, and
  `upload-store` behind sans-IO seams; `fw` and `hal-ext` have no host
  tests. A bug fix gets a host regression test when the behavior is
  reachable there.

## Agent skills

### Issue tracker

Issues and PRDs are tracked as local markdown under `.scratch/`. See `docs/agents/issue-tracker.md`.

### Triage labels

This repo uses the default mattpocock/skills triage vocabulary. See `docs/agents/triage-labels.md`.

### Domain docs

This is a single-context repo: read the domain and architecture docs in `docs/`, plus `docs/adr/` if present. See `docs/agents/domain.md`.

### Cutting a release

Releases are tag-triggered and CI-built; `tools/prepare-release.sh <version>`
first syncs the crate version and the site's version/size labels so the
descriptor stamp and page don't lie. Never pre-create the GitHub release — the
workflow creates it, and Pages can't deploy without a populated release. See
`docs/agents/release.md`.

### Bench workflow

Development bench runs use `tools/bench/bench.py` and structured `bench:` serial
telemetry. See `docs/agents/bench.md`.

### Visual & Layout Changes Verification

Visual, layout, rendering, and typography changes are verified locally against the
emulator's golden frames, on both the X4 and the X3. Host-side cargo commands need
an explicit `--target`. See `docs/agents/visual-verification.md`.
