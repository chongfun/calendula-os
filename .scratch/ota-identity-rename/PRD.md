# Drop the MarigoldOS lineage from the firmware identity

Status: implemented on `feature/ota-identity-rename` (2026-08-07), with one
scope change recorded below.

## Problem

Every build stamps `CalendulaOS <board> u<updater-generation> (MarigoldOS)` into
its app descriptor. The `(MarigoldOS)` suffix records a fork lineage the project
has since diverged from, and a new board should not inherit it.

The forcing function is the Sticky. Adding a Sticky identity in a new format
while X3/X4 keep the old one would leave `parse_identity` returning `None` for
exactly one board — it requires the suffix — and that function is about to gain a
runtime caller in `board-identity-guard`. So the format has to be settled for all
boards at once rather than diverging per board.

## Decision (2026-08-06)

Rename all three, bumping the two existing boards' updater generation:

| Board | Before | After |
|---|---|---|
| X4 | `CalendulaOS X4 u1 (MarigoldOS)` | `CalendulaOS X4 u2` |
| X3 | `CalendulaOS X3 u1 (MarigoldOS)` | `CalendulaOS X3 u2` |
| Sticky | — | `CalendulaOS Sticky u1`, **added by the Sticky milestone** |

**Scope change (2026-08-07).** `IDENTITY_STICKY` is *not* part of this
change; the Sticky milestone adds it. This document's dependency is that the
*format* be settled before a Sticky identity is chosen, and deleting
`IDENTITY_SUFFIX` settles it — shipping the constant here would have added an
API surface with no consumer to a change whose value is being a narrow,
reviewable rename. Confirmed while implementing: there is no `device-sticky`
feature and no Sticky board arm in `fw/src/main.rs`, so the arm this document
lists under Scope did not exist to modify. The rule below is instead recorded
as a doc comment on `IDENTITY_X4`, stated generally, so the Sticky milestone
inherits it without this change naming Sticky at all.

**Why bump to u2.** The digit's documented rule is that it moves "whenever the
trigger filename or the update hand-off changes." A rename produces exactly that
discontinuity: `staged_image_is_installable` is `project_name(candidate) ==
running_identity`, so pre-rename firmware refuses a renamed image outright.
Leaving the digit at `u1` would let it claim continuity across a break. The
generation is what should carry that fact, so it carries it.

**Why Sticky starts at u1, not u2.** The generation is per-board: cross-board
identities never match anyway, since the board is in the string, so the digit
only ever answers "is this anchor an older updater *for this board*." The Sticky
has no predecessor, so its first generation is 1. Do not "fix" this to u2 for
symmetry — it would mean a Sticky generation 1 that never existed.

**What this does not change.** MarigoldOS card-format interoperability is a
separate, live contract and stays exactly as it is: `proto/src/durable.rs:18`
(record layout byte-identical to MarigoldOS v0.4.x "so cards move between the two
firmwares"), `proto/src/nvm.rs:292` (shared envelope), and the position mirrors
in `fw/src/book_build.rs:706` and `fw/src/tasks/display.rs:2049`. The fork
attribution in `README.md` and `web/index.html` also stays — that is a licensing
and courtesy matter, not a descriptor string.

## Context

### Fielded impact: none

**There is no installed base — the author is the sole user** (confirmed
2026-08-06). Tags v0.3.1 through v0.5.0 exist on a public repo, so a stray
third-party flash is conceivable, but there is no fleet to migrate.

The cost of the rename is therefore *one* USB reflash of *one* X3, which is the
remedy `staged_image_is_installable` already documents: *"Moving to a new updater
generation requires replacing or re-establishing the slot-0 anchor via the
computer/OEM installation path."* Reflashing both slots retires the superseded
identity from the world entirely.

Keep a line in the release notes for the version that carries it, and update
`docs/FLASHING.md` — cheap, and correct for anyone who did pick the firmware up
from a release. But this is not a migration to manage.

### Therefore: drop the suffix from the parser outright

An earlier draft of this document proposed making `IDENTITY_SUFFIX` *optional* in
`parse_identity`, so a refusal could name which generation an anchor holds. That
was justified by diagnostic quality for fielded users. With no fielded users, and
with the one affected device reflashed on both slots as part of this change, the
superseded form stops existing — so the machinery would carry a case that does
not occur.

Delete `IDENTITY_SUFFIX` and let `parse_identity` handle
`CalendulaOS <board> u<gen>` only.

This costs nothing in refusal correctness, because **legacy handling has never
gone through the parser.** `anchor_can_apply_update` and
`staged_image_is_installable` are exact equality on the raw descriptor field; the
existing `an_anchor_predating_the_board_identity_cannot_apply_the_update` test
covers the pre-board `"CalendulaOS (MarigoldOS)"` form that way and
`parse_identity` already returns `None` for it. The superseded
`u1 (MarigoldOS)` forms get test coverage by the same route.

## Scope

### Files

- **[MODIFY]** `proto/src/ota.rs` — `IDENTITY_X4` / `IDENTITY_X3`; delete
  `IDENTITY_SUFFIX` and its `strip_suffix` in
  `parse_identity`; update the format doc comment at :661 and :701; ~15 identity
  literals across the tests, plus cases pinning the renamed forms and refusing
  the superseded ones
- **[MODIFY]** `fw/src/main.rs` — the `PROJECT_NAME` doc comment at :44 (format
  string) only; there is no Sticky arm to modify (see the scope change above)
- **[MODIFY]** `docs/FLASHING.md` — the format at :224, and a note on crossing
  the rename by USB
- **[UNCHANGED, deliberately]** `README.md`, `web/index.html` attribution;
  `proto/src/durable.rs`, `proto/src/nvm.rs`, `fw/src/book_build.rs`,
  `fw/src/tasks/display.rs` card-format compatibility

### Dependencies

- Should land **before** `reterminal-sticky-support` issue 01, so that milestone
  adds `IDENTITY_STICKY` in a settled format rather than choosing one. That
  milestone owns the constant; this one owns the format.
- Feeds `board-identity-guard`, which reads the compiled-in board through
  `parse_identity` and benefits from the legacy forms parsing.

### Notes

- The 32-byte descriptor limit is not close: the longest new identity is
  `CalendulaOS Sticky u1` at 21 bytes. The `const _: () = assert!` in
  `fw/src/main.rs` still guards it.
- Trigger filenames (`FWUPDATE.BIN` / `FWUPDX3.BIN`) are **not** changing here.
  If a Sticky trigger filename is ever added, that is its own generation bump.
- No release sequencing constraints, since there is no fleet to migrate. Land it
  whenever, ideally before Sticky milestone 01 needs the constant.

## Done when

- The two shipped identity constants read `CalendulaOS X4 u2` and
  `CalendulaOS X3 u2`, and the per-board generation rule is documented so the
  Sticky milestone can add `CalendulaOS Sticky u1` without re-deciding it.
- `parse_identity` returns the board and generation for the renamed forms, and
  still rejects foreign, truncated, and malformed names — including the
  superseded `u1 (MarigoldOS)` and pre-board forms.
- Tests pin the renamed identities and the refusal of both superseded forms
  through the exact-equality path they already use.
- `docs/FLASHING.md` documents the new format and the one-time USB reflash that
  crosses it.
- The release notes for the carrying version mention the reflash.
- The author's X3 is reflashed on both slots, retiring the superseded identity.
- MarigoldOS card-format compatibility is untouched, and a card written by this
  build still round-trips per the existing `durable`/`nvm` tests.
- `tools/check.sh all` passes for X4 and X3.
