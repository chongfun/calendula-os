# WS-E: Flash & RAM budget — image size, stack headroom

Status (2026-07-30): E1, E2 and E3 are done — **~246 KB flash freed, ~7 KB
stack headroom on both boards, `.data` 52 → 5 KB**. E4 is deferred until
flash headroom is actually wanted; flash is not tight.

Owns: `.cargo/config.toml`, `display/src/font.rs` (the `GlyphMetric` struct
and accessor), `fw/src/custom_font.rs` (decode side), generated font tables,
the `DISPLAY_EVENTS` declaration.

Every change here re-checks the link-time stack ASSERT **and**
`tools/check.sh stack-frames` on both X4 and X3. X3 is the tight board.

## Open

### E4 (L, deferred): ship secondary font families as SD packs

Fonts are 77% of the image: Merriweather is 1.30 MB and Literata SemiBold
(the "Heavy" setting) 556 KB, each shipping 1,631-codepoint coverage in every
variant. The whole SD-pack mechanism already exists
(`fw/src/custom_font.rs`, `proto/src/font_pack.rs`,
`tools/build_font_pack.py`), and
`docs/plans/2026-07-08-custom-fonts-investigation.md` already evaluates
SD-hosted packs — this re-proposes its path for the built-in secondary family.

**Deferred on purpose: flash is not currently tight** (~2.4 MB of slot
headroom). The payoff is OTA transfer (−33%) and room for more built-in faces,
neither of which anyone is asking for. Product trade-offs if it is ever taken
up: SD-pack rendering is slower than XIP; pagination caches version on a
family switch; the subsetting alternative degrades to `?` fallback glyphs and
invalidates caches through changed advance widths.

## Done

- **E1** — stack headroom pair: `ESP_HAL_CONFIG_PLACE_SWITCH_TABLES_IN_RAM =
  "false"` (3,058 B across 74 `.Lswitch.table.*` symbols were sitting in
  `.data`, including the same esp-radio `DisconnectReason` Debug table four
  times), plus `DISPLAY_EVENTS` 16 → 8 slots at 270 B each. ~5.1 KB of stack
  headroom on both boards.
- **E2** — packed `GlyphMetric` 16 → 12 bytes, adopting the layout the SD
  font-pack format already proved sufficient
  (`offset: u32, len: u16, w/h/x/y: u8, advance_fp: u16`). 49,802 entries,
  ~195 KB of flash, and the `FONT_METRICS` cache shrank 4,688 → ~3,516 B of
  `.bss`. `decode_metric` became near-identity.
- **E3** — zero-init `ReaderStore`, so the 47,240 B `SD_LIBRARY` moved from
  `.data` to `.bss`: ~46 KB of flash and a skipped 47 KB flash→RAM memcpy at
  boot. It was in `.data` because a few fields in `ReaderStore::new()` were
  non-zero — chiefly `EMPTY_TOC_RECORD.spine_index = -1` poisoning a multi-KB
  array with 0xFF.

## Do not re-propose — measured dead ends

- **Duplicate dependencies.** Three embassy-sync copies total **5.9 KB** of
  `.text`; heapless duplicates 1.4 KB; darling and syn are proc-macro only.
- **panic/fmt machinery** — under 20 KB total, and esp-backtrace is kept
  deliberately.
- **`opt-level="z"`** — ~15–30 KB, but the per-package opt-level 3 overrides
  were stack-measured. Not worth revalidating for that.
- **`partitions.csv`** — intentionally mirrors the stock layout. Leave alone.
- **`.rwtext.wifi`** (33.8 KB SRAM) must stay in IRAM; esp-radio has no
  placement knob.
- **ilp32e ABI and frame-pointer removal** — rejected in the stack brainstorm
  and still correct.
- **Runtime `StaticCell::init(ReaderStore::new())`** in place of
  `ConstStaticCell` — that constructs 47 KB on the stack, which is exactly
  what E3 was avoiding.
