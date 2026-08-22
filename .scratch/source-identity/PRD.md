# Source Identity

Supersedes the content-fingerprint half of the User-Managed Library PRD
(its §6.3), deleted in this commit and recoverable at
`git show 26f7238:.scratch/user-managed-library/PRD.md`.

## Summary

Introduce a stable identity for the bytes of an EPUB, independent of its filesystem path or any particular user-visible copy.

This identity exists primarily to support content-derived and rebuildable artifacts such as parsed EPUB data, covers, decoded images, and future render caches. It is deliberately **not** the identity of a library entry and must not own user state such as reading position.

The source identity is authoritative: two byte-identical EPUB files have the same source identity, while different bytes have different identities.

## Motivation

Calendula currently derives cache identity partly from filesystem location. That works while books live at stable names under `/BOOKS`, but it creates unnecessary coupling between:

- where an EPUB happens to live;
- whether two EPUBs contain the same bytes;
- derived rendering/cache artifacts;
- future filesystem reorganization.

Image rendering particularly needs an identity for the EPUB source bytes so extracted or decoded image artifacts can survive renames and can be shared by identical copies.

The broader user-managed-library design should not be required merely to supply that primitive.

## Goals

1. Give each distinct EPUB byte stream an authoritative `SourceDigest`.
2. Make the digest independent of path, filename, directory, FAT cluster, and library-entry identity.
3. Compute the digest without an additional full-card scan in normal upload workflows.
4. Allow byte-identical EPUB copies to share rebuildable derived state.
5. Provide a foundation for image rendering and future content-addressed caches.
6. Preserve the existing storage transaction and power-cut model.

## Non-goals

This PRD does not:

- introduce physical-folder browsing;
- allow uploads to arbitrary directories;
- identify individual library copies;
- move reading positions to a new identity;
- reconcile filesystem moves or renames;
- change `INSTALL.JNL` or `RECLAIM.JNL`;
- make cryptographic authenticity or security claims about EPUB contents.

## Terminology

### Locator

The current filesystem location of a physical EPUB.

Example:

```text
/BOOKS/Dune.epub
```

A locator may change without changing the EPUB contents.

### SourceDigest

The authoritative identity of an EPUB's bytes.

Conceptually:

```rust
struct SourceDigest {
    byte_len: u64,
    sha256: [u8; 32],
}
```

The length is retained alongside SHA-256 because it is useful metadata and makes accidental misuse easier to diagnose.

Two files with identical bytes have the same `SourceDigest`.

A `SourceDigest` is not a library-entry identifier.

## Requirements

### R1. Authoritative full-content digest

`SourceDigest` must be derived from the complete EPUB byte stream.

Do not use a sampled fingerprint as the canonical source identity.

A sampled fingerprint may be introduced later as an optimization for finding move candidates, but it must not substitute for `SourceDigest`.

### R2. SHA-256

Use SHA-256 for the canonical digest.

The goal is not adversarial authentication; the stronger digest is chosen because this identifier may persist for a long time and become the root of several derived caches. There is little value in permanently designing collision ambiguity into that layer.

### R3. Compute during hotspot upload

When Calendula already receives every byte of an EPUB through the hotspot upload path, compute SHA-256 incrementally during the same stream.

A successful staged upload should therefore know its source digest without rereading the newly written EPUB.

The digest does not participate in FAT transaction recovery. It becomes library metadata only after the storage transaction reaches a legal landing.

### R4. Lazy adoption for sideloaded files

For EPUBs placed on the card by a computer, compute `SourceDigest` lazily.

Suitable triggers include:

- first open;
- first operation that needs content-derived caching;
- an explicit background/indexing operation if one is introduced later.

Do not require hashing every EPUB on every boot.

### R5. Derived caches may key by source identity

Artifacts that are purely functions of EPUB contents may use `SourceDigest` as their primary source key.

Examples:

- EPUB structural parse results;
- table of contents;
- cover extraction;
- embedded-image extraction;
- decoded image data;
- source-level render assets.

### R6. Layout-dependent artifacts include layout identity

Anything whose result also depends on reader/display configuration must not key only by `SourceDigest`.

Conceptually:

```text
(SourceDigest, LayoutId) -> pagination/render artifacts
```

`LayoutId` may include relevant inputs such as:

- screen geometry;
- margins;
- font;
- font size;
- line spacing;
- renderer/version information.

The exact `LayoutId` design is outside this PRD.

### R7. User state must not key solely by SourceDigest

Reading position, bookmarks, annotations, and other per-copy state must not be migrated to `SourceDigest`.

Two byte-identical EPUB files may later represent two independent library entries.

### R8. Path remains a locator

Existing code may continue to locate files by path.

Introducing `SourceDigest` does not require filesystem operations to become content-addressed.

### R9. Digest storage must be replaceable/rebuildable

A stored digest is metadata derived from the EPUB and may be regenerated from the source file.

Its persistence format should be versioned or otherwise safe to invalidate when necessary.

When a stored digest may be revalidated rather than trusted is specified by R11.

### R10. No new storage-transaction semantics

`INSTALL.JNL` and `RECLAIM.JNL` remain responsible only for making filesystem mutation recoverable.

Neither journal needs to contain a SHA-256 digest as part of this milestone.

Their temporary FAT/name identity and `SourceDigest` solve different problems.

### R11. A persisted digest is cached evidence until revalidated

A digest is trusted only inside a **trust epoch**: while Calendula
continuously owns the card, or when the current bytes are the known result of
a Calendula-managed operation. Boot, remount, and card reinsertion each begin a
new epoch, because a removable card gives the device no way to prove it was
left alone while absent or powered down.

Outside its epoch a persisted `SourceDigest` is cached evidence rather than
trusted identity, and the first operation that needs authoritative content
identity revalidates the file by hashing it.

**Trust attaches to a file, not to a digest value.** It is a property of the
association between one physical file and its `SourceDigest`, and identical
bytes deliberately produce identical digests, so validating one copy proves
nothing about another. Two files whose persisted digests match must each be
revalidated after an epoch boundary.

Conceptually the trusted fact is:

```text
TrustedSource {
    physical file identity (locator for this milestone),
    SourceDigest,
    epoch,
}
```

and it is not a set of trusted digest values. A set would allow this:

```text
/BOOKS/A.epub and /BOOKS/B.epub both hold X, both persisted as X
a computer replaces only B with same-sized Y
open A  -> hash gives X, X marked trusted
open B  -> X already trusted, B is served from the X-keyed cache
           while B actually holds Y
```

Locator is a sufficient file identity for this milestone, since moves are out
of scope here. Once `BookId` exists the association can hang from the
`BookRecord`, and a Calendula-managed rename or move can transfer or
re-establish trust as part of a known operation.

Per file, the epoch rule reads:

```text
managed upload of Y            -> that file trusted at Y immediately
same uninterrupted session     -> that file stays trusted
reboot, remount, reinsertion   -> its persisted digest becomes cached evidence
first operation needing
  content identity             -> hash that file, trusted for this epoch
```

Stating it as an epoch rather than as evidence of sole control is deliberate.
No durable marker the device can write survives the test: a generation
counter, a clean-unmount flag, cached size and timestamps, or FAT metadata can
all be left untouched by a computer that edits the file, so any such marker
would report trust the device cannot support.

This stays lazy and does not imply hashing at boot:

```text
boot                          -> no hashing
browse folders                -> no hashing
open a book, or need an
  artifact keyed by content   -> validate that one file
```

Without this rule the digest is authoritative as an algorithm and not as the
identity attached to a file. A computer can replace a book with a same-sized
edition, leaving a cheap scan satisfied that the locator is unchanged, and a
cache keyed by the stored digest would then serve artifacts belonging to the
previous bytes.

Size, timestamps, or a sampled fingerprint may narrow which files need
revalidation. They cannot stand in for the check itself, since the same-path
replacement semantics in Library Identity R4 depend on knowing the current
bytes.

## Data model

The minimum new model is:

```text
Locator
   │
   └── EPUB bytes ── SHA-256 ──> SourceDigest
                                  │
                                  ├── parsed EPUB data
                                  ├── cover
                                  ├── extracted images
                                  └── future source-derived caches
```

No `BookId` is required for this milestone.

## Persistence

The first implementation may attach source identity to existing derived metadata rather than introduce a general library database.

Requirements for persistence:

- full 32-byte SHA-256 must be retained;
- byte length must be retained;
- corrupt or missing identity metadata must cause recomputation, not loss of the EPUB;
- metadata may be discarded at any time without affecting the source book.

Do not make the source EPUB dependent on its derived identity record for readability.

## Failure handling

If hashing a sideloaded EPUB fails:

- leave the EPUB untouched;
- do not publish a partial digest;
- report or defer the derived operation that needed identity.

If an upload fails or rolls back:

- do not attach the in-flight digest to the surviving old book;
- discard the digest state associated only with the unsuccessful upload.

## Testing

### Unit tests

Cover:

- identical bytes produce identical identities;
- one-byte changes produce different identities;
- chunk boundaries do not affect the result;
- empty input hashes correctly;
- byte length is included correctly.

### Upload integration tests

Verify:

- digest computed while streaming equals an independent digest of the committed file;
- replacement publishes the new digest only when the new body commits;
- rollback retains the old source identity;
- interrupted upload does not attach the new digest to the old landing.

### Trust boundary tests

Verify the epoch rule directly:

- compute `X` for a book, reboot, replace the file externally with same-sized
  `Y` while preserving cheap metadata such as size and timestamps, then
  require the first content-addressed operation to discover `Y` rather than
  consume an artifact keyed by `X`;
- two files identical at `X`, then a reboot, then an external replacement of
  only the second with same-sized `Y`: validating the first must not authorize
  the second, and a content-addressed operation on the second discovers `Y`;
- within one uninterrupted session, a digest computed during a managed upload
  is reused without rehashing;
- browsing and boot perform no hashing.

### Sideload tests

Verify:

- an existing EPUB can be lazily hashed;
- rename does not change its source digest;
- two identical files at different paths receive the same source digest.

## Milestones

### Milestone 1: Core identity type

- Add `SourceDigest`.
- Add streaming SHA-256 implementation at the appropriate shared layer.
- Add deterministic unit tests.

### Milestone 2: Upload integration

- Compute digest while receiving EPUB bytes.
- Carry it through successful upload completion.
- Verify replacement/rollback behavior.

### Milestone 3: Lazy existing-file identity

- Hash sideloaded EPUBs on demand.
- Persist/reuse the result where appropriate.
- Invalidate safely if the source can no longer be trusted to match the stored identity.

### Milestone 4: First consumer

Use `SourceDigest` for the first content-derived feature, preferably image-rendering artifacts.

This milestone proves that the identity abstraction is useful without introducing library-instance identity.

## Done when

- Every hotspot-uploaded EPUB can obtain its full SHA-256 without a second full-file read.
- Existing EPUBs can obtain the same identity lazily.
- Identical EPUB copies share a `SourceDigest`.
- Image-rendering or another content-derived cache can use the identity without depending on the EPUB path.
- Reading position remains unaffected.
- Existing install/reclaim power-cut guarantees remain unchanged.
