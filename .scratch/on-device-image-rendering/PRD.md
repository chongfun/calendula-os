# On-Device Image Rendering — PRD

Status: **Planning. Product behavior and cache architecture are closed except where explicitly assigned to an M0 decision gate. Decoder implementation is intentionally not selected until M0A completes.**

Drafted 2026-07-25 from:

- the current CalendulaOS reader, EPUB, cache, framebuffer, and host-preview architecture;
- measurements from the X4 release image;
- the target EPUB corpus;
- research into `crosspoint-reader`;
- evaluation of JPEGDEC, TJpgDec, `tjpgdec-rs`, and `zune-jpeg`;
- review of the available testing and fuzzing infrastructure for those decoders.

Numbers marked **CalendulaOS measured** come from this repository at `c8439e8`.

Numbers marked **sibling-reported** come from comments or measurements recorded by another firmware and are not reproducible CalendulaOS benchmark results.

Numbers marked **estimate** have not been verified on CalendulaOS hardware and must be replaced during M0 or the relevant firmware milestone.

## Summary

CalendulaOS can display a pre-rasterized 1bpp cover, but it cannot decode an EPUB image on the device.

Today, `tools/preview` uses the host `image` crate to decode a cover, crop it to 202×303, threshold it to 1bpp, and write `COVER.BIN` to the SD card. Books that do not go through that manual step have no cover. EPUB illustrations are never rendered: the XHTML parser reduces `<img>` elements to text placeholders, and the reader never opens the referenced image bytes.

This project adds a `no_std`, allocation-free JPEG raster pipeline in `proto`, staged so the smallest useful result lands first:

- **M0A — decoder adoption spike:** determine whether `tjpgdec-rs` can be safely adapted to CalendulaOS’s forward-only, luma-only, caller-buffered architecture.
- **M0B — decoder verification and raster pipeline:** complete the selected baseline decoder, differential harness, resampling, and dithering on the host.
- **M1 — on-device covers:** generate and cache a book cover on any book-open path when no usable cover exists.
- **M2 — dedicated image pages:** represent image-only spine items in the content and page caches, then decode them into the framebuffer while reading.
- **M3 candidate — progressive thumbnails:** consider a bounded DC-only progressive JPEG mode only after baseline JPEG has shipped and corpus evidence justifies the extra decoder surface.

M2 initially supports only EPUB spine items whose rendered body consists of one image. Images embedded among prose remain placeholders. Inline image layout and heuristic promotion of decorative images are explicitly out of scope.

Stopping after M1 is an acceptable outcome if hardware testing shows that full-page rendering is too slow, consumes too much scratch, or produces visually poor 1bpp output.

The project does **not** begin by porting all of JPEGDEC or by writing a decoder from scratch. M0A evaluates the closest native Rust implementation first. Any adopted, adapted, or ported decoder must pass the same independent verification gate.

## Background

### Existing cover destination

The destination half of cover rendering is already implemented:

| Piece | Location |
| --- | --- |
| Cover cache format `X4CV`, 202×303 at 1bpp | `proto/src/cache.rs` |
| Cover cache reader and validation | `fw/src/reader_cache_files.rs` |
| Resident cover buffer | `fw/src/reader_store.rs` |
| UI model `UiCover` | `ui/src/lib.rs` |
| Home and library view wiring | `fw/src/views.rs` |
| Host-side decode and rasterization | `tools/preview/src/main.rs` |

The existing `COVER.BIN` payload is 7,878 bytes. The firmware can load and display it, but nothing on the device creates it.

`tools/preview` currently:

1. finds the cover manifest item;
2. reads the image from the EPUB ZIP;
3. decodes it through the host `image` crate;
4. uses centered fill-and-crop scaling to 202×303;
5. applies a hard luma threshold;
6. writes `COVER.BIN`.

### Existing reading path

The reading path has no persistent image concept.

The XHTML parser emits body text through `XhtmlBlockSink::push_block`. `<img>` is represented as placeholder text. `CONT.BIN` captures that text-block stream so a type-settings change can repaginate without reopening the EPUB.

Page-cache records identify only a range of text blocks. They cannot identify an image page or carry an EPUB image href.

M2 therefore requires coordinated changes to:

- the XHTML event interface;
- `CONT.BIN`;
- section/page cache records;
- cache-version invalidation;
- the reading render path.

Adding only an image marker to the parser is insufficient because that marker must survive content-cache replay and page-cache loading.

## User-visible behavior

### Covers

After M1:

- Opening a book with no usable cached cover causes the device to discover, decode, rasterize, and cache the cover.
- Cover generation runs regardless of whether the book text came from:
  - a valid `BOOK.BIN`;
  - `CONT.BIN` replay; or
  - a full EPUB parse and cache build.
- The generated cover becomes available to the existing home and library UI after the first successful book open.
- A valid existing host-generated `COVER.BIN` remains usable and is not regenerated merely because the device decoder exists.
- A missing, corrupt, unsupported, or malformed cover never prevents the book from opening.
- A deterministic unsupported-cover result is not retried on every open.
- A transient I/O or publication failure remains retryable.

Generating covers for every library entry before that book has been opened is out of scope.

### Image pages

After M2:

- A dedicated image-only EPUB spine item appears as one reader page.
- The image is scaled to fit inside the active reader content viewport while preserving its aspect ratio.
- Unused viewport area is white.
- A successfully rendered image page always requests a Full e-ink refresh.
- Unsupported, missing, or malformed images display the existing image placeholder instead of aborting the book open or page render.
- Images that coexist with prose, captions, or additional images remain placeholders.
- Revisiting an image that deterministically failed during the current reading session does not repeatedly perform the full decode.
- A transient source or SD failure may be retried later.

## Goals

- Generate a book’s cover on-device without a host preprocessing step.
- Preserve generated covers across subsequent opens.
- Render dedicated full-page illustrations in the reading flow.
- Keep decoder, parser, scaling, and dither logic host-testable.
- Remain `no_std`, allocation-free, and panic-free for external input.
- Use caller-owned, bounded workspace.
- Read JPEG bytes directly from the existing forward ZIP stream.
- Avoid extracting image entries to temporary plain files merely to satisfy a decoder interface.
- Stay within existing RAM, stack, and static-memory budgets.
- Preserve exact semantic content-cache replay behavior.
- Make malformed, oversized, unsupported, and truncated images fail locally rather than failing the enclosing book operation.
- Produce deterministic host and firmware output from the same decoder, resampler, and dither implementation.
- Establish a verification harness strong enough that an agent-assisted decoder adaptation or port can be reviewed as a behavioral change rather than trusted as a textual translation.
- Record exact upstream revisions and source provenance for any adopted or derived decoder code.

## Non-goals

- Inline images participating in text layout.
- Floating images, CSS sizing, text wrapping around images, or image captions.
- Heuristic promotion of an image embedded in an otherwise textual spine item.
- Rendering arbitrary SVG.
- PNG decoding.
- Progressive JPEG in the initial shipping decoder.
- Arithmetic-coded JPEG.
- Lossless JPEG.
- 12-bit JPEG samples.
- CMYK or YCCK output.
- Arbitrary EXIF rotation or mirroring.
- Grayscale e-ink waveforms or multi-pass panel grayscale.
- Changes to Wi-Fi upload, synchronization, or book-transfer behavior.
- Prefetching or background decoding.
- Maintaining compatibility with old M2 page caches after the page-record schema changes.
- Matching JPEGDEC’s complete API or every output pixel format.
- A line-by-line translation of an upstream decoder without an independent behavioral specification.
- Treating any single decoder implementation as the sole correctness oracle.

## Evidence

### EPUB corpus — CalendulaOS measured

`EIGHTY-SIX-VOLUME-1.epub` contains 20 image files:

- all are JPEG;
- all use SOF0 rather than progressive SOF2;
- typical illustrations are 1200×1800;
- the largest image file is the 1,170,441-byte cover;
- all image entries are DEFLATE-compressed inside the ZIP;
- total image payload is approximately 8.7 MB.

This corpus contains no `<svg>` or SVG `<image>` elements. Its image references are ordinary XHTML `<img>` elements.

This corpus is the initial performance and visual-quality corpus. It is not a JPEG conformance corpus.

### DRAM — CalendulaOS measured

The X4 release ELF currently occupies the available DRAM:

| Region | Bytes |
| --- | ---: |
| Statics: `.data`, `.bss`, and rwdata shadow | 294,864 |
| Main stack | 43,944 |
| Previous framebuffer slot | 48,001 |
| Unallocated DRAM | Approximately 0 |

The decoder must not add a large static.

The display task already owns large EPUB scratch buffers:

| Scratch | Bytes | Required during |
| --- | ---: | --- |
| ZIP inflate state and window | 43,316 | Any compressed ZIP-entry read |
| XHTML buffer | 24,576 | XHTML parsing |
| OPF buffer | 16,385 | Package parsing |
| Compressed-input buffer | 8,192 | ZIP reads |

The ZIP inflate scratch is required concurrently with JPEG decoding because image entries may themselves be DEFLATE-compressed.

The XHTML and OPF buffers may be reused only after all values borrowing from them have been dropped. The implementation must make this lifetime transition explicit rather than relying on comments or informal sequencing.

### Refresh and latency

Current CalendulaOS behavior:

- Full refresh busy time: approximately 3,000–4,300 ms.
- Fast refresh: approximately 500 ms.
- Median fast press-to-settled time: approximately 550 ms.
- Warm book open: approximately 150 ms.

Full-page art requires a Full refresh because partial refreshes ghost badly on high-contrast images.

The earlier 2–5 second decode estimate is not an acceptance result. M0 and M1 must replace it with measured data.

### Sibling-firmware data point

`crosspoint-reader` runs EPUB image decoding on the same SoC and X3/X4 panel family. Source comments reviewed during research report approximately two seconds for a full-page decode and approximately 55 KB of free heap during an image page. [crosspoint-reader]

These are **sibling-reported** figures, not CalendulaOS measurements. The observation is useful only as evidence that the order of magnitude may be viable. The sibling pipeline differs materially:

- its image may already have been extracted to a seekable SD file;
- it uses JPEGDEC;
- it renders 2bpp output;
- its display path is multi-pass;
- it persists rendered sidecar artifacts.

The implementation issue that relies on this research must record the exact evaluated `crosspoint-reader` commit and the source locations for each claim. The PRD must not use an unpinned moving branch as a permanent technical reference.

### Flash — CalendulaOS measured

The application partition is approximately 6.5 MB and the current firmware image is approximately 3.7 MB.

Decoder flash growth is not expected to be the primary constraint, but M0 and firmware milestones must report:

- `proto` code-size delta;
- firmware image-size delta;
- any native or FFI object contribution;
- any table or lookup data added to read-only memory.

## Prior art: `crosspoint-reader`

`crosspoint-reader` is a sibling C++/Arduino firmware for the same hardware family that already renders EPUB images. The research is useful for identifying practical constraints, but its pipeline is not copied wholesale.

Before implementation begins, the M0 issue must pin:

- the exact `crosspoint-reader` revision;
- the relevant cover-conversion files;
- the relevant EPUB page-rendering files;
- the image-extraction code;
- the panel-render pass structure;
- the comments or benchmark output behind reported latency and heap figures.

Observations must distinguish source code, source comments, issue discussions, and locally reproduced measurements.

### Adopted lesson: streaming dimension probe

The sibling firmware obtains image dimensions by feeding a bounded prefix through a small state machine that recognizes JPEG SOF or PNG IHDR metadata and stops once the dimensions are known.

M2 adopts the architectural idea:

- no allocation;
- no seek;
- no whole-image inflate during cache construction;
- bounded bytes consumed;
- explicit unsupported, incomplete, and malformed results.

The C++ source is not copied.

### Adopted lesson: separate metadata and raster operations

M2 needs image format and dimensions during cache construction, but it does not need pixel decoding until the page is rendered.

The PRD therefore separates:

- `ImageProbe`, which consumes a bounded stream prefix;
- `JpegDecoder`, which consumes the complete encoded stream;
- `ImagePageRecord`, which persists normalized metadata;
- the render-time pipeline, which reopens and decodes the entry.

### Adopted lesson: per-axis scale factors

Horizontal and vertical destination ratios are computed independently.

A fitted destination dimension is rounded to an integer. Applying one rounded scale to both axes can select the wrong final source row or column and can lose image content.

The common resampler therefore computes:

- source-to-destination horizontal ratio;
- source-to-destination vertical ratio;
- destination-to-source horizontal ratio;
- destination-to-source vertical ratio.

### Adopted lesson: bounded failure suppression

A deterministic malformed or unsupported image should not perform the complete failed decode every time the user revisits the page in one reading session.

M2 adopts a bounded in-RAM memo, but not a path-hash-only table. It uses a collision-safe stable image identity and records only deterministic failures.

### Considered lesson: rendered-output cache

A persisted rendered raster can make revisits inexpensive. It also adds format versioning, source invalidation, SD lifecycle, publication, and storage accounting.

This is deferred until native single-pass decode latency is measured.

### Rejected as the default: extracted-image files

JPEGDEC’s SD-file interface expects open, read, close, and seek callbacks. A forward-only DEFLATE `ZipStream` cannot satisfy arbitrary backward seeking directly. Direct JPEGDEC integration would therefore require one or more of:

- seek emulation by restarting inflation;
- staging the complete compressed JPEG in memory;
- staging the complete JPEG in a plain SD file;
- a new JPEGDEC input path;
- a different decoder.

Extraction is not logically forced by the JPEG format, and JPEGDEC can also read complete memory-backed images. It is the sibling firmware’s chosen adapter between a seek-oriented decoder API and its EPUB storage path. JPEGDEC documents its file callbacks and built-in reduced decode modes, progressive DC-only thumbnails, and low-bit-depth dithering. [JPEGDEC]

CalendulaOS rejects mandatory extraction because it would add:

- an extra full-image SD write before every first decode;
- temporary-file lifecycle;
- power-loss cleanup;
- storage accounting;
- additional wear;
- stale-source invalidation;
- higher implementation complexity.

### Rejected as a direct dependency: JPEGDEC

JPEGDEC is a C++ API around a portable native decoding core. It is designed for microcontrollers, supports reduced decode at 1/2, 1/4, and 1/8, baseline grayscale and YCbCr, progressive DC-only thumbnails, cropping, callbacks, and optional Floyd–Steinberg dithering to 1, 2, or 4-bpp grayscale output. [JPEGDEC]

Its 1-bpp dithered output is the same format this project targets, which is why it is worth citing as prior art even though it is not adopted.

It is not selected as the initial firmware dependency because:

- integration introduces an FFI boundary;
- the firmware would need narrowly audited `unsafe` declarations and wrappers;
- the file-oriented input path does not naturally match the forward-only ZIP stream;
- a full JPEGDEC integration exposes more formats, modes, and state than M1 requires;
- the sibling project has had to maintain local fixes in the progressive path;
- repository testing consists primarily of examples, performance programs, and sample images rather than a comprehensive executable conformance suite.

JPEGDEC’s root Makefile builds a demonstration executable, and its ESP-IDF CMake file registers the component rather than defining a decoder test suite. Its repository does contain test images and platform examples, which are useful corpus inputs but are not by themselves a behavioral specification. [JPEGDEC]

The decision is a repository policy choice, not a proven claim that a Rust decoder will require less total engineering effort:

> CalendulaOS accepts a potentially larger initial implementation cost because forward-only streaming, caller-owned workspace, Rust auditability, deterministic output, and a narrow supported contract are higher-priority constraints than minimizing initial decoder-development effort.

### Rejected: nearest-neighbor downscaling

Nearest-neighbor downscaling discards most input samples at the reductions expected for the target corpus and can alias badly on line art and typography.

The initial CalendulaOS policy is area or box averaging for shrink operations.

A faster policy may replace it only at the M0 visual and performance gate.

### Qualified lesson: progressive JPEG thumbnails

JPEGDEC demonstrates that DC-only progressive thumbnail decoding is practical and exposes it as a feature. [JPEGDEC]

This does **not** establish that CalendulaOS can decode every progressive JPEG by reading only the literal first scan, that every component is present in that scan, or that no coefficient or scan-state retention is required.

Progressive support remains out of the initial contract. A later spike must determine:

- supported progressive scan organizations;
- whether all required DC data can be consumed forward-only;
- retained coefficient or predictor state;
- restart handling;
- successive-approximation handling;
- behavior when components appear in separate scans;
- exact failure behavior for unsupported organizations;
- visual quality of forced 1/8 output.

Until then, progressive images render the placeholder.

## Decoder sourcing strategy

### Decision hierarchy

M0 evaluates decoder sources in this order:

1. **Adapt `tjpgdec-rs` for CalendulaOS.**
2. **Implement a constrained Rust baseline decoder using the verification harness**, reusing only appropriately licensed algorithms or independently derived behavior.
3. **Port narrowly selected JPEGDEC algorithms** only when a specific required capability is absent from the first two paths.
4. **Full JPEGDEC port** only if measurements show that its unique features are necessary and the project accepts the substantially larger verification surface.

No implementation path bypasses the M0 verification requirements.

### Candidate A: `tjpgdec-rs`

`tjpgdec-rs` 0.4.0 is a native Rust port of ChaN’s TJpgDec intended for embedded systems. It declares `no_std` support, uses a caller-provided memory pool, and supports three optimization levels with workspaces of 3,100, 3,500, and 9,644 bytes. Version 0.4.0 is the only published release. Its crate metadata declares `MIT OR Apache-2.0`. [tjpgdec-rs]

The repository README describes the Rust implementation as MIT while the published crate metadata declares the dual licence. Any adoption must resolve that discrepancy against the LICENSE files present at the pinned commit before code is vendored.

Its architecture is close to M1, but it is not a drop-in dependency:

- `prepare` accepts the complete JPEG as `&[u8]`;
- `decompress` again accepts the complete JPEG as `&[u8]`;
- it records the SOS location and later slices the source to find entropy data;
- output callbacks receive RGB888 MCU data;
- its workspace stores tables through raw pointers;
- the implementation contains `unsafe`: its linear allocator hands out slices built with `core::slice::from_raw_parts_mut`, justified by an inline comment rather than by a checked invariant;
- its only in-crate unit test asserts that a buffer-size constant equals 512. [tjpgdec-rs]

M0A must determine whether the adaptation can:

- replace complete-slice input with a forward-only byte source;
- parse headers and continue directly into entropy data without rewind;
- emit scaled luma rather than RGB888;
- remove unnecessary color conversion and RGB work buffers;
- preserve restart-marker behavior;
- support the target baseline sampling layouts;
- eliminate raw-pointer workspace storage, or reduce retained `unsafe` to a separately reviewed and justified module;
- pass the project’s malformed-input and differential suite;
- remain smaller than a direct constrained implementation.

The project must pin the exact upstream commit used for evaluation.

Adoption is allowed only as a maintained fork or vendored adaptation with:

- license notices;
- upstream provenance;
- a documented patch set;
- exact behavioral differences;
- update policy;
- reproducible verification.

### Candidate B: `zune-jpeg`

`zune-jpeg` is a pure Rust decoder with tests and fuzz targets in its repository. Its documentation states that it works in no-std, but its decode call returns an owned buffer holding the whole image, so it requires an allocator and does not satisfy the caller-owned streaming workspace model without a substantial fork. [zune-jpeg]

It is therefore not the initial firmware decoder.

It is useful as:

- an independent host oracle;
- a source of regression JPEGs;
- a source of malformed inputs discovered by fuzzing;
- a reference for JPEG marker, scan, and color-space handling;
- a comparison implementation for metadata and decoded luma.

Because JPEG leaves some reconstruction and color-conversion details implementation-defined, `zune-jpeg` output is not required to be bit-identical to every other decoder. Comparisons use explicit tolerances where exact equality is inappropriate.

### Candidate C: TJpgDec C reference

ChaN’s TJpgDec is the upstream algorithm behind `tjpgdec-rs`. It is designed for low-memory embedded decode and documents a 3.5 KB work area independent of image width, with 3.5–8.5 KB of code. Upstream terms are permissive: use, modification, and redistribution are allowed without restriction, at the user’s own responsibility. [TJpgDec]

The `cmumford/TJpgDec` fork adds a libFuzzer target, sanitizer support, fixes for the memory-access errors that fuzzing found, a CMake build, and GitHub Actions CI, while keeping the pristine upstream source on a separate branch. That fork is useful as a reference executable and fuzz-corpus source. Its repository is GPL-3.0, so its modified source must not be copied into CalendulaOS unless the project deliberately accepts that license. It may be used externally as a test oracle. [TJpgDec-fork]

Note that the fork's GPL-3.0 terms differ from the permissive upstream terms above. Provenance must therefore be tracked per file, not per algorithm.

### Candidate D: JPEGDEC algorithm port

JPEGDEC may be used as a source for a narrowly scoped later algorithm, especially:

- progressive DC-only thumbnail handling;
- optimized scaled IDCT ideas;
- low-bit-depth output;
- crop traversal.

A source-derived Rust port must:

- pin the exact JPEGDEC commit;
- retain Apache-2.0 notices;
- record source-function provenance;
- list intentional behavioral differences;
- receive independent tests rather than using only JPEGDEC output as truth.

A complete agentic translation is not accepted merely because it compiles or matches a few sample images.

## Architecture

### Shared pipeline

```text
EPUB file
  -> ZIP entry lookup
  -> ZipStream and existing DEFLATE inflate
  -> bounded image metadata probe
  -> selected baseline JPEG decoder
  -> scaled luma bands
  -> deterministic geometry and resampler
  -> deterministic 1bpp dither
  -> COVER.BIN       (M1)
     or framebuffer  (M2)
```

Every stage is bounded and streaming.

No stage may allocate or retain:

- a complete encoded JPEG;
- a complete decoded luma image;
- a complete RGB image;
- a second framebuffer.

### Module placement

Pure image logic lives in `proto` or a dedicated workspace crate that follows the same safety requirements:

- image metadata probing;
- JPEG header and marker parsing;
- entropy decoding;
- dequantization and scaled IDCT;
- luma-band output;
- crop/fit geometry;
- resampling;
- dithering;
- bounded metadata types;
- error classification.

Firmware owns:

- SD and ZIP-entry access;
- scratch-buffer lending;
- cache-file publication;
- framebuffer writes;
- refresh-policy selection;
- telemetry.

Host tools use the same decoder, resampler, geometry, and dither implementation as firmware.

### Input abstraction

The decoder consumes bytes through a forward-only sans-I/O interface supplied by the caller.

It must not depend on:

- `std`;
- filesystem types;
- `embedded_sdmmc`;
- EPUB-specific structures;
- `Seek`;
- a complete `&[u8]`.

Conceptually:

```rust
trait ByteSource {
    type Error;

    fn read(&mut self, output: &mut [u8]) -> Result<usize, Self::Error>;
}
```

The final interface may use a pull or push model, but it must guarantee:

- no request for an earlier input position;
- deterministic behavior for every legal chunk size;
- clean distinction between EOF and I/O failure;
- bounded buffering;
- no hidden allocation.

The caller provides:

- encoded bytes incrementally;
- caller-owned decoder workspace;
- output-band storage;
- a callback or sink for completed luma rows or bands.

The decoder returns bounded error values and never panics for malformed input.

### Decoder traversal

The initial decoder processes a baseline JPEG in one forward traversal:

1. validate SOI;
2. consume metadata and table markers;
3. validate SOF0;
4. validate SOS and scan organization;
5. enter entropy decode without rewinding;
6. emit scaled luma in deterministic image order;
7. validate required restart and end conditions;
8. finish at EOI or an explicitly accepted terminal condition.

Header parsing and entropy decoding are states of one decoder instance. M0A must not preserve `tjpgdec-rs`’s current prepare-then-reslice design if doing so requires the complete source.

### Output abstraction

The decoder’s primary output is scaled luma, not RGB.

The output sink receives:

- source-space or scaled-space rectangle;
- row count;
- packed or unpacked luma values;
- final-row indication where useful.

YCbCr chroma may be skipped entirely when only luma is required, provided that the JPEG’s component metadata has been validated as a supported YCbCr organization.

Grayscale JPEG emits its sole component directly.

## Initial JPEG contract

The first shipping decoder supports only:

- SOF0 baseline sequential DCT;
- 8-bit samples;
- Huffman entropy coding;
- one-component grayscale;
- supported three-component YCbCr;
- common 4:4:4, 4:2:2, and 4:2:0 sampling;
- one interleaved scan for a three-component image;
- one scan for a grayscale image;
- restart markers and restart intervals;
- 8-bit quantization tables;
- 16-bit quantization tables only if M0 verifies them within the workspace and arithmetic limits;
- absent EXIF orientation or orientation value 1.

The decoder refuses:

- SOF1 extended sequential JPEG;
- SOF2 progressive JPEG;
- baseline multi-scan or non-interleaved component scans;
- arithmetic coding;
- lossless JPEG;
- 12-bit samples;
- CMYK and YCCK;
- unsupported component layouts or transforms;
- mirrored or rotated EXIF orientations;
- malformed, truncated, or internally inconsistent files.

“Refuse” means return an explicit unsupported or invalid-image result.

Refusal must not:

- panic;
- loop indefinitely;
- read beyond supplied bytes;
- write beyond output buffers;
- corrupt retained state;
- fail the enclosing book operation.

The implementation must not infer YCbCr solely from a three-component count. M0 must define and test the exact accepted component identifiers, ordering, sampling constraints, and metadata rules.

Support may be widened after M0, but the implementing issue and acceptance suite must describe the actual supported subset precisely.

## Resource and abuse limits

The parser rejects an image before expensive decode work when a configured limit is exceeded.

Initial limits:

- Maximum JPEG width: 8,192 pixels.
- Maximum JPEG height: 8,192 pixels.
- Maximum decoded pixel count: 32,000,000 pixels.
- Maximum uncompressed JPEG entry length: 16 MiB.
- Maximum normalized EPUB image path: 256 UTF-8 bytes.
- Maximum component count: the initial contract’s supported count.
- Maximum table IDs and table counts: the initial contract’s bounded arrays.
- Maximum marker length: the JPEG segment’s validated 16-bit length.
- Maximum scratch use: supplied buffer length.
- Maximum output dimensions: active cover or reader geometry.

All calculations involving:

- dimensions;
- MCU counts;
- block counts;
- row strides;
- segment positions;
- table lengths;
- sampling factors;
- restart intervals;
- output offsets

must use checked arithmetic before conversion or indexing.

Path resolution must:

- remove URL fragments before ZIP lookup;
- resolve relative to the containing OPF or XHTML path;
- reject absolute paths;
- reject normalized paths that escape the EPUB root through `..`;
- reject overlength output;
- avoid lossy UTF-8 conversion.

These limits are correctness and denial-of-service controls, not claims about the JPEG or EPUB specifications’ theoretical maxima.

## Scaling and geometry

### DCT scale selection

Supported decoder reductions are 1/1, 1/2, 1/4, and 1/8.

The decoder chooses the largest reduction whose decoded output remains large enough for the required crop or fit operation in both dimensions.

A scale that undershoots the target is not chosen merely because it is faster.

For a typical 1200×1800 image targeting 202×303:

- 1/8 produces 150×225 and undershoots;
- 1/4 produces 300×450 and is the normal initial choice.

M0 may benchmark 1/8 followed by upscaling as an optional fast mode, but it cannot become the default unless:

- the upsampler is defined;
- output remains visually acceptable;
- host and firmware remain deterministic;
- the performance gain is meaningful on X4.

When the original image is smaller than the target, the decoder uses 1/1 and the common resampler performs any permitted enlargement.

### M1 cover geometry

Covers preserve the current product behavior:

- destination: 202×303;
- aspect ratio preserved;
- centered fill;
- excess pixels cropped;
- no stretching.

The host-preview path and device path use the same crop rectangle and resampler.

### M2 reader-page geometry

Image pages use contain rather than fill:

- aspect ratio preserved;
- the entire image remains visible;
- centered horizontally and vertically in the reader content viewport;
- unused pixels are white;
- no stretching;
- no overlap with reader chrome or margins.

The target is the active reader content rectangle, not necessarily the complete physical framebuffer.

### Resampler

The common resampler must be deterministic and streaming.

Initial policy:

- area or box averaging while shrinking;
- fixed-point bilinear interpolation while enlarging;
- independent horizontal and vertical factors;
- identical integer rounding on host and firmware.

M0 must verify that the selected decoder emits data in an order that permits the required box-filter row accumulation without retaining a complete decoded frame.

A different deterministic algorithm may be selected at the M0 gate if it materially reduces workspace or runtime.

## Dithering

The initial candidate is Floyd–Steinberg error diffusion:

- fixed integer arithmetic;
- deterministic left-to-right row traversal;
- no floating point;
- documented error clamp;
- white padding treated as white source input;
- two caller-owned `i16` error rows of `target_width + 2`.

At an 800-pixel target the error rows consume approximately 3.2 KB.

A stateless ordered Bayer matrix is the fallback when:

- single-pass row order cannot be guaranteed;
- strip or multi-pass rendering is required;
- Floyd–Steinberg workspace is too large;
- runtime is unacceptable;
- error propagation across output bands becomes fragile.

### Single-pass precondition

Floyd–Steinberg output depends on one deterministic destination traversal.

CalendulaOS may use it only while:

- the complete framebuffer is prepared once;
- destination rows are visited in defined order;
- panel flushing occurs after framebuffer completion;
- the image is not independently recomputed for separate panel passes.

The sibling renderer is useful as a warning: its panel architecture re-enters image rendering multiple times, so a stateless dither is operationally simpler. The exact number and shape of those passes must be cited from the pinned sibling revision rather than generalized as a permanent constant.

Any CalendulaOS change to multi-pass, strip-by-strip, or independently recomputed rendering must reopen the dither decision.

The selected dither must be shared by:

- firmware cover generation;
- firmware image-page rendering;
- `tools/preview`;
- host golden-output tests.

Existing threshold-only `COVER.BIN` files remain readable.

## Error model

Errors are divided into deterministic, transient, and cache-corruption classes.

### Deterministic absence or refusal

Examples:

- the EPUB declares no cover;
- a cover manifest reference cannot be resolved;
- the image uses an unsupported format;
- the JPEG uses an unsupported subtype;
- dimensions exceed fixed limits;
- the image is structurally malformed;
- orientation is unsupported;
- a path fails normalization or traversal checks.

These outcomes may be negative-cached when their identity includes the source-book generation and decoder-policy version.

### Transient failure

Examples:

- SD read error;
- short read caused by underlying I/O failure rather than source EOF;
- temporary-file creation failure;
- write failure;
- inability to replace the final cache artifact;
- source EPUB temporarily unavailable.

Transient failures are not persisted as deterministic negative results.

### Corrupt cache artifact

Examples:

- invalid `COVER.BIN` magic or version;
- invalid dimensions or stride;
- truncated payload;
- stale temporary file;
- invalid rendered-image sidecar;
- mismatched source identity.

A corrupt artifact is never presented as valid.

### Decoder error categories

The decoder exposes bounded error enums that distinguish at least:

- unsupported JPEG process;
- unsupported precision;
- unsupported components;
- unsupported sampling;
- unsupported scan organization;
- unsupported color transform;
- unsupported orientation;
- invalid marker;
- invalid segment length;
- missing table;
- invalid Huffman table;
- invalid quantization table;
- entropy truncation;
- invalid restart sequence;
- dimension limit;
- arithmetic overflow;
- output buffer too small;
- workspace too small;
- source I/O;
- sink failure.

Host tools may add human-readable context outside the core `no_std` error.

## M0A — Decoder adoption spike

M0A introduces no firmware behavior change.

### Objective

Determine whether adapting `tjpgdec-rs` is smaller, safer, and more maintainable than implementing the same SOF0 subset directly.

### Deliverables

1. Pin the evaluated `tjpgdec-rs` commit and crate version.
2. Produce an adaptation inventory covering:
   - complete-slice input;
   - two-phase prepare/decompress behavior;
   - RGB888 output;
   - raw-pointer workspace;
   - `unsafe`;
   - supported sampling;
   - restart handling;
   - quantization-table precision;
   - error model;
   - tests.
3. Implement a minimal forward-only input proof of concept.
4. Implement or demonstrate luma-only MCU output.
5. Demonstrate header-to-entropy continuation without rewind.
6. Measure:
   - workspace;
   - MCU buffer;
   - output-band buffer;
   - stack;
   - flash;
   - host decode speed.
7. Run the target EPUB corpus through the proof of concept.
8. Run basic malformed and truncation smoke tests.
9. Produce an adoption decision:
   - adapt;
   - constrained rewrite;
   - reject and use another path.

### M0A acceptance gate

Adaptation proceeds only if:

- forward-only input does not require buffering the complete JPEG;
- luma-only output avoids a complete RGB image and unnecessary RGB MCU storage;
- supported corpus images decode correctly;
- the resulting code can meet repository safety policy;
- patch complexity is materially smaller than a constrained decoder;
- upstream provenance and licensing are clear;
- the implementation can be covered by the M0B verification harness.

If adaptation fails, the implementation issue records why rather than silently turning the fork into a rewrite.

## M0B — Verification harness and raster pipeline

M0B introduces no firmware behavior change.

### Verification principle

Compilation and visual plausibility are insufficient.

The selected decoder must be tested against:

- independent decoders;
- generated format combinations;
- malformed-input mutations;
- streaming chunk variations;
- failure injection;
- memory-safety instrumentation where applicable.

### Pinned reference executables

M0B creates host-only reference runners for at least two independent implementations.

Preferred set:

- libjpeg-turbo or another mature JPEG implementation as a general JPEG oracle;
- `zune-jpeg` as an independent Rust oracle;
- JPEGDEC for JPEGDEC-specific progressive or low-bit-depth behavior;
- the fuzz-hardened TJpgDec fork for comparison with `tjpgdec-rs` ancestry.

Each runner accepts:

- input JPEG;
- requested scale;
- crop or fit geometry where supported;
- output format;
- decoder mode.

Each runner emits:

- parsed metadata;
- normalized result category;
- output dimensions and stride;
- raw luma or RGB output;
- checksum;
- optional timing.

C/C++ runners are built with:

- AddressSanitizer;
- UndefinedBehaviorSanitizer where supported;
- warnings enabled;
- reproducible compiler flags.

### Exact and tolerance comparisons

Exact equality is required for:

- parsed width and height;
- supported/unsupported classification defined by CalendulaOS;
- output dimensions;
- destination geometry;
- deterministic CalendulaOS resampler output;
- deterministic CalendulaOS dither output;
- host versus firmware-compatible build output;
- repeated decode output;
- all chunking variations of the same source.

Tolerance comparison is allowed for:

- decoder luma values where different valid integer IDCT rounding is expected;
- chroma-derived host comparison when a reference does not expose native luma;
- algorithms where JPEG does not prescribe exact upsampling or color-conversion rounding.

The tolerance must be numeric, documented, and justified.

No decoder is treated as correct merely because it matches JPEGDEC byte-for-byte.

### Conformance matrix

Generated valid fixtures cover:

- grayscale;
- 4:4:4;
- 4:2:2 horizontal;
- supported 4:2:2 variants;
- 4:2:0;
- odd widths;
- odd heights;
- partial MCUs;
- minimum dimensions;
- maximum accepted dimensions;
- each DCT reduction;
- restart intervals;
- multiple valid Huffman table assignments;
- multiple valid quantization table assignments;
- 16-bit quantization if supported;
- unusual but accepted component identifiers;
- APP and COM segments before and between required markers;
- byte stuffing;
- legal marker padding.

Unsupported fixtures cover:

- SOF1;
- SOF2;
- arithmetic coding;
- 12-bit samples;
- four-component images;
- baseline multi-scan;
- unsupported sampling factors;
- unsupported color transforms;
- unsupported EXIF orientation.

Malformed fixtures cover:

- every representative truncation boundary;
- invalid segment lengths;
- length arithmetic overflow attempts;
- missing DQT;
- missing DHT;
- invalid table IDs;
- oversubscribed Huffman trees;
- incomplete Huffman trees where invalid;
- invalid symbols;
- invalid SOS selectors;
- invalid restart order;
- unexpected markers in entropy data;
- missing EOI;
- excessive dimensions;
- excessive pixel count;
- malformed EXIF;
- random mutations.

### Streaming metamorphic tests

Every valid representative fixture is decoded with:

- one-byte source chunks;
- every chunk size from 1 through at least 512;
- boundary-aligned chunks around markers;
- pseudo-random chunk schedules;
- short final chunks;
- source callbacks that return less than requested.

All chunkings must produce the same result and output.

The test harness also injects an I/O error after every reachable source position and verifies:

- bounded failure;
- no panic;
- no invalid output write;
- no infinite retry;
- correct transient classification.

### Output-buffer safety

Every caller-provided buffer is surrounded by guard regions during host tests.

Tests verify:

- guards remain unchanged;
- reported output length does not exceed capacity;
- undersized buffers fail before out-of-bounds writes;
- early sink cancellation stops cleanly;
- sink errors propagate without further writes.

Rust code uses:

- Miri where supported;
- debug overflow checks;
- release-mode tests;
- sanitizers where available;
- `cargo-fuzz`.

Any retained `unsafe` requires:

- a written safety invariant;
- narrow scope;
- targeted Miri or guard tests;
- no raw pointer whose validity depends on undocumented pool movement;
- explicit review before M1.

### Layered port verification

An agent-assisted implementation is divided into independently testable layers:

1. Marker and segment parser.
2. Dimension and component validator.
3. Quantization-table parser.
4. Huffman-table builder.
5. Entropy bit reader.
6. Baseline coefficient decode.
7. Restart handling.
8. IDCT and reduced IDCT.
9. MCU assembly.
10. Luma emission.
11. Geometry.
12. Resampling.
13. Dithering.
14. Cache serialization.

Each layer receives focused vectors before the next layer lands.

A PR that introduces the complete decoder without these seams is not merge-ready.

### M0B deliverables

1. Selected decoder implementation.
2. Caller-owned workspace.
3. Forward-only input.
4. Streaming scaled-luma output.
5. Shared geometry.
6. Shared resampler.
7. Shared 1bpp dither.
8. Reference runners.
9. Generated conformance fixtures.
10. Malformed corpus.
11. Differential tests.
12. Streaming metamorphic tests.
13. Fuzz targets.
14. Workspace, stack, code-size, output-quality, and runtime report.
15. Written M1 and M2 go/no-go decisions.

### M0B measurements

Report:

- decoder struct bytes;
- table/workspace bytes;
- MCU workspace bytes;
- resampler bytes;
- dither bytes;
- total concurrent workspace;
- maximum decoder stack frame from `-Zemit-stack-sizes`;
- total affected call-chain stack;
- host runtime at 1/8, 1/4, 1/2, and 1/1;
- corpus success and refusal counts;
- fuzz duration and final corpus size;
- source bytes consumed;
- expected output-band size;
- 202×303 cover images;
- X4 and X3 page-size images;
- Floyd–Steinberg versus ordered Bayer;
- 1/4 reduction versus any 1/8-plus-upscale mode.

### M0B acceptance gate

M1 may proceed when:

- all supported fixtures decode within the defined exact or tolerance criteria;
- unsupported fixtures return the intended bounded error;
- malformed fixtures fail without panic;
- streaming chunk schedules produce identical results;
- source-error injection is clean;
- buffer guards remain intact;
- the complete pipeline is allocation-free;
- workspace fits the borrowable scratch with margin;
- stack analysis preserves the firmware floor;
- output quality is acceptable at 202×303;
- host and firmware-compatible builds produce byte-identical final 1bpp output;
- licensing and provenance are recorded;
- any retained `unsafe` has explicit approval.

M2 may proceed only when:

- full-page 1bpp output is visually useful;
- projected decode latency justifies firmware integration;
- required output ordering supports the selected dither;
- the page-render workspace fits concurrently with ZIP inflation.

## M1 — On-device cover generation

### Cover discovery

Cover discovery moves into `proto` so host and firmware use the same rules.

Order:

1. EPUB 3 manifest item with `properties="cover-image"`.
2. EPUB 2 cover metadata resolving to a manifest item.
3. Existing conservative id/href fallback for image manifest entries containing “cover”.

Discovery returns a bounded normalized image path, not a borrowed string tied to OPF scratch.

When discovery occurs while `EpubPackage` borrows `EPUB_OPF`:

1. resolve and copy the selected path into an owned bounded buffer;
2. copy required metadata;
3. end package and OPF-backed borrows;
4. only then lend OPF/XHTML scratch to the decoder.

The borrow transition is represented by Rust ownership and lifetimes rather than scratch aliasing.

### Cache-independent generation

Cover generation is an `ensure_cover_cache` operation after the existing-cover load attempt.

It is not limited to the full text-cache build.

Book-open sequence:

1. Load or build the text/page cache through the existing fast, replay, or full path.
2. Attempt to load `COVER.BIN`.
3. If a valid cover loaded, finish normally.
4. If missing or invalid, inspect versioned negative-cover state.
5. If no applicable negative state exists, open the source EPUB and attempt generation.
6. Load the newly published cover into `ReaderStore`.
7. Return the successful text-cache result regardless of cover outcome.

A book with a valid old `BOOK.BIN` but no cover can therefore gain a generated cover.

### Existing-cover policy

- A valid current `COVER.BIN` is trusted.
- A valid legacy host-generated cover accepted by the existing reader is trusted.
- M1 does not regenerate a valid cover merely to change thresholding to dithering.
- A corrupt or truncated cover is treated as absent.
- A stale temporary cover is removed before a new attempt.
- Changing this policy requires a cover-generator policy version.

### Negative result

M1 adds a small versioned negative-cover artifact separate from `COVER.BIN`.

It records deterministic outcomes such as:

- no cover declared;
- unsupported image format;
- unsupported JPEG subtype;
- unsupported orientation;
- permanently invalid image;
- rejected path or resource limit.

Its identity includes:

- book/source generation identity;
- normalized cover path where available;
- cover-discovery policy version;
- decoder-support policy version.

Increasing decoder support or changing discovery invalidates the result.

Transient I/O and publication failures are never persisted as negative results.

A successful cover publication removes previous negative state.

### Transactional publication

Cover generation does not write directly to the final `COVER.BIN`.

Required sequence:

1. Remove stale temporary siblings.
2. Create a temporary cover file.
3. Write the complete header and payload.
4. Close the file.
5. Reopen and validate:
   - magic and version;
   - dimensions;
   - stride;
   - exact payload length;
   - clean EOF.
6. Publish through the safest replace operation supported by the filesystem.
7. Remove the temporary file after success or recoverable failure.

A previously valid cover remains in place until its replacement has been fully written and validated.

If atomic rename is unavailable, the implementation issue documents the residual power-loss window and recovery behavior.

### M1 observability

Events distinguish:

- `cover_hit`;
- `cover_negative_hit`;
- `cover_missing`;
- `cover_invalid`;
- `cover_generated`;
- `cover_no_declared_image`;
- `cover_unsupported`;
- `cover_decode_invalid`;
- `cover_resource_limit`;
- `cover_io_error`;
- `cover_publish_error`.

Successful generation reports:

- source dimensions;
- component/sampling mode;
- selected DCT scale;
- target dimensions;
- encoded bytes consumed;
- decoder duration;
- raster duration;
- publication duration;
- total added open latency.

### M1 acceptance criteria

M1 is complete when:

- valid `BOOK.BIN` plus missing cover generates a cover;
- `CONT.BIN` replay plus missing cover generates a cover;
- full EPUB cache build plus missing cover generates a cover;
- valid existing cover is not regenerated;
- corrupt or truncated cover is replaced;
- deterministic refusal is not retried on the next open;
- transient failure is retried on a later open;
- injected write failures leave no valid-looking partial file;
- existing valid cover survives failed replacement;
- host and device generation produce byte-identical output;
- malformed and unsupported images do not prevent book opening;
- X4 cold and warm open measurements are reported;
- stack and static-memory checks remain satisfied.

Initial performance target:

- no more than five seconds of added generation latency for a typical 1200×1800 cover on X4, excluding panel refresh.

Exceeding that target requires a documented product decision based on hardware measurements.

## M2 — Dedicated image pages

### Initial promotion rule

M2 promotes an image only when the complete rendered content of one EPUB spine item is a single eligible image.

Transparent wrappers may include:

- `html`;
- `body`;
- `section`;
- `div`;
- `figure`;
- `p`;
- `a`.

The spine item may contain whitespace, metadata, styles, and scripts that do not produce reader content.

It is not eligible when it contains:

- meaningful text;
- a caption;
- more than one image;
- another media element;
- navigation content;
- a visible heading;
- an ordinary text block before or after the image.

This intentionally excludes decorative ornaments and avoids aspect-ratio-only heuristics.

### Parser event model

The XHTML layer introduces semantic events rather than encoding every event as text.

Conceptually:

```rust
enum ContentEvent<'a> {
    Text(TextBlock<'a>),
    ImageCandidate(ImageCandidate<'a>),
}
```

An `ImageCandidate` contains bounded parser-level information:

- raw `src` or SVG-image href;
- alt text;
- element and structural context;
- meaningful-sibling state;
- hidden, decorative, or presentation flags;
- current XHTML document path.

The parser does not replace the candidate with final placeholder text before the sink can classify it.

The build sink:

1. resolves and normalizes the href;
2. locates the manifest or ZIP entry;
3. feeds a bounded prefix through `ImageProbe`;
4. records format, dimensions, and orientation;
5. applies the image-only-spine rule;
6. emits either:
   - normalized image-page event; or
   - existing placeholder text event.

### Normalized image-page event

An image-page event contains:

- normalized EPUB-root-relative path;
- stable image-record ID;
- image format;
- intrinsic width and height;
- orientation status;
- fit mode, initially `Contain`;
- reserved flags.

The stable image-record ID is assigned during cache construction and is collision-free within the book cache.

The event contains no borrowed reference into XHTML or OPF scratch.

### `CONT.BIN` schema

M2 increments `CONTENT_VERSION`.

The content stream becomes a typed event stream with at least:

- `Text`;
- `ImagePage`;
- `SpineEnd`.

Text events preserve the existing text, role, style, alignment, and paragraph-end semantics.

Image events contain normalized image metadata and href.

Replay must reproduce the same semantic event sequence without reopening the EPUB for classification.

A malformed, unknown, overlength, truncated, or inconsistent event invalidates `CONT.BIN` and triggers the existing full-EPUB fallback.

Tests prove:

- full parse and replay generate identical page-cache bytes;
- image events survive type-setting changes;
- corrupt image framing cannot become text;
- old versions are rejected.

### Page-cache schema

M2 adds an explicit page kind.

Conceptually:

```rust
enum PageKind {
    Text,
    Image,
}

struct PageRecord {
    kind: PageKind,
    flags: u8,

    first_block: u16,
    block_count: u16,

    image_record_id: u16,
    image_href_offset: u32,
    image_href_len: u16,
}
```

For a text page:

- text range fields are valid;
- image fields are zero.

For an image page:

- text fields are zero;
- image fields identify one normalized image reference.

The exact packed layout may differ, but these invariants are required.

### Cache invalidation

M2 changes parsing semantics and page-record layout.

M2 must:

- increment `CONTENT_VERSION`;
- increment `CACHE_V2_VERSION`;
- reject older page and section records;
- remove the old page-cache version from any compatibility window that would hide images;
- rebuild from the source EPUB;
- reject stale or partial rebuilt sections.

M1 alone does not require this text/page-cache version change.

### Render-time source access

The display task:

1. obtains the normalized href and stable image ID;
2. opens the source EPUB;
3. locates the ZIP entry;
4. streams and inflates it;
5. decodes and rasterizes into the reader content rectangle;
6. requests Full refresh.

The cache stores a normalized href, not a ZIP local-header offset.

If the EPUB is missing, changed, unreadable, or lacks the entry, the page renders the placeholder.

### Rendered-page cache — deferred

Without a cache, every visit pays source-open, inflate, decode, resample, and dither cost.

A future sidecar could persist the final 1bpp raster. At a roughly 43 KB reader rectangle, seventeen illustrations would consume under 1 MB.

The cache is not required for correctness in a single-pass renderer and is deferred until revisit latency is measured.

If adopted, its identity includes:

- book/source generation identity;
- normalized image path;
- source image length and digest or equivalent generation value;
- target viewport dimensions;
- crop/fit policy;
- decoder policy version;
- resampler version;
- dither version;
- output pixel format.

It uses M1-style temporary publication and exact payload validation.

### M2 failure memo

The reading session owns a fixed-capacity table of deterministic image failures.

The key is the stable image-record ID plus the current book-cache identity, not only a path hash.

Each entry contains:

- stable image ID;
- deterministic error class.

The memo:

- is allocation-free;
- has fixed capacity;
- stops recording when full or uses a documented bounded replacement policy;
- is cleared when leaving the book;
- never records transient SD, source-open, or I/O failures;
- is never persisted.

Unsupported format and dimensions should normally be classified during cache construction, so the memo primarily suppresses repeated malformed-JPEG decode work.

### Framebuffer transaction

The panel must never display a partially decoded image.

Sequence:

1. Prepare fixed page chrome.
2. Clear the image rectangle to white.
3. Decode and rasterize.
4. On success, submit the completed image page.
5. On failure:
   - clear the image rectangle again;
   - draw the existing placeholder;
   - submit only the completed fallback page.

No panel refresh begins until success or fallback is complete.

A successful image page forces Full refresh.

### M2 observability

Telemetry distinguishes:

- image page loaded from cache;
- source EPUB missing;
- ZIP entry missing;
- unsupported format;
- unsupported JPEG subtype;
- invalid JPEG;
- resource-limit rejection;
- decode success;
- decode failure;
- deterministic memo hit;
- placeholder fallback.

Success reports:

- global and section page;
- stable image ID;
- source and target dimensions;
- sampling;
- DCT scale;
- compressed and uncompressed bytes;
- source-open time;
- inflate/decode/raster time;
- frame-ready time;
- refresh class.

### M2 acceptance criteria

M2 is complete when:

- image-only spine item becomes exactly one image page;
- image page survives `CONT.BIN` replay;
- full parse and replay generate identical page caches;
- decorative images in prose remain placeholders;
- text plus image remains text plus placeholder;
- image plus caption is not promoted;
- multiple images are not promoted;
- overlength, absolute, and escaping paths are rejected;
- missing source or ZIP entry renders placeholder;
- malformed, progressive, oversized, and unsupported JPEGs render placeholder;
- transient failures remain retryable;
- deterministic failures are suppressed within the session;
- failure after partial framebuffer writes leaves no remnants;
- successful image page forces Full refresh;
- old content/page caches rebuild;
- emulator goldens pass on X4 and X3;
- X4 hardware page-turn and stack measurements are reported.

Initial framebuffer-preparation target:

- no more than five seconds for a typical 1200×1800 illustration, excluding Full-refresh panel busy time.

Shipping above that target requires an explicit product decision and should consider a rendering plate or rendered-page cache.

## SVG-wrapped images

Arbitrary SVG rasterization remains out of scope.

A later extension may recognize an image-only spine item whose SVG contains exactly one external supported raster image.

That extension must:

- ignore `<title>` and `<desc>` as reader prose where appropriate;
- preserve navigation labels;
- resolve `href` and `xlink:href`;
- reject transforms, clipping, vector drawing, multiple images, and data URLs unless explicitly supported;
- pass the normalized raster path through the same image-page pipeline.

It is not required for initial M2.

## Scratch-memory and lifetime plan

### No new large statics

The decoder and raster workspace are borrowed from existing reader scratch.

The implementation must not add:

- full encoded-image storage;
- full decoded-image storage;
- second framebuffer;
- decoder singleton static;
- large task-local arrays.

### Lifetime sequence for cover generation

During a full EPUB open:

1. parse OPF and construct `EpubPackage`;
2. discover and normalize the cover;
3. copy path and metadata into bounded owned values;
4. complete text-cache work that still borrows package data;
5. drop package, manifest, spine, and OPF-backed views;
6. lend OPF/XHTML storage as decoder workspace;
7. retain ZIP inflate state and compressed input separately;
8. decode and publish.

After `BOOK.BIN` or `CONT.BIN` hit:

1. complete text-cache open;
2. parse only package information needed for cover discovery;
3. copy and normalize the cover path;
4. drop package borrows;
5. reuse scratch for decode.

For M2, no parser or package object remains live while the same scratch is lent to the decoder.

The borrow checker enforces exclusivity.

### Working-set budget

M0B replaces estimates with exact values.

Expected state includes:

- input buffer;
- Huffman tables;
- quantization tables;
- component and scan metadata;
- coefficient/IDCT workspace;
- MCU or scaled output band;
- resampler rows;
- dither rows.

Initial planning range:

- approximately 8–18 KB beyond existing ZIP inflate and compressed-input state.

The selected implementation must retain at least 8 KB of margin in the scratch region actually lent to it.

### Stack

Requirements:

- no recursion;
- no large local arrays;
- no data-dependent stack growth;
- buffers supplied by caller;
- `_stack_start - _stack_end >= 27 KB`;
- complete affected call chains reported through `-Zemit-stack-sizes`.

A regression near the current stack floor blocks firmware integration.

## Refresh policy

- Cover generation performs no panel refresh.
- Existing views display the loaded cover through current refresh behavior.
- Successful full-page image rendering requests Full refresh.
- Fallback page must be visually correct under the selected refresh.
- Benchmarks report decoder/frame preparation separately from panel busy time.
- Image rendering does not violate the display task’s single-writer ownership.
- No background task renders into the framebuffer.

A “Rendering…” plate is deferred.

Product review is required when measured framebuffer preparation routinely exceeds two seconds and the unchanged previous screen appears misleading or unresponsive.

## Robustness and security

Image and cache inputs are untrusted.

Required protections:

- checked integer arithmetic;
- bounds checks before every table, input, and output access;
- bounded loops derived from validated dimensions;
- no panic for external input;
- no unchecked UTF-8;
- no path traversal;
- no recursion;
- no unbounded marker skipping;
- no allocation from encoded dimensions;
- exact cache payload validation;
- version validation before record interpretation;
- deterministic failure on insufficient workspace;
- fuzz coverage;
- streaming chunk tests;
- source-error injection;
- output guard testing.

A failed image operation must not corrupt:

- reader position;
- section-cache state;
- book metadata;
- cover state;
- source EPUB;
- a prior valid cache artifact.

## Verification commands

Each implementing issue restates commands relevant to its scope.

Required repository checks:

- `tools/check.sh fmt`;
- `tools/check.sh fast`;
- `tools/check.sh emulator` on X4 and X3 for changed rendering;
- `tools/check.sh firmware` for display-task, SD, scratch, or publication changes;
- `tools/check.sh all` before merge-ready.

Required decoder checks:

- host unit and integration tests;
- `no_std` build;
- debug and release tests;
- differential corpus;
- chunking metamorphic tests;
- failure injection;
- fuzz targets;
- Miri for applicable safe/unsafe modules;
- C/C++ oracle sanitizer runs.

Required hardware results:

- M1 cold open with missing cover;
- M1 warm open with cached cover;
- M1 retry after transient failure;
- M2 first image render;
- M2 repeat image render;
- X4 page-turn latency;
- stack delta;
- firmware image-size delta;
- SD bytes and read duration where measurable.

Golden output changes caused by the shared raster pipeline land with the code that produces them.

## Build sequence

### M0A — Decoder selection

1. Pin `tjpgdec-rs`.
2. Inventory implementation gaps.
3. Prototype forward-only input.
4. Prototype luma output.
5. Measure workspace and stack.
6. Decode target corpus.
7. Decide adapt versus constrained rewrite.

### M0B — Verification and raster pipeline

8. Build independent reference runners.
9. Add generated JPEG fixtures.
10. Add malformed corpus.
11. Add differential tests.
12. Add streaming metamorphic tests.
13. Add source-error injection.
14. Add output guards.
15. Add fuzz targets.
16. Complete selected baseline decoder.
17. Implement geometry and resampler.
18. Implement candidate dithers.
19. Produce measurements and M1/M2 gates.

### M1 — Covers

20. Move cover discovery and path normalization into `proto`.
21. Add negative-cover artifact.
22. Add transactional publication helpers.
23. Add cache-independent `ensure_cover_cache`.
24. Wire all text-cache paths.
25. Load newly generated cover into `ReaderStore`.
26. Add failure injection and legacy-cover tests.
27. Run X4 hardware benchmarks.

### M2 — Image pages

28. Add parser image candidates.
29. Add image-only-spine classification.
30. Add normalized image-page events.
31. Bump and implement typed `CONT.BIN`.
32. Add page kind and image reference.
33. Bump cache versions.
34. Add render-time source access.
35. Add deterministic failure memo.
36. Add transactional framebuffer fallback.
37. Force Full refresh.
38. Add X4/X3 goldens.
39. Run X4 page-turn benchmarks.
40. Decide rendering plate and rendered-cache needs.

### Later candidate — progressive thumbnails

41. Measure progressive incidence in representative EPUBs.
42. Define supported scan organizations.
43. Build progressive conformance fixtures.
44. Prototype forward-only DC-only decode.
45. Compare output and latency.
46. Decide whether the feature justifies its parser and test surface.

## Risks

### Decoder adaptation becomes a rewrite

Mitigation:

- explicit M0A gate;
- patch inventory;
- time-boxed proof of concept;
- compare against a constrained implementation rather than continuing indefinitely.

### Agentic port reproduces upstream defects

Mitigation:

- multiple independent oracles;
- layer-by-layer tests;
- malformed corpus;
- fuzzing;
- no single implementation treated as truth.

### Native Rust candidate contains unsafe internals

Mitigation:

- eliminate raw-pointer workspace where practical;
- narrow any retained unsafe;
- document invariants;
- Miri and guard tests;
- explicit review gate.

### Decoder performance is slower than expected

Mitigation:

- M0 before firmware integration;
- reduced IDCT;
- luma-only output;
- bounded format contract;
- stop after M1 if M2 is not worthwhile.

### Stack regression

Mitigation:

- caller-owned state;
- no recursion;
- stack reports;
- preserve firmware floor.

### Scratch aliasing

Mitigation:

- copy paths out of package data;
- end parser borrows before decode;
- route buffers through `ReaderCacheScratch`;
- no undocumented aliasing.

### Poor 1bpp quality

Mitigation:

- real corpus inspection;
- deterministic shared dither;
- separate cover and page geometry;
- retain ordered-dither fallback;
- accept M1 without M2.

### Repeated cover-generation cost

Mitigation:

- valid cover cache;
- deterministic negative result;
- transient-only retry.

### Partial cache files

Mitigation:

- temporary artifact;
- close and read-back validation;
- preserve prior valid output;
- recovery tests.

### M2 cache inconsistency

Mitigation:

- typed events;
- persisted image metadata;
- explicit page kind;
- cache-version bump;
- parse/replay equivalence.

### Decorative-image promotion

Mitigation:

- only complete image-only spine items;
- no aspect-ratio-only promotion;
- fixture coverage.

### Stale rendered-page sidecar

Mitigation:

- deferred until needed;
- source identity and digest;
- geometry/resampler/dither/output-format versions;
- transactional publication.

### Unpredictable image latency

Mitigation:

- separate timing stages;
- explicit targets;
- rendering-plate decision;
- rendered cache based on measured revisit cost.

## Decisions closed by this PRD

- Decoder implementation is selected through M0A, not assumed.
- `tjpgdec-rs` is evaluated before a greenfield decoder or full JPEGDEC port.
- `zune-jpeg` is an oracle, not the initial firmware dependency.
- JPEGDEC is prior art and a possible algorithm source, not the initial dependency.
- A decoder translation is not accepted without differential and fuzz verification.
- JPEG input is forward-only and does not require a complete source slice.
- Primary decoder output is luma.
- M1 generation is independent of the text-cache path.
- Valid existing covers are trusted.
- Deterministic negative results are persisted; transient failures are retried.
- Cover publication is transactional.
- Default cover scale does not undershoot the target.
- Covers use centered fill-and-crop.
- Reader image pages use contain with white padding.
- Initial JPEG support is SOF0 baseline only.
- Progressive JPEG is a later measured extension.
- M2 uses typed parser and content-cache events.
- M2 adds explicit image-page records and persisted hrefs.
- M2 bumps content and page-cache versions.
- Initial promotion is limited to image-only spine items.
- Failure memo uses collision-safe stable IDs and deterministic errors only.
- Partial framebuffer output is cleared before fallback.
- Rendered-page caching is deferred.
- Host and firmware share the complete raster pipeline.

## Remaining decision gates

- Adapted `tjpgdec-rs` versus constrained Rust implementation.
- Whether any retained unsafe is acceptable.
- Exact decoder workspace.
- 16-bit quantization-table support.
- Floyd–Steinberg versus ordered Bayer.
- 1/8-plus-upscale fast mode.
- M2 go/no-go after full-page inspection.
- Rendering plate.
- Rendered-page sidecar.
- Progressive DC-only extension.
- X3-specific cover geometry.
- Transparent SVG wrappers.
- Standalone image blocks inside textual spine items.

## References

External claims in this document carry a bracketed tag resolved here. All entries were checked on 2026-07-25; upstream repositories change, so any decision that turns on a licence or a published figure must be re-checked against the pinned revision at the time it is acted on.

**[tjpgdec-rs]** — https://github.com/planet0104/tjpgdec-rs, crate `tjpgdec-rs`.
Checked: crates.io metadata (version 0.4.0, the only published release; licence `MIT OR Apache-2.0`); repository README (`no_std` support, caller-provided memory pool, workspace sizes 3,100 / 3,500 / 9,644 bytes for optimization levels 0/1/2); `src/lib.rs` at 0.4.0 (single test `test_basic`, asserting `BUFFER_SIZE == 512`); `src/pool.rs` at 0.4.0 (`unsafe` blocks returning slices via `core::slice::from_raw_parts_mut`).
No commit is pinned yet. M0A must pin one before evaluation results are recorded.

**[zune-jpeg]** — https://github.com/etemesi254/zune-image/tree/dev/crates/zune-jpeg, crate `zune-jpeg`.
Checked: crates.io metadata (version 0.5.15; licence `MIT OR Apache-2.0 OR Zlib`); crate documentation (states no-std operation; the decode call returns an owned buffer containing the whole image rather than streaming through callbacks).

**[TJpgDec]** — ChaN's TJpgDec, http://elm-chan.org/fsw/tjpgd/00index.html.
Checked: 3.5 KB work area independent of image width; 3.5–8.5 KB of code in `.text` + `.rodata`; permissive terms allowing use, modification, and redistribution without restriction at the user's own responsibility.

**[TJpgDec-fork]** — https://github.com/cmumford/TJpgDec.
Checked: GPL-3.0. Adds a libFuzzer target, sanitizer support, fixes for memory-access errors found by fuzzing, a CMake build, and GitHub Actions CI; retains unmodified upstream source on a separate branch.

**[JPEGDEC]** — https://github.com/bitbank2/JPEGDEC.
Checked: Apache-2.0 (`LICENSE`: "Copyright 2020 BitBank Software, Inc."); documented features covering 1/2, 1/4, and 1/8 reduced decode, DC-only progressive thumbnail decode, and optional Floyd–Steinberg dithering to 1, 2, or 4-bpp grayscale.
Repository contents verified at commit `86282979224c8a32fd51e091ed5a35b0c699a52b`, the revision `crosspoint-reader` pins: the root `Makefile` builds a `demo` executable; `CMakeLists.txt` is an ESP-IDF `idf_component_register` declaration; the tree carries `test_images/`, `examples/`, `MacOS/JPEGDEC_Test`, and `linux/examples`, and no separate conformance test suite.

**[crosspoint-reader]** — sibling firmware for the same X3/X4 hardware, read from a local checkout.
The 2-second decode and ~55 KB free-heap figures quoted under "Sibling-firmware data point" are comments recorded in its `PixelCache` implementation, not CalendulaOS benchmark results.
