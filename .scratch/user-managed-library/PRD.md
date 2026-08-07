# User-Managed Library — PRD

Status: **Draft for review.** Requirements below marked *(stated)* come from
the product owner; those marked *(inferred)* are the author's reading and
need confirmation. Facts marked *(verified)* were read from the pinned
dependency or measured on hardware and are cited.

Scope is deliberately independent of the on-device image-rendering PRD.
Nothing here depends on render bundles, and nothing here blocks them; the
two meet only at the point where a book has an identity.

This document **absorbs the `cache3-architecture` draft** (commit
`49b2c0a`), which is superseded. The two cannot be delivered separately:
they share one identity contract, they rewrite the same on-disk layout,
and the headline acceptance criterion — move a book from a computer and
keep your place — cannot be demonstrated by either alone. Splitting them
would also mean writing the reading-position format twice and breaking
card compatibility twice.

---

## 1. Problem

The device can read a library the user organizes, but it cannot *write*
into one. Books uploaded over the wireless hotspot land in a single
directory under machine-generated 8.3 names — `8620JLY1.EPU` — because the
SD driver can only create short filenames. The device shows a real title
(read from the EPUB), so the defect is invisible on the device and total on
a computer: a card of uploaded books cannot be organized by hand, because
the files cannot be identified by name.

Reading long filenames already works, so the gap is narrow and specific.

Two further defects share the same root — derived state keyed to where a
book *is* rather than what it *is* — and are in scope because fixing them
separately would mean writing the same formats twice:

- Reorganizing the card from a computer discards a book's cache and its
  reading position, because both are keyed by a hash of its path.
- Changing type settings moves the reader's place, because the position is
  stored as a page index and a settings change reflows the text. It also
  rebuilds pagination for a configuration the reader may switch straight
  back to.

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

These are properties of the `embedded-sdmmc` revision *this repository
currently pins* (`d26892f`) unless noted — see §4.7, which supersedes
several of them.

On the pin comment: it warns that newer upstream SD rewrites "fail cold
card init on the X4." That observation is **inherited from upstream**
(`Jon-Vii/xteink-x4-os`, whose device the X4 is) and was never reproduced
here; this project develops on the X3 and has no X4 hardware. Treat it as
upstream's finding on upstream's board, not as a local measurement.

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
7. **Long-name creation already exists upstream, and we have not synced
   it.** *(verified)* Upstream commit `7f9856d`, "Write wireless uploads as
   EPUB long names", replaces the dependency with a fork
   (`Jon-Vii/embedded-sdmmc-rs`, rev `329b3a5`) described as "based exactly
   on the proven #210 revision, adding allocation-free VFAT long-name
   creation/deletion" — the same revision we pin, so it avoids the rewrites
   the pin comment warns about. It adds `create_file_in_dir_lfn`, a
   deterministic probeable short-alias generator
   (`proto::upload::upload_short_alias`), a long-name sanitizer
   (`wireless_epub_filename`), and LFN-aware deletion, which is constraint
   4's defect. It does **not** add rename. Our `fw` has diverged 87 commits
   since, so this is a port rather than a cherry-pick — but the driver fork
   is reusable as-is.
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

### 5.5 Derived state: cache and reading position

Absorbed from the `CACHE3 + POS3` draft, with identity corrected per R6.

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
- R20. Two type-setting configurations coexist on card, so alternating
  between them does not rebuild pagination each time.
- R21. A cache generation is valid only once fully written. Interruption
  leaves it ignorable and reclaimable, never partially loadable.
- R22. Cache directories carry a book's full identity in their path, so
  distinct books cannot collide on a truncated key.
- R23. Superseded cache layouts are removed on a best-effort background
  sweep. No in-place migration is attempted; rebuilding is cheap and
  correct, migrating is neither.

## 6. Design sketch

### 6.1 Long-filename write support

**Port upstream's implementation rather than writing one** (§4.7). It
already covers R2, R3, and R5 — creation, deterministic alias generation
with a uniqueness probe, and LFN-aware deletion — and is built on the same
driver revision we pin, so it does not walk into the cold-init question.

Remaining work is the port itself: our `fw` has moved 87 commits since,
and the upload path in particular was rewritten. Verify on the X3, which
is the only hardware this project has.

Not provided upstream, and therefore still open: **rename**. §6.2 avoids
needing it.

Residual risk is much lower than writing this from scratch, but not zero —
it still writes directory-entry structures, and a bug corrupts directories
rather than only our files. The fork carries upstream's own testing; the
acceptance criterion about leaving no orphan entries (§7) is what proves
it here.

### 6.2 Upload staging

No rename exists (§4.3, §4.7), so staging cannot mean "write under a
temporary name and rename on commit." It does not need to: the durable
**staging marker** already carries exactly this meaning — a committed
marker with no matching committed metadata keeps its candidate hidden —
and that machinery is already implemented and power-cut validated.

1. Create the destination folder if needed.
2. Publish the staging marker naming the intended path.
3. Create the file **under its final long name** and stream, verifying
   declared length and SHA-256 as bytes arrive.
4. Reread the persisted file and confirm identity independently.
5. Publish the metadata record; the marker's candidate becomes the book.

The library scan must consult the marker so a candidate is hidden while it
is staging — the one new coupling this introduces. Crash windows: before
step 5 the marker explains the file, which stays hidden and is reclaimable
by cleanup; after step 5 the book is live. A user who pulls the card
mid-upload sees a truncated `.epub` on their computer, which is what an
interrupted copy looks like anywhere.

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

### 6.6 Derived-state layout

From the absorbed draft, with `<identity>` now content-derived (R16)
rather than `FNV(display_path, size)`:

```text
/XTEINK/
  CACHE3/<identity>/
    COMMON/            layout-independent: TOC.BIN, COVER.BIN, CONT.BIN
    SLOT0/ SLOT1/      one per type-setting configuration (R20)
      BOOK.BIN         written last; the slot's commit record (R21)
      SECTIONS/S<nnn>.BIN
    RECENTA.BIN RECENTB.BIN   advisory recency for eviction
  POS3/<identity>/
    POSA.BIN POSB.BIN  reading position, isolated from cache (R19)
```

Publication empties the victim slot, writes sections, then writes
`BOOK.BIN` last, so an interrupted publication leaves a slot that fails
validation and is reclaimed rather than half-loaded. Recency is advisory:
losing it costs a wrong eviction choice, never correctness.

`<identity>` is split across two directory levels so no single directory
accumulates an entry per book — the same reason §4.5 makes folders a
performance property. The components come from the content identity R6
already computes.

### 6.7 Library scan

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
- Alternating between two type-setting configurations does not rebuild
  pagination on each switch.
- A cache publication interrupted at any point leaves no slot that loads
  partially; the next open reclaims it.
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

1. What happens to a book whose file the user deletes from a computer — is
   the record reclaimed automatically, or does the book show as unavailable
   until the user acts?
2. Does the browser pick from existing folders only, or may it create new
   ones at upload time?
3. Should sideloaded books receive records at all in this milestone, or
   only when a later milestone needs to persist something derived from them?
4. Sequencing: the upstream long-name port (§6.1) is self-contained and
   delivers a visible improvement on its own — uploads stop being cryptic —
   without any of the identity or cache work. Land it first as its own
   slice?
5. Is there anything else unsynced from upstream that this PRD would
   otherwise reinvent? `7f9856d` was found only by tracing a comment.
