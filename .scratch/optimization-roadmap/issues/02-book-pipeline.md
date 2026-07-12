# WS-B: Book pipeline — cold builds, custom fonts, catalog scan, progressive open

Status: B2+B3 DONE (#10). Next: B6 (CONTENT.BIN — brought in 2026-07-12 from the retired docs/OPTIMIZATION_PLAN.md brainstorm; full design inlined below), then B4 (its prerequisites B3, D1 landed; sequence after B6 — both restructure the open flow). B1 is DEMOTED, not dropped: the owner reads with built-in fonts only, so custom-font build speed has near-zero personal ROI; the spec below stays current (unblocked by E2 — store encoded 12-B metric records, as the in-flash FONT_METRICS cache now does) if custom fonts come into use. Note the upload write path now belongs to the upload-store crate (#18).

Owns: `fw/src/reader_cache*.rs`, `fw/src/custom_font.rs`, `fw/src/library_sd.rs`, `fw/src/reader_layout.rs`, `ui/src/reading.rs`, `proto/`.
Do not touch: `fw/src/sd_session.rs` — SD chunk/clock/multi-block changes belong to WS-D (this workstream benefits automatically).

Baseline: cold V2 cache build 3.9 s for a 117-page EPUB (~70% CPU inflate+parse+measure, ~30% SD I/O; 537 wr + 723 rd blocks); HPMOR-scale extrapolates to ~2.5 min. Warm reopen 50–85 ms (fine). EPUB open chain must stay inside the 30–43 KB stack region; link-time ASSERT fails under 27 KB.

## B1 (Tier 1, S–M): Custom-font metric cache for non-ASCII glyphs

`MetricCache` (`fw/src/custom_font.rs:24-125`, 4 slots) covers only ASCII 0x20–0x7E. Every non-ASCII char — curly quotes U+2018/2019/201C/201D, em-dash U+2014, thousands of occurrences per novel — misses into `measure_char` (`custom_font.rs:197-229`): open `/XTEINK` dir → `FONTS` dir → pack file → seek → 12-B read, **per occurrence** (several FAT block reads per apostrophe). Fix, in combination: (a) extend slots with the General-Punctuation run the pack already indexes (~120–240 B/slot); (b) small direct-mapped LRU (16–32 entries, ~400 B) for arbitrary non-ASCII; (c) hold the pack file open across the spine walk (sink already holds `root`).

- Impact: custom-font cold builds recover tens of seconds to minutes, back to near built-in speed. RAM: ~0.5–1.5 KB static, count against stack headroom.
- Constraint: measurements must stay bit-identical (`READER_LAYOUT_VERSION` untouched) — a cache trivially preserves this.
- Verify: `bench.py storage-cache --cold` with a custom pack vs built-in (`rd_calls`/`elapsed_ms`); host tests for cache correctness.
- Coordination: WS-E item 13 shrinks the 12-B metric struct this cache stores — land 13 first or coordinate on `custom_font.rs`.
- Prior art: `docs/plans/2026-07-08-custom-fonts-investigation.md:281` anticipates "a small metric cache".

## B2 (Tier 2, M): Catalog scan — kill the O(walks×N) re-walks and O(C×N) orphan sweep

`scan_books` (`fw/src/library_sd.rs:42`, runs on every boot/wake/refresh): (1) `write_catalog_streaming` (`:148`) walks the full FAT tree once to count plus once per 48-record batch; (2) `sweep_orphan_caches` (`:544`) streams the **entire catalog per cache dir** via `find_in_catalog` (`:321`) — O(C×N); (3) `read_catalog_window` (`:246`) does a dir-walk + BOOK.BIN title read per row on every Library window crossing (`cached_title_label`, `:424`). Fixes: (a) load catalog identities (8 B hash+size pairs) once into an idle scratch region (16 KB `ReaderStore.text` arena or 24 KB xhtml scratch — confirm not live during scans) → O(C+N); (b) stage records in that scratch instead of the 4.4 KB stack batch → ~2 walks total and a smaller stack frame; (c) persist the title into the catalog record (bump `CATALOG_VERSION` 4→5; version byte already forces clean rescan on mismatch, `:236`; refresh title on book open).

- Impact: on a 100-book card with 50 caches, eliminates ~5,000 record reads + ~2 FAT walks per scan; visibly snappier Library scrolling. Measure first via the existing `bench: storage_catalog action=scan elapsed_ms=` line (`library_sd.rs:79`).
- Verify: storage-cache suites, `storage_catalog` line with a many-book fixture card, emulator library-scroll goldens, `reader-soak` with Library visits.

## B3 (Tier 2, S–M): Incremental pagination cursor during builds

`flush_if_full` → `rebuild_pages_if_dirty` → full `rebuild_page_index` (`fw/src/reader_layout.rs:54`) once per flushed line → O(blocks²/2) per section (~74 k `block_height` calls at 384 blocks). Maintain the cursor (`y`, `first_block`, `page_count`) incrementally in `LibraryBlockSink` — O(1)/line — falling back to full rebuild on the carry path (`flush_section` `carry_last_page`, `reader_cache.rs:1683-1694`) and handling `mark_last_block_paragraph_end`'s retro gap change (`:1950-1954`) as a bounded one-block fix-up.

- Impact: ~100–300 ms per build (3–8%), linear with book length.
- **Invariant: exact agreement with `rebuild_page_index`** — page records persist into section files; divergence is silent cache corruption. Keep full rebuild as a debug assertion in host tests (copy the naive-reference harness pattern from `ui/src/reading.rs:1022-1200`).

## B4 (Tier 3, L): Progressive first open — publish the target section early

`build_or_load_epub_cache_from_zip` (`fw/src/reader_cache.rs:876`) walks the entire spine before `BookLoadStatus::Ready`. Publish Ready once the section containing the requested page is flushed (`flush_section`, `:1620`, already writes `S%03d.BIN` incrementally), record a provisional/partial `BOOK.BIN`, and continue the walk as re-enqueued storage-command steps on the display task so renders interleave. `partial` flag, `start_page` bookkeeping, and `load_v2_section_by_global_page` already model "not fully indexed"; new parts are resumable build state (spine cursor + running page total) and a final index rewrite.

- Impact: time-to-first-page 3.9 s → ~0.5–1 s; minutes → ~1–2 s on huge books. Total build time unchanged.
- Risks: growing page-total denominator mid-read (footer/progress must tolerate); interrupted-build resume policy; storage/render interleave must keep the single-SPI-owner invariant (chunk granularity matters — held sessions block renders). Stack-neutral. No format migration if the final BOOK.BIN write is unchanged.
- Verify: `storage-cache --cold --warm --strict` (+ first_section_ms field), `page-turn` during background build (<550 ms budget), `channel-stress --host`, emulator scenario opening mid-book.
- Sequencing: land after WS-D's SD tiers (shorter background phase) and after B3 (bundles naturally). Touches display-task storage dispatch — coordinate with WS-A/WS-C on `fw/src/tasks/display.rs`.
- Prior art: named in IMPLEMENTATION_PLAN as a candidate next win. Considered, never planned.

## B5 (bundle only, S): word-loop micro-costs

Per-word `String<768>` copy to satisfy borrows (`reader_cache.rs:1860-1862`), full line re-measure on wrap (`:1883-1885`), `sanitize_preview_block`'s ~15 scans + two LowerAscii copies (`:2133-2212`). Only while already in this file for B3/B4.

## B6 (Tier 2, M): Settings-independent content cache — CONTENT.BIN

A Type Size/Weight/Family change flips wrap-relevant bits in `reader_layout_config`, so `load_v2_section_cache` rejects every section (`font_config & !0b11` check, `fw/src/reader_cache_files.rs:914`) and the open re-does the entire EPUB pass — zip read + inflate + XML parse + wrap + section rewrite, a full cold build (14.1 s on the measured 11.7 MB EPUB). Everything upstream of wrapping is settings-independent. Persist the `push_block` argument stream (text, role, style, align, paragraph_end, plus spine-boundary markers) to `XTEINK/CACHE2/<key>/CONTENT.BIN` during the full build; on a layout-config miss, replay it through the same `LibraryBlockSink` instead of re-parsing the EPUB.

Design (folded in from the retired docs/OPTIMIZATION_PLAN.md item 4, audited against main 2026-07-12):

- **Capture point:** the `XhtmlBlockSink::push_block` boundary — the exact argument stream `(text, role, style, align, paragraph_end)` per fragment, plus a spine-boundary marker between spine items (record `spine_index` and the `finish_spine` events). Capturing here (raw, pre-normalization) guarantees a replay produces byte-identical sections, because the replay literally calls the same `push_block`. `start_spine_index` and navigation-spine skipping (`spine_item_is_navigation`) run before `push_block`, so the captured stream already excludes them — replay must not re-filter.
- **File and format:** `XTEINK/CACHE2/<key>/CONTENT.BIN`. Header: own magic + `CONTENT_VERSION` + `source_hash` + `source_size` + `complete` flag. Records: `spine_index: u16, role: u8, style: u8, align: u8, flags: u8 (paragraph_end, spine_end), text_len: u16, text bytes`. Constants and encode/decode helpers go in `proto/src/cache.rs` next to the existing section records, with unit tests there (host-buildable).
- **Write during the full build:** wrap the sink so each `push_block` also appends one record, sequential append only, through a staging buffer loaned from `ReaderCacheScratch` — never a stack buffer (`scratch.xhtml` is busy during spine streaming; `scratch.opf`/`scratch.container` are idle after OPF parse — verify before reusing, and document the reuse where the field is declared, as `load_epub_toc` does for `xhtml`). If any write fails, delete the file and continue the build — CONTENT.BIN is purely an accelerator. Partial/stopped builds (`book_partial`, spine truncation) must record `complete = false`.
- **Replay path:** in `build_or_load_book_cache_from_root`, when the section load misses on layout config (today that surfaces as `CacheLoadResult::Invalid` from the `font_config` check — plumb the distinction out, or simply try CONTENT.BIN before the EPUB whenever the index/section load misses): verify identity + version + `complete`, then stream the records into a `LibraryBlockSink` configured with no zip work — same `flush_if_full`/`flush_section`/`write_v2_book_index` flow, `generate_toc_from_headings = false`. Fall back to the full EPUB path on any read/decode error, after deleting the bad file. Replay only from a `complete` capture.
- **TOC.BIN and COVER.BIN survive settings rebuilds:** their contents don't depend on type settings — only the page map does, and that is already recomputed per config (`refresh_chapter_tracking`, `chapter_page_token`). On replay, don't re-stream or rewrite TOC.BIN; populate the resident `library.toc*` fields from the old BOOK.BIN (load it before invalidating) or from TOC.BIN, so the rewritten BOOK.BIN keeps its TOC block.
- **Invalidation:** keyed by source identity (hash + size) and `CONTENT_VERSION` only — never by layout config. Bump `CONTENT_VERSION` whenever XHTML-parser / entity-decode / sink-normalization semantics change (same bump-log discipline as `READER_LAYOUT_VERSION` in `ui/src/reading.rs`). Cache-dir eviction (`fw/src/reader_cache_files.rs:642` area) must learn to delete CONTENT.BIN too.
- **Cost:** roughly the book's raw text on SD (typically well under a few MB) plus one extra sequential write on the first open — accepted; it buys every later settings change.
- One simplification vs the original design: the forward-only build path (`ZipLocalStream`) has no firmware call site anymore — uploads store files (#18) and builds happen on first open — so capture only needs the `ZipStream` path; keep the sink generic but don't spend effort on forward-only capture.

- Impact: settings-change reopen drops from a full cold build to sequential read + wrap + section writes — skips zip/inflate/XML, the bulk of the CPU-dominated build. First open pays one extra sequential write (~raw text size).
- Verify: encode/decode round-trip tests in `proto` (host); on device: (1) a cold open writes CONTENT.BIN, (2) A/B a Type Size change on the 11.7 MB baseline book, (3) identical `total=` page counts from replay vs from-EPUB rebuild at the same settings, (4) hand-corrupt/truncate CONTENT.BIN → fallback still opens.
- Coordination: same files as B4 (`reader_cache*.rs`, `reader_cache_files.rs`) — land B6 before starting B4.

## B7 (conditional, S–M): Per-config section caches

Keep caches per layout config instead of overwriting, so flipping back to a previously-built setting is an instant cache hit (folded in from the retired docs/OPTIMIZATION_PLAN.md item 5). The layout config is a small packed integer (`reader_layout_config`: `version<<6 | family<<5 | weight<<4 | size<<2 | spacing`); key the wrap-relevant nibble (family/weight/size, 4 bits) into the section file names (e.g. `S<cfg>_<n>` — mind the 8.3 name budget, `CACHE_SECTION_FILE_BYTES = 8`) and into per-config BOOK.BIN names. Costs: SD space multiplies per config used; eviction needs a policy (e.g. keep at most 2 configs per book, delete least recently used); `CACHE_V2_VERSION` bump if BOOK.BIN naming changes. Only worth it if B6's measured replay is still noticeably slow on device — decide from B6's verify numbers, don't schedule independently.

## Do not re-propose

Already done in the July build-path work: write staging, held-open SECTIONS dir, dirty-gated rebuilds, 8 KB read_at clamp, warm SD session reuse, style-marker dedup, OPF span strings, streamed whole-spine XHTML, ZIP central-directory hash index. V1 cache migration is disabled by design. Page-turn latency and reopen path are not software targets.

Suggested order: B6 → B4 (with B5 folded in). B7 only if B6's replay measures slow; B1 only if custom fonts come into use.
