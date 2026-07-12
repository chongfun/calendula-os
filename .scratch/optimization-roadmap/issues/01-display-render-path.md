# WS-A: Display render path — shave the ~50 ms of software around the 421 ms panel BUSY

Status: A1 DONE (#12, incl. the wait_ready micro-fix). Next: A2 (unblocked by E2; gate on Landscape frames — portrait is active work). A3 open (coordinate with portrait first). A4/A5 hardware experiments, unscheduled.

Owns: `display/` crate, `fw/src/display_flush/`, flush/prestage region of `fw/src/tasks/display.rs`, `hal-ext/src/spi_dma.rs`.
Do not touch: `fw/src/sd_session.rs` (WS-D), boot-init region of display task (WS-C item 2 owns the double-`init_panel` fix).

Baseline: press-to-settled 470–473 ms; 421 ms is fast-waveform BUSY (89%). Non-panel budget: layout 20–36 ms + BW stream 22–24 ms + ~5 ms overhead. RED prestage (~23 ms) additionally gates the next turn's admission. Stacked target for items A1+A2+A3: ~450 ms press-to-settled with better held-button cadence.

## A1 (Tier 1, S): Send `DisplayEvent::Settled` before prestage and chapter tracking

`fw/src/tasks/display.rs:200-236` currently runs `prestage_previous` (~22–24 ms) and `track_reading_chapter` (occasionally an SD session) *before* sending `Settled`/`PowerEvent::DisplaySettled`. Reorder: send `Settled` right after `flush()` Ok (after `record_render`/`prev_fb.copy_from`), then prestage. Both run on the same task, so prestage still completes before the next flush — `prev_prestaged` invariant intact. Keep `prestage_ms` in the `bench: render` line (print after prestage).

- Impact: ~20–25 ms per turn sustained cadence; removes chapter-crossing SD latency from press-to-settled.
- Risk check: power_task may send `DisplayCommand::Sleep` after `DisplaySettled`; sleep already handles `prev_prestaged` conservatively (display.rs:298) and commands queue behind the loop iteration.
- Verify: `bench.py channel-stress --host`, then `page-turn --turns 50` (median drops, `prestage_ms` stays ~23). No pixel change.

## A2 (Tier 2, M): Byte-run rasterizer fast paths

All drawing goes per-pixel through `set_pixel` (re-runs frame `map()` per pixel). `render::fill_rect` (`display/src/render.rs:37-49`) and `font::draw_glyph` (`display/src/font.rs:447-467`) can take byte-run paths in Landscape/LandscapeFlipped frames: whole-byte fills with masked edges; glyph rows blitted by shifting the packed MSB-first glyph row into the destination byte pair. Keep per-pixel as Portrait/odd-case fallback — Portrait mode is active work (`docs/plans/2026-07-09-portrait-mode.md`), gate on frame.

- Impact: Reading layout 20–36 → ~10–15 ms; menus 82–90 ms likely well under half (shortens FastClean view changes + sleep path).
- Must be bit-exact: goldens are the oracle and must pass **unchanged** (no re-blessing).
- Verify: emulator runner vs `fixtures/golden`, display crate tests, `page-turn` watching `layout_ms` p95 vs 60 ms budget.
- Coordination: touches `font.rs` — sequence vs WS-E item 13 (metric struct); land 13 first or rebase.

## A3 (Tier 2, M–L): Panel-native framebuffer byte order — flush becomes a pure stream

Every RAM write runs `fill_transformed_band` (`display/src/epd/mod.rs:63-110`; X4 `MIRROR_X=true`, `REVERSE_BITS=true` at `display/src/epd/ssd1677.rs:37-39`) into `tx_band`, then `SpiDmaBus` copies again into its 8000-B DMA buffer (`fw/src/display_flush/ssd1677.rs:150-158`). Fold MIRROR_X/REVERSE_BITS into `Framebuffer::map`/`set_pixel` index math (`display/src/fb.rs:102-124`: mirrored byte index `ROW_BYTES-1-x/8`, mask `0x01 << (x&7)` — same arithmetic shape, zero per-pixel cost), making the panel transform identity; `write_ram` streams `fb.band()` directly, prestage streams `prev_fb` directly, and the 8 KB `TX_BAND` static (`fw/src/tasks/display.rs:60-61`) is freed → direct stack headroom.

- Impact: ~10–13 ms per turn (BW plane), same off prestage, ~2×12 ms off Full/FastClean; +8 KB RAM.
- Churn: `native_pixel` semantics change → emulator PNG dump/present, wasm canvas blit, and the UC8253 twin (`display/src/epd/uc8253.rs`, `tools/emulator/src/panel_uc8253.rs:248,355`, different constants — keep the seam per-panel) need the inverse transform at presentation or deliberate golden re-bless per `docs/agents/visual-verification.md`. X3 `MIRROR_Y` needs its own arm. `dram2` prev_fb slot size unchanged (fb.rs:42-49 repr(C)).
- Verify: rewritten fb.rs unit tests, emulator vs goldens, hardware `page-turn` expecting `flush_ms` ≈ 421+~11 and `prestage_ms` ≈ 11.
- **Supersedes** the DMA-overlap alternative (two-band pipelining, which would *spend* 8 KB). Do not implement both.

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

Suggested order: A1 → A2 → A3 → A4 (verify-first) → A5 (experiment).
