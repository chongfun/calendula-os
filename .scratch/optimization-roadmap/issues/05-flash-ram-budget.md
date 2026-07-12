# WS-E: Flash & RAM budget — image size, stack headroom

Status: E1+E2+E3 DONE (measured: ~246 KB flash freed, ~7 KB stack headroom both boards, .data 52→5 KB). E4 deferred until flash headroom is wanted.

Owns: `.cargo/config.toml`, `display/src/font.rs` (GlyphMetric struct + accessor), `fw/src/reader_store.rs`, `fw/src/custom_font.rs` (decode side), generated font tables, DISPLAY_EVENTS declaration.
Coordination: `font.rs`/`custom_font.rs` overlap WS-A item A2 and WS-B item B1 — land E2 (metric packing) **first**, then those proceed independently.

Measured baseline (release ELF, 2026-07-09): image 3.87 MB in a 6.25 MB OTA slot; `.rodata` 3.11 MB (fonts 2.97 MB: bitmaps 2.03 MB, **metrics 797 KB across 34 tables**, kerning 197 KB); `.data` 57.1 KB of which `SD_LIBRARY` is 47.2 KB; `.bss` 201.6 KB; main stack 39.4 KB X4 (down from 45.7 KB on 2026-07-07 — statics grew), X3 ~low-30s vs the 27 KB link-time ASSERT. `.eh_frame` is non-alloc — not flashed, ignore it.

## E1 (Tier 1, S): Stack headroom pair — switch tables to flash + halve DISPLAY_EVENTS

Both are parked options from `docs/brainstorms/2026-07-07-stack-headroom-options.md`, re-proposed because the stack has since shrunk ~6 KB and the tables have grown: **3,058 B across 74 `.Lswitch.table.*` symbols in .data** (incl. the same esp-radio DisconnectReason Debug table 4×). One line in `.cargo/config.toml`: `[env] ESP_HAL_CONFIG_PLACE_SWITCH_TABLES_IN_RAM = "false"`. Plus DISPLAY_EVENTS 16→8 slots (270 B/slot, 4,320 B .bss today) → ~2.1 KB.

- Impact: ~5.1 KB stack headroom on both devices; X3 is the one nearing the floor.
- Risk: switch dispatch runs from icache — a miss during flash-cache-suspended windows (esp-storage NVM writes) adds latency; probably unmeasurable but validate. Halved queue → more drop-with-retry during cache-build bursts; watch the "required display event queue full" log.
- Verify: `llvm-nm` `_stack_start − _stack_end` diff; `bench: render` timing lines before/after; sync-session smoke test (radio active); cold build of a large EPUB while mashing buttons.

## E2 (Tier 2, S–M): Pack `GlyphMetric` 16 → 12 bytes (~195 KB flash)

`display::font::GlyphMetric` is `{offset: usize, len: usize, w/h: u8, x/y: i8, advance_fp: u16}` = 16 B padded; 49,802 entries = 796,832 B (~26% of font data). The repo already proves 12 B suffices: the SD font-pack format (`proto/src/font_pack.rs`, `FONT_PACK_METRIC_BYTES = 12`) and `decode_metric()` in `fw/src/custom_font.rs` use `offset: u32, len: u16, w/h/x/y: u8, advance_fp: u16`. Adopt that layout in-flash; generated `*_generated.rs` struct literals largely recompile unchanged — only `usize` use-sites need casts (the `glyph()` accessor already slices).

- Impact: ~195 KB flash (3.87 → ~3.68 MB); bonus: `FONT_METRICS` cache shrinks 4,688 → ~3,516 B .bss (~1.2 KB stack headroom); `decode_metric` becomes near-identity.
- Risk: low — pure representation change, no timing-sensitive code, .rodata is XIP.
- Verify: `llvm-size -A` (−195 KB .rodata); golden frames + `tools/preview`; one `bench: render` run; custom font pack still opens (shared decode path).

## E3 (Tier 2, M): Zero-init `ReaderStore` → `SD_LIBRARY` moves .data → .bss (~46 KB flash + faster boot)

`SD_LIBRARY: ConstStaticCell<ReaderStore>` (`fw/src/tasks/display.rs:76`) is 47,240 B — 83% of .data — because a few fields in `ReaderStore::new()` are non-zero: `EMPTY_TOC_RECORD.spine_index = -1` poisons a multi-KB array with 0xFF (`fw/src/reader_store.rs:66`), plus cover dims and TypeSettings defaults (discriminant 1). Make the const value all-zero bytes — e.g. `spine_plus1: u16` (0 = none) or inverted sentinel; set cover dims / type settings at first use (both overwritten before read in the normal flow) — and the linker moves it to .bss automatically. **Do not** switch to runtime `StaticCell::init(ReaderStore::new())` — that constructs 47 KB on the stack, exactly what ConstStaticCell avoids.

- Impact: ~46 KB flash image; skips a 47 KB flash→RAM memcpy at boot; RAM unchanged.
- Risk: sentinel semantics touch several `spine_index` comparison sites — logic bugs, no timing/stack risk.
- Verify: `llvm-nm` shows `SD_LIBRARY` type `b` not `d`; .data drops to ~10 KB; full reader flow (scan, open, TOC/Chapters) on emulator + device.

## E4 (Tier 3, L, deferred until flash headroom is wanted): Ship secondary font families as SD packs

Fonts are 77% of the image; Merriweather is 1.30 MB, Literata SemiBold ("Heavy" setting) 556 KB, each shipping 1,631-codepoint coverage in every variant. The whole SD-pack mechanism exists (`fw/src/custom_font.rs`, `proto/src/font_pack.rs`, `tools/build_font_pack.py`); `docs/plans/2026-07-08-custom-fonts-investigation.md` already evaluates SD-hosted packs — this re-proposes its path for the built-in secondary family. Flash is not currently tight (2.4 MB slot headroom); payoff is OTA transfer (−33%) and headroom for more built-in faces. Product tradeoffs: SD-pack rendering is slower than XIP; pagination cache versioning on family switch; subsetting alternative degrades to `?` fallback glyphs (`ui/src/reading.rs:546`) and invalidates caches via advance widths.

## Measured dead ends — do not chase

Duplicate deps: 3× embassy-sync copies total **5.9 KB** .text; heapless dupes 1.4 KB; darling/syn are proc-macro only. panic/fmt machinery <20 KB total and esp-backtrace is kept deliberately. `opt-level="z"`: ~15–30 KB but the per-package opt-level 3 overrides were stack-measured — not worth revalidating. `partitions.csv` intentionally mirrors stock layout — leave alone. `.rwtext.wifi` (33.8 KB SRAM) must stay in IRAM; no esp-radio placement knob. ilp32e ABI and frame-pointer removal: rejected in the brainstorm, still correct.

Suggested order: E1 → E2 → E3 → E4 only when headroom is actually needed. Every change re-checks the stack ASSERT on both X4 and X3.
