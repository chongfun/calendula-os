# WS-G: App state & render invalidation

**New workstream, 2026-07-30.** The roadmap's top-ranked item ("single repaint
per page turn") was filed under no workstream because this region had no
owner. It is where the largest remaining user-facing wins are, so it gets a
file.

The economics here are unlike anywhere else in the tree. **One avoided panel
refresh is worth ~405 ms.** The entire software layout budget is 13 ms. So the
ceiling on any CPU optimization in `ui/` is ~3% of a single refresh, and the
only thing in this region worth ranking is **a render that happens and should
not**. Do not bring CPU micro-optimizations here.

Owns: `app-core/` (reducer, `storage_loop`, `RefreshPlanner`, event plumbing),
`ui/` (screen drawing), and `fw/src/tasks/app.rs` as the seam that drives them.
Coordinates with: WS-A, which owns the flush seam where A14's guard lives.

## Open

Order: G1 → G2 (with A14) → G3 → G4 → G5.

### G1 (S): strip the debug fields that defeat request equality

`RenderRequest` carries `aux_raw`, `nav_raw`, `page_raw` and `battery_mv`
(`app-core/src/lib.rs:322-326`), set on every ADC sample and copied into every
request. **No renderer reads any of them.** The only consumer of the group is
`draw_input_sample` in `fw/src/views.rs`, which reads `last_button` alone and
sits behind `const SHOW_INPUT_DEBUG: bool = false`.

Worth 0 ms by itself — it is an **enabler**, and that is why it goes first.
The cheapest form of every suppression below is
`next.render_request(kind) == last_request`, and these four fields change on
essentially every sample, so that comparison can never be true today. Remove
them and `RenderRequest` equality means "same pixels" for every view except
the built-in demo book. Keep `last_button`, or make the demo view stop drawing
it.

- Effort S. Risk none. Confidence high.

### G2 (M): the `Loaded` repaint — the tracked #1 item, with its predicate

Every page change in Reading issues an `ExtendSection`
(`app-core/src/lib.rs:799-801` — *every* change, not only section crossings).
Every extend answers with `LibraryEvent::Loaded`. The `Loaded` arm sets
`self.dirty = Rect::FULL` unconditionally whenever the book id matches
(`:2151-2167`), and `library_event_affects_view` returns
`state.book_id == book_id` for `Loaded` with no check that anything changed
(`fw/src/tasks/app.rs:520-527`). **Two full panel refreshes per page turn, in
every book, cached or not.**

The neighbouring arms in that same function already do it correctly, which is
the tell: `ChapterPage` compares the stored page and renders only on a
difference, and `CacheCleared` carries the comment *"an abandoned clear's
answer changes nothing on screen, so it must not cost a panel refresh
either."* The `Loaded` arm is the one place that skipped the discipline the
file had already established.

**State the value precisely — the roadmap currently overstates it.** This does
*not* take 405 ms off press-to-settled: the reader sees the page after the
first render, so an isolated deliberate turn stays ~424 ms. What it removes is
the ~405 ms of panel time *after* the page is already readable. That shows up
as queueing delay at any reading pace faster than ~0.9 s/page (a device
capture during a build measured 31 renders against 16 inputs), as roughly half
the panel energy per turn, and as half the refresh count for panel wear.
Calling it "405 ms off every page turn" repeats exactly the bracketing error
that promoted A7.

**Safe predicate:** render iff `pages.max(1) != state.sd_page_count`, or
`current_chapter != state.chapter`, or `position` (after the clamp) differs
from `state.page`. For an `ExtendSection`, `OpenSequence` sets
`resumable = false` so `position` is `None`; `chapter_pages` is not in
`RenderRequest` at all.

**The trap, and it is the one this codebase has already been bitten by six
times:** apply the event either way — suppress only the *render*. If the event
itself is suppressed, the reducer never learns `sd_page_count` grew, and the
reducer clamps the reader to the advertised count, so `Next` stops at a stale
frontier permanently. That is the same one-way trap as method rule 4.

Two further sites the same predicate covers, both currently unconditional:
`background_announce(finished, …)` (on the Reading view the footer reads
chapter position from the store and the page count from the plan, so a finish
announcement mid-book changes no drawn pixel), and the `LoadChapters` /
`JumpChapter` `Loaded`s.

**The branch implements this at the wrong layer and has a reachable defect.
Do not land `opt/single-repaint-per-page-turn` as written.**

`announce_is_owed(extend, read_from_card, position)` gates the *send* of
`LibraryEvent::Loaded` from the display task. But `Loaded` is the only thing
that raises the app's `sd_page_count`, and the app clamps `Next` to
`sd_page_count - 1` — and when clamped, `next.page == self.page`, so
`storage_command_for_transition` returns `None` and **no extend is issued at
all**. There is no self-healing request. The build's catch-up channel cannot
cover it either, because `background_announce` is evaluated against the
*store's* advertised count, not the app's, and the branch leaves that call
site untouched.

Reachable sequence — the ordinary first open of a new book, i.e. exactly what
B4 made the normal case:

1. `publish_first_open` publishes an index spanning the sections written so
   far; the open announces `pages = k`.
2. The reader walks pages 0…k−1. Every turn is a RAM hit, so every closing
   `Loaded` is suppressed and the app's count stays at `k`.
3. The background build grows the store well past `k` and stays silent,
   because `reader_page + 1 >= advertised_before` is false.
4. At page k−1 the reader hits the clamp. Next does nothing — no state
   change, no render, no command — and nothing can rescue it until the walk
   reports `finished`. On a large book that is minutes; if the walk ends
   `Abandoned` or `Stopped`, indefinitely.

On `main` this cannot happen: every RAM-hit extend re-announced the store's
count, so the app's ceiling tracked the build with at most one turn of lag.
The branch removes that resync and puts nothing in its place. The commit
message dismisses the interaction as *"Not a B4 behaviour: this is the
ordinary reading path"* — that is the one wrong claim in an otherwise careful
pair of commits, and it is load-bearing.

**This is method rule 4 through a different door,** and it is why the
predicate above insists on suppressing the *render* while still applying the
event. Fix: add a fourth term — skip only when the extend read nothing *and*
the store's advertised page and chapter counts equal what the app was last
told. That needs the storage task to remember the last announced count (one
`u32` each for pages and chapters), and it makes the predicate host-testable
in the same style as the nine tests the branch already adds. No host test
currently models a growing page count; that is the test to write first.

**Note that A14's flush-seam guard is structurally immune to this defect,**
because it sits below the reducer: the event is delivered, `sd_page_count`
updates, the frame is drawn, and only the *flush* is skipped. It pays 13 ms
of layout per suppression to save the 405 ms flush. That is a good reason to
land A14 as the general backstop even after G2 is fixed — the two are
complementary, and A14 fails safe where G2 fails dangerous.

### G3 (S–M): move the render gate into `app-core` so it can be tested

`library_event_affects_view`, `library_event_allows_first_render`,
`fold_library_event` and `first_render_kind` are pure, already sans-IO
(`&ReaderState` + `&LibraryEvent` → `bool`) — and they live in `fw`, where
`grep -rn '#\[cfg(test)\]' fw/src/` returns **no matches at all**, against
~163 host tests in `app-core`.

This is the same structural gap that produced six review rounds on B4 and was
closed there by extracting `reader-cache`. Every suppression in this file
edits exactly these functions. Move them first, then tighten them with
regression tests, rather than arguing each predicate in review.

- Effort M, mechanical. Risk low. This is the precondition for G4.

### G4 (S each, free if A14 lands): presses that provably change nothing still spend a refresh

Seven arms return unchanged state and then render anyway via
`fw/src/tasks/app.rs:277-283`:

| Site | `app-core/src/lib.rs` | Note |
|---|---|---|
| Library `Busy` swallows every press but Back/Power | `:1885-1890` | repaints an unchanged "clearing…" screen |
| Wireless + `Next`/`PageNext` | `:2045` | an explicitly empty arm |
| Wireless + `Confirm` during an in-flight session | `:2021` | `_ => {}` |
| Wireless + `Previous` when status ≠ `Idle` | `:2038-2044` | |
| Library `Confirm`/`PagePrevious`, selection out of range | `:1911-1919` | |
| Reading + `Previous` at page 0 | `:1952-1958` | mirror of the last-page case |
| Any wrap in a one-item list | `:2501-2507` | `wrap_next(0, 1) == 0` |

The known last-page case is one member of this class: `StableButton`
auto-repeats a held browse key every `REPEAT_COOLDOWN_TICKS = 32` × 15 ms
≈ 480 ms, which is how a device capture accumulated **62 consecutive
identical full renders of `page=561`**, each burning a 379 ms refresh.

Frequency is unmeasured — no capture exists for how often a user lands on the
Wireless dead keys or presses during a cache clear. A14's `identical=<bool>`
counter measures all of them at once.

### G5 (S): the loading plate re-flushes a frame the app just rendered

The plate request is `refresh_planner.last_request()` with view/book/page
overridden (`fw/src/tasks/display.rs:1093-1101`), and on every transition
reaching that arm those overrides already equal the app's own request — whose
render lands on the same plate branch, because the plate's "not covered"
condition is nearly the same predicate as `views.rs`'s `reading_buffer_ready`.
Two byte-identical bookplate flushes.

The comment justifying the plate says "the storage receiver can win that
race". It does not — `select5` polls `DISPLAY_COMMANDS` first — and more to
the point **it does not matter who wins**, because in either order both frames
are the same plate, so one is always redundant.

- ~405 ms per book open, per type-settings or orientation change entering
  Reading, per Contents exit. Free if A14 lands; otherwise return `None` when
  the constructed request equals `last_request`.
- Related, ranked lower because it trades a real UX property: **Contents exit**
  renders an immediate content-free bookplate and then the real page, two
  frames per press. Entry already suppresses its optimistic render and lets
  `Loaded` paint once (`app.rs:192-197`); exit could copy that, gated on
  `outcome == StorageDispatch::Sent`. Check first that the extend's service
  time is short — if it is a real card read rather than a RAM hit, the plate
  is earning its keep and suppressing it is a regression, not a win.

## Do not re-propose

- **Shrinking a dirty rect, or any partial-refresh idea.** `RenderRequest.dirty`
  and `ReaderState.dirty` have **13 assignment sites and zero reads** anywhere
  in `ui/`, `fw/` or `display/`. This region has no dirty-rect bookkeeping
  despite appearing to. Worse, the field's presence invites exactly the fix
  that partial-window panel refresh has permanently ruled out. Delete the
  field or leave it; do not build on it. The win is in *skipping* a refresh,
  never in shrinking one.
- **Caching or hoisting layout work in `ui/src/reading.rs`.** Measured false
  once already (2.8 µs pagination against 194 µs drawing). `draw_reading_page_body`
  walks only `page.block_count` blocks; no pagination happens at draw time.
- **"Fixing" `catalog_refresh_requested`** (`fw/src/tasks/app.rs:33`). It is
  initialised `true` and never cleared, so the post-`Settled` catalog refresh
  is unreachable. **Leave it dead.** Making it work would queue a
  `LoadCatalogCache` after every render, each emitting `CustomFont` + `Scanned`
  and, on a miss, a `RefreshCatalog` that emits both again.
- **`LibraryEvent::ChapterPage`** — no firmware code constructs one; the sole
  constructor is the emulator's scenario runner. Dead on device, and could not
  move a pixel anyway since `sd_chapter_pages` is absent from `RenderRequest`.
- **Hoisting `views::render`'s 256-entry chapters array into a static.** A net
  loss — see WS-E: `_stack_end` is exactly the end of `.bss`, so a 4,224 B
  static costs 4,224 B of stack while the peak chain (in the EPUB build) is
  untouched. Only shrink it *in place*, by windowing the list the way Library
  already does, and only if it is ever worth touching.
- **Redundant progress writes** — checked; `previous_persisted != next_persisted`
  fires only on real changes and same-context writes coalesce behind
  `PROGRESS_WRITE_MIN_SECS`. No stray 41 ms writes exist.
- **Boot double-render** — checked; `Restored` consumes the first-render
  one-shot and the following `Scanned` fails the view gate. Boot is one paint.

## Measured, for anyone sizing work here

Host-measured struct sizes: `ReaderState` 372 B, `RenderRequest` 104 B,
`LibraryEvent` 276 B, `StorageCommand` 100 B, `Rect` 8 B.

`render_shell` clears the framebuffer and then every view clears it again —
two `fill(0xFF)` passes over 48,000 B (X4) / 52,272 B (X3) per shell render,
~0.1–0.3 ms. That is 1–2% of layout and 0.05% of a refresh: a tidiness fix,
not a performance one. Recorded so nobody re-derives it as a finding.
