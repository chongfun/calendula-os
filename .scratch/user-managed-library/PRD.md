# User-Managed Library — PRD

Status: **Draft for review.** Requirements below marked *(stated)* come from
the product owner; those marked *(inferred)* are the author's reading and
need confirmation. Facts marked *(verified)* were read from the pinned
dependency or measured on hardware and are cited.

Scope is deliberately independent of the on-device image-rendering PRD.
Nothing here depends on render bundles, and nothing here blocks them; the
two meet only at the point where a book has an identity.

---

## 1. Problem

The device can read a library the user organizes, but it cannot *write*
into one. Books uploaded over the wireless hotspot land in a single
directory under machine-generated 8.3 names — `8620JLY1.EPU` — because the
SD driver can only create short filenames. The device shows a real title
(read from the EPUB), so the defect is invisible on the device and total on
a computer: a card of uploaded books cannot be organized by hand, because
the files cannot be identified by name.

That is the whole problem. Reading long filenames already works.

## 2. Goals

1. A book uploaded over the hotspot lands in **a folder the user picks at
   upload time**, under **its real filename**. *(stated)*
2. The SD card remains organizable from a computer: real directories, real
   filenames, freely rearranged. *(stated)*
3. Sideloaded books continue to work exactly as they do now, including the
   on-device decode fallback for images. *(stated)*
4. The library scales to 1000+ books without holding the library in RAM.
   *(stated)*
5. Reorganizing the card from a computer does not destroy derived state —
   reading positions, caches, and (later) render bundles survive a file
   being moved or renamed. *(stated)*
6. Changing type settings preserves the reader's place in the book, and
   does not discard work that does not depend on those settings. *(stated)*

## 3. Non-goals

- Image rendering, render bundles, and the device decoder. Separate PRD.
- The managed-slot workspace redesign (`source-store` index/streaming). It
  is affected by §6.3 but sequenced separately.
- Tags or saved searches. Folders first; tags may layer on later.
- Any migration of existing cards. Starting fresh is acceptable. *(stated)*

## 4. Verified constraints

These are properties of the pinned `embedded-sdmmc`
(`d26892f7405469eba95741763ab39bda2239d5ec`) unless noted. The pin is not
casually movable: later upstream revisions fail cold card init on the X4
(see the note in `fw/Cargo.toml`).

1. **Long filenames can be read, not written.** *(verified)*
   `Directory::iterate_dir_lfn` walks the long-name chain, but
   `open_file_in_dir` and `make_dir_in_dir` are bounded by
   `ToShortFileName`. A file the device creates has an 8.3 entry and no
   long name at all — on the device *or* on a computer.
2. **Every long-named file also has a unique 8.3 alias**, and the read path
   ties the chain to it by checksum (`csum` plus sequence tracking in the
   directory state machine). *(verified)* Generating that alias is
   therefore part of any write support, and it must be unique within the
   directory.
3. **There is no rename or move API.** *(verified)* No `rename`/`move_file`
   exists anywhere in the driver.
4. **Delete removes only the short entry.** *(verified)*
   `delete_entry_in_block` writes `0xE5` over the matched 8.3 entry and
   nothing else, so deleting a long-named file orphans its chain entries.
   Because reads are checksum-guarded, an orphan is *clutter, not
   corruption* — it cannot be misattributed to another file — but it
   consumes directory entries and `chkdsk` will flag it.
5. **Directory lookup is a linear scan.** *(verified)* Both
   `find_directory_entry` and the delete path walk directory blocks, so
   cost grows with entries per directory. Folders are a performance
   property, not only an organizational one.
6. **Upload throughput is ~88 KB/s end to end** (2.5 MB committed in
   28.4 s, X3, 2026-08-06), with the network itself near 160 KB/s. *(verified)*
   Nothing has yet established what caps it; see §8.

## 5. Requirements

### 5.1 Naming and placement

- R1. The upload request carries a **destination folder** and a **filename**.
- R2. The device creates the file with that filename, preserving case and
  characters that FAT long names permit; it rejects a request whose name
  cannot be represented, rather than silently substituting one.
- R3. The device generates a unique 8.3 alias for each created file. The
  alias is an implementation detail and never shown to the user.
- R4. The device creates the destination folder if absent, subject to the
  same naming rules.
- R5. Deleting a device-created or user-created file removes its long-name
  chain along with its short entry.

### 5.2 Identity and reorganization

- R6. A book's identity is its **content** — exact length plus complete
  SHA-256 — not its path. *(stated)*
- R7. A book's path is a **locator**: a hint for finding bytes, repairable
  when the user moves or renames the file, and carrying no authority.
- R8. Moving or renaming a book from a computer preserves its identity, and
  therefore everything bound to that identity.
- R9. Two files with the same name in different folders are different
  books. Two copies of identical bytes are **one** logical book with more
  than one locator. This is not an independent choice: it follows from R6,
  since identical bytes cannot produce different identities without
  reintroducing the path dependence R6 removes. The simplifying
  consequence is that a record holds one locator at a time and may be
  re-pointed at any copy whose content matches. *(resolved)*

### 5.3 Staging and crash safety

- R10. A partially uploaded file is never presented as a book.
- R11. Interruption at any point leaves the card in one of exactly two
  states: no book, or a complete and readable book. Nothing in between is
  visible to the reader.
- R12. Cleanup of interrupted uploads is restartable and requires no
  in-memory state carried across a reboot.

### 5.4 Library

- R13. The library browses **physical folders**, one directory at a time,
  so resident memory is bounded by folder size and not by library size.
- R14. Books display their real title where known, falling back to the
  filename — the current catalog's cached-title behavior is preserved
  rather than regressing to filenames.
- R15. Uploaded and sideloaded books appear together, indistinguishable to
  the reader except where integrity state differs.

### 5.5 Cache and reading position

The companion `CACHE3 + POS3` draft (`.scratch/cache3-architecture/`,
commit `49b2c0a`) covers the cache layout itself. These are the
requirements this PRD places on it, because they follow from R6 and R8.

- R16. Cache and position records are addressed by **content identity**,
  not by a path-derived hash. The current `source_hash` is
  `FNV(display_path, byte_size)`; keying the new layout on it would make
  moving a book on a computer orphan its cache and lose its position,
  which R8 forbids.
- R17. Cached work that does not depend on type settings — the parsed
  spine, TOC, cover, container index — is not discarded when type settings
  change.
- R18. Reading position is stored as a **content anchor** that is
  independent of layout — `(spine index, content-block ordinal)`, a
  position in the `CONT.BIN` stream — and the page number is derived from
  it for the current settings. Storing a page index means a settings
  change silently moves the reader, which is a defect present today.
  Paragraph granularity is sufficient *(stated)*; the format should leave
  room for an optional intra-paragraph offset without a version bump.
- R19. Position survives cache clearing, cache eviction, and any number of
  layout changes. Losing cached pages is an inconvenience; losing the
  reader's place is not.

## 6. Design sketch

### 6.1 Long-filename write support

Add to the driver (fork or upstream contribution, respecting the pin):

- create a file/directory with a long name: write the LFN entry chain in
  reverse sequence order, each entry carrying the checksum byte of its
  short alias, followed by the 8.3 entry;
- generate a unique short alias — numeric `~N` suffixes are cheap only
  while the directory is small, and R13's folders keep directories small,
  but a hash-derived alias avoids the O(directory) probe entirely and is
  what the existing `proto::upload::sanitized_name` already computes;
- delete a long-named file by clearing its chain and its short entry (R5);
- rename within a directory (needed by §6.2), which for a long-named file
  means rewriting the chain.

Risk: this writes directory-entry structures. A bug corrupts directories,
not just our files. It wants its own fault-injection tests against a
simulated block device before it touches a real card.

### 6.2 Upload staging

1. Create the destination folder if needed.
2. Create the staged file **in the destination folder** under a name the
   library scan ignores (extension outside the accepted set).
3. Stream, verifying declared length and SHA-256 as bytes arrive.
4. Reread the persisted file and confirm identity independently.
5. Rename to the final long filename.
6. Publish the metadata record naming the final path.

The two crash windows are both benign, which is what makes this simpler
than the reserved-slot staging it replaces: before step 5 an ignorable
staged file remains, for cleanup; after step 5 but before step 6 the card
holds an ordinary, readable, correctly-named book with no record — which
is exactly the sideloaded case that must work anyway (goal 3).

### 6.3 Records

Source metadata keeps its durable A/B commit protocol unchanged. What
changes is how a record points at its book: the fixed 12-byte 8.3
`unmanaged_name` becomes a path locator, and lookups key on content
identity (R6). The existing bare-filename field cannot express folders and
would treat `/SciFi/BOOK1.EPU` and `/Fantasy/BOOK1.EPU` as one book.

This is where the milestone touches the image-rendering PRD's M0S work, and
the touch is narrow: the record protocol, idempotency, receipts, container
gate, and validation rules are unaffected.

### 6.4 What a settings change actually rebuilds

`CONT.BIN` already captures the parsed content stream — the `push_block`
sequence of `(spine_index, text, role, style, align, paragraph_end)`, all
content-derived — and a type-settings change replays it rather than
re-reading the zip, inflating, and parsing XML. So the expensive half of
R17 holds today, and `CACHE3`'s placement of `CONT.BIN` under `COMMON/` is
correct. What a settings change legitimately rebuilds is line breaking and
page assembly, which is what a settings change *is*.

This is also why the anchor cannot be a block index. `BlockRecord` carries
`line_count` and is produced by replaying the content stream through the
layout engine, so it is layout-derived and no more stable than a page
number. The stable unit is the position in the `CONT.BIN` stream itself.

The anchor costs nothing on the reading path: it is written at page turn
as an ordinal the layout already holds, and resolved only during a layout
pass that a settings change performs regardless.

### 6.5 Cache and position identity

`CACHE3`'s two-level `<hash-8>/<size-8>` directory split is kept — it also
bounds directory size, which §4.5 makes a performance requirement and not
only a tidiness one — but the components become content-derived: a prefix
of the source SHA-256 plus the exact length, both already computed for R6.
Directory names stay within 8.3, which the device must satisfy to create
them.

Reading position becomes `(spine index, block index, offset within block)`
or the narrowest anchor the layout engine can resolve back to a page. The
open path resolves the anchor against the current layout; the page number
stops being stored at all. This is the change that fixes the settings
bug, and `POS3`'s separation from the cache is what keeps it durable
across eviction.

### 6.6 Library scan

Per-directory catalogs rather than one flat file, so entering a folder
reads only that folder's records and cached titles (R13, R14).

## 7. Acceptance

- A book uploaded to a chosen folder appears on a computer under its real
  name, in that folder.
- Moving that file to another folder on a computer, then remounting,
  leaves the book readable with its position intact.
- Renaming it on a computer does the same.
- Deleting a long-named book from the device leaves no orphan directory
  entries (`fsck`/`chkdsk` clean).
- Power loss at every step of §6.2 yields either no book or a complete
  readable book, verified by the abrupt-reset campaign harness
  (`tools/powercut_campaign.py`) extended to this path.
- Changing type settings on an open book leaves the reader on the same
  text, not the same page number, and does not rebuild the parsed spine,
  TOC, or cover.
- A 1000-book library browses with resident memory bounded by folder size.
- No watchdog or responsiveness limit is violated; no request blocks on a
  whole-library operation.

## 8. Measurement gates

- **Upload throughput.** Currently ~88 KB/s end to end (§4.6), which sets
  how long filling a library takes. Cause unknown; measure before
  optimizing, and before assuming uploading is a viable way to move a large
  library onto a card.
- **Alias generation cost** in a large directory, if `~N` probing is chosen
  over hashing.
- **Per-directory catalog read cost** at the folder sizes a 1000-book
  library actually produces.

## 9. Open questions

1. Anchor granularity for R18: is a block index sufficient, or does the
   anchor need a character offset within the block to land the reader on
   the same sentence rather than the same paragraph?
2. What happens to a book whose file the user deletes from a computer — is
   the record reclaimed automatically, or does the book show as unavailable
   until the user acts?
3. Does the browser pick from existing folders only, or may it create new
   ones at upload time?
4. Should sideloaded books receive records at all in this milestone, or
   only when a later milestone needs to persist something derived from them?
