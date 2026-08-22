# Library Identity and Reconciliation

Supersedes the identity and reorganization half of the User-Managed Library
PRD (its §5.2 and §6.6), deleted in this commit and recoverable at
`git show 26f7238:.scratch/user-managed-library/PRD.md`. That PRD keyed both
cache and reading position on one content identity, which this model
deliberately splits.

## Summary

Introduce a persistent identity for each **physical library copy** of a book, separate from both its current filesystem path and the identity of its EPUB bytes.

A library entry receives a stable opaque `BookId`.

The resulting model is:

```text
BookId -> Locator -> physical EPUB
   │
   ├── SourceDigest
   └── user state
       ├── reading position
       ├── bookmarks
       └── future per-copy state
```

Moving or renaming a file updates its `Locator` without changing its `BookId`.

Two byte-identical EPUB files may share a `SourceDigest` while retaining different `BookId`s and independent user state.

## Motivation

Filesystem paths are locations, not durable book identities.

Content identity is also insufficient as a library-entry identity because users may intentionally keep multiple identical copies and expect them to behave independently.

The architecture therefore needs three distinct concepts:

- `Locator`: where a file is;
- `SourceDigest`: which bytes it contains;
- `BookId`: which user-visible library instance it is.

This separation also prevents image/render cache requirements from dictating reading-position semantics.

## Goals

1. Give every adopted physical EPUB copy a persistent opaque `BookId`.
2. Move reading position and future per-copy state onto `BookId`.
3. Preserve `BookId` across ordinary rename/move operations.
4. Support multiple byte-identical copies as independent books.
5. Reconcile filesystem changes performed on another computer between Calendula transactions.
6. Reuse `SourceDigest` as authoritative content evidence where needed.
7. Degrade conservatively when a move cannot be determined uniquely.

## Non-goals

This PRD does not:

- provide arbitrary upload destination selection;
- add mkdir/write support for a user-managed hierarchy;
- permit concurrent external filesystem edits while Calendula has a live storage transaction;
- make Calendula a general FAT filesystem repair tool;
- guarantee automatic reconciliation in every ambiguous filesystem-edit scenario.

## Terminology

### BookId

Opaque persistent identity for one library instance.

Recommended form:

```rust
struct BookId([u8; 16]);
```

Generated randomly when a new physical copy is adopted.

A `BookId` must never be derived solely from:

- path;
- filename;
- FAT cluster;
- `SourceDigest`.

### Locator

Canonical path to the current physical EPUB.

Example:

```text
/Books/Fiction/Dune.epub
```

A locator is mutable.

### SourceDigest

Full-content identity from the Source Identity PRD.

Two books may have:

```text
Book A: BookId=A, SourceDigest=X
Book B: BookId=B, SourceDigest=X
```

and must retain independent reading state.

### BookRecord

Conceptually:

```rust
struct BookRecord {
    id: BookId,
    locator: Locator,
    source: Option<SourceDigest>,
}
```

Source identity may initially be lazy.

## Requirements

### R1. BookId owns user state

Reading position must eventually be addressed by `BookId`.

Future per-copy state should follow the same rule unless there is a strong reason otherwise.

### R2. Identical copies remain independent

If two physical EPUB files contain identical bytes:

- they have the same `SourceDigest`;
- they have different `BookId`s;
- reading one does not advance the other.

### R3. Rename preserves BookId

If a known EPUB moves from:

```text
/Books/Dune.epub
```

to:

```text
/Books/Fiction/Dune.epub
```

and reconciliation determines it is the same physical library instance, update the record's locator while preserving its `BookId`.

### R4. Replacement semantics distinguish known intent from unexplained path reuse

Changing the bytes at a locator does not inherently create a new library entry.

For a **Calendula-managed replacement** of an existing `BookId`:

- preserve the existing `BookId`;
- update its `SourceDigest` after the replacement transaction commits;
- preserve per-book user state, including reading position;
- allow the position layer to migrate its content anchor onto the new source;
- retain the old `SourceDigest` only as long as needed for rollback or derived-cache garbage collection.

This covers ordinary workflows such as uploading a corrected or updated copy of a book without treating it as an unrelated library entry.

For a filesystem change performed outside Calendula where the same locator now contains different bytes and no durable operation establishes replacement intent:

- do not assume pathname reuse means the same book;
- reconcile conservatively using available library metadata and source evidence;
- assign a new `BookId` if continuity cannot be established safely.

A path is therefore neither permanent identity nor sufficient evidence of replacement. Calendula-managed transaction intent may establish continuity that an unexplained external filesystem change cannot.

### R5. Reconciliation happens between transactions

Computer-side filesystem edits are supported when no Calendula storage transaction is live.

Concurrent outside edits while `INSTALL.JNL` or `RECLAIM.JNL` is live remain unsupported.

Where interference is recognizable, Calendula should refuse or require reconciliation rather than guessing.

### R6. Reconciliation starts from stable locators

On scan:

1. known locator still exists and appears unchanged → preserve record;
2. known locator is missing → candidate for move/reconciliation;
3. unknown physical EPUB appears → candidate for new entry or move target.

Do not re-identify every stable file on every boot.

### R7. SourceDigest is authoritative confirmation

When a missing known entry and a new unknown file need to be matched, full `SourceDigest` is authoritative evidence that their contents match.

A cheap move fingerprint may narrow candidates but must not be the final proof.

### R8. Optional MoveFingerprint

If measurements show full hashing of candidate sets is too expensive, introduce:

```text
MoveFingerprint = length + sampled digest
```

Its only role is candidate filtering.

Reconciliation must still confirm a chosen match with `SourceDigest`.

### R9. Ambiguity must not merge state

If one old record could plausibly match multiple new files, or multiple old records share the same source:

- do not arbitrarily choose one;
- preserve independent `BookId`s;
- adopt unmatched copies as new entries if necessary;
- surface ambiguity only if user intervention becomes valuable.

A wrong merge is worse than losing automatic move continuity.

### R10. Missing books may retain state

Removing the SD card entry does not have to immediately delete its `BookId` metadata.

A bounded stale-record retention policy allows:

- temporary card edits;
- moves detected on a subsequent scan;
- future UI for missing books.

Garbage collection policy is implementation-dependent.

### R11. User state must survive path changes

At minimum:

- reading position;
- book-open state persisted across restart;

must follow `BookId`, not locator.

### R12. Content-derived state may be shared

Artifacts addressed by `SourceDigest` may be reused by multiple `BookId`s.

Deleting one library copy must not invalidate shared derived state still referenced by another copy.

### R13. Storage journals remain separate

`BookId` is not an authorization token for FAT mutation.

`INSTALL.JNL` and `RECLAIM.JNL` continue to use their own filesystem/FAT transaction identities.

Library reconciliation happens above that layer.

## Persistent model

Conceptually:

```text
Library Index

BookId A
  locator = /Books/Fiction/Dune.epub
  source  = SHA256(...)
  position = ...

BookId B
  locator = /Books/Backup/Dune.epub
  source  = SHA256(...)   # same source as A
  position = ...          # independent
```

Derived cache:

```text
SourceDigest X
  parsed EPUB
  images
  cover
  ...

(SourceDigest X, LayoutId Y)
  pagination
```

## Migration

Existing reading-position data is currently tied to the previous identity scheme.

Migration should prioritize preserving real user state.

A practical migration may:

1. scan existing known books;
2. create one `BookId` per current physical copy;
3. associate existing position state with the best matching current entry;
4. retain source-derived caches independently where possible.

If existing data cannot distinguish duplicate copies, document that migration ambiguity rather than inventing precision that did not previously exist.

## Reconciliation algorithm

Initial version:

### Phase 1: Stable entries

For every known `BookRecord` whose locator still exists:

- preserve `BookId`;
- verify cheap metadata sufficient to know no reconciliation is required;
- defer expensive hashing unless another feature needs it.

### Phase 2: Missing records and new files

Build:

```text
missing known records
new/unclaimed physical EPUBs
```

Narrow by cheap metadata such as size.

If needed, compute `SourceDigest` lazily.

### Phase 3: Unique matches

If exactly one missing record and one new file share the authoritative source identity:

- update the record locator;
- preserve `BookId`.

### Phase 4: Ambiguous matches

If multiple old/new copies share the same source identity:

- do not infer which physical copy is which unless additional durable evidence exists;
- preserve existing IDs as missing if necessary;
- assign new `BookId`s to unmatched current copies.

This may lose automatic position continuity for ambiguous duplicate moves, but it never merges independent user state incorrectly.

## Failure handling

Library metadata is secondary to the physical EPUB.

Corrupt library-index metadata must not prevent a user from accessing valid EPUB files where feasible.

Recovery strategy should prefer:

- rebuilding locator/source relationships;
- preserving opaque user state when identifiable;
- assigning new `BookId`s rather than mutating/deleting source files.

## Testing

### Identity tests

- new physical copies receive distinct `BookId`s;
- identical bytes receive the same `SourceDigest`;
- identical copies have independent positions.

### Move tests

- rename within a directory preserves `BookId`;
- move across directories preserves `BookId`;
- move plus reboot preserves position.

### Replacement tests

- same path + different source does not silently inherit the old `BookId`;
- failed replacement preserves the original mapping.

### Duplicate tests

- two identical copies remain separate;
- deleting one does not delete the other's position;
- ambiguous duplicate reorganization does not merge their state.

### Reconciliation tests

- stable paths require no full-card rehash;
- a uniquely moved sideloaded book is repaired;
- an ambiguous move fails conservatively.

## Milestones

### Milestone 1: BookId and metadata model

- Introduce `BookId`.
- Persist `BookRecord`.
- Keep current locator behavior.

### Milestone 2: Position migration

- Address reading positions by `BookId`.
- Migrate existing data.
- Pin duplicate-copy independence.

### Milestone 3: Basic reconciliation

- Detect missing/new locators.
- Confirm unique moves with `SourceDigest`.
- Repair locators.

### Milestone 4: Performance optimization

Only if needed:

- add `MoveFingerprint`;
- measure large-card reconciliation;
- preserve full digest as final confirmation.

## Done when

- Paths can change without inherently changing book identity.
- Two identical physical copies have different `BookId`s and independent positions.
- Source-derived artifacts can still be shared.
- Unique filesystem moves can be repaired automatically.
- Ambiguous filesystem changes fail conservatively.
- No new coupling is introduced between library identity and FAT transaction recovery.
