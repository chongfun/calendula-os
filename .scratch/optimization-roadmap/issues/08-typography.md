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

Note the retired lever: `TEXT_RENDER_THRESHOLD` (default 128) no longer does
anything on the shipped faces, because the mono path has no grey to threshold.
Only `generate_mockup_fonts.py` still uses it.

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
per book.

## Do not re-propose

- **2 bpp / grayscale text.** The second panel plane is the previous-frame
  buffer for differential update, not a grey channel — `update_control_1` for
  Full/FastClean is `[0x40, 0x00]`, the RED-bypass bit. Grey would need a
  multi-pass waveform, so it multiplies the 379 ms BUSY rather than adding to
  it. This was considered and rejected on that basis, not on effort.
- **Tuning `TEXT_RENDER_THRESHOLD`.** Superseded by H1: there is no grey left
  to threshold on the shipped faces.
- **Judging text quality from emulator PNGs alone.** They are the right oracle
  for *layout* — that nothing moved — and the wrong one for *rendering*, which
  is a question about a reflective display under real light.
