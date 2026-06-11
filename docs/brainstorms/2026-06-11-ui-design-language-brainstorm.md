---
date: 2026-06-11
topic: ui-design-language
---

# UI/UX Refactor — Design Language Exploration

## What We're Exploring

A blank-slate rethink of the whole UI/UX (shell screens AND reading experience)
around a deliberate design language. Four complete directions were mocked up
through the real 1-bit framebuffer pipeline, four screens each (home, library,
reading, in-book overlay):

```
PATH="$HOME/.cargo/bin:$PATH" cargo run \
  --manifest-path tools/preview/Cargo.toml \
  --target aarch64-apple-darwin -- --design-mockups
open target/previews/design/index.html
```

## The Four Directions

### Zen Minimal — "Still Water" (stated lead candidate)
One thing per screen. No boxes, no resident chrome; type floats in whitespace.
Type scale does all hierarchy work (46px display / 22px body / 16px whisper).
Chrome is summoned (Confirm raises a bottom band), never resident. Progress is
a hairline with a 3px head. Navigation is three whisper-words above the
physical buttons.

### The Shelf — book-forward
Covers are the heroes. Home is the book you left open (big cover + continue
card + "on the shelf" mini covers). Library is a cover grid with shadowed
selection. Reading footer is a chapter-ticked progress rail. Overlay is a
bookmark-ribbon contents panel sliding from the right.

### Folio — editorial print
The UI is typeset like a fine book: letterspaced small caps, hairline +
double rules, dot leaders in the library index, folios ("– 142 –"), drop caps
on chapter openings, numbered footnote-style menus. No fills, only rules.

### Cockpit — instrument panel
Honest hardware. Inverse status bar, scaled 5x7 terminal type for all chrome
(Literata reserved for book text), master progress gauge with ticks, reading
stats + pages/day bar chart, tabular library with inverted selection row,
physical-button legend strip.

### Imprint — the blend (added after review; Claude's recommendation)
Folio's typographic logic everywhere, Zen's restraint inside the book. No
masthead, no running heads. Home keeps Zen's 46px title but speaks in Folio's
voice (italic "now reading" / "by …", colophon line, contents-style nav,
printer's-mark footer). Library is Folio's index page with a quieter heading.
Reading page is bare except a centered folio "– 142 –". Summoned overlay is a
footnote: short separator rule, apparatus line, numbered menu entries.
Rationale: "the UI is a book" is a generative rule that answers future design
questions; Zen wins only the page you stare at for hours, which Imprint keeps.

## Physical Button Reality (user-confirmed)

- Four-button ladder on the **left short side**, a vertical column
  (top→bottom: Back, Confirm, Prev, Next per `FrontLayout::BackConfirmLeftRight`).
- Two dedicated **page keys on the long side**, under the reading grip
  (`page_pin` ladder, Up/Down → Prev/Next).
- Auto-repeat on direction keys at ~480ms cooldown ≈ the 473ms fast refresh.
- **No long-press detection** in the input task; all designs must be
  press-only unless the driver grows long-press support.
- Implication: soft-key labels belong stacked on the LEFT edge beside the
  buttons (cf. the left action rail in the earlier dock-home exploration),
  not in bottom rows. With dedicated page keys, front Prev/Next are free
  while reading — natural map: chapter back / chapter ahead.

## Two Interaction Models (Imprint mocked in both)

- **A — cursor** (`imprint-*`): layout follows content; Prev/Next move an
  underline cursor, Confirm activates, Back dismisses. Consistent grammar,
  but every cursor move costs a 473ms repaint before you ever activate.
- **B — direct map / "marginalia"** (`imprintdm-*`): the left margin carries
  marginal notes aligned beside the four physical buttons — one label, one
  button, no cursor, zero repaints to navigate. While reading the margin
  sleeps to four small ticks; any front button wakes it. Costs: max four
  targets per screen, per-screen relabeling erodes "Back always means back",
  and lists (library) still need a cursor, so B is really
  direct-map-for-commands + cursor-for-content.

## Imprint B v1 — Consolidated Rules (after marginalia + bracket studies)

- **Margin grammar (final)**: a typeset em-dash faces each physical
  button — the same dash family as the folios — with letterspaced
  small-caps labels (16px, +2 tracking); the screen's one primary
  action is bold caps. Rationale (user call, confirmed): caps are
  apparatus type, so controls declare themselves; italic stays
  reserved for the book's voice. Sleeping margin (while reading) =
  dashes only, no words. Body-italic labels superseded (altstudy-1).
- **Three-voice type system**: upright body = content; italic = the
  book's voice (authors, whispers, quotes); letterspaced small caps =
  the device's voice (screen headings AND controls); 16px small
  regular = metadata apparatus (colophons, folios, meta lines).
- **Home is set on the rail's grid**: everything left-aligned to one
  column (x=210), apparatus bottom-right like every working screen;
  the centered title-page home survives as altstudy-2.
- **Key grammar (revised on device, user call): Back zooms out, OK
  affirms.** The top key is the hardware Back key and always retreats
  one level (home→library is "out of the book, onto the shelf");
  the second key is Confirm/OK and always carries the screen's primary
  action in bold caps. Slots 3-4 = paired browse/secondary.
  Home: library / **continue** / sync / settings. Library: home /
  **open**. Contents: close / **open**. Settings: home / **change**.
  This matches hardware roles and universal OK-button muscle memory;
  the earlier "top = primary" rule is superseded.
- **Contents is a true contents page**: tight 36px index rows, dot
  leaders to right-aligned book page numbers (from the store's TOC
  page targets), 9 visible rows — not a spaced menu.
- **Type palette (fixed)**: 46px display = book title on home only;
  22px body = content and key labels; 16px small = apparatus (colophon,
  folios, running meta, footnotes); letterspaced small caps = screen
  headings only; italic = "voice" (authors, marginalia, whispers).
- **Furniture vocabulary**: en-dash pairs for folios/pagination
  ("\u{2013} 142 \u{2013}", "\u{2013} 1 of 2 \u{2013}"); middots for inline
  apparatus separation; battery+time bottom-right on working screens,
  centered printer's-imprint line on the home/title page only, absent
  while reading.
- **Reading is full bleed (user call)**: text fills the firmware reader's
  real bounds (x 8..792, footer strip at 466) — the device bezel IS the
  margin; no artificial margins, no resident chrome. The only resident
  furniture is the firmware's existing page-in-chapter counter,
  "{page}/{pages-in-chapter}" right-aligned in the footer strip.
- **Summoning creates the margin**: any front button slides a 160px white
  key sheet over the page's button edge (hairline at its edge) plus the
  bottom apparatus band; closing returns the full-bleed page.
- **No clock anywhere (user call)**: the device doesn't tell time;
  apparatus shows battery percent only.
- **The index row is the universal list pattern**: upright name, dot
  leaders, italic value right-aligned at the 740 grid edge. Library
  uses it for books, settings for values, sync for status. One row
  DNA across every list screen.
- **Unused key = bare dash**: the mark stays, the word goes (sync
  uses only two keys; slots 3-4 show lone em-dashes).
- **No listening keys, no rail**: sleep is the one centered ceremonial
  screen because nothing is pressable; it shows the book's plate
  (title, author, position) and no battery — a days-old panel image
  must not show stale numbers.
- **Boot carries the masthead**: the letterspaced XTEINK X4 with the
  double rule (evicted from home) lives on the boot half-title, with a
  colophon version line ("edition 0.4 · set in Literata").
- Settings editing: CHANGE cycles the selected value in place on fast
  refresh; previous/next move the arrow. (Editing state not yet mocked.)
- Rejected treatments (kept in gallery for the record): drawn ticks,
  thumb tabs, registration rule, ruled column, pilcrows, em-dash,
  roman numerals.

## Home v2 (chosen from the iteration sheet, flashed)

- iter-5 layout + chapter-name colophon (iter-8): big title, author in
  letterspaced caps (no "now reading" label, no "by"), the hairline
  becomes the progress rule (page-based fill when page count is known),
  colophon = chapter NAME in the book's italic voice + upright
  " · page 142 of 380" apparatus; roman-numeral fallback when the book
  has no chapter titles, pages omitted when page count unknown.
- Rejected: "Chapter N · NN per cent" (verbose, mixes story-position
  and book-quantity units), lowercase italic "now reading"/"by"
  (precious), iter-6 measure as drawn (two numbers flanking a rule
  read as a RANGE 142..380 — broken metaphor; corrected you-are-here
  version kept as iter-7 for the record).
- Battery display: input task holds reported percent until the raw
  reading moves ≥2 points (ADC noise was flipping 88/89 every refresh).

## Cross-Cutting Ideas (apply to whichever direction wins)

- **Button grammar**: Prev/Next = move, Confirm = select/summon, Back =
  dismiss/up, consistent everywhere. The 473ms fast refresh makes paginated,
  deliberate screens the right model; avoid scrolly interactions.
- **Full refresh as ceremony**: the 3.5s flash is spent only on entering or
  leaving a book — a deliberate "turning to a new page" moment. In-shell and
  in-book navigation stays on fast refresh.
- **Summoned chrome**: the reading page owns the whole panel; status/menus
  appear on Confirm and vanish on Back.

## Key Decisions (so far)

- Mockups are host-side only: extra Literata sizes (16/30/46px) are generated
  into `tools/preview/src/mockup_fonts_generated.rs` by
  `tools/generate_mockup_fonts.py`. Promote chosen sizes into `display/src`
  only when a direction is picked.
- Stock 5x7 glyph table stores bit 6 as the top row; the mockup renderer
  flips bit order (see `big_ascii`).

## Open Questions

- Which direction (or blend)? e.g. Zen as the base with Folio's library
  index and Shelf's contents panel is a plausible hybrid.
- Does Home survive at all, or does the device wake straight into the book
  (pure Zen), with Library/Settings summoned from within?
- Type scale promotion: which sizes does the firmware actually need, given
  ~60KB rodata per size-style and flash budget?
- Covers: 1-bit dithered real covers vs. typeset placeholder covers (the
  mockups typeset them — it looks intentional and avoids mud).

## Next Steps

→ Pick a direction (or blend) from `target/previews/design/index.html`,
then `/workflows:plan` the implementation: promote fonts, restructure
`UiShell` into the chosen language's render model, wire emulator goldens.
