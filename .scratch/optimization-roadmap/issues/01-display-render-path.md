# WS-A: Display render path — shave the ~50 ms of software around the 421 ms panel BUSY

Status: A1 DONE (#12). A2 DONE for landscape frames (#24). A2-P DONE (hoisted map, bit-shift, and division out of portrait inner loops; portrait reading layout dropped to 28–34 ms). A3 DONE (panel-native framebuffer byte order landed; fill_transformed_band_impl and 8 KB TX_BAND static removed; zero-copy fb.band() SPI streaming). A6 DONE (#47; O(1) ASCII direct indexing for glyph advance lookups). **A10 DISPROVEN as specified — shipped as a portrait glyph-blit transpose instead (#50): portrait reading layout 33 → 13 ms median on X3.** **A7 merged (#48) but measured INERT — the skip never fires; rework is queue item 1.** A8 dropped as superseded (#49 closed). A11 is the new open rasterizer item. A4/A5 hardware experiments, unscheduled.

**Baseline measured on main `e9163b3`, X3, 2026-07-27** (deliberate cadence, 50 turns, spread 445–449 ms): an isolated portrait turn is **13 ms layout + 405 ms flush (379 ms of it panel BUSY) + 24 ms prestage = 448 ms** press-to-settled. Layout is finished at 2.9% of the turn. Prestage is the one live item. Everything else outside the BUSY floor is single-digit milliseconds except A4/A5.

**Bench protocol note, learned the hard way:** `page-turn` is operator-driven and its `page turn` statistic is input→next-render, so it is *not* cadence-robust — a press landing mid-render is credited only with the remainder, and burst captures show durations as low as 2 ms. Quote `page turn` only from deliberate cadence (one press per settled page). `layout_ms`, `flush_ms`, and `busy_ms` are per-render and safe to read from either.

Owns: `display/` crate, `fw/src/display_flush/`, flush/prestage region of `fw/src/tasks/display.rs`, `hal-ext/src/spi_dma.rs`.
Do not touch: `fw/src/sd_session.rs` (WS-D), boot-init region of display task (WS-C item 2 owns the double-`init_panel` fix).

Baseline: press-to-settled 470–473 ms; 421 ms is fast-waveform BUSY (89%). Non-panel budget: layout 20–36 ms + BW stream 22–24 ms + ~5 ms overhead. RED prestage (~23 ms) additionally gates the next turn's admission. Stacked target for items A1+A2+A3: ~450 ms press-to-settled with better held-button cadence.

## A1 (Tier 1, S): Send `DisplayEvent::Settled` before prestage and chapter tracking — DONE (#12)

## A2 (Tier 2, M): Byte-run rasterizer fast paths — DONE for landscape (#24)

## A2-P (Tier 2, M): Portrait byte-run/strided fast paths — DONE

Landed 2026-07-26: `fill_span` and `blit_row` in `FbFrame::Portrait` hoist `map()` coordinate transform, bounds checks, `byte_x()`, bit-shifts, and division out of the inner loop, stepping down native row indices via pointer adds (`index += stride`). Tested against `set_pixel()` per-pixel reference across all frames. Measured on X3 hardware: portrait reading layout dropped to **28–34 ms** (averaging ~31 ms per turn).

## A3 (Tier 2, M–L): Panel-native framebuffer byte order — DONE

Landed 2026-07-26: `Framebuffer::data` internal storage is stored in native byte order. Bit-reversal and X/Y mirroring are handled inside coordinate indexing math. `fill_transformed_band_impl` and `REVERSE_BITS_LUT` removed from `epd/mod.rs`. Deleted the 8 KB `TX_BAND` static buffer in `fw/src/tasks/display.rs`. Firmware streams `fb.band()` zero-copy directly over SPI. P1 (`MIRROR_X`) and P2 (`FbFrame::Native` identity) invariants verified; goldens re-blessed.

## A6 (Tier 2, S): O(1) ASCII Direct Indexing for Glyph Advance Lookups — DONE (#47)

Landed 2026-07-26: `BitmapFont::glyph` checks for printable ASCII codepoints (`32..=126`) using direct array offset indexing (`codepoint - 32`), bypassing binary search. Also bypasses kerning table search when the font contains no kerning entries.

## Follow-on Display & Portrait Path Optimizations

### A10: Pre-computed Line-Wrap Caching — DISPROVEN; shipped as a glyph transpose instead (#50)

**A10's premise was false and the item is dead. Do not re-propose it.** It claimed "~16 ms (Landscape) to ~31 ms (Portrait) of every turn is spent in `ui/src/reading.rs` re-measuring glyph widths and re-computing word wrapping for every paragraph on the page." No word wrapping happens at render time at all:

- The firmware EPUB sink wraps at **cache-build** time. `push_line_block` (`fw/src/reader_store.rs`) stores every finished *physical line* as its own `BlockRecord` and hardcodes `line_count: 1`, so `ui::reading::block_height`'s one-line arm already short-circuits every block a device loads.
- The page record is already an $O(1)$ lookup: `reader_page_at` (`fw/src/reader_layout.rs`) indexes `ReaderStore::pages` directly.
- Host measurement, one full portrait page in the production block shape: **2.8 µs** for a complete `paginate_block_pages` walk and **0.4 µs** for `page_record_at`, against **194 µs** of `draw_reading_page_body`. Layout is ~1.5% of the render; the other 98.5% is glyph rasterization.

A first attempt to implement A10 as written (since reverted) only flipped `block_height` to trust `BlockRecord::line_count` for counts above one. Zero device effect, and it made an unvalidated cache byte authoritative geometry — a damaged byte could give one short block up to `255 * advance` of height. If paragraph-sized blocks are ever stored, that count needs validating against the block text on load, or a versioned multiline block format.

**What shipped under the A10 branch instead (#50), and what actually moved the number:** `Framebuffer::blit_bitmap` in `display/src/fb.rs`. A frame row runs down a native *column* in portrait, so `blit_row` could only place one pixel per read-modify-write and re-paid the column setup for each of a glyph's rows. Eight consecutive frame rows land in eight bits of the **same** native byte, so transposing an 8×8 block of the glyph (`transpose8`, Hacker's Delight 7-3) collapses those 64 masked writes into 8. `draw_glyph` and the SD custom-font path in `fw/src/custom_font.rs` — which was doing a full coordinate transform per lit pixel via `set_pixel` — both route through it. Landscape still blits row by row through `blit_row`, untouched. Empty 8×8 blocks skip both the transpose and the writes.

Measured on X3: **portrait reading layout 33 ms median / 35 ms p95 → 13 ms median / 14 ms p95** (2.5x, −20 ms/turn). Host predicted 2.1x; the device gained more, as expected — the removed work is dependent per-pixel loads, stores, and unpredictable branches that an out-of-order host core hides and the C3 pays in full. Cost +852 B `.text`, +16 B `.rodata`, no `.bss` or stack change. `blit_bitmap` is pinned by test against the row-at-a-time `blit_row` loop across all four frames, both boards, and every clipping edge; all X4 and X3 goldens pass unblessed.

**Method note for the next agent:** the A10 premise survived three roadmap revisions because nobody measured the split between layout and rasterization before ranking it first. Any future "cache the layout" reading-perf proposal needs that measurement first.

### A7: Asynchronous / Pipelined Prestaging — MERGED (#48) BUT INERT; REWORK IS QUEUE ITEM 1

#48 landed a **conditional skip, not an overlap**: after a flush settles the display task calls `embassy_futures::yield_now()` once, checks `crate::DISPLAY_COMMANDS.is_empty()`, and skips `prestage_previous` when something is queued (`fw/src/tasks/display.rs`, 8 lines).

**Measured on main 2026-07-27: the skip never fires.** Across two X3 `page-turn --turns 50` captures at opposite cadences, prestage ran on **100 of 100 renders** (values only {24} and {24, 25}), and the branch's own `display: pending command queued, yielding prestage` line printed **zero** times. This was not for lack of queued work — the burst capture contains a render with no preceding input, so commands genuinely were queuing.

**Cause.** The next render cannot exist until the app has answered. The display task sends `DisplayEvent::Settled`, and only then does `fw/src/tasks/app.rs:286` receive it, clear `rendering`, and push the next `RenderRequest` into `DISPLAY_COMMANDS`. One `yield_now()` is a single scheduling pass — nowhere near enough for that round trip, so the queue is reliably empty at the check.

**Do not fix this by adding more yields.** That trades a guaranteed 24 ms for a race, and a lost race silently costs a `prev_prestaged = false` and the next turn's fast path. Two directions worth costing out:

- Wait for the app's next command with a bounded timeout (`select` on the channel vs. a deadline well under 24 ms), so the decision is made on the real signal rather than on scheduler luck.
- Move the decision to where the command actually arrives: prestage lazily at the top of the next loop iteration, when the queue state is known for certain, instead of eagerly at the end of the previous one.

Either way this is the largest remaining non-BUSY term on the page turn (24 of 448 ms) and the only display item left with double digits behind it. True overlap with panel BUSY or SPI DMA — the original A7 design — remains unimplemented and is what A9 would build on.

### A8: 32-Bit Word-Wide Strided Iteration for Portrait Blits — DROPPED, superseded by #50 (PR #49 closed)

A8 proposed unrolling the portrait `blit_row` column loop 4 native rows at a time for ~20–30%. #50 removed that loop from the glyph path entirely and took 2.5x instead, so the unrolling has nothing left to unroll except `fill_span` — menu furniture, already at 3–8 ms. Not worth reopening.

### A11: Batch Landscape Glyph Rows the Same Way (new, 2026-07-27)

Fallout from #50: **landscape is now the slower frame per page.** Host, one full reading page: landscape 146 µs vs portrait 93 µs. Landscape still calls `blit_row` once per glyph row, re-paying per-row native-y computation, frame match, and `reverse_bits` + `blit_native_bits` per source byte — roughly 700 ops per 16-row glyph against portrait's ~270. A whole-glyph landscape path inside `blit_bitmap` (batch the row setup; the byte writes are already 8 pixels wide, so the win is setup amortization, not fewer stores) should close most of that gap.

Lower priority than the portrait work was: portrait is the default orientation (#5), and landscape reading layout is already 16–18 ms. Size the win before building it — expect single-digit milliseconds, against a 448 ms turn.

### A9: Overlapped SPI DMA Band Transmits
Double-buffered SPI DMA streaming so band $N+1$ is prepared while band $N$ transmits over SPI wire, reducing display flush transmission time to raw hardware clock limits.

## A4 (Tier 3, S code / medium hw risk): Skip RED-plane write when CTRL1 bypasses it — verify first

Non-Fast flushes write the same fb to BW and RED (`fw/src/display_flush/ssd1677.rs:55-62`, ~23 ms each), but `update_control_1` for Full/FastClean is `[0x40, 0x00]` (`display/src/epd/ssd1677.rs:148-153`) — the RED-bypass bit — and prestage overwrites RED right after anyway. Needs hardware A/B (cold-boot ghost clearing is the sensitive case) before shipping. Emulator panel model validates RED writes, so its op plan changes too (`tools/emulator/src/panel.rs`).

## A5 (Tier 3, M, high hw risk, opt-in only): Temperature-override "hot" LUT for Fast refresh

The 421 ms BUSY is the sensed-temperature OTP fast waveform — the only lever below the floor. `FastClean` already proves the mechanism (`FAST_CLEAN_TEMPERATURE` = 90 °C, skip load-temp bit, restore after — `display/src/epd/ssd1677.rs:25-35`, `fw/src/display_flush/ssd1677.rs:87-95`). Apply a moderate override (35–50 °C) to `Fast` as an **opt-in RefreshPolicy tier**, never default. `RefreshMode` is shared with emulators and never forked per panel (`display/src/epd/mod.rs:3-5`).

- Impact: potentially 60–120 ms/turn. Risks: ghosting/contrast, unit/temperature variance, more frequent FastClean eating the win; emulator can't model waveform physics — pure hardware validation.
- Verify: `page-turn` fast-BUSY distribution, `thermal-run` cold/warm, long `reader-soak` for ghost accumulation, visual check per ghosting guidance.

## Micro (fold into adjacent work)

`wait_ready`'s fixed 1 ms pre-delay (`hal-ext/src/spi_dma.rs:79-88`) → bounded wait-for-high-then-low; ~1–2 ms per refresh.

## Do not re-propose

Partial-window refresh (deliberately shelved ×2), SPI >40 MHz (rated ceiling), MIRROR_Y=true (tested, wrong), software work on the Full waveform ("noise" per IMPLEMENTATION_PLAN). RED prestaging already exists — A1/A3 build on it. **Line-wrap / layout caching for the reading view (A10 as originally specified) — measured false, see A10 above.** **A8's portrait `blit_row` unrolling — superseded by #50.**

Suggested order: A1 ✓ → A2 ✓ → A2-P ✓ → A3 ✓ → A6 ✓ → A10-as-shipped ✓ (#50) → re-baseline ✓ (2026-07-27) → **A7 rework (the skip never fires — 24 of 448 ms)** → A11 (landscape batching, size it first) → A4 (verify-first) → A5 (experiment).

