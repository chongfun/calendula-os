# User-Managed Library — PRD

Status: **Draft for review.** Requirements marked *(stated)* come from the
product owner; *(inferred)* are the author's reading and need confirmation;
*(decided)* were settled in review on 2026-08-13 and the alternative is
recorded so it is not silently relitigated. Facts marked *(verified)* were
read from the pinned dependency or measured on hardware, and are cited.

This document **absorbs the `cache3-architecture` draft**, which is superseded
and has been deleted. It is recoverable at `833221b` — `git show
833221b:.scratch/cache3-architecture/PRD.md`. *(The draft previously cited
`49b2c0a` for the same content; that SHA is a pre-rebase copy, unreachable from
any branch and eventually collectable. Cite the reachable one.)*

Scope is deliberately independent of the on-device image-rendering PRD.
Nothing here depends on render bundles, and nothing here blocks them; the two
meet only at the point where a book has an identity.

---

## 0. What changed on 2026-08-13, and why this was rewritten

**PR #75 shipped the naming half of this PRD**, and it did not ship the design
sketched here. Reading the previous draft against the tree now would mislead on
three counts, so the affected sections are rewritten rather than annotated:

- **Long-name creation, deletion, and a same-volume move all exist**, in our
  own fork. The previous §4 was written against a pin that no longer describes
  this repository. See §4.
- **The staging design here was tried and abandoned.** The old §6.2 argued that
  because no rename existed, staging had to mean "create under the final name
  and hide it behind a marker." A rename exists now, and the marker protocol
  was retired for documented reasons across four review rounds. See §6.1.
- **The previous draft's own Q4 — "land the long-name port first as its own
  slice?" — was answered by events.** That slice is #75. What remains is
  folders, identity, and derived state.

One thing #75 also made visible, which no version of this document has
addressed: **the shipped installer's safety promise is explicitly "this device
is the only writer."** A user-managed library is, definitionally, a card edited
elsewhere. §5.3 and §6.2 now carry a policy for that rather than leaving it
implicit.

## 1. Problem

The device can read a library the user organizes, and since #75 it writes real
filenames into it. What it still cannot do is let the user decide **where** a
book goes, or keep anything it derived from a book when the user moves that
book on a computer.

Three defects share one root — derived state keyed to where a book *is* rather
than what it *is*:

- Every uploaded book lands in one directory. The user picks the name now, not
  the place.
- Reorganizing the card from a computer discards a book's cache and its reading
  position, because both are keyed by a hash of its path.
- Changing type settings moves the reader's place, because the position is
  stored as a page index and a settings change reflows the text. It also
  rebuilds pagination for a configuration the reader may switch straight back
  to.

## 2. Goals

1. A book uploaded over the hotspot lands in **a folder the user picks at
   upload time**. *(stated)* — the filename half of this shipped in #75.
2. The SD card remains organizable from a computer: real directories, real
   filenames, freely rearranged. *(stated)*
3. Sideloaded books continue to work exactly as they do now, including the
   on-device decode fallback for images. *(stated)*
4. The library scales to 1000+ books without holding the library in RAM.
   *(stated)*
5. Reorganizing the card from a computer does not destroy derived state —
   reading positions, caches, and (later) render bundles survive a file being
   moved or renamed. *(stated)*
6. Changing type settings preserves the reader's place in the book, and does
   not discard work that does not depend on those settings. *(stated)*

## 3. Non-goals

- Image rendering, render bundles, and the device decoder. Separate PRD.
- The managed-slot workspace redesign (`source-store` index/streaming).
  Affected by §6.4, sequenced separately.
- Tags or saved searches. Folders first; tags may layer on later.
- Any migration of existing cards. Starting fresh is acceptable. *(stated)*
- Supporting a card edited by another writer *concurrently with* a device
  transaction. §5.3 makes this detectable and survivable, not supported.

## 4. Verified constraints

Properties of the revision this repository pins **today**: our fork
`chongfun/embedded-sdmmc-rs`, branch `calendula/long-names-and-error-fidelity`,
pinned in `[workspace.dependencies]`. Rev omitted here on purpose — it moves,
and the manifest is the authority.

The old pin comment warning that newer upstream SD rewrites "fail cold card
init on the X4" is **retired**. It was upstream's observation on upstream's
board, never reproduced here, and the fork is now on the v0.10 base and boots
on the X3.

1. **Long filenames can be read *and written*.** *(verified)* The fork adds
   long-name creation, and #75 ships books to the shelf under real names.
2. **Every long-named file also has a unique 8.3 alias**, tied to the chain by
   checksum. *(verified)* **The driver derives the alias itself** and refuses a
   long name already present in the destination, so nothing above the driver
   chooses or records an alias. A freed alias is reused by whatever is created
   next, which is why an alias cannot be an identity.
3. **A same-volume move exists.** *(verified)* `move_file_in_dir_lfn` renames by
   rewriting directory entries and leaving the cluster chain alone. It is the
   primitive #75's installer is built on.
4. **Delete removes the whole long-name chain.** *(verified)*
   `mark_directory_slots_deleted` marks the short entry first as the commit
   point, then the LFN slots. **This supersedes the previous draft's
   orphan-entry constraint**, which described the old upstream pin. The
   `chkdsk`-clean acceptance criterion is met by the current pin rather than
   being work.
5. **Directory lookup is a linear scan.** *(verified)* Both
   `find_directory_entry` and the delete path walk directory blocks, so cost
   grows with entries per directory. Folders are a performance property, not
   only an organizational one.
6. **The first cluster is the only on-card identifier a move preserves.**
   *(verified)* #75's install journal names a book's predecessor by
   `ClusterId`, precisely because a move rewrites the long name and a retired
   alias is re-derived for whatever replaces it. This is a *transaction-scoped*
   identity, not a library one — a cluster number is reused after free. §6.3
   explains why this PRD needs a second, content-derived identity rather than
   reusing it.
7. **Uploads are ~88 KB/s end to end** (2.5 MB in 28.4 s, X3, 2026-08-06),
   with the network near 160 KB/s. *(verified)* Cause unestablished. Note this
   predates #75, which changed the write path; see §9.
8. **The installer does not support concurrent outside writers.** *(verified,
   from `upload-store/src/install.rs`)* Its stated promise is power-loss safety
   "while this device is the only thing writing to the card." FAT counts no
   references, so a chain freed by another writer is indistinguishable from one
   still held. Where it can, the installer refuses rather than destroys.

## 5. Requirements

### 5.1 Naming and placement

- R1. The upload request carries a **destination folder**. *(the filename half
  shipped in #75)*
- R2. The device creates the file with the requested filename, preserving case
  and permitted characters, and rejects a name it cannot represent rather than
  substituting one. *(shipped, #75)*
- R3. The 8.3 alias is the driver's to choose and is never shown to the user.
  *(shipped, #75 — restated because it constrains §6.3)*
- R4. The device creates the destination folder if absent, subject to the same
  naming rules.
- R5. Deleting a long-named book removes its whole chain. *(satisfied by the
  pin, §4.4)*

### 5.2 Identity and reorganization

- R6. A book's identity is derived from its **content**, not its path: exact
  length plus a hash over sampled regions. *(decided — §6.3 gives the
  construction and the rejected alternative)*
- R7. A book's path is a **locator**: a hint for finding bytes, repairable when
  the user moves or renames the file, and carrying no authority.
- R8. Moving or renaming a book from a computer preserves its identity, and
  therefore everything bound to that identity.
- R9. **Each locator is its own book.** Two files with identical bytes in two
  folders are two books with two reading positions. Identity exists to *repair
  a move*, not to merge copies. *(decided; supersedes the previous draft's
  "one logical book, many locators")*

  The rejected reading followed from "identity is content" and is coherent, but
  its user-visible consequence is that two people reading the same EPUB from
  separate folders share one place in it, and that deleting one copy re-points
  the record at the other. Repair is the behaviour goal 5 actually asks for;
  merging is a different feature nobody requested.

- R10. A move is recognized as: a record's locator no longer resolves, **and**
  exactly one unclaimed file with matching identity exists. If the old locator
  still resolves, or more than one candidate matches, the device changes
  nothing — that is a copy, not a move. *(inferred from R9; the conservative
  direction, since doing nothing costs a rebuild and guessing costs the
  reader's place)*

### 5.3 Staging, crash safety, and outside writers

- R11. A partially uploaded file is never presented as a book. *(shipped, #75)*
- R12. Interruption at any point leaves the card in one of exactly two states:
  no book, or a complete and readable book. *(shipped, #75)*
- R13. Cleanup is restartable and carries no in-memory state across a reboot.
  *(shipped, #75 — the card's state is the progress record)*
- R14. **Recovery runs before the library is scanned or a cached catalog is
  trusted.** *(shipped, #75)*
- R15. **When recovery cannot resolve what it finds, the device surfaces it
  rather than guessing.** *(decided)* A card edited underneath an open
  transaction can present a combination the plan does not map — a predecessor
  deleted from a computer mid-install, a destination name now held by a foreign
  file. The device must leave the card alone in that case and say so. A visible
  odd state is recoverable by the user; a silent wrong guess costs a book.

### 5.4 Library

- R16. The library browses **physical folders**, one directory at a time, so
  resident memory is bounded by folder size and not by library size.
- R17. Books display their real title where known, falling back to the
  filename — the current cached-title behavior is preserved rather than
  regressing to filenames.
- R18. Uploaded and sideloaded books appear together, indistinguishable to the
  reader except where integrity state differs.

### 5.5 Derived state: cache and reading position

Absorbed from the `CACHE3 + POS3` draft, with identity corrected per R6.

- R19. Cache and position records are addressed by **content identity**, not by
  a path-derived hash. The current key is `source_hash: u32` =
  `FNV(display_path, byte_size)` (`proto/src/cache.rs`), which makes moving a
  book orphan its cache and lose its position — what R8 forbids.
- R20. Cached work that does not depend on type settings — the parsed spine,
  TOC, cover, container index — is not discarded when type settings change.
- R21. Reading position is stored as a **content anchor** independent of
  layout: `(spine index, content-block ordinal)`, a position in the `CONT.BIN`
  stream, with the page number derived for the current settings. Paragraph
  granularity is sufficient *(stated)*; the format should leave room for an
  optional intra-paragraph offset without a version bump.
- R22. Position survives cache clearing, eviction, and any number of layout
  changes. Losing cached pages is an inconvenience; losing the reader's place
  is not.
- R23. Two type-setting configurations coexist on card, so alternating between
  them does not rebuild pagination each time.
- R24. A cache generation is valid only once fully written. Interruption leaves
  it ignorable and reclaimable, never partially loadable.
- R25. Cache directories carry enough of a book's identity in their path that
  distinct books cannot collide on a truncated key.
- R26. Superseded cache layouts are removed on a best-effort background sweep.
  No in-place migration; rebuilding is cheap and correct, migrating is neither.

## 6. Design

### 6.1 What #75 already provides, and what this PRD must not undo

The installer in `upload-store/src/install.rs` is the foundation for everything
in slice 1. Its shape:

```text
/XTEINK/UPLOAD/<txn>   scratch, opaque, no long name — nothing scans it
/BOOKS/<old>  --move--> /XTEINK/ROLLBACK/<txn>   predecessor parked
/XTEINK/UPLOAD/<txn> --move--> /BOOKS/, under the long name
/XTEINK/ROLLBACK/<txn> --delete, reclaiming its clusters
```

One `INSTALL.JNL` record describes the whole intent. It is written before
anything is touched, cleared when everything is done, never updated in between,
and while it stands it owns the names it describes — further uploads and
deletes are refused until it clears. Progress is not recorded because the four
places a file can be at rest determine the next action uniquely.

**Two properties of it are load-bearing for this PRD and must survive:**

- *Only one step frees clusters, and never while two names share a chain.* A
  move puts two names on one chain briefly; cleanup there must unlink, never
  reclaim.
- *The predecessor is identified by cluster chain, not by name*, because
  retiring it frees its alias and the driver re-derives the same alias for its
  replacement.

**The retired approach, recorded so it does not return.** The previous draft
staged by creating the file under its final long name and hiding it behind a
durable `.PND`/`.CLN` marker. That protocol was abandoned after four review
rounds each found another place where state spread across multiple directory
entries could not distinguish "absent" from "unknown", "old generation" from
"new", or "committed" from "partially cleaned up". It was compensating for a
missing move primitive. The move now exists.

### 6.2 Slice 1 — destination folders on the shipped installer

The installer already moves a finished file to a chosen name in a chosen
directory. Slice 1 generalizes *which* directory, and the work is mostly about
what the journal owns.

- The destination becomes a path rather than today's `in_books` boolean
  (`fw/src/upload.rs`, `book_build.rs`). Book location is currently a two-value
  choice — `/BOOKS` or the card root — and folders make it a path.
- **The journal's name-ownership widens to path-ownership.** Today a standing
  record owns the names it describes; with folders it owns
  `(directory, long name)` pairs, and the refusal that protects them has to key
  on both. A record naming `/SciFi/Dune.epub` must not block an upload to
  `/Fantasy/Dune.epub`.
- Folder creation (R4) happens before the journal record is written, because a
  failed `mkdir` after the record stands is an unresolvable state — it makes
  the record describe a destination that cannot exist.
- `/XTEINK/UPLOAD` and `/XTEINK/ROLLBACK` stay where they are. Staging is not
  per-folder; the move is same-volume, so the scratch directory's location is
  irrelevant to cost.

**Catalog: keep one file in slice 1.** #75 clears a single `CATALOG.BIN` and
**refuses the whole session if it cannot prove it gone** — the snapshot must
not survive a change it does not describe. Per-directory catalogs turn one
proof into N, and an N-way partial failure has no equally clean answer. So
slice 1 keeps a single catalog whose records carry full paths, and
per-directory catalogs move to slice 2, where the identity rework forces a
format change anyway. **This sequences the on-disk break into one event rather
than two.** *(inferred; the alternative is per-directory catalogs in slice 1
and a definition of what a partial invalidation means)*

### 6.3 Content fingerprint

**Identity is `(exact_length, SHA-256 over sampled regions)`.** *(decided)*

```text
identity = ( length,
             SHA256( head  ‖ tail  ‖ interior[0..N] ) )

head      first 64 KiB
tail      last  64 KiB
interior  N fixed-size blocks at offsets that are a pure
          function of `length` alone
```

Offsets must depend on nothing but the length, so the same bytes always produce
the same identity on any device and after any move. Files below
head+tail+interior are hashed whole, which makes the small-file case exact
rather than special.

**Why sampling rather than the whole file.** The full-SHA-256 reading of
"identity is content" is the honest one, and it was rejected on cost, not
principle. Recognizing a moved book means hashing it; hashing is software
(`sha2`, as used for OTA images — no hardware SHA is wired up); and the work
scales with **total bytes on the card**, so a rescan after the user reorganizes
a full card reads and hashes the whole card. Sampling makes the same rescan
scale with book *count*: ~256 KiB per book instead of an entire book.

**Why it is safe enough for EPUBs, stated honestly.** Two books would have to
share an exact length *and* every sampled region. An EPUB is a zip: its central
directory sits at the tail and its first local header at the head, so two
different books colliding is a constructed case rather than an accidental one.
Two *builds* of the same book — recompressed, re-ordered — differ in length or
in the central directory almost always.

**The blast radius, which is the part that makes this acceptable.** A collision
binds one book to another's cache and reading position. That is a wrong place
in a book and a stale pagination, repaired by clearing the cache. It is not
corruption, it cannot cross into file contents, and it cannot lose a file. The
same collision under a scheme that *merged* books (rejected R9) would have been
worse; under R9 as decided, each locator keeps its own record and a collision
misroutes one binding.

**Not reusable for this: the first cluster** (§4.6). It is the right identity
inside a transaction and the wrong one across time, because FAT reuses a freed
cluster number for whatever is written next. A record keyed on it would
silently re-point at an unrelated file after a delete.

### 6.4 Records

Source metadata keeps its durable A/B commit protocol unchanged. What changes
is how a record points at its book: the fixed 12-byte 8.3 `unmanaged_name`
becomes a path locator, and lookups key on content identity (R6). The existing
bare-filename field cannot express folders and would treat `/SciFi/BOOK1.EPU`
and `/Fantasy/BOOK1.EPU` as one book.

This is where the milestone touches the image-rendering PRD's M0S work, and the
touch is narrow: the record protocol, idempotency, receipts, container gate, and
validation rules are unaffected.

### 6.5 What a settings change actually rebuilds

`CONT.BIN` already captures the parsed content stream — the `push_block`
sequence of `(spine_index, text, role, style, align, paragraph_end)`, all
content-derived — and a type-settings change replays it rather than re-reading
the zip, inflating, and parsing XML. So the expensive half of R20 holds today,
and `CACHE3`'s placement of `CONT.BIN` under `COMMON/` is correct. What a
settings change legitimately rebuilds is line breaking and page assembly, which
is what a settings change *is*.

This is also why the anchor cannot be a block index. `BlockRecord` carries
`line_count` and is produced by replaying the content stream through the layout
engine, so it is layout-derived and no more stable than a page number. The
stable unit is the position in the `CONT.BIN` stream itself.

The anchor costs nothing on the reading path: it is written at page turn as an
ordinal the layout already holds, and resolved only during a layout pass that a
settings change performs regardless.

### 6.6 Cache and position identity

`CACHE3`'s two-level directory split is kept — it bounds directory size, which
§4.5 makes a performance requirement and not only a tidiness one — but the
components become content-derived: a prefix of the fingerprint plus the exact
length, both already computed for R6. Directory names stay within 8.3, which
the device must satisfy to create them.

Note this changes `source_hash`/`source_size` in `BookV2Header` and
`SectionV2Header`, so it is a cache format break and a version bump. Per R26
there is no migration: the sweep removes the old layout and the next open
rebuilds.

Reading position becomes the R21 anchor. The open path resolves it against the
current layout; the page number stops being stored at all. `POS3`'s separation
from the cache is what keeps position durable across eviction.

### 6.7 Derived-state layout

`<identity>` is content-derived (R6). Shown alongside the directories #75 owns,
because they share the namespace:

```text
/XTEINK/
  UPLOAD/            #75: upload scratch, opaque names
  ROLLBACK/          #75: parked predecessors
  INSTALL.JNL        #75: standing transaction intent
  CATALOG.BIN        one catalog in slice 1; per-folder in slice 2
  CACHE3/<identity>/
    COMMON/          layout-independent: TOC.BIN, COVER.BIN, CONT.BIN
    SLOT0/ SLOT1/    one per type-setting configuration (R23)
      BOOK.BIN       written last; the slot's commit record (R24)
      SECTIONS/S<nnn>.BIN
    RECENTA.BIN RECENTB.BIN   advisory recency for eviction
  POS3/<identity>/
    POSA.BIN POSB.BIN          reading position, isolated from cache (R22)
```

Publication empties the victim slot, writes sections, then writes `BOOK.BIN`
last, so an interrupted publication leaves a slot that fails validation and is
reclaimed rather than half-loaded. Recency is advisory: losing it costs a wrong
eviction choice, never correctness.

### 6.8 Library scan

Per-directory catalogs rather than one flat file, so entering a folder reads
only that folder's records and cached titles (R16, R17). **Slice 2**, per
§6.2's sequencing argument.

Identity repair (R10) runs as part of the scan: a record whose locator does not
resolve is a candidate for re-pointing, resolved against unclaimed files with
matching identity. This is the one place the fingerprint cost lands on a path
the user waits for, which is why §9 gates it.

## 7. Slices

Two, sequenced, each independently shippable. *(decided)*

**Slice 1 — placement.** R1, R4, R16, R17, R18, plus the journal path-ownership
and folder-creation ordering of §6.2. Builds directly on #75. Delivers the
stated goal that is currently half-met: the user picks the folder, and the card
browses the way it is organized. No identity work, no cache format change, one
catalog.

**Slice 2 — identity and derived state.** R6–R10, R19–R26. Changes the cache
key, the position format, and the catalog layout in one on-disk break.

Splitting here works because slice 1 changes no derived-state format and slice
2 changes them all at once. The previous draft argued identity and cache could
not be split from naming; that was true when naming was in scope, and naming
shipped.

## 8. Acceptance

**Slice 1**

- A book uploaded to a chosen folder appears on a computer under its real name,
  in that folder.
- A book uploaded to `/SciFi/Dune.epub` is not blocked by a standing record for
  `/Fantasy/Dune.epub`, and is blocked by one for its own path.
- Creating the destination folder fails cleanly *before* any journal record
  exists; no failure leaves a record naming an absent directory.
- A 1000-book library browses with resident memory bounded by folder size.
- Deleting a long-named book leaves no orphan directory entries (`fsck`/
  `chkdsk` clean). Expected to pass on the current pin (§4.4) — this is a
  regression guard, not new work.

**Slice 2**

- Moving a book to another folder on a computer, then remounting, leaves it
  readable with its position intact.
- Renaming it on a computer does the same.
- Moving *and* renaming in one action does the same. (This is the case the
  rejected locator-only scheme fails, so it is the criterion that proves the
  fingerprint earns its cost.)
- Copying a book to a second folder produces **two** books with independent
  positions, and re-pointing does not occur while both files exist (R9, R10).
- Changing type settings on an open book leaves the reader on the same text,
  not the same page number, and does not rebuild the parsed spine, TOC, or
  cover.
- Alternating between two type-setting configurations does not rebuild
  pagination on each switch.
- A cache publication interrupted at any point leaves no slot that loads
  partially; the next open reclaims it.

**Both**

- Power loss at every step yields either no book or a complete readable book.
  **The harness this needs does not exist**: the previous draft cited
  `tools/powercut_campaign.py`, which is referenced nowhere else in the repo
  and has never existed. Either build it or name the real procedure — #75 was
  validated by a hand-run reset campaign, and that is worth automating once
  rather than repeating.
- A card edited underneath an open transaction leaves the device saying so,
  not guessing (R15). Test by deleting the predecessor from a computer with a
  record standing.
- No watchdog or responsiveness limit is violated; no request blocks on a
  whole-library operation.

## 9. Measurement gates

- **Fingerprint cost, per book and per rescan.** *(gates slice 2)* Measure
  SHA-256 throughput on the X3 and the wall time to fingerprint one book, then
  a 1000-book card. The sampled construction was chosen to make this bounded by
  book count; that has to be confirmed, not assumed. If it lands somewhere that
  cannot run on the scan path, the fallback is a background pass with the
  library usable from locators meanwhile — decide that *after* the number
  exists. **Check first whether the C3's hardware SHA is reachable through
  `esp-hal`**; nothing in the tree uses it today and it would change the answer.
- **Upload throughput.** §4.7's ~88 KB/s predates #75 and its write path.
  Re-take before quoting; the roadmap's WS-D owns the ceiling investigation.
- **Per-directory catalog read cost** at the folder sizes a 1000-book library
  actually produces. *(gates slice 2's catalog split)*
- **Directory-scan cost at depth**, since §4.5 makes lookup linear in entries
  and folders are the mitigation.

## 10. Open questions

1. What happens to a book whose file the user deletes from a computer — is the
   record reclaimed automatically, or does the book show as unavailable until
   the user acts? Interacts with R10: a deleted book and a moved book look
   identical until a matching candidate is found.
2. Does the browser pick from existing folders only, or may it create new ones
   at upload time? R4 assumes creation is allowed; if not, slice 1 loses the
   folder-creation ordering constraint in §6.2.
3. Should sideloaded books receive records in slice 1, or only when slice 2
   needs to persist something derived from them?
4. How is R15's "surface it" presented? A library-level notice, a per-book
   state, or a boot-time screen. The requirement is that it is not silent; the
   form is open.
5. *(answered 2026-08-13)* The long-name port landed as #75. The eight-commit
   upstream backlog from 2026-08-07 is resolved for this PRD's purposes;
   `9b0123d` "Preserve UTF-8 in catalog labels" should be checked against the
   catalog work in slice 1.
