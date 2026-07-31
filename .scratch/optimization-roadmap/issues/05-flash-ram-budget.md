# WS-E: Flash & RAM budget — image size, stack headroom

Status (2026-07-30): E1, E2 and E3 are done — **~246 KB flash freed, ~7 KB
stack headroom on both boards, `.data` 52 → 5 KB**. E4 is deferred until
flash headroom is actually wanted; flash is not tight.

Owns: `.cargo/config.toml`, `display/src/font.rs` (the `GlyphMetric` struct
and accessor), `fw/src/custom_font.rs` (decode side), generated font tables,
the `DISPLAY_EVENTS` declaration.

Every change here re-checks the link-time stack ASSERT **and**
`tools/check.sh stack-frames` on both X4 and X3. X3 is the tight board.

## The rule that governs this workstream (measured 2026-07-30)

**`_stack_end` is exactly `ADDR(.bss) + SIZEOF(.bss)` on both boards**, and
`.bss` starts right after `.data`. Verified two ways: by address arithmetic on
fresh ELFs (X3: `0x3FC8B340 + 0x3C2F8`; X4: `0x3FC8B320 + 0x3B040`), and by a
natural experiment — a stale binary differing by `.data` +552 and `.bss` −7,816
moved `_stack_end` by exactly −7,264.

**So every `.data`/`.bss` byte trades 1:1 against the main stack.** Two
corollaries that should govern every proposal here:

1. **Moving a stack temporary into a static is neutral at best**, and a net
   loss if that temporary was not on the peak call chain. See the anti-finding
   in issue 07 about hoisting the chapters array.
2. Only **deleting `.bss`**, or **shrinking a frame that is on the peak
   chain**, buys real margin.

### Measured footprint, fresh ELFs at `main` `d1bd126` (both boards built 2026-07-30)

Flashed image (Σ `PT_LOAD` FileSize): **X3 3,735,430 B / X4 3,720,275 B** —
**57.0% of the 6,553,600 B `app0` slot, 2.69 MB spare.** (More headroom than
the 2.4 MB previously recorded, because E2 and E3 landed.) Font tables are
**2,841,019 B — 96.9% of `.rodata`, 76.1% of the image**: 34 `*_BITMAP`
(2,030,745), 34 `*_METRICS` (597,624), 34 `*_KERNING` (197,022), 5 shared
`*_CODEPOINTS` (13,484). By family: merriweather 1,218,840 · literata_sizes
691,820 · literata_semibold 517,118 · literata 341,476 · literata_extra 70,845.

Stack regions: **X3 42,136 B**, X4 51,232 B, against the 27,648 B ASSERT.

Largest frames (X3, `tools/stack_frames.py` over a fresh disassembly):
`ensure_epub_scratch` **20,960** · `build_or_load_epub_cache_from_zip`
**13,840** · `parse_opf` **7,696** · display-task closure 5,232 ·
`views::render` 4,448 · `read_ota_layout` 4,192 · `write_image` 4,160 ·
`driftsort_main` 4,128 · `try_load_v2_book_cache` 4,048.

**Peak reader chain** (these genuinely nest): closure + `build_or_load` +
`parse_opf` = **26,768 B of 42,136 (63.5%)**, leaving ~15.4 KB for the
zip/inflate/SD leaves beneath. That is the binding constraint in the tree, and
E5/E6 below cut it roughly in half for two mechanical changes.

## Open

Order: E7 (trivial) → E5 → E6 → E8 (after the inflate branch) → E9.

### E5 (S–M): `CssRules` puts ~6.9 KB on the deepest frame

`fw/src/book_build.rs` builds `CssRules::new()` as a plain stack local inside
the 13,840 B frame. `CssRule { selector: heapless::String<64>, align }` is
68–72 B and `Vec<CssRule, MAX_CSS_RULES=96>` is **6,532–6,916 B**. (That frame
decomposes as CssRules ~6.9 K + `EpubPackage` sret ~6.0 K + two small strings
≈ 13.3 K, which matches the measured 13,840 to within spills.)

Store selectors as a `Span` into the CSS text — the pattern `ManifestItem` and
`SpineItem` already use — for 96 × 8 = 772 B, **−6.1 KB**. Cheap version:
`String<32>` for **−3.1 KB**, since `selector_is_supported` already rejects
long selectors. *Arithmetic from struct layout, corroborated by the measured
frame.* On the peak chain, no `.bss` growth. Risk low.

### E6 (M): `parse_opf` keeps a second live copy of manifest + spine

`proto/src/epub.rs` builds `Vec<ManifestItem,224>` (3,588 B),
`Vec<SpineItem,192>` (2,308 B) and `Vec<&str,192>` (1,540 B) on its **own**
stack and moves them into the sret slot at the end — so **5,896 B is live
twice** while `build_or_load`'s frame is also up. Sum 7,436 B against the
measured 7,696 B frame.

Take `out: &mut EpubPackage<'a>`, or build the two `Vec`s through the sret
pointer. **−5,896 B off the peak chain, no `.bss` growth.** Risk low–medium
(signature change, several call sites).

**E5 + E6 together take the peak reader chain from ~26.8 KB to ~14.8 KB** —
roughly doubling the margin — for two mechanical changes with no format or
behaviour implications.

### E7 (S): one stable sort of 20 elements costs a 4,128 B frame and 3.8 KB of flash

`fw/src/tasks/wifi.rs` sorts the scan results with `sort_by_key`. Measured:
`core::slice::sort::stable::driftsort_main` carries a **4,128 B** frame (its
`AlignedStorage` scratch), and the stable-sort machinery totals **3,804 B** of
`.text` (`drift::sort` 1,840, `quicksort` 1,288, and four smaller symbols).

`sort_unstable_by_key` (ipnsort) has no scratch buffer. **Stability is
irrelevant here** — the dedup loop below keys on SSID and keeps the first
occurrence, which is the strongest either way. *Measured, not estimated.*
The cheapest item in this file; it does not move the binding constraint (the
wifi chain is well under the reader peak) but it is nearly free.

### E8 (L, sequence after `opt/inflate-caller-owned-window`): merge the inflate window with the XHTML window

`ZipInflateScratch` is **43,284 B**, of which **32,768 B** is miniz_oxide's
private LZ77 dict; the residue is `DecompressorOxide` (~10.5 KB).
`inflate_chunks_to_sink` then decompresses into that private dict and
**memcpys out into a second buffer**, `EPUB_XHTML` = **24,577 B**. Two
buffers, 67,861 B.

`inflate::core::decompress` with `TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF`
lets the caller's output buffer *be* the window. One 32,768 B buffer serving
as both window and XHTML text window, plus the decompressor, saves
**24,577 B of `.bss` — i.e. 24,577 B of stack** on both boards, and removes a
full copy of every decompressed byte.

The in-flight branch already does the first half (caller-owned window). This
is the second half: collapsing the *second* buffer. Sequence it after that
branch — same code, would conflict. Note `READER_XHTML_SCRATCH` grows
24,576 → 32,768, and the on-card CONT.BIN record bound is derived from it.

### E9 (S): `MetricCache` holds 5,328 B of `.bss` unconditionally

Measured 5,328 B, resident on every boot, read only when the card holds a font
pack *and* `FontFamily::Custom` is selected. No heap means it cannot be lazy,
but `METRIC_CACHE_SLOTS` / `NON_ASCII_METRIC_SLOTS` are tunable — halving the
ASCII slots returns ~2.3 KB of stack on both boards. Pure cache-hit-rate
trade-off on custom-font books.

*(Related, WS-D's file so only noted here: `run_sd_session` monomorphizes 23×
for **34,334 B** of `.text` — 27,516 in `run_sd_session`, 6,818 in
`with_root`, largest copy 5,070 B — for a body that is identical in all 23.
A thin generic shim over one `&mut dyn FnMut(&SdRoot<'_>)` body would return
~28–30 KB of flash. Flash is not tight, so this is low priority, and the
`#[inline(never)]` there is load-bearing for stack — re-run `stack-frames`.)*

### E4 (L, deferred): ship secondary font families as SD packs

Fonts are 77% of the image: Merriweather is 1.30 MB and Literata SemiBold
(the "Heavy" setting) 556 KB, each shipping 1,631-codepoint coverage in every
variant. The whole SD-pack mechanism already exists
(`fw/src/custom_font.rs`, `proto/src/font_pack.rs`,
`tools/build_font_pack.py`), and
`docs/plans/2026-07-08-custom-fonts-investigation.md` already evaluates
SD-hosted packs — this re-proposes its path for the built-in secondary family.

**Deferred on purpose: flash is not currently tight** — re-measured
2026-07-30 at **57.0% of the slot, 2.69 MB spare**, so the case is weaker than
when this was written, not stronger. The payoff is OTA transfer (−33%) and room
for more built-in faces, neither of which anyone is asking for. Exact current
numbers if it is ever taken up: merriweather 1,218,840 B + literata_semibold
517,118 B = **1,735,958 B**, taking the image to ~2.0 MB.

**But note WS-F's F5 re-scope:** the *same* Merriweather bytes are 42.9% of the
web emulator's wasm and it is not the default face there either, where the win
is −41% on every first visit rather than on an OTA nobody is waiting for. If
the font-hosting work is ever done, the web side is where it pays first. Product trade-offs if it is ever taken
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
