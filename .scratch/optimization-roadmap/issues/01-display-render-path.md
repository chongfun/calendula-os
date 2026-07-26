# WS-A: Display render path — shave the ~50 ms of software around the 421 ms panel BUSY

Status: A1 DONE (#12). A2 DONE for landscape frames (#24). A2-P DONE (hoisted map, bit-shift, and division out of portrait inner loops; portrait reading layout dropped to 28–34 ms). A3 DONE (panel-native framebuffer byte order landed; fill_transformed_band_impl and 8 KB TX_BAND static removed; zero-copy fb.band() SPI streaming). A4/A5 hardware experiments, unscheduled.

Owns: `display/` crate, `fw/src/display_flush/`, flush/prestage region of `fw/src/tasks/display.rs`, `hal-ext/src/spi_dma.rs`.
Do not touch: `fw/src/sd_session.rs` (WS-D), boot-init region of display task (WS-C item 2 owns the double-`init_panel` fix).

Baseline: press-to-settled 470–473 ms; 421 ms is fast-waveform BUSY (89%). Non-panel budget: layout 20–36 ms + BW stream 22–24 ms + ~5 ms overhead. RED prestage (~23 ms) additionally gates the next turn's admission. Stacked target for items A1+A2+A3: ~450 ms press-to-settled with better held-button cadence.

## A1 (Tier 1, S): Send `DisplayEvent::Settled` before prestage and chapter tracking — DONE (#12)

## A2 (Tier 2, M): Byte-run rasterizer fast paths — DONE for landscape (#24)

## A2-P (Tier 2, M): Portrait byte-run/strided fast paths — DONE

Landed 2026-07-26: `fill_span` and `blit_row` in `FbFrame::Portrait` hoist `map()` coordinate transform, bounds checks, `byte_x()`, bit-shifts, and division out of the inner loop, stepping down native row indices via pointer adds (`index += stride`). Tested against `set_pixel()` per-pixel reference across all frames. Measured on X3 hardware: portrait reading layout dropped to **28–34 ms** (averaging ~31 ms per turn).

## A3 (Tier 2, M–L): Panel-native framebuffer byte order — DONE

Landed 2026-07-26: `Framebuffer::data` internal storage is stored in native byte order. Bit-reversal and X/Y mirroring are handled inside coordinate indexing math. `fill_transformed_band_impl` and `REVERSE_BITS_LUT` removed from `epd/mod.rs`. Deleted the 8 KB `TX_BAND` static buffer in `fw/src/tasks/display.rs`. Firmware streams `fb.band()` zero-copy directly over SPI. P1 (`MIRROR_X`) and P2 (`FbFrame::Native` identity) invariants verified; goldens re-blessed.

## Follow-on Display & Portrait Path Optimizations

### A6: Pre-computed Line-Wrap Caching (High Impact for Reading Layout)
In Reading View, ~16 ms (Landscape) to ~31 ms (Portrait) of every turn is spent in `ui/src/reading.rs` re-measuring glyph widths and re-computing word wrapping for every paragraph on the page. Pre-calculating and caching line-wrap offsets for adjacent pages while displaying the current page reduces reading layout time (`layout_ms`) to **near-0 ms** ($O(1)$ cache hit).

### A7: Asynchronous / Pipelined Prestaging (`prestage_ms` ~28 ms)
`prestage_previous` spends ~28 ms copying the active frame into the previous-frame buffer (`DTM1`) before initiating the E-Ink refresh. Overlapping prestaging with panel BUSY wait or SPI DMA transfers removes the **28 ms** prestage delay from the critical path.

### A8: 32-Bit Word-Wide Strided Iteration for Portrait Blits
`blit_row` in Portrait steps 1 byte at a time down the column stride (`index += ROW_BYTES`). Unrolling the loop with 32-bit `u32` word pointers on RISC-V processes 4 vertical native rows per iteration, reducing Portrait blitting CPU cycle count by ~20–30%.

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

Partial-window refresh (deliberately shelved ×2), SPI >40 MHz (rated ceiling), MIRROR_Y=true (tested, wrong), software work on the Full waveform ("noise" per IMPLEMENTATION_PLAN). RED prestaging already exists — A1/A3 build on it.

Suggested order: A1 ✓ → A2 ✓ → A2-P ✓ → A3 ✓ → A6 (line-wrap cache) → A7 (pipelined prestage) → A4 (verify-first) → A5 (experiment).

