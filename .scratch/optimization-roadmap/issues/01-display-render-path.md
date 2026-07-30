# WS-A: Display render path

**Status (2026-07-30): finished.** A portrait turn is **13 ms layout +
~405 ms flush (379 ms of it panel BUSY) = ~424 ms press-to-settled**, with a
24 ms prestage afterwards the reader never waits on. Layout is 3% of the
turn. Nothing left here is worth more than single-digit milliseconds except
two hardware-risk experiments. **Stop optimizing the render path** — every
remaining win in the system is in the book pipeline and power.

Owns: `display/`, `fw/src/display_flush/`, flush/prestage region of
`fw/src/tasks/display.rs`, `hal-ext/src/spi_dma.rs`.
Do not touch: `fw/src/sd_session.rs` (WS-D), boot-init region of the display
task (WS-C).

## Open, if the render path is ever revisited

Order: A11 (size it first) → A4 (verify first) → A5 (experiment).

### A11 (M): batch landscape glyph rows

Fallout from #50: **landscape is now the slower frame per page** — host, one
full reading page, landscape 146 µs against portrait's 93 µs. Landscape still
calls `blit_row` once per glyph row, re-paying per-row native-y computation,
frame match, and `reverse_bits` + `blit_native_bits` per source byte: roughly
700 ops per 16-row glyph against portrait's ~270. A whole-glyph landscape
path inside `blit_bitmap` should close most of the gap — batch the row setup;
the byte writes are already 8 pixels wide, so the win is setup amortization,
not fewer stores.

Lower priority than the portrait work was: portrait is the default
orientation (#5), landscape reading layout is already 16–18 ms, and the whole
term is single-digit milliseconds against a 424 ms turn. **Size the win
before building it.**

### A9 (L): overlapped SPI DMA band transmits

Double-buffered SPI DMA so band *N+1* is prepared while band *N* is on the
wire, taking flush transmission down to the hardware clock limit. This is
also the only honest home for prestage overlap: genuine overlap with panel
BUSY needs this machinery and cannot cancel a plane write mid-transaction.

### A4 (S code, medium hw risk): skip the RED-plane write when CTRL1 bypasses it

Non-Fast flushes write the same framebuffer to BW and RED
(`fw/src/display_flush/ssd1677.rs:55-62`, ~23 ms each), but `update_control_1`
for Full/FastClean is `[0x40, 0x00]` (`display/src/epd/ssd1677.rs:148-153`) —
the RED-bypass bit — and the prestage overwrites RED immediately afterwards.

Needs a hardware A/B before shipping; cold-boot ghost clearing is the
sensitive case. The emulator panel model validates RED writes, so its op plan
changes too (`tools/emulator/src/panel.rs`).

### A5 (M, high hw risk, opt-in only): temperature-override "hot" LUT for Fast

The 379 ms BUSY is the sensed-temperature OTP fast waveform — the only lever
below the floor. `FastClean` already proves the mechanism
(`FAST_CLEAN_TEMPERATURE` = 90 °C, skip the load-temp bit, restore after).
Apply a moderate override (35–50 °C) to `Fast` as an **opt-in RefreshPolicy
tier, never the default**. `RefreshMode` is shared with the emulators and is
never forked per panel.

Potentially 60–120 ms/turn. Risks: ghosting and contrast, unit and
temperature variance, more frequent FastClean eating the win. The emulator
cannot model waveform physics — this is pure hardware validation
(`page-turn` BUSY distribution, `thermal-run` cold/warm, long `reader-soak`
for ghost accumulation).

### Micro (fold into adjacent work)

`wait_ready`'s fixed 1 ms pre-delay (`hal-ext/src/spi_dma.rs:79-88`) → a
bounded wait-for-high-then-low. ~1–2 ms per refresh.

## Done

- **A1** (#12) — send `DisplayEvent::Settled` before the prestage and chapter
  tracking. This is what put the prestage off the critical path, and it is
  why A7 was backwards.
- **A2 / A2-P** (#24, #46) — byte-run rasterizer fast paths. `fill_span` and
  `blit_row` hoist the `map()` transform, bounds checks, bit-shifts and
  division out of the inner loop. Landscape reading layout 16–18 ms; portrait
  reached 28–34 ms; menus 82–90 → 3–8 ms.
- **A3** (#46) — panel-native framebuffer byte order. `Framebuffer::data` is
  stored in native order, bit-reversal and mirroring fold into the indexing
  math, `fill_transformed_band_impl` and `REVERSE_BITS_LUT` are gone, and the
  8 KB `TX_BAND` static is freed. Firmware streams `fb.band()` zero-copy over
  SPI. Goldens re-blessed deliberately.
- **A6** (#47) — O(1) ASCII direct indexing for glyph advance lookups, and a
  bypass of the kerning search when the font has no kerning entries.
- **A10-as-shipped** (#50) — **not what A10 specified.** A frame row runs down
  a native *column* in portrait, so `blit_row` could place only one pixel per
  read-modify-write and re-paid the column setup for each of a glyph's rows.
  Eight consecutive frame rows land in eight bits of the *same* native byte,
  so transposing an 8×8 block of the glyph (`transpose8`, Hacker's Delight
  7-3) collapses 64 masked writes into 8. `draw_glyph` and the SD custom-font
  path both route through it; empty blocks skip the transpose and the writes.
  **Portrait reading layout 33 → 13 ms median on X3 (2.5×, −20 ms/turn.)**
  Host predicted 2.1×; the device gained more, as expected — the removed work
  is dependent per-pixel loads, stores and unpredictable branches that an
  out-of-order host core hides and the C3 pays in full. Cost +852 B `.text`,
  +16 B `.rodata`, no `.bss` or stack change. `blit_bitmap` is pinned against
  the row-at-a-time loop across all four frames, both boards, every clipping
  edge; all goldens passed unblessed.

## Do not re-propose

- **A10 as originally specified — line-wrap / layout caching for the reading
  view.** Measured false. There is no wrapping in the render path to cache:
  the EPUB sink wraps at cache-build time, `push_line_block` stores one
  physical line per `BlockRecord` with `line_count: 1`, and `reader_page_at`
  is already an O(1) index into `ReaderStore::pages`. Host measurement of one
  portrait page: **2.8 µs** of pagination against **194 µs** of drawing.
  Reading-view render time moves on rasterizer work only.

  A first attempt (reverted) only flipped `block_height` to trust
  `BlockRecord::line_count` above one. Zero device effect, and it made an
  unvalidated cache byte authoritative geometry — a damaged byte could give
  one short block up to `255 * advance` of height. If paragraph-sized blocks
  are ever stored, that count needs validating against the block text on load,
  or a versioned multiline block format.

  **The premise survived three roadmap revisions because nobody measured the
  layout/rasterization split before ranking it first.** Any future "cache the
  layout" proposal needs that measurement before it is ranked.

- **A7 — skipping or deferring the RED prestage** (#48, reverted #52).
  Measured backwards on both counts.

  *It never fired.* Across two X3 50-turn captures at opposite cadences the
  prestage ran on **100 of 100** renders and the skip's own log line printed
  **zero** times — not for lack of queued work, since the burst capture
  contains a render with no preceding input. One `embassy_futures::yield_now()`
  is a single scheduling pass and cannot cover the round trip from the display
  task sending `Settled` to the app task clearing `rendering` and pushing the
  next `RenderRequest`.

  *And firing would have cost time.* The prestage write is already off the
  critical path thanks to A1. Skipping does not delete it — it defers it into
  the next Fast flush, ahead of `DisplayRefresh`, where the reader does wait:

  | | Next Fast flush |
  |---|---|
  | Prestaged | LoadBank → WritePlane(New) → DisplayRefresh |
  | Unstaged | LoadBank → **WritePlane(Old, Previous) → DataStop** → WritePlane(New) → DisplayRefresh |

  `fast_plan_only_writes_previous_plane_when_not_prestaged` pins that
  asymmetry and the X4 writes RED from `prev_fb` for the same reason. The skip
  is self-sustaining too — each skipped turn leaves the next unstaged — so a
  held button pays the write on-path *every* turn instead of off-path once.
  The reasoning lives as a comment at the call site. Genuine overlap is A9.

- **A8 — 4-way strided unrolling of the portrait `blit_row` column loop**
  (#49 closed). #50 removed that loop from the glyph path and took 2.5×
  instead of A8's projected 20–30%. Only `fill_span` is left for it, which is
  menu furniture already at 3–8 ms.

- Partial-window refresh (shelved twice), SPI above 40 MHz (rated ceiling),
  `MIRROR_Y=true` (tested, wrong), software work on the Full waveform (noise).
  RED prestaging itself already exists — A1 and A3 build on it.

## Bench protocol, learned the hard way

`page-turn` is operator-driven and its `page turn` statistic is
input→next-render, so it is **not cadence-robust**: a press landing mid-render
is credited only with the remainder, and burst captures show durations as low
as 2 ms. Quote `page turn` only from deliberate cadence — one press per
settled page. `layout_ms`, `flush_ms` and `busy_ms` are per-render and safe
from either.

Captures taken before the 2026-07-27 instrumentation split (#54) printed the
render event *after* the prestage, so their `page turn` runs ~24 ms long by
construction. **Do not compare them against newer runs.** The 354 ms figure
that once appeared as a baseline was a burst-cadence artifact and is retired;
it is not a number this system ever hit.
