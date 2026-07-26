# Image Rendering and Hotspot Preprocessing — PRD

Status: **Planning. Product behavior, source lifecycle, persistent commit semantics, upload and deletion behavior, render-bundle architecture, cache representation, recovery behavior, artifact invalidation rules, bounded cache behavior, and end-to-end performance requirements are closed except where explicitly assigned to a measurement or decoder-selection gate.**

Date: 2026-07-26

## Summary

CalendulaOS can load a legacy pre-rasterized 1bpp cover, but it does not currently draw that bitmap in the Home view, generate cover rasters on the device, or render EPUB illustrations as image pages.

This project adds two image-production paths:

1. **Hotspot-assisted preprocessing**
   - The browser parses an EPUB during upload.
   - It creates a cover and eligible image-page rasters.
   - It uploads a versioned render bundle alongside the unchanged EPUB.

2. **On-device fallback**
   - Books copied directly to SD, legacy uploads, and books with missing or invalid render bundles are decoded from supported source JPEGs on the device.
   - Device-produced covers are persisted.
   - Device-produced image pages initially remain session-only.
   - A later milestone may persist a bounded set of Device-produced image pages without rewriting every retained page for every newly cached page.

Both decoded-image paths share:

- EPUB semantics;
- normalized paths;
- cover and image-page classification;
- bounded semantic-finalization behavior;
- canonical post-decode luma conversion;
- output geometry;
- resampling;
- dithering;
- raster serialization;
- artifact validation;
- runtime precedence.

Host and Device source decoders remain independently versioned. A Host decoder update does not unnecessarily invalidate Device-decoded artifacts, and a Device decoder update does not unnecessarily invalidate Host artifacts.

Persistent authority is established only through a storage primitive whose durability contract is explicitly defined by this PRD. A checksum-valid record is not committed merely because firmware can reopen and reread it. A prepared record whose commit marker is absent, malformed, torn, inconsistent, or not durably published is never selected after reboot.

Source validation is required before source-bound persistent state is trusted, but validation must not silently destroy the existing fast cached-open experience. This PRD therefore distinguishes:

- background or deferred mount-session validation;
- operations that require validated source bytes before proceeding;
- cached reading that may temporarily use previously validated state under the rules defined later in this document;
- end-to-end first-open performance, including any required source hashing.

The project is delivered through these milestones:

- **M0S — source transaction foundation:** logical-book operations, storage mutation serialization, upload and deletion idempotency, source identity, source-generation authority, durable prepared/committed metadata publication, deletion tombstones, source-integrity validation, cleanup, recovery, and hardware power-cut verification.
- **M0R — render artifact foundation:** shared EPUB image semantics, policy versions, capabilities, render formats, Host preprocessing, bounded artifact validation, durable prepared/committed bundle publication, resident-cover representation, and legacy-raster provenance.
- **M0D — on-device decoder selection and verification:** evaluate `tjpgdec-rs`, implement or adapt the selected baseline JPEG decoder, define a bounded tile-delivery contract, and establish differential, fuzz, streaming, resource, and memory-safety verification.
- **M1A — hotspot-preprocessed covers:** upload an EPUB and Host cover artifact, validate and commit the bundle, load the active-profile cover, and visibly render it in the Home view.
- **M1B — on-device cover fallback:** generate and publish a cover-only Device bundle when no valid Host or Device cover exists.
- **M2A — hotspot-preprocessed image pages:** identify image-only spine items, persist typed image-page events, upload their rasters, and render them while reading.
- **M2B — on-device image-page fallback:** reopen the source EPUB and decode supported JPEG pages when no valid preprocessed asset exists.
- **M3 — bounded persistent Device image pages:** retain a bounded set of Device-produced page rasters using a publication design with explicit retention, eviction, free-space, write-amplification, and worst-case latency limits.
- **Later candidate — progressive JPEG thumbnails:** consider bounded progressive support only after baseline support has shipped and representative corpus data justifies it.

M1A does not depend on the embedded JPEG decoder.

M2A may render browser-preprocessed PNG and progressive-JPEG image pages before firmware supports those source encodings.

Stopping after M1 is acceptable if full-page 1bpp images are too slow or visually poor on hardware.

## Existing behavior

### Cover path

The repository already contains:

- a legacy cover-cache format and validator;
- a fixed-capacity resident cover buffer in `ReaderStore`;
- a `UiCover` model;
- a path that supplies the active book’s cover to the Home view model;
- host-side cover decoding in `tools/preview`.

The legacy `COVER.BIN` uses an X4-specific fixed-geometry format identified as `X4CV`.

The cover bitmap is not currently drawn by `render_home` or `render_library`.

This project introduces a board-neutral `XTCV` format whose header carries:

- dimensions;
- stride;
- pixel format;
- producer;
- production method;
- applicable policy versions;
- payload length;
- payload checksum.

M1A includes the missing Home-view bitmap rendering.

Library thumbnails are not part of the initial feature.

### Reading path

The current XHTML parser emits text blocks through `XhtmlBlockSink::push_block`.

An `<img>` becomes italic, centered placeholder text using:

- its `alt` value; or
- `"[Image]"`.

The current content cache stores a flat stream of text records.

The current page cache identifies only ranges of text blocks.

Neither format can represent an image page or retain an EPUB image path.

M2 therefore requires:

- semantic parser events;
- order-preserving deferred image classification;
- bounded image probing;
- deterministic capacity behavior;
- a typed content-cache stream;
- an explicit page kind;
- a section-local image table;
- cache-version invalidation;
- render-bundle lookup;
- source-decode fallback.

## User-visible behavior

### Covers after M1A

- A book uploaded through the hotspot may include a preprocessed cover.
- The original EPUB remains byte-for-byte unchanged.
- Firmware validates the cover against:
  - the authoritative logical book and source generation;
  - exact source length and SHA-256 when the operation requires validated source authority;
  - active board profile;
  - production method;
  - all applicable policy versions;
  - manifest integrity;
  - payload integrity;
  - active-profile cover geometry.
- A valid cover is loaded and visibly drawn in the Home view.
- An interrupted or rejected render-bundle upload does not cause the EPUB upload to fail.
- Missing covers leave the designated Home cover area blank.
- A prepared but uncommitted render manifest is never selected.
- Source validation required for artifact installation may run cooperatively, but the device must not report the artifact as installed before validation and durable publication complete.

### Covers after M1B

- A book copied directly to SD, uploaded without preprocessing, or paired with a missing or invalid bundle can generate its cover on-device.
- Generation works whether book text initially opened through `BOOK.BIN`, `CONT.BIN`, or a full source parse.
- Source decoding does not begin until the exact source generation required by the decode has been validated.
- The resulting cover is published as a cover-only Device bundle for the active board profile.
- A valid Host bundle remains preferred.
- A malformed, unsupported, missing, or oversized cover never prevents the book text from opening.
- Deterministic failures may be persisted with complete source and policy identity.
- Transient failures remain retryable.
- A raw legacy `X4CV` cover is never treated as an ordinary modern Device-decoded artifact.

### Image pages after M2A

- A spine item whose complete rendered body is one eligible JPEG or PNG image becomes one image page.
- Eligibility is determined by the same bounded semantic-finalization algorithm used by firmware.
- Browser target enumeration must use the exact semantic limits and behavior advertised by the device.
- A valid hotspot-preprocessed raster is rendered even when firmware cannot decode the source encoding.
- Progressive JPEG and PNG may render through Host artifacts before firmware supports them.
- The full image is contained within the reader content rectangle.
- The image is centered with white padding.
- Image pages use a Full e-ink refresh.
- Decorative images, inline images, captions, and images mixed with prose remain placeholders.
- Placeholder order remains identical to source-event order when an image candidate is later disqualified.
- Exceeding a semantic cache or fallback-table capacity must not independently change Host and Device pagination. Capacity behavior is part of the semantic policy and is applied identically before Host target enumeration and firmware cache publication.

### Image pages after M2B

- When no valid raster artifact exists, firmware attempts source decoding for formats in its supported Device JPEG contract.
- Unsupported source plus no valid artifact produces the placeholder.
- Malformed images do not abort the book or corrupt the page cache.
- Deterministic failures are suppressed for the rest of the current reading session.
- Transient SD and source-access failures may be retried.
- No partially decoded image reaches the panel.
- Source decoding does not begin until the source generation required by the image event has been validated for the current authority epoch.

### Image pages after M3

- Successfully decoded image pages may be persisted in a bounded Device page cache.
- Persisting a page does not overwrite or discard Host assets.
- A failed Device publication leaves the previous committed Device state and all Host generations intact.
- The persistent Device page cache has:
  - a compile-time or capability-advertised maximum retained byte count;
  - a maximum retained page count;
  - deterministic eviction;
  - a minimum-free-space admission rule;
  - a bounded publication cost;
  - a bounded write-amplification target.
- Adding one page must not require copying all retained page payload bytes into a new monolithic generation.
- M3 does not ship until its storage layout passes the write-amplification and worst-case latency gates defined later in this PRD.

## Goals

- Preserve every uploaded EPUB byte-for-byte.
- Preprocess all images that the current reader can display:
  - the selected cover;
  - images belonging to eligible JPEG or PNG image-only spine items.
- Allow hotspot preprocessing to deliver visible images before the on-device decoder is complete.
- Provide on-device fallback for direct-SD, legacy, missing-artifact, and stale-artifact cases.
- Use one shared definition of:
  - cover discovery;
  - path normalization;
  - image-page eligibility;
  - bounded semantic-finalization behavior;
  - canonical post-decode luma conversion;
  - geometry;
  - resampling;
  - dithering;
  - raster serialization;
  - artifact validation.
- Keep firmware image logic `no_std`, allocation-free, bounded, and panic-free for external input.
- Use forward-only ZIP-entry reads for on-device JPEG decoding.
- Avoid extracting complete images to temporary plain files solely to satisfy a decoder.
- Prohibit buffering a complete decoded source image in firmware.
- Keep existing EPUB inflate scratch available concurrently with decoder and raster state.
- Preserve deterministic and order-equivalent content-cache replay.
- Publish uploaded books, source metadata, deletion state, image artifacts, and persistent negative results transactionally.
- Define the actual storage durability primitive used by every persistent commit.
- Make the persistent commit point observable and recoverable after power loss.
- Prevent prepared but uncommitted records from becoming authoritative after reboot.
- Prevent interrupted managed uploads from appearing as unmanaged books.
- Define explicit create, replace, list, delete, and externally-modified-source recovery operations for logical books.
- Make create, replace, and delete operations safe to retry after an unknown HTTP outcome.
- Persist display labels atomically with their authoritative source generation.
- Prevent a deleted logical book from reappearing after interrupted cleanup.
- Serialize source and artifact mutations through one storage owner.
- Abort publications whose source authority changes while data is being prepared.
- Define one authoritative source generation for every managed logical book.
- Detect external replacement of any SD-resident EPUB before trusting source-bound persistent state.
- Never treat `ManagedUpload` provenance as proof that removable storage remained immutable.
- Publish unmanaged-source metadata without overwriting its only committed identity record.
- Bind every render bundle, cache, and negative result to the exact source EPUB and applicable policies.
- Preserve board-specific Host and Device artifacts when an SD card moves between X3 and X4.
- Prevent Host and Device artifacts from overwriting one another.
- Version common luma behavior independently from Host and Device source decoders.
- Preserve legacy covers only through explicit legacy-raster provenance.
- Make an adapted or agent-ported decoder verifiable through executable tests rather than textual translation.
- Report exact memory, stack, code-size, latency, validation-I/O, write-amplification, storage-wear, and output-quality results before firmware rollout.
- Preserve a measurable fast cached-open experience.
- Define end-to-end first-open targets that include any source validation required before reading or rendering.
- Ensure browser preprocessing never applies Device-only source-decoder limits to images that the Host decoder can safely reduce to an accepted output raster.
- Require transparency-preserving decoder output whenever source pixels are not fully opaque.

## Non-goals

- Rewriting or optimizing the source EPUB.
- Inline image layout.
- CSS image sizing, floats, or text wrapping.
- Captions associated with image pages.
- Promoting images from otherwise textual spine items.
- General SVG rasterization.
- Rendering arbitrary SVG transforms, clipping, or vector drawing.
- Host image-page formats other than JPEG and PNG in the initial M2A contract.
- PNG decoding in the initial firmware decoder.
- Progressive JPEG in the initial firmware decoder.
- Arithmetic-coded, lossless, 12-bit, CMYK, or YCCK JPEG in the initial firmware decoder.
- Background image decoding or image prefetching.
- Multi-level grayscale panel rendering.
- Library-view cover thumbnails.
- Matching JPEGDEC’s complete API.
- Treating a textual port of any decoder as sufficient evidence of correctness.
- Preserving compatibility with old M2 content or page-cache layouts.
- Persisting Device-decoded image-page rasters before M3.
- Uploading artifacts for a non-active board profile in protocol version 1.
- Restoring a deleted logical-book identity under the same logical-book ID.
- Treating a raw legacy `X4CV` cover as an ordinary decoded-source artifact.
- Silently adopting an externally modified managed source as a replacement generation.
- Supporting source orientations other than absent or orientation 1 in the initial Device decoder.
- Claiming that filesystem close or immediate reread proves power-loss durability without a defined block-device flush contract.
- An unbounded or copy-forward-on-every-page persistent Device page cache.
- Using Device JPEG workspace limits as Host-browser source-decode limits.

## Architectural overview

### Hotspot-assisted path

```text
Browser receives EPUB
  -> fetch active-profile device capabilities
  -> calculate source SHA-256
  -> generate upload request ID
  -> parse container, OPF, spine, and XHTML
  -> apply advertised semantic-policy limits and finalization behavior
  -> discover cover
  -> classify eligible JPEG and PNG image-only spine items
  -> decode source images with pinned Host/Wasm decoder
  -> preserve effective transparency in decoder output
  -> normalize opaque output to upright Gray8
     or transparent output to upright non-premultiplied RGBA8
  -> apply canonical luma policy
  -> apply shared geometry and resampling
  -> apply shared 1bpp dither
  -> validate output rasters against Host-output limits
  -> create or replace logical book through explicit HTTP operation
  -> device serializes mutation through storage owner
  -> device writes managed-upload staging marker
  -> device stages EPUB and prepared source metadata
  -> device validates prepared metadata
  -> device revalidates authority immediately before source commit
  -> device durably publishes source-metadata commit marker
  -> device validates committed source and returns token and identity
  -> browser creates source-bound Host render bundle
  -> browser uploads one framed bundle request for active profile
  -> device acquires exact-source mutation lease
  -> device writes inactive Host payload files
  -> device validates payloads and manifest template
  -> device constructs prepared final manifest
  -> device revalidates authority and policies
  -> device durably publishes manifest commit marker
  -> device validates the committed generation
  -> firmware loads and blits validated rasters
```

### Raw-source fallback path

```text
EPUB has no valid matching artifact
  -> select authoritative, non-deleted source generation
  -> establish required source-validation state
  -> if validation is required and incomplete, validate cooperatively
  -> locate source image in EPUB
  -> stream DEFLATE entry through ZipStream
  -> probe JPEG metadata against Device decoder limits
  -> choose a supported reduced-IDCT scale
  -> decode supported baseline JPEG as bounded ordered tiles
  -> convert each tile to canonical luma
  -> stream tiles through bounded scaler and dither state
  -> write only the destination raster or framebuffer
  -> for a cover, publish a prepared cover-only Device bundle
  -> revalidate source authority
  -> durably publish bundle commit marker
  -> for a page, optionally retain through the bounded M3 cache
```

### Runtime precedence

For a cover or image page, firmware resolves assets in this order:

1. Valid **Host-decoded** bundle asset for the exact source generation and active board profile.
2. Valid **Device-decoded** bundle asset for the exact source generation and active board profile.
3. Valid **legacy-raster-import** cover in the Device namespace, for covers only.
4. On-device source decode when:
   - the source format is supported;
   - required source validation is complete;
   - source authority still matches the event or cover request.
5. Blank cover area, image placeholder, or applicable deterministic negative result.

A raw legacy cover is not drawn directly under the modern architecture. It must first be imported into a source-bound Device bundle with explicit legacy provenance.

Each producer has independent generation pairs for X3 and X4.

Publication for one profile cannot evict another profile’s artifacts.

Device production never modifies or replaces Host bundles.

## Shared semantic layer

The following logic is implemented once in Rust and reused by firmware, host tools, and hotspot Wasm code:

- EPUB container and OPF path handling;
- cover discovery;
- URL-fragment removal;
- normalized EPUB-root-relative paths;
- root-escape rejection;
- image-only-spine classification;
- order-preserving placeholder handling;
- bounded deferred-candidate finalization;
- bounded fallback-record behavior;
- deterministic capacity-exhaustion behavior;
- render-target enumeration;
- image metadata probing;
- canonical post-decode luma conversion;
- cover and page geometry;
- resampling;
- dithering;
- raster serialization;
- bundle encoding and validation;
- typed content-cache encoding;
- section and page-cache encoding.

The browser may use an allocating source-image decoder.

Firmware may not allocate based on external image dimensions and may not retain a complete decoded image.

Shared code must distinguish:

- semantic limits that affect target classification or pagination;
- Host source-decoder limits;
- Device source-decoder limits;
- artifact-output and upload limits.

Only semantic limits may affect whether an XHTML event becomes an `ImagePage` or placeholder.

A Device decoder workspace limit must not change Host target enumeration.

## Artifact-policy versions

Persistent decoded-image artifacts use independently versioned policy dimensions.

### Semantic policy version

`semantic_policy_version` covers:

- cover discovery precedence;
- image-only-spine classification;
- transparent-wrapper rules;
- normalized-path semantics;
- URL-fragment handling;
- root-escape handling;
- image-probe interpretation;
- placeholder-versus-image-page decisions;
- deferred-candidate limits;
- fallback-record limits;
- path-table limits that can affect classification;
- deterministic behavior when those limits are exceeded;
- render-target enumeration order;
- event-stream finalization behavior.

A semantic-policy mismatch makes the affected decoded artifact or semantic cache ineligible.

All constants capable of changing the finalized semantic event stream are part of the semantic policy identity. They must either:

- be fixed by the semantic policy version; or
- be explicitly advertised in capabilities and included in the effective semantic-policy identity used by the Host and firmware.

Running out of room in an optional persistent cache must not silently change the live semantic event stream. When a cache cannot encode an event stream that was already semantically finalized, firmware must preserve the same live parse behavior and decline or truncate cache publication according to an explicitly versioned all-or-nothing or prefix-safe rule.

### Producer decoder policy version

`producer_decoder_policy_version` is interpreted together with:

- `producer`;
- `production_method`.

For `production_method = HostDecoded`, it covers:

- pinned Wasm decoder implementation and revision;
- supported Host source formats;
- JPEG marker and entropy interpretation;
- JPEG IDCT behavior;
- JPEG YCbCr-to-RGB conversion;
- PNG sample decoding;
- PNG palette and `tRNS` handling;
- grayscale-plus-alpha handling;
- source color-space handling;
- ICC-profile interpretation;
- PNG gamma interpretation;
- EXIF parsing;
- EXIF-orientation normalization;
- source-specific conversion and rounding before canonical output;
- rules for choosing Gray8 versus RGBA8 decoder output.

For `production_method = DeviceDecoded`, it covers:

- firmware JPEG decoder implementation and revision;
- supported JPEG subset;
- JPEG marker and entropy interpretation;
- IDCT and reduced-IDCT behavior;
- sampling and component handling;
- YCbCr or grayscale reconstruction;
- EXIF parsing;
- supported orientation handling;
- source-to-Gray8 or source-to-RGBA8 behavior;
- tile-delivery ordering and rounding.

The producer decoder must not apply:

- final alpha compositing over white;
- final RGB-to-luma conversion;
- final luma clamping.

Those belong only to the canonical luma policy.

A producer-decoder-policy mismatch invalidates decoded artifacts from that producer only.

For `production_method = LegacyRasterImport`, `producer_decoder_policy_version` must be zero and is not applicable.

Capabilities expose:

```text
host_producer_decoder_policy_version
device_producer_decoder_policy_version
```

### Canonical luma policy version

`canonical_luma_policy_version` operates only on a decoded producer’s upright output.

It covers:

- opaque `Gray8` pass-through;
- non-premultiplied RGBA alpha compositing over white;
- RGB-to-luma coefficients;
- integer rounding;
- clamping to 0–255.

A producer may emit `Gray8` only when every decoded source pixel is known to be fully opaque.

A producer must emit non-premultiplied `RGBA8` whenever the source has effective transparency, including:

- PNG alpha channels;
- grayscale-plus-alpha;
- palette alpha;
- `tRNS`;
- any other supported source mechanism that can make a decoded pixel nonopaque.

Discarding transparency before the canonical luma stage is a producer-decoder error.

The canonical luma policy does not cover:

- source parsing;
- ICC interpretation;
- gamma interpretation;
- EXIF parsing;
- orientation normalization;
- JPEG IDCT;
- YCbCr conversion;
- PNG sample decoding.

A canonical-luma-policy mismatch invalidates both Host-decoded and Device-decoded artifacts.

For `LegacyRasterImport`, this field must be zero because no canonical-luma operation was performed during import.

### Geometry policy version

`geometry_policy_version` covers:

- cover fill-and-crop behavior;
- image-page contain behavior;
- centering;
- padding;
- destination-rectangle interpretation;
- coordinate rounding.

For exact-copy `LegacyRasterImport`, this field must be zero. Legacy migration is permitted only when the existing raster already exactly matches the active-profile target geometry.

### Resampler policy version

`resampler_policy_version` covers:

- shrink algorithm;
- enlargement algorithm;
- filter weights;
- fixed-point representation;
- edge handling;
- integer rounding.

For exact-copy `LegacyRasterImport`, this field must be zero.

### Dither policy version

`dither_policy_version` covers:

- dither algorithm;
- traversal order;
- error distribution;
- clamping;
- thresholding;
- packed-bit polarity.

For exact-copy `LegacyRasterImport`, this field must be zero.

### Legacy-import policy version

`legacy_import_policy_version` applies only when:

```text
production_method = LegacyRasterImport
```

It covers:

- accepted legacy magic and schema;
- required legacy geometry;
- required stride and bit polarity;
- source-slot association rules;
- source-integrity prerequisites;
- exact-copy behavior;
- legacy payload validation;
- modern wrapper serialization.

For decoded artifacts, this field must be zero.

A legacy-import-policy mismatch invalidates only legacy-import artifacts.

### Policy applicability

The runtime validator applies this table:

| Production method | Semantic | Producer decoder | Canonical luma | Geometry | Resampler | Dither | Legacy import |
|---|---:|---:|---:|---:|---:|---:|---:|
| `HostDecoded` | Required | Host version | Required | Required | Required | Required | Zero |
| `DeviceDecoded` | Required | Device version | Required | Required | Required | Required | Zero |
| `LegacyRasterImport` | Zero | Zero | Zero | Zero | Zero | Zero | Required |

A field that is required must exactly match the accepted version.

A field that is not applicable must be zero.

Unknown production methods are rejected.

## Device-capabilities handshake

The browser fetches device capabilities before decoding or rasterizing images.

A versioned capabilities response contains:

- protocol version;
- active board profile:
  - `X3`;
  - `X4`;
- display width and height;
- cover target width and height;
- active reader content rectangle:
  - x;
  - y;
  - width;
  - height;
- supported render-bundle schema versions;
- accepted production methods for Host upload;
- accepted pixel formats;
- accepted initial Host source formats:
  - baseline JPEG;
  - progressive JPEG;
  - PNG;
- semantic policy version;
- semantic limits that can affect classification or event finalization:
  - maximum deferred image candidates per spine item;
  - maximum fallback records per section;
  - maximum normalized paths per section;
  - maximum normalized path bytes;
  - any other bound included by reference in the semantic policy;
- canonical luma policy version;
- Host producer-decoder policy version;
- Device producer-decoder policy version;
- geometry policy version;
- resampler policy version;
- dither policy version;
- legacy-import policy version;
- source-digest algorithm;
- payload-checksum algorithm;
- Host preprocessing input limits:
  - accepted source media types;
  - maximum metadata dimensions representable in a bundle;
  - maximum source-entry compressed bytes accepted by the upload workflow, when any;
  - maximum source-entry uncompressed bytes accepted by the upload workflow, when any;
- Device source-decoder limits:
  - maximum JPEG width;
  - maximum JPEG height;
  - maximum components;
  - maximum MCU geometry;
  - maximum decoder row or tile bytes;
  - maximum supported ZIP-entry uncompressed bytes;
  - supported JPEG modes and sampling;
- uploaded output and bundle limits:
  - maximum asset count;
  - maximum normalized asset-path bytes;
  - maximum manifest bytes;
  - maximum cover payload bytes;
  - maximum image-page payload bytes;
  - maximum individual asset bytes;
  - maximum aggregate payload bytes;
  - maximum total render-bundle request bytes;
- fixed `XTCV` stride rule;
- resident cover capacity;
- supported logical-book operations;
- supported recovery operations;
- whether Host bundles, Device bundles, or both are readable;
- source-validation behavior:
  - validation state values;
  - operations that require completed validation;
  - whether cached reading may begin while validation is pending;
  - validation chunk or cooperative-yield policy;
- M3 persistent Device page-cache limits when M3 is enabled:
  - maximum retained pages;
  - maximum retained payload bytes;
  - minimum free space;
  - eviction policy identifier;
  - storage-layout version.

The advertised geometry and limits are authoritative.

The browser must not hardcode X3 or X4 assumptions.

Host preprocessing applies:

- semantic limits while enumerating render targets;
- Host input limits while parsing and decoding;
- output and bundle limits after rasterization.

Host preprocessing does not reject a source image merely because its source dimensions or decoded row width exceed the Device JPEG decoder’s workspace limits. A large Host-decodable image may be accepted when:

- the Host decoder can process it safely;
- the resulting target raster satisfies output geometry and bundle limits;
- its stored source metadata fits the manifest field widths;
- all other Host input limits are satisfied.

Device source-decoder limits apply only to on-device source probing and decoding.

Capabilities must never advertise a cover geometry whose packed payload exceeds the firmware’s compile-time resident-cover capacity.

Protocol version 1 supports render-bundle uploads only for the active board profile.

An unsupported capabilities or bundle version disables preprocessing without disabling ordinary EPUB upload.

A capabilities response is internally invalid and must disable preprocessing when:

- a semantic bound required by the advertised semantic policy is omitted;
- an output limit cannot contain the advertised target geometry;
- accepted pixel formats cannot represent the advertised cover;
- the resident-cover capacity is smaller than the advertised packed cover;
- a limit exceeds the firmware field width used to validate it;
- the advertised source-validation behavior is unsupported by the firmware build.

# Part 2 of 7 — Persistence, Storage Ownership, and Logical-Book Operations

## Persistent publication protocol

### Purpose

The following filesystem objects determine authority after reboot:

- source metadata;
- deletion tombstones;
- idempotency state and compact operation receipts;
- render manifests;
- persistent negative-cover records;
- M3 Device page-cache indexes and payload slots.

A checksum-valid record body is not committed merely because it can be closed, reopened, and reread.

Every authoritative publication must use the durable-storage primitive defined below. Reopen-and-reread validation proves structural readability only. It does not prove survival after immediate power loss.

### Protocol-v1 storage assumptions

Protocol version 1 requires:

- a block device exposing 512-byte logical sectors;
- a filesystem and SD abstraction capable of:
  - flushing file data;
  - flushing file length and directory metadata;
  - flushing FAT or allocation metadata;
  - flushing any firmware-side block cache;
  - waiting until the SD device reports completion of prior writes;
- checked, bounded writes;
- deterministic error reporting when any flush operation fails.

The implementation exposes one internal primitive:

```rust
fn durable_sync(
    file: &mut File,
    volume: &mut Volume,
) -> Result<(), StorageError>;
```

A successful `durable_sync` means:

1. all file-data writes issued before the call have been submitted;
2. the file’s length and directory entry have been persisted;
3. required FAT or allocation-table updates have been persisted;
4. firmware block caches have been drained;
5. the card has completed the writes according to the supported SD interface;
6. a subsequent immediate loss of power is within the failure model tested by this PRD.

A file close, drop, reopen, reread, or checksum pass is not a substitute for `durable_sync`.

If the selected filesystem or SD stack cannot implement and verify this primitive, M0S is blocked. The implementation must not weaken the requirement by retaining the same “durable” terminology.

### Record framing

Every authoritative record consists of:

```text
logical body
zero padding
commit sector
```

Protocol-v1 constants:

```text
COMMIT_FOOTER_BYTES = 512
COMMIT_ALIGNMENT = 512
```

Requirements:

- the commit sector begins at a 512-byte-aligned file offset;
- the commit sector is the final 512 bytes of the file;
- the logical body contains its own exact logical length;
- bytes between the logical body and commit sector are zero;
- the padded body length is a multiple of 512;
- the body checksum covers:
  - the complete logical body;
  - the canonical zero padding;
  - with the body-checksum field zeroed where required;
- the body checksum does not cover the commit sector;
- total file length is exactly:

```text
padded_body_length + COMMIT_FOOTER_BYTES
```

The dedicated commit sector prevents the commit write from sharing a logical sector with authoritative body bytes.

### Record states

A record is in exactly one state:

1. **Absent**
2. **Prepared**
   - body, padding, and body checksum are valid;
   - commit sector is absent, zeroed, malformed, torn, or inconsistent.
3. **Committed**
   - body and padding are valid;
   - commit sector is valid;
   - commit sector identifies the exact body generation, padded length, and body checksum.
4. **Corrupt**
   - framing, lengths, padding, checksums, identity, or commit sector are inconsistent.

Only `Committed` records participate in authority selection.

A higher prepared generation never outranks a lower committed generation.

### Commit sector

The commit sector conceptually contains:

```text
commit_magic
commit_schema_version
committed_generation
padded_body_length
body_crc32_copy
commit_nonce_or_zero
reserved_zero_bytes
commit_crc32
```

Requirements:

- all integer widths and byte order are fixed;
- reserved bytes are zero;
- `commit_crc32` covers all 512 commit-sector bytes with its checksum field zeroed;
- body generation, padded length, and checksum must exactly match the selected body;
- unknown commit versions are not committed;
- a partially written or torn sector is rejected;
- a valid body with an invalid commit sector remains prepared.

### Durable publication sequence

To publish an authoritative record:

1. Select an unused, inactive, or older physical record.
2. Write the complete logical body.
3. Write canonical zero padding through the next 512-byte boundary.
4. Append a zeroed commit sector.
5. Set the final file length.
6. Call `durable_sync`.
7. Close and reopen the record.
8. Validate:
   - file length;
   - body structure;
   - zero padding;
   - body checksum;
   - expected identity;
   - expected generation;
   - invalid commit sector.
9. Revalidate immediately before commit:
   - logical-book authority;
   - source generation;
   - source length and SHA-256;
   - authoritative token where applicable;
   - deletion state;
   - board profile where applicable;
   - applicable policy versions;
   - candidate generation;
   - mutation lease;
   - cancellation state.
10. Overwrite the complete aligned commit sector with the valid sector.
11. Call `durable_sync`.
12. Close and reopen the record.
13. Select and validate it through the exact startup selector.
14. Only then report success or make dependent cleanup eligible.
15. Retain the previous committed record until the candidate validates as committed.

The persistent commit point is successful completion of the second `durable_sync` containing the valid commit sector.

A successful reread before that second sync is not a commit.

### Power-loss semantics

A power loss:

- before the prepared-body sync leaves no new authority;
- during body or padding write leaves no new authority;
- after body completion but before prepared-body sync leaves no guaranteed new authority;
- after prepared-body sync but before commit-sector sync leaves a prepared record;
- during commit-sector overwrite or sync leaves a prepared, corrupt, or committed record;
- after successful commit-sector sync leaves a committed record;
- during later cleanup does not roll back the committed record.

The selector must never infer commitment from:

- a valid body alone;
- an expected final file length alone;
- a commit magic without a valid complete commit-sector checksum;
- a generation number in a prepared record.

### Hardware power-cut gate

Simulation and injected I/O failures are necessary but insufficient.

M0S must include hardware power-cut tests that remove power:

- during body writes;
- during zero-padding writes;
- during the prepared-body sync;
- after the prepared-body sync;
- during the commit-sector overwrite;
- during the commit-sector sync;
- immediately after the commit-sector sync returns;
- during post-commit cleanup.

Tests must cover:

- source metadata;
- deletion tombstones;
- idempotency state;
- render manifests;
- negative-cover state;
- M3 indexes and payload slots when M3 is implemented.

For each cut point, reboot selection must produce exactly one deterministic result:

- the previous committed state; or
- the new committed state.

It must never produce:

- a mixed generation;
- a prepared generation as authority;
- a deleted book without a committed tombstone;
- a new source without committed source metadata;
- an index referencing an uncommitted payload slot.

The supported SD-card contract must identify the cards and interface modes used for this verification.

### Serialization guarantee

The storage owner and mutation lease prevent relevant authority from changing between final revalidation and the commit-sector write.

No conflicting source mutation for the same logical book may commit while a source-dependent publication holds its exact-source mutation lease.

If this guarantee cannot be maintained, publication must abort before writing the valid commit sector.

## Cooperative long-running work

Source hashing, ZIP scanning or inflation, image probing, decoding, resampling, dithering, publication validation, and cleanup may exceed one cooperative-executor turn.

Every such operation is implemented as a resumable bounded job.

Conceptually:

```rust
enum StepResult<T> {
    Pending,
    Complete(T),
    Failed(ImageError),
    Cancelled,
}

trait BoundedJob {
    type Output;

    fn step(&mut self, budget: WorkBudget) -> StepResult<Self::Output>;
}
```

Requirements:

- one step performs only a bounded number of:
  - input bytes;
  - ZIP records;
  - MCUs;
  - source rows;
  - destination rows;
  - asset records;
  - cleanup entries;
- software-controlled work in one step does not exceed `MAX_IMAGE_WORK_SLICE_US`;
- blocking SD requests use bounded transfer sizes;
- SD latency is reported separately from software-controlled execution time;
- resumable state is fixed-capacity;
- execution returns to the executor between steps;
- input, power, networking, display, and storage-owner tasks remain runnable;
- no step waits on a channel whose consumer can be waiting for the current owner;
- a mutation lease retained between steps is owner-managed state, not a blocking executor lock;
- each step checks cancellation and source authority when practical;
- replacement, deletion, recovery, book exit, superseding render requests, source-integrity failure, and shutdown cancel affected jobs;
- cancellation is observed no later than:
  - the end of the current bounded step;
  - plus one in-flight bounded SD request;
- partial decoded output remains private;
- no panel refresh or persistent commit occurs after cancellation;
- final authority revalidation still occurs immediately before publication.

## Storage ownership and mutation serialization

All mutations covered by this PRD execute through one storage owner.

The storage owner serializes:

- logical-book creation;
- logical-book replacement;
- logical-book deletion;
- explicit externally-modified-source recovery;
- unmanaged-source adoption;
- unmanaged-source re-identification;
- source metadata publication;
- idempotency-state publication and epoch rotation;
- Host bundle publication;
- Device cover publication;
- legacy-cover import;
- M3 Device page-cache insertion and eviction;
- negative-result publication;
- cleanup.

### Logical-book mutation rules

At most one source mutation is active for a logical book.

Source mutations include:

- replace;
- delete;
- externally-modified-source recovery;
- unmanaged re-identification;
- managed-source quarantine or repair.

Operations for unrelated logical books may overlap only where the storage implementation proves that authority, memory, and SD-use constraints remain bounded.

### Mutation lease

A source-dependent publication lease contains:

```text
logical_book_id
source_generation
source_length
source_sha256
deletion_state
board_profile_or_none
applicable_policy_identity
candidate_generation
cancellation_epoch
```

The lease is an in-memory execution guard.

It is not a persistent commit marker.

### Required interleaving behavior

The implementation defines deterministic outcomes for:

- replace versus replace;
- replace versus delete;
- replace versus recovery;
- delete versus recovery;
- bundle publication versus replace;
- bundle publication versus delete;
- bundle publication versus recovery;
- Device page-cache insertion versus source mutation;
- legacy import versus source mutation;
- unmanaged re-identification versus artifact publication;
- managed-source integrity failure versus publication;
- cleanup versus active publication.

No operation may commit source-bound state after its source ceases to be authoritative.

## Logical-book HTTP contract

Physical filenames remain internal.

Browser operations use logical-book tokens and epoch-scoped operation request IDs.

### Request-ID format

Create, replace, delete, and recovery use request IDs with the same binary format:

```text
<16 lowercase hexadecimal epoch characters>
<32 lowercase hexadecimal random nonce characters>
```

The first component is the device-advertised `idempotency_epoch: u64`.

The second is a browser-generated random 128-bit nonce.

Request IDs are not authentication secrets.

### Create and replace headers

```text
Content-Length: <exact EPUB byte length>
X-Source-SHA256: <64 lowercase hexadecimal characters>
X-Upload-Request-Id: <48 lowercase hexadecimal characters>
```

The browser computes SHA-256 before upload.

Firmware independently computes SHA-256:

- while receiving the upload;
- while rereading the persisted candidate.

All lengths and digests must match.

### Create a logical book

```text
POST /upload?name=<percent-encoded-display-label>
```

Behavior:

- creates a new logical-book identity;
- assigns a new `logical_book_id`;
- does not replace another book because its label or client filename matches.

### Replace a logical book

```text
POST /upload?name=<percent-encoded-display-label>&replace=<current-book-token>
```

Behavior:

- replaces exactly the authoritative generation identified by the token;
- preserves `logical_book_id`;
- creates a higher source generation;
- generates a new token;
- rejects a genuinely new request with an unknown, stale, deleted, malformed, lower-generation, or nonauthoritative token.

A matching retained idempotency result is resolved before stale-token rejection.

### Delete a logical book

```text
POST /delete-book
Content-Type: application/json
X-Delete-Request-Id: <48 lowercase hexadecimal characters>
```

Request:

```json
{
  "book_token": "32 lowercase hexadecimal characters"
}
```

Behavior:

1. Parse request syntax.
2. Resolve the delete request ID through staging, tombstone, and receipt state before ordinary token validation.
3. For a genuinely new request:
   - require the current epoch;
   - validate the authoritative token;
   - acquire the source-mutation lease.
4. Prepare a logical-book tombstone containing the delete request identity and result.
5. Revalidate authority.
6. durably commit the tombstone.
7. validate it through startup selection.
8. hide the logical book.
9. clean associated state afterward.

A repeated delete with the same request ID and matching parameters returns the original successful result even when:

- the token is now deleted;
- source files have been removed;
- the tombstone has later been reclaimed;
- only a retained compact receipt remains.

Delete idempotency therefore does not depend on tombstone retention.

A prepared tombstone never hides a book.

### Recover an externally modified managed book

```text
POST /recover-book
Content-Type: application/json
X-Recovery-Request-Id: <48 lowercase hexadecimal characters>
```

Request:

```json
{
  "book_token": "32 lowercase hexadecimal characters",
  "observed_source_length": 1234,
  "observed_source_sha256": "64 lowercase hexadecimal characters",
  "display_label": "Optional validated replacement label"
}
```

This operation explicitly adopts the currently present changed EPUB bytes as the next source generation of the same logical book.

Behavior:

1. Resolve the recovery request ID before ordinary token validation.
2. Require the logical book to be in `ExternallyModified`.
3. Require the supplied token to identify the last committed generation from which recovery is being authorized.
4. Rehash the currently present physical source.
5. Require exact match with the supplied observed length and digest.
6. Validate the source-container contract.
7. Confirm:
   - no newer committed generation exists;
   - no deletion tombstone covers the identity;
   - the physical source has not changed again;
   - the request uses the current epoch.
8. Assign the next source generation.
9. Generate a new random book token.
10. Publish source metadata with:
    - the same logical-book ID;
    - the newly observed source identity;
    - an `ExternalRecoveryRequest` operation provenance;
    - an `externally_recovered` flag;
    - the validated display label.
11. durably commit and select the new metadata.
12. invalidate all old source-bound caches, artifacts, and negative results.
13. return the new authoritative result.

Recovery is idempotent through the same retained-receipt mechanism as create, replace, and delete.

Recovery never silently occurs because a file changed.

### List logical books

The list response exposes only authoritative, non-deleted logical books.

Each entry contains:

```text
display_label
logical_book_id
book_token
source_generation
source_origin
externally_recovered
source_integrity_status
source_length
observed_source_length_or_none
observed_source_sha256_or_none
allowed_operations
artifact_presence_by_profile_and_producer
```

`source_origin` remains:

```text
ManagedUpload
UnmanagedSd
```

`externally_recovered` records that the current managed generation was explicitly adopted from externally changed bytes.

Source-integrity states are:

```text
UncheckedThisMount
ValidatedThisMount
Unavailable
ExternallyModified
UnsupportedSourceContainer
```

`allowed_operations` is derived from current authority. Examples:

- normal managed book:
  - `Replace`;
  - `Delete`;
  - `UploadRenderBundle`;
- externally modified managed book:
  - `RecoverCurrentBytes`;
  - `Delete`;
- unmanaged book:
  - `Delete`;
  - operations allowed by unmanaged policy.

Ordinary replacement is not offered for an externally modified generation whose committed source bytes no longer exist.

Prepared candidates, staging-only files, deleted identities, physical filenames, and lower generations are never exposed.

## Display-label contract

The display label is authoritative source-generation metadata.

Validation:

- valid UTF-8;
- 1–64 bytes;
- no NUL;
- no disallowed C0 controls;
- no silent truncation.

Source metadata stores:

```text
display_label_length: u8
display_label: [u8; 64]
```

The body checksum covers both fields.

Replacement and recovery atomically update the label with the new source generation.

An unmanaged book initially derives its label from the filename stem. Invalid or empty stems become `"Untitled"`.

Once committed modern source metadata exists, its embedded label is authoritative.

## Logical book and source-generation model

### Logical-book identity

```text
logical_book_id: [u8; 16]
```

Requirements:

- stable across replacement and recovery;
- distinct from the physical source slot;
- distinct from `book_token`;
- included in source metadata, tombstones, caches, manifests, negative results, and M3 indexes;
- catalog enumeration exposes only its authoritative generation.

### Source origin and operation provenance

Source origin:

```text
ManagedUpload
UnmanagedSd
```

Operation provenance is independently tagged:

```text
ManagedUploadRequest
LocalUnmanagedOperation
ExternalRecoveryRequest
```

`ManagedUpload` does not mean removable storage was immutable.

### Source identity

Canonical identity:

```text
source_length: u64
source_sha256: [u8; 32]
```

A persistent source record also stores a bounded quick-check fingerprint used only for provisional read-only startup:

```text
quick_fingerprint_sha256: [u8; 32]
quick_fingerprint_policy_version: u16
```

The quick fingerprint is computed from deterministic bounded source regions defined by its policy, such as:

- beginning;
- one or more interior offsets derived from source length;
- end.

It is not exact source identity and cannot authorize mutation or artifact publication.

Exact source matching always requires full length plus full SHA-256.

### Source-container bounds

Firmware advertises:

```text
MAX_EPUB_BYTES
MAX_ZIP_OFFSET
MAX_ZIP_ENTRY_COMPRESSED_BYTES
MAX_ZIP_ENTRY_UNCOMPRESSED_BYTES
ZIP64_SUPPORTED = false
```

Protocol-v1 requirements:

- managed uploads exceeding `MAX_EPUB_BYTES` are rejected before staging;
- all classic ZIP offsets and derived ranges use checked arithmetic;
- required ZIP64 interpretation is unsupported;
- oversized or ZIP64 direct-SD sources are not adopted as ordinary books;
- a managed source that becomes unsupported remains identified but unavailable;
- diagnostics may report a rejected physical source without creating a logical-book token.

### Source generation

```text
next_source_generation =
    highest committed source_generation + 1
```

Generation is monotonic per logical-book ID.

Values are never reused.

Overflow blocks further mutation with a stable error.

### Book token

```text
book_token: [u8; 16]
```

Generation:

- 128 random bits from hardware RNG;
- collision checked against surviving source metadata and tombstones;
- valid for one exact source generation;
- encoded as 32 lowercase hexadecimal characters externally.

A lower, deleted, quarantined, or nonauthoritative token is rejected unless a matching idempotency receipt resolves the request first.

## Operation idempotency

### Required lookup order

For create, replace, delete, and recovery:

1. Parse request-ID syntax and operation parameters.
2. Search:
   - active transactions;
   - staging markers;
   - committed source metadata;
   - committed tombstones;
   - retained compact receipts.
3. Resolve a matching prior or active operation.
4. Check request-ID parameter consistency.
5. Only for a genuinely new operation:
   - check epoch freshness;
   - perform ordinary token and authority validation.

This ordering is mandatory because successful replace, delete, and recovery operations make their original tokens stale.

### Stored request identity

Each operation receipt binds:

```text
idempotency_epoch
request_nonce
operation
logical_book_id
base_book_token_or_zero
source_generation
source_length_or_zero
source_sha256_or_zero
display_label_or_empty
result_book_token_or_zero
result_status
receipt_crc32
```

Operation values include:

```text
Create
Replace
Delete
RecoverExternallyModified
```

### Retry after committed success

A matching request ID and matching parameters return the original committed result.

The operation is not executed again.

This remains true after:

- replacement made the base token stale;
- deletion removed files;
- recovery replaced the quarantined generation;
- source metadata moved to a compact receipt;
- tombstone cleanup completed.

### Retry during active work

A matching active request returns an in-progress result and does not start a second transaction.

### Retry after interrupted staging

A matching staged create or replace may resume only when its original authority base is unchanged.

A staged delete or recovery may resume only when:

- the same logical-book authority still applies;
- no newer generation or tombstone superseded it;
- the observed recovery source still has the same full identity.

Otherwise it becomes stale and is cleaned without executing again.

### Request-ID misuse

A request ID reused with different bound parameters is rejected.

### Epoch and retention model

Idempotency state uses permanent A/B authoritative records and the durable publication protocol.

The selected body contains:

```text
magic
schema_version
state_generation
current_epoch
previous_epoch_or_zero
receipt_counts
receipt_record_size
sorted_receipts
body_crc32
```

Rules:

- genuinely new requests must use the current epoch;
- known request IDs are resolved before epoch freshness checks;
- current-epoch successful receipts are retained;
- previous-epoch successful receipts are retained through a bounded migration window;
- epoch values are monotonic and never reused;
- state corruption fails closed;
- firmware never silently resets the epoch;
- rotation occurs before accepting more requests than the current receipt capacity can retain;
- an operation receipt is not reclaimed while any delayed request in an accepted epoch could be interpreted as new.

The capabilities response advertises:

- current epoch;
- maximum new operation requests per epoch;
- retained prior-epoch count.

---

# Part 3 of 7 — Source Authority, Validation, Recovery, and Cache Identity

## Managed-upload staging marker

Before creating or truncating a managed candidate EPUB, firmware publishes a durable staging marker.

Its body contains:

```text
magic
schema_version
marker_generation
operation
operation_request_id
logical_book_id
base_book_token_or_zero
candidate_source_generation
candidate_physical_slot
expected_source_length
expected_source_sha256
display_label
body_crc32
```

Rules:

- the marker is durably prepared and committed before candidate source creation;
- reserved managed slots never enter unmanaged discovery;
- a committed marker with no committed matching source metadata keeps its candidate hidden;
- a prepared or corrupt marker does not authorize a candidate;
- cleanup may remove a candidate only after proving it is not authoritative;
- the marker is removed only after committed source metadata validates or the transaction is deterministically abandoned.

## Managed-slot provenance namespace

Managed source slots use a naming or directory namespace that cannot be mistaken for unmanaged direct-SD EPUBs.

This provenance survives loss of both source-metadata records.

A file in a reserved managed slot without valid selected metadata is:

- hidden;
- quarantined;
- eligible for staged recovery or cleanup;
- never adopted as `UnmanagedSd`.

## Source metadata A/B records

Each physical source slot owns two metadata records.

The logical body includes:

```text
magic
schema_version
metadata_generation
logical_book_id
source_generation
source_origin
source_operation_kind
source_operation_request_id_or_local_id
externally_recovered
physical_slot
source_length
source_sha256
quick_fingerprint_policy_version
quick_fingerprint_sha256
book_token
display_label
body_crc32
```

### Metadata generation

`metadata_generation` is monotonic for the physical slot.

It is distinct from `source_generation`.

### Record selection

A source metadata record is selectable only when:

- body and commit sector are valid;
- source and metadata generations are valid;
- physical-slot identity matches;
- source fields are structurally valid;
- no committed tombstone covers the logical identity.

Among valid records for one slot, the highest committed metadata generation is selected.

Among committed source generations for one logical-book ID, the highest source generation is authoritative.

Ambiguous duplicate authority is corruption and requires recovery rather than arbitrary selection.

### Publication

Metadata uses inactive-record copy-on-write publication.

The previous committed metadata record remains until the new record validates through normal selection.

### Managed uploads

Managed source metadata is published only after:

- receive-time source verification;
- persisted-file reread and full SHA-256 verification;
- source-container validation;
- final authority revalidation.

### Unmanaged sources

Initial unmanaged metadata and later unmanaged re-identification also use A/B publication.

The only committed identity record is never overwritten in place.

## Logical-book deletion tombstone

The tombstone body contains:

```text
magic
schema_version
tombstone_generation
logical_book_id
deleted_source_generation
deleted_book_token
delete_request_id
delete_result_status
body_crc32
```

A committed tombstone hides:

- all source generations at or below `deleted_source_generation`;
- all matching source-bound artifacts;
- all matching caches;
- all matching negative results;
- all matching M3 entries.

A prepared tombstone has no authority.

### Deletion transaction

1. Resolve idempotency.
2. Validate the authoritative token for a genuinely new request.
3. Acquire the source-mutation lease.
4. Select the next tombstone generation.
5. Write and durably sync a prepared tombstone.
6. Revalidate:
   - logical-book authority;
   - source generation;
   - token;
   - absence of a newer tombstone;
   - mutation lease.
7. durably commit the tombstone.
8. Validate it through startup selection.
9. publish or retain a compact delete receipt.
10. hide the book.
11. cancel source-dependent work.
12. clean source and artifact state.

The tombstone may be reclaimed only after:

- no matching source, metadata, marker, cache, artifact, negative result, or M3 entry remains;
- the successful delete result is retained in compact idempotency state for all accepted request epochs.

## Authoritative source selection

Startup selection order:

1. Load valid committed deletion tombstones.
2. Scan reserved managed slots and committed staging markers.
3. Select committed source metadata records.
4. Group records by logical-book ID.
5. Exclude generations covered by committed tombstones.
6. Select the highest committed source generation.
7. Reject ambiguous equal-generation authorities.
8. Hide:
   - prepared records;
   - staging-only candidates;
   - managed orphans;
   - lower generations;
   - deleted identities.
9. Discover eligible unmanaged candidates only outside the managed namespace.

### Persistent source commit point

A managed source becomes authoritative only when:

- persisted source reread matches exact length and SHA-256;
- source-container validation succeeds;
- prepared metadata is durably synced;
- final authority revalidation succeeds;
- the valid metadata commit sector is durably synced;
- startup selection chooses the new generation;
- no tombstone covers it.

Deleting old generations and removing the staging marker are cleanup.

### Persistent deletion commit point

Deletion becomes authoritative only after the valid tombstone commit sector is durably synced and selected.

File removal is cleanup.

## Managed EPUB upload transaction

Required sequence:

1. Validate request syntax, label, declared length, digest, and request ID.
2. Resolve idempotency before token or epoch checks.
3. For a new request:
   - require current epoch;
   - validate Create or Replace authority.
4. Acquire the source-mutation lease.
5. assign or resolve logical-book ID.
6. confirm no tombstone prohibits the operation.
7. reserve the next source generation.
8. choose an unused managed physical slot.
9. durably publish the staging marker.
10. create the candidate EPUB.
11. receive exactly `Content-Length` bytes while calculating SHA-256.
12. reject early EOF, excess bytes, length mismatch, or digest mismatch.
13. sync and close the candidate.
14. reopen and independently read every persisted byte.
15. recompute exact length and SHA-256.
16. require agreement with declaration and receive-time measurements.
17. validate classic-ZIP bounds and reject ZIP64.
18. compute the quick fingerprint.
19. generate a collision-checked token.
20. prepare source metadata.
21. durably sync the prepared record.
22. validate the prepared record and persisted source.
23. immediately revalidate source authority.
24. durably commit source metadata.
25. select the generation through normal startup logic.
26. seed the current-mount exact-validation set for that generation.
27. remove the staging marker.
28. clean lower generations and stale artifacts.

The successful HTTP response is sent only after committed source selection succeeds.

A pre-commit failure preserves prior authority and keeps the candidate hidden.

## EPUB upload response

Success includes:

```text
status
logical_book_id
book_token
operation_request_id
source_length
source_sha256
source_generation
display_label
active_board_profile
capabilities_version
accepted_render_bundle_schema
render_bundle_upload_enabled
```

Errors include:

- stable machine-readable code;
- retry classification;
- no physical filesystem paths.

## Source-integrity validation

### Validation levels

Firmware distinguishes:

1. **Unchecked**
2. **QuickChecked**
3. **FullyValidated**
4. **Mismatch**
5. **Unavailable**
6. **UnsupportedContainer**

Only full length plus full SHA-256 establishes exact identity.

A quick check is explicitly not an integrity proof.

### Quick check

The bounded quick check compares:

- expected physical slot;
- exact file length;
- available filesystem identity metadata;
- stored quick fingerprint over deterministic bounded source regions.

A quick-check mismatch forces full validation before any source-bound cache or artifact is displayed.

A quick-check match permits only the provisional read-only behavior below.

### Provisional fast cached open

To preserve the existing fast cached-open experience, firmware may provisionally display previously committed state after a successful quick check but before full SHA-256 completes.

Permitted provisional behavior:

- display a validated text cache;
- display an existing committed cover artifact;
- display an existing committed image-page artifact;
- navigate cached pages;
- begin full SHA-256 validation in the background.

Prohibited until full validation:

- source JPEG decode;
- source ZIP parsing for new content;
- persistent cache creation or replacement;
- render-bundle installation;
- Device cover publication;
- negative-result publication;
- M3 insertion;
- source mutation based on the current bytes;
- treating the source as exact for any persistent authority decision.

The UI remains in `UncheckedThisMount` until full validation succeeds.

If background validation fails:

1. cancel source-dependent work;
2. prevent further cache or artifact presentation;
3. clear resident source-bound images;
4. mark the logical book `ExternallyModified` or `Unavailable`;
5. return to a safe library or error view;
6. offer only allowed repair operations.

This product choice explicitly trades a bounded possibility of briefly displaying stale cached content for preserving fast read-only reopen. It does not permit stale data to authorize persistent mutation.

### Full mount-session validation

Full validation compares:

- exact source length;
- complete SHA-256.

It is required before:

- source open;
- source decode;
- artifact upload commit;
- any persistent source-derived publication;
- externally modified recovery;
- legacy import;
- trusting a persistent negative result as a reason not to attempt supported work.

Successful validation remains trusted until:

- unmount;
- remount;
- reboot;
- detected filesystem change;
- source I/O inconsistency.

A managed upload’s persisted-file reread may seed full validation after committed source selection confirms the same slot, generation, length, and digest.

### End-to-end cached-open contract

For a source with valid committed caches and a successful quick check:

- first cached reader frame must not wait for full-file SHA-256;
- background validation starts no later than the first reader open;
- cached-open latency is measured from user activation to completed framebuffer render request;
- acceptance targets appear in Part 7.

### Initial unmanaged identity

An eligible unmanaged candidate is adopted only after:

- source-container bounds pass;
- complete source length and SHA-256 are calculated;
- quick fingerprint is calculated.

Firmware then:

1. acquires a mutation lease;
2. assigns a logical-book ID;
3. assigns source generation 1;
4. generates a token;
5. generates a local operation ID;
6. validates the label;
7. publishes A/B metadata;
8. revalidates the source immediately before commit;
9. durably commits metadata;
10. selects it normally.

### Unmanaged source mismatch

When an unmanaged source changes:

1. acquire the source-mutation lease;
2. fully hash and reconfirm the current file;
3. invalidate old source-bound state;
4. assign the next source generation;
5. generate a new token and local operation ID;
6. compute a new quick fingerprint;
7. prepare inactive metadata;
8. revalidate source identity and deletion state;
9. durably commit metadata;
10. clean old state afterward.

A failed publication leaves prior metadata intact, but the validation mismatch prevents stale state from being used beyond any already permitted provisional display.

### Managed source mismatch

When a managed source differs from committed metadata:

- exact validation fails;
- the logical book becomes `ExternallyModified`;
- old source-bound state is rejected after mismatch discovery;
- the changed bytes are not silently adopted;
- bundle upload and source decode are prohibited;
- ordinary replacement from the missing old source authority is not offered;
- list output exposes:
  - observed length;
  - observed full SHA-256;
  - `RecoverCurrentBytes`;
  - `Delete`.

The explicit recovery endpoint defined in Part 2 is the protocol-v1 repair path.

### Repeated change during recovery

If the physical source changes after mismatch observation but before recovery commit:

- observed identity revalidation fails;
- no new generation commits;
- the request remains retryable only with a new observed identity and new genuinely new request ID;
- a previously committed recovery receipt remains idempotent.

## Cache identity and invalidation

Persistent semantic caches bind to:

```text
logical_book_id
source_generation
source_length
source_sha256
semantic_policy_version
cache_schema_version
```

Image-rendering state additionally binds to:

```text
producer
production_method
board_profile
applicable policy versions
```

Quick checking may permit provisional read-only display.

Full validation is required before cache or artifact creation, replacement, or authoritative use in a mutation.

Any source, schema, production-method, or policy mismatch causes deterministic rejection and rebuild.

A cache-capacity failure must never independently change the semantic event stream. It may prevent cache publication, but not reclassify an already finalized `ImagePage` as text.

---

# Part 4 of 7 — Render-Bundle Transport, Manifest, Covers, and Host Preprocessing

## Render-bundle HTTP transport

### Endpoint

```text
POST /render-bundle
Content-Type: application/octet-stream
Content-Length: <exact framed request length>
```

Protocol version 1 accepts only:

```text
producer = Host
production_method = HostDecoded
requested_profile = active_profile
```

### Transport framing

The forward-streamed request contains:

```text
transport header
client manifest template
cover payload
image payload
```

The transport header includes:

```text
magic
transport_version
header_length
book_token
source_length
source_sha256
board_profile
bundle_schema
manifest_template_length
cover_payload_length
image_payload_length
total_request_length
header_crc32
```

The client manifest template contains:

- HostDecoded namespace;
- Host producer;
- HostDecoded method;
- applicable policy versions;
- sorted asset records;
- normalized path table;
- payload offsets and checksums;
- generation zero;
- body checksum zero;
- no commit sector.

The template is never authoritative.

### Streaming publication

The handler:

1. validates framing and exact request length;
2. checks capability limits;
3. resolves authoritative token and exact source identity;
4. requires full current-mount source validation;
5. rejects deleted or externally modified authority;
6. acquires an exact-source Host bundle lease;
7. selects the inactive Host generation;
8. streams the manifest template to staging;
9. performs bounded preliminary validation;
10. streams cover and image payloads;
11. rejects early EOF, excess bytes, and inconsistent lengths;
12. syncs and closes payload files;
13. completely validates staged structure, ranges, and CRCs;
14. constructs the final manifest body;
15. writes and durably syncs the prepared manifest;
16. validates it in prepared mode;
17. revalidates:
    - token;
    - source identity;
    - source generation;
    - full-validation status;
    - deletion state;
    - profile;
    - policies;
    - generation;
    - lease;
18. durably commits the aligned manifest commit sector;
19. validates the generation through normal runtime selection;
20. removes staging files;
21. reclaims the prior Host generation afterward.

A failed retry may modify only the inactive Host generation for the active profile.

It cannot modify:

- the active Host generation;
- another profile;
- Device state;
- legacy-import state;
- the source EPUB.

## Wi-Fi memory and resource budget

The upload path uses:

- bounded socket and HTTP buffers;
- one fixed transport-header buffer;
- bounded record buffers;
- a fixed-capacity identity index;
- bounded path comparison;
- bounded checksum state.

It does not retain the complete manifest or payload in RAM.

M0R reports:

- peak Wi-Fi heap;
- minimum free heap;
- peak stack;
- identity-index bytes;
- path-buffer bytes;
- manifest RAM;
- upload throughput;
- staged-manifest reads;
- durable prepare and commit costs;
- behavior under interrupted and malformed requests.

## Replacement, deletion, recovery, and cleanup

Cleanup is:

- restartable;
- idempotent;
- bounded;
- serialized with conflicting mutations;
- incapable of committing new source or artifact authority.

It reclaims:

- stale staging markers;
- hidden managed candidates;
- prepared metadata;
- prepared tombstones;
- invalid inactive bundles;
- orphan M3 slots;
- stale raw legacy covers;
- lower source generations;
- artifacts bound to old generations;
- expired receipts only when epoch safety permits.

Committed tombstones remain until matching persistent state is gone and delete replay safety has moved to compact receipts.

Cleanup failure never rolls back current authority.

## Render-bundle model

### Logical contents

A Host or Device cover generation contains:

- `RENDER.MF`;
- optional `COVER.BIN`;
- optional `IMAGES.BIN` for Host image-page bundles.

`COVER.BIN` uses `XTCV`.

`IMAGES.BIN` contains concatenated Host image-page rasters.

M3 Device page persistence uses the separate fixed-slot cache defined in Part 6. It does not extend a monolithic Device bundle by copying all prior page payloads forward.

### Namespaces and valid combinations

```text
HostDecoded + Host + HostDecoded
DeviceDecoded + Device + DeviceDecoded
LegacyImport + Device + LegacyRasterImport
```

Every manifest is homogeneous.

Namespace key:

```text
bundle_namespace
board_profile
```

Each profile and namespace owns A/B generations.

Host and Device cover generations for X3 and X4 may coexist.

LegacyImport contains exactly one cover.

### Runtime precedence

Cover:

1. HostDecoded cover;
2. DeviceDecoded cover;
3. LegacyImport cover;
4. Device source decode;
5. blank or deterministic negative result.

Image page:

1. HostDecoded image-page asset;
2. M3 Device page-cache asset;
3. Device source decode;
4. retained fallback placeholder.

## Manifest schema

### Header

`RENDER.MF` contains:

```text
magic
schema_version
header_length
logical_manifest_length
bundle_namespace
producer
production_method
device_assigned_generation
board_profile
logical_book_id
source_generation
source_length
source_sha256
semantic_policy_version
canonical_luma_policy_version
producer_decoder_policy_version
geometry_policy_version
resampler_policy_version
dither_policy_version
legacy_import_policy_version
pixel_format
cover_target_geometry
reader_content_geometry
asset_count
asset_record_bytes
normalized_path_bytes
cover_payload_length
image_payload_length
body_crc32
```

The logical body is followed by canonical zero padding and the aligned commit sector.

### Whole-manifest integrity

The body checksum covers:

- header with checksum field zeroed;
- asset records;
- path table;
- canonical padding.

Validation rejects:

- inconsistent lengths;
- unknown versions;
- arithmetic overflow;
- nonzero reserved fields;
- malformed padding;
- trailing data;
- invalid commit state.

### Policy validation

Policy fields are checked according to production method.

Decoded artifacts require current:

- semantic;
- producer-decoder;
- canonical-luma;
- geometry;
- resampler;
- dither versions.

Legacy import requires:

- all decoded-image policy fields zero;
- current legacy-import version.

### Generation authority

A generation is eligible only when:

- manifest body and commit sector are valid;
- source identity exactly matches authoritative metadata;
- source is not deleted;
- profile matches;
- namespace and production method are valid;
- required policy versions match;
- payload files have exact expected lengths.

## Asset records

Each record includes:

```text
stable_asset_id
role
normalized_href_offset
normalized_href_length
payload_file
payload_offset
payload_length
payload_crc32
source_format
source_width
source_height
normalized_orientation
output_width
output_height
output_stride
pixel_format
flags
```

Roles:

```text
Cover
ImagePage
```

Rules:

- stable ID derives from complete normalized path and role;
- complete paths are compared on stable-ID collision;
- all offsets and ranges use checked arithmetic;
- output geometry must fit active capabilities;
- source metadata fields must fit Host manifest limits;
- source metadata does not need to fit Device decoder workspace limits for HostDecoded assets.

## Manifest cardinality and role invariants

A valid generation requires:

- zero or one Cover record;
- Cover references only `COVER.BIN`;
- `COVER.BIN` exists exactly when a Cover exists;
- every Cover has valid `XTCV`;
- ImagePage references only `IMAGES.BIN`;
- `IMAGES.BIN` exists exactly when image records exist;
- payload ranges are nonoverlapping;
- every payload byte belongs to exactly one record;
- no undeclared padding or trailing payload bytes.

LegacyImport additionally requires:

- exactly one Cover;
- no image records;
- exact active X4 legacy-compatible geometry;
- no decoded-image policy claims.

## Canonical ordering and bounded validation

Records are sorted by:

```text
payload_file
payload_offset
role
normalized_href_bytes
```

Publication validation uses:

1. header pass;
2. record and range pass;
3. path and identity pass;
4. complete payload CRC pass;
5. prepared-manifest pass;
6. final authority pass;
7. durable commit;
8. post-commit selector and complete validation.

Runtime selection uses:

1. generation-structure validation without reading unrelated asset payloads;
2. requested-asset CRC and format validation.

Runtime corruption behavior:

- structural corruption invalidates the generation;
- requested-asset payload corruption invalidates only that asset for the mount when ranges remain structurally sound;
- cover loading reads no `IMAGES.BIN` payload bytes;
- one image-page load reads only its range.

## Cover format and resident representation

### `XTCV`

Header:

```text
magic
schema_version
header_length
producer
production_method
width
height
stride
pixel_format
semantic_policy_version
canonical_luma_policy_version
producer_decoder_policy_version
geometry_policy_version
resampler_policy_version
dither_policy_version
legacy_import_policy_version
payload_length
payload_crc32
header_crc32
```

The header identity must match the manifest.

### V1 stride

```text
stride = ceil(width / 8)
payload_length = stride * height
```

Validation rejects:

- zero dimensions;
- unsupported geometry;
- overflow;
- stride mismatch;
- unsupported format or polarity;
- payload-length mismatch;
- payload exceeding resident capacity;
- trailing bytes.

### Resident cover

Firmware defines:

```text
MAX_COVER_BYTES
```

`ReaderStore` retains:

```text
cover_data
cover_len
cover_width
cover_height
cover_stride
cover_valid
```

Cover loading:

1. validates generation structure;
2. selects the Cover record;
3. reads and CRC-checks only the Cover range;
4. validates `XTCV`;
5. copies exactly the initialized payload;
6. records runtime width, height, stride, and length.

`UiCover` carries runtime geometry.

Rendering never substitutes legacy compile-time stride.

## Legacy `X4CV` import

A raw legacy cover is migration input only.

Eligibility requires:

- authoritative non-deleted source;
- full current-mount source validation;
- association with the authoritative physical slot;
- no valid Host or Device cover;
- valid legacy magic, exact dimensions, stride, length, polarity, and payload;
- active X4 profile with exact matching target geometry.

Import:

1. acquire exact-source lease;
2. validate legacy payload;
3. wrap exact bytes in `XTCV`;
4. set:
   - producer Device;
   - method LegacyRasterImport;
   - decoded policy fields zero;
   - current legacy-import version;
5. bind to exact source identity and profile;
6. publish inactive LegacyImport generation using durable prepare and commit;
7. validate through ordinary runtime selection;
8. remove or ignore the raw legacy file only after success.

Any source mismatch rejects the raw legacy cover.

## Browser preprocessing

### Host decoder policy

The shipping browser uses a pinned deterministic Wasm decoder.

Browser Canvas is not normative.

Initial Host formats:

- baseline JPEG;
- progressive JPEG;
- PNG.

### Host input limits

Host preprocessing applies Host-specific bounds from capabilities, including:

- accepted media types;
- maximum representable source metadata dimensions;
- maximum source-entry compressed and uncompressed sizes when imposed;
- browser implementation safety bounds.

It does not apply Device JPEG workspace limits.

A Host decoder may process a source larger than Device limits when the resulting target raster and manifest satisfy output limits.

### Decoder-output contract

The Host decoder emits exactly:

- upright `Gray8`; or
- upright non-premultiplied `RGBA8`.

`Gray8` is permitted only when every effective decoded pixel is fully opaque.

`RGBA8` is required for effective transparency, including:

- alpha channels;
- grayscale-plus-alpha;
- palette alpha;
- PNG `tRNS`;
- any supported source feature producing nonopaque pixels.

The Host decoder owns:

- source parsing;
- IDCT;
- JPEG color conversion;
- PNG sample decoding;
- palette and transparency interpretation;
- source color-space handling;
- ICC and gamma handling;
- EXIF and orientation;
- source-specific rounding.

The canonical luma stage owns only:

- opaque Gray8 pass-through;
- RGBA compositing over white;
- RGB-to-luma conversion;
- final rounding and clamp.

### Policy closure

M0R freezes:

- semantic policy and bounds;
- Host decoder policy;
- canonical luma;
- geometry;
- resampler;
- dither;
- serialization;
- all coefficients and rounding.

M0D later freezes Device decoder policy.

### Cover discovery and geometry

Discovery order:

1. EPUB 3 `cover-image`;
2. EPUB 2 cover metadata;
3. conservative ID or href fallback.

Geometry:

- aspect-preserving fill;
- centered crop;
- no stretching;
- active-profile target dimensions.

### Home rendering

M1A draws the active cover:

- opaque;
- clipped;
- right-aligned in the Home content area;
- vertically centered;
- blank when absent;
- using runtime width, height, stride, and payload length.

### On-device cover generation

1. Select source authority.
2. Require full source validation.
3. Try Host cover.
4. Try Device cover.
5. Try LegacyImport cover.
6. optionally import eligible legacy cover.
7. inspect persistent negative state.
8. discover and probe source cover.
9. decode supported JPEG through bounded jobs.
10. apply canonical luma, geometry, resampling, and dither.
11. acquire Device cover lease.
12. durably publish a cover-only Device generation.
13. validate and load it.
14. preserve successful text open regardless of cover failure.

---

# Part 5 of 7 — Image Semantics, Typed Caches, Rendering, and Refresh Behavior

## Persistent negative-cover state

A persistent negative result suppresses repeated deterministic Device cover failures.

Transient failures are never persisted.

The A/B record body includes:

```text
magic
schema_version
record_generation
logical_book_id
source_generation
source_length
source_sha256
source_origin
normalized_cover_path_or_absent
producer
production_method
semantic_policy_version
canonical_luma_policy_version
producer_decoder_policy_version
geometry_policy_version
resampler_policy_version
dither_policy_version
legacy_import_policy_version
board_profile
deterministic_failure_class
body_crc32
```

Initial deterministic classes:

```text
NoCoverCandidate
UnsupportedSourceFormat
UnsupportedOrientation
SourceImageMalformed
SourceImageExceedsConfiguredLimits
DecoderFeatureUnsupported
DeterministicRasterPipelineFailure
```

A mismatch in source, profile, producer, method, or applicable policy invalidates the record.

Publication requires:

- full source validation;
- exact-source lease;
- durable prepared sync;
- final authority revalidation;
- durable commit;
- ordinary selector validation.

## Image-page eligibility

A dedicated image page is produced only when the complete rendered body of one spine item is one eligible JPEG or PNG image.

Transparent wrappers may include:

- `html`;
- `body`;
- `section`;
- `div`;
- `figure`;
- `p`;
- `a`.

Disqualifiers include:

- meaningful text;
- heading;
- caption;
- second image;
- another media element;
- navigation;
- visible content before or after the image;
- unsupported Host source format.

Aspect ratio does not affect eligibility.

## Order-preserving deferred candidate

Conceptual states:

```text
NoCandidate
WithheldCandidate
Disqualified
```

Rules:

- an image after meaningful content emits its placeholder immediately;
- the first image before meaningful content is withheld with its exact finalized placeholder;
- later disqualifying content emits:
  1. withheld placeholder;
  2. disqualifying content;
- a second image emits both placeholders in source order;
- at spine end, a sole withheld candidate is probed after the ZIP callback ends;
- it becomes:
  - an `ImagePage` with retained fallback; or
  - the ordinary placeholder;
- `SpineEnd` follows.

### Semantic bound behavior

The maximum withheld candidates is fixed at one in protocol version 1.

Any future bound that can change final event classification must be:

- part of `semantic_policy_version`; or
- explicitly advertised and included in effective semantic identity.

Host and firmware execute the same bounded algorithm.

Cache-storage capacity does not alter this finalized event sequence.

## Image metadata probe

The bounded forward probe recognizes:

- baseline JPEG;
- progressive JPEG;
- PNG.

It reports:

```text
format
dimensions
orientation_status
complete
unsupported
malformed
insufficient_prefix
```

It does not:

- allocate;
- seek;
- decode pixels;
- retain the complete image.

Host target enumeration applies Host metadata limits.

Device source decode later applies Device decoder limits.

## Typed content cache

M2 increments `CONTENT_VERSION`.

### Cache structure

```text
header
fixed event records
normalized path table
fallback-record table
```

Header:

```text
magic
schema_version
header_length
logical_book_id
source_generation
source_length
source_sha256
semantic_policy_version
event_count
event_record_bytes
normalized_path_bytes
fallback_record_bytes
event_stream_crc32
path_table_crc32
fallback_table_crc32
header_crc32
```

### Event kinds

```text
Text
ImagePage
SpineEnd
```

Every event begins with:

```text
event_kind
flags
record_length
```

### ImagePage event

```text
stable_asset_id
normalized_href_offset
normalized_href_length
source_format
source_width
source_height
normalized_orientation
fit_policy
fallback_record_offset
fallback_record_length
image_flags
reserved
```

Requirements:

- path is valid normalized UTF-8;
- stable ID recomputes from role and complete path;
- source dimensions fit schema field widths;
- source dimensions need not fit Device decoder limits for Host-only rendering;
- fallback bytes exactly reproduce the prior centered italic placeholder;
- fallback is nonempty and bounded;
- reserved fields are zero.

### Fallback and path tables

Tables are deduplicated by exact bytes in first-seen finalized-event order.

Stable-ID equality never substitutes for complete-path equality.

### Capacity behavior

Content-cache encoding limits include:

```text
MAX_IMAGE_FALLBACK_RECORD_BYTES
MAX_CONTENT_FALLBACK_TABLE_BYTES
MAX_CONTENT_PATH_TABLE_BYTES
MAX_CONTENT_EVENT_COUNT
```

When a finalized event stream cannot fit:

- the event stream is not changed;
- an `ImagePage` is not reclassified as placeholder merely to fit the cache;
- no partial or semantically different cache is published;
- the live source parse continues with the finalized event stream;
- a future open may reparse the source;
- Host render-target enumeration remains unchanged.

Cache publication is all-or-nothing per section unless a future schema defines a prefix-safe representation whose replay is provably identical.

These encoding limits therefore affect cache availability, not semantic classification.

### Replay guarantee

A valid cache replay requires no source reopen for:

- image classification;
- probe;
- path normalization;
- source format;
- dimensions;
- orientation;
- fallback reconstruction.

Malformed, incomplete, mismatched, or old caches are rejected.

### Golden requirement

For every fixture:

1. full source parse;
2. finalized event capture;
3. attempted cache serialization;
4. if serialization succeeds, cache-only replay;
5. compare:
   - every finalized event;
   - normalized paths;
   - fallback records;
   - image indices;
   - section bytes;
   - page records.

For capacity-overflow fixtures:

- Host enumeration and firmware full parse must still match;
- cache publication must fail deterministically;
- no semantic event may change.

## Section and page cache

M2 increments `CACHE_V2_VERSION`.

### Page record

```rust
enum PageKind {
    Text,
    Image,
}

struct PageRecord {
    kind: PageKind,
    flags: u8,
    first_or_image: u16,
    block_count: u16,
    reserved: u16,
}
```

Text pages reference text blocks.

Image pages reference one section-local image record.

### Section image record

Each record stores:

```text
stable_asset_id
normalized_href
source_format
source_width
source_height
normalized_orientation
fit_policy
fallback_record
```

The section image table is:

- source-bound;
- semantic-policy-bound;
- checksummed;
- bounded;
- deterministically ordered.

An image-table capacity failure prevents section-cache publication. It does not alter the live finalized event stream.

## Image-page rendering

1. Select authoritative non-deleted source.
2. Read and validate section image and fallback records.
3. Attempt Host artifact:
   - validate generation structure;
   - read and CRC-check only requested range.
4. Attempt M3 Device page-cache artifact.
5. If an artifact is available during provisional fast open, it may be displayed subject to Part 3.
6. Before source decode, require full source validation.
7. Check session failure memo.
8. open source EPUB;
9. locate and probe image;
10. select supported Device reduction;
11. decode and rasterize through bounded jobs;
12. recheck cancellation and source authority;
13. expose only a fully completed private framebuffer;
14. on failure, clear partial image output and render retained fallback text.

No partial image reaches the panel.

A stale or cancelled job produces no panel update or persistent publication.

## Render outcome and refresh planning

Renderer result:

```rust
enum PresentedContentKind {
    Text,
    Image,
    Placeholder,
}

struct RenderOutcome {
    kind: PresentedContentKind,
    minimum_refresh: Option<RefreshMode>,
}
```

Rules:

- successful image:
  - `kind = Image`;
  - `minimum_refresh = Full`;
- fallback:
  - `kind = Placeholder`;
- planner computes normal policy, then raises it to the minimum;
- display task stores `last_presented_content_kind`;
- history updates only after successful panel flush;
- entering an image requires Full;
- image-to-image requires Full;
- leaving an image requires Full;
- failed flush preserves previous history;
- cancelled render does not alter history.

## Session failure memo

Key:

```text
logical_book_id
source_generation
section_index
section_image_index
device_decoder_policy_version
geometry_policy_version
resampler_policy_version
dither_policy_version
```

Only deterministic failures are memoized.

The bounded memo clears when:

- leaving the book;
- source generation changes;
- included policies change.

---

# Part 6 of 7 — Device JPEG Decoder, Raster Pipeline, and Bounded M3 Persistence

## On-device JPEG decoder

### Selection hierarchy

1. Adapt `tjpgdec-rs`.
2. Implement a constrained Rust SOF0 decoder.
3. Port only required JPEGDEC algorithms.
4. Consider a full JPEGDEC port only if later features justify it.

### Source-input contract

The decoder reads a forward-only resumable stream.

It must:

- accept arbitrary positive chunk sizes;
- handle boundaries at any byte;
- distinguish truncation, malformed data, unsupported features, and I/O failure;
- never seek backward;
- never require the complete compressed entry;
- enforce compressed and uncompressed bounds;
- stop after a bounded input or MCU budget;
- resume without restarting the entry;
- check cancellation between steps.

### Pixel-delivery contract

The decoder emits borrowed bounded tiles.

```rust
trait GrayTileSink {
    fn push_tile(
        &mut self,
        x: u32,
        y: u32,
        width: u16,
        height: u16,
        stride: u16,
        pixels: &[u8],
    ) -> Result<(), ImageError>;
}
```

Initial Device output is opaque `Gray8`.

Requirements:

- coordinates are in upright reduced-image space;
- initial supported orientation is absent or 1;
- tiles arrive top-to-bottom then left-to-right;
- no overlaps or repeats;
- every output pixel is delivered exactly once;
- tiles are no larger than approved MCU bounds;
- initial target maximum is 16×16;
- stride and slice calculations are checked;
- borrowed pixels expire when callback returns;
- callback failure aborts decode;
- no callback follows terminal failure;
- external input cannot produce out-of-range coordinates.

A bounded MCU-row strip may replace tiles only after M0D proves:

- a fixed maximum byte count;
- workspace fit on X3 and X4;
- equivalent ordering and ownership;
- no full-image buffering.

### Raster-pipeline contract

The streaming pipeline may retain only:

- decoder state;
- one tile or bounded strip;
- horizontal scaler state;
- minimum vertical source-row ring;
- one destination luma row or segment;
- current and next dither error rows;
- destination packed raster or private framebuffer;
- ZIP inflate scratch.

It may not retain:

- complete source image;
- complete reduced source image;
- unbounded source rows;
- buffers sized from unvalidated dimensions.

Cover crop and page containment are computed from probed dimensions before decode.

Pixels outside the cover crop may be discarded early.

### Device-only workspace limits

M0D defines:

```text
MAX_DEVICE_SOURCE_WIDTH
MAX_DEVICE_SOURCE_HEIGHT
MAX_TILE_WIDTH
MAX_TILE_HEIGHT
MAX_TILE_BYTES
MAX_DESTINATION_WIDTH
MAX_RESAMPLER_ROWS
MAX_DECODER_WORKSPACE
MAX_RASTER_WORKSPACE
MAX_TOTAL_IMAGE_WORKSPACE
MAX_IMAGE_WORK_SLICE_US
MAX_IMAGE_IO_CHUNK_BYTES
```

These limits are advertised separately from Host input and uploaded-output limits.

They apply only to on-device source decode.

A decoder candidate is rejected if EPUB inflate scratch cannot remain resident concurrently.

### Device producer-decoder responsibilities

Device decoder policy owns:

- JPEG markers;
- entropy;
- quantization;
- IDCT and reduced IDCT;
- components and sampling;
- restart handling;
- source orientation;
- source-to-Gray8 rounding;
- tile partition and delivery.

Canonical luma remains a separate stage.

### Initial firmware JPEG contract

Supports:

- SOF0;
- 8-bit Huffman JPEG;
- grayscale;
- YCbCr 4:4:4, 4:2:2, and 4:2:0 where verified;
- one grayscale or interleaved three-component scan;
- restart intervals;
- 8-bit quantization;
- orientation absent or 1.

Optional 16-bit quantization remains measurement-gated.

Refuses:

- SOF1;
- progressive;
- multi-scan baseline;
- arithmetic coding;
- lossless;
- 12-bit;
- CMYK or YCCK;
- unsupported sampling or transform;
- unsupported orientation;
- malformed, truncated, or oversized input.

## Decoder verification

M0D includes:

- two independent reference decoders;
- marker, entropy, IDCT, color, tile, and pipeline unit tests;
- valid-format matrix;
- unsupported and malformed matrix;
- arbitrary input-chunk metamorphic tests;
- tile-boundary metamorphic tests;
- injected I/O failures;
- callback-failure tests;
- guard regions;
- fuzzing;
- Miri where applicable;
- sanitizer native runners;
- maximum-dimension tests;
- scaler and dither guard tests;
- proof that no full-image buffer exists;
- X3 and X4 stack and workspace measurements.

A textual or agent-assisted port is not sufficient evidence.

A decoder that matches pixels but violates bounded delivery is rejected.

## Scaling and dithering

### Reduced IDCT

Candidates:

```text
1/1
1/2
1/4
1/8
```

Choose the largest reduction that does not undershoot the dimensions required by the destination crop or contained page.

Use checked rational arithmetic.

### Cover geometry

- fill;
- centered crop;
- no stretching.

### Page geometry

- contain;
- centered;
- white padding.

### Resampling

Initial candidates:

- area or box shrink;
- fixed-point bilinear enlargement;
- separable horizontal and vertical processing.

The selected filter must operate within the bounded tile or strip contract.

### Dithering

Initial candidate:

- fixed-point Floyd–Steinberg;
- current and next destination-width error rows.

Fallback:

- ordered Bayer.

Host policy is frozen in M0R.

Equivalent Device behavior and workspace are proven in M0D.

## M3 bounded persistent Device image pages

### Design choice

M3 does not store Device image pages by rebuilding a monolithic DeviceDecoded bundle.

It uses:

- a fixed number of page-cache slots;
- two physical payload copies per slot;
- an A/B authoritative index;
- immutable committed payload records;
- deterministic FIFO eviction.

Adding one page writes:

- one inactive slot payload;
- one bounded index generation.

It does not copy retained page payload bytes.

### Capability-advertised limits

```text
M3_PAGE_SLOT_COUNT
M3_MAX_PAGE_PAYLOAD_BYTES
M3_MAX_TOTAL_RETAINED_BYTES
M3_MIN_FREE_SPACE_BYTES
M3_INDEX_SCHEMA_VERSION
M3_SLOT_SCHEMA_VERSION
M3_EVICTION_POLICY = FIFO
```

The slot count and index size are compile-time bounded.

### Payload slot

Each logical slot owns A/B payload records.

A payload body contains:

```text
magic
schema_version
slot_generation
logical_book_id
source_generation
source_length
source_sha256
board_profile
stable_asset_id
normalized_path_hash
policy_identity
output_geometry
payload_length
payload_crc32
packed_payload
body_crc32
```

It uses canonical padding and a durable aligned commit sector.

### Authoritative index

The A/B index body contains:

```text
magic
schema_version
index_generation
logical_book_id
source_generation
source_length
source_sha256
board_profile
policy_identity
next_insertion_sequence
entry_count
fixed_slot_entries
body_crc32
```

Each entry includes:

```text
occupied
slot_number
selected_slot_copy
slot_generation
stable_asset_id
normalized_path
payload_length
payload_crc32
insertion_sequence
```

The complete normalized path is retained or compared through a bounded side table so stable-ID collisions cannot alias assets.

### Insertion

1. Require full source validation.
2. Acquire exact-source Device page-cache lease.
3. Validate current index.
4. If asset already exists, return success without rewriting.
5. Select:
   - a free slot; or
   - the occupied entry with the lowest insertion sequence.
6. Write the inactive payload copy for that slot.
7. durably sync its prepared body.
8. revalidate source authority and policies.
9. durably commit the payload slot.
10. validate it normally.
11. build a new inactive index:
    - add or replace the selected entry;
    - increment insertion sequence;
    - retain other fixed entries without copying their payloads.
12. durably sync the prepared index.
13. revalidate authority.
14. durably commit the index.
15. validate normal selection.
16. reclaim the old victim payload copy afterward.

A crash:

- before slot commit leaves no referenced new page;
- after slot commit but before index commit leaves an orphan committed slot;
- after index commit exposes the new page;
- during cleanup cannot remove the newly indexed page.

Orphan slots are ignored and later reclaimed.

### Lookup

Runtime validates:

1. committed index structure and source identity;
2. matching complete path and stable ID;
3. selected payload-slot generation;
4. payload CRC and format.

A corrupt slot invalidates only that page entry for the mount.

### Eviction

Protocol-v1 eviction is FIFO by committed insertion sequence.

Reads do not update persistent recency and therefore do not create write traffic.

Sequence overflow disables further insertion with a stable error until a future schema migration.

### Free-space admission

Before insertion, firmware requires free space for:

- maximum candidate slot record;
- one complete inactive index;
- filesystem metadata overhead;
- configured safety margin.

Failure is nonfatal and leaves prior cache state intact.

### Write-amplification gate

Per inserted page, logical bytes written must be:

```text
O(new page payload + fixed bounded index)
```

They must not depend on total retained payload bytes.

M3 reports:

- payload bytes;
- index bytes;
- filesystem overhead;
- total physical writes where measurable;
- write amplification;
- insertion latency;
- eviction latency;
- power-cut outcome.

M3 is blocked if implementation rewrites all retained page payloads for each insertion.

---

# Part 7 of 7 — Milestones, Verification, Performance, and Closed Decisions

## Milestones

### M0S — Source transaction foundation

Deliverables:

1. storage-owner serialization;
2. exact-source leases;
3. verified `durable_sync`;
4. aligned commit-sector record protocol;
5. create, replace, delete, recover, and list operations;
6. transactional labels;
7. epoch-scoped request IDs for all source mutations;
8. compact receipts;
9. idempotency lookup before token checks;
10. managed staging markers;
11. reserved managed-slot provenance;
12. A/B source metadata;
13. persisted-source reread and SHA-256;
14. quick-fingerprint policy;
15. classic-ZIP gate and ZIP64 rejection;
16. deletion tombstones;
17. source selection;
18. provisional fast cached-open contract;
19. full mount-session validation;
20. unmanaged adoption and re-identification;
21. managed external-modification quarantine;
22. explicit recovery;
23. restartable cleanup;
24. simulated and hardware power-cut tests.

Acceptance includes:

- close or reread alone is never treated as durable commit;
- loss of power at every publication phase yields old or new authority only;
- staging-only managed files never appear unmanaged;
- create, replace, delete, and recovery retries return original results;
- stale base tokens do not override matching receipts;
- receipt cleanup cannot cause delayed operation reexecution;
- prepared metadata and tombstones never affect authority;
- deletion remains effective through interrupted cleanup;
- externally modified managed bytes are not silently adopted;
- explicit recovery produces a new generation;
- recovery fails when bytes change again;
- cached reopen may render provisionally after quick check;
- no persistent mutation occurs before full validation;
- a later full-hash mismatch cancels and quarantines the book;
- oversized and ZIP64 sources are rejected deterministically;
- no watchdog or responsiveness limits are violated.

### M0R — Render artifact foundation

Deliverables:

1. shared EPUB semantics;
2. semantic bounds in policy identity;
3. Host and Device decoder policy separation;
4. transparency-safe Host output contract;
5. canonical luma;
6. geometry, resampler, dither, and legacy versions;
7. separated Host input, Device decode, and output limits;
8. active-profile capabilities;
9. framed Host bundle upload;
10. A/B Host, Device cover, and Legacy namespaces;
11. durable manifest publication;
12. cardinality and role invariants;
13. bounded validation;
14. `XTCV`;
15. resident cover;
16. legacy import;
17. pinned Host decoder;
18. Host browser preprocessing;
19. power-cut and memory measurements.

Acceptance includes:

- large Host-decodable sources are not rejected solely by Device workspace limits;
- transparent PNG mechanisms require RGBA output;
- prepared or torn manifests are never selected;
- old committed bundle survives failed publication;
- Host and Device policy invalidation is independent;
- cover load reads no image payload bytes;
- asset-local corruption remains local;
- runtime geometry and stride reach Home rendering;
- X3 and X4 goldens pass.

### M0D — Device decoder selection and verification

Deliverables:

- decoder evaluation;
- forward-only resumable input;
- bounded tile or strip delivery;
- streaming raster pipeline;
- Device decoder policy;
- differential and fuzz suites;
- responsiveness, memory, stack, speed, and quality measurements.

Acceptance:

- no complete source image buffer;
- bounded output tile or strip;
- arbitrary chunking gives identical output;
- callback and I/O failures expose no partial result;
- inflate scratch remains resident;
- X3 and X4 workspace fits;
- baseline contract passes;
- fuzzing reports no memory-safety defect;
- cancellation prevents panel update and commit;
- watchdog misses are zero.

### M1A — Hotspot-preprocessed covers

Dependencies:

- M0S;
- M0R.

Acceptance:

- visible Host cover without Device decoder;
- baseline, progressive JPEG, and PNG support;
- transparency fixtures render correctly;
- stale, deleted, and externally modified sources reject bundle publication;
- active-profile and policy enforcement;
- durable manifest publication;
- runtime stride and geometry;
- X3 and X4 goldens.

### M1B — On-device cover fallback

Dependencies:

- M0D;
- M1A cover consumer.

Acceptance:

- direct-SD baseline JPEG cover;
- Host precedence;
- Device precedence over legacy;
- durable Device cover publication;
- bounded tile-to-cover pipeline;
- policy-bound negative results;
- book text opens when cover generation fails.

### M2A — Hotspot-preprocessed image pages

Acceptance:

- eligible JPEG or PNG spine item becomes one page;
- progressive JPEG and PNG render from Host artifacts;
- semantic Host enumeration matches firmware;
- cache-capacity overflow does not reclassify pages;
- fallback order and text are preserved;
- typed replay is byte-identical when cache publication succeeds;
- image refresh transitions are Full.

### M2B — On-device image-page fallback

Acceptance:

- supported baseline JPEG renders without artifact;
- unsupported source renders retained fallback;
- source decode requires full validation;
- deterministic failure memo works;
- no partial image reaches panel;
- input and power responsiveness meets limits.

### M3 — Bounded persistent Device image pages

Acceptance:

- fixed slot count and retained byte limit;
- deterministic FIFO eviction;
- insertion does not copy all retained payloads;
- old index survives failed insertion;
- committed orphan slots remain unreferenced;
- source or policy mismatch rejects index;
- free-space rejection preserves old state;
- hardware power cuts yield old or new index only;
- measured write amplification is independent of retained payload volume.

## Verification requirements

### Persistent commit and recovery

Test:

- every body-write boundary;
- prepared sync failure;
- commit-sector write boundary;
- commit sync failure;
- immediate hardware power cut;
- selector behavior;
- old-record retention;
- cleanup interruption.

Run against all authoritative record classes.

### Logical-book operations

Test:

- create retry after lost response;
- replace retry after stale base token;
- delete retry after tombstone cleanup;
- recovery retry after new generation commit;
- parameter-mismatch request-ID reuse;
- epoch rotation;
- receipt migration;
- concurrent replace/delete/recover races;
- label validation;
- physical-name isolation.

### Source integrity

Test:

- exact match;
- same-length modification;
- changed length;
- quick-check false negative simulation;
- provisional cached display followed by full-hash failure;
- cancellation after mismatch;
- managed recovery;
- source changing during recovery;
- direct-SD re-identification;
- unsupported ZIP64 and oversized files.

### Artifact validation

Test:

- malformed framing;
- invalid padding;
- torn commit sector;
- manifest checksum;
- path normalization;
- stable-ID collisions;
- overlapping ranges;
- cardinality violations;
- source and policy mismatch;
- transparency fixtures;
- Host source dimensions exceeding Device limits but valid Host output;
- requested-asset corruption;
- unrelated asset preservation.

### Typed cache

Test:

- source-order preservation;
- sole eligible image;
- disqualified candidates;
- second image;
- caption and text cases;
- exact fallback;
- path collision;
- cache replay;
- fallback-table overflow;
- path-table overflow;
- event-count overflow;
- Host-versus-firmware target enumeration under every semantic bound.

### Decoder and raster pipeline

Test:

- supported sampling;
- grayscale;
- restart intervals;
- unsupported markers;
- malformed and truncated streams;
- arbitrary chunking;
- tile order;
- callback failure;
- cancellation;
- maximum dimensions;
- crop and contain geometry;
- resampling;
- dither guards;
- no full-image allocation;
- X3 and X4 hardware.

### M3

Test:

- free slot insertion;
- FIFO eviction;
- duplicate insertion;
- payload commit failure;
- index commit failure;
- orphan cleanup;
- source replacement race;
- deletion race;
- policy upgrade;
- full cache;
- low free space;
- sequence overflow;
- hardware power cuts.

## Performance and resource targets

### Cached-open latency

For a valid committed text cache and successful quick check, measured from user activation to completed framebuffer render request:

```text
p50 <= 150 ms
p95 <= 250 ms
maximum <= 500 ms
```

Targets apply separately to X3 and X4 on the supported SD-card set.

Full SHA-256 runs in the background and is not on this provisional cached-open critical path.

The report includes:

- quick-check bytes read;
- quick-check latency;
- cache bytes read;
- render latency;
- executor stalls;
- time until full SHA validation completes.

### Operations requiring full validation

Bundle upload, source decode, persistent publication, and recovery may wait for full validation.

Requirements:

- visible progress or nonblocking busy state begins within 250 ms;
- input and power remain responsive;
- cancellation is supported;
- SHA throughput and total latency are reported through `MAX_EPUB_BYTES`;
- full validation must not be described as part of the 500 ms artifact-loading target.

### Hotspot path

Report:

- browser hashing;
- upload throughput;
- persisted-source reread;
- source-container validation;
- durable marker, metadata, tombstone, receipt, and manifest prepare/commit costs;
- Host preprocessing;
- Wi-Fi heap floor;
- receipt and epoch costs;
- mutation-lease overhead.

### Runtime artifact lookup

After full source validation, report:

- generation-structure validation;
- memoized selection;
- cover CRC and `XTCV`;
- resident copy;
- image-page range validation;
- bytes read from each file.

Targets:

- valid cover selection and load completes within 500 ms excluding panel refresh;
- cover loading reads zero image payload bytes;
- increasing image payload to its maximum does not change cover result;
- one image-page load reads only its declared range plus bounded metadata.

### Cooperative responsiveness

On X3 and X4:

```text
software-controlled work per executor turn <= 10 ms
input and power event latency <= 100 ms
cancellation <= one work slice + one bounded SD operation
watchdog misses = 0
```

Report p50, p95, p99, and maximum.

SD-operation latency is separate.

An SD card that cannot satisfy the supported responsiveness contract is excluded or requires a storage-strategy change.

### On-device decode

Initial X4 target:

- typical 1200×1800 baseline JPEG raster completes within five seconds, excluding panel refresh.

Report:

- compressed bytes;
- reduction;
- decoder workspace;
- tile or strip bytes;
- scaler and dither workspace;
- destination bytes;
- inflate scratch;
- total concurrent workspace;
- stack;
- code size;
- raster time;
- image comparison;
- responsiveness;
- cancellation;
- watchdog.

### M3 storage performance

Report:

- payload bytes written;
- index bytes written;
- filesystem metadata writes;
- physical writes where measurable;
- insertion p50, p95, p99, maximum;
- eviction latency;
- free-space-check cost;
- power-cut result;
- write amplification across empty, half-full, and full caches.

Shipping requires insertion cost to remain independent of total retained payload bytes.

## Decisions closed by this PRD

- Reopen and reread do not prove durability.
- Persistent authority requires a verified `durable_sync`.
- The commit marker occupies a dedicated aligned sector.
- Body durability is established before commit-sector publication.
- Hardware power-cut verification is mandatory.
- All storage mutations execute through one owner.
- Exact-source leases protect final revalidation.
- Create, replace, delete, and recovery are explicitly idempotent.
- Delete replay does not depend on tombstone retention.
- Externally modified managed books have a protocol-v1 recovery operation.
- Recovery is explicit and produces a new source generation.
- Fast cached reopen may use provisional read-only state after a bounded quick check.
- Provisional state cannot authorize persistent mutation.
- Full SHA-256 remains the exact source-identity proof.
- A full-hash mismatch cancels work and quarantines the managed book.
- Semantic capacity and cache-encoding capacity are separate.
- Cache encoding failure cannot change finalized image-page classification.
- Host preprocessing uses Host limits, not Device workspace limits.
- Device decoder limits apply only to Device source decode.
- Gray8 is permitted only for known-opaque decoded output.
- Effective transparency requires non-premultiplied RGBA8 through canonical compositing.
- Host and Device decoder policies remain independently versioned.
- Device image pages use fixed slots and a bounded A/B index in M3.
- M3 insertion never copies all retained page payloads.
- M3 eviction is FIFO and does not write on page reads.
- Source, artifact, cache, negative, and M3 state are exact-source-bound.
- ZIP64 remains unsupported in protocol version 1.
- Image refresh decisions use renderer outcome and successful panel history.

## Remaining measurement and implementation gates

- exact filesystem and SD implementation of `durable_sync`;
- verified supported SD-card set;
- commit-sector write behavior under real power cuts;
- pinned Host/Wasm decoder;
- Device decoder selection;
- exact Device policy and sampling identifiers;
- optional 16-bit quantization;
- retained `unsafe`, if any;
- tile or strip dimensions;
- decoder and raster workspace;
- final resampler and dither;
- semantic and cache-capacity constants;
- maximum asset count and identity-index bytes;
- staged-manifest validation I/O;
- quick-fingerprint region policy;
- provisional cached-open hardware results;
- full SHA throughput;
- receipt capacity and epoch rotation;
- M3 slot count and page-size limit;
- M3 index size;
- M3 filesystem overhead and write amplification;
- whether M2B needs a rendering plate;
- progressive Device JPEG;
- SVG wrappers;
- inline image layout.
