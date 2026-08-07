# PRD: CalendulaOS optimization roadmap

Status: **WS-A reopened**; a new WS-G owns app-state render invalidation;
the bench harness has an owner for the first time. **Tier 0 is implemented**
(`opt/tier0-measurement-integrity`) and **the double repaint is merged**
(#56), so the two largest items in the round are resolved within it.
Updated 2026-07-30 after a seven-region survey, a branch audit, and a
code-reviewed implementation of Tier 0. Started 2026-07-09 from six parallel
code-survey agents, one per workstream, scoped to mostly-disjoint code regions
so work can proceed in parallel.

This document is kept to three things: **what is left**, **what has been
done**, and **what not to do**. Round-by-round history has been dropped —
it lives in the git log and the PR descriptions, which are the honest
record. What survives from it is only what still changes a decision.

## Goal

Ship the highest-ROI performance, battery, and size improvements across the
firmware and web emulator. Every item cites a measured baseline, not a guess.

## The queue

**Re-ranked 2026-07-30** after a seven-region survey and an audit of the six
in-flight branches. Three things moved the ranking more than any single new
item:

- **The measurement layer is partly broken and largely unread**, and every
  ranking in this document is downstream of it. Tier 0 exists for that reason
  and comes first.
- **"Stop optimizing the render path" was wrong**, and wrong for a specific
  reason worth remembering: the reasoning behind it was an **X4** statement
  applied to the **X3**. See WS-A's reopened status.
- **Six branches are already written and none needs a rebase** — all six sit
  on the current tip of `main` and all 15 pairs merge cleanly, so landing
  order is unconstrained by conflicts. Rank on correctness and residual work,
  not on cost-to-build. Three are much closer to done than their old queue
  positions implied; two have defects that must be fixed first.

### Tier 0 — measurement integrity (do first; hours, not days)

Nothing below this line can be honestly ranked until these land. Three rounds
of misranking — A10, A7, the retired 354 ms baseline — came from this layer.

**DONE, on `opt/tier0-measurement-integrity`** (1 commit over `main`, not
merged; rebased onto #56, clippy clean on X4/X3 × ±default-features, 56 host
tests on both interpreters). What each item turned out to be:

| # | Item | Outcome |
|---|---|---|
| 0a | `--strict` silently checked nothing on Python < 3.11 | Fixed. It now refuses to run without a TOML parser, naming the version and `pip install tomli`; non-strict warns visibly. **Re-check anything previously signed off "with `--strict`"** — on this machine it verified nothing. |
| 0b | Telemetry already captured, consumed by nothing | Fixed. `storage_build` / `storage_first_page` / `storage_background_build` are summarized, and boot-to-first-paint is reported **per boot kind**. Dead `section_extend_warn_ms` deleted, with a test that fails on any future dead key. |
| 0c | `page turn` was a function of tapping rhythm | Fixed, and it was worse than diagnosed — see below. |
| 0d | Serial logging blocks untethered | Seam built: default-on `serial-log`, inert by default. Scope is honest in the Cargo.toml comment — it does *not* yet cover the SD session's per-call lines or most of `book_build`'s chatter. The confirming device measurement is still untaken. |

**0c's root cause was the double repaint, which makes it the most instructive
result of the round.** Pairing was render-driven — each render popped a press
off a FIFO — so the second repaint of turn N consumed press N+1 and credited
it with a near-zero duration. **The #1 optimization item was corrupting the
primary metric.** Pairing is now begin-time (`t_ms − layout_ms − flush_ms`),
one duration per render from the newest press it answers, with superseded
presses counted as `coalesced`; trust gates on `(coalesced + unmatched)`,
because an unmatched-only gate is structurally blind whenever renders ≥
presses — the normal shape of these captures.

On the reference capture: **median 476 → 477 ms, p95 33,681 → 2,991 ms, min
2 → 462 ms.** The median was always right and the tails were fiction. **So the
~424 ms press-to-settled baseline stands** — it does not need retiring, which
is the opposite of what was expected when this started.

**Three things fell out of doing it:**

1. **The first real split of a build.** Of a ~63 s full build: `build spine`
   62,942 ms, `build write` 11,324 ms — section writes are ~18%, and the
   spine walk is essentially the whole build. The fixture holds no *replay*
   events, so this does not yet size the 24–27 s replay directly; but a replay
   does the same writes while skipping zip/inflate/XML, which would put writes
   near 40% of it and raise WS-B's write-alignment item.
2. **`wr_calls == wr_blocks` (18,626) and `rd_calls == rd_blocks`** on fresh
   data — D6's precondition, still holding.
3. **A live gate is miscalibrated.** `[sleep-sync] full_refresh_busy_min_ms =
   3000` against a measured X3 Full busy of **928–930 ms** — the same stale
   "~3.5 s Full" figure this round retired, sitting in an enforced budget.
   Now that `--strict` actually works, **every strict X3 sleep-sync run will
   fail on it.** Left untouched deliberately: it is a real finding about the
   budget, not about the harness.

### Tier 1 — large, cheap, high confidence

| # | Item | WS | Why it ranks here | Effort |
|---|---|---|---|---|
| 1 | **A13 — FastClean's 200 ms trailing settle** | A | Measured: `flush_ms` 686 against `busy_ms` 455, and the 204 ms tail is a `DelayMs(200)` whose only job is to precede the *next* RAM write — which already happens after `Settled`. Pure reordering. **−200 ms (−29%) on every view change, every wake, every menu step.** | S |
| 2 | **A12 — the 136 ms that is not waveform drive** | A | `busy_ms = 136.0 + 12.79 × frames` fits three modes to under 1 ms. 136 ms is **36% of every Fast BUSY** and is controller interval, not drive. Prime suspect is a CDI nibble never varied since the reference driver. **One byte, one capture; potentially ~77–100 ms off every refresh = 18–24% of a page turn.** Test before building. | S to test |
| 3 | **A14 — frame-identity guard at the flush seam** | A | **G2 shipped as #56**, so the double repaint is gone. A14 remains worth landing: `fb == prev_fb` catches an identical frame from *any* cause — the 62-refresh end-of-book case, the loading plate's duplicate flush, six no-op input sites — and being below the reducer it cannot strand the reader the way an event-layer suppression can. Measure the hit rate first (`identical=<bool>` on `bench: render`); under ~2% outside the known cases, drop it. | S–M |
| 4 | **C2** — measure sleep current with the fuel gauge, then hold GPIOs if it indicts them | C | **Unblocked 2026-07-30: the first-line experiment needs no meter and no disassembly.** The X3's BQ27220 sits on the battery and keeps integrating while the SoC is in deep sleep, so a charge-register read, a 24–72 h sleep, and a second read give average standby draw. Over 48 h, 15 µA is 0.72 mAh against 300 µA's 14.4 mAh — decisive even at 1 mAh resolution, and a null result *is* the answer. Cost is one register and one `println!`. The series meter drops to a follow-up for if it comes back high. **The "which GPIOs" half now has two named suspects from the 2026-08-06 upstream sweep, so a high reading has somewhere to go: (a) the X3 SD rail on GPIO13 — we drive that pin nowhere and so never cut the card for sleep, and freeink confirmed the pin by factory-firmware RE (`x3-sd-rail-sleep-power`); and (b) the panel RST line floating in deep sleep, closed unmerged as PR #70, which upstream reports as ~36 h-to-dead on a UC8179 while calling the SSD1677 tolerant. Neither is confirmed on our hardware and the gauge cannot separate them, but the card can be removed outright for a control run, making (a) the cheaper one to isolate.** | S |

### Tier 2 — gated on a Tier 0 measurement

| # | Item | WS | Why it ranks here | Effort |
|---|---|---|---|---|
| 5 | **Upload instrumentation, then D6** | D | The D2 post-mortem has been misread: it tested write *stalls*, never write *bandwidth*. Arithmetic says **~72% of upload wall time is single-block SD writes** and the ceiling is 1.4× above observed. D6's own precondition was met in July and it stayed deferred. ~25 lines of instrumentation resolve it in one capture. | S–M then L |
| 6 | **Cache write alignment** | B | Nothing arranges block-aligned writes, so CONT.BIN pays **2 writes + 1 read per 512 B**. Modelled at ~1.75× write amplification, cross-checked against the 2026-07-09 537-block measurement. No format change, no version bump. | S–M |
| 7 | **E5 + E6 — halve the peak stack chain** | E | `CssRules` ~6.9 KB and `parse_opf`'s duplicated manifest+spine 5,896 B, both on the peak reader chain (26,768 B of 42,136). Two mechanical changes take it to ~14.8 KB. | S–M, M |

### Tier 3 — in-flight branches, ranked by residual work

All sit on current `main`; none needs a rebase.

| Branch | State | Residual |
|---|---|---|
| `opt/tier0-measurement-integrity` | **Ready**, reviewed and reworked. Rebased onto #56; 56 host tests on both interpreters; clippy clean on X4/X3 × ±default-features. | Merge. Device confirmation of 0d's ~36 ms is still owed but gates nothing — the feature is inert by default |
| ~~`opt/font-mono-raster`~~ | **MERGED as #61, pixels superseded 2026-08-05.** The device verdict was mixed — per-glyph grid-fitting plus the two-render re-seat made some glyphs unbalanced. What survives it: the H2 justification fix, the specimen/fingerprint machinery, and the diagnosis its successor is built on (issue 08, H1/H5). | — |
| `opt/font-aa-low-threshold` | **Ready (a45d548), device A/B verdict positive** — "better overall" reading on the X3. One antialiased render per glyph, cut at a swept threshold (112); no re-seat; all 49,802 metrics byte-identical sans pool offset, so no cache invalidation. Also corrects the fontgen toolchain pin (#66's pair cannot rebuild the shipped tables; 12.3.0/2.14.3 reproduces byte-for-byte). Full sweep and residuals in issue 08 H5. | Merge; watch bold Merriweather dashes in reading (dropout residual, targeted fix known) |
| `opt/prune-orphan-sections` | **Ready.** Deletes the section files a shrinking rebuild strands (~360 KB per type change, per book), gated on `resume_spine` so a suspended walk is never pruned against. Three fault-harness tests, each mutation-checked. | Merge — **before B7**, which multiplies the leak |
| `opt/a11-landscape-glyph-batching` | **Ready.** Differential test against the code it replaces, on both board configs; all goldens pass **unblessed**. | Device measurement only — the author's own merge gate |
| `opt/upload-session-token` | **Ready.** Complete, gate anchored not scanned, goldens re-blessed and visually verified on both boards. | Device check |
| `opt/inflate-caller-owned-window` | **One-line fix.** Silently cuts the Wi-Fi session heap ~10,504 B (67,856 → 57,352) because `heap_a` is literally `size_of::<ZipInflateScratch>()` and the type shrank. `.data`/`.bss` are unchanged, so every gate stays green. | Donate the new decompressor as the third heap region, then a `sleep-sync` run |
| `opt/d4-directed-wifi-join` | Complete, well-argued (see issue 04 — its separate-file design is better than this PRD's spec). | Four device checks |
| ~~`opt/single-repaint-per-page-turn`~~ | **MERGED as #56**, reworked first. The audit found it suppressed the `Loaded` *event* rather than the render, freezing the app's page count during a background build and stranding the reader at the frontier — rule 4 through a door rule 4 does not name. #56 moved the decision into `app_core::loaded_repaints` and kept the event unconditional, and picked up an open-gate latch and a failed-refresh retry on the way. | — |
| `opt/b7-per-config-section-caches` | **Three defects** (issue 02): `&str` byte-slicing that aborts on SD-derived filenames — reproduced end-to-end, reachable from the orphan sweep on every catalog write; a cross-config cache wipe at three sites, not the one admitted; `BookBuildResume` not keyed by layout config. Still the best-structured large change in the queue. | The three fixes, plus prune orphaned sections first |

### Tier 4 — worthwhile, unblocked, smaller

| # | Item | WS | Why | Effort |
|---|---|---|---|---|
| 8 | **First open on a deep resume** | B | B4 gives nothing near the end of a book; only a progress indicator helps. | M |
| 9 | **C8 — sleep entry's discarded second pass** | C | ~657 ms of the ~4.1 s sleep entry, thrown away by the next boot's `init_panel`. Refuting check is free (count `bench: refresh` per sleep in an existing capture). Risk gates it. | M + hw |
| 10 | **C9 — recovery combo on every wake** | C | 28 ms + 48 ADC conversions that cannot succeed on a button wake. One-line gate. | S |
| 11 | **F10/F11 — close two CI holes** | F | `tools/web-emulator` is built by **no PR gate** (a broken wasm merges green); the bench harness's own 25 tests run nowhere. Both cost ~0 CI wall clock. | S |
| 12 | **D5** — portal → station handoff | D | ~40–60 s and 3 steps off first-time onboarding. | M |
| 13 | **E7** — `sort_unstable_by_key` in the wifi scan | E | One stable sort of 20 elements costs a 4,128 B frame and 3.8 KB of flash; stability is irrelevant there. Cheapest item in the roadmap. | S |
| 14 | **F5 re-scoped** | F | Merriweather is **42.9% of the wasm** and is not the default face: −41% on *every* first visit, not just a board switch. The old deferral reasoning was wrong. | L |

### Unresolved — measure before ranking

**A15 — `fill_plane`'s 528 single-row transfers.** Two surveys costed it and
disagree by an order of magnitude (~87–139 ms vs ~10 ms), and neither
reconciles a third data point. The per-transaction model is not identified.
~20 minutes on device settles it; nobody should write code against it first.

Unscheduled, deliberately: **A4, A5** (hardware-risk display experiments, and
A5 is X4-only — the owner has no X4), **C6** and **C10** (both gated on C2's
meter reading), **E4** (flash is 57% used, 2.69 MB spare — less urgent than
recorded), **B5** (bundle-only; and its cost is stack, not time — see issue
02), **B1's remaining levers** (only if custom-font cold builds still measure
slow), **A9** (see below).

**A9 is effectively dead as a bandwidth item.** A plane write is ~26 ms of
which ~20.9 ms is wire time at the datasheet-ceiling 20 MHz, so the entire
addressable overhead is ~5 ms and A9 has at most ~3 ms to win — while
re-spending the 8 KB `TX_BAND` that A3 freed, on the tight board. It survives
only as the honest home for prestage overlap.

## Landed

| Item | PR | Measured result |
|---|---|---|
| A1 — `Settled` before prestage | #12 | Prestage off the page-turn path |
| A2 / A2-P — byte-run rasterizer fast paths | #24, #46 | Landscape layout 16–18 ms; portrait 33 ms (later 13 via #50); menus 82–90 → 3–8 ms |
| A3 — panel-native framebuffer byte order | #46 | Zero-copy `fb.band()` SPI; 8 KB `TX_BAND` freed |
| A6 — O(1) ASCII glyph advance lookup | #47 | Bypasses binary search for printable ASCII and the kerning search on unkerned fonts. No separate measurement — it landed inside the same rasterizer push |
| A10-as-shipped — portrait glyph transpose | #50 | **Portrait reading layout 33 → 13 ms median on X3 (2.5×)**. Not what A10 specified — see "Do not re-propose" |
| B2+B3 — catalog scan + incremental pagination | #10 | O(C+N) sweep; ~100–300 ms per build |
| B6 — settings-independent content cache (CONT.BIN) | #23 | Settings-change rebuild 2.4–2.6× faster than a full build (~37 s saved) |
| B4 — progressive first open | #53 | **Time-to-prologue 45.3 → 32.6 s; page-turn median during a build 1270 → 231 ms.** Reader now runs 15.9 s ahead of the builder |
| Reader-cache crate extraction | #55 | `reader-cache` host-testable; 7 fault-injection tests on an in-memory FAT16 card |
| **Single repaint per page turn** | #56 | Storage emits `Loaded` unconditionally; the repaint decision moved into `app_core::loaded_repaints` with a `text_replaced` flag. **~405 ms of panel time off every page turn in every book** — half the panel duty cycle and half the refresh count. Also fixed an open gate that latched forever when an open was served from RAM, and added a retry when a refresh fails |
| **B7 — per-config section caches** | *branch* `opt/b7-per-config-section-caches` | A book keeps a paginated copy per layout config, so flipping back to a size or orientation already read is a cache hit instead of the 24–27 s replay. +6.7 KB flash, no static RAM |
| C1+C3+C4+C5 — wake refresh, gauge decimation, idle tiers, boot init | #11, #36 | Wake seeded from the deep-sleep cause; idle tiered 10 min Reading / 3 min menus |
| D1 — SD SPI tier | #14 | Cold build −5.4%, write_ms −9.5%, progress write −35%. **Not** the hoped 2× |
| D3 — portal PSK | #19 | Shipped as a per-session runtime PSK, not the build-time one this PRD proposed |
| E1+E2+E3 — flash and stack budget | — | ~246 KB flash freed, ~7 KB stack headroom both boards, `.data` 52 → 5 KB |
| F1–F4+F6 — web emulator and CI | #13 | Initial transfer −49% gz; reading goldens now run in CI |

B7 is committed but not merged; everything else above is on `main`.

## Current measured baselines

**Display, X3, deliberate cadence, 50 turns (2026-07-27, main `e9163b3`).**
Quote these, not anything older.

| Metric | Value |
|---|---|
| Reading layout, portrait | **13 ms** median / 14 p95 |
| Reading layout, landscape | 16–18 ms |
| Menu / Settings layout | 3–8 ms |
| Render flush | **405 ms** median (379 ms of it panel BUSY) |
| Prestage (after the settle; reader never waits) | 24 ms |
| Progress write | 42 ms |
| **Page turn, press-to-settled** | **~424 ms** |

**Book pipeline, X3, 11.7 MB baseline book (2026-07-25 / 07-28).**

| Metric | Value |
|---|---|
| Full cold build | 64.0 s portrait (1240 pp / 100 sections), 62.2 s landscape |
| Settings-change replay via CONT.BIN | 24.7 s (736 pp) — 27.1 s (1240 pp), ~280–300 ms/section |
| Orientation flip | same replay path — B7 turns the flip *back* into a hit |
| Progressive first open → prologue | 32.6 s, interactive throughout |
| Warm reopen (RAM hit) | 13–15 ms |

**Two figures retired 2026-07-30, both of which items were sized against:**

- **Fast BUSY is 379 ms, not 421 ms.** The 421 figure is a June 10 2026
  capture on an unnamed board and is superseded.
- **Full BUSY on the X3 is 928 ms, not "~3.5 s".** Measured, n=16, in the
  bench log on disk. C1's "~2 s off every wake" was sized against the 3.5 s
  figure; the real saving is Full 928 → FastClean 455, about 474 ms. Anything
  still ranked against 3.5 s needs re-sizing.

**Every latency figure in this document is a *tethered* baseline.** All of
them were captured with a USB monitor attached, and with a monitor attached
`esp-println` writes into a USB FIFO. Untethered there is no SOF and the same
prints take a blocking UART path — see Tier 0d. The numbers are not wrong;
they describe a device plugged into a laptop, which is not the shipped one.

**Never measured:** deep-sleep current or any other power figure at any
operating point (C2); the upload throughput ceiling's cause; boot- and
wake-to-first-paint, though the data is already in every capture on disk.
**Permanently unavailable: the owner has no X4.** Every "verify on both
boards" step is X4 compile/clippy/goldens only; on-device X4 validation
happens if a contributor with hardware appears. C2's wake-reliability step
is X3-only for this reason.

## Method rules, learned expensively

1. **Measure the split before ranking a "cache the layout" item.** A10 sat
   at #1 for three revisions on a premise nobody checked: layout is 2.8 µs
   of a 197 µs portrait page render. The other 98.5% is rasterization.
2. **Check that a measurement brackets what the user experiences.** One
   misplaced `println!` put the prestage inside the reported page turn and
   promoted a pessimization to #2 in the queue. Likewise `storage_first_page`
   at 455 ms described nothing a reader feels — the metric is
   time-to-first-*content*, compared against a plain build, not against zero.
3. **`page-turn` is operator-driven and not cadence-robust.** Its `page turn`
   statistic is input→next-render, so a press landing mid-render is credited
   with only the remainder; burst captures show 2 ms turns. Quote it only
   from deliberate cadence. `layout_ms`, `flush_ms`, `busy_ms` are per-render
   and safe from either.
4. **A partial cache must say on disk whether something is coming back for
   the rest.** The reducer clamps the reader to the advertised page count, so
   a truncated index is a one-way trap the reader cannot provoke a rebuild
   out of — and it looks exactly like a short book.
5. **The compiler names what compiles, not what should be public.** Driving a
   crate extraction's visibility by "promote whatever rustc names" widened
   eighteen fields that only mean anything as a set.
6. **Host gates cannot see a stack overflow.** `tools/check.sh stack-frames`
   now disassembles the release build and fails over 24 KB per frame. It
   bounds one frame, not call depth.
7. **Check whether the answer is already being printed.** Added 2026-07-30,
   and it is the round's most expensive lesson. The replay split WS-B guessed
   at for three rounds, boot-to-first-paint, and B4's own headline numbers are
   all emitted by the firmware on every run and consumed by nothing. Before
   designing an experiment, `grep` the firmware for what it already prints and
   `grep` `bench.py` for what it actually reads — they are not the same set.
8. **A verification gate that can be silently absent is worse than none.**
   `report --strict` checks nothing on Python < 3.11 and exits 0 without a
   diagnostic, which is the interpreter `#!/usr/bin/env python3` selects on
   macOS. Any gate this project relies on should fail loudly when its
   preconditions are missing.
9. **Do not carry a conclusion across boards.** "The BUSY floor is OTP and
   cannot be touched" is true of the X4's SSD1677 and false of the X3's
   UC8253, which uploads its own LUTs and sets its own frame timing. That one
   conflation hid 136 ms of every X3 refresh for four rounds, and it is why
   WS-A is reopened. When a number and a mechanism come from different boards,
   at least one of them is wrong.
10. **Suppress the render, never the event.** An event that carries state the
   reducer needs must always be applied; only the repaint may be skipped.
   Suppressing the event instead is how the top-ranked branch reintroduced
   rule 4's one-way trap. A guard placed *below* the reducer — at the flush
   seam — is structurally immune to this and is the safer default.
11. **A pin protects bytes, not version numbers.** Before regenerating any
   frozen artifact, run the generator unmodified and require a clean diff;
   only then does the pinned toolchain mean anything. #66 pinned a
   Pillow/FreeType pair that could not rebuild the shipped font tables — the
   fingerprints came from #61's toolchain and the pin was never validated by
   regeneration — and the mismatch sat under a "reproduction verified" commit
   message for a week. The prove-first habit is what caught it (issue 08,
   H5).

## Workstreams

One issue file each, owning a distinct set of files.

- **WS-A — Display render path** (`issues/01-display-render-path.md`).
  `display/`, `fw/src/display_flush/`, flush/prestage region of
  `fw/src/tasks/display.rs`, `hal-ext/src/spi_dma.rs`. **Reopened
  2026-07-30** — the panel's own timing is software-set on the X3.
- **WS-B — Book pipeline** (`issues/02-book-pipeline.md`). `reader-cache/`,
  `fw/src/book_build.rs`, `fw/src/custom_font.rs`, `fw/src/library_sd.rs`,
  `ui/src/reading.rs`, `proto/`.
- **WS-C — Power & boot** (`issues/03-power-boot.md`). `fw/src/tasks/power.rs`,
  `fw/src/tasks/input.rs`, `hal-ext/src/rtc.rs`, `hal-ext/src/bq27220.rs`,
  planner seed in `app-core/src/lib.rs`, boot region of the display task.
- **WS-D — Storage & Wi-Fi** (`issues/04-storage-wifi-throughput.md`).
  `fw/src/sd_session.rs`, `fw/src/tasks/wifi.rs`, `fw/src/upload.rs`,
  `fw/src/sync_mem.rs`, pinned `embedded-sdmmc`.
- **WS-E — Flash & RAM budget** (`issues/05-flash-ram-budget.md`).
  `.cargo/config.toml`, `display/src/font.rs` (struct only), generated font
  tables.
- **WS-F — Web emulator, CI & the bench harness**
  (`issues/06-web-emulator-ci.md`). `web/`, `tools/web-emulator/`,
  `tools/build-web.sh`, **`tools/bench/`**, `.github/workflows/`. Disjoint
  from firmware work. **Scope widened 2026-07-30** to give the measurement
  harness an owner — it produces every device number in this roadmap and had
  none.
- **WS-H — Typography & text rendering quality**
  (`issues/08-typography.md`). `tools/fontgen_common.py` and the font
  generators, the generated `display/src/*_generated.rs` tables, and the text
  layout in `ui/src/reading.rs`. **New 2026-07-30.** Text quality had no owner
  because it spans three regions. Note the economics invert here: glyph
  rasterization runs on the *host*, so quality costs the device nothing and
  the binding constraint is cache invalidation rather than speed.
- **WS-G — App state & render invalidation**
  (`issues/07-app-render-invalidation.md`). `app-core/`, `ui/`, and
  `fw/src/tasks/app.rs` as the seam. **New 2026-07-30.** The top-ranked item
  was previously filed under no workstream because this region had no owner.
  One avoided panel refresh is worth ~405 ms against a 13 ms total layout
  budget, so the only thing worth ranking here is a render that should not
  happen — never a CPU micro-optimization.

### Coordination hazards

1. **`fw/src/tasks/display.rs`** is touched by WS-A (flush/prestage), WS-C
   (boot init, OTA probe skip), and WS-B (the background-build branch of the
   select). Disjoint regions; rebase carefully.
2. **Stack and RAM are shared currency.** Every `.bss` change re-checks the
   link-time stack ASSERT and `tools/check.sh stack-frames` on **both** X4
   and X3 — X3 is the tight one, 42 KB region against a 27 KB floor.
3. **Golden frames** gate anything touching a render path:
   `fixtures/golden` + `tools/emulator/tests/reading_golden.rs`, per
   `docs/agents/visual-verification.md`.
4. **Hardware sign-off** is required for C2, C6, A4, A5. An agent can prepare
   the code; the verdict is a measurement. C2's first-line experiment now
   needs only a device and patience (the fuel gauge integrates over a long
   sleep), not an instrument; C6 and A4/A5 still need a meter or a careful
   visual soak, and A5 needs an X4 nobody has.
5. **`fw/src/sd_session.rs` is WS-D's.** WS-B benefits from its changes but
   must not modify it.

## Verification

- Firmware timing: `tools/bench/bench.py` suites (`page-turn`,
  `storage-cache`, `sleep-sync`, `channel-stress --host`, `reader-soak`) per
  `docs/agents/bench.md`. Budgets in `tools/bench/benches.toml`.
  **Run it under Python ≥ 3.11 until Tier 0a lands** — on 3.9 the `--strict`
  budget check silently passes everything (`tomllib` is absent, and macOS
  system `python3` is 3.9.6).
- **Fold the protocol rules into `docs/agents/bench.md`.** Method rules 3, 7
  and 8 live only in this scratch document, and `docs/agents/bench.md` — which
  `AGENTS.md` routes every bench user to — says nothing about cadence,
  unmatched presses, or the required interpreter. The rule that was learned by
  losing a round should not be discoverable only by reading the roadmap.
  Likewise `AGENTS.md`'s verification entry points do not mention
  `tools/check.sh stack-frames`.
- Visual: emulator goldens on both boards per
  `docs/agents/visual-verification.md`.
- Size/stack: `llvm-size -A`, `llvm-nm` on `_stack_start`/`_stack_end`, the
  link-time ASSERT, and `tools/check.sh stack-frames`.
- Upload: timed `curl --data-binary @book.epub` A/B plus `sd_stats` counters
  (`write_calls` vs `write_blocks` proves batching).
- Power: **first line is the X3's BQ27220 as an integrating coulomb counter**
  over a long deep sleep — no meter, no disassembly, and it stays powered
  while the SoC does not (issue 03, C2). Do **not** meter the 4-pin pogo
  cable: it is USB, so it sits on the wrong side of the charger and plugging
  it in changes the state under test. A series meter on the *battery lead* is
  the follow-up if the gauge indicts something, and it wants a PPK2-class
  instrument rather than a DMM — burden voltage on a µA range can brown the
  device out on a wake spike, and the span needed is ~15 µA to ~100 mA.
  `bench.py` has no power channel either way.

## Do not re-propose

Each of these was tried, measured, and rejected. The reason is the part that
matters — without it the idea comes back.

- **A10 as specified (pre-computed line-wrap caching).** There is no wrapping
  in the render path to cache. The EPUB sink wraps at cache-build time,
  `push_line_block` stores one physical line per `BlockRecord` with
  `line_count: 1`, and the page record is already an O(1) index. Reading-view
  render time moves on rasterizer work only.
- **A7 / skipping or deferring the RED prestage** (#48, reverted #52). The
  prestage is what keeps the previous-frame write *off* the turn; skipping it
  defers the same write into the next Fast flush ahead of `DisplayRefresh`,
  where the reader does wait — and each skip leaves the next turn unstaged,
  so a held button pays it every time. It also never fired: one `yield_now()`
  cannot cover the Settled → app → next-`RenderRequest` round trip. Genuine
  overlap with panel BUSY is a different design and belongs to A9.
- **A8 (4-way strided unrolling of the portrait `blit_row` loop)** — #49
  closed. #50 removed that loop from the glyph path and took 2.5× instead.
  Only `fill_span` is left for it, which is menu furniture at 3–8 ms.
- **D2 (radio RX buffers + AMPDU-RX + paced SD writes).** Timed X3 A/B on a
  3.2 MB EPUB: main 19.3 s, D1+D2 21.1 s, D1+buffers-only ~20.2 s. Pacing
  cost ~1 s/upload; the buffers bought nothing and spent ~6.6 KB of loaned
  heap. Throughput sits near 160 KB/s under every configuration — the
  bottleneck is neither radio RX nor SD write stalls. Start with the
  upload-ceiling investigation, not by re-trying these.
- **Build-time portal PSK.** A committed PSK is public in a public repo, and
  a CI-minted one is extractable from released firmware. Shipped as a
  per-session runtime PSK instead (#19).
- **Partial-window panel refresh** — deliberately shelved twice.
- **SPI above 40 MHz** (rated ceiling); **`MIRROR_Y=true`** (tested, wrong).
- **80 MHz CPU clock** — 160 MHz race-to-idle is an explicit decision.
- **ilp32e ABI, frame-pointer removal** — rejected in the stack brainstorm.
- **Re-donating dram2 to the radio heap** — removed on purpose to restore
  stack. Do not win D2's heap back this way.
- **kosync progress sync** — implemented, shipped unused, removed.
- **Software optimization of the Full-refresh waveform** — noise.
- **Dependency-dedup and panic/fmt shrinking** — <25 KB combined, poor ROI.
- **`.eh_frame`** — non-alloc, not in the flash image. Nothing to reclaim.
- **Optimizing the golden runner** — measured fast (24 scenarios in 0.52 s).
- **esp-radio power-save during an upload session** — already off.
  `WifiController::new` calls `set_power_saving(PowerSaveMode::default())` and
  that default is `None`, with an upstream comment saying the blob default is
  bad for bandwidth. Refuted from source 2026-07-30; it was suspect 2 in the
  upload-ceiling list and is now deleted from it.
- **Decimating the 15 ms input tick** — now has a number, not just a
  principle. One tick is ~150–250 µs of CPU (~1.0–1.7% duty) and the rest is
  already WFI, so decimating to 60 ms saves ~**0.06 mA of ~15–20 mA** while
  costing 4× the press latency. The standing rejection is correct.
- **A9 as a bandwidth item on the X3** — a plane write is ~26 ms of which
  ~20.9 ms is wire time at the datasheet-ceiling 20 MHz, so there is at most
  ~3 ms to win, and double-buffering re-spends the 8 KB `TX_BAND` that A3
  freed on the tight board. A9 survives only as the home for prestage overlap.
- **Moving a large stack temporary into a static to "save stack".**
  `_stack_end` is exactly `ADDR(.bss) + SIZEOF(.bss)` on both boards —
  verified by address arithmetic and by a natural experiment — so every
  `.bss` byte trades **1:1** against the main stack. Hoisting is neutral at
  best and a net loss when the temporary was not on the peak call chain. Only
  deleting `.bss`, or shrinking a frame on the peak chain, buys margin.
- **Holding the panel RST line through deep sleep, on the hardware we ship.**
  Closed unmerged as PR #70 — the one entry here that was decided rather than
  measured, so the condition matters more than usual. Upstream's ~36 h-to-dead
  field report names the **UC8179**, whose DC-DC booster is host-programmed via
  BTST and restarts off a drifting RST; it calls the SSD1677 tolerant, having no
  external booster and a deep sleep that actively discharges. Two separate
  reasons not to re-propose it as written: today's units are SSD1677 and UC8253,
  and #70's `rst.set_high()` would not have worked anyway, because a C3 pad goes
  high-Z in deep sleep whatever level it was last driven to. It comes back only
  with `uc8179-x4-driver` or `uc8279-x3-driver`, and then as a pad hold armed
  before sleep and released before the wake reset pulse — both halves, or the
  reset pulse bounces off the latch. Tracked on those two PRDs. The separate SD
  rail suspect is `x3-sd-rail-sleep-power`; do not conflate them.

## Doc drift to fold into whichever PR touches the area

- `hal_ext::rtc::enter_light_sleep_timer` has zero call sites. Either wire it
  up behind C4's successor tier or delete it.
- `docs/FLASHING.md`'s open "Locked-unit confirmation" box describes only the
  stock **bootloader's** descriptor gate. It does not mention the separate risk
  that a locked X3's OEM SD updater rejects `update.bin` through the buggy
  app-side validator in the stock firmware — garbage eFuse block revisions read
  via a misaligned `bootloader_mmap`, which CrossPoint documents and works around
  in its own shipped app. Nothing we ship can change that outcome, since the bug
  is in the firmware being replaced, which is exactly why it belongs in the doc
  next to the existing box rather than in a PRD. (The PRD that asked whether we
  needed CrossPoint's linker wrap was removed: we link no ESP-IDF, so there is no
  such symbol in our image.)

The other two entries that lived here are resolved: `ARCHITECTURE.md` no
longer claims a 1.5 s wake unconditionally (C1 landed and the doc now states
the wake-cause condition), and the dram2 radio-heap paragraph has been
corrected.
