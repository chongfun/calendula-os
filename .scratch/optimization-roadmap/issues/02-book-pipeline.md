# WS-B: Book pipeline — cold builds, custom fonts, catalog scan, progressive open

Status (2026-07-30): B2+B3, B6, B4, and the reader-cache extraction are on
`main`. **B7 is done** on `opt/b7-per-config-section-caches`, not yet merged.
What is left is one non-B item that outranks everything else here, one gap B4
left behind, and a `proto` stack item.

Owns: `reader-cache/`, `fw/src/book_build.rs`, `fw/src/custom_font.rs`,
`fw/src/library_sd.rs`, `ui/src/reading.rs`, `proto/`.
Do not touch: `fw/src/sd_session.rs` — SD chunk/clock/multi-block changes
belong to WS-D, and this workstream benefits automatically.

## Open

### 1. Single repaint per page turn — not a B item, and worth more than any of them

Every page turn renders **twice**: once optimistically when the page changes,
then again when storage answers with `LibraryEvent::Loaded`, which sets
`dirty = Rect::FULL` unconditionally (`app-core/src/lib.rs`) whether or not
anything about the book's shape changed. At ~405 ms of panel time each, that
is ~405 ms off every page turn in every book, cached or not.

Found while measuring B4, but it is not a B4 behaviour — it is the ordinary
reading path, present whenever a page turn issues an extend, and merely more
visible while a build runs (~6.5 s of a 48 s background window).

`bench: storage_open` already reports `ram_hit=true` for the case where the
extend loaded nothing, so the signal exists. **Care:** the second render is
genuinely necessary when the extend *did* load new section content, so the
suppression must key off "nothing was loaded and no field changed", not off
the event's fields alone.

Implemented on `opt/single-repaint-per-page-turn` (one commit over `main`);
needs review and device numbers.

### 2. First open on a deep resume

B4 gives nothing when the resume position is near the end of the book.
`FirstOpen::suspend_here` needs `total_pages > requested_page`, which for
page 561 of 562 is only true once the whole spine is built — and by then
there is nothing left to suspend on. Measured on device: a full 24.7 s cold
build, no `storage_first_page`, no progressive benefit at all. "Near the end
of a book you are reading" is not an edge case.

Nothing in the progressive mechanism can fix this; the work genuinely has to
happen. The answer is a **progress indicator** — the
`LibraryEvent::Loaded` unfinished-frontier field declined during B4's review.

### 3. `proto` inflate rework — 21 KB of stack that need not exist

`ZipInflateScratch::new()` costs a 20,960 B frame (`ensure_epub_scratch`,
release, X3) against `tools/check.sh stack-frames`' 24 KB gate, because
miniz_oxide 0.9.1's stream layer keeps a private 32 KB window and can only be
built by value. One layer down in the same crate, `inflate::core::decompress`
takes the caller's buffer as the window and `DecompressorOxide` (10,500 B)
has an in-place `init()`, so nothing on the inflate path would be constructed
by value at all. `.bss` comes out neutral (−43,280 + 10,500 + 32,768).

Needs `inflate_chunks_to_sink` / `inflate_chunks_prefix` reworked around
ring-buffer output semantics. **Its own change, in `proto`, not folded into
anything.**

### B5 (S, bundle only): word-loop micro-costs

Per-word `String<768>` copy to satisfy borrows, full line re-measure on wrap,
and `sanitize_preview_block`'s ~15 scans plus two LowerAscii copies — all in
`fw/src/book_build.rs` (`push_styled_preview_fragment` ~:2896,
`sanitize_preview_block` ~:3298).

Bundle-only by design, and it currently has no host. It was kept out of B4
(the risk there was all in the suspend/resume seam) and out of B7 (which
never touched these functions). Fold it into the next change that works
`book_build.rs`'s sink; do not open a PR for it alone.

### B1's remaining levers — only if custom-font cold builds still measure slow

#37 covered this item's goal: a 16-slot ring of decoded non-ASCII metrics
keyed by (identity, face, codepoint), `for_each_metric` feeding whole styled
runs through one open pack handle, and one seek+read per glyph bitmap. If
custom-font builds still measure slow, what is left is (a) General-Punctuation
slot runs and (b) holding the pack handle across the whole spine walk rather
than per run. Measure before building either.

## Done

- **B2+B3** (#10) — catalog scan O(C×N) → O(C+N), title persisted in the
  catalog record; incremental pagination cursor (~100–300 ms per build).
- **B6 — CONT.BIN settings-independent content cache** (#23). Captures the
  build's `push_block` stream so a settings change replays it instead of
  re-reading and re-parsing the EPUB. **Measured X3: replay 24.7 s (736 pp) /
  27.1 s (1240 pp) against a 64.0 s full build — 2.4–2.6× faster, ~37 s saved
  per settings change.** Proven genuine by read volume: 2.8–3.8 MB per rebuild
  against the 11.7 MB source zip a fallback would stream. The remaining 24–27 s
  is downstream of the capture point (wrap + section writes), which no
  zip/inflate/XML skip can touch — that is what promoted B7.
- **B4 — progressive first open** (#53). The walk stops at the first spine
  boundary past the requested page, publishes a partial BOOK.BIN stamped with
  `resume_spine`, and finishes in slices from a fifth branch of the display
  task's select. **Time-to-prologue 45.3 → 32.6 s after two scheduling fixes
  (a settle wait between steps, and size-aware step admission that refuses an
  item which would overrun the slice); page-turn median during a build
  1270 → 231 ms; the reader now finishes 15.9 s ahead of the builder instead
  of 0.4 s behind.** Total build time is unchanged and slightly worse in wall
  clock (26 steps rather than 16); that buys 12.7 s off time-to-content.
- **Reader-cache crate extraction** (#55). The store, layout, file layer and
  publish tail now live in `reader-cache`, generic over
  `embedded_sdmmc::BlockDevice` and building for the host, with
  fault-injection tests against an in-memory FAT16 card that can fail the Nth
  read or write. B4 took six review rounds and every escaped defect was the
  same shape — a write fails *after* the store has been updated and the
  cleanup gets it wrong — in code that lived in a `#![no_main]` binary no test
  could reach.
- **B7 — per-config section caches** (`opt/b7-per-config-section-caches`,
  committed, not merged). A book keeps a paginated copy per layout config
  instead of overwriting the one it has, so flipping back to a type size or
  orientation already read is a cache hit rather than the 24–27 s replay.
  Sections are `SECTIONS/S<cfg><nnn>.BIN`, the index is `BK<cfg>.BIN`, and
  `CFG.BIN` lists the resident configs most-recently-used first — driving both
  eviction (2 slots, LRU) and the readers that need a book's
  config-*independent* facts (source identity, title, TOC) to know which index
  exists. Cost: +6,882 B flash, no static RAM, one extra paginated copy on the
  card.

  Two deliberate deviations from the spec below, both worth knowing:
  - The spec said to **add** a page-box bit to the config key.
    `READER_LAYOUT_VERSION` v18 already put one in bit 7, so the key only had
    to include it. The wrap-relevant key is bits 2–7 (size, weight, family,
    portrait) — six bits, two hex digits.
  - The spec said to bump `CACHE_V2_VERSION` because the index name changed.
    **Not done, deliberately:** nothing reads the old names, so a bump would
    only invalidate CONT.BIN as well and turn the one-time upgrade from a
    replay into a full EPUB re-parse. The first open that finds an unkeyed
    `BOOK.BIN` deletes it and the unkeyed section files instead.

  **Known follow-up:** `publish.rs`'s provisional-publish failure path still
  calls `empty_cache_dir`, so a torn index write for one config now takes the
  other config's good cache with it. Scoping it to the failing config is the
  right fix; it changes a fault path with tests pinning it, and the current
  behaviour degrades to a rebuild rather than to anything incorrect.

## Constraints that bind anything here

- **The EPUB-open chain must stay inside the ~42 KB X3 stack region**, against
  a 27 KB link ASSERT floor, and no single frame may exceed
  `tools/check.sh stack-frames`' 24 KB gate. B4's 28-byte `resume` field once
  pushed `ReaderCacheScratch` past the point where LLVM would elide an `sret`
  copy, and a 43 KB `ZipInflateScratch` landed on the stack — a 53,744 B frame
  that ran 11,608 bytes past the bottom of the stack and through `.bss`, with
  every host gate passing. That gate exists because of it.
- **A partial index must say on disk that a walk is coming back for the rest.**
  The reducer clamps the reader to the advertised page count
  (`app-core/src/lib.rs`), so the reader can never *ask* for the first missing
  page and nothing else provokes a rebuild. `partial` cannot carry this — it
  means "pages are missing", not "someone is fetching them". That is what
  `BookV2Header::resume_spine` is for.
- **A page turn across a section boundary arrives as an `ExtendSection`** and
  reaches `build_or_load_book_cache`. The fast path is the only route that
  does not touch the scratch's section records, so it is the only one that can
  leave a running background walk standing. Without that,
  reading normally would kill the build at the first section crossing.

## Do not re-propose

- Already done in the July build-path work: write staging, held-open SECTIONS
  dir, dirty-gated rebuilds, 8 KB `read_at` clamp, warm SD session reuse,
  style-marker dedup, OPF span strings, streamed whole-spine XHTML, ZIP
  central-directory hash index.
- V1 cache migration is disabled by design.
- Page-turn latency and the reopen path are not software targets — except for
  the double-repaint item above, which is a redundant repaint rather than slow
  code.
- **Benchmarking progressive open on `storage_first_page`.** It hits 455 ms
  and describes nothing a reader experiences: front matter is the book's own
  spine and cannot be skipped, so the number is time-to-first-*content*,
  compared against a plain build rather than against zero.
- **The stranded `origin/opt/b4-progressive-open` branch.** Superseded; do not
  rebase it.
