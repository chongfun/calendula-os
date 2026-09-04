# Image Rendering and Hotspot Preprocessing

Status: **Planning.** Product behavior, artifact scope and binding, render-bundle
architecture, artifact validation, rendering and refresh behavior, and the
bounded device page cache are closed except where explicitly assigned to a
measurement or decoder-selection gate.

Date: 2026-09-03, revising the 2026-07-26 draft.

## What this revision changes

The 2026-07-26 draft carried its own source-transaction foundation as milestone
M0S: logical books, book tokens, monotonic source generations, deletion
tombstones, epoch-scoped idempotency receipts, managed staging markers, an
aligned commit-sector publication protocol with its own `durable_sync`, and a
mount-session validation model built on a quick fingerprint and a provisional
cached open. It was implemented on `feature/image-rendering` as the
`source-store` crate, about 17,000 lines, and hardware-validated on the X3.

None of that survives here. Three PRDs took the same problem apart along better
lines, and #75 replaced the storage transaction underneath it:

- **Source Identity** gives an EPUB's bytes an authoritative `SourceDigest`, and
  draws the line this draft kept crossing. A cheap per-file identity decides
  whether one file's own cache is usable. Only a full digest, read in the
  current trust epoch, may claim that two files hold the same bytes.
- **Library Identity** gives each physical copy a random opaque `BookId` with a
  durable two-generation ledger, and settles what a replacement means, including
  the library-metadata intent that stands before `INSTALL.JNL` and outlives it.
- **Physical Folder Library** gives a book a locator relative to a root, and
  settles that storage mutations stay globally serialized.
- **#75** replaced the `.PND`/`.CLN` marker protocol under uploads with
  `INSTALL.JNL`, `RECLAIM.JNL`, and FAT directory-entry moves.

This revision assumes those are complete and deletes what they own. What remains
is the part that was always this project's: turning image bytes into a 1bpp
raster the panel can show, deciding which spine items become image pages, and
keeping both bounded inside 400 KB of SRAM.

The result is a much smaller document. The old Part 2 and Part 3 are gone
entirely, replaced by one part on artifact identity and durability. M0S is
retired, so the milestone list starts at M0R.

## Summary

CalendulaOS can load a pre-rasterized 1bpp cover from a book's cache directory,
but it does not draw that bitmap in the Home view, generate cover rasters on the
device, or render EPUB illustrations as image pages.

This project adds two image-production paths:

1. **Hotspot-assisted preprocessing.** The browser parses an EPUB during upload,
   produces a cover and eligible image-page rasters, and uploads a versioned
   render bundle alongside the unchanged EPUB.
2. **On-device fallback.** Books copied directly to the card, uploads made
   without preprocessing, and books whose bundle is missing or invalid are
   decoded from supported source JPEGs on the device. Device-produced covers are
   persisted; device-produced image pages are session-only until M3.

Both paths share one implementation of EPUB semantics, normalized paths, cover
and image-page classification, bounded semantic finalization, canonical
post-decode luma conversion, output geometry, resampling, dithering, raster
serialization, artifact validation, and runtime precedence.

Host and device source decoders stay independently versioned. A host decoder
update does not invalidate device-decoded artifacts, and the reverse holds too.

Milestones:

- **M0R** render artifact foundation: shared EPUB image semantics, policy
  versions, capabilities, render formats, host preprocessing, bounded artifact
  validation, artifact publication, resident cover representation.
- **M0D** on-device decoder selection and verification: evaluate `tjpgdec-rs`,
  implement or adapt the selected baseline JPEG decoder, define a bounded
  tile-delivery contract, and establish differential, fuzz, streaming, resource,
  and memory-safety verification.
- **M1A** hotspot-preprocessed covers: upload an EPUB and a host cover artifact,
  validate and install it, load the active-profile cover, and draw it in Home.
- **M1B** on-device cover fallback: generate and publish a cover-only device
  artifact when no valid host cover exists.
- **M2A** hotspot-preprocessed image pages: identify image-only spine items,
  persist typed image-page events, upload their rasters, render them while
  reading.
- **M2B** on-device image-page fallback: reopen the source EPUB and decode
  supported JPEG pages when no valid preprocessed asset exists.
- **M3** bounded persistent device image pages: retain a bounded set of
  device-produced page rasters with explicit retention, eviction, free-space,
  write-amplification, and worst-case latency limits.
- **Later candidate** progressive JPEG thumbnails, considered only after
  baseline support ships and corpus data justifies it.

M1A does not depend on the embedded JPEG decoder. M2A may render
browser-preprocessed PNG and progressive-JPEG image pages before firmware
supports those source encodings. Stopping after M1 is acceptable if full-page
1bpp images turn out to be too slow or too ugly on hardware.

## The foundation this PRD builds on

### What the library PRDs supply

| Fact | Owner | Shape today |
|---|---|---|
| Where a copy sits | Physical Folder Library R4 | `BookRoot` plus a root-relative `LibraryPath`, `proto::library_path` |
| Which bytes a file holds | Source Identity R1, R2 | `SourceDigest`, `proto::source` |
| Whether a stored digest still describes the file | Source Identity R11 | `CachedSourceDigest`, which cannot yield a `SourceDigest` without a read |
| Which user-visible copy a file is | Library Identity R14 to R16 | `BookId` plus the two-generation ledger, `proto::identity` and `upload_store::ledger` |
| Whether one file's own cache is usable | Source Identity R5 | `proto::cache::source_hash_at(root, locator, byte_size)`, keyed into `/READER/CACHE2/E<hash>` |
| Durable filesystem mutation | #75, Physical Folder R8 | `INSTALL.JNL`, `RECLAIM.JNL`, staging under `/READER/UPLOAD`, predecessor parked in `/READER/ROLLBACK` |
| Managed replacement continuity | Library Identity R4, R17 | the library-metadata intent, published before the install and settled after it |
| Move repair | Library Identity R3, R6 to R9 | the reconciliation pass inside the catalog scan, reporting `FoundAgain` |

Two names are used throughout and mean exactly what those PRDs mean by them:

```text
LocalCacheIdentity
    cheap, per physical file: source_hash_at(root, locator, byte_size)
    decides whether this file's own cache is still usable
    does not authorize sharing anything with another file

SourceDigest
    full SHA-256 over the whole stream, plus the byte length
    authoritative byte identity
    authorizes equivalence across files and locators, once read in this epoch
```

### What this PRD therefore stops specifying

| Retired from the 2026-07-26 draft | Why, and who owns it now |
|---|---|
| Logical-book identity, `logical_book_id`, `book_token` | Library Identity: a copy is a `BookId`, and a request names a locator |
| Monotonic `source_generation` | Source Identity: bytes are named by their digest, not by a counter |
| Deletion tombstones and their reclamation rules | The journalled delete, plus the catalog sweep that already reclaims orphaned cache directories |
| Epoch-scoped request IDs, receipts, idempotency lookup order | The install journal makes an upload recoverable; artifacts are rebuildable, so a repeated bundle upload costs a rewrite and nothing else |
| Managed staging markers and the managed-slot namespace | `/READER/UPLOAD` staging plus `INSTALL.JNL` |
| The aligned commit-sector protocol and `durable_sync` | Nothing this PRD writes is durable authority. See "Durability class" below |
| A/B source metadata records, source selection | The ledger and the catalog |
| The quick fingerprint and provisional fast cached open | Source Identity R5 and R11: opening a book asks no identity question, so there is nothing to be provisional about |
| Full mount-session validation as a precondition for reading | Same. Revalidation is owed only where equivalence is claimed |
| `ExternallyModified` quarantine and the `/recover-book` endpoint | Library Identity R4 handles unexplained external replacement by refusing continuity, not by quarantining the book |
| The hardware power-cut gate for artifact publication | The gate belongs to the ledger and the journals, and Library Identity's durability tests own it |

### A retired approach, recorded so it does not return

The `source-store` crate on `feature/image-rendering` is reference material, not
a branch to rebase. It predates #75, it builds a second storage transaction
beside `INSTALL.JNL`, and its central premise, that image artifacts need durable
authority of their own, is wrong: an artifact that cannot be rebuilt from the
EPUB and the policy versions is an artifact this PRD should not be creating.

Three things it learned are worth keeping, and appear in this document rather
than in that crate:

- adoption of a sideloaded book cost two full passes at roughly 250 KB/s, which
  is why nothing here hashes a book on the reading path;
- the session owner needed one contiguous claim taken immediately after
  `sync_mem::donate_heap`, before radio init fragments the donated regions;
- the abrupt-reset campaign harness and `tools/powercut_campaign.py` are
  reusable, and the library work is where they now point.

## Existing behavior

### Cover path

The repository already contains a legacy cover cache and its validator
(`X4CV`, `proto::cache::encode_cover_header`), a fixed-capacity resident cover
buffer in `ReaderStore`, a `UiCover` model, a path that supplies the active
book's cover to the Home view model, and host-side cover decoding in
`tools/preview`.

The current cover file is `/READER/CACHE2/E<hash>/COVER.BIN`, in the same
per-book cache directory as `BOOK.BIN`, `CONT.BIN`, and the section files. Its
geometry is fixed at 202x303 with a compile-time stride, which is an X4
assumption in a tree that now defaults to portrait and ships an X3.

Only `tools/preview` writes it. `reader_cache::publish` and `fw::book_build`
load it into the store on a successful publish. No renderer draws it:
`UiBook::cover` is constructed as `None`.

This project introduces a board-neutral `XTCV` format carrying dimensions,
stride, pixel format, producer, production method, applicable policy versions,
payload length, and payload checksum. M1A adds the missing Home-view rendering.
Library-view thumbnails are not part of the initial feature.

### Reading path

The XHTML parser emits text blocks through `XhtmlBlockSink::push_block`. An
`<img>` becomes italic, centered placeholder text using its `alt` value, or
`"[Image]"`. The content cache (`CONT.BIN`, `CONTENT_VERSION` 3) stores a flat
stream of text records, and the page cache (`CACHE_V2_VERSION` 26) identifies
only ranges of text blocks. Neither format can represent an image page or retain
an EPUB image path.

M2 therefore requires semantic parser events, order-preserving deferred image
classification, bounded image probing, deterministic capacity behavior, a typed
content-cache stream, an explicit page kind, a section-local image table, cache
version invalidation, artifact lookup, and source-decode fallback.

### Identity path

`proto::source` supplies `SourceDigest`, the streaming `SourceHasher`, the
`CachedSourceDigest` evidence type, and its on-card record. `StagedUpload::write`
hashes an upload while it streams, so a managed upload knows its digest without
rereading the file. A book's cache directory carries a claim naming its owner
locator plus a `CacheEvidence { cluster, digest }` slot, and the identity work
fills that slot from a sliced background read after an open rather than on the
open itself.

## User-visible behavior

### Covers after M1A

- A book uploaded through the hotspot may carry a preprocessed cover.
- The original EPUB stays byte-for-byte unchanged.
- On arrival, firmware confirms the bundle names a copy it can find and that the
  file at that copy's locator still holds the digest the bundle names.
- On load, firmware validates the stored set against the copy it names, the
  active board profile, the production method, every applicable policy version,
  manifest integrity, payload integrity, and the active-profile cover geometry.
  It does not hash the book to draw a cover.
- A valid cover is loaded and drawn in the Home view.
- An interrupted or rejected bundle upload does not fail the EPUB upload.
- A missing cover leaves the Home cover area blank.
- A partially written bundle is not adopted.

### Covers after M1B

- A book copied directly to the card, uploaded without preprocessing, or paired
  with an invalid bundle can generate its cover on the device.
- Generation works whether the book's text opened through `BOOK.BIN`, `CONT.BIN`,
  or a full source parse.
- The result is published as a cover-only device artifact for the active profile.
- A valid host cover stays preferred.
- A malformed, unsupported, missing, or oversized cover does not prevent the book
  text from opening.
- A deterministic failure may be recorded so the device stops retrying it.
- A transient failure stays retryable.

### Image pages after M2A

- A spine item whose complete rendered body is one eligible JPEG or PNG image
  becomes one image page.
- Eligibility comes from the same bounded finalization algorithm firmware runs,
  and the browser enumerates targets under the exact semantic limits the device
  advertises.
- A valid host raster renders even when firmware cannot decode the source
  encoding, so progressive JPEG and PNG may appear before firmware supports them.
- The full image is contained inside the reader content rectangle, centered, with
  white padding, and drawn with a Full e-ink refresh.
- Decorative images, inline images, captions, and images mixed with prose stay
  placeholders, and placeholder order matches source-event order even when an
  image candidate is later disqualified.
- Running out of room in a cache does not change pagination. Capacity behavior
  that can affect classification is part of the semantic policy and is applied
  identically by the browser and by firmware.

### Image pages after M2B

- With no valid raster, firmware attempts source decoding for the formats in its
  device JPEG contract.
- An unsupported source with no artifact produces the placeholder.
- A malformed image does not abort the book or corrupt the page cache.
- Deterministic failures are suppressed for the rest of the reading session;
  transient SD and source-access failures may be retried.
- No partially decoded image reaches the panel.

### Image pages after M3

- Successfully decoded image pages may be persisted in a bounded device page
  cache with a maximum retained byte count, a maximum retained page count,
  deterministic eviction, a minimum-free-space admission rule, a bounded
  publication cost, and a bounded write-amplification target.
- Persisting a page does not overwrite or discard host assets, and a failed
  publication leaves the previous state intact.
- Adding one page does not copy every retained page payload forward.
- M3 does not ship until its layout passes the write-amplification and
  worst-case latency gates in Part 6.

## Goals

- Preserve every uploaded EPUB byte-for-byte.
- Preprocess every image the reader can display: the selected cover, and the
  images belonging to eligible JPEG or PNG image-only spine items.
- Let hotspot preprocessing deliver visible images before the on-device decoder
  exists.
- Provide on-device fallback for sideloaded, unpreprocessed, and stale-artifact
  cases.
- Use one shared definition of cover discovery, path normalization, image-page
  eligibility, bounded semantic finalization, canonical post-decode luma
  conversion, geometry, resampling, dithering, raster serialization, and artifact
  validation.
- Keep firmware image logic `no_std`, allocation-free, bounded, and panic-free
  for external input.
- Use forward-only ZIP-entry reads for on-device decoding, without extracting a
  complete image to a temporary file and without buffering a complete decoded
  source image.
- Keep the reading path where Source Identity left it: opening a book waits on no
  digest, and no artifact lookup is allowed to make it wait either.
- Keep artifacts rebuildable, so a lost or torn artifact costs work rather than
  user state.

## Non-goals

- Rewriting or optimizing the source EPUB.
- Inline image layout, CSS image sizing, floats, or text wrapping.
- Captions associated with image pages.
- Promoting images out of otherwise textual spine items.
- General SVG rasterization, or any vector drawing, transform, or clipping.
- Host image-page formats other than JPEG and PNG in the initial M2A contract.
- PNG, progressive, arithmetic-coded, lossless, 12-bit, CMYK, or YCCK JPEG in the
  initial firmware decoder.
- Source orientations other than absent or 1 in the initial device decoder.
- Background image decoding or prefetching.
- Multi-level grayscale panel rendering.
- Library-view cover thumbnails.
- Matching JPEGDEC's complete API, or treating a textual port of any decoder as
  evidence of correctness.
- Compatibility with the current `X4CV` cover file, with old M2 content, or with
  old page-cache layouts.
- Persisting device-decoded image-page rasters before M3, or an unbounded or
  copy-forward-on-every-page persistent page cache.
- Uploading artifacts for a board profile other than the active one.
- Using device JPEG workspace limits as host source-decode limits.
- Giving artifacts a durability contract of their own.

## Architectural overview

### Hotspot-assisted path

```text
Browser receives EPUB
  -> fetch active-profile device capabilities
  -> parse container, OPF, spine, and XHTML
  -> apply the advertised semantic limits and finalization behavior
  -> discover cover
  -> classify eligible JPEG and PNG image-only spine items
  -> decode source images with the pinned host/Wasm decoder
  -> preserve effective transparency in decoder output
  -> normalize to upright Gray8, or to upright non-premultiplied RGBA8
  -> apply canonical luma policy
  -> apply shared geometry and resampling
  -> apply the shared 1bpp dither
  -> validate rasters against the advertised output limits
  -> POST /upload, unchanged, which installs through INSTALL.JNL
  -> device replies with the copy's BookId and the digest it hashed while streaming
  -> browser builds a bundle naming that BookId and that digest
  -> POST /render-bundle, one framed request, active profile only
  -> device resolves the copy, confirms the digest still describes its bytes
  -> device stages payloads and a manifest in the copy's cache directory
  -> device validates the staged set whole
  -> device renames it into place
  -> firmware loads and blits validated rasters
```

### Raw-source fallback path

```text
No valid artifact for this copy
  -> resolve the copy's locator from the catalog row
  -> locate the source image in the EPUB
  -> stream the DEFLATE entry through ZipStream
  -> probe JPEG metadata against the device decoder limits
  -> choose a supported reduced-IDCT scale
  -> decode supported baseline JPEG as bounded ordered tiles
  -> convert each tile to canonical luma
  -> stream tiles through the bounded scaler and dither state
  -> write only the destination raster or the private framebuffer
  -> for a cover, publish a cover-only device artifact for this copy
  -> for a page, optionally retain it through the bounded M3 cache
```

Neither path hashes the book. The fallback path reads the bytes the locator
names, and the artifact it writes is bound to the same local cache identity the
rest of that directory already uses.

### Runtime precedence

For a cover or an image page, firmware resolves assets in this order:

1. a valid **host-decoded** asset for this copy and the active board profile;
2. a valid **device-decoded** asset for this copy and the active board profile;
3. on-device source decode, when the source format is supported;
4. a blank cover area, an image placeholder, or the applicable recorded
   deterministic negative result.

Each producer owns its own artifact set per board profile. Publishing one
profile's artifact does not evict another's, and device production does not
modify or replace host artifacts.

## Shared semantic layer

The following is implemented once in Rust and reused by firmware, host tools, and
the hotspot Wasm build:

- EPUB container and OPF path handling;
- cover discovery;
- URL-fragment removal, normalized EPUB-root-relative paths, root-escape
  rejection;
- image-only-spine classification and order-preserving placeholder handling;
- bounded deferred-candidate finalization and bounded fallback-record behavior;
- deterministic capacity-exhaustion behavior;
- render-target enumeration;
- image metadata probing;
- canonical post-decode luma conversion;
- cover and page geometry, resampling, dithering, raster serialization;
- bundle encoding and validation;
- typed content-cache encoding, and section and page-cache encoding.

The browser may use an allocating source-image decoder. Firmware may not allocate
from external image dimensions and may not retain a complete decoded image.

Shared code distinguishes four kinds of limit, and conflating them is the bug
this section exists to prevent:

- semantic limits, which affect target classification or pagination;
- host source-decoder limits;
- device source-decoder limits;
- artifact-output and upload limits.

Only a semantic limit may decide whether an XHTML event becomes an `ImagePage` or
a placeholder. A device decoder workspace limit does not change host target
enumeration.

## Artifact policy versions

Persistent decoded-image artifacts carry independently versioned policy
dimensions, so a change in one does not invalidate artifacts that did not depend
on it.

### Semantic policy version

`semantic_policy_version` covers cover-discovery precedence, image-only-spine
classification, transparent-wrapper rules, normalized-path semantics, URL
fragments, root escapes, image-probe interpretation, the placeholder versus
image-page decision, deferred-candidate limits, fallback-record limits,
path-table limits that can affect classification, the deterministic behavior
when those limits are exceeded, render-target enumeration order, and event-stream
finalization.

A semantic-policy mismatch makes the affected artifact or semantic cache
ineligible.

Every constant that can change the finalized semantic event stream is part of the
semantic policy identity. It is either fixed by the version, or advertised in
capabilities and folded into the effective semantic identity both sides compute.

Running out of room in an optional cache does not silently change the live
semantic event stream. When a cache cannot encode a stream that was already
finalized, firmware keeps the same live parse behavior and declines or truncates
publication under an explicitly versioned all-or-nothing or prefix-safe rule.

### Producer decoder policy version

`producer_decoder_policy_version` is read together with `producer` and
`production_method`.

For `HostDecoded` it covers the pinned Wasm decoder implementation and revision,
supported host source formats, JPEG marker and entropy interpretation, JPEG IDCT,
JPEG YCbCr to RGB conversion, PNG sample decoding, PNG palette and `tRNS`
handling, grayscale-plus-alpha handling, source color-space handling, ICC
interpretation, PNG gamma interpretation, EXIF parsing, EXIF-orientation
normalization, source-specific conversion and rounding before canonical output,
and the rule for choosing Gray8 versus RGBA8 output.

For `DeviceDecoded` it covers the firmware JPEG decoder implementation and
revision, the supported JPEG subset, marker and entropy interpretation, IDCT and
reduced IDCT, sampling and component handling, YCbCr or grayscale reconstruction,
EXIF parsing, supported orientation handling, source-to-Gray8 or source-to-RGBA8
behavior, and tile-delivery ordering and rounding.

A producer decoder does not apply final alpha compositing over white, final RGB
to luma conversion, or final luma clamping. Those belong to the canonical luma
policy alone.

A producer-decoder mismatch invalidates artifacts from that producer only.
Capabilities expose `host_producer_decoder_policy_version` and
`device_producer_decoder_policy_version` separately.

### Canonical luma policy version

`canonical_luma_policy_version` operates only on a producer's upright output. It
covers opaque `Gray8` pass-through, non-premultiplied RGBA compositing over
white, RGB to luma coefficients, integer rounding, and clamping to 0 to 255.

A producer may emit `Gray8` only when every decoded source pixel is known to be
fully opaque. It must emit non-premultiplied `RGBA8` whenever the source has
effective transparency, including PNG alpha channels, grayscale-plus-alpha,
palette alpha, `tRNS`, and any other supported mechanism that can make a decoded
pixel nonopaque. Discarding transparency before the canonical luma stage is a
producer-decoder error.

This policy does not cover source parsing, ICC or gamma interpretation, EXIF
parsing, orientation normalization, JPEG IDCT, YCbCr conversion, or PNG sample
decoding.

A canonical-luma mismatch invalidates both host-decoded and device-decoded
artifacts.

### Geometry, resampler, and dither policy versions

`geometry_policy_version` covers cover fill-and-crop behavior, image-page
contain behavior, centering, padding, destination-rectangle interpretation, and
coordinate rounding.

`resampler_policy_version` covers the shrink algorithm, the enlargement
algorithm, filter weights, the fixed-point representation, edge handling, and
integer rounding.

`dither_policy_version` covers the dither algorithm, traversal order, error
distribution, clamping, thresholding, and packed-bit polarity.

### Policy applicability

| Production method | Semantic | Producer decoder | Canonical luma | Geometry | Resampler | Dither |
|---|---:|---:|---:|---:|---:|---:|
| `HostDecoded` | required | host version | required | required | required | required |
| `DeviceDecoded` | required | device version | required | required | required | required |

A required field must match the accepted version exactly. Unknown production
methods are rejected.

There is no third production method. The 2026-07-26 draft carried a
`LegacyRasterImport` method, a `legacy_import_policy_version`, and a whole
namespace for wrapping an existing `X4CV` cover into a modern artifact without
re-decoding it. That is retired. Only `tools/preview` writes `X4CV`, no shipped
build has ever drawn a cover, and no card in the field depends on one, so the
import would preserve nothing a regeneration cannot produce. A stale `X4CV` file
reads as absent and is deleted with the rest of an invalidated artifact set.
Reversing this costs one production method and one policy field, and it should be
reversed only if `X4CV` covers turn out to exist somewhere that matters.

## Device capabilities handshake

The browser fetches device capabilities before decoding or rasterizing anything.

A versioned capabilities response carries:

- protocol version;
- active board profile, `X3` or `X4`;
- display width and height;
- cover target width and height;
- the active reader content rectangle, as x, y, width, height;
- supported render-bundle schema versions;
- accepted production methods for upload;
- accepted pixel formats;
- accepted host source formats: baseline JPEG, progressive JPEG, PNG;
- semantic policy version;
- the semantic limits that can affect classification or finalization:
  - maximum deferred image candidates per spine item,
  - maximum fallback records per section,
  - maximum normalized paths per section,
  - maximum normalized path bytes,
  - any other bound the semantic policy includes by reference;
- canonical luma, host producer-decoder, device producer-decoder, geometry,
  resampler, and dither policy versions;
- the source-digest algorithm and the payload-checksum algorithm;
- host preprocessing input limits: accepted source media types, maximum metadata
  dimensions representable in a manifest, and the maximum source-entry compressed
  and uncompressed bytes the workflow accepts, where any;
- device source-decoder limits: maximum JPEG width and height, maximum
  components, maximum MCU geometry, maximum decoder row or tile bytes, maximum
  supported ZIP-entry uncompressed bytes, and supported JPEG modes and sampling;
- uploaded output and bundle limits: maximum asset count, maximum normalized
  asset-path bytes, maximum manifest bytes, maximum cover payload bytes, maximum
  image-page payload bytes, maximum individual asset bytes, maximum aggregate
  payload bytes, and maximum total request bytes;
- the fixed `XTCV` stride rule and the resident cover capacity;
- the M3 page-cache limits when M3 is enabled: maximum retained pages, maximum
  retained payload bytes, minimum free space, eviction policy identifier, and
  storage layout version.

The advertised geometry and limits are authoritative, and the browser does not
hardcode X3 or X4 assumptions.

Host preprocessing applies semantic limits while enumerating render targets, host
input limits while parsing and decoding, and output limits after rasterization.
It does not reject a source image merely because its dimensions or decoded row
width exceed the device decoder's workspace. A large host-decodable image is
accepted when the host decoder can process it safely, the resulting raster
satisfies output geometry and bundle limits, its stored source metadata fits the
manifest field widths, and every other host input limit is satisfied. Device
decoder limits apply only to on-device probing and decoding.

Capabilities do not advertise a cover geometry whose packed payload exceeds the
firmware's compile-time resident-cover capacity.

Protocol version 1 accepts bundle uploads for the active board profile only.

An unsupported capabilities or bundle version disables preprocessing without
disabling ordinary EPUB upload. A capabilities response is internally invalid,
and preprocessing is disabled, when a semantic bound the advertised policy
requires is omitted, an output limit cannot contain the advertised target
geometry, the accepted pixel formats cannot represent the advertised cover, the
resident-cover capacity is smaller than the advertised packed cover, or a limit
exceeds the firmware field width used to validate it.

---

# Part 2 of 6: Artifact identity, scope, and durability

## Where artifacts live

Artifacts belong to the copy that produced them, in that copy's existing cache
directory:

```text
/READER/CACHE2/E<hash>/BOOK.BIN        already there
/READER/CACHE2/E<hash>/CONT.BIN        already there
/READER/CACHE2/E<hash>/SECTIONS/...    already there
/READER/CACHE2/E<hash>/WHO.BIN         already there: owner locator + evidence
/READER/CACHE2/E<hash>/RENDER.MF       the artifact manifest
/READER/CACHE2/E<hash>/COVER.BIN       XTCV cover payload
/READER/CACHE2/E<hash>/IMAGES.BIN      concatenated image-page rasters
/READER/CACHE2/E<hash>/NOCOVER.BIN     recorded deterministic cover failure
```

No second store, no content-addressed tree, no reference counts. Three
consequences make this the right shape:

- deletion needs nothing new. The orphan sweep already retires a cache directory
  whose claim names a locator that is definitively gone, and it retires these
  files with it;
- Library Identity R12 is satisfied without a garbage collector. Two identical
  copies each hold their own artifacts, so deleting one cannot invalidate
  anything the other still uses;
- an artifact set is validated against the same claim the rest of the directory
  is validated against, so there is one answer to "whose cache is this".

The cost is duplicate bytes for duplicate copies. That is a bounded cost paid
only by a card that holds the same book twice, and the alternative is a shared
store with lifetimes, which is the machinery this project should not be adding.

## What an artifact is bound to

Every manifest header carries the identity of the copy it was made for and the
policies it was made under:

```text
board_profile
local_cache_identity   root byte, locator, byte size, and source_hash_at over them
digest_witness         Option<SourceDigest>, present when the producer read the bytes
producer               Host | Device
production_method      HostDecoded | DeviceDecoded
semantic_policy_version
canonical_luma_policy_version
producer_decoder_policy_version
geometry_policy_version
resampler_policy_version
dither_policy_version
```

The full locator and byte size are stored rather than the 32-bit hash alone. The
directory name is 28 bits of that hash, so two locators can share it, and the
existing cache headers already compare the whole hash and the size for exactly
this reason. An artifact set whose stored locator is not the one the claim names
reads as absent.

`digest_witness` is the bytes the producer actually read. A host bundle always
carries one, because the upload hashed the stream. A device artifact carries one
only when something else had already read the file whole. It is evidence in the
Source Identity sense: it narrows a later promotion question and proves nothing
on its own.

### Geometry binding, and the orientation problem

A cover is fit to the profile's cover target, which does not depend on the reader
layout, so a cover binds to the board profile alone.

An image page is fit to the reader content rectangle, which does depend on
orientation and margins. The 2026-07-26 draft advertised one "active reader
content rectangle", which silently meant that flipping orientation invalidated
every uploaded image raster with no browser present to remake them.

So capabilities advertise **every page-render rectangle the device can present**,
each with an identifier, and an image-page asset records which one it was fit to:

```text
page_render_targets:
  - id: 0, portrait  content rect
  - id: 1, landscape content rect
```

The browser produces one asset per eligible image per advertised target. The
aggregate payload limit bounds the total, and a device that would rather not pay
for the second orientation advertises one target. An image page with no asset for
the current target falls back to device source decode, and then to the
placeholder, which is the M2B path unchanged.

A change to a target's rectangle, from a margin or layout change, invalidates the
assets bound to it. That is the same rule the pagination cache already follows,
and it is why `LayoutId` in Source Identity R6 exists.

## Scope: local by default, shared by promotion

Artifacts are produced, stored, and consumed under the local cache identity of
the file that made them. Nothing on the reading path asks a digest question, and
nothing waits on one. This is Source Identity R5 applied literally: reusing state
scoped to one physical file is a local validity check, not a claim about bytes.

A digest authorizes exactly two things this project cares about, and both are
optimizations rather than requirements:

**Adoption by an identical copy.** A card holding the same book twice can spend
one full read to learn that the second copy holds the bytes the first one's
artifacts describe, and copy the artifact set across rather than rebuilding or
re-uploading it. Under Source Identity R11 both files must be read in the current
epoch, because a stored digest is evidence and identical bytes deliberately
produce identical digests.

**Recovery after a move.** A move changes the locator, so it changes the local
cache identity and orphans the directory. Library Identity's reconciliation pass
already establishes that a missing record and an unclaimed file are the same
copy, confirmed by `SourceDigest`, and reports `FoundAgain`. An artifact set may
follow that verdict.

Adoption copies bytes; it does not create a reference. A copied set is rewritten
with the recipient's locator and byte size in its header, so it is bound to the
recipient exactly as if it had been produced there.

## Following a move

The move repair runs inside the catalog scan, which the reader is waiting on, so
what it carries has to stay small.

- **The cover carries with the position.** It is one file of a few kilobytes,
  `carry_position_for_move` is already opening both directories, and the Home view
  is the first thing a reader sees after the scan.
- **The image-page set relocates at idle**, from the same idle branch of the
  display task that finishes a cold build. Until it lands, an image page falls
  back to device decode or to the placeholder.
- **Nothing is deleted from the donor until the recipient validates.** An
  interrupted relocation leaves the artifacts where they were, and the next idle
  pass tries again.

The existing decision that pagination rebuilds rather than relocating stands.
Rebuilding pagination is seconds of work the device can do alone; a host bundle
cannot be rebuilt on the device at all, which is what makes images worth carrying
when pagination is not.

## Replacement and invalidation

A managed replacement preserves the `BookId` and changes the bytes (Library
Identity R4 and R17). The artifacts belong to the old bytes, so they do not
survive it.

Usually this falls out for free: a replacement of a different size changes the
byte size, so it changes the local cache identity, and the old directory is
orphaned and swept. A same-size replacement at the same locator does not, and the
device would keep serving the previous edition's cover, pagination, and text
cache.

**The replace transaction invalidates the copy's derived state.** It is the one
moment the device knows the bytes changed, and it already has the copy in hand.
This is stated here because image artifacts make the stale-cover case visible,
but it protects `BOOK.BIN`, `CONT.BIN`, and the section files too, and it closes a
gap that predates this PRD.

An unexplained external replacement gets no such signal. Source Identity records
that accepted consequence: the copy may serve its own stale cache until an
operation that needs byte equivalence discovers the change. An artifact set is
part of that copy's own cache and inherits the same bounded exposure. It does not
inherit anything worse, because an artifact is only ever served to the copy whose
locator its header names.

Every other invalidation is a mismatch in the header: a different board profile, a
different policy version, a different page-render target, a different production
method. Each reads as absent and rebuilds.

## Durability class

**Artifacts are rebuildable derived state.** They are not authority over anything
a user can lose. A torn artifact costs a decode, an upload, or a blank cover
area. This is the difference between this PRD and the draft it replaces, and
almost everything the old Part 2 specified follows from getting it wrong.

Publication is therefore not a transaction:

1. write the payload files and the manifest under staging names in the copy's
   cache directory;
2. validate the staged set whole: lengths, ranges, checksums, cardinality, and
   header binding;
3. delete the current manifest, which retires the previous set;
4. rename the payloads into place, over the predecessor's;
5. rename the manifest into place, which publishes the new set.

The manifest is the only thing that makes a set selectable, and it names every
payload's length and CRC, so a set is used only when its manifest is in place and
everything it names validates. The manifest is removed before any payload is
overwritten, so no interruption can leave a manifest describing payloads it did
not describe when it was written.

The window between step 3 and step 5 is a window with no cover and no image
pages, not a window with the wrong ones. An interruption inside it costs a
regeneration or a re-upload, which is what "rebuildable" is worth paying. Keeping
the predecessor across that window would mean two payload generations and a rule
for choosing between them, which is the A/B machinery this class of state does
not earn.

A staged file left behind by an interruption is reclaimed by the next publication
or by the orphan sweep, and is not selectable in the meantime because no manifest
names it.

The rename primitive is the one #75 added to the driver fork and the installer
already uses.

What this deliberately does not have:

- no `durable_sync`, no aligned commit sector, no A/B generations, and no
  prepared or committed record states;
- no hardware power-cut gate. The card-physics rig belongs to the ledger and the
  journals, which hold state that cannot be rebuilt, and Library Identity's
  durability tests own it;
- no interaction with `INSTALL.JNL` or `RECLAIM.JNL` beyond serialization. No
  digest and no `BookId` enters either journal, which is what Library Identity's
  final "done when" preserves.

What it does still require:

- a partially written payload is not adopted, which the manifest's declared
  lengths and CRCs decide;
- a set that fails validation is treated as absent and reclaimed rather than
  repaired;
- validation of the requested asset happens before its bytes reach the panel, not
  after.

## Serialization with storage mutations

Physical Folder R8 keeps one globally serialized mutation at a time, and the
library-metadata intent can span several journal records. Artifact publication
joins that discipline:

- a bundle upload is refused while `INSTALL.JNL`, `RECLAIM.JNL`, or a library
  intent stands, and an install is refused while a bundle upload is in flight;
- device-side publication, M1B covers and M3 pages, runs from the display task's
  idle branch and takes the same serialization, so it cannot allocate clusters
  while a reclaim is unsettled;
- a publication that finds its copy gone, its locator no longer resolving, or its
  claim naming another book abandons without writing.

Artifact publication does not create a lock domain of its own. A path says where
to act; the storage owner says whether anything may act at all.

## Cooperative long-running work

ZIP scanning and inflation, image probing, decoding, resampling, dithering,
validation, and relocation can all exceed one cooperative-executor turn. Each is
a resumable bounded job:

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

- one step performs a bounded number of input bytes, ZIP records, MCUs, source
  rows, destination rows, asset records, or relocation entries;
- software-controlled work in one step stays inside `MAX_IMAGE_WORK_SLICE_US`;
- blocking SD requests use bounded transfer sizes, and SD latency is reported
  separately from software-controlled execution time;
- resumable state is fixed capacity, and execution returns to the executor
  between steps;
- input, power, networking, display, and storage tasks stay runnable, and no step
  waits on a channel whose consumer can be waiting for the current owner;
- each step checks cancellation, and replacement, deletion, book exit, a
  superseding render request, and shutdown all cancel affected jobs;
- cancellation is observed no later than the end of the current step plus one
  in-flight bounded SD request;
- partial decoded output stays private: no panel refresh and no publication
  follows a cancellation.

The existing sliced jobs are the model, the background section build and the
sliced digest read that fills the cache claim's evidence slot, and image work
sizes its slices against theirs since it is the same reader waiting either way.

---

# Part 3 of 6: Render-bundle transport, manifest, covers, and host preprocessing

## Upload response

The EPUB upload endpoint is unchanged in shape and behavior. Its response gains
the two facts the browser needs in order to send a bundle:

```text
book_id              the copy's BookId, 32 lowercase hexadecimal characters
source_digest        byte length plus SHA-256 of the installed bytes
locator_root         Library | CardRoot
active_board_profile
capabilities_version
render_bundle_enabled
```

The digest costs nothing: `StagedUpload::write` already hashes the stream. The
`BookId` comes from the ledger record the install publishes.

An upload that lands but cannot report a `BookId`, because the ledger is
unavailable, still succeeds as an upload and reports `render_bundle_enabled` as
false. Uploading a book is not allowed to fail because its pictures cannot be
preprocessed.

## Render-bundle transport

### Endpoint

```text
POST /render-bundle
Content-Type: application/octet-stream
Content-Length: <exact framed request length>
```

Protocol version 1 accepts only `producer = Host`,
`production_method = HostDecoded`, and the active board profile.

### Framing

One forward-streamed request:

```text
transport header
client manifest template
cover payload
image payload
```

The transport header carries:

```text
magic
transport_version
header_length
book_id
source_digest        byte length and SHA-256
board_profile
bundle_schema
manifest_template_length
cover_payload_length
image_payload_length
total_request_length
header_crc32
```

The manifest template carries the producer, the production method, the applicable
policy versions, sorted asset records, the normalized path table, payload offsets
and checksums, and a zeroed body checksum. The template is a client's claim, not
an authority: the device rebuilds the manifest it stores from what it validated.

### Handling

1. validate framing and the exact request length;
2. check the capability limits before reading payload bytes;
3. resolve `book_id` to a ledger record and its locator, and refuse an unknown,
   missing, or ambiguous copy;
4. confirm the file at that locator holds the named digest. Free when this
   session installed those bytes, and a full read otherwise, under Source
   Identity R11;
5. refuse if a storage mutation is live;
6. stream the payloads to staging names in the copy's cache directory, rejecting
   early EOF, excess bytes, and inconsistent lengths;
7. validate the staged set whole: structure, ranges, CRCs, cardinality, path
   table, and header binding;
8. build the stored manifest from the validated set;
9. publish it by the sequence in Part 2: retire the current manifest, rename the
   payloads into place, rename the manifest last.

A failed request modifies nothing a reader can see. It cannot touch the source
EPUB, the ledger, or another copy, and it cannot touch the previous artifact set
either, since every rejection above happens before that set is retired.

Step 4 is the whole authority story, and it is worth being explicit about what it
buys. A digest confirmed in this epoch says the bytes at that locator are the
bytes the browser preprocessed. If the file was replaced between the upload and
the bundle, the digest disagrees and the bundle is refused. There is no token to
go stale, no generation to compare, and no window in which a bundle can attach to
bytes nobody hashed.

What it can cost is a full read. In the ordinary flow it costs nothing, because
the same session installed those bytes and hashed them while streaming. After a
reboot, a remount, or a card reinsertion the epoch is gone and the file has to be
read, which measured between roughly 15 and 50 seconds for a large book on a real
card. So the confirmation runs as a bounded cooperative job while the request is
held, under the responsiveness rules in Part 6, and the device may instead answer
with a retryable status telling the browser to send the bundle again once the
digest is available. It does not hold the executor for tens of seconds, and it
does not accept a bundle on a stored digest nobody confirmed.

## Wi-Fi memory and resource budget

The upload path uses bounded socket and HTTP buffers, one fixed transport-header
buffer, bounded record buffers, a fixed-capacity identity index, bounded path
comparison, and bounded checksum state. It does not retain the complete manifest
or any payload in RAM.

The session heap is the constraint that already bit this project once. Anything
this path claims from the donated regions is claimed early, while they are
pristine, for the reason the retired implementation recorded: radio init
fragments them until no image-sized hole survives, and total free bytes say
nothing about whether a contiguous claim will succeed.

M0R reports peak Wi-Fi heap, minimum free heap, peak stack, identity-index bytes,
path-buffer bytes, manifest RAM, upload throughput, staged-set validation reads,
publication cost, and behavior under interrupted and malformed requests.

## Artifact set model

A *bundle* is what the browser sends. A *set* is what the device keeps once
it has validated one. The two have different shapes on purpose: a bundle is a
single framed request with a client's claims in it, and a set is files the
device wrote and can revalidate on its own.

### Contents

One artifact set per producer per board profile, in the copy's cache directory:

- `RENDER.MF`, the manifest;
- `COVER.BIN`, an `XTCV` cover, optional;
- `IMAGES.BIN`, concatenated image-page rasters, optional, host sets only before
  M3.

A set is homogeneous: one producer, one production method.

Host and device sets coexist. A device cover does not displace a host cover, and
publishing one profile's set does not touch another's. M3 device pages use the
separate bounded slot cache in Part 5 rather than extending a set by copying
every retained payload forward.

### Precedence

Cover: host asset, then device asset, then device source decode, then blank or a
recorded deterministic negative result.

Image page: host asset for the current page-render target, then an M3 device
page-cache asset, then device source decode, then the retained placeholder.

## Manifest schema

### Header

```text
magic
schema_version
header_length
logical_manifest_length
producer
production_method
board_profile
local_cache_identity     root byte, locator length, locator, byte size, hash
digest_witness_present
digest_witness           byte length and SHA-256 when present
semantic_policy_version
canonical_luma_policy_version
producer_decoder_policy_version
geometry_policy_version
resampler_policy_version
dither_policy_version
pixel_format
cover_target_geometry
page_render_target_count
page_render_targets      id and rectangle for each
asset_count
asset_record_bytes
normalized_path_bytes
cover_payload_length
image_payload_length
body_crc32
```

The body checksum covers the header with its own field zeroed, the asset records,
and the path table.

Validation rejects inconsistent lengths, unknown versions, arithmetic overflow,
nonzero reserved fields, and trailing data.

### Eligibility

A set is eligible only when the manifest validates, the stored local cache
identity matches the claim of the directory it sits in, the board profile
matches, the production method is known, every required policy version matches,
and every payload file has exactly the declared length.

## Asset records

```text
stable_asset_id
role                    Cover | ImagePage
normalized_href_offset
normalized_href_length
page_render_target_id   ImagePage only; zero for a Cover
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

Rules:

- the stable ID derives from the complete normalized path, the role, and the
  target id, and complete paths are compared whenever stable IDs collide;
- all offsets and ranges use checked arithmetic;
- output geometry fits the active capabilities;
- source metadata fields fit the manifest limits, and need not fit the device
  decoder workspace for a host asset.

## Cardinality and role invariants

- zero or one `Cover` record;
- a `Cover` references only `COVER.BIN`, and `COVER.BIN` exists exactly when a
  `Cover` record does;
- every `Cover` payload is valid `XTCV`;
- an `ImagePage` references only `IMAGES.BIN`, and `IMAGES.BIN` exists exactly
  when image records do;
- at most one `ImagePage` record per normalized path per page-render target;
- every declared page-render target id is one the manifest header lists;
- payload ranges do not overlap, every payload byte belongs to exactly one
  record, and there is no undeclared padding or trailing payload.

## Ordering and bounded validation

Records are sorted by payload file, then payload offset, then role, then
normalized path bytes.

Publication validation runs as a header pass, a record and range pass, a path and
identity pass, a complete payload CRC pass, and a final binding check before the
rename.

Runtime selection validates the manifest structure without reading unrelated
payload bytes, then validates the requested asset's CRC and format. Structural
corruption invalidates the set. Corruption inside one requested asset invalidates
that asset for the mount while the rest of the set stays usable, as long as the
ranges are structurally sound.

Loading a cover reads no `IMAGES.BIN` bytes. Loading one image page reads only its
declared range plus bounded metadata.

## Cover format and resident representation

### `XTCV`

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
payload_length
payload_crc32
header_crc32
```

The header identity matches the manifest.

Version 1 stride:

```text
stride = ceil(width / 8)
payload_length = stride * height
```

Validation rejects zero dimensions, unsupported geometry, overflow, a stride
mismatch, an unsupported format or polarity, a payload-length mismatch, a payload
exceeding the resident capacity, and trailing bytes.

`XTCV` replaces `X4CV`, which is fixed at 202x303 with a compile-time stride and
carries no producer, policy, or checksum fields. A file with the old magic reads
as absent.

### Resident cover

Firmware defines `MAX_COVER_BYTES`. `ReaderStore` retains the payload, its
length, and the runtime width, height, and stride, plus a validity flag.

Cover loading validates the manifest structure, selects the `Cover` record, reads
and CRC-checks only that range, validates `XTCV`, copies exactly the initialized
payload, and records the runtime geometry. `UiCover` carries that geometry, and
rendering uses it rather than a compile-time stride.

## Browser preprocessing

### Host decoder policy

The shipping browser uses a pinned deterministic Wasm decoder. Browser Canvas is
not normative, since its color management and resampling are not specified
across engines.

Initial host formats: baseline JPEG, progressive JPEG, PNG.

### Host input limits

Host preprocessing applies the accepted media types, the maximum representable
source metadata dimensions, the maximum source-entry compressed and uncompressed
sizes where imposed, and the browser's own safety bounds. It does not apply
device JPEG workspace limits, so a source too large for the device may still
produce a valid host asset.

### Decoder-output contract

The host decoder emits upright `Gray8`, or upright non-premultiplied `RGBA8`.
`Gray8` is permitted only when every effective decoded pixel is fully opaque, and
`RGBA8` is required for any effective transparency, including alpha channels,
grayscale-plus-alpha, palette alpha, and PNG `tRNS`.

The host decoder owns source parsing, IDCT, JPEG color conversion, PNG sample
decoding, palette and transparency interpretation, color-space, ICC and gamma
handling, EXIF and orientation, and source-specific rounding. The canonical luma
stage owns opaque pass-through, RGBA compositing over white, RGB to luma
conversion, and the final rounding and clamp. Nothing else.

### Policy closure

M0R freezes the semantic policy and its bounds, the host decoder policy, canonical
luma, geometry, resampler, dither, serialization, and every coefficient and
rounding rule. M0D later freezes the device decoder policy.

### Cover discovery and geometry

Discovery order: EPUB 3 `cover-image`, then EPUB 2 cover metadata, then a
conservative ID or href fallback.

Geometry: aspect-preserving fill, centered crop, no stretching, to the
active-profile target dimensions.

### Home rendering

M1A draws the active book's cover in the Home view, opaque and clipped, blank
when absent, using the runtime width, height, stride, and payload length.

The placement is defined in M1A against the current Home composition, which is a
title page with a wrapped title, an author line, a progress rule, and a chapter
colophon, laid out differently in portrait and landscape. The 202x303 legacy
target is the starting candidate for the cover geometry and is not a constraint:
the format carries its dimensions, and capabilities advertise them. M1A re-takes
the Home goldens for both device configurations.

### On-device cover generation

1. try a host cover, then a device cover;
2. read the recorded deterministic negative state;
3. discover and probe the source cover;
4. decode supported JPEG through bounded jobs;
5. apply canonical luma, geometry, resampling, and dither;
6. publish a cover-only device set by stage and rename;
7. validate and load it;
8. leave a successful text open untouched whatever the cover did.

---

# Part 4 of 6: Image semantics, typed caches, rendering, and refresh

## Recorded negative cover state

`NOCOVER.BIN` stops the device retrying a cover generation that deterministically
failed. Transient failures are not recorded.

```text
magic
schema_version
local_cache_identity
board_profile
producer
production_method
normalized_cover_path_or_absent
semantic_policy_version
canonical_luma_policy_version
producer_decoder_policy_version
geometry_policy_version
resampler_policy_version
dither_policy_version
deterministic_failure_class
crc32
```

Failure classes:

```text
NoCoverCandidate
UnsupportedSourceFormat
UnsupportedOrientation
SourceImageMalformed
SourceImageExceedsConfiguredLimits
DecoderFeatureUnsupported
DeterministicRasterPipelineFailure
```

Any mismatch in identity, profile, producer, method, or an applicable policy
invalidates the record. It is an accelerator, like every other file in the
directory: losing it costs one retry, so it is written and validated the same way
and gets no publication ceremony of its own.

## Image-page eligibility

A dedicated image page is produced only when the complete rendered body of one
spine item is one eligible JPEG or PNG image.

Transparent wrappers may include `html`, `body`, `section`, `div`, `figure`, `p`,
and `a`.

Disqualifiers include meaningful text, a heading, a caption, a second image,
another media element, navigation, any visible content before or after the image,
and an unsupported host source format.

Aspect ratio does not affect eligibility.

## Order-preserving deferred candidate

```text
NoCandidate
WithheldCandidate
Disqualified
```

- an image after meaningful content emits its placeholder immediately;
- the first image before meaningful content is withheld together with its exact
  finalized placeholder;
- later disqualifying content emits the withheld placeholder first, then itself;
- a second image emits both placeholders in source order;
- at spine end a sole withheld candidate is probed after the ZIP callback ends,
  and becomes either an `ImagePage` with its retained fallback or the ordinary
  placeholder;
- `SpineEnd` follows.

The maximum number of withheld candidates is fixed at one in protocol version 1.
Any future bound that can change final classification is part of
`semantic_policy_version`, or is advertised and folded into the effective
semantic identity. Cache storage capacity does not alter this sequence.

## Image metadata probe

The bounded forward probe recognizes baseline JPEG, progressive JPEG, and PNG,
and reports format, dimensions, orientation status, and whether the prefix was
complete, unsupported, malformed, or insufficient.

It does not allocate, seek, decode pixels, or retain the complete image.

Host target enumeration applies host metadata limits. Device source decode later
applies device decoder limits.

## Typed content cache

M2 increments `CONTENT_VERSION`, currently 3.

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
local_cache_identity
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

Event kinds: `Text`, `ImagePage`, `SpineEnd`. Every event begins with its kind,
flags, and record length.

`ImagePage`:

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

- the path is valid normalized UTF-8, and the stable ID recomputes from the role
  and the complete path;
- source dimensions fit the schema field widths, and need not fit the device
  decoder limits for host-only rendering;
- fallback bytes reproduce the prior centered italic placeholder exactly, and are
  nonempty and bounded;
- reserved fields are zero.

Tables are deduplicated by exact bytes in first-seen finalized order. Stable-ID
equality does not substitute for complete-path equality.

### Capacity behavior

```text
MAX_IMAGE_FALLBACK_RECORD_BYTES
MAX_CONTENT_FALLBACK_TABLE_BYTES
MAX_CONTENT_PATH_TABLE_BYTES
MAX_CONTENT_EVENT_COUNT
```

When a finalized event stream cannot fit, the event stream does not change, an
`ImagePage` is not reclassified as a placeholder to make it fit, no partial or
semantically different cache is published, the live parse continues, and a later
open may reparse the source. Host target enumeration is unaffected.

Publication is all-or-nothing per section unless a future schema defines a
prefix-safe representation whose replay is provably identical. These limits
affect cache availability, not classification.

### Replay guarantee

A valid replay requires no source reopen for classification, probing, path
normalization, source format, dimensions, orientation, or fallback
reconstruction. A malformed, incomplete, mismatched, or old cache is rejected.

### Golden requirement

For every fixture: full source parse, finalized event capture, attempted cache
serialization, and, when serialization succeeds, a cache-only replay compared
against every finalized event, normalized path, fallback record, image index,
section byte, and page record.

For capacity-overflow fixtures, host enumeration and the firmware's full parse
still match, cache publication fails deterministically, and no semantic event
changes.

## Section and page cache

M2 increments `CACHE_V2_VERSION`, currently 26.

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

A text page references text blocks. An image page references one section-local
image record:

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

The section image table is bound to the local cache identity and the semantic
policy, checksummed, bounded, and deterministically ordered. A capacity failure
prevents section-cache publication and does not alter the live event stream.

## Image-page rendering

1. read and validate the section image and fallback records;
2. try the host asset for the current page-render target: validate the manifest
   structure, then read and CRC-check only the requested range;
3. try the M3 device page-cache asset;
4. check the session failure memo;
5. open the source EPUB, locate and probe the image;
6. select a supported device reduction;
7. decode and rasterize through bounded jobs;
8. recheck cancellation before presenting;
9. expose only a fully completed private framebuffer;
10. on failure, clear partial output and render the retained fallback text.

No partial image reaches the panel, and a stale or cancelled job produces no
panel update and no publication.

## Render outcome and refresh planning

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

- a successful image reports `Image` with a minimum of `Full`;
- a fallback reports `Placeholder`;
- the planner computes its normal policy and then raises it to the minimum;
- the display task stores `last_presented_content_kind`, updated only after a
  successful panel flush;
- entering an image, moving image to image, and leaving an image each require
  `Full`;
- a failed flush preserves the previous history, and a cancelled render does not
  alter it.

## Session failure memo

```text
local_cache_identity
section_index
section_image_index
device_producer_decoder_policy_version
geometry_policy_version
resampler_policy_version
dither_policy_version
```

Only deterministic failures are memoized. The bounded memo clears when leaving
the book, when the copy's identity changes, or when an included policy changes.

---

# Part 5 of 6: Device JPEG decoder, raster pipeline, and bounded device pages

## On-device JPEG decoder

### Selection hierarchy

1. adapt `tjpgdec-rs`;
2. implement a constrained Rust SOF0 decoder;
3. port only the required JPEGDEC algorithms;
4. consider a full JPEGDEC port only if later features justify it.

### Source-input contract

The decoder reads a forward-only resumable stream. It accepts arbitrary positive
chunk sizes, handles a boundary at any byte, distinguishes truncation from
malformed data from an unsupported feature from an I/O failure, does not seek
backward, does not require the complete compressed entry, enforces compressed and
uncompressed bounds, stops after a bounded input or MCU budget, resumes without
restarting the entry, and checks cancellation between steps.

The container bounds it enforces are the ones firmware advertises:
`MAX_EPUB_BYTES`, `MAX_ZIP_OFFSET`, `MAX_ZIP_ENTRY_COMPRESSED_BYTES`,
`MAX_ZIP_ENTRY_UNCOMPRESSED_BYTES`, and no ZIP64. Every classic-ZIP offset and
derived range uses checked arithmetic. A source that fails these bounds is not
decoded, and its book still opens: the bound gates image work, not reading.

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

Initial device output is opaque `Gray8`.

Requirements:

- coordinates are in upright reduced-image space, and the initial supported
  orientation is absent or 1;
- tiles arrive top to bottom then left to right, with no overlap and no repeat,
  and every output pixel is delivered exactly once;
- tiles stay inside the approved MCU bounds, with an initial target maximum of
  16x16;
- stride and slice calculations are checked, and external input cannot produce an
  out-of-range coordinate;
- borrowed pixels expire when the callback returns;
- a callback failure aborts the decode, and no callback follows a terminal
  failure.

A bounded MCU-row strip may replace tiles only after M0D proves a fixed maximum
byte count, a workspace that fits both device configurations, equivalent ordering
and ownership, and no full-image buffering.

### Raster-pipeline contract

The streaming pipeline may retain decoder state, one tile or bounded strip,
horizontal scaler state, the minimum vertical source-row ring, one destination
luma row or segment, the current and next dither error rows, the destination
packed raster or private framebuffer, and the ZIP inflate scratch.

It may not retain the complete source image, the complete reduced source image,
unbounded source rows, or any buffer sized from unvalidated dimensions.

Cover crop and page containment are computed from probed dimensions before the
decode, and pixels outside the cover crop may be discarded early.

### Device-only workspace limits

M0D defines and advertises:

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

These apply only to on-device source decode, and are advertised separately from
host input limits and uploaded-output limits. A decoder candidate is rejected if
the EPUB inflate scratch cannot stay resident alongside it.

### Initial firmware JPEG contract

Supports SOF0, 8-bit Huffman JPEG, grayscale, YCbCr 4:4:4, 4:2:2 and 4:2:0 where
verified, one grayscale or interleaved three-component scan, restart intervals,
8-bit quantization, and orientation absent or 1. Optional 16-bit quantization
stays measurement-gated.

Refuses SOF1, progressive, multi-scan baseline, arithmetic coding, lossless,
12-bit, CMYK or YCCK, unsupported sampling or transform, unsupported orientation,
and malformed, truncated, or oversized input.

## Decoder verification

M0D includes two independent reference decoders; marker, entropy, IDCT, color,
tile, and pipeline unit tests; a valid-format matrix; an unsupported and
malformed matrix; arbitrary input-chunk metamorphic tests; tile-boundary
metamorphic tests; injected I/O failures; callback-failure tests; guard regions;
fuzzing; Miri where applicable; sanitizer native runners; maximum-dimension
tests; scaler and dither guard tests; proof that no full-image buffer exists; and
workspace and stack measurements for both device configurations.

A textual or agent-assisted port is not evidence of correctness. A decoder that
matches pixels but violates bounded delivery is rejected.

## Scaling and dithering

Reduced IDCT candidates are 1/1, 1/2, 1/4, and 1/8. Choose the largest reduction
that does not undershoot the dimensions the destination crop or contained page
requires, using checked rational arithmetic.

Cover geometry fills, crops centered, and does not stretch. Page geometry contains,
centres, and pads with white.

Resampling candidates: area or box shrink, fixed-point bilinear enlargement,
separable horizontal and vertical processing. The selected filter must operate
inside the bounded tile or strip contract.

Dithering candidate: fixed-point Floyd-Steinberg with current and next
destination-width error rows, falling back to ordered Bayer. Host policy freezes
in M0R, and M0D proves the device produces equivalent output inside its
workspace.

## M3 bounded persistent device image pages

### Design choice

M3 does not rebuild a monolithic device set to store a page. It uses a fixed
number of page slots, an index naming them, and deterministic FIFO eviction, all
inside the copy's cache directory:

```text
/READER/CACHE2/E<hash>/PAGES/INDEX.BIN
/READER/CACHE2/E<hash>/PAGES/P00.BIN ... P<N>.BIN
```

Adding one page writes one slot payload and one bounded index. It does not copy
retained payload bytes.

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

### Slot and index

A slot payload carries:

```text
magic
schema_version
local_cache_identity
board_profile
stable_asset_id
page_render_target_id
normalized_path_hash
policy_identity
output_geometry
payload_length
payload_crc32
packed_payload
crc32
```

The index carries:

```text
magic
schema_version
local_cache_identity
board_profile
policy_identity
next_insertion_sequence
entry_count
fixed_slot_entries
crc32
```

Each entry carries whether it is occupied, its slot number, the stable asset ID,
the page-render target id, the complete normalized path or a bounded side-table
reference to it, the payload length and CRC, and the insertion sequence. The
complete path is retained or compared so a stable-ID collision cannot alias two
assets.

### Insertion

1. select a free slot, or the occupied entry with the lowest insertion sequence;
2. write the slot payload under a staging name and validate it;
3. rename it into place, over the evicted payload;
4. build the new index, adding or replacing the selected entry and retaining the
   other entries without copying their payloads;
5. write the index under a staging name, validate it, and rename it into place.

Because both are rebuildable, the failure modes read as absence rather than being
recovered:

- an interruption before the slot rename leaves the previous cache exactly as it
  was;
- an interruption between the slot rename and the index rename leaves an index
  whose entry for that slot names a payload CRC the slot no longer has. That
  entry drops, which loses the evicted page a moment earlier than planned and
  exposes nothing wrong. Every other entry is untouched;
- an interruption after the index rename exposes the new page.

An index entry that fails to validate drops that entry. An index that fails to
validate drops the whole page cache for that copy, which costs re-decoding. The
per-entry payload CRC is what makes the middle case safe, so it is not optional.

### Lookup

Validate the index structure and its identity binding, match the complete path,
the stable ID, and the page-render target, then validate the slot payload's CRC
and format. A corrupt slot invalidates only that page for the mount.

### Eviction and admission

Eviction is FIFO by insertion sequence. Reads do not update persistent recency,
so reading creates no write traffic. Sequence overflow disables further insertion
with a stable error until a future schema migration.

Before inserting, firmware requires free space for the maximum candidate slot,
one complete index, filesystem metadata overhead, and a configured safety margin.
Failure is nonfatal and leaves the prior cache intact.

### Write-amplification gate

Per inserted page, logical bytes written must be:

```text
O(new page payload + fixed bounded index)
```

and must not depend on total retained payload bytes.

M3 reports payload bytes, index bytes, filesystem overhead, total physical writes
where measurable, write amplification, insertion latency, and eviction latency.
M3 is blocked if an implementation rewrites all retained payloads per insertion.

---

# Part 6 of 6: Milestones, verification, performance, and closed decisions

## Dependencies on the library PRDs

| This milestone | needs | because |
|---|---|---|
| M0R | Source Identity M1 and M2 | the bundle names the digest the upload hashed while streaming |
| M0R | Physical Folder Library M1 | a locator is what an artifact header binds to |
| M0R | Library Identity M1 | the bundle names a `BookId`, and the upload response has to be able to report one |
| M1A | the above, plus Library Identity M1b | a replacement must not leave the previous edition's cover in place |
| M1B | M0D | there is no device cover without a device decoder |
| M2A | M0R, plus Library Identity M3 | image assets are the first artifacts worth carrying across a move |
| M2B | M0D | same decoder |
| M3 | M2B | there is nothing to persist until the device can decode a page |

Source Identity M4 names image rendering as its first consumer, and this PRD is
that consumer. Two of its requirements land here rather than there: the promotion
rule in Part 2, and the requirement that rendering does not wait on identity.

The library milestones this depends on are done or in review as of 2026-09-03.
Source Identity M1 to M3 are on `main`. Library Identity M1, M1b, M2, and M3 are
on `feature/library-identity-m1`. Physical Folder Library M1 to M3 are on `main`.
Nothing here should begin before that branch merges, because the ledger and the
replace intent are what M1A binds to.

## Milestones

### M0R, render artifact foundation

Deliverables:

1. shared EPUB image semantics, with the semantic bounds inside the policy
   identity;
2. host and device decoder policy separation;
3. the transparency-safe host output contract;
4. canonical luma, geometry, resampler, and dither policies and versions;
5. separated host input, device decode, and output limits;
6. the capabilities response, including the page-render target list;
7. framed bundle upload and its validation;
8. artifact publication by stage and rename, and the manifest;
9. cardinality and role invariants, and bounded validation;
10. `XTCV` and the resident cover;
11. the pinned host decoder and the browser preprocessing path;
12. the upload response fields;
13. session-heap and throughput measurements.

Acceptance:

- a large host-decodable source is not rejected solely by a device workspace
  limit;
- every transparency mechanism produces RGBA output through canonical
  compositing;
- a bundle rejected for framing, limits, identity, or content leaves the previous
  set intact and readable, because every such rejection happens before the
  predecessor is retired;
- an interruption inside the publish window leaves no set rather than a mixed one;
- host and device policy invalidation are independent;
- loading a cover reads no image payload bytes;
- corruption inside one asset stays local to that asset;
- runtime geometry and stride reach the renderer;
- goldens pass in both device configurations.

### M0D, device decoder selection and verification

Deliverables: decoder evaluation, forward-only resumable input, bounded tile or
strip delivery, the streaming raster pipeline, the device decoder policy, the
differential and fuzz suites, and responsiveness, memory, stack, speed, and
quality measurements.

Acceptance: no complete source image buffer; bounded output tiles or strips;
arbitrary chunking gives identical output; callback and I/O failures expose no
partial result; inflate scratch stays resident; the workspace fits both device
configurations; the baseline contract passes; fuzzing reports no memory-safety
defect; cancellation prevents any panel update or publication; zero watchdog
misses.

### M1A, hotspot-preprocessed covers

Acceptance: a visible host cover with no device decoder present; baseline JPEG,
progressive JPEG, and PNG sources; transparency fixtures correct; a bundle refused
when the file no longer holds the named digest; profile and policy enforcement;
runtime stride and geometry; a managed replacement leaving no previous cover
behind; Home goldens re-taken for both configurations.

### M1B, on-device cover fallback

Acceptance: a sideloaded baseline-JPEG cover generated on the device; host
precedence preserved; a bounded tile-to-cover pipeline; recorded deterministic
negative results honored and invalidated by policy changes; the book text opening
whatever the cover does.

### M2A, hotspot-preprocessed image pages

Acceptance: an eligible JPEG or PNG spine item becomes one page; progressive JPEG
and PNG render from host assets; host enumeration matches firmware under every
semantic bound; a cache-capacity overflow does not reclassify a page; fallback
order and text preserved; typed replay byte-identical when publication succeeds;
image refresh transitions are Full; an orientation whose target has no asset falls
back rather than rendering the wrong geometry.

### M2B, on-device image-page fallback

Acceptance: a supported baseline JPEG renders with no artifact present; an
unsupported source renders the retained fallback; the deterministic failure memo
works; no partial image reaches the panel; input and power responsiveness stay
inside their limits.

### M3, bounded persistent device image pages

Acceptance: a fixed slot count and retained byte limit; deterministic FIFO
eviction; insertion that does not copy retained payloads; a failed insertion
leaving the previous index usable; an unreferenced slot ignored and reclaimed; an
identity or policy mismatch dropping the cache rather than serving it; free-space
rejection preserving the prior state; measured write amplification independent of
retained payload volume.

## Verification

### Hardware scope

The owner has an X3 and no X4, so on-device acceptance means X3. The X4
configuration is verified by the repository matrix and the emulator goldens, which
`tools/check.sh` already covers for both. A claim that something works on X4
hardware cannot be made and should not appear in a milestone report.

### Artifact publication

Test an interruption at each step of the sequence: after staging and before the
manifest is retired, after the manifest is retired and before any payload lands,
between two payload renames, and after the payloads land but before the manifest
does. Also test a truncated payload; a manifest naming a length the payload does
not have; a corrupt CRC in one asset among several; a staged set left behind by an
earlier interruption; and a set whose stored locator is not the claim's.

Each case resolves to the previous set or to no set, and a set that fails
validation is reclaimed rather than repaired. No case produces a manifest paired
with payloads it did not describe.

### Identity and scope

Test: an artifact set is not served to a copy whose locator differs; two identical
copies each keep their own set; deleting one copy leaves the other's set intact; a
managed replacement invalidates the replaced copy's set, including a same-size
replacement at the same locator; a move followed by reconciliation carries the
cover and, at idle, the image set; an interrupted relocation leaves the donor
intact; adoption between identical copies happens only after both files are read
in the current epoch.

### Bundle transport

Test: malformed framing; a length that disagrees with the body; an unknown
`BookId`; a digest that no longer describes the file; a bundle arriving while an
install journal stands; an interrupted upload; limits exceeded in each dimension;
path normalization; stable-ID collisions; overlapping ranges; cardinality
violations; a page-render target id the header does not list; host source
dimensions beyond the device limits with valid host output.

### Typed cache

Test: source-order preservation; a sole eligible image; disqualified candidates; a
second image; caption and mixed-text cases; exact fallback reproduction; path
collisions; cache replay; fallback-table, path-table, and event-count overflow;
host versus firmware enumeration under every semantic bound.

### Decoder and raster pipeline

Test: supported sampling; grayscale; restart intervals; unsupported markers;
malformed and truncated streams; arbitrary chunking; tile ordering; callback
failure; cancellation; maximum dimensions; crop and contain geometry; resampling;
dither guards; proof of no full-image allocation; X3 hardware measurements.

### M3

Test: free-slot insertion; FIFO eviction; duplicate insertion; interruption at
each rename; an unreferenced slot; a replacement or deletion during insertion; a
policy upgrade; a full cache; low free space; sequence overflow.

## Performance and resource targets

### Opening a book

The reading path does not regress. Opening a book waits on no digest, no artifact
manifest, and no image decode. A cover load happens after the text open has been
requested, and cannot delay the first text frame.

Cover budget, measured on X3 with a warm cache:

```text
manifest validation + cover read + XTCV validation + resident copy
    p95 <= 25 ms
    reads: one manifest, one cover range, zero IMAGES.BIN bytes
```

Report bytes read, latency, and executor stalls for each stage.

### Bundle upload

Report browser hashing, upload throughput, staged validation reads, publication
cost, host preprocessing time, and the Wi-Fi heap floor. Visible progress or a
nonblocking busy state begins within 250 ms, input and power stay responsive, and
the request is cancellable.

### Runtime artifact lookup

After a set is selected, report structure validation, memoized selection, cover
CRC and `XTCV` validation, the resident copy, and image-page range validation,
with the bytes read from each file.

Targets: a valid cover meets the warm budget above, and completes within 500 ms
in the worst case, excluding panel refresh; cover loading reads zero image payload
bytes; increasing the image payload to its maximum does not change the cover
result or its latency; one image-page load reads only its declared range plus
bounded metadata.

### Cooperative responsiveness

```text
software-controlled work per executor turn <= 10 ms
input and power event latency <= 100 ms
cancellation <= one work slice + one bounded SD operation
watchdog misses = 0
```

Report p50, p95, p99, and maximum. SD latency is reported separately. A card that
cannot meet this is excluded or forces a storage-strategy change.

### On-device decode

Initial X3 target: a typical 1200x1800 baseline JPEG rasterizes within five
seconds, excluding panel refresh.

Report compressed bytes, reduction, decoder workspace, tile or strip bytes,
scaler and dither workspace, destination bytes, inflate scratch, total concurrent
workspace, stack, code size, raster time, an image comparison, responsiveness,
cancellation behavior, and watchdog misses.

### M3 storage

Report payload and index bytes written, filesystem metadata writes, physical
writes where measurable, insertion p50 through maximum, eviction latency,
free-space check cost, and write amplification across an empty, a half-full, and a
full cache. Shipping requires insertion cost to stay independent of retained
payload volume.

## Decisions closed by this PRD

- The source transaction foundation belongs to the library PRDs. This document
  specifies no logical books, tokens, source generations, tombstones, idempotency
  epochs, staging markers, or commit sectors, and the `source-store` crate that
  implemented them is retired.
- Render artifacts are rebuildable derived state, not durable authority.
  Publication is stage, validate, retire, rename, and a failure reads as absence.
- There is no hardware power-cut gate for artifacts. That gate belongs to the
  ledger and the journals.
- Artifacts live in the copy's existing cache directory, with no content-addressed
  store and no reference counts.
- An artifact binds to the local cache identity of the file that produced it: the
  root, the full locator, the byte size, and the hash over them.
- A `SourceDigest` authorizes adoption by an identical copy and recovery after a
  move, and nothing else. Both require a read in the current trust epoch.
- Opening a book waits on no digest and no artifact.
- A cover follows a confirmed move with the reading position; the image-page set
  relocates at idle.
- A managed replacement invalidates the replaced copy's derived state, including a
  same-size replacement at the same locator.
- Capabilities advertise every page-render target, and an image asset records
  which one it was fit to. An orientation with no asset falls back.
- The `LegacyRasterImport` production method is retired, and `X4CV` reads as
  absent.
- Semantic capacity and cache-encoding capacity are separate concerns, and a cache
  encoding failure cannot change a finalized classification.
- Host preprocessing uses host limits; device workspace limits apply only to
  device decode.
- `Gray8` is permitted only for known-opaque decoded output, and effective
  transparency requires non-premultiplied `RGBA8` through canonical compositing.
- Host and device decoder policies stay independently versioned.
- Device image pages use fixed slots and a bounded index in M3, insertion copies no
  retained payloads, eviction is FIFO, and reads write nothing.
- ZIP64 stays unsupported, and container bounds gate image work rather than
  reading.
- Refresh decisions use the renderer outcome and the successful panel history.
- On-device acceptance means X3. X4 is verified by the build matrix and the
  emulator goldens.

## Remaining measurement and implementation gates

- the pinned host and Wasm decoder;
- device decoder selection, and the exact device policy and sampling identifiers;
- optional 16-bit quantization;
- any retained `unsafe`;
- tile or strip dimensions, and the decoder and raster workspace;
- the final resampler and dither choice;
- the semantic and cache-capacity constants;
- the maximum asset count and identity-index bytes;
- staged-set validation I/O;
- the cover geometry and its placement in the Home composition;
- whether the second page-render target is worth its payload, measured against a
  real bundle upload over the hotspot;
- the relocation slice size, measured against the idle branch's existing work;
- M3 slot count, page-size limit, index size, filesystem overhead, and write
  amplification;
- whether M2B needs a rendering plate;
- progressive device JPEG, SVG wrappers, and inline image layout.
