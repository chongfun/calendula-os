# PRD: CalendulaOS optimization roadmap

Status: WS-A finished, WS-B's queue down to one non-B item plus leftovers.
Updated 2026-07-30 (B7 done, on a branch). Started 2026-07-09 from six parallel
code-survey agents, one per workstream, scoped to mostly-disjoint code
regions so work can proceed in parallel.

This document is kept to three things: **what is left**, **what has been
done**, and **what not to do**. Round-by-round history has been dropped —
it lives in the git log and the PR descriptions, which are the honest
record. What survives from it is only what still changes a decision.

## Goal

Ship the highest-ROI performance, battery, and size improvements across the
firmware and web emulator. Every item cites a measured baseline, not a guess.

## The queue

Ranked by measured user-facing cost. Items 1–2 are worth more than
everything below them combined.

| # | Item | WS | Why it ranks here | Effort |
|---|---|---|---|---|
| 1 | **Single repaint per page turn** | — | ~405 ms of panel time off *every* page turn, cached or not, in every book. Not a lettered item; found while measuring B4. Implemented on `opt/single-repaint-per-page-turn` (1 commit over main), not merged. | S |
| 2 | **C2** — deep-sleep GPIO hold + first sleep-current measurement | C | Sleep current has never been measured. If pins are leaking, this is the difference between months and ~2 weeks of shelf life. Needs a device and a µA meter. | S–M + hw |
| 3 | **First open on a deep resume** | B | B4 does nothing when the resume position is near the end of the book: `suspend_here` needs `total_pages > requested_page`, so resuming at page 561/562 pays a full 24.7 s build with zero progressive benefit. Only a progress indicator helps. | M |
| 4 | **`proto` inflate rework** | B | `ZipInflateScratch::new()` still costs a 20,960 B stack frame against a 24 KB gate, because miniz_oxide's stream layer keeps a private 32 KB window and can only be built by value. `inflate::core::decompress` takes the caller's buffer instead. Not a speed item — headroom. | M |
| 5 | **D4** — directed Wi-Fi join + strongest-AP | D | Repeat-session join ~21 s → ~3–6 s. | M |
| 6 | **Upload-ceiling investigation** | D | ~160 KB/s and nobody knows why. Instrument before fixing. | S–M |
| 7 | **D5** — portal → station handoff | D | ~40–60 s and 3 steps off first-time onboarding. | M |
| 8 | **A11** — batch landscape glyph rows | A | Landscape is now the slower frame (146 µs vs portrait's 93 µs host) after #50. Size it first — expect single-digit ms against a 424 ms turn, and portrait is the default. | M |
| 9 | **A9** — overlapped SPI DMA band transmits | A | Last render-path item with real headroom. | L |

Unscheduled, deliberately: **A4, A5** (hardware-risk display experiments),
**C6** (blocked on the X3 display path being hardware-verified), **D6**
(embedded-sdmmc fork maintenance), **E4** (only when flash headroom is
wanted), **F5** (only if board-switching matters), **B5** (micro-costs,
bundle-only — see issue 02 for why it has no host right now), **B1's
remaining levers** (only if custom-font cold builds still measure slow).

**Stop optimizing the render path.** WS-A is done: a portrait turn is 13 ms
of layout against ~405 ms of flush, 379 ms of which is panel BUSY. Nothing
left there is worth more than single-digit milliseconds except the two
hardware experiments.

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

**Never measured:** deep-sleep current (C2), upload throughput ceiling cause.
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

## Workstreams

One issue file each, owning a distinct set of files.

- **WS-A — Display render path** (`issues/01-display-render-path.md`).
  `display/`, `fw/src/display_flush/`, flush/prestage region of
  `fw/src/tasks/display.rs`, `hal-ext/src/spi_dma.rs`. **Finished.**
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
- **WS-F — Web emulator & CI** (`issues/06-web-emulator-ci.md`). `web/`,
  `tools/web-emulator/`, `tools/build-web.sh`, `.github/workflows/`. Fully
  disjoint from firmware work.

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
4. **Hardware sign-off** is required for C2, C6, A4, A5 — and C2 needs a µA
   meter. An agent can prepare the code; the verdict is a measurement.
5. **`fw/src/sd_session.rs` is WS-D's.** WS-B benefits from its changes but
   must not modify it.

## Verification

- Firmware timing: `tools/bench/bench.py` suites (`page-turn`,
  `storage-cache`, `sleep-sync`, `channel-stress --host`, `reader-soak`) per
  `docs/agents/bench.md`. Budgets in `tools/bench/benches.toml`.
- Visual: emulator goldens on both boards per
  `docs/agents/visual-verification.md`.
- Size/stack: `llvm-size -A`, `llvm-nm` on `_stack_start`/`_stack_end`, the
  link-time ASSERT, and `tools/check.sh stack-frames`.
- Upload: timed `curl --data-binary @book.epub` A/B plus `sd_stats` counters
  (`write_calls` vs `write_blocks` proves batching).
- Power: external µA/mA meter. bench.py has no power channel.

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

## Doc drift to fold into whichever PR touches the area

- `hal_ext::rtc::enter_light_sleep_timer` has zero call sites. Either wire it
  up behind C4's successor tier or delete it.

The other two entries that lived here are resolved: `ARCHITECTURE.md` no
longer claims a 1.5 s wake unconditionally (C1 landed and the doc now states
the wake-cause condition), and the dram2 radio-heap paragraph has been
corrected.
