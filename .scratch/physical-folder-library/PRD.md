# Physical Folder Library

Supersedes the placement and browsing half of the User-Managed Library PRD,
deleted in this commit and recoverable at
`git show 26f7238:.scratch/user-managed-library/PRD.md`.

## Summary

Allow Calendula to present the user's actual SD-card folder hierarchy as the library.

The first version is intentionally **read-oriented**: users may organize EPUBs into folders on a computer, and Calendula browses that structure directly.

Writing to arbitrary destination folders from the hotspot is a later milestone, not a prerequisite for folder browsing.

This avoids generalizing the crash-recovery journals before the browsing model has demonstrated value.

## Motivation

A user-managed library should not require Calendula to invent a second organizational database when the SD card already has a portable hierarchy that users can edit with ordinary filesystem tools.

Physical folders provide:

- organization visible both on Calendula and on a computer;
- no vendor-specific metadata required to understand the card;
- natural support for large libraries;
- straightforward interoperability.

The existing fixed `/BOOKS` model can remain the hotspot upload destination while physical-folder browsing is introduced independently.

## Goals

1. Browse EPUBs through their actual physical folder hierarchy.
2. Support long directory and EPUB names.
3. Keep paths as locators rather than durable book identity.
4. Work with the `BookId`/`SourceDigest` model from the identity PRDs.
5. Preserve current storage transaction guarantees.
6. Avoid requiring arbitrary-path writes in the initial milestone.
7. Keep external organization by a computer simple and supported between Calendula transactions.

## Non-goals

Initial scope does not require:

- moving books from the device UI;
- renaming books from the device UI;
- creating directories from the device;
- deleting arbitrary directories;
- hotspot upload to arbitrary nested destinations;
- multiple concurrent independent mutation operations;
- live coexistence with a computer writing to the SD card;
- a virtual tag/collection hierarchy independent of the filesystem.

## Filesystem root

Do not treat every directory on the SD card as library content.

Define a library root.

The library root is:

```text
/BOOKS
```

Users may organize arbitrary subdirectories beneath it:

```text
/BOOKS/
  Fiction/
    Dune.epub
  History/
    Rome/
      SPQR.epub
  Reference/
```

Firmware-private state remains outside the browsable hierarchy under `/READER`.

## Requirements

### R1. Physical directories are the navigation model

The library UI reflects the actual hierarchy below `/BOOKS`.

A directory is not copied into a separate organizational database.

### R2. Only EPUBs and directories participate

At each folder level:

- show child directories;
- show supported EPUB files;
- ignore unrelated filesystem artifacts unless future requirements say otherwise.

### R3. Long filenames and directory names are supported

The implementation must use the fork's LFN-aware directory enumeration.

Do not reduce visible names to 8.3 aliases.

### R4. Paths are locators

A selected book resolves to a canonical locator, stored relative to the
library root:

```text
/BOOKS/History/Rome/SPQR.epub    where the book sits on the card
History/Rome/SPQR.epub           the locator, as stored and compared
```

Root-relative because `/BOOKS` is a product decision that may move, and a
locator naming it would have to be rewritten if it did. That locator is passed
to the library-identity layer.

Moving the file later updates the locator without inherently changing `BookId`.

### R5. Directory traversal needs a real LFN resolver

`embedded-sdmmc` does not provide a generic “open this arbitrary long path” abstraction.

Implement a resolver which, for each path component:

1. enumerates the current directory with LFN-aware iteration;
2. matches the requested long component;
3. obtains the actual FAT directory entry/short alias;
4. descends through that entry.

Keep this resolver in one storage-layer abstraction rather than duplicating LFN scans across firmware callers.

### R6. Canonical path rules are explicit

Define and validate:

- maximum path depth;
- maximum serialized locator length;
- treatment of `.` and `..`;
- path separator;
- root-relative versus absolute representation;
- case comparison rules appropriate to FAT;
- illegal/control characters.

The UI may display the original long filename while internal locator representation remains normalized.

**Settled in milestone 1**, since the rules turned out to have a sharp edge:

- structure is normalized, spelling is preserved, and locator equality is
  exact;
- matching the way the card matches belongs to the resolver, which can see
  whether a component names a long entry or a short one.

Those two namespaces compare differently in the pinned driver. Long names use
a scalar-by-scalar lowercase mapping, and short names are built with
`to_ascii_uppercase` over ISO-8859-1 and then compared as bytes, so `Ü.EPU`
and `ü.EPU` are two files. A single rule in the locator type would merge files
the card keeps apart, which is a book opened in place of another. Exact
equality risks the opposite, one file under two keys, which costs a rebuild.

Locators should therefore be built from the names a scan read off the card,
so a stored locator carries the card's own spelling.

### R7. Reads remain available during safe recovery states

The existing storage owner decides whether the shelf is readable versus mutable.

Browsing must respect the current recovery gates.

A live reclaim may still permit some reads, but must prevent any operation that could allocate/reuse clusters.

### R8. Mutations remain globally serialized

Folder support must **not** introduce per-directory transaction concurrency.

There is one globally serialized mutation operation at a time. One such
operation may span several coordinated recovery records: the library-metadata
intent of Library Identity R17 stands before the install begins, overlaps it,
may overlap the reclaim it drives, and survives after `INSTALL.JNL` clears.
Those records describe one logical mutation rather than competing ones.

No unrelated allocating mutation may run while any part of that protocol is
live.

A path tells an operation where to act; it does not create a separate lock domain.

This is required because a live reclaim record contains raw cluster numbers whose ownership depends on preventing unrelated allocations.

### R9. Hotspot uploads land directly under /BOOKS

*(decided 2026-08-22.)* The first release semantics are:

> `/BOOKS` is the library root and the default hotspot upload destination.
> User-created subdirectories beneath it are browsed normally. Files uploaded
> by Calendula appear at the library root until the user reorganizes them on a
> computer.

There is no firmware-managed or reserved `/BOOKS/Inbox`, and a user-created
directory with that name is an ordinary folder like any other. A reserved
inbox reads as tidier and buys little, while creating
obligations this release does not want: migration behaviour for existing
cards, ensuring the directory exists, deciding what happens when the user
renames or deletes it, and above all putting mkdir on the upload path. This
PRD exists so browsing can ship without generalizing the write and recovery
model, and auto-creating an inbox hands part of that back.

Leaving uploads at the root is also a product test. If users find root-level
uploads annoying once they have folders, that is concrete evidence for
destination selection later, which is better than guessing at it now.

`INSTALL.JNL` and `RECLAIM.JNL` stay unchanged while physical-folder browsing
ships.

### R10. Computer-side reorganization is supported between transactions

Users may:

- create directories;
- move EPUBs;
- rename EPUBs;
- copy/delete EPUBs;

while no Calendula mutation operation is live.

On next scan, the library-identity reconciliation layer repairs `BookId` locators where possible.

### R11. Concurrent outside writes remain unsupported

If another device edits the FAT while any part of a Calendula mutation protocol is live, correctness is not guaranteed.

Recognizable interference should fail conservatively where possible, but this PRD does not promise arbitrary outside-writer recovery.

Recovery may contain specific conservative mitigations where interference is
recognizable. External edits while any part of the protocol is live sit outside the
correctness guarantee, and are not guaranteed to be detectable or reportable.
The mutation protocols guarantee power loss while Calendula is the only
writer. That covers `INSTALL.JNL`, `RECLAIM.JNL`, and the library-metadata
intent of Library Identity R17, which can outlive both. The reclaim journal
keeps freeing the clusters a record names even when the name has been taken by
a stranger, which is correct under that contract and hazardous outside it.

Reserve a hard refuse-and-report requirement for states Calendula can identify
safely, such as a journal record written by an unsupported version.

### R12. Empty directories are allowed

Empty user-created folders are valid library objects.

The UI should not require every directory to contain a book.

### R13. Derived state remains outside the library tree

Do not place Calendula caches alongside user EPUBs.

Continue using `/READER` or equivalent firmware-private storage for:

- catalogs;
- positions;
- image caches;
- render caches;
- journals;
- library metadata.

### R14. Catalog/cache keys must not depend solely on paths

Any whole-library catalog may cache locators for speed, but authoritative per-book identity follows the Library Identity PRD.

A path change should invalidate/update a locator, not inherently create a different source identity.

## UI model

Initial navigation can remain deliberately simple:

```text
Library
  > Fiction/
    History/
    Reference/
    Book at root.epub
```

Selecting a directory pushes a new directory view.

Selecting a book opens it through its `BookId`/locator.

Required first-version operations:

- enter folder;
- go to parent;
- open book.

Sorting/filtering policy may reuse the current library behavior.

## Storage architecture

Introduce a storage-layer abstraction conceptually like:

```text
LibraryPath
resolve_directory(path)
list_directory(path)
open_book(path)
```

This layer owns:

- LFN component resolution;
- canonical path validation;
- translation to actual embedded-sdmmc directory handles.

Higher layers should not manually walk FAT directory aliases.

## Catalog strategy

Do not make a global precomputed catalog mandatory for correctness.

Two acceptable approaches:

### Option A: On-demand directory reads

Enumerate the current folder when entered.

Advantages:

- simple;
- naturally reflects computer-side edits;
- low global indexing cost.

### Option B: Derived tree index

Maintain a rebuildable index under `/READER` for faster navigation.

If used:

- it is derived state;
- physical folders remain authoritative;
- stale/corrupt index data is rebuilt rather than mutating user files.

Start with the simplest version whose on-device latency is acceptable.

## Arbitrary upload placement: deferred milestone

Uploading directly to:

```text
/BOOKS/History/Rome/
```

requires generalizing the transaction journals from fixed places to arbitrary resolved directories.

Do not make this a prerequisite for read-only folder browsing.

**What #75 shipped, and what this work must not undo.** The installer in
`upload-store/src/install.rs` stages under an opaque name in `/READER/UPLOAD`,
parks any predecessor in `/READER/ROLLBACK`, moves the staged file into place
under its long name, and reclaims the predecessor's clusters. One `INSTALL.JNL`
record describes the whole intent, written before anything is touched and
cleared when everything is done. Two properties of it must survive any
later change:

- Only one step frees clusters, and not while two names share a chain. A move
  puts two names on one chain briefly, and cleanup there unlinks rather than
  reclaims.
- The predecessor is identified by cluster chain rather than by name, because
  retiring it frees its alias and the driver re-derives the same alias for the
  replacement.

**A retired approach, recorded so it does not return.** An earlier draft staged
by creating the file under its final long name and hiding it behind a durable
`.PND`/`.CLN` marker. Four review rounds each found another place where state
spread across several directory entries could not distinguish absent from
unknown, old generation from new, or committed from partially cleaned up. It
was compensating for a missing move primitive, and the fork now has one. Do not
repropose it.

When undertaken, that work must separately specify:

- the on-disk directory locator format for `INSTALL.JNL`;
- the locator format/capacity impact for `RECLAIM.JNL`;
- format versioning;
- maximum path/depth;
- recovery when a serialized directory no longer resolves;
- behaviour when the chosen destination has been removed between selection
  and commit;
- global mutation serialization.

That feature targets directories that already exist, so it needs path-aware
journals and does not need mkdir recovery. The two architectural changes stay
separable.

This should be a dedicated storage PRD or milestone.

## Folder creation is out of scope

*(decided 2026-08-22.)* Device-side folder creation is not part of this PRD
and is not on the planned path. Creating and reorganizing the hierarchy are
computer-side operations. Calendula discovers and browses what it finds.

This supersedes the intent recorded on the same day in the retired
user-managed library PRD, that the upload browser could create folders. The
sequencing argument won: arbitrary upload placement, if it is added at all,
targets directories that already exist.

The intended shape, with each step independently justifiable:

```text
Phase 1:  browse arbitrary existing folders
          upload -> /BOOKS

Later:    upload -> user selects an existing folder
          (path-aware journals, no mkdir recovery)

Only if wanted: create, rename or move folders on device
```

**If device-side mkdir is ever wanted**, the crash semantics get designed
then, against real demand rather than in advance. The owner's position is
recorded so it is not re-derived: a bounded power-loss leak is acceptable
there in preference to a third journal, because folder creation is rare,
explicitly user-triggered metadata work. That trade differs from delete,
where the old ordering could leave a visible book unreadable during an
ordinary reader operation.

Accepting it would require proving three properties:

- interruption before publication leaves no visible malformed folder;
- interruption after publication leaves a valid, usable folder;
- the only intermediate loss is a bounded unreachable allocation, with no
  possibility of reclaiming or reusing another chain.

Testing would have to establish a hard bound of one unpublished directory
cluster per interrupted mkdir. If the fork cannot prove those properties,
mkdir needs more machinery than a leak, and that is a reason to design it
properly rather than to ship it cheaply.

## Testing

### Directory enumeration

Cover:

- root-level books;
- nested folders;
- long names;
- duplicate filenames in different directories;
- empty directories;
- ignored unrelated files.

### LFN path resolution

Cover:

- every path component long;
- mixed LFN/8.3 components;
- missing component;
- file where directory expected;
- ambiguous/case variants according to FAT semantics;
- maximum supported depth/length.

### Identity interaction

Verify:

- moving a book between folders preserves `BookId` after reconciliation;
- identical books in separate folders remain independent library entries;
- source-derived cache can still be shared.

### Recovery interaction

Verify:

- browsing respects `shelf_readable`;
- no catalog or index allocation occurs while reclaim is unsettled;
- entering upload mode continues to replay reclaim before any allocation;
- computer-created folder structures do not disturb `INSTALL.JNL`/`RECLAIM.JNL`.

### Large-library behavior

Measure:

- time to enter a directory with 10, 100, and 1000 entries;
- memory use during LFN enumeration;
- boot impact;
- navigation latency.

Optimize with derived indexing only if measurement justifies it.

## Milestones

### Milestone 1: LFN path abstraction

- Introduce canonical `LibraryPath`.
- Implement component-by-component LFN resolution.
- Add host/disk tests.

### Milestone 2: Folder enumeration

- List child folders and EPUBs below `/BOOKS`.
- Keep existing flat view available during transition if useful.

### Milestone 3: Folder UI

- Navigate into/out of folders.
- Open books from arbitrary nested locators.
- Verify X3 UX with a representative card.

### Milestone 4: Identity reconciliation

Integrate with `BookId` so filesystem moves do not reset reading position.

This may land in parallel with the Library Identity PRD rather than being implemented twice.

### Milestone 5: Optional derived directory index

Only if direct enumeration proves too slow.

### Milestone 6: Optional arbitrary upload placement

Separate design and review for path-aware `INSTALL.JNL` and `RECLAIM.JNL`,
targeting existing directories only. No mkdir, and no folder creation on the
device.

## Done when

- A user can organize EPUBs into nested folders on a computer.
- Calendula displays and navigates that physical hierarchy.
- Long directory and EPUB names work.
- A nested EPUB opens normally.
- Paths are treated as locators, not book identity.
- Identical copies can remain separate books.
- Existing journalled install/delete guarantees are unchanged.
- No arbitrary-path write support was required merely to ship folder browsing.
- No device-side directory creation was required either, and none was added.
