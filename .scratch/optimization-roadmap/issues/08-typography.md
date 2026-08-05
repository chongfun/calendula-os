# WS-H: Typography & text rendering quality

**New workstream, 2026-07-30.** Text quality had no owner: the rasterizer
lives in `tools/`, the tables in `display/`, and the line layout in `ui/`, so
nothing in the roadmap covered how the words actually look. That is the
product, on a device whose only job is showing words.

The economics here are the opposite of the rest of the roadmap. **Glyph
rasterization happens on the host, at font-generation time**, so quality work
there costs the device *nothing* — no runtime, no flash, no RAM. The usual
"quality versus performance" trade does not apply; the binding constraint is
cache invalidation, not speed.

Owns: `tools/fontgen_common.py` and `tools/generate_*.py`, the generated
`display/src/*_generated.rs` tables, `tools/build_font_pack.py`, and the text
layout in `ui/src/reading.rs`.
Coordination: `ui/src/reading.rs` is also listed under WS-B, and
`display/src/font.rs`'s struct under WS-E. Touch the layout half only in
coordination with WS-B.

## The constraint that shapes everything here

**Advance widths and glyph boxes are cache geometry.** `text_ink_width` uses
`advance + x_offset + width`, the wrap consumes it at cache-build time, and
`READER_LAYOUT_VERSION` gates invalidation — so a change that moves any of
them repaginates every cached book on every device, at 24–27 s per book.

A rasterization change that alters only the *pixels inside* an unchanged box
costs nothing and invalidates nothing. That distinction is the difference
between a free improvement and one that taxes every user, and it is worth
measuring before writing code, not after.

## Done

### H1 — monochrome rasterization (`opt/font-mono-raster`)

The generators antialiased to 8-bit and cut at a hard threshold of 128.
Thresholding an antialiased bitmap is the worst way to reach a bilevel grid: a
stem that lands half-covered across two pixel columns rounds to one column or
two depending only on where it falls, which is what produced asymmetric bowls
and stems that changed width partway down a stroke.

Rendering onto a `"1"`-mode canvas makes Pillow ask FreeType for its
**monochrome target**, which selects the hinting algorithm built for a bilevel
grid and applies dropout control. Per FreeType's own documentation
`FT_LOAD_MONOCHROME` alone does *not* change hinting, and neither does
thresholding after the fact — the hinter has to be told it is targeting mono.

**Pillow already wraps this, so no dependency was added.** The sibling
`rust_ink` port of the same idea (commit `b5275d7`) had to vendor FreeType
because fontdue has no monochrome mode; here it is a three-line change to one
file.

Measured before committing to it, on real Literata at the three shipped sizes:

| | 19 px | 22 px | 26 px |
|---|---|---|---|
| glyphs whose pixels change | 71% | 86% | 78% |
| total ink | +0% | +0% | +1% |
| total bitmap bytes | 0% | 0% | 0% |

So at reading sizes this is **quality-only** — no flash change either way.
`rust_ink` saw ~8% smaller bitmaps because its win was on a 14 px UI face,
where the grid is far less forgiving. Expect a larger effect from this change
on any small face, and a modest one on body text.

**Cache safety, which is the part that mattered.** Advances come from
`font.getlength()`, which is render-mode independent, so they cannot move. The
open question was the ink box. Across 8,469 glyph renders (3 Literata styles ×
19/22/26 px × the full shipped coverage) **81 renders — 37 codepoints — have
mono ink one pixel outside the box `getbbox` reported.** Every one is a
diacritic glyph, a fraction, or a guillemet: ï î ĩ Ī ī ĭ Ĵ ĥ ď ľ « ¼ ½, Greek
tonos, Cyrillic Ї. **The only affected ASCII character is `_`.**

The fix is to keep the box `getbbox` gives and let those 37 clip into it,
which the implementation does by construction — it never asks FreeType for a
box at all. Verified on the regenerated tables: **zero `GlyphMetric` lines
changed** across Literata and Merriweather. No `READER_LAYOUT_VERSION` bump,
no repagination, no cached book touched.

Goldens re-blessed deliberately on both boards: 25 of 31 scenario frames
change, by 0.03–0.25% of pixels, with no frame changing size. Inspected before
blessing per `docs/agents/visual-verification.md` — letter positions are
pixel-identical between old and new, which is the layout invariance visible
rather than merely asserted.

**Superseded by H5, 2026-08-05.** The device verdict on these tables was
mixed — some glyphs improved, others unbalanced — and both mechanisms trace
to rendering each glyph twice: the mono hinter grid-fits every glyph on its
own, so neighbours can carry different stem weights, and the re-seat that
placed the mono ink against a separately rendered antialiased reference
turned every disagreement between the two renders into a one-pixel placement
lottery. What H1 keeps is the diagnosis (a 50% cut rounds half-covered stem
edges by grid phase — that is what H5's lower cut fixes directly) and the
machinery: the specimen goldens, the fingerprint table, and the frozen-box
discipline all came from this change and are what made H5 cheap to build and
safe to verify. The lesson is size-dependent, not absolute: mono hinting won
on rust_ink's 14 px UI face, where the grid is brutal; at 19–26 px reading
sizes with frozen boxes, it lost.

### H2 — justified lines were left-heavy (`opt/font-mono-raster`)

`draw_justified_line` divided the line's slack with integer division and spent
the remainder on the *first* gaps: ten gaps with six spare pixels gave the
first six gaps an extra pixel and the last four none. On every line of every
page, which reads as a slight leftward crowding.

The remainder is now carried the way a Bresenham line carries its error, so
the wider gaps land evenly. The total is unchanged — exactly `remainder` gaps
still get the extra pixel — so the line ends where it did. Extracted as
`GapSlack` so it is testable away from the framebuffer; three host tests, and
the front-loading mutation fails the one that names it.

### H5 — one antialiased render, seated by itself (`opt/font-aa-low-threshold`)

**Ready (a45d548, unpushed). Device A/B verdict 2026-08-05: better overall.
Supersedes H1's pixels; keeps its diagnosis.**

The shape is crosspoint-reader's: every face it ships comes from a single
FreeType call, so ink, box and bearings cannot disagree with each other. Here
that is one `"L"`-mode render per glyph, cut to 1 bit at
`AA_INK_THRESHOLD = 112`, no mono render, no re-seat. The box stays the
shipped table's box — frozen cache geometry.

**The cut is measured, not copied.** crosspoint cuts at ~12.5% coverage, but
its boxes derive from the same render and grow with the ink; ours are frozen,
so that cut clips whole edge rows off reading glyphs (63k pixels, ASCII
included). A sweep of every shipped render: ink vs the mono tables runs
+25% / +14% / +8% / **+3.5%** / +0.3% at cuts 32/64/96/112/128, with clipping
collapsing to noise from 64 up. The trap was per-face: Literata's 22 px stems
— the default reading configuration — have their antialiased edges at
coverage 96..112, so any cut ≤ 96 swallows them wholesale (+20–31% ink, a
weight change). At 112 every face lands at +1–5% except the 22 px italics
(+11–15%), the direction e-ink wants anyway.

Cache safety verified the same way as H1: all 49,802 metrics byte-identical
except the dedup pool's `offset`. No repagination. Fingerprints and goldens
re-blessed after inspection; prose frames are letter-position-identical, and
the specimen pages reflow only because they enumerate distinct glyphs and the
dedup classes moved with the pixels.

**Residual to watch on device: bold dashes.** Mono's dropout control was
doing real work on thin horizontal bars: Merriweather Bold's hyphen/dash/
equals family lost exactly half its ink (em-dash 38→19 px at 19 px) because
the bar straddles two rows at ~70/30 coverage and the 30% row now drops.
Regular weights and Literata are unaffected. If it shows in reading, the fix
is a targeted rescue rule (keep a stroke's second row when dropping it would
halve the glyph), not a return to mono.

**The branch also corrects the toolchain pin.** Regenerating under the
Pillow 10.4.0 / FreeType 2.13.2 pair pinned by #66 does not reproduce the
shipped tables on this machine — 12.3.0 / 2.14.3 reproduces all five byte
for byte, which follows from history: the fingerprints were created by #61
(built on 12.3.0/2.14.3) and #66 pinned without regenerating. #66's
hinted-vs-unhinted `getlength` numbers do not reproduce here either; both
Pillow versions return identical hinted advances on this machine, so those
numbers came from some other environment — likely a raqm-enabled Pillow.
**Follow-up: run one generator unmodified on the machine that produced #66
and see what its toolchain actually resolves to.**
`tools/reseat_generated_glyphs.py` is deleted with its premise.

## Open

### H3 (S, host, needs a device photograph): stem darkening

E-ink renders thinner than the same bitmap on an LCD, and the typographic
literature on low-DPI bilevel rendering favours moderate stroke contrast for
exactly this reason. FreeType can embolden an outline slightly before
rasterization, which would thicken stems by a fraction of a pixel and let more
of them survive grid-fitting as two columns rather than one.

This is a host-side knob with no device cost, but **it cannot be judged on the
host**: the whole question is how the panel renders it, so the verdict is a
photograph of an X3 at each shipped size, against the current build. Do not
tune it against emulator PNGs.

The live lever is now `AA_INK_THRESHOLD` (112), and H5's sweep is the tuning
curve: each step down toward 96 buys roughly +1–2% ink per face until the
22 px cliff, and the per-face table in the H5 commit says exactly where each
face sits. H5 already shipped +3.5% of exactly this effect, so the remaining
H3 question is narrower than it was: does the panel want *more* than that —
and H7 (optical sizing) is the designer-drawn version of the same lever,
worth judging in the same photo session. `TEXT_RENDER_THRESHOLD` remains
retired; only `generate_mockup_fonts.py` reads it.

### H4 (M–L, needs a version bump): hyphenation

The reader justifies text (`draw_justified_wrapped_literata`) and does not
hyphenate. On a narrow measure that is the classic recipe for rivers and loose
lines, and it is plausibly a larger perceived-quality lever than glyph
rasterization — H1 and H2 both refine pixels, while this changes how the words
sit on the line.

Liang hyphenation with English patterns is roughly 25–30 KB of flash, which
the image can afford (57% of the slot used, 2.69 MB spare). The cost is that
it **changes line breaking**, so it needs a `READER_LAYOUT_VERSION` bump and
every cached book rebuilds once. That is the whole item: the algorithm is well
understood, the question is whether the typography is worth one 24–27 s replay
per book. **If that bump ever happens, H7 should ride along** — the two
changes share the one cost that makes each expensive alone.

### H6 (S, no cache cost): per-board default sizes for physical parity

The same pixel size is ~18% physically smaller on the X3 (259 PPI) than the
X4 (219 PPI), and the shipped ladder happens to encode the ratio: X4 at 22 px
and X3 at 26 px are both 7.23 physical points, X4@19 ≈ X3@22 within 2%. So
cross-board physical consistency is a per-board *default-size* constant —
shift one ladder step — not new tables. All three sizes already ship in both
builds. Rasterization itself has no PPI input: a 22 px glyph is rendered
identically whatever panel it lands on, so there is nothing else to "match."

### H7 (M, needs a version bump, pairs with H4): optical-size instances

At 259 PPI the reading sizes are physically 5.3–7.2 pt — small print — and
the pinned Literata statics are the family's *default* optical cut, drawn for
nominal text sizes. Literata is an opsz 7–72 optical-size family: instancing
at the physical point size (≈6–7 for X3, ≈7–8.5 for X4) buys larger x-height,
opened apertures and heavier hairlines from the typeface designer's own
drawings — the structural version of H3. This is the one legitimately
PPI-grounded reason for per-board tables, and since each board ships its own
binary it costs no flash on any device. Instancing moves advances, so it is a
`READER_LAYOUT_VERSION` bump — pointless to pay alone, natural alongside H4.
`fonttools.instancer` does the cutting; crosspoint's `build-sd-fonts.py` has
the working pattern.

## Do not re-propose

- **2 bpp / grayscale text — the impossibility half is dead, the cost half
  stands.** The original rejection reasoned from the stock waveform (second
  plane = previous-frame differential buffer). crosspoint-reader ships
  4-level gray text on this exact controller: community gray LUTs for the
  UC8253 X3 (`freeink-sdk` `Uc8253X3Luts.h`, plus a scrub bank for the
  residue), two bitplanes rendered strip-wise after the BW turn, gray pass
  displayed as an overlay. Our X3 already uploads LUTs per flush, so nothing
  hardware-side forbids it. What stands is the cost: an extra gray render and
  waveform pass per page, ghost-cleanup cadence pressure (their #2190), and
  RAM for strip scratch. If text AA ever matters more than page-turn latency,
  this is a costed design with working prior art, not a wall — but it is a
  large project, and H5 just moved the 1-bpp ceiling up for free.
- **The two-render re-seat.** Rendering mono ink and seating it against a
  separately rendered antialiased reference turns every disagreement between
  the renders into a one-pixel placement lottery, dumped into whichever edge
  the clamp leaves free. Ink and seat must come from one rasterization. This
  is the defect the device caught as "some glyphs improved, others
  unbalanced."
- **Mono-target hinting at reading sizes.** Grid-fitting each glyph
  independently gives neighbouring letters different stem weights; at
  19–26 px with frozen boxes it lost the A/B to a single antialiased render
  with a swept cut. The caveat that keeps this from being absolute: it won on
  rust_ink's 14 px UI face. If a face ever ships below ~16 px, re-measure.
- **Re-tuning `AA_INK_THRESHOLD` by eye.** The cut is data-pinned: H5's sweep
  maps ink and clipping per face per cut, and the 22 px cliff means small
  moves have wildly uneven per-face effects. Move it only with the sweep and
  clip analysis rerun, never as a lone constant tweak.
- **Judging text quality from emulator PNGs alone.** They are the right oracle
  for *layout* — that nothing moved — and the wrong one for *rendering*, which
  is a question about a reflective display under real light.
