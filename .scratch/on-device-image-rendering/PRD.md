# On-Device Image Rendering — PRD

Status: **planning; implementation decisions are closed except where explicitly assigned to the M0 decision gate.**

Drafted 2026-07-25 from a review of `fix/svg-wrapped-images`, the current reader-cache architecture, and measurements from the X4 release image. No implementation has landed.

Numbers marked *(measured)* come from this repository at `c8439e8`. Numbers marked *(estimate)* have not been verified on hardware and are the primary purpose of milestone 0.

## Summary

CalendulaOS can display a pre-rasterized 1bpp cover, but it cannot decode an EPUB image on the device.

Today, `tools/preview` uses the host `image` crate to decode a cover, crop it to 202×303, threshold it to 1bpp, and write `COVER.BIN` to the SD card. Books that do not go through that manual step have no cover. EPUB illustrations are never rendered: the XHTML parser reduces `<img>` elements to text placeholders, and the reader never opens the referenced image bytes.

This project adds a `no_std`, allocation-free JPEG pipeline in `proto`, staged so the smallest useful result lands first:

- **M0 — decoder spike:** implement and validate the JPEG decoder, deterministic resampling, and dithering on the host.
- **M1 — on-device covers:** generate and cache a book cover on any book-open path when no usable cover exists.
- **M2 — dedicated image pages:** represent image-only spine items in the content and page caches, then decode them into the framebuffer while reading.

M2 initially supports only EPUB spine items whose rendered body consists of one image. Images embedded among prose remain placeholders. Inline image layout and heuristic promotion of decorative images are explicitly out of scope.

Stopping after M1 is an acceptable outcome if M0 or hardware testing shows that full-page rendering is too slow or visually poor.

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
- The generated cover becomes available to existing home and library UI after the first successful book open.
- A valid existing host-generated `COVER.BIN` remains usable and is not regenerated merely because the device decoder exists.
- A missing, corrupt, or unsupported cover never prevents the book from opening.

Generating covers for every library entry before that book has been opened is out of scope.

### Image pages

After M2:

- A dedicated image-only EPUB spine item appears as one reader page.
- The image is scaled to fit inside the reader viewport while preserving its aspect ratio.
- Unused viewport area is white.
- A rendered image page always requests a Full e-ink refresh.
- Unsupported or malformed images display the existing image placeholder instead of aborting the book open or page render.
- Images that coexist with prose, captions, or additional images remain placeholders.

## Goals

- Generate a book’s cover on-device without a host preprocessing step.
- Preserve generated covers across subsequent opens.
- Render dedicated full-page illustrations in the reading flow.
- Keep decoder and parser logic host-testable in `proto`.
- Remain `no_std`, allocation-free, and panic-free for external input.
- Stay within the existing RAM, stack, and static-memory budgets.
- Preserve exact content-cache replay behavior.
- Make malformed, oversized, unsupported, and truncated images fail locally rather than failing the enclosing book operation.
- Produce deterministic host and firmware output from the same decoder, resampler, and dither implementation.

## Non-goals

- Inline images participating in text layout.
- Floating images, CSS sizing, text wrapping around images, or image captions.
- Heuristic promotion of an image embedded in an otherwise textual spine item.
- Rendering arbitrary SVG.
- PNG decoding.
- Progressive JPEG (initially; see "Why progressive JPEG is refused initially").
- Arithmetic-coded JPEG.
- Lossless JPEG.
- 12-bit JPEG samples.
- Grayscale e-ink waveforms or multi-pass panel grayscale.
- Changes to Wi-Fi upload, synchronization, or book-transfer behavior.
- Prefetching or background decoding.
- Maintaining compatibility with old M2 page caches after the page-record schema changes.

## Evidence

### EPUB corpus *(measured)*

`EIGHTY-SIX-VOLUME-1.epub` contains 20 image files:

- all are JPEG;
- all use SOF0 rather than progressive SOF2;
- typical illustrations are 1200×1800;
- the largest image file is the 1,170,441-byte cover;
- all image entries are DEFLATE-compressed inside the ZIP;
- total image payload is approximately 8.7 MB.

This corpus contains no `<svg>` or SVG `<image>` elements. Its image references are ordinary XHTML `<img>` elements.

This corpus is the initial performance and visual-quality corpus, not the complete JPEG conformance suite.

### DRAM *(measured)*

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

### Refresh and latency *(measured budgets, estimated decode)*

Current refresh behavior:

- Full refresh busy time: approximately 3,000–4,300 ms.
- Fast refresh: approximately 500 ms.
- Median fast press-to-settled time: approximately 550 ms.
- Warm book open: approximately 150 ms.

Full-page art requires a Full refresh because partial refreshes ghost badly on high-contrast images.

The earlier 2–5 second decode estimate is not an acceptance result. M0 and M1 must replace it with measured data.

### Sibling-firmware data point *(measured, different implementation)*

`crosspoint-reader` runs full-page EPUB image decoding on the same SoC and the same X3/X4 panels. Its own source notes record two figures:

- a full-page image decode costs approximately 2 seconds;
- free heap during an image page runs approximately 55 KB.

These are the closest available hardware measurements, but they are not a substitute for ours. Their decode reads an image already extracted to a plain SD file, so it excludes ZIP inflate; it uses a different decoder; and it produces 2bpp rather than 1bpp output. Treat the 2-second figure as evidence that the order of magnitude is workable, not as a projected result for this design.

### Flash *(measured)*

The application partition is approximately 6.5 MB and the current firmware image is approximately 3.7 MB. Decoder flash growth is not expected to be the limiting resource, but the final delta must still be reported.

## Prior art: `crosspoint-reader`

`crosspoint-reader` is a C++/Arduino firmware for the same X3/X4 hardware that already ships full inline EPUB image rendering. It was evaluated in full. This section records what was taken and what was rejected, so the decisions are not re-litigated.

Its pipeline: bitbank2 **JPEGDEC** (Apache-2.0, roughly a 17–20 KB object, heap-allocated per decode) in 8-bit grayscale mode, with the same 1/1, 1/2, 1/4, 1/8 built-in reductions; nearest-neighbour downscaling and bilinear upscaling in 16.16 fixed point; a stateless 4×4 Bayer dither to four levels; images lazily extracted from the ZIP to plain SD files; and the dithered 2bpp output cached to a sidecar file so a page is decoded exactly once.

### Rejected: JPEGDEC as a dependency

- It is C, so it arrives through FFI and `unsafe`. Library crates here `forbid(unsafe_code)` and `fw` permits only narrow per-item opt-ins; an entire decoder's FFI surface is not narrow.
- Its file interface requires a working `seek` callback. Image entries in the target corpus are DEFLATE-compressed inside the ZIP and are not seekable without re-inflating from the entry start.
- `crosspoint-reader` carries local patches for two real defects it found in JPEGDEC's progressive path — a wild pointer and an incorrect DC write. Vendoring the library means owning that maintenance.

The Apache-2.0 licence is not an obstacle; the architecture is. A `no_std`, allocation-free Rust decoder written against the narrow initial contract in this PRD is the smaller long-term cost.

### Rejected: extracting images to SD before decoding

This is forced on `crosspoint-reader` by the `seek` requirement above. Baseline JPEG decodes forward-only, so this design streams directly from the ZIP entry instead. That avoids writing roughly a megabyte to SD per image, and avoids owning the lifecycle, wear, and space accounting of extracted image files.

### Rejected: nearest-neighbour downscaling

See "Resampler". Their choice is a speed compromise whose failure mode they had to work around.

### Adopted: streaming dimension probe

`crosspoint-reader` sizes an image during layout by feeding roughly the first 1 KB of the entry through a byte-at-a-time state machine that reads JPEG SOF or PNG IHDR dimensions, with early stop, and leaves the image bytes inside the EPUB until its page is first rendered.

This is precisely the metadata step M2 needs, it requires no seeking and no allocation, and it is straightforward to express as a `no_std` Rust state machine. The design is adopted; the C++ source is not copied.

### Adopted: per-axis scale factors

See "Resampler".

### Adopted: bounded in-session failure memo

See "M2 failure behavior".

### Considered: rendered-output cache

See "Rendered-page cache (deferred decision)". Mandatory for their multi-pass renderer; a latency optimisation for this single-pass one.

## Architecture

### Shared pipeline

```text
EPUB file
  -> ZIP entry lookup
  -> ZipStream and existing DEFLATE inflate
  -> JPEG marker and entropy parser
  -> scaled luma output
  -> deterministic resampler
  -> deterministic 1bpp dither
  -> COVER.BIN       (M1)
     or framebuffer  (M2)
```

Every stage is streaming. No stage may allocate or retain a complete decoded image.

### Module placement

Pure image logic lives in `proto`:

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

Host tools use the same `proto` decoder, resampler, and dither code as firmware.

### Input abstraction

The decoder consumes bytes through a sans-I/O streaming interface supplied by the caller. It must not depend on `std`, filesystem types, `embedded_sdmmc`, or EPUB-specific structures.

The caller provides:

- encoded input bytes incrementally;
- caller-owned decoder workspace;
- output-band storage;
- a callback or sink for completed luma or 1bpp rows.

The decoder returns bounded error values and never panics for malformed input.

## Initial JPEG contract

The first shipping decoder supports only the following:

- SOF0 baseline sequential DCT.
- 8-bit samples.
- Huffman entropy coding.
- One-component grayscale JPEG.
- Three-component YCbCr JPEG when the component ordering and color transform are supported.
- Common 4:4:4, 4:2:2, and 4:2:0 sampling.
- One interleaved scan for a three-component image.
- One scan for a grayscale image.
- Restart markers and restart intervals.
- 8-bit and 16-bit quantization tables if M0 demonstrates both can be supported within the workspace budget.
- Absent EXIF orientation or orientation value 1.

The decoder refuses:

- SOF1 extended sequential JPEG;
- SOF2 progressive JPEG;
- multiple-scan baseline files;
- arithmetic coding;
- lossless JPEG;
- 12-bit samples;
- CMYK and YCCK;
- unsupported component layouts or transforms;
- mirrored or rotated EXIF orientations;
- malformed, truncated, or internally inconsistent files.

“Refuse” means return an explicit unsupported or invalid-image result. It must not panic, loop indefinitely, read beyond supplied input, or fail the enclosing book operation.

The implementation must not assume that every three-component file is YCbCr solely because it has three components. M0 must define and test the exact metadata and component-ordering rules used to recognize supported YCbCr input.

Support may be widened after M0, but M1 and M2 acceptance tests must describe the actual supported subset precisely.

### Why progressive JPEG is refused initially

Progressive JPEG is excluded for cost, not because it is infeasible.

A useful degradation exists and is worth stating so it is not rediscovered later. A progressive file's first scan is its DC scan, and decoding only that scan yields a complete 1/8-resolution image in raster order. It needs no full coefficient plane and no seeking, because the DC scan precedes the refinement scans in the file. `crosspoint-reader` ships exactly this: it detects progressive input, forces 1/8, and bilinear-upscales the result.

The cost is a second entropy-decoding path with its own successive-approximation and spectral-selection handling, its own bug surface, and its own fixture set, in exchange for a visibly soft image. That is the wrong trade for the first shipping decoder, which must be small enough to audit.

Progressive DC-only decode is therefore listed as a post-M0 widening candidate rather than a permanent exclusion. Until it exists, progressive input is refused and renders the placeholder.

## Resource and abuse limits

The parser rejects an image before expensive decode work when any configured limit is exceeded.

Initial limits:

- Maximum JPEG width: 8,192 pixels.
- Maximum JPEG height: 8,192 pixels.
- Maximum decoded pixel count: 32,000,000 pixels.
- Maximum uncompressed JPEG entry length: 16 MiB.
- Maximum normalized EPUB image path: 256 UTF-8 bytes.
- Marker lengths, table counts, component counts, sampling factors, MCU counts, and restart intervals must be checked for arithmetic overflow before use.

Path resolution must:

- remove URL fragments before ZIP lookup;
- resolve relative to the containing OPF or XHTML path as appropriate;
- reject absolute paths;
- reject any normalized path that escapes the EPUB root through `..`;
- reject overlength output;
- avoid lossy UTF-8 conversion.

These are correctness and denial-of-service limits, not claims about the EPUB specification’s theoretical maxima.

## Scaling and geometry

### DCT scale selection

Supported decoder reductions are 1/1, 1/2, 1/4, and 1/8.

The decoder chooses the largest reduction whose decoded output is still large enough for the required crop or fit operation in both dimensions.

A scale that undershoots the target is not chosen merely because it is faster.

For a typical 1200×1800 image targeting 202×303:

- 1/8 produces 150×225 and undershoots the target;
- 1/4 produces 300×450 and is the normal initial choice.

M0 may benchmark 1/8 followed by upscaling as an optional fast mode, but it cannot become the default unless its resampling policy and visual-quality threshold are explicitly accepted.

When the original image is smaller than the target, the decoder uses 1/1 and the common resampler performs any permitted enlargement.

### M1 cover geometry

Covers preserve the current product behavior:

- destination: 202×303;
- aspect ratio preserved;
- centered fill;
- excess pixels cropped;
- no stretching;
- white is used only if malformed geometry prevents a complete output.

The host preview path and the device path use the same crop rectangle and resampler.

### M2 reader-page geometry

Image pages use contain rather than fill:

- aspect ratio preserved;
- the entire image remains visible;
- centered horizontally and vertically within the reader content viewport;
- unused pixels are white;
- no stretching;
- image pixels do not overlap reader chrome or margins.

The target is the active reader content rectangle, not necessarily the full physical framebuffer.

### Resampler

The common resampler must be deterministic and streaming.

The initial policy is:

- area or box averaging when shrinking;
- fixed-point bilinear interpolation when enlarging;
- identical integer rounding on host and firmware.

Horizontal and vertical scale factors are derived independently. Integer rounding of the fitted destination height means the destination aspect ratio does not exactly match the source, so a single scale factor applied to both axes selects the wrong source row and can drop content. Both the source-to-destination and destination-to-source factors are computed per axis.

Nearest-neighbour downscaling is explicitly not the policy. At the reductions this project actually performs — roughly 6:1 for a 1200×1800 cover — it discards almost every source pixel and aliases badly on the line art that dominates the target corpus. `crosspoint-reader` uses nearest-neighbour for speed and needed the per-axis fix above to stop it losing rows outright; that is a symptom worth avoiding rather than a pattern worth copying.

M0 must verify that this can be implemented without a full decoded frame. A different deterministic algorithm may be selected at the M0 gate if it materially reduces memory or runtime, but host and firmware must still share it.

## Dithering

The initial output algorithm is Floyd–Steinberg error diffusion:

- fixed integer arithmetic;
- deterministic left-to-right row traversal;
- no floating point;
- errors clamped to a documented range;
- white padding participates as white input rather than uninitialized state.

The implementation may use two caller-owned `i16` error rows of `target_width + 2` elements. At an 800-pixel target this is approximately 3.2 KB.

An ordered Bayer matrix is the fallback only if M0 shows that Floyd–Steinberg causes unacceptable workspace, runtime, or band-boundary complexity.

### Single-pass rendering is a precondition

Floyd–Steinberg is order-dependent: its output is defined only for one deterministic traversal of the destination. It is available here solely because a render writes the framebuffer exactly once, and the panel flush bands out of the already-completed buffer.

Any future change that renders a page more than once, or that renders it in independently computed strips or bit planes, invalidates this choice and forces a stateless dither such as ordered Bayer. `crosspoint-reader` is the worked example: its renderer invokes the image draw path roughly 14 times per page (a BW pass, an anti-aliasing restore, and two grayscale planes of about six strips each), so it uses a stateless 4×4 Bayer matrix and could not use error diffusion at all.

An implementation that adds multi-pass or strip rendering must revisit this section rather than attempting to preserve error diffusion across passes.

The selected dither implementation must be used by:

- firmware cover generation;
- firmware image-page rendering;
- `tools/preview`;
- host golden-output tests.

Existing host-generated threshold-only `COVER.BIN` files remain readable. Only newly generated artifacts are required to match the new deterministic pipeline.

## Error model

Errors are divided into three classes.

### Deterministic absence

Examples:

- the EPUB declares no cover;
- the referenced manifest item does not exist;
- a candidate uses an explicitly unsupported format;
- an image exceeds a fixed resource limit;
- the image is malformed in a way that will not change while the source EPUB is unchanged.

These outcomes may be persisted as a versioned negative cover result so they are not retried on every open.

### Transient failure

Examples:

- SD read error;
- temporary file creation or write failure;
- failed ZIP read caused by an I/O error;
- inability to publish a completed cache artifact.

Transient failures are not negative-cached. A later open may retry.

### Corrupt cache artifact

Examples:

- bad `COVER.BIN` magic or version;
- invalid dimensions or stride;
- truncated payload;
- stale temporary cover file.

A corrupt generated artifact is never presented as a valid cover. It is removed or replaced when possible.

All public decoder and pipeline errors must be bounded enums suitable for logging without allocation.

## M0 — Decoder and raster pipeline spike

M0 introduces no firmware behavior change.

### Deliverables

1. A `proto` JPEG decoder satisfying the initial JPEG contract.
2. Caller-owned workspace with no allocation and no recursion.
3. Streaming scaled-luma output.
4. Shared crop/fit geometry and deterministic resampler.
5. Shared 1bpp dither.
6. Host integration capable of decoding EPUB ZIP entries.
7. Host fixtures and malformed-input tests.
8. Workspace, stack, output-quality, and runtime measurements.
9. A written go/no-go decision for M1 and M2.

### Test corpus

M0 tests include:

- all 20 JPEGs in `EIGHTY-SIX-VOLUME-1.epub`;
- synthetic grayscale JPEG;
- synthetic 4:4:4 JPEG;
- synthetic 4:2:2 JPEG;
- synthetic 4:2:0 JPEG;
- restart-marker fixture;
- 16-bit quantization-table fixture if supported;
- progressive JPEG refusal;
- SOF1 refusal;
- multi-scan refusal;
- CMYK/YCCK refusal;
- unsupported EXIF-orientation refusal;
- missing-table cases;
- malformed marker lengths;
- truncated entropy data;
- invalid Huffman tables;
- oversized dimensions and pixel counts;
- integer-overflow edge cases;
- random malformed-input and fuzz corpus.

### Measurements

M0 reports:

- caller-owned workspace bytes;
- maximum decoder stack frame from `-Zemit-stack-sizes`;
- total relevant call-chain stack;
- host runtime at 1/8, 1/4, 1/2, and 1/1 where applicable;
- expected MCU count and output-band size;
- output comparison against a trusted host decoder;
- 202×303 cover images for visual inspection;
- X4 and X3 reader-page-size images for visual inspection;
- Floyd–Steinberg versus ordered-dither comparison;
- 1/4 downsample versus any proposed 1/8-plus-upscale fast mode.

### M0 acceptance gate

M1 may proceed when:

- all supported fixtures decode correctly;
- all unsupported and malformed fixtures fail without panic;
- the complete pipeline is allocation-free;
- caller-owned decoder and raster workspace fits in the borrowable scratch budget with margin;
- stack analysis preserves the firmware stack floor;
- output quality is acceptable at 202×303;
- host and `no_std` builds produce byte-identical 1bpp output.

M2 may proceed only when full-page 1bpp output is visually useful and the projected decode latency is acceptable enough to justify hardware implementation.

Any widening of JPEG support or change of scaling/dither policy must be written into this PRD or the implementing issue before M1 lands.

## M1 — On-device cover generation

### Cover discovery

Cover discovery moves into `proto` so host and firmware use the same rules.

Order:

1. EPUB 3 manifest item with `properties="cover-image"`.
2. EPUB 2 cover metadata that resolves to a manifest item.
3. Existing conservative id/href fallback for image manifest entries containing “cover”.

Discovery returns a bounded normalized image path, not a borrowed string tied to OPF scratch.

When discovery occurs while `EpubPackage` borrows `EPUB_OPF`:

1. resolve and copy the selected path into an owned bounded buffer;
2. copy any required metadata;
3. end all package and OPF-backed borrows;
4. only then lend OPF/XHTML scratch to the decoder.

The borrow transition must be represented by Rust lifetimes and ownership rather than `unsafe` aliasing.

### Cache-independent generation

Cover generation is an `ensure_cover_cache` operation that runs after the reader has attempted to load the existing cover.

It is not part only of the full text-cache build.

Book-open sequencing is:

1. Load or build the text/page cache through the existing fast, replay, or full path.
2. Attempt to load `COVER.BIN`.
3. If a valid cover loaded, finish normally.
4. If the cover is missing or invalid, inspect the versioned negative-cover status.
5. If no applicable negative status exists, reopen or continue using the source EPUB and attempt cover generation.
6. Load the newly published cover into `ReaderStore`.
7. Regardless of cover outcome, return the book’s text cache as ready when the text path succeeded.

This guarantees that a book with an old valid `BOOK.BIN` but no cover can still gain a generated cover.

### Existing cover policy

- A valid current `COVER.BIN` is trusted.
- A valid legacy host-generated cover accepted by the existing format reader is trusted.
- M1 does not re-decode a valid cover solely to change thresholding to dithering.
- A corrupt or truncated cover is treated as absent and is eligible for regeneration.
- A stale temporary file is deleted before a new attempt.

Changing this policy later requires an explicit cover-format or generator-policy version.

### Negative result

M1 adds a small versioned negative-cover artifact, separate from `COVER.BIN`.

It records deterministic outcomes such as:

- no cover declared;
- unsupported image format;
- unsupported JPEG subtype or orientation;
- permanently invalid image;
- path rejected by resource or traversal rules.

The artifact includes a cover-policy version. Increasing decoder support or changing discovery rules invalidates the negative result and allows another attempt.

Transient I/O and publication failures are never written as negative results.

A successful `COVER.BIN` publication removes any prior negative result.

### Transactional publication

Cover generation must not write directly into the final `COVER.BIN`.

Required sequence:

1. Remove any stale temporary sibling.
2. Create a temporary cover file.
3. Write the complete header and payload.
4. Close the file.
5. Reopen and validate:
   - magic and version;
   - dimensions and stride;
   - exact payload length;
   - clean EOF after the payload.
6. Publish the completed file using the safest replace operation supported by the filesystem layer.
7. Remove the temporary file after either success or failure.

A previously valid `COVER.BIN` is preserved until the replacement has been fully written and validated.

If the filesystem API cannot provide an atomic rename, the implementation must still be transactional for ordinary decode and write errors. The implementing issue must document the remaining power-loss window and ensure the next open recognizes and cleans up any interrupted state.

### M1 failure behavior

A cover failure:

- does not delete a previously valid cover;
- does not make `BOOK.BIN` or section caches invalid;
- does not change the requested reading page;
- does not fail a successful text-cache open;
- leaves no temporary file after recoverable cleanup;
- emits a bounded diagnostic result.

### M1 observability

Firmware logs or bench output distinguish:

- `cover_hit`;
- `cover_negative_hit`;
- `cover_missing`;
- `cover_invalid`;
- `cover_generated`;
- `cover_no_declared_image`;
- `cover_unsupported`;
- `cover_decode_invalid`;
- `cover_io_error`;
- `cover_publish_error`.

A successful generation reports:

- source dimensions;
- JPEG sampling mode;
- selected DCT scale;
- output dimensions;
- encoded bytes consumed;
- decode/raster duration;
- publication duration.

### M1 acceptance criteria

M1 is complete when:

- a book with valid `BOOK.BIN` and no cover generates a cover;
- a book opened through `CONT.BIN` replay and no cover generates a cover;
- a full EPUB cache build generates a cover;
- a valid existing cover is not regenerated;
- a corrupt or truncated cover is replaced;
- a deterministic no-cover result is not retried on the next open;
- a transient I/O failure is retried on a later open;
- an injected failure at the beginning, middle, and end of cover writing leaves no valid-looking partial file;
- an existing valid cover survives a failed replacement attempt;
- host and device generation produce byte-identical output for the same source image;
- malformed or unsupported images do not prevent book opening;
- cold and warm book-open measurements are reported on X4 hardware;
- the stack and static-memory checks remain satisfied.

The initial performance target is no more than 5 seconds of added cover-generation latency for a typical 1200×1800 cover on X4, excluding panel refresh. M1 may exceed that only with a documented product decision based on measured hardware results.

## M2 — Dedicated image pages

### Initial scope

M2 promotes an image only when the complete rendered content of one EPUB spine item is a single eligible image.

Transparent structural wrappers are allowed, including:

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
- any ordinary text block before or after the image.

This rule intentionally excludes decorative ornaments embedded in chapters and avoids subjective “meaningful container” or aspect-ratio-only heuristics.

Supporting standalone image blocks inside an otherwise textual spine item requires a later scoped extension.

### Parser event model

The XHTML layer introduces a semantic event interface rather than encoding every event as text.

Conceptually:

```rust
enum ContentEvent<'a> {
    Text(TextBlock<'a>),
    ImageCandidate(ImageCandidate<'a>),
}
```

An `ImageCandidate` contains only bounded parser-level information:

- raw `src` or SVG-image href;
- alt text;
- element and structural context;
- whether any meaningful sibling content exists;
- hidden, decorative, or presentation flags known from markup;
- current XHTML document path.

The parser does not silently turn an eligible image candidate into final placeholder text before the sink can inspect it.

The build sink:

1. resolves and normalizes the href;
2. locates the manifest or ZIP entry;
3. parses only enough image metadata to identify format, dimensions, and orientation, by streaming a bounded prefix of the entry — on the order of 1 KB — through the streaming dimension probe described under "Prior art", stopping as soon as the header fields are known and never inflating the whole image at cache-build time;
4. applies the deterministic image-only-spine rule;
5. emits either:
   - a normalized image-page event; or
   - the existing placeholder text event.

The normalized event, rather than the raw parser candidate, is what `CONT.BIN` captures.

### Normalized image-page event

An image-page event contains:

- normalized EPUB-root-relative image path;
- image format;
- intrinsic width and height;
- orientation support result;
- page fit mode, initially `Contain`;
- reserved flags for future behavior.

The href is stored as bounded UTF-8 bytes. Fragment identifiers are removed before persistence.

The event contains no borrowed reference into XHTML or OPF scratch.

### `CONT.BIN` schema

M2 increments `CONTENT_VERSION`.

The content stream becomes a typed event stream with at least:

- `Text`;
- `ImagePage`;
- `SpineEnd`.

For a text event, the payload preserves the existing text, role, style, alignment, and paragraph-end semantics.

For an image-page event, the payload contains the normalized image-page metadata and href.

Replay must reproduce the same semantic event sequence without reopening the EPUB. It must not need to repeat image discovery, metadata inspection, or classification.

A partial, malformed, unknown-kind, overlength, or inconsistent event invalidates `CONT.BIN` and causes the existing full-EPUB fallback.

Host tests must prove that:

- full parse and content replay produce identical page-cache bytes;
- image events survive a type-settings change;
- corrupt image-event framing cannot be replayed as text;
- old `CONT.BIN` versions are rejected.

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

    // Text page:
    first_block: u16,
    block_count: u16,

    // Image page:
    image_href_offset: u32,
    image_href_len: u16,
}
```

For a text page:

- `first_block` and `block_count` are valid;
- image fields are zero.

For an image page:

- text block fields are zero;
- image href fields reference bytes in the section payload/string area;
- the page has exactly one image reference.

The exact packed layout may differ, but these invariants are required.

Section headers must distinguish text bytes from general payload bytes, or document that image hrefs share the existing bounded string payload.

### Cache invalidation

M2 changes both parsing semantics and page-record layout.

Therefore M2 must:

- increment `CONTENT_VERSION`;
- increment `CACHE_V2_VERSION`;
- reject older page and section records for the new reader path;
- not leave the previous cache version inside the compatibility window if doing so could load text-only pages without image metadata;
- rebuild old caches from the source EPUB;
- delete or ignore stale sections after a failed rebuild according to the existing cache-publication rules.

A firmware update must not continue using an old valid `BOOK.BIN` in a way that permanently hides newly supported image pages.

M1 by itself does not require this text/page-cache version bump.

### Render-time source access

Rendering an image page introduces an EPUB read into the page-render path.

The display task:

1. obtains the normalized image href from the loaded page record;
2. opens the source EPUB;
3. locates the matching ZIP entry;
4. streams and inflates it;
5. decodes and rasterizes it into the reader content rectangle;
6. requests a Full refresh.

The page cache stores a stable normalized href rather than a ZIP local-header offset. Offsets may become an optimization later but are not part of the initial persistence contract.

If the EPUB is missing, changed, unreadable, or lacks the referenced entry, the page falls back to the placeholder.

### Rendered-page cache (deferred decision)

Without a cache, every visit to an image page pays the full source-open, inflate, decode, and raster cost again. Paging back and forth across an illustration repeats it each time.

The option is to persist the finished 1bpp raster for the image page and, on a later visit, read it back instead of re-decoding. At the reader content rectangle this is roughly 43 KB per image page in 1bpp, so a book with seventeen full-page illustrations costs under 1 MB of SD space. A revisit becomes a single sequential read.

`crosspoint-reader` treats the equivalent cache as mandatory, because its renderer re-enters the image path about 14 times per page; without it a 2-second decode became a roughly 30-second freeze and a watchdog reset. That pressure does not exist here — a render writes the framebuffer once — so for this design the cache is a latency optimisation, not a correctness requirement.

It is therefore deferred to measured evidence rather than specified now. If it is adopted it must reuse the M1 transactional-publication rules, carry its own version, be invalidated by any change to geometry, resampler, or dither, and never be presented as valid when truncated.

### M2 failure behavior

A decode or source failure on an image page:

- renders the existing placeholder in place of the image;
- does not fail the page turn, invalidate the page cache, or change reader position;
- leaves no partially decoded pixels in the submitted framebuffer;
- emits a bounded diagnostic result.

A failed image is recorded in a bounded in-RAM memo — a fixed-size table of image-path hashes — so the same image does not re-attempt a full decode on every visit within the reading session. The memo:

- is fixed-capacity and allocation-free, and simply stops recording when full;
- is cleared when the reader is entered, so transient SD or timing failures are retried in a later session;
- is never persisted, because unlike the M1 negative-cover result it does not distinguish deterministic from transient causes.

Deterministic per-image refusals are not persisted in M2. The image-page record already carries the format and dimensions established at cache-build time, so an unsupported image is normally rejected before a decode is attempted.

### Framebuffer transaction

The panel must never display a partially decoded image.

The framebuffer sequence is:

1. prepare the complete page background and fixed chrome;
2. clear the image content rectangle to white;
3. decode into the framebuffer;
4. on success, finalize the image page and submit it;
5. on any decode or source error:
   - clear the image rectangle again;
   - draw the existing placeholder;
   - submit only the complete fallback page.

No panel refresh begins until decode has either succeeded or the fallback framebuffer has been reconstructed.

A successful image page forces Full refresh. A failed image-page attempt may also use Full refresh when required by the planner or golden-output policy, but no partially decoded pixels may survive in the submitted framebuffer.

### M2 observability

Image-page telemetry distinguishes:

- image page loaded from cache;
- EPUB source missing;
- ZIP entry missing;
- unsupported format;
- unsupported JPEG subtype;
- invalid JPEG;
- resource-limit rejection;
- decode success;
- decode failure;
- placeholder fallback.

Success reports:

- global and section page;
- source and target dimensions;
- JPEG sampling;
- DCT scale;
- compressed and uncompressed bytes;
- source-open time;
- inflate/decode/raster time;
- total press-to-frame-ready time;
- refresh class.

### M2 acceptance criteria

M2 is complete when:

- an image-only spine item becomes exactly one image page;
- an image-only spine item remains an image page after type-settings replay from `CONT.BIN`;
- full parse and replay generate byte-identical page caches;
- decorative `alt=""` images inside prose remain placeholders;
- a chapter containing text plus an image remains text plus placeholder;
- an image plus caption is not promoted;
- a spine item with two images is not promoted;
- overlength, absolute, and root-escaping hrefs are rejected;
- a missing EPUB or ZIP entry renders a placeholder;
- malformed, progressive, oversized, or unsupported JPEGs render a placeholder;
- failures injected after partial framebuffer writes leave no image remnants;
- successful image pages force Full refresh;
- old content and page caches are rebuilt rather than silently accepted;
- emulator goldens pass on X4 and X3;
- X4 hardware page-turn and stack measurements are reported.

The initial performance target is no more than 5 seconds from image-page render start to a completed framebuffer for a typical 1200×1800 illustration, excluding the panel’s Full refresh busy time. Shipping above that target requires an explicit product decision and should consider a rendering plate.

## SVG-wrapped images

Arbitrary SVG rasterization remains out of scope.

M2 may later recognize an image-only spine item whose SVG contains exactly one external raster `<image>` reference, but only as a transparent wrapper around a supported JPEG.

That extension must:

- ignore SVG `<title>` and `<desc>` as reader prose;
- avoid affecting EPUB navigation labels;
- resolve `href` and `xlink:href` safely;
- reject transforms, clipping, multiple images, embedded data URLs, or vector drawing that would change the raster’s presentation;
- feed the resolved raster href through the same normalized image-page pipeline.

It is not required for the initial M2 milestone.

A placeholder-only fix must not remove `svg` from parser skip lists unless it implements the complete intended SVG semantics.

## Scratch-memory and lifetime plan

### No new large statics

The decoder state and raster workspace are borrowed from existing reader-cache scratch.

The implementation must not add:

- a full decoded image buffer;
- a second framebuffer;
- a decoder singleton static;
- a large task-local stack array.

### Required lifetime sequence

For cover generation during a full EPUB open:

1. parse the OPF and construct `EpubPackage`;
2. discover and normalize the cover path;
3. copy it into a bounded owned value;
4. complete any text-cache work that still borrows package data;
5. drop `EpubPackage`, manifest views, spine views, and other OPF-backed references;
6. borrow OPF/XHTML storage as decoder workspace;
7. retain the ZIP inflate state and compressed-input buffer separately because they remain concurrently live;
8. decode and publish the cover.

For cover generation after a `BOOK.BIN` or `CONT.BIN` hit:

1. complete the text-cache open;
2. parse only the package information needed for cover discovery;
3. copy and normalize the cover path;
4. drop package borrows;
5. reuse the scratch for decode.

For M2 rendering, no content parser or package object may remain live while the same scratch is lent to the decoder.

The borrow checker must enforce exclusivity. This work must not introduce `unsafe` scratch aliasing.

### Working-set budget

The M0 report must replace estimates with exact sizes.

Expected caller-owned decoder/raster state includes:

- Huffman tables and lookup acceleration;
- quantization tables;
- component and scan metadata;
- coefficient/IDCT workspace;
- one scaled MCU or output band;
- resampler rows;
- two dither error rows.

The expected total is approximately 10–18 KB, excluding the existing ZIP inflate state and compressed-input buffer.

The implementation must retain at least 8 KB of margin within the scratch actually lent to it. If it cannot, the design returns to the M0 gate.

### Stack

Requirements:

- no recursion;
- no large local arrays;
- no data-dependent stack growth;
- decoder and raster buffers supplied by the caller;
- `_stack_start - _stack_end >= 27 KB` remains true;
- `-Zemit-stack-sizes` output is reported for the complete affected call chains.

Any regression near the existing stack floor blocks M1 or M2 until resolved.

## Refresh policy

- Cover generation itself performs no panel refresh.
- Existing views display the loaded cover through their current refresh policy.
- A successfully rendered full-page image requests Full refresh.
- Placeholder fallback must remain visually correct under the selected refresh.
- M2 benchmarks report decode time separately from panel busy time.
- M2 must not block input ownership or violate the display task’s existing single-writer assumptions.

A temporary “Rendering…” plate is not part of the initial implementation. It becomes required for product review if measured framebuffer preparation routinely exceeds 2 seconds and the current screen otherwise appears unresponsive.

## Robustness and security

All image and cache parsing treats input as untrusted.

Required protections:

- checked integer arithmetic;
- bounds checks before every table, buffer, and output access;
- bounded loops derived from validated dimensions and MCU counts;
- no panics on malformed input;
- no unchecked UTF-8;
- no path traversal;
- no recursive parsing;
- no unbounded marker skipping;
- no allocation based on encoded dimensions;
- clean cancellation on source read failure;
- exact cache payload-length validation;
- version validation before interpreting record layouts;
- fuzz coverage for decoder and cache record parsing.

A failed image operation must not corrupt:

- reader position;
- section-cache state;
- book metadata;
- cover state;
- the source EPUB;
- any previously valid cache artifact.

## Verification plan

Every implementing issue restates the commands relevant to its scope.

Required repository checks:

- `tools/check.sh fmt`;
- `tools/check.sh fast` for `proto` decoder and cache work;
- `tools/check.sh emulator` on X4 and X3 for changed rendering;
- `tools/check.sh firmware` for display-task, SD, scratch, or cache-publication changes;
- `tools/check.sh all` before a PR is merge-ready.

Required host tests:

- supported JPEG conformance fixtures;
- unsupported-format refusal;
- truncated and malformed streams;
- deterministic resampler output;
- deterministic dither output;
- host/device-equivalent cover output;
- content-event encode/decode;
- full-parse versus replay equivalence;
- old-version rejection;
- path normalization and traversal rejection;
- injected write and read failures.

Required hardware results:

- M1 cold open with missing cover;
- M1 warm open with cached cover;
- M1 retry after transient failure;
- M2 first render of an image page;
- M2 repeat render of the same image page;
- X4 page-turn latency;
- stack-size delta;
- firmware image-size delta;
- SD bytes and read duration where measurable.

Golden output changes caused by the shared dither must land with the host-tool change that produces them.

## Build sequence

### M0 — Host decoder and raster pipeline

1. Define supported JPEG and error enums.
2. Implement marker, table, scan, and entropy parsing.
3. Implement scaled luma decode.
4. Implement deterministic crop/fit geometry and resampling.
5. Implement deterministic 1bpp dithering.
6. Add synthetic conformance and malformed-input fixtures.
7. Decode the 20-image EPUB corpus.
8. Integrate the shared pipeline into `tools/preview`.
9. Measure workspace, stack, runtime, and output quality.
10. Close the M0 decision gate separately for M1 and M2.

### M1 — On-device cover generation

11. Move cover discovery and normalized href resolution into `proto`.
12. Add versioned negative-cover status.
13. Add temporary-file validation and publication helpers.
14. Add cache-independent `ensure_cover_cache`.
15. Wire it after all three text-cache paths.
16. Load a newly generated cover into `ReaderStore`.
17. Add failure-injection and legacy-cover tests.
18. Run X4 hardware open benchmarks.
19. Land host/device golden-output updates together.

### M2 — Dedicated image pages

20. Add parser-level image candidates.
21. Add deterministic image-only-spine classification.
22. Add normalized image-page content events.
23. Bump and implement the typed `CONT.BIN` format.
24. Add page kind and image reference to section/page caches.
25. Bump cache versions and force old-cache rebuild.
26. Add render-time EPUB image access.
27. Add transactional framebuffer fallback.
28. Force Full refresh for successful image pages.
29. Add X4 and X3 emulator goldens.
30. Run X4 hardware page-turn benchmarks.
31. Decide whether a rendering plate is required.

## Risks

### Decoder performance is slower than expected

Mitigation:

- M0 before firmware integration;
- use the largest valid DCT reduction;
- keep luma-only output;
- reject unsupported complexity early;
- stop after M1 if full-page rendering is not worthwhile.

### Stack regression

Mitigation:

- caller-owned state;
- no recursion;
- stack-size reporting in every milestone;
- preserve the build-time stack floor.

### Scratch aliasing

Mitigation:

- copy normalized paths out of borrowed package data;
- drop package/parser borrows before lending scratch;
- route workspace through `ReaderCacheScratch`;
- no `unsafe` aliasing.

### Poor 1bpp visual quality

Mitigation:

- real corpus inspection in M0;
- shared deterministic dither;
- separate cover fill and page contain geometry;
- accept M1 without M2.

### Repeated cover-generation cost

Mitigation:

- valid cover cache;
- versioned deterministic negative result;
- transient-only retry behavior.

### Corrupt partial cache files

Mitigation:

- temporary artifact;
- close and read-back validation;
- preserve existing valid output;
- cleanup and recovery tests.

### M2 cache inconsistency

Mitigation:

- typed semantic events;
- image metadata persisted in `CONT.BIN`;
- explicit page kind;
- mandatory cache-version bump;
- full-parse/replay byte-equivalence tests.

### Decorative images become full pages

Mitigation:

- initial M2 promotes only an entire image-only spine item;
- no aspect-ratio-only promotion;
- no image-plus-text promotion;
- fixture coverage for ornaments and captions.

### Unpredictable image-page latency

Mitigation:

- separate source-open, inflate, decode, raster, and panel timing;
- Full refresh already expected for image pages;
- explicit 5-second framebuffer-preparation target;
- rendering plate decision based on measured hardware data.

## Decisions closed by this PRD

- M1 generation runs independently of the text-cache path.
- Existing valid cover files are trusted.
- Missing and corrupt covers are eligible for generation.
- Deterministic negative results are persisted; transient failures are retried.
- Cover publication uses a temporary validated artifact.
- The default cover DCT scale may not undershoot the target.
- Covers use centered fill-and-crop.
- Reader image pages use contain with white padding.
- The first JPEG contract is SOF0-only rather than SOF0/SOF1.
- M2 uses typed parser and content-cache events.
- M2 adds an explicit image page kind and persisted href.
- M2 bumps content and page-cache versions.
- Initial M2 promotion is limited to image-only spine items.
- Partial framebuffer output is cleared before placeholder fallback.
- Host and firmware use the same raster pipeline.
- The decoder is written in `no_std` Rust rather than wrapping JPEGDEC.
- Images stream from the ZIP entry rather than being extracted to SD first.
- Downscaling uses area averaging, not nearest neighbour, with per-axis scale factors.
- Error diffusion is conditional on single-pass framebuffer rendering.

## Remaining decision gates

The following are intentionally deferred to measured evidence:

- Floyd–Steinberg versus ordered dithering, subject to the single-pass precondition.
- Exact decoder workspace after implementation.
- Whether 16-bit quantization tables fit the initial support set.
- Whether a 1/8-plus-upscale cover fast mode is visually acceptable.
- Whether M2 should proceed after full-page 1bpp inspection.
- Whether M2 requires a rendering plate.
- Whether M2 adopts a rendered-page cache, based on measured revisit latency.
- Whether a later milestone adds progressive JPEG as a DC-only 1/8 decode.
- Whether X3 eventually needs a different cover-cache geometry.
- Whether a later milestone should support transparent SVG wrappers.
- Whether a later milestone should promote standalone image blocks inside textual spine items.
