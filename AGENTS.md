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

Apply the rules relevant to the changed behavior. Read the affected callers,
readers/writers, and nearby tests before reporting a violation;
`docs/ARCHITECTURE.md` defines the intended contracts and their exceptions.

- Report actionable defects introduced or made reachable by the PR. Each
  finding should identify a concrete input or event sequence, the resulting
  failure, and the smallest useful range of changed lines. Trace the failure
  through the implementation; a rule naming a risk is not evidence that the
  change has that bug.
- Prioritize data loss, wrong-book state, crashes, hangs, and incorrect reader
  behavior. Deduplicate findings with the same cause. Leave formatting and
  routine lint to CI; avoid speculative hardening and unrelated refactors.
- Use tests and CI results as evidence, and distinguish checks you ran from
  those you only inspected. An unavailable tool or an unrun check is a
  verification limitation, not by itself a code defect. For regression tests,
  check that the assertion distinguishes the bug from the intended result;
  asserting `page == 0` is insufficient when both success and fallback land
  there. Check readability, resolved content, or scan count as appropriate.

### Crash consistency and file identity

- For SD/FAT mutations, journal replay, cache publication, and persisted state,
  trace interruption at write, close, rename, and commit boundaries, including
  replay interrupted a second time. A write can reach the card and still
  report failure; an error return does not prove the disk is unchanged.
  Preserve committed durable state until its replacement is recoverable.
  Derivable caches may be invalidated and rebuilt, but an incomplete cache
  must never be published as complete or used to authorize destructive cleanup.
- Distinguish confirmed absence, invalid bytes, unsupported versions, and I/O
  failure. A failed read does not prove absence or authorize overwriting a
  possibly newer `STATEA`/`STATEB` or `POSA`/`POSB` generation. For the ledger,
  `LEDGER.JNL` determines the live side: damage to that committed side must
  refuse recovery, not silently fall back to an older side and re-mint ids.
  An interrupted rewrite of the non-live side follows the journal's recovery
  rules; it is not the same as loss of committed data.
- Recovery, retries, and move carries must use the identity evidence required
  by the operation. FAT aliases and clusters can be reused after deletion:
  - Filesystem-transaction identity is the `(8.3 alias, first cluster, byte size)`
    recorded in `INSTALL.JNL` for an interrupted move or rollback. It is not a
    durable copy id outside that transaction.
  - Logical-copy identity is the ledger's `BookId`. It follows a shelf copy
    across moves and managed replacements; `REPLACE.JNL` preserves it while
    changing the bytes. Two byte-identical copies have distinct ids, and an id
    alone never proves which bytes are on disk.
  - Content identity is the SHA-256 digest. For move matching, equal bytes
    still need a unique match on both sides; unreadable candidates or twins
    beyond a scratch-table limit must not become false evidence of uniqueness.
    Warm opens and unchanged-place catalog joins deliberately use the cheaper
    `(locator/source_hash, byte size)` contract. Do not demand full-file hashes
    on those paths.
- Normal removals must reclaim cluster chains through
  `remove_file_reclaiming_clusters` / `remove_dir_reclaiming_clusters`.
  Interrupted moves can leave two names sharing one chain: recovery must
  settle the carry markers and unlink the duplicate without freeing the
  surviving file's chain. A documented leak after an interrupted reclaim is
  safer than freeing a chain whose ownership is unresolved.
- Safe path: stage work under scratch names or invalid headers, publish the
  validity marker only after its dependencies are recoverable, and make replay
  idempotent. Abandoned uncommitted scratch may be discarded. Respect documented
  cache-rebuild fallbacks and bounded leaks; do not impose durable-state
  preservation on every derived file.

### Persistent-format compatibility

- For changes to persisted records, caches, anchors, or protocols, trace every
  affected writer and reader, including lightweight metadata/title probes and
  replay paths that bypass parsing. Validate header counts against the format's
  `MAX_*` bounds and use checked arithmetic for lengths and offsets before
  seeking or slicing. Check old bytes under new semantics and new bytes under
  any older reader still allowed to accept them.
- Check each affected compatibility axis; one version bump does not necessarily
  invalidate every coupled artifact:
  - `CACHE_V2_VERSION` (`proto::cache`): book/section caches, plus `TOC.BIN`
    reloaded without parsing nav/NCX and `CONT.BIN` replayed without parsing
    the EPUB. Check the actual acceptance checks, including compatibility
    windows, so stale data cannot bypass the fix.
  - `CONTENT_STREAM_VERSION` (`proto::anchor`): persisted `ContentAnchor`
    offsets in `POSA.BIN`/`POSB.BIN`, counted as `text.len() + 1` per emitted
    block. Revisit it when block emission, order, or normalized text length
    changes. TOC-only, wrapping, or drawing changes that leave those offsets
    unchanged do not require this bump.
  - `READER_LAYOUT_VERSION` (`ui::reading`): wrapping, glyph boxes/advances,
    style-marker measurement, and `PageIndexCursor` page breaks. Because
    `layout_key` omits line spacing, cross-spacing section reuse requires a
    section ending its spine item and a re-walk matching its index record's
    page span; a mid-item section can contain different text at another spacing.
  - File/record versions: `CONTENT_VERSION`, `TOC_FILE_VERSION`, `COVER_VERSION`,
    `CLAIM_VERSION`, `CATALOG_VERSION`, `LEDGER_VERSION` /
    `LEDGER_JOURNAL_VERSION`, `FONT_PACK_VERSION`, `proto::nvm` and
    `upload-store` record/journal versions, and the RTC `probe_cache` generation.
    Incompatible encodings or meanings need an appropriate version gate even
    when the three semantic axes above do not change.
- A durable-state migration must survive interruption and preserve generation
  selection. A new-format `A`/`B` record starting at generation 1 must not lose
  to a still-accepted legacy record with a higher generation; check the next
  boot and the next write, including counter wraparound.
- Safe path: reject and rebuild derivable caches. For non-reconstructible state
  or unresolved journals, migrate explicitly or refuse in place without
  overwriting; `BookId`s cannot be re-derived from EPUBs. Preserve supported
  legacy fallbacks. A version bump is unnecessary when compatibility is
  demonstrated, such as a reserved field whose prior writers all zero-filled,
  where zero has the correct new meaning and older accepting readers remain
  safe. Zero-filling alone is not proof of semantic compatibility.

### Bounded-input semantic correctness

- For parser, extractor, paginator, and bounded-collection changes, check inputs
  at the limit and beyond it. A cut OPF buffer or omitted spine/manifest entries
  must propagate `partial` / `spine_truncated`. A fallback replacing an unused
  primary parse must not inherit that discarded attempt's truncation flags.
  A partial result is valid only where consumers support it: a truncated
  catalog cannot stand for the complete library used by orphan cleanup.
- Check nesting and chunk boundaries: bounded depth counters must not saturate
  and then decrement on close, prematurely leaving an enclosing state;
  self-closing tags must restore that state. Splits must preserve trailing
  carries and inter-word spaces, and control-only runs must not arm paragraph
  dropping. Quoted attributes, XML/CSS comments, and book control characters
  (`ESC` / `&#27;`) must not spoof markup or internal style markers.
- Compare paths that promise the same semantics: buffered/streamed extraction,
  `storage::book_build`/`tools/preview`/vec sinks, and `ui::reading` measurement
  versus `display::font` drawing. Safe path: preserve valid content, or return
  the documented partial, unsupported, or recoverable error state. Fail closed
  on depth overflow. Respect documented differences between consumers.

### Async state and progressive builds

- For changes to `app-core`, `storage`, or `reader-cache`, trace success,
  refusal, cancellation, and retry across task boundaries. A refused
  pre-transition flush must preserve the pending save and prevent the
  dependent transition. A failed folder move must restore its browse cursor.
  When disk writes may already have landed, reconcile state with the readable
  disk outcome or report it unavailable; blindly restoring old RAM can be as
  wrong as advancing it. Keep app and storage state consistent, including
  failure notifications needed to end a wait.
- Check identity across queues and yields: browse-position generation guards
  row addresses, catalog epoch guards catalog indices, and request identity
  guards work canceled or superseded while awaiting a repaint. Check both
  the response and any command derived from it. A stale result must not open
  another book or leave the current request waiting forever; preserve the
  documented unfenced boot/active-book opens.
- Per-book state in `ReaderStore` (`toc_total`, unbacked marks, place holds)
  must not leak into another book or be lost merely by reloading the same book
  or its mid-walk `BOOK.BIN`. An unopened default book or temporary frontier
  landing must not overwrite the saved place before the restore/jump resolves
  and `app_task` folds it. Clear holds only at the corresponding completion or
  documented cancellation, not merely when a response is queued.
- Safe path: distinguish a live progressive walk (`partial` with a walk coming
  back via `resume_spine`) from a whole or permanently capped book with no
  remaining walk. Chapter jumps and anchor resolution must terminate under
  those outcomes, and interrupted indexes must follow the documented rebuild
  path on reopen. `partial` alone does not prove that more pages will arrive.

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
