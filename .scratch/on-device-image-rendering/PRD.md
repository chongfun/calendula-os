# On-Device Image Rendering — PRD

Status: **planning, no decisions closed.** Drafted 2026-07-25 from a review of
`fix/svg-wrapped-images` and a measurement pass over the X4 release image. No
code written. Numbers below marked *(measured)* come from this repo at
`c8439e8`; numbers marked *(estimate)* have not been benched and are the main
thing milestone 0 exists to settle.

## Summary

CalendulaOS cannot decode an image on the device. It can *display* one — the
1bpp cover path is complete from cache file to panel — but the decode happens on
a developer's Mac in `tools/preview`, which writes a pre-rasterized `COVER.BIN`
to the SD card. Any book that did not go through that manual step shows no
cover, and no EPUB illustration is ever visible: the XHTML parser turns `<img>`
into an alt-text placeholder and the bytes are never opened.

This PRD proposes closing that gap with a `no_std`, no-alloc **baseline JPEG
decoder** in `proto`, staged so the cheapest useful win lands first:

- **M0** — host spike: decoder + dither, benched against real EPUB JPEGs.
- **M1** — on-device cover decode, replacing the host bake. Existing
  destination, existing UI, ~1/8 scale factor, cheapest possible decode.
- **M2** — full-page image pages in the reading flow: covers, art inserts, and
  full-page chapter illustrations.

Inline images that flow with text are explicitly out of scope.

## Background: what already exists

This was not obvious and is the reason the work is smaller than it looks. The
*destination* half is built and wired:

| Piece | Location |
| --- | --- |
| Cache format `X4CV`, 202×303 @ 1bpp, 7,878 bytes | `proto/src/cache.rs:48` |
| Cover cache reader | `fw/src/reader_cache_files.rs:485` |
| Resident buffer `ReaderStore::cover_bits` | `fw/src/reader_store.rs:183` |
| UI model `UiCover` | `ui/src/lib.rs:69` |
| View wiring (home/library) | `fw/src/views.rs:135` |
| **Host-side decoder + resize + threshold** | `tools/preview/src/main.rs:250` |

`tools/preview` is the only thing in the tree that decodes a JPEG. It uses the
`image` crate, `resize_to_fill` to 202×303, then a hard threshold at luma < 180
with no dithering. Nothing on the device writes `COVER.BIN`.

The reading path, by contrast, has no image concept at all. `XhtmlBlockStreamParser`
emits a text placeholder for `<img>` (`proto/src/epub.rs:1921`) and pagination
treats it as an ordinary body block. The image's manifest entry, dimensions, and
bytes are never consulted.

## Goals

- A book's cover renders on-device with no host preprocessing step.
- Full-page illustrations in an EPUB render as pages when reading.
- The reading path stays allocation-free and inside its existing RAM and stack
  budget; no new large statics.
- Malformed, unsupported, and oversized images degrade to today's placeholder
  behavior rather than failing an open or panicking.

## Non-goals

- **Inline images flowed with text.** Pagination is precomputed into the section
  cache at open time; making images participate in line layout is a much larger
  change with a much worse payoff on a 1bpp panel.
- **Grayscale output.** `display/src/epd/uc8253.rs:13` records that grayscale is
  deliberately not ported. X4's SSD1677 runs OTP waveforms selected via `0x1A`;
  custom multi-pass gray LUTs are a separate project.
- **PNG.** See "Format policy" — the RAM shape makes it a bad fit today.
- **Progressive JPEG.** Needs the whole coefficient plane resident.
- **Image-aware sync/upload.** The Wi-Fi session is untouched.

## Evidence

### The corpus is favorable *(measured)*

`EIGHTY-SIX-VOLUME-1.epub`, 20 images, all JPEG:

- **All baseline** (`SOF0`), zero progressive. This is the streamable case.
- Typical art is 1200×1800; largest file is the 1,170,441-byte cover.
- All are **DEFLATE-compressed inside the ZIP** (`compress_type 8`), not stored.
  So: one streaming inflate pass, no seeking, no random access. The existing
  `ZipStream` bounded-window API is already exactly this shape.
- Total image payload 8.7 MB across the book.

Note the book has **zero `<svg>` and zero `<image>` elements** — all 39 image
references are plain `<img>`. Any SVG-wrapped-cover handling is a separate,
real, but different problem.

### DRAM has no free bytes, but has ~84 KB of borrowable scratch *(measured)*

Parsed from the X4 release ELF:

| Region | Bytes |
| --- | --- |
| statics (`.data` + `.bss` + rwdata shadow) | 294,864 |
| main stack (`_stack_start - _stack_end`) | 43,944 |
| previous framebuffer (dram2, pinned to top) | 48,001 |
| **unallocated** | **~0** |

The map is packed: statics run up to `_stack_end`, the stack runs up to the
previous-framebuffer slot, and that slot is packed against the top of
`dram2_seg`. A decoder cannot simply add a static.

It does not need to. The largest statics are EPUB **open-time** scratch owned by
the display task, idle during a render:

| Static | Bytes | Live during |
| --- | --- | --- |
| `EPUB_SCRATCH` (miniz `InflateState`: 32 KB window + tables) | 43,316 | open + any zip read |
| `EPUB_XHTML` | 24,576 | open only |
| `EPUB_OPF` | 16,385 | open only |
| `EPUB_COMPRESSED` | 8,192 | open + any zip read |

The decoder needs the inflate scratch *concurrently* (the JPEG is deflated), but
`EPUB_XHTML` and `EPUB_OPF` are free — ~41 KB against an estimated 8–12 KB
working set. The single-writer rule holds: the display task already owns SD CS,
`ReaderStore`, and the framebuffer, so nothing new crosses an ownership boundary.

### Time fits inside a refresh the user already pays for *(measured budgets)*

`tools/bench/benches.toml`:

- Full refresh busy: **3000–4300 ms**. An image page needs a Full refresh anyway
  — Fast/partial ghosts badly on high-contrast art.
- Fast refresh: ~500 ms; median press-to-settled 550 ms.
- Warm book open: 150 ms.

*(estimate)* A 1200×1800 4:2:0 baseline decode is ~50,600 blocks; at roughly
1,500–3,000 cycles/block on a 160 MHz RV32IMC with scaled IDCT, that is
**0.5–1.5 s** of entropy+IDCT work. SD read plus inflate of ~700 KB–1.1 MB adds
an unknown amount at 25 MHz SPI. Working assumption: **2–5 s added to a page
that already costs 3–4 s.** M0 exists to replace this paragraph with real
numbers.

### Flash is a non-issue *(measured)*

`app0` is 0x640000 (6.5 MB); the current image is ~3.7 MB. A decoder is 10–20 KB.

## Design

### Pipeline

```text
SD -> ZipStream (bounded window, existing) -> inflate (existing, 43 KB)
   -> baseline JPEG decode (new, ~10 KB, luma only)
   -> box downscale to target -> Floyd-Steinberg dither -> 1bpp
   -> COVER.BIN (M1) or framebuffer (M2)
```

Every stage is streaming. Nothing holds a full decoded image.

### Decoder shape

New module in `proto` (pure logic, host-testable, per the sans-IO seam rule in
AGENTS.md). Constraints:

- `no_std`, no alloc, no recursion, no panicking paths on external data —
  malformed input returns `Err`, per the release-aborts-on-panic rule.
- All state in a caller-owned struct the firmware places in borrowed scratch.
- **Luma only.** Chroma coefficients must still be Huffman-decoded to advance
  the bitstream, but their IDCT and upsampling are skipped entirely. Output is
  1bpp, so chroma is dead weight.
- **DCT-domain scaling** picks the cheapest sufficient reduction: 1/8 (DC-only,
  no IDCT at all), 1/4, 1/2, or 1/1, chosen so the result is ≥ the target and
  then box-filtered down the remainder.
  - M1: 1200×1800 → 202×303 is ~1/6, so **1/8 DC-only then upscale**, or 1/4
    then box-down. DC-only is nearly free and is the first thing to try.
  - M2: 1200×1800 → ~480×720 is ~1/2.5, so **1/2 then box-down**.
- Output is delivered band-by-band through a callback, mirroring how
  `ZipStream` emits into an `output_window`.

### Dithering

Floyd–Steinberg across the target width; one `i16` error row (~1 KB at 480 px,
~1.6 KB at 800 px). This is a visible quality win over the current host
threshold and costs almost nothing. Ordered/Bayer is the fallback if the error
row proves awkward.

Worth doing independently: `tools/preview` should use the same dither so
host-baked and device-decoded covers match, and so the emulator's golden frames
mean something.

### Format policy

| Input | Behavior |
| --- | --- |
| Baseline JPEG (`SOF0`/`SOF1`) | Decode |
| Progressive JPEG (`SOF2`) | Refuse → placeholder |
| Arithmetic-coded, 12-bit, lossless | Refuse → placeholder |
| PNG | Refuse → placeholder (see below) |
| Anything else / malformed / truncated | Refuse → placeholder |

**PNG is refused because of a RAM shape, not a missing feature.** PNG's IDAT is
a DEFLATE stream, and in this corpus the ZIP entry is *also* DEFLATE — two
concurrent 32 KB windows, ~64 KB that does not exist. A PNG stored uncompressed
in the ZIP (`compress_type 0`, common since PNG is already compressed) would
need only one window and could be supported later. Out of scope for now.

Refusal must be cheap and must never fail the enclosing open or render.

### M1 — on-device cover decode

Smallest scope with a real user-visible result, and it removes a manual host
step. The destination already exists; the only new thing is producing those
7,878 bytes on the device.

- Cover discovery already exists in `tools/preview` (`find_cover_href`:
  `properties=cover-image`, else a manifest item whose id/href says "cover").
  Port it to `proto` so host and firmware agree.
- Decode during the existing book-open/cache-build phase, where `EPUB_XHTML` and
  `EPUB_OPF` are in play but the XHTML pass is done — sequence it after.
- Write `COVER.BIN` through the existing `with_v2_cover_file` path.
- On any failure, leave `COVER.BIN` absent — the reader already handles that.

At ~1/8 DC-only, this is the cheapest decode in the design and the best place to
find out whether the estimates hold.

### M2 — full-page image pages

- The XHTML parser emits an **image marker block** carrying the resolved
  manifest href — not alt text. A section-cache record marks the page as
  image-backed.
- At render time the display task streams the image out of the EPUB, decodes at
  1/2 or 1/4, dithers into the framebuffer, and forces a Full refresh via
  `RefreshPlanner`.
- Heuristic for "full-page": the image is the only meaningful content in its
  container, or its intrinsic aspect ratio is within tolerance of the page.
  Everything else keeps today's placeholder.
- Reading from the EPUB at render time is a **new SD access pattern in the
  render path** — today a render reads only the section cache. Structurally fine
  (same owner), but it is the main latency and correctness risk in M2.

## Interaction with `fix/svg-wrapped-images`

That branch should not land as-is (see the review: it does not change this
book's SVG handling because the book has no SVG, it leaks SVG `<title>`/`<desc>`
into prose, it pollutes EPUB3 nav TOC labels, and it turns 20 decorative
`alt=""` ornaments into `[Image]` lines mid-chapter).

More importantly it points the wrong way for M2. The forward-compatible shape is
a marker carrying the image's href, which the render path can resolve to bytes.
A text placeholder built from `alt`/`title` cannot become a rendered image later.
If a placeholder-quality fix is wanted before M2, it should be scoped to that
and should not remove `svg` from either skip list.

## RAM and stack budget

- **No new statics.** The decoder state lives in scratch borrowed from
  `EPUB_XHTML`/`EPUB_OPF` via the existing `ReaderCacheScratch` ownership.
- Estimated working set: Huffman tables (~2 KB with fast lookup), quant tables
  (512 B), one MCU output band (~5 KB at 1/2 scale on a 1200-wide source), one
  dither error row (~1–1.6 KB). **~8–12 KB.**
- **Stack: no recursion, no large locals.** The `_stack_start - _stack_end >= 27 KB`
  assert in `fw/build.rs:60` must still hold, and the EPUB-open chain already
  peaks near that floor. M0 must report a `-Zemit-stack-sizes` figure for the
  decoder before any firmware change lands.

## Risks

1. **Decode is slower than estimated.** Mitigation: M0 benches before any
   firmware work; if 1/2-scale is too slow, M2 drops to 1/4 and upscales.
2. **Stack regression in the open chain.** The floor has shipped silent `.bss`
   corruption once before. Mitigation: measure, and keep decoder state entirely
   in borrowed statics.
3. **Scratch aliasing.** Borrowing `EPUB_XHTML` for decode while something still
   holds a reference would be a genuine memory bug. Mitigation: route through
   `ReaderCacheScratch` so the borrow checker enforces exclusivity.
4. **1bpp art looks bad.** Light-novel line art dithers well; photographic
   covers less so. Mitigation: evaluate real output in M0 before committing to
   M2. This is a legitimate reason to stop after M1.
5. **M2 lengthens page turns unpredictably.** Mitigation: image pages force Full
   refresh anyway; consider a "rendering" plate if decode exceeds a threshold.
6. **Golden-frame churn.** Any dither change rewrites frames that contain
   covers. Mitigation: land the dither and the `tools/preview` change together
   so host and device agree in one step.

## Verification plan

Per AGENTS.md, and to be restated concretely in each issue:

- `tools/check.sh fmt` and `tools/check.sh fast` for the `proto` decoder work.
- `tools/check.sh emulator` on **both X4 and X3** for anything that changes
  rendered output — dithering, cover drawing, image pages.
- `tools/check.sh firmware` for the display-task and scratch-ownership changes.
- `tools/check.sh all` before any PR is ready.
- Host regression tests in `proto` against the real corpus: baseline decode,
  progressive refusal, truncated-stream refusal, and a fixed-output dither test.
- `tools/bench/bench.py page-turn` and `storage-cache` on hardware for M1/M2
  latency, with a new budget line once known-good runs exist.
- Explicitly report the `-Zemit-stack-sizes` delta for the open chain.

## Build sequence

**M0 — host spike (no firmware changes)**
1. Baseline JPEG decoder in `proto`, `no_std`/no-alloc, luma-only, DCT-scaled.
2. Floyd–Steinberg dither to 1bpp.
3. Host tests over the 20 JPEGs in `EIGHTY-SIX-VOLUME-1.epub`.
4. Bench: cycles/decode at 1/8, 1/4, 1/2; peak working set; stack sizes.
5. Eyeball real dithered output at 202×303 and at page size.
6. **Decision gate:** do the numbers and the picture justify M1/M2?

**M1 — on-device cover**
7. Port cover discovery from `tools/preview` into `proto`.
8. Wire decode into book open using borrowed scratch; write `COVER.BIN`.
9. Switch `tools/preview` to the same decode + dither so host and device agree;
   regenerate affected golden frames in one commit.
10. Bench cold/warm open on hardware.

**M2 — full-page image pages**
11. Image marker block in the XHTML parser (carrying href, not alt text).
12. Section-cache record for image-backed pages.
13. Render-time decode into the framebuffer + forced Full refresh.
14. Full-page heuristic; everything else keeps the placeholder.
15. Emulator goldens on X4 and X3; hardware page-turn bench.

## Open questions

- **Stop after M1?** If dithered full-page art looks poor, M1 alone (covers, no
  host step) may be the whole win. Decide at the M0 gate.
- **X3 target size.** The cover cache is fixed at 202×303 with an `x4_dock_clean()`
  constructor. Does X3's 792×528 geometry want a different cover size, and does
  that mean a `COVER_VERSION` bump?
- **Cache invalidation.** Should an existing host-baked `COVER.BIN` be trusted,
  or re-decoded once the device can? A version byte in the header would let the
  device re-bake stale threshold-only covers.
- **Do we ever want `<svg><image>` covers?** Not in this book, but common in the
  wild. It only matters for M2, and only as href resolution — the decode is
  identical.
- **Is a "rendering…" plate needed** if M2 decode exceeds ~2 s, or does the
  e-ink refresh already cover the latency perceptually?
