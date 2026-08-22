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
/BOOKS/Fiction/Dune.epub
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
/BOOKS/Dune.epub
```

to:

```text
/BOOKS/Fiction/Dune.epub
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

Concurrent outside edits while any Calendula storage transaction is live remain
unsupported. That includes `INSTALL.JNL`, `RECLAIM.JNL`, and the
library-metadata intent of R17, which can outlive both journals.

Where interference is recognizable, Calendula should refuse or require reconciliation rather than guessing.

### R6. Reconciliation starts from stable locators

On scan:

1. known locator still exists and appears unchanged → preserve record;
2. known locator is missing → candidate for move/reconciliation;
3. unknown physical EPUB appears → candidate for new entry or move target.

Do not re-identify every stable file on every boot.

"Appears unchanged" is a cheap filter for reconciliation, not evidence about
content. A locator that looks stable may hold different bytes after an outside
edit, so any operation needing authoritative content identity revalidates
under Source Identity R11 rather than trusting the stored digest.

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
  locator = /BOOKS/Fiction/Dune.epub
  source  = SHA256(...)
  position = ...

BookId B
  locator = /BOOKS/Backup/Dune.epub
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

## Library metadata durability

`SourceDigest` is rebuildable derived identity. `BookId` is not. It is random
by design and cannot be reconstructed from the EPUB, the path, the cluster, or
the digest. Once a `BookId` owns reading position, the mapping from that id to
a physical copy becomes durable user state rather than a cache, and it needs a
crash-durability contract of its own.

Treating the EPUB as authoritative recovers the book. It does not recover the
identity and position associated with that copy:

```text
position[7db3...] = chapter 12
BookRecord for 7db3... is torn
EPUB is intact

scan sees an unclaimed EPUB, mints BookId 91af...
position[7db3...] is now unreachable
```

The position file survived perfectly and the user still lost their place.

Split the persistent library data accordingly:

```text
Durable identity state
    BookId <-> adopted physical copy
    replacement and move handoff state
    per-BookId user state

Rebuildable index
    sorted catalog
    directory acceleration
    cached metadata
    SourceDigest lookup indices
```

The first category needs an explicitly recoverable scheme of the kind reading
position already uses, such as two generations with a commit record. The
second may be deleted and rebuilt freely.

### R14. A BookId is durable before user state depends on it

Adopting a copy establishes its `BookId` durably before any per-book state may
reference it.

### R15. Locator updates are crash recoverable

A move repair that is interrupted resumes or rolls back. It does not leave a
record pointing at a path that holds a different book.

### R16. An interrupted metadata write does not mint a new id

Losing a single interrupted write must not cause a physical copy to acquire a
fresh `BookId`. Corruption may leave a book temporarily unassociated, and
recovery reconnects durable ids where it can. Minting a replacement id is the
last resort, not the default response to a torn record.

### R17. Managed replacement carries durable intent that outlives INSTALL.JNL

The filesystem transaction and the library metadata update are two
transactions, and R4's managed-replacement guarantee spans both. Nothing
bridges them today:

```text
1. upload replacement Y, digest computed while streaming
2. INSTALL.JNL written
3. filesystem commits Y at /BOOKS/Dune.epub
4. recovery observes Done and clears INSTALL.JNL
5. power loss
6. the library update A: X -> Y never happened
```

On reboot the record says `A / X`, the file holds `Y`, and no journal remains.
That is observationally identical to the unexplained external replacement of
R4, which is allowed to mint a new `BookId` and orphan the position. An
ordinary power cut would therefore break the managed-replacement guarantee in
this PRD and in Reading Position R12.

**Decided:** a small library-metadata transaction, separate from
`INSTALL.JNL`. It records at least:

```text
(BookId, locator, old: Option<SourceDigest>, new: SourceDigest)
```

The old digest is optional because `BookRecord.source` is optional: source
identity is lazy, and replacing a book should not force a full read of the
predecessor purely to populate transaction metadata.

The record is written before the install begins, stands after `INSTALL.JNL`
clears, and is cleared once the `BookRecord` is updated.

**A live intent does not say which side won.** It stands both when the install
never became durable and when the install completed and cleared, so recovery
has to ask the card rather than the record:

```text
recovery order, on mount or at session start:

1. settle RECLAIM.JNL
2. settle INSTALL.JNL
3. resolve the library intent

then, against what the destination actually holds:

  matches the new SourceDigest
      -> publish BookRecord as (BookId, locator, new)
      -> clear the intent

  still the old landing
      -> leave the BookRecord unchanged
      -> clear the intent

  neither legal landing can be established
      -> keep the intent and fail conservatively
```

Library metadata recovery does not run ahead of filesystem recovery. The
filesystem transaction decides what the card holds, and the library
transaction only records what that means for identity.

**A live library intent is a storage transaction for the sole-writer
contract.** External modification while it stands is unsupported exactly as it
is for `INSTALL.JNL` and `RECLAIM.JNL`, and the intent can outlive both of
them. Without that, `old: None` has no sound resolution: an unrelated file
placed at the locator by a computer is indistinguishable from the predecessor
this transaction was replacing.

Resolution, after filesystem recovery and under that contract:

```text
destination == new
    -> new landing

old is Some(X) and destination == X
    -> old landing

old is None and destination exists and destination != new
    -> old landing

anything else
    -> refuse, keep the intent
```

The third line is the one the contract pays for. It reads a difference as the
predecessor rather than as a stranger, which holds because nothing else was
permitted to write while the intent stood.

Establishing which landing occurred requires the destination's content
identity, so this is one of the operations that revalidates a persisted digest
under Source Identity R11.

Keeping it separate preserves Source Identity R10: FAT recovery does not need
to understand semantic book identity, and no SHA-256 or `BookId` enters the
storage journals.

*Alternatives recorded so they are not relitigated.* A simpler product rule,
that an existing locator always retains its `BookId` when its bytes change,
needs no durable intent at all, but it drops the managed and unexplained
distinction R4 was amended to make. Extending `INSTALL.JNL` with opaque
library identity also works and costs the separation above.

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

Rebuildable library index metadata is secondary to the EPUB. Durable `BookId`
identity state and per-book user state are not.

Corrupt library-index metadata must not prevent a user from accessing valid EPUB files where feasible.

Recovery strategy should prefer:

- rebuilding locator/source relationships;
- preserving opaque user state when identifiable;
- reconnecting a durable `BookId` to its physical copy where the evidence
  allows;
- assigning new `BookId`s rather than mutating or deleting source files, as a
  last resort once reconnection has failed, and subject to R16.

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

- a Calendula-managed replacement preserves the existing `BookId` and updates
  its `SourceDigest`, keeping reading position and other per-book state;
- an unexplained external replacement at the same locator does not inherit the
  old `BookId` unless reconciliation establishes continuity;
- failed replacement preserves the original mapping.

### Duplicate tests

- two identical copies remain separate;
- deleting one does not delete the other's position;
- ambiguous duplicate reorganization does not merge their state.

### Durability tests

- a power cut swept across the whole library-intent protocol, from before the
  intent is published through to after it clears, leaves the managed
  replacement recoverable at every point, with `BookId` preserved and the
  position intact. The install may not begin until the intent is durably
  recoverable, so the earliest part of that sweep matters as much as the
  commit boundary;
- a torn library-metadata write does not cause the copy to be adopted under a
  fresh `BookId`;
- an interrupted locator repair resumes or rolls back.

### Reconciliation tests

- stable paths require no full-card rehash;
- a uniquely moved sideloaded book is repaired;
- an ambiguous move fails conservatively.

## Milestones

### Milestone 1: BookId and a recoverable metadata model

- Introduce `BookId`.
- Persist `BookRecord` in a representation that survives an interrupted write,
  such as two generations with a commit record, per R14 to R16.
- Keep current locator behavior.

### Milestone 1b: Managed-replacement transaction

- Add the library-metadata intent and its recovery resolution, per R17.
- Sweep a power cut across every durable write in the library-intent protocol:
  intent publication, install and reclaim recovery, `BookRecord` publication,
  and the intent clear. A sweep that starts at the filesystem commit would
  pass an implementation that begins installing before its intent is durable,
  and that ordering is the point of the protocol.

Both land before any user state depends on `BookId`, since a `BookId` that can
be lost or reminted is worse than no `BookId` once a position hangs from it.

### Milestone 2: Position ownership

- Expose the `BookId` mapping that positions will be addressed by.
- Pin duplicate-copy independence.

Migration of persisted reading positions onto `BookId` belongs to the Reading
Position and Layout Durability PRD, so the ownership-key change and the page
index to anchor change happen in one position-format migration rather than
two. This PRD keeps the done-when requirement that positions follow `BookId`,
and does not schedule the format change itself.

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
- Library identity stays out of the FAT journal formats: no `BookId` or
  `SourceDigest` is encoded in `INSTALL.JNL` or `RECLAIM.JNL`, and library
  metadata recovery runs after filesystem recovery rather than beside it. The
  protocol ordering in R17 is deliberate coupling; the format separation is
  what this PRD preserves.
