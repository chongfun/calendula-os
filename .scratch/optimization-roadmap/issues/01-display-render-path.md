# WS-A: Display render path — shave the ~50 ms of software around the 421 ms panel BUSY

Status: A1 DONE (#12, incl. the wait_ready micro-fix). A2 DONE for landscape frames (#24 — goldens unchanged; on-device `layout_ms` A/B pending). NEXT, TOP PRIORITY: A2-P, the portrait extension — portrait became the default orientation (#5) and the shell is portrait-pinned, so the common path still draws per-pixel while #24's fast paths only fire on manual landscape holds. A3 unblocked (portrait landed; design its index math with A2-P, land after). A4/A5 hardware experiments, unscheduled. Baseline shift: #42 clocks both panels' display SPI at the datasheet 20 MHz — X3 plane writes ~25% faster, X4 refreshes pay ~17–20 ms; on-device check pending, each value a one-constant revert.

Owns: `display/` crate, `fw/src/display_flush/`, flush/prestage region of `fw/src/tasks/display.rs`, `hal-ext/src/spi_dma.rs`.
Do not touch: `fw/src/sd_session.rs` (WS-D), boot-init region of display task (WS-C item 2 owns the double-`init_panel` fix).

Baseline: press-to-settled 470–473 ms; 421 ms is fast-waveform BUSY (89%). Non-panel budget: layout 20–36 ms + BW stream 22–24 ms + ~5 ms overhead. RED prestage (~23 ms) additionally gates the next turn's admission. Stacked target for items A1+A2+A3: ~450 ms press-to-settled with better held-button cadence.

## A1 (Tier 1, S): Send `DisplayEvent::Settled` before prestage and chapter tracking

`fw/src/tasks/display.rs:200-236` currently runs `prestage_previous` (~22–24 ms) and `track_reading_chapter` (occasionally an SD session) *before* sending `Settled`/`PowerEvent::DisplaySettled`. Reorder: send `Settled` right after `flush()` Ok (after `record_render`/`prev_fb.copy_from`), then prestage. Both run on the same task, so prestage still completes before the next flush — `prev_prestaged` invariant intact. Keep `prestage_ms` in the `bench: render` line (print after prestage).

- Impact: ~20–25 ms per turn sustained cadence; removes chapter-crossing SD latency from press-to-settled.
- Risk check: power_task may send `DisplayCommand::Sleep` after `DisplaySettled`; sleep already handles `prev_prestaged` conservatively (display.rs:298) and commands queue behind the loop iteration.
- Verify: `bench.py channel-stress --host`, then `page-turn --turns 50` (median drops, `prestage_ms` stays ~23). No pixel change.

## A2 (Tier 2, M): Byte-run rasterizer fast paths — DONE for landscape (#24)

Landed 2026-07-25: `Framebuffer::fill_span`/`blit_row` byte-run primitives, landscape frames only, with fast-vs-per-pixel-reference equivalence tests across all four frames; goldens unchanged on both boards. The on-device `page-turn` `layout_ms` A/B is still pending. Portrait deliberately kept per-pixel — that gap is now A2-P below.

## A2-P (NEW, top priority, M): Portrait byte-run/strided fast paths

`fill_span` and `blit_row` fall back to the per-pixel loop in `FbFrame::Portrait` (`display/src/fb.rs:132-133,181-186`) — which #5 made the default reading orientation, on top of the already portrait-pinned shell. So nearly every frame the device draws pays the slowest path, re-running the frame `map()` per pixel. Portrait's map is a transpose (`(x,y) → (WIDTH-1-y, HEIGHT-1-x)`, `fb.rs:107`): a portrait row is a native **column** — one fixed bit position walking row-strided bytes — so #24's whole-byte row runs don't apply directly. Candidate shapes, in order of likely payoff per effort:

- (a) Hoist the map out of the loops: compute base byte index, bit mask, and row stride once per span / glyph row, then walk with pure adds — kills the per-pixel bounds-check + match + multiply without new geometry. Measure this first; it may capture most of the win.
- (b) Glyphs: process 8 portrait rows per pass with an 8×8 bit-transpose so each pass writes whole destination bytes down a glyph column.
- (c) `fill_rect`: iterate native rows (portrait columns) instead of portrait rows to recover whole-byte runs for tall fills.

- Impact: unmeasured — the portrait `page-turn` `layout_ms` baseline (round-2 bench session) comes first; expectation is the same shape as A2's landscape win, applied to the path that now renders everything.
- Must be bit-exact: goldens pass **unchanged** (no re-blessing); extend the existing fast-vs-per-pixel-reference equivalence tests — the harness in `fb.rs` already enumerates the Portrait frame.
- Verify: emulator runner vs `fixtures/golden` on both boards, display crate tests, portrait `page-turn` watching `layout_ms` p95 vs the 60 ms budget.
- Coordination: design the index math together with A3 so it is written once; A2-P lands first (goldens-unchanged), A3 re-blesses after.

## A3 (Tier 2, M–L): Panel-native framebuffer byte order — flush becomes a pure stream

Every RAM write runs `fill_transformed_band` (`display/src/epd/mod.rs:63-110`; X4 `MIRROR_X=true`, `REVERSE_BITS=true` at `display/src/epd/ssd1677.rs:37-39`) into `tx_band`, then `SpiDmaBus` copies again into its 8000-B DMA buffer (`fw/src/display_flush/ssd1677.rs:150-158`). Fold MIRROR_X/REVERSE_BITS into `Framebuffer::map`/`set_pixel` index math (`display/src/fb.rs:102-124`: mirrored byte index `ROW_BYTES-1-x/8`, mask `0x01 << (x&7)` — same arithmetic shape, zero per-pixel cost), making the panel transform identity; `write_ram` streams `fb.band()` directly, prestage streams `prev_fb` directly, and the 8 KB `TX_BAND` static (`fw/src/tasks/display.rs:60-61`) is freed → direct stack headroom.

- Impact: ~10–13 ms per turn (BW plane), same off prestage, ~2×12 ms off Full/FastClean; +8 KB RAM.
- Churn: `native_pixel` semantics change → emulator PNG dump/present, wasm canvas blit, and the UC8253 twin (`display/src/epd/uc8253.rs`, `tools/emulator/src/panel_uc8253.rs:248,355`, different constants — keep the seam per-panel) need the inverse transform at presentation or deliberate golden re-bless per `docs/agents/visual-verification.md`. X3 `MIRROR_Y` needs its own arm. `dram2` prev_fb slot size unchanged (fb.rs:42-49 repr(C)).
- Portrait (updated 2026-07-25): the "coordinate with portrait" blocker is resolved — portrait landed and is the default. The fold-in must cover the Portrait arm's transpose too; design the index math together with A2-P (same surface) and land after it, since A2-P is goldens-unchanged while A3 re-blesses deliberately.
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

Suggested order: A1 ✓ → A2 (landscape) ✓ → A2-P → A3 → A4 (verify-first) → A5 (experiment).
