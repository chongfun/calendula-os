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

### 1. Single repaint per page turn — DONE (#56), and it was never a B item

Filed here because it was found while measuring B4. It is an app-state item;
`issues/07-app-render-invalidation.md` G2 owns it and records what shipped.

Two corrections to how this file valued it, kept because both are the kind of
error the method rules exist to catch. It fired on **every** page turn, not
only section crossings — `storage_command_for_transition` issues an
`ExtendSection` on every page change. And it did **not** take 405 ms off
press-to-settled; the reader sees the page after the first render. What it
removed is ~405 ms of panel time *after* the page is readable: queueing delay
when reading faster than ~0.9 s/page, half the panel energy per turn, and half
the refresh count.

**It was also corrupting the bench harness**, which nobody suspected while it
was open. Page-turn pairing was render-driven, so the second repaint of turn N
consumed press N+1 and credited it with a near-zero duration — the source of
the reference capture's 2 ms samples. Fixing the harness (Tier 0) and fixing
the firmware were the same bug seen from two ends.

### 4. Write alignment — the pipeline pays ~1.75× the SD write transactions it needs

**New 2026-07-30.** Nothing in the cache writer arranges for block-aligned
writes, and the FAT layer's only cheap write is a whole block starting on a
block boundary (`embedded-sdmmc` takes the no-read `blank_mut` branch only
when `block_offset == 0 && to_copy == block_avail`, and `write_back()` fires
once per 512-byte block regardless).

- **CONT.BIN** begins with a 24-byte header, so every 512-byte stage flush
  straddles two blocks: 488 B into block *N*, then 24 B at offset 0 of *N+1*
  with `to_copy != 512` → a real **read-modify-write**. Steady state is
  **2 writes + 1 read per 512 bytes written** against an ideal of 1 write.
  Raising `CONTENT_STAGE_BYTES` 256 → 512 already helped, but it treated
  *size* as the lever when *alignment* is.
- **Section files**: 56 B header written directly, ~4 KB of records through a
  256 B stage, then 16 KB of text written directly. Modelled at ~58 writes +
  9 reads per section where 40 writes + 0 reads would do. The 16 KB text write
  is already near-optimal; the waste is the record region.

| | today (modelled) | aligned | waste |
|---|---|---|---|
| CONT.BIN, ~2.5 MB | 9,766 wr + 4,883 rd | 4,883 wr | 4,883 wr + 4,883 rd |
| 100 section files | 5,800 wr + 900 rd | 4,000 wr | 1,800 wr + 900 rd |

**Model cross-check:** the 2026-07-09 measurement recorded **537 write blocks**
for a 117-page book; this model predicts ~580 for the same shape. That is a
real validation of the transaction count. *State the counts, not seconds* —
the only time calibration available (0.86 s / 537 = 1.6 ms per write) would
make the whole build write-bound, contradicting the same note's "~70% CPU", so
per-write is probably 0.5–0.8 ms now and the saving is **~1.1–5.4 s** off a
build or replay. Counts are solid; seconds are not.

- Fix: make `staged_write`/`WriteStage` **offset-aware** — `File::offset()` is
  free — flushing `(512 − offset % 512)` bytes first and then whole blocks, and
  route the 16 KB text through the same stage instead of bypassing it.
  **No cache-format change, no version bump**; the read side is sequential and
  unaffected. Costs +256 B of stack in `write_section_records`, inside the deep
  EPUB frame — state it in the PR.
- **The one measurement:** diff `wr_blocks` from `bench: storage_build`
  between a cold build and a replay of the same book at identical settings;
  the delta is CONT.BIN's write traffic. Add `content_bytes=` to the
  `epub: content capture kept=` line (one argument) to turn it into the
  amplification ratio directly. **Ratio ≈ 2.0 confirms; ≈ 1.0 kills it.**
- Note `write_micros` wraps *only* `write_v2_section_cache_in`, so **CONT.BIN's
  write cost has never appeared in `write_ms`** and never will until this is
  fixed.

### 5. Prune orphaned section files — DONE (`opt/prune-orphan-sections`)

Implemented 2026-07-30, one commit over `main`, not merged. **Land it before
B7**, which multiplies the leak by keeping a copy per config.

The publish tail prunes after `write_v2_book_index` succeeds, deleting section
files whose ordinal is past the new count. Two things make it safe, and both
are gates rather than comments: **`resume_spine` must be zero** — non-zero
means a suspended walk is coming back and `sections_slice` is its provisional
frontier, so pruning against it would delete sections that walk is about to
need — and it runs **after** the index lands, so everything it deletes is
already unreachable from the index on the card. Best effort: a refused delete
leaves the rest and never turns a successful publish into a failed one.

Deletion is by *parsed* ordinal, not by directory contents: `SECTIONS/` is on
removable media, so a name this code did not write is left alone, including
near-misses like a two-digit `S12.BIN`.

Three tests on the FAT16 fault-injection harness, each verified by mutation
(dropping the gate, weakening `>=` to `>`, and ignoring the name parse each
fail the test that covers them). Gates: fmt, host clippy, `check.sh fast`,
`clippy -p fw` on both boards.

One thing worth carrying: the naming key had to be checked before the bound
was correct. `BookV2SectionRecord` carries `section` *and* `spine`, and the
writer's parameter is called `spine` in one helper and `section` in another —
the file is keyed by the dense ordinal, so "delete past *n*" works. Had it
been keyed by spine index, which is sparse because navigation items are
skipped, that bound would have deleted live sections.

### 5b. Original write-up, kept for the reasoning

**New 2026-07-30.** A settings change that *shrinks* the section count orphans
the tail forever. A build at Large produces 100 sections; dropping to Small
produces 82; `S082..S099` are never named again, because
`load_v2_section_by_global_page` indexes off the BOOK.BIN table. They survive
until the whole cache dir is emptied. On the measured book that is **~360 KB
of dead space per shrink, per book**, accumulating across every type change
and orientation flip.

**B7 multiplies this** — it creates more section files under more names, two
configs at a time — so whatever prunes them should exist first. Fix: after a
successful publish, delete `S<n>..` up to the previous header's
`section_count`, or enumerate `SECTIONS/` and drop anything past `n`. It must
run **after** the new index lands, never before; deleting a section the reader
is holding is precisely the class of defect the publish tail spent six rounds
on, and `reader-cache/tests/publish_faults.rs` is the right home for the test.

### 6. Two counters that would settle where the replay time actually goes

**New 2026-07-30, and this is the cheapest item in the workstream.** The split
this file has been *guessing* at for three rounds is already being printed on
every build and every replay:

```
bench: storage_build elapsed_ms= spine_ms= write_ms= sections= pages=
       rd_calls= rd_blocks= wr_calls= wr_blocks= key=
```

and `bench.py` contains **zero** references to it (see WS-F F9). Read
`write_ms` and `wr_blocks` before writing any wrap-loop code.

Two one-line additions complete the picture: a `measure_micros` accumulator
around `push_line_ink_str` (same pattern as the existing `write_micros`),
printed beside `write_ms`; and `content_bytes=` per item 4 above. Together
they split wrap from I/O and size items 4, 7 and B5 in a single capture.

**A model of where the 271–301 ms per section goes, for calibration only —
not evidence:** section-file writes ~60–95 ms (25–35%), CONT.BIN reads plus
RMW reads ~20–30 ms (8–11%), ink measurement ~33–47 ms (12–17%), normalize +
sanitize ~7 ms (2.6%), framing <2 ms, **unattributed ~90–150 ms (35–50%)**.
The file's standing claim that "nearly all remaining cost is wrap + section
writes" is directionally right but wrong on balance: the wrap is only ~15–20%
and the write path is the larger identified block. That large unattributed
remainder is exactly why the counters go first.

### 7. Per-character measure cost — **do not write code before item 6 lands**

`InkCursor::push_char` calls `font.glyph(codepoint)` **twice** for the common
hit (the measure path never uses the returned bitmap slice), and
`kerning_adjust_fp` runs a `binary_search_by` over **852 entries** — ~10
iterations of flash loads — per character, for a value that is zero for the
overwhelming majority of pairs. On the custom-font path, `face_for(style)`
linear-scans up to 12 face records **once per character** though it is
invariant for a whole styled run.

Estimated ~12–17% of a section (instruction counting, *not* measured), of
which kerning is ~60–70%. Cheapest fixes first: (a) single `glyph()` call,
reuse the result — pure refactor; (b) hoist `face_for` out of the per-char
loop in `for_each_metric`; (c) a 256-bit "this codepoint appears as a left
kerning operand" bitmap emitted by the font generators, one bit test to skip
the search — that one touches `display/` and `tools/generate_*.py`, so it
belongs to WS-E's owner, not here.

**This is the exact shape of claim that killed A10.** Item 6's `measure_micros`
counter settles it: **if measure is under 5% of `elapsed_ms`, drop this
finding entirely.** Any change here must also be bit-identical — a host test
asserting `text_ink_width` is unchanged over a corpus is mandatory, since drift
silently repaginates every cached book.

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
`book_build.rs`'s sink; do not open a PR for it alone — **item 4 is that
change.**

**Sized 2026-07-30, and the framing was wrong: the cost is stack, not time.**
`heapless::String::new()` is `MaybeUninit`, so there is no memset, and the
temporaries copy only the appended word (~8 B) or the just-restarted line. The
"full line re-measure on wrap" is not a full line — by the time it runs,
`flush_styled_preview_line` has already emptied and re-seeded the line, so it
re-measures **one word**. Total time cost: **well under 1% of a section.** Do
not spend on it for speed.

What does matter is up to **four 768-byte slots** in
`push_styled_preview_fragment` plus one in `flush_styled_preview_line` — if
LLVM does not coalesce them, ~3 KB of frame in the deepest EPUB chain, in a
budget where a 28-byte struct field once caused a stack overflow. One site
B5 never listed is free to remove: `let line = sink.line.clone();` (~`:3084`)
is a 768-byte-capacity clone per *flushed line*; `core::mem::take(&mut
sink.line)` gives the same owned value, makes the later `clear()` redundant,
and cannot leave two live copies.

**The one measurement:** read `push_styled_preview_fragment`'s peak `sp` off
`tools/check.sh stack-frames`, which already computes it. **If the frame is
under ~1 KB, LLVM coalesced them and there is nothing here.**

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

  **Rebase note, 2026-08-13 — this branch no longer merges cleanly.** #75
  moved the driver pin to our fork and rebased it onto upstream v0.10, and the
  conflict is in `reader-cache/src/files.rs`. It is an API migration, not a
  textual one: `delete_file_in_dir` → `delete_entry_in_dir`, `CardType` moved
  into `embedded-sdmmc-types` (`SDHC` → `SdhcSdxc`), and `iterate_dir`
  callbacks now return `ControlFlow` instead of `()`. The last one is not
  mechanical — `main`'s version of this file now uses `ControlFlow::Break` to
  stop scans early, and a migration that returns `Continue` everywhere will
  compile while quietly reverting those. Do the migration against `main`'s
  intent, not against the compiler's complaints.

  **Branch audit, 2026-07-30 — three defects to fix before it merges.** The
  branch is the best-structured large change in the queue (1,473 of 1,485
  added lines are in host-testable crates, 11 real tests on the FAT
  fault-injection harness, and a `read_cache_header` that fails closed with
  real care). It should not be demoted. It does need another round:

  1. **`&str` byte-index slicing aborts on SD-derived filenames.** Three name
     recognizers in `proto/src/cache.rs` (~`:692`, `:707`, `:725`) check *one*
     char boundary and then slice at others. `name.len()` is a **byte** length,
     and embedded-sdmmc's `ShortFileName: Display` emits `byte as char`, so any
     FAT entry byte ≥ 0x80 becomes two UTF-8 bytes and shifts the boundaries.
     Reproduced end-to-end on the branch's own fake-card harness:
     `read_cache_header` aborts with *"start byte index 4 is not a char
     boundary; it is inside 'é'"*. Release is `panic = "abort"`, and the entry
     point includes the orphan sweep that runs on **every catalog write**.
     Fix is mechanical — compare over `name.as_bytes()`; nothing here needs
     `str` semantics. `main` has no equivalent exposure; this is new surface.
     The near-miss test table is all ASCII, which is what let it through.
     **Still live after #75** — re-checked in the fork, where `ShortFileName:
     Display` still writes `c as char`
     (`embedded-sdmmc/src/filesystem/filename.rs:238`). Long-name support does
     not help here: `DirEntry::name` is still a `ShortFileName`, so every scan
     in `reader-cache` still sees the 8.3 alias. If anything the exposure is
     now easier to hit, since aliases are derived by the driver from
     user-supplied filenames rather than from a hash.
  2. **The cross-config cache wipe is at three sites, not the one admitted.**
     Besides `publish.rs`, `fw/src/book_build.rs` calls `empty_cache_dir` on
     the cold-build `IndexWriteFailed` arm (~`:1962`) and on the **replay**
     failure path (~`:2163`) — the very flow B7 exists to accelerate, and it
     takes `CONT.BIN` with it, downgrading the fallback from a replay to a full
     EPUB re-parse. The justifying comment ("safe because the book never became
     readable") was true with one cache per book and is now false: the other
     config's complete, valid pagination is collateral. Every existing fault
     test uses a single config, which is why none of them sees this.
  3. **`BookBuildResume` is not keyed by the layout config.** `belongs_to`
     compares only catalog row + source identity. Before B7 a fast hit implied
     the suspended walk's config; now a fast hit under config B can carry a
     live walk that was building A, and the resumed step's writers derive
     config B. Memory-safe and self-correcting (the per-file `font_config`
     check rebuilds), but it silently destroys the second cached copy the
     feature exists to preserve. Fold the layout key into `belongs_to`.

  Also unaccounted in the cost section: `adopt_layout_config` runs on every
  `build_or_load_book_cache` — including section-crossing page turns — and
  unconditionally does a dir walk, a failed `BOOK.BIN` open and a `CFG.BIN`
  **write**, even when the registry is unchanged. `LayoutConfigRegistry`
  already derives `PartialEq`; an equality guard is one line.

  **Rollback is a rebuild, not corruption** — acceptable — but messier than
  the commit implies: old firmware sees `Absent`, sweeps the directory, and its
  name-based `empty_cache_dir` has never heard of `CFG.BIN`/`BK*.BIN`, so the
  verify fails, `SECTIONS/` is never removed, and the sweep re-fails on every
  catalog write. Worth documenting as a one-way door.

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
