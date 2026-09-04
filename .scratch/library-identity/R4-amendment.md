# R4 amendment for .scratch/library-identity/PRD.md

Apply on the `optimization-roadmap` branch, where the PRD lives. It records
the two decisions taken on 2026-09-03, so the requirement and the
implementation say the same thing.

## R4, after the existing "For a filesystem change performed outside
## Calendula..." list, add:

A copy the library adopted without reading it has no recorded bytes, and a
same-sized replacement at its own locator is invisible to the scan's cheap
filter. Such a copy therefore takes its source identity from the bytes read
at its own locator while that locator appeared unchanged, which may be a
replacement rather than the bytes it was adopted with. This is accepted
rather than defended against:

- no later observation can distinguish the two, since nothing recorded what
  the original bytes were;
- defending against it means reading every book as the scan adopts it, which
  costs hours on a full card for a move that may never happen;
- the derived caches already resume the old copy's place at a same-sized
  replacement, so the rule makes an existing local false positive durable
  across a later move rather than introducing one.

A copy that arrived through a Calendula transaction is outside this rule: its
bytes were read as it landed, and its source identity is that reading.

## Move tests, amend "move plus reboot preserves position" to read:

- move plus reboot preserves position, except where the card refuses the
  write that carries it. While reading positions are addressed by place, a
  repaired locator has to copy the position between two cache directories,
  and a card that refuses that write costs the copy its place rather than
  failing the scan, since failing it would let an unwritable cache stop the
  library being rebuilt. The exception ends with the position format
  migration, after which a repaired locator preserves the position with
  nothing to copy.
