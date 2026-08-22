# Reading Position and Layout Durability

Supersedes the derived-state half of the User-Managed Library PRD (its §6.5
and §6.7), deleted in this commit and recoverable at
`git show 26f7238:.scratch/user-managed-library/PRD.md`.

## Summary

Make reading position independent of pagination and typography settings.

A reader's durable place in a reflowable EPUB must be represented as a **content anchor**, not a page number. Pagination is a derived mapping from content into pages for one particular layout configuration.

Changing font, font size, margins, line spacing, orientation, or another pagination-affecting setting must therefore:

1. preserve the user's content anchor;
2. select the page in the new layout containing that anchor;
3. leave pagination for previous layouts cached and reusable rather than eagerly discarding it.

The resulting model is:

```text
BookId
  └── ReadingPosition
        └── ContentAnchor

LocalCacheIdentity (one physical file)
  ├── LayoutId A -> Pagination A
  └── LayoutId B -> Pagination B

SourceDigest (once equivalence is established)
  ├── source-derived artifacts
  └── LayoutId A -> the same pagination, shared or recovered
```

Reading position belongs to a `BookId`.

Pagination belongs to a layout and a source scope. Ordinary opens use the
cheap per-file scope, so opening a book waits on no hashing. A trusted
`SourceDigest` widens that scope later, letting identical copies share
pagination and letting a moved file recover it.

Neither is the other.

## Motivation

The current reader persists a page index.

A page number is meaningful only for the layout that produced it. Changing typography reflows the EPUB, so restoring the same numeric page after a settings change moves the reader to different content.

The same coupling also causes useful pagination data to be discarded when changing settings, even when the reader may immediately switch back to a previously used configuration.

These are separate failures caused by treating derived page geometry as durable reading state.

The reader needs a stable content coordinate system beneath pagination.

## Goals

1. Preserve the reader's logical place across all pagination-affecting settings changes.
2. Store reading position independently of page number.
3. Address reading position by `BookId`, so identical physical copies remain independent.
4. Cache pagination independently for multiple layout configurations.
5. Reuse prior pagination immediately when returning to a known configuration.
6. Allow a position to survive a Calendula-managed replacement of the EPUB when continuity can be established.
7. Keep all pagination data rebuildable.
8. Avoid requiring EPUB CFI or another heavyweight general locator standard unless measurements or interoperability requirements justify it.

## Non-goals

This PRD does not initially require:

- synchronization of positions between devices;
- bookmarks or annotations, although they may later use the same anchor type;
- arbitrary position migration between substantially different editions;
- EPUB CFI interoperability;
- retaining every layout cache forever;
- treating pagination data as authoritative user state.

## Terminology

### BookId

Persistent identity of one library copy.

Reading position is owned by `BookId`.

### SourceDigest

Identity of the current EPUB bytes.

Pagination is derived from a source and therefore includes `SourceDigest` in its cache identity.

### LayoutId

Stable identity for every input that affects pagination.

Conceptually:

```rust
struct LayoutId(...);
```

It must distinguish configurations whose page boundaries can differ.

Inputs include at least:

- display geometry;
- orientation;
- font family;
- font size;
- font metrics/version;
- line spacing;
- margins;
- paragraph/layout rules;
- image-layout behavior;
- pagination algorithm/version.

`LayoutId` should be derived deterministically from those inputs.

### ContentAnchor

A layout-independent coordinate identifying a logical location within an EPUB.

A content anchor must not contain a page number.

### Pagination

A derived mapping from a source scope and a layout to ordered page boundaries
expressed as `ContentAnchor` values:

```text
(LocalCacheIdentity, LayoutId) -> page boundaries      // ordinary open
(SourceDigest, LayoutId)       -> the same boundaries  // shareable
```

`LocalCacheIdentity` is the cheap per-file identity of Source Identity R5. It
decides whether this file's own pagination is still usable, and it authorizes
nothing about any other file.

## Requirements

### R1. Position belongs to BookId

Persist:

```text
BookId -> ReadingPosition
```

not:

```text
SourceDigest -> page
path -> page
```

Two byte-identical books with different `BookId`s therefore retain independent positions.

### R2. Durable position is a ContentAnchor

A persisted reading position must identify content, not derived page geometry.

Conceptually:

```rust
struct ReadingPosition {
    anchor: ContentAnchor,
}
```

A page index may be retained in memory as an optimization but is not authoritative persisted state.

### R3. Anchor representation is stable across layout changes

The initial anchor scheme should identify a location within the EPUB source independently of rendering.

Recommended model:

```text
ContentAnchor {
    spine_item,
    logical_offset,
}
```

`logical_offset` refers to a stable, versioned logical content stream for that spine item.

The logical stream must preserve ordering of renderable content independently of:

- line wrapping;
- font metrics;
- margins;
- screen dimensions;
- page boundaries.

The precise encoding may evolve during implementation, but its persistence contract must be explicitly versioned.

### R4. Anchors must support non-text content

Do not define the durable coordinate system in a way that can represent only text.

Image-only or other non-text renderable content must have positions in the same ordered content space.

An implementation may model the logical stream as ordered content units rather than raw character count if that produces a cleaner parser contract.

### R5. Changing settings preserves the anchor

Before applying a pagination-affecting settings change:

1. retain the current `ContentAnchor`;
2. select or construct pagination for the new `LayoutId`;
3. locate the page whose content range contains the anchor;
4. display that page.

Do not translate:

```text
old page number -> new page number
```

by equality or proportional page count.

The content anchor is authoritative.

### R6. Page boundaries are expressed as anchors

Pagination output must expose enough information to answer:

```text
Which page contains ContentAnchor X?
```

A suitable representation is conceptually:

```text
Page {
    start: ContentAnchor,
    end: ContentAnchor,
}
```

The exact inclusive/exclusive convention must be defined and tested.

### R7. Page turns update the durable anchor

When the reader moves to another page, update reading position to a deterministic anchor associated with the new page.

Recommended initial policy:

```text
position = page.start
```

This gives settings reflow a clear semantic meaning:

> restore the page containing the content that was at the beginning of the reader's previous page.

Persist according to the existing position-write policy rather than introducing an SD write on every rendering operation if one is not already required.

### R8. Layout caches are keyed by a source scope and LayoutId

Pagination is stored under a source scope together with `LayoutId`, and not
merely under the book or the current settings.

The ordinary scope is `LocalCacheIdentity`, so opening a book and reusing its
pagination costs no hashing. Once a `SourceDigest` is trusted for a file, its
pagination may be looked up or promoted under `(SourceDigest, LayoutId)`, and
identical copies may then share it while keeping independent positions.

Sharing requires established equivalence. A cheap identity matching across two
files proves nothing about their contents and may not be used to hand one
file's pagination to another.

### R9. Settings changes do not eagerly delete other layouts

Changing settings must not delete pagination merely because it is no longer active.

If:

```text
Layout A -> Layout B -> Layout A
```

and Layout A remains cached, returning to A should reuse its previous pagination without rebuilding it.

### R10. Cache eviction is independent of settings selection

Pagination remains derived state and may be evicted.

Eviction should be based on an explicit cache policy such as:

- storage pressure;
- age/LRU;
- cache-format version;

not:

- "this layout is not the current layout."

At minimum, normal switching among recently used typography configurations should preserve their pagination.

### R11. Current layout is not part of reading identity

The durable position must remain valid even if the layout used when it was saved is no longer available.

Deleting pagination for a layout never deletes the reading position.

### R12. Source replacement preserves BookId position ownership

For a Calendula-managed replacement:

```text
BookId stays the same
SourceDigest changes
```

The existing reading position remains associated with the `BookId`.

Because its anchor may refer to the previous source, the position layer must attempt migration rather than dropping the position.

### R13. Anchor migration across source changes

A `ReadingPosition` should retain enough information to make reasonable migration possible when the source changes.

**Migration across a changed source is approximate in the first version.**

An offset that remains in range is not evidence that it still points at the
same content. Inserting a thousand characters early in a chapter leaves
`(chapter, 10000)` perfectly valid and pointing somewhere else, and once the
replacement completes the predecessor may already be reclaimed, so there is
nothing left to compare against.

The first implementation therefore promises a best-effort nearby position
based on publication and spine progression, and does not claim exactness
merely because an offset resolves. Silently resetting to page 0 remains
unacceptable.

Exactness is promised where it is real: an unchanged `SourceDigest`, which
covers the settings changes this PRD exists for. See R14.

A context-bearing anchor would allow exact migration across edited editions,
by carrying a small stable fingerprint of the content around the anchor so the
new source can be searched for it:

```text
ContentAnchor {
    spine_item,
    logical_offset,
    context_before_hash,
    context_after_hash,
}
```

That is deferred until there is evidence that minor-edition updates need to
preserve an exact place. Substantially different books stay outside any
guarantee.

### R14. Source identity validates exact anchor interpretation

When `SourceDigest` has not changed, anchor resolution must be deterministic against that source.

The persisted position may therefore record the source identity against which the anchor was last resolved:

```text
ReadingPosition {
    source: SourceDigest,
    anchor: ContentAnchor,
    fallback_progression: ...,
}
```

This makes source replacement detectable without making source identity the owner of the position.

### R15. Approximate progression is fallback, not authority

A normalized progression such as:

```text
0.0 .. 1.0
```

may be persisted alongside the anchor for recovery/migration.

It must not replace the content anchor for ordinary settings changes.

The goal for typography reflow is content continuity, not merely approximately equal percentage through a chapter.

### R16. Position persistence format is versioned

Replace or supersede the current page-index position format with a version that explicitly identifies:

- `BookId`;
- anchor format/version;
- current anchor;
- source identity or source-version evidence;
- optional progression fallback.

An unknown future position format must fail conservatively without modifying the EPUB.

### R17. Pagination format is separately versioned

Do not couple position-format compatibility to pagination-cache compatibility.

A firmware update may invalidate every pagination cache while retaining every reading position.

That distinction is a core architectural requirement.

## Anchor design

The anchor is the most load-bearing new primitive in this PRD.

The implementation should prefer the simplest representation that meets these properties:

1. stable for identical EPUB bytes;
2. independent of typography/layout;
3. cheap to compare and persist;
4. emitted naturally by pagination;
5. resolvable without loading the entire book into RAM;
6. able to represent text and images.

The first implementation does not need a general-purpose standards-compliant locator.

If the parser already exposes a stable ordered content representation, use that rather than inventing EPUB CFI solely for this feature.

The normalization/ordering rules that define `logical_offset` become persistence ABI and must therefore be documented and versioned.

## Layout identity

`LayoutId` should be produced from a versioned canonical layout descriptor.

Conceptually:

```text
LayoutDescriptor {
    viewport,
    orientation,
    font,
    font_size,
    line_spacing,
    margins,
    renderer_revision,
    ...
}
        |
        v
      hash
        |
        v
     LayoutId
```

The full descriptor may also be stored with a cache for diagnostics.

Changing an input that cannot affect pagination should not unnecessarily create a new layout identity.

Changing any input that can affect page boundaries must.

## Runtime flow

### Open book

```text
BookId
  |
  +-> ReadingPosition(ContentAnchor)
  |
  +-> LocalCacheIdentity of the file at this locator
          |
          +-> active LayoutId
                  |
                  +-> existing pagination?
                        | yes -> use
                        | no  -> build
  |
  +-> resolve ContentAnchor to page
```

No hashing appears anywhere in that flow. A trusted `SourceDigest` enters only
when pagination is shared across copies or recovered after a move, which is a
separate path from opening a book.

### Turn page

```text
current page
    |
    v
next page
    |
    v
persist next_page.start as ContentAnchor
```

### Change typography

```text
retain ContentAnchor
       |
       v
calculate new LayoutId
       |
       +-> pagination cached -> reuse
       |
       +-> not cached -> build
       |
       v
find page containing retained anchor
       |
       v
display without changing logical reading place
```

### Switch settings back

```text
retain current anchor
       |
       v
old LayoutId
       |
       v
reuse old pagination
       |
       v
resolve current anchor in it
```

The old page number does not need to be remembered.

## Cache layout

Exact filenames are implementation-specific, but conceptually:

```text
/READER/
  files/
    <LocalCacheIdentity>/          the ordinary open path
      layouts/
        <LayoutId>/
          pagination
  sources/
    <SourceDigest>/                shared once equivalence is established
      layouts/
        <LayoutId>/
          pagination
          ...
```

An artifact may be promoted from the first tree to the second once a digest is
trusted for that file. Promotion is an optimization, and losing either tree
costs a rebuild rather than a reading position.

Per-book state remains separate:

```text
/READER/
  books/
    <BookId>/
      position
```

This separation ensures:

- identical copies can share pagination;
- deleting one copy does not destroy another's position;
- changing layout does not move user state;
- evicting pagination cannot erase reading place.

## Migration

### Current page-index positions

Existing positions expressed as page indices cannot be transformed without knowing the pagination that gave those indices meaning.

Migration should therefore occur while the old pagination is still available when possible:

1. load old position;
2. load/rebuild the corresponding old layout pagination;
3. obtain the stored page's content start anchor;
4. persist the new anchor-based position format.

If exact reconstruction is unavailable, use the best available progression fallback rather than silently resetting to the beginning.

### Existing pagination caches

Existing caches may be:

- reused if their format can provide stable page boundaries;
- migrated once;
- invalidated and rebuilt.

Their loss must not imply loss of the newly migrated position.

## Failure handling

### An externally replaced file whose cheap identity still matches

Local cache reuse prioritizes open latency, so a book a computer replaced with
a same-size edition may reopen against its existing pagination and position
before anything notices. That is a bounded false positive within one file: the
cost is a rebuild once a later operation establishes equivalence and
reconciles the changed source, and the anchor migration rules of R13 apply
then.

No state is shared with another physical file on that evidence. Cheap identity
matching decides only whether this file may reuse its own work.

### Missing pagination

Rebuild it and resolve the anchor.

### Corrupt pagination

Discard and rebuild it.

Do not alter reading position merely because derived pagination is corrupt.

### Unresolvable anchor with unchanged source

Treat this as a position/cache-format defect.

Use the stored progression fallback if available and surface diagnostics in development/test builds.

### Changed source

Attempt anchor migration according to R13.

### Missing position

Start from the normal beginning-of-book behavior and create a position after navigation.

## Testing

### Reflow invariance

For a fixture book:

1. open at a known content anchor;
2. record the visible content;
3. change font size;
4. repaginate;
5. require the retained anchor to be visible;
6. repeat for margins, line spacing, and supported orientation changes.

Do not assert equal page numbers.

### Round-trip settings

Exercise:

```text
Layout A
 -> Layout B
 -> Layout A
```

Require:

- logical position preserved at every transition;
- original Layout A pagination reused on return;
- no unnecessary rebuild.

### Duplicate copies

Two identical EPUBs:

- share `SourceDigest`;
- may share pagination for a given `LayoutId` **once content equivalence has
  been established**, and not before, since a matching cheap identity is not
  evidence about contents;
- have distinct `BookId`s;
- retain independent anchors.

### Restart durability

Persist an anchor, reboot, reopen the book, and require the same logical content to be selected under:

- the same layout;
- a different layout.

### Cache deletion

Delete pagination while retaining position.

Reopen and require rebuilt pagination to resolve the same anchor.

### Source replacement

Perform a Calendula-managed replacement while partway through a book.

Require:

- `BookId` preserved;
- `SourceDigest` updated;
- position not discarded;
- approximate migration performed according to R13, without claiming exact
  content continuity.

### Image/non-text anchors

Use a fixture containing pages/sections dominated by images and verify those locations can be persisted and restored without relying on nearby text.

## Milestones

### Milestone 1: ContentAnchor

- Define the stable ordered content coordinate.
- Make parser/pagination expose anchors.
- Add anchor invariance tests across layout settings.

### Milestone 2: Anchor-based positions

- Introduce the new position format.
- Address it by `BookId`.
- Persist page-start anchors instead of page indices.
- Migrate existing positions.

This PRD owns that migration. Library Identity establishes the `BookId`
mapping and defers the format change here, so a stored position moves to its
new owner and its new representation in one step rather than through an
intermediate format that carries a page index under a `BookId`.

### Milestone 3: LayoutId

- Define canonical layout descriptor.
- Key pagination by `(LocalCacheIdentity, LayoutId)` for the ordinary open
  path, with no hashing.
- Leave `(SourceDigest, LayoutId)` for the sharing and recovery path, which
  arrives with the identity work rather than here.

### Milestone 4: Multi-layout cache retention

- Stop deleting prior pagination on settings changes.
- Add cache reuse and eviction policy.
- Pin A → B → A behavior.

### Milestone 5: Source replacement migration

- Preserve `BookId` across managed replacement.
- Detect source change.
- Map the old position by publication and spine progression.
- Persist and use the progression fallback.
- Do not claim exact content continuity across a changed `SourceDigest`.

## Done when

- Changing typography does not move the reader's logical place.
- Reading position contains no authoritative page index.
- Two identical copies retain independent positions.
- Identical copies can share pagination.
- Switching back to a recently used layout reuses its pagination.
- Deleting/rebuilding pagination cannot erase reading place.
- A managed EPUB replacement does not automatically orphan the book's position.
- Position persistence and pagination-cache persistence are separate, independently versioned concerns.
