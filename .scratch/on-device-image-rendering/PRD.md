# On-Device EPUB Image Rendering (PNG, JPEG, Block & Inline)

Status: **Ready for implementation.** Supersedes the 2026-09-03 and 2026-07-26
hotspot-preprocessing / `/render-bundle` drafts.

Date: 2026-10-28, replacing the 2026-09-03 draft.

## What this revision changes

The 2026-09-03 draft was built around the premise that decoding EPUB images on
an ESP32-C3 inside 400 KB of SRAM would be slow, restricted to baseline JPEG,
and limited to full-page image-only spine items. From that premise followed a
dual-producer architecture: a pinned browser Wasm preprocessor, a
`/capabilities` handshake, a framed `POST /render-bundle` endpoint contending
for the Wi-Fi session heap, a six-axis policy-version matrix (`semantic`,
`producer_decoder`, `canonical_luma`, `geometry`, `resampler`, `dither`),
`RENDER.MF` manifests, `SourceDigest` epoch checks, idle move-relocation jobs
for host bundles, and a separate M3 FIFO page-slot cache (`PAGES/INDEX.BIN` and
`P00.BIN`..`P<N>.BIN`).

Upstream (`marigold-os` commits `92caf858`, `992cd481`, `a836150e`, `bdf11ce6`,
`9097f165`, and `f7e78141`) disproved that premise on hardware:

1. **Both PNG and JPEG decode fast on device with zero static RAM.**
   `proto::image` decodes every non-interlaced PNG (gray, RGB, palette,
   gray+alpha, RGBA at bit depths 1, 2, 4, 8, and 16, plus `tRNS`) and every
   baseline (`SOF0`) or extended-sequential (`SOF1`) Huffman JPEG (grayscale or
   YCbCr with 1x/2x subsampling and restart intervals) inside the existing
   borrowed 24 KB `EpubScratch` XHTML window when the parser is between spine
   items. On the ESP32-C3 at 160 MHz (80 MHz DIO flash), a 640×480 PNG diagram
   decodes and dithers in 374 ms; Gutenberg's illustrated *Alice* decodes its 55
   JPEGs in 60–623 ms each; and a 600 KB 1600 px JPEG takes 2.1 s stored and
   2.2 s deflated.
2. **Real books put images inside text chapters and inside lines.** Restricting
   support to image-only spine items leaves nearly all illustrations as `[Image]`
   placeholders. Upstream supports standalone block images anywhere in a chapter,
   `<svg><image>` wrappers, inline images inside running text lines (such as
   Byrne's *Euclid*, 639 mid-sentence symbols), and floated single-letter drop
   caps (`class="figleft"` / `float: left`).
3. **One append-only pack per orientation (`IMGL.BIN`, `IMGP.BIN`) plus a fixed
   index (`IMAGES.BIN`) replaces both `/render-bundle` and the M3 slot cache.**
   Opening a file in a FAT directory is a linear directory scan; one pack per
   orientation adds at most three files to a book's cache directory regardless
   of whether the book has 1 image or 639. Because every image raster is 100%
   derivable on device, sideloaded SD books and Wi-Fi uploads behave
   identically, and no host bundle protocol is needed.

Everything that existed only to coordinate the browser preprocessor and the
dual-producer manifest is retired here. This document specifies the port of
upstream's on-device image pipeline into CalendulaOS's multi-crate workspace and
its integration with `CONT.BIN`, `ContentAnchor`, and `carry_pagination_for_move`.

---

## Upstream commits to port

| Commit | Scope | Summary |
|---|---|---|
| `92caf858` | `proto::image` (`png`, `dither`, `fit`) | Streaming non-interlaced PNG decoder, incremental division-free box filter, and serpentine 1 bpp Floyd-Steinberg ditherer in borrowed scratch. |
| `992cd481` | `proto::image` (`index`, `pack`, `token`), `proto::epub`, `ui::reading` | `IMAGES.BIN` index (32 B records sorted by path hash), `SectionV2BlockRecord::image_height`, `<img src>` and `<svg><image href>` parsing, centered block image layout, and alt-text fallback outline box. |
| `a836150e` | `display::fb`, `storage`, `fw` | Background/on-demand image decoding, `IMGL.BIN` and `IMGP.BIN` orientation packs in panel-native row order, deflated PNG spool (`IMG.SPL`), `Framebuffer::copy_packed_row`, and full-refresh transitions onto/off image pages. |
| `bdf11ce6` | `proto::image::dither` | Near-white paper stretch (`PAPER_WHITE = 240`..`255` mapped to white) so off-white backgrounds in scanned plates and JPEGs do not pepper the page with stray dither dots. |
| `9097f165` | `proto`, `ui::reading`, `storage`, `fw` | Inline images inside text lines (`U+FFFC` 15-byte hex token, 64 px height cap, `0x8000` flag in `image_height`), float-left drop-cap promotion into the next body paragraph, and custom-font inline image parity. |
| `f7e78141` | `proto::image::jpeg`, `storage` | Streaming `SOF0`/`SOF1` JPEG decoder with luma-only reconstruction, 1/8, 1/4, 1/2, and 1/1 DCT-domain scaling + box filter, APPn skipping during probe, and direct deflated-JPEG streaming without a spool file. |

---

## Goals and non-goals

### Goals

- Decode and render non-interlaced PNGs (gray, RGB, palette, gray+alpha, RGBA;
  bit depths 1, 2, 4, 8, 16; `tRNS`) and sequential 8-bit Huffman JPEGs (`SOF0`
  and `SOF1`, grayscale or YCbCr with 1x/2x sampling factors, single-scan or
  multi-scan, restart intervals) on device.
- Support standalone block images (`<img>` and `<svg><image>`), inline images
  flowing inside text lines, and float-left single-letter drop caps.
- Composite transparency onto white paper (`255`) before luma conversion, stretch
  near-white (`>= 240`) to paper white, and dither with serpentine
  Floyd-Steinberg at build/decode time so stored 1 bpp rows never flicker under
  fast refresh.
- Keep static RAM growth at **+0 bytes** on both X3 and X4 by carving all decoder
  and dither workspaces from `EpubScratch` while the XHTML parser is between
  spine items.
- Keep stack growth within X3 and X4 budgets (deepest image decode path <= 11 KB,
  verified with `tools/check.sh stack-frames`).
- Integrate cleanly with CalendulaOS's layout-independent content cache
  (`CONT.BIN`), `ContentAnchor` reading positions (`POSA.BIN`/`POSB.BIN`), and
  move-recovery cache transfers (`carry_pagination_for_move`).
- Preserve identical performance, pagination, and cache reuse for books without
  images.

### Non-goals

- Hotspot/browser Wasm preprocessing, `/capabilities`, or `POST /render-bundle`.
- Interlaced (Adam7) PNG or progressive (`SOF2`), arithmetic-coded, lossless,
  12-bit, CMYK/YCCK, or raw-RGB JPEG (these probe as unsupported and keep their
  italic alt-text fallback).
- General SVG vector rasterization (only `<svg>` wrappers around a single bitmap
  `<image href=...>` / `xlink:href=...` are extracted).
- Multi-level (2 bpp / four-tone) grayscale panel rendering; see "Four-tone
  panel output" below for the standing gate.
- Library-view cover thumbnails.

---

## Architecture and crate mapping

CalendulaOS splits storage, caching, and pure protocol logic across workspace
crates (`proto`, `display`, `ui`, `app-core`, `reader-cache`, `storage`, `fw`).
Upstream's modules map into the workspace as follows:

| Upstream path | CalendulaOS crate & module | Responsibility |
|---|---|---|
| `proto/src/image/mod.rs` | `proto::image` | Shared `LumaSink`, `ImageFormat`, dimension/fitting helpers, and error types (`#![no_std]`, `forbid(unsafe_code)`). |
| `proto/src/image/dither.rs` | `proto::image::dither` | Serpentine Floyd-Steinberg ditherer (`Ditherer`) with near-white paper stretch (`PAPER_WHITE = 240`) and panel-native 1 bpp row packing. |
| `proto/src/image/fit.rs` | `proto::image::fit` | Aspect-preserving `fit_inside` downscaling (never upscales) for block page boxes and the 64 px inline cap. |
| `proto/src/image/png.rs` | `proto::image::png` | Streaming chunk-fed PNG probe and decoder over borrowed scratch + caller-lent `ZipInflateScratch`. |
| `proto/src/image/jpeg.rs`, `jpeg/idct.rs` | `proto::image::jpeg` | Streaming chunk-fed `SOF0`/`SOF1` JPEG probe and luma decoder with 1/8, 1/4, 1/2, and 1/1 integer IDCT scaling + box filter. |
| `proto/src/image/index.rs` | `proto::image::index` | `IMAGES.BIN` v2 header and 32-byte `ImageEntry` encoding/decoding, FNV-1a 64-bit path hashing, and binary-search lookup. |
| `proto/src/image/pack.rs` | `proto::image::pack` | `IMGL.BIN` and `IMGP.BIN` append-only pack record headers (`width: u16, height: u16` + packed 1 bpp rows). |
| `proto/src/image/token.rs` | `proto::image::token` | Block image token (`\x1bIMG:...`) and inline image token (`U+FFFC` + 12 hex digits for slot, width, height) encoding/parsing. |
| `proto/src/epub.rs` | `proto::epub` | `<img src>`, `<svg><image href>`, `<figure>`/`<figcaption>`, `float: left` / `figleft` drop-cap detection, and pending-inline-image staging in the idle tail of `container` scratch. |
| `display/src/fb.rs` | `display::fb` | `Framebuffer::copy_packed_row` for bit-shifted row blits in landscape and portrait. |
| `ui/src/reading.rs` | `ui::reading` | `PageImageSource` trait, block and inline image layout/measurement, `PageIndexCursor` parity, and fallback outline box + alt-text rendering. |
| `fw/src/reader_store.rs` | `reader-cache::store` | `ReaderBlock` `image_height: u16` (top bit `0x8000` for inline-image lines), `SectionV2BlockRecord` wire persistence, and `page_has_images` queries. |
| `fw/src/reader_cache_files.rs` | `reader-cache::files` | `IMAGES.BIN` / `IMGL.BIN` / `IMGP.BIN` file helpers and `carry_pagination_for_move` extension. |
| `fw/src/block_sink.rs`, `book_images.rs`, `image_decode.rs`, `page_images.rs` | `storage::block_sink`, `storage::book_images`, `storage::image_decode`, `storage::page_images` | SD-backed `IMAGES.BIN` probing, `CONT.BIN` + section layout integration, background/on-demand decode into `IMGL.BIN`/`IMGP.BIN`, and page-draw SD reader. |
| `fw/src/custom_font.rs` | `storage::custom_font` | Custom-font line renderer handling `U+FFFC` inline image tokens alongside custom-font glyphs. |
| `fw/src/tasks/display.rs` | `fw::tasks::display` | Idle image-decode slice scheduling, on-demand decode before painting an uncached image page, and `RefreshPlanner` clean refresh on/after image pages. |

---

## Detailed design

### 1. Image index (`IMAGES.BIN`)

When opening a book for a cold build, the ZIP central directory is already
streamed in a 512-byte window to populate `ZipIndex`. During that pass, entries
with `.png`, `.jpg`, or `.jpeg` extensions (case-insensitive) are recorded into
`IMAGES.BIN` in `/READER/CACHE2/E<hash>/`:

- **Header (16 bytes)**: magic `IMGI`, version `2`, `source_hash: u32`,
  `entry_count: u16`, reserved flags.
- **Entries (32 bytes each, up to `MAX_BOOK_IMAGES = 512`)**: sorted by 64-bit
  normalized path hash (`path_hash: u64`) for binary search on disk:
  - `path_hash: u64`
  - `local_header_offset: u32`
  - `compressed_size: u32`
  - `uncompressed_size: u32`
  - `width: u16`, `height: u16`
  - `landscape_offset: u32`, `portrait_offset: u32` (`0` = not decoded,
    `u32::MAX` = decode failed)
  - `compression_method: u8`, `format: u8` (`Unprobed`, `Png`, `Jpeg`,
    `Unsupported`), `flags: u8` (bit 0 = probed, bit 1 = sticky `inline`),
    `spine_hint: u8`

Building `IMAGES.BIN` from the central directory reads zero image headers up
front (probing all 639 images in *Euclid* at open time would add ~2 s before
page 1). Instead, entries start as `Unprobed`.

### 2. Probing and integration with `CONT.BIN` and `ContentAnchor`

Unlike upstream (which parses XHTML directly into `SectionV2BlockRecord`s),
CalendulaOS decouples EPUB extraction from layout via `CONT.BIN`
(`CONTENT_VERSION`) and persists reading positions as layout-independent
`ContentAnchor`s (`CONTENT_STREAM_VERSION`).

To keep `CONT.BIN` layout-independent so font, size, weight, spacing, and
orientation changes never re-parse the EPUB or re-probe ZIP entries:

1. **Lazy per-chapter probing during EPUB extraction**:
   - When the XHTML walk encounters `<img src>` or `<svg><image href>`, it
     resolves the relative path against the spine item's directory, hashes the
     normalized path, and binary-searches `IMAGES.BIN`.
   - If the chapter hits any `Unprobed` entries in `IMAGES.BIN`, the chapter
     notes their slots, finishes the current inflate pass, probes only those
     noted entries while `ZipInflateScratch` is idle (`<= 33` bytes for PNG;
     skipping `APPn` segments up to `SOF0`/`SOF1` bounded at 256 KB for JPEG,
     ~3 ms per image), writes the probed `(format, width, height)` back to
     `IMAGES.BIN`, and re-walks that spine item so its emitted blocks carry the
     real source dimensions.
2. **Representing images in `CONT.BIN`**:
   - **Block images**: A supported block image is emitted to `CONT.BIN` as a
     single block whose text is a `proto::image::token` block token carrying the
     `IMAGES.BIN` slot, source `(width, height)`, and sanitized alt text (for
     fallback rendering if decoding later fails). Unsupported or missing images
     emit their italic centered alt-text (or `[Image]`) block as today.
   - **Inline images and drop caps**: An `<img>` inside a paragraph that also
     contains non-whitespace text (or a floated single-letter initial promoted
     into the start of the following body paragraph) is emitted inline inside
     the paragraph's text stream as a `U+FFFC` token carrying the slot and
     source/inline dimensions, provided its fitted block height in both
     orientations is `<= 128` px (`2 * INLINE_IMAGE_MAX_HEIGHT`). Larger images
     in mixed paragraphs break the paragraph around themselves as standalone
     block images.
   - **Sanitization**: When a book has an image index, any literal `U+FFFC`
     present in the EPUB's raw XHTML text is stripped by the parser so book text
     can never spoof an inline image token.
3. **Layout replay from `CONT.BIN` (`SECTIONS/I<hex>.BIN` and `S<idx>_<hex>.BIN`)**:
   - When wrapping a `CONT.BIN` block stream for a specific `(TypeSettings,
     portrait)` layout:
     - A block-image token resolves its fitted height for the active
       orientation's `PageBox` (`fit_inside(src_w, src_h, page_box.width(),
       page_box.height())`) without touching the EPUB or `IMAGES.BIN`, and
       produces a `SectionV2BlockRecord` with `line_count = 1` and
       `image_height = fitted_h` (stored in the existing 2 padding/reserved
       bytes 10–11 of the 12-byte `SectionV2BlockRecord`).
     - An inline image token inside a paragraph is measured as an unbreakable
       unit of `fit_inside(src_w, src_h, min_line_width, 64)` width and height.
       Each wrapped output line that contains one or more inline image tokens
       sets bit 15 (`0x8000`) of `image_height` on its `SectionV2BlockRecord`,
       with the low 15 bits holding the tallest inline image's height on that
       line.
4. **Compatibility versions**:
   - Bump `CONTENT_VERSION` (`proto::cache`), `CONTENT_STREAM_VERSION`
     (`proto::anchor`), `CACHE_V2_VERSION` (`proto::cache`), and
     `READER_LAYOUT_VERSION` (`ui::reading`).
   - Because `CONT.BIN` block emission is deterministic across layouts,
     `ContentAnchor` byte offsets (`text.len() + 1` per emitted `CONT.BIN`
     block) remain invariant across font, size, spacing, and orientation
     changes. A reading position saved on or after an image page resolves to
     the exact image or paragraph after any relayout.

### 3. Streaming decoders and workspace budget (`proto::image`)

Both decoders operate under a strict sans-IO contract in `proto::image`:

- **Zero static RAM**: All working buffers are carved from a caller-supplied
  `&mut [u8]` slice (the 24 KB `EpubScratch` XHTML/arena buffer when the parser
  is idle) and zero-initialized before use.
- **PNG (`proto::image::png`)**:
  - Supports color types `0` (Gray), `2` (RGB), `3` (Palette), `4` (Gray+Alpha),
    and `6` (RGBA), bit depths `1`, `2`, `4`, `8`, and `16`, and `tRNS`
    transparency chunks. Rejects interlaced (Adam7) PNGs.
  - Composites alpha onto white (`255`) and converts RGB to BT.601 luma
    (`(77*R + 150*G + 29*B + 128) >> 8`).
  - Uses word-wise `Sub`/`Up` unfiltering, word-wise row shifting (avoiding the
    ESP32-C3 ROM `memmove` byte-by-byte backward copy penalty), and
    division-free incremental box filtering with precomputed reciprocals.
  - Up to `4096` px wide RGBA fits inside 20 KB of borrowed scratch.
- **JPEG (`proto::image::jpeg`)**:
  - Supports `SOF0` (baseline) and `SOF1` (extended sequential) 8-bit Huffman
    JPEGs: grayscale (1 component) or YCbCr (3 components) with any `1` or `2`
    horizontal/vertical sampling factors (`4:4:4`, `4:2:2`, `4:2:0`, `4:4:0`,
    etc.), single interleaved scan or separate component scans, and `DRI`/`RSTn`
    restart intervals.
  - Reconstructs only the `Y` (luma) component; `Cb` and `Cr` blocks are
    Huffman-decoded to advance the bitstream and discarded.
  - **DCT-domain scaling**: Selects the smallest of `1/8` (DC-only), `1/4`
    (2×2 IDCT), `1/2` (4×4 IDCT), or `1/1` (8×8 integer `islow` IDCT) whose
    scaled dimensions still cover the fitted output size, followed by the shared
    division-free box filter to the exact target dimensions.
  - Scratch footprint is `4,227 + (4 + mcu_luma_rows) * out_w` bytes, which
    depends on **output** width (`<= 784` px) rather than source width: worst
    case is 19,907 B at `out_w = 784`, and 10,499 B for a 4,000 px photo scaled
    at `1/4`.
  - Hostile-input bounds: rejects early `EOI`, >256 bytes of inter-segment junk,
    out-of-sequence or empty restart intervals, and deflated streams exceeding
    their declared size or the 4 MiB uncompressed cap.
- **Dithering (`proto::image::dither`)**:
  - Serpentine Floyd-Steinberg error diffusion over two `i16` error rows.
  - Stretches luma monotonically with `PAPER_WHITE = 240` (`>= 240` maps to
    `255` white with zero diffused error) so slightly off-white paper scans and
    JPEG ringing beside white backgrounds render cleanly without peppering dots.
  - Packs output bits in panel-native orientation order (landscape rows or
    transposed portrait columns) so page drawing requires only a bit-shifted row
    blit.

### 4. Orientation packs (`IMGL.BIN`, `IMGP.BIN`) and scheduling

- **Pack format**: One append-only pack per orientation (`IMGL.BIN` for
  landscape, `IMGP.BIN` for portrait) in `/READER/CACHE2/E<hash>/`. Each record
  is a 4-byte header (`width: u16, height: u16` in logical reading coordinates)
  followed by packed 1 bpp rows in panel-native order.
- **Stored vs. deflated ZIP entries**:
  - Stored PNG and stored JPEG entries stream directly from `File::read` chunks.
  - Deflated JPEG entries stream directly through `ZipInflateScratch` into the
    JPEG decoder in 1 KB chunks with no temporary file, because the JPEG
    decoder does not need the inflater internally.
  - Deflated PNG entries require two inflate layers (ZIP deflate + PNG `IDAT`
    zlib), so the ZIP entry is inflated once to a temporary `/READER/CACHE2/E<hash>/IMG.SPL`
    spool file (capped at 4 MiB), decoded from `IMG.SPL`, and `IMG.SPL` is
    removed via `remove_file_reclaiming_clusters`.
- **Cooperative scheduling and prefetch**:
  - During idle background work on `display_task`, image decoding is scheduled
    with priority given to the **prefetch window** (undecoded images on the next
    few pages after the current reading page, then the previous page), followed
    by text/layout build slices, followed by remaining book images in reading
    order.
  - The decoder checks for pending input/display commands every few dozen source
    rows; any button press aborts the in-progress decode immediately without
    publishing a pack offset in `IMAGES.BIN`.
  - If the reader turns or jumps directly onto a page with an undecoded image
    before background prefetch reaches it, `display_task` decodes that page's
    images before painting (checking for button presses so the reader can flip
    past an image page without waiting).
- **Inline + block size reconciliation**:
  - If the same image entry is used both as a block image and as an inline image
    (and its block height exceeds the 64 px inline cap), the sticky `inline`
    flag (bit 1 of `ImageEntry::flags`) marks it to decode at its inline size
    once per orientation; block uses center the smaller bitmap inside their
    block box so the pack never thrashes between two sizes.

### 5. Page rendering and panel refresh

- **Drawing**:
  - When a `Reading` page contains any block or inline image
    (`sd_library.page_has_images(page)`), `display_task` opens an SD session
    (matching the custom-font reading path) and passes a `PageImageSource`
    backed by `IMAGES.BIN` and `IMGL.BIN`/`IMGP.BIN` into `ui::reading` (or
    `storage::custom_font`).
  - Each image reads its packed rows into the idle `EpubScratch` window and
    blits into the `Framebuffer` via `fb.copy_packed_row`.
  - If an image failed to decode or cannot be read, `ui::reading` draws a 1 px
    outlined box with its centered italic alt text (or inline italic alt text
    for unshowable inline images).
- **Refresh mode**:
  - Turning onto a page with an image, turning from one image page to another,
    and turning off an image page onto a text page use `RefreshMode::Full` (or
    `needs_clean`) so 1 bpp dither patterns neither ghost onto subsequent text
    pages nor inherit text ghosting.
  - On X4 with the warm fast LUT (`panel_temp`), image-page transitions keep the
    full OTP refresh rather than the 300 ms text LUT.

### 6. Cache lifecycle and move recovery (`carry_pagination_for_move`)

- **Cache directory scope**: `IMAGES.BIN`, `IMGL.BIN`, and `IMGP.BIN` live in
  `/READER/CACHE2/E<hash>/` under the copy's `LocalCacheIdentity`. Orphan sweep
  and managed replacement (`REPLACE.JNL`) automatically reclaim them with the
  rest of the cache directory via `remove_dir_reclaiming_clusters`.
- **Carrying across a proven folder move**:
  - In `reader_cache::files::carry_pagination_for_move` (Reading Position M6),
    include `IMAGES.BIN`, `IMGL.BIN`, and `IMGP.BIN` in the directory-entry move
    list (`move_file_in_dir_lfn` before `BOOK.BIN`), and update `IMAGES.BIN`'s
    header `source_hash` in place under the destination key alongside `CONT.BIN`,
    `TOC.BIN`, and `BOOK.BIN`. Any leftover `IMG.SPL` scratch file in the donor
    directory is left behind for the orphan sweep to reclaim.

---

## Four-tone panel output

Not in any milestone. Recorded 2026-09-24 because both panels can show four
tones, upstream now has a driver path for it that the sweep declined, and the
roadmap's standing "no grayscale" rejection rests partly on a RAM argument that
streaming strip uploads no longer need. The block that remains is a consumer
that earns the refresh cost.

### What FreeInk / CrossPoint shipped

FreeInk `3080ca2`, `a3db714`, `3c6e110`, `4a69a29` and `5916724` (2026-09-09
to 09-14), pulled into CrossPoint through the submodule, where it drives sleep
images, EPUB covers and BMP viewing. Text anti-aliasing stays on their older
overlay pipeline and is out of scope here entirely.

- **Absolute mode.** The host supplies a complete four-tone image as two
  planes, LSB then MSB: black `(0,0)`, dark `(1,0)`, light `(0,1)`, white
  `(1,1)`. Every pixel is present, including background. No black-and-white
  base is painted first.
- **SSD1677, our X4: one activation.** Factory selector `0xCC`, factory LUT
  bytes retained, and the previous image stays on the glass until both planes
  are in. The power-down is a separate step when requested. The "single
  pass" in the CrossPoint discussion means this activation. It is a
  controller property, not an MCU one, and the C3 X4 is the primary target.
- **UC8253, our X3: two shapes.** Absolute mode with a separate
  black-and-white conditioning pass, and a "Direct" combined-activation mode
  behind an opt-in driver config. The four-tone bank is the stock XTH4 set.
  FreeInk's earlier note (`cd2bcc5`) says the bank needs a dedicated
  grayscale panel init with different rails and VCOM from the black-and-white
  init (PSR `3F 4A`, PWR `43 00 78 78 17`, VCOM `0x26`), so on the X3 this is
  a driver init change, not a LUT upload.
- **Strip uploads on both.** Full-width rows from row zero, LSB first, strips
  may then interleave planes. The host streams from its decoder and holds no
  second full framebuffer.
- **Recovery is the driver's.** The next black-and-white refresh after an
  absolute pass, or after a cancelled one, is forced clean. Any normal paint,
  inversion change, or sleep cancels an incomplete pass.
- **Validation status.** Host tests and X4 Pro builds. The commit messages say
  physical panel validation remains, and the SSD1677 power-down split was
  committed with builds and tests not run. Treat the waveform recipes as
  upstream's reading of stock firmware, not as bench results.
- **UC8279 X3.** Its four-tone bank is XTH4 as well, with a register exchange
  fix in `357b806`. The `uc8279-x3-driver` PRD is black-and-white only, and
  nothing here changes until that driver exists.

### Gate to reopen

Two things, both required: a consumer judged unacceptable at one-bit serpentine
Floyd-Steinberg dither on an X3 (looked at on the glass, not in the emulator),
and a physical validation report for the UC8253 four-tone path from FreeInk or
CrossPoint. Until both exist the non-goal stands, and the roadmap's rejection
should cite this section rather than RAM.

---

## Implementation phases and verification

### Phase 1 — `proto::image` decoders, ditherer, index, and pack formats
- Port `proto::image` (`dither`, `fit`, `png`, `jpeg`, `jpeg::idct`, `index`,
  `pack`, `token`) from upstream `92caf858`, `992cd481`, `bdf11ce6`, `9097f165`,
  and `f7e78141`.
- Run all unit, reference-comparison (`png` crate and `zune-jpeg` / float-IDCT),
  1-byte/random chunking, dirty-scratch (`0xA5`), and 40,000-file mutation tests
  on the host target (`tools/check.sh fast`).

### Phase 2 — `CONT.BIN`, `proto::epub`, `ui::reading`, and `reader-cache`
- Port `<img src>`, `<svg><image href>`, `<figure>`/`<figcaption>`, and
  float-left drop-cap parsing in `proto::epub`.
- Wire image tokens through `CONT.BIN` (`CONTENT_VERSION` and
  `CONTENT_STREAM_VERSION` bump) and section layout (`CACHE_V2_VERSION` and
  `READER_LAYOUT_VERSION` bump).
- Port `Framebuffer::copy_packed_row` (`display::fb`), `PageImageSource`, block
  and inline image layout/rendering in `ui::reading`, and `SectionV2BlockRecord`
  `image_height` persistence in `reader-cache`.
- Verify `ContentAnchor` round-trip resolution across font, size, spacing, and
  orientation changes on books containing block and inline images.
- Update and verify emulator golden frames on both X4 and X3
  (`tools/check.sh emulator`).

### Phase 3 — `storage` & `fw` pipeline, custom-font parity, and move carry
- Port `storage::book_images`, `storage::image_decode`, `storage::page_images`,
  and custom-font inline-image rendering in `storage::custom_font`.
- Wire prefetch/on-demand image decoding and full-refresh image transitions in
  `fw::tasks::display`.
- Extend `reader_cache::files::carry_pagination_for_move` to carry and re-stamp
  `IMAGES.BIN`, `IMGL.BIN`, and `IMGP.BIN`.
- Run `tools/check.sh all` (including `firmware` and `stack-frames` on both X4
  and X3) and verify static RAM (`+0 B`) and stack headroom bounds.
