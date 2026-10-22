# Working contract

Before changing code:

- Read `docs/ARCHITECTURE.md`, `docs/IMPLEMENTATION_PLAN.md`, and any relevant ADRs or files under `docs/agents/`.
- Inspect the existing implementation and nearby tests before designing a replacement.
- Keep changes scoped to the requested work. Do not rewrite unrelated code or regenerate golden files unless the change intentionally affects them.
- The repository defaults Cargo to the firmware target. Determine the local host with `rustc -vV` and pass an explicit `--target` to every host-side Cargo command.
- `rust-toolchain.toml` pins one compiler release and CI installs from it, so a local run of `tools/check.sh` reproduces CI. A failure that only CI shows means the pin is not what is running locally: check `rustup show active-toolchain` in the checkout before looking anywhere else. Bump the pin in its own PR.

## Definition of done

Do not declare a task complete until you have:

1. Run the checks applicable to the changed code.
2. Fixed failures introduced by your change.
3. Updated documentation to match your code. If your work changes the architecture, display model, storage assumptions, or performance characteristics, update `docs/ARCHITECTURE.md` and `README.md` accordingly.
4. Reviewed the final diff for accidental files, formatting churn, generated artifacts, and unrelated edits.
5. Reported every verification command you ran and whether it passed.
6. Explicitly reported any applicable check you could not run and the reason.

Never say that tests, lint, builds, or visual checks pass unless you actually ran them.

Use the repository verification entry points:

- `tools/check.sh fmt` for formatting.
- Python runs on the series in `.python-version` (3.14), so any 3.14.x will
  do and a patch release does not block anyone. `python3` is not assumed to be
  it (macOS ships 3.9 under that name), so `tools/check.sh` prefers
  `python3.14` and checks what it found; `PYTHON=/path/to/python` overrides.
  Install with `uv python install 3.14` or `pyenv install 3.14`. Only the
  targets that run Python require it -- `ruff`, `test-bench`, `stack-frames`
  -- so the Rust-only ones need no interpreter at all. The check compares as
  many components as the file names, so pinning a release later is a one-line
  edit.
- `tools/check.sh ruff` for Python changes: lint plus a formatting check.
  `ruff.toml` pins the version (`required-version`), because the formatter's
  style can move between releases; install that exact one, e.g.
  `uv tool install ruff==0.16.1` or `pipx install ruff==0.16.1`. Run
  `ruff format .` to fix formatting. It is part of `fast`, and refuses to run
  rather than skipping when ruff is absent.
- `tools/check.sh fast` for normal Rust changes.
- `tools/check.sh emulator` for UI, layout, rendering, typography, reader-state, or golden-frame changes.
  It includes `tools/check.sh web-emulator`, which checks the browser build on both boards.
- `tools/check.sh firmware` for firmware, HAL, board-specific, or release-sensitive changes.
- `tools/check.sh all` before a pull request is considered ready.

Changes affecting boards, rendering, or firmware must be checked for both X4 and X3.

`tools/check.sh` covers the X4 and X3 device configurations, but not every
optional feature. When changing feature-gated code, additionally compile or
lint every affected valid feature combination. For example:

    tools/cargo.sh clippy -p fw --features ota-selftest -- -D warnings

Do not use `--all-features` as a substitute for the repository's defined
matrix. The `builtin-custom-font` case needs special handling: the checked-in
`display/src/custom_generated.rs` emits `compile_error!` until a generated
module replaces it, so verifying changes under that feature requires running
`tools/font_pack_to_rust.py` first and compiling the affected X4/X3
combination.

Do not push, tag, publish a release, rewrite history, or discard user changes without explicit permission.

## Rust and embedded constraints

The firmware is `#![no_std]` on an ESP32-C3: 400 KB SRAM, no PSRAM, ~43 KB of
usable stack, two framebuffer allocations (active drawing buffer in main DRAM,
previous-frame buffer in DRAM2; each is FB_BYTES + 1 bytes: 48,001 bytes on X4,
52,273 on X3, where the extra byte is due to the Framebuffer layout's FbFrame
field defined in display/src/fb.rs, and fw/build.rs mirrors
size_of::<Framebuffer>() when sizing the previous-frame linker slot),
`panic = "abort"` in release.
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
  `--all-features` builds a nonsense hybrid and verifies nothing.
  `tools/check.sh` covers both devices but does not enable
  `builtin-custom-font` or `ota-selftest`; see the verification note above
  for how to check those paths. Keep new features additive and document
  their interactions in Cargo.toml comments as the existing ones do.
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

## Code Review Rules

### Crash consistency and file identity

- For changes to SD/FAT mutations, journal replay, cache publication, or persisted reader state, check whether interruption at any write, rename, or commit boundary can destroy previously committed data, publish incomplete data as valid, or leave recovery unable to converge. Verify that an I/O read failure or an unreadable side of an alternating `A`/`B` generation pair (`STATEA`/`STATEB`, `POSA`/`POSB`, `LEDGERA`/`LEDGERB`) is not treated as confirmed absence or allowed to overwrite a newer generation with a stale one.
- Recovery, retries, and move carries must not overwrite, adopt, or reclaim a file merely because its name or first cluster matches a previous operation: FAT deletes free both the 8.3 alias and the cluster chain for reuse by later files. Distinguish the three identity levels and require the evidence appropriate to the operation rather than treating them as interchangeable:
  - **Filesystem-transaction identity** (`(8.3 alias, first cluster, byte size)` in `INSTALL.JNL`) for interrupted FAT moves and rollbacks.
  - **Logical-copy identity** (`BookId` in the library ledger), which tracks a shelf copy across moves and managed replacements (`REPLACE.JNL` preserves `BookId` while swapping the file's bytes, and two byte-identical copies have distinct `BookId`s) — `BookId` never proves which bytes are on disk.
  - **Content identity** (SHA-256 digest) to prove byte identity across moves or interrupted replacements, versus `(locator/source_hash, byte size)` where avoiding a full-file hash is part of the documented contract (such as warm opens and unchanged-place catalog joins).
  Also check that normal file and directory removals reclaim cluster chains (`remove_file_reclaiming_clusters`, `remove_dir_reclaiming_clusters`) rather than only unlinking directory entries.
- Safe path: stage incomplete work outside the visible namespace, preserve the previous committed generation until its replacement is recoverable, make replay idempotent, and preserve documented cases where cheaper `(locator, byte size)` matching is intentional. Abandoned scratch data may be discarded if it was never committed; leaking clusters across an interrupted delete or reclaim is the documented safe direction, and explicitly documented, bounded resource leaks are not automatically correctness failures.

### Persistent-format compatibility

- When changing the interpretation or encoding of persisted records, cached layouts, reading anchors, or communication protocols, check every writer and reader of the affected format (including lightweight metadata/title probes, which must validate header counts against `MAX_*` bounds with checked arithmetic before seeking or slicing). In particular, look for older bytes being accepted under newer semantics, newer bytes being accepted by an incompatible reader, or an upgraded `A`/`B` record starting at generation 1 losing to a legacy record with a higher generation number.
- Check coupled formats and derived data across the three principal semantic version axes, as well as file- and record-specific format versions:
  - `CACHE_V2_VERSION` (`proto::cache`) covers `BOOK.BIN`, section files, `TOC.BIN` (reloaded on warm opens without re-parsing nav/NCX), and `CONT.BIN` (replayed across settings changes without re-parsing the EPUB).
  - `CONTENT_STREAM_VERSION` (`proto::anchor`) covers persisted `ContentAnchor` byte offsets in `POSA.BIN`/`POSB.BIN` (`text.len() + 1` per emitted block); it changes when XHTML block emission, order, or normalized text length changes.
  - `READER_LAYOUT_VERSION` (`ui::reading`) covers line wrapping, glyph boxes/advances, style-marker measurement, and `PageIndexCursor` page-break rules. Because `layout_key` omits line spacing, cross-spacing section reuse is valid only when the section covers the exact same text span as the index record (ends its spine item).
  - Independent file and record format versions (such as `CONTENT_VERSION` for `CONT.BIN`, `TOC_FILE_VERSION` for `TOC.BIN`, `COVER_VERSION`, `CLAIM_VERSION`, `CATALOG_VERSION`, `LEDGER_VERSION` / `LEDGER_JOURNAL_VERSION`, `FONT_PACK_VERSION`, `proto::nvm` record `VERSION`s, `upload-store` journal `VERSION`s, and the RTC `probe_cache` generation) must be checked and bumped whenever their binary encodings or record semantics change, even if the three semantic axes above do not.
- Safe path: bump the appropriate compatibility version(s) and reject/rebuild incompatible data, or provide an explicit migration while preserving documented legacy fallbacks where still supported. Do not demand a `CONTENT_STREAM_VERSION` bump for TOC-only, line-wrapping, or rendering changes that leave parser block offsets unchanged, and do not demand a version bump for changes proven to be wire- and behavior-compatible (such as repurposing a reserved field that every prior writer zero-filled).

### Bounded-input semantic correctness

- When a parser, extractor, paginator, or bounded collection reaches a capacity, depth, or input-size limit, check that the resulting behavior remains semantically honest. A truncated or incomplete EPUB structure (such as a cut OPF buffer or a spine/manifest over `MAX_SPINE_ITEMS` / `MAX_BOOK_SECTIONS`) must still propagate `partial` / `spine_truncated` rather than being silently treated as complete, while a fallback path that replaces an unused primary path must not inherit stale truncation flags from the discarded attempt.
- Check boundary and nesting edge cases: bounded depth counters (`u8` skip/formatting depths, fixed table/list stacks) must not saturate and then decrement on close (prematurely exiting an enclosing state), self-closing tags (`<tag/>`) must restore enclosing state, and chunk/block splits must not drop trailing carries, swallow inter-word spaces, arm paragraph-drop flags on control-only runs, or let quoted attributes, XML/CSS comments, or book control characters (`ESC` / `&#27;`) spoof markup or style markers. Compare alternate implementations of the same semantics — buffered versus streamed extraction, storage (`storage::book_build`) versus preview (`tools/preview`) and vec sinks, and measurement (`ui::reading`) versus rendering (`display::font`).
- Safe path: preserve complete valid content when possible; otherwise fail closed on depth overflow or propagate an explicit partial, unsupported, or error state appropriate to the consumer. Intentional partial-book support is acceptable when downstream consumers retain that distinction. Do not report differences that are explicitly part of the documented contract.

### Async state, progressive builds, and test fidelity

- For changes to `app-core`, `storage`, or `reader-cache`, check state invariants across multi-step and asynchronous flows: if a pre-transition flush, folder move, chapter jump, or open is refused by the card, in-memory state (browse cursor, pending place hold, coalesced save) must remain unchanged rather than half-transitioning. Per-book state in `ReaderStore` (such as `toc_total`, unbacked marks, and place holds) must reset when switching to another book, persist across same-book reloads or mid-walk `BOOK.BIN` refreshes, and never let an unopened default book or a temporary frontier landing overwrite a user's saved reading place before a restore or jump resolves and is folded by `app_task`.
- Distinguish progressive builds still in flight (`partial` with a walk coming back via `resume_spine`) from whole or permanently capped books (`partial` at the spine or section ceiling with no remaining walk), so chapter jumps and anchor resolution do not wait indefinitely on a walk that will never run.
- Safe path: gate cross-queue responses with the appropriate catalog/browse generation or hold state, and verify that regression tests would fail with the fix reverted (for example, asserting the book is readable and checking the resolved content or scan count when error and fallback paths both land on `page 0`).

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
