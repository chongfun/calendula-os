# PRD: CalendulaOS optimization roadmap

Status: round 3 landed (A2-P, A3) — see "Status after round 3" below for the current queue
Date: 2026-07-09 (status updated 2026-07-26)
Author: research pass over six parallel code-survey agents (display, book pipeline, power/boot, flash/RAM, storage/Wi-Fi, web emulator), each scoped to a mostly-disjoint code region so implementation can proceed in parallel.

## Status after round 3 (2026-07-26)

**Landed on main / branch since round 2:**

- **A2-P** — portrait byte-run/strided fast paths (issue 01). Hoisted `map()` coordinate math, bounds checking, bit-shifts, and division out of the `fill_span` and `blit_row` inner loops in `display/src/fb.rs`. Measured on X3 hardware: portrait reading layout dropped to `28–34 ms` (averaging ~31 ms per turn).
- **A3** — panel-native framebuffer byte order (issue 01). Internal `Framebuffer` (`self.data`) is now stored in panel-native byte order across both X4 and X3. Completely eliminated `fill_transformed_band_impl` and the 8 KB static `TX_BAND` buffer allocation in `fw/src/tasks/display.rs`. Display bands now stream zero-copy directly over SPI via `fb.band()`. Goldens re-blessed; P1 and P2 invariants verified.
- **A2** — byte-run rasterizer fast paths, landscape frames (#24). Landscape reading layout `16–18 ms` (sub-20 ms).
- **B6** — settings-independent content cache, CONT.BIN (#23).
- **B1's goal arrived as an upstream port** (#37).
- **Portrait is default orientation** (#5).
- **Display SPI at datasheet 20 MHz on both panels** (#42).

**Next queue (re-ranked 2026-07-26; strictly prioritized for the portrait reading experience):**

1. **A6 — Pre-computed Line-Wrap Caching for Reading View** (issues 01 & 02). Pre-calculates paragraph line-break offsets for adjacent pages while reading in Portrait mode. Reduces Portrait reading `layout_ms` from ~31 ms to **near-0 ms** ($O(1)$ lookup).
2. **A7 — Asynchronous / Pipelined Prestaging** (issue 01). Overlaps the previous-frame buffer prestaging (`DTM1`) with panel BUSY wait or SPI DMA transfers. Completely eliminates the **28 ms** prestage delay from the portrait page-turn latency path.
3. **A8 — 32-Bit Word-Wide Strided Iteration for Portrait Blits** (issue 01). Unrolls `blit_row` column loops with 32-bit `u32` word pointers on RISC-V to process 4 native rows per iteration, further reducing Portrait blitting CPU cycle count by ~20–30%.
4. **B7 — Per-Config Section Caches & Orientation Toggle Acceleration** (issue 02). Fast return to previously-built configs; avoids ~24 s replay on font size changes and portrait↔landscape toggles.
5. **B4 Rework — Progressive First Open for Reading** (issue 02). Time-to-first-page when opening a new book in Portrait mode.
6. **A9 — Overlapped SPI DMA Band Transmits** (issue 01). Double-buffered SPI DMA streaming so band $N+1$ is prepared while band $N$ transmits.
7. **C2** — deep-sleep GPIO hold + sleep-current measurement (issue 03).
8. **D4** — directed Wi-Fi join + strongest-AP join (issue 04).
9. **Upload-ceiling investigation** (issue 04).
10. **D5** — portal → station handoff (issue 04).

Tier 3 unchanged: A4, A5, C6, E4, F5, D6.

**Bench session results (2026-07-26 live X3 measurements):**

| Metric | 2026-07-25 baseline | 2026-07-26 measured (A2-P + A3 landed) |
|---|---|---|
| Reading layout, landscape (A2 fast paths) | 17 ms median / 18 ms p95 | **16–18 ms** (sub-20 ms!) |
| Reading layout, portrait (A2-P strided fast paths) | 33 ms median / 35 ms p95 | **28–34 ms** (~31 ms average) |
| Menu / Settings layout | 82–90 ms (pre-A2) | **3–8 ms** |
| Fast flush | 408–409 ms (busy 379) | **408–409 ms** (busy 379 ms) |
| Prestage | 28 ms | **27–28 ms** (8 KB static `TX_BAND` buffer deleted) |
| Storage open (RAM hit) | 0 ms | **0 ms** |
| Progress write | 44–60 ms | **41–44 ms** |
| B6 replay (Type Size change) | — | **24.7 s** (736 pp / 82 sections) and **27.1 s** (1240 pp / 100 sections), ~280–300 ms/section |
| Orientation flip (portrait↔landscape) | — | **23.8 s** (same replay path — page box is wrap-relevant) |
| Full build, same settings (evening session, via the new cache-clear) | 14.1 s (July config: 441 pp, pre-CONT.BIN) | **64.0 s** portrait (1240 pp / 100 sections), **62.2 s** landscape (1303 pp / 82) |

**B6's ratio, measured 2026-07-25 evening (cache cleared on device, then
rebuilt at identical settings): replay is 2.4–2.6× faster than the full
build** — 27.1 s vs 64.0 s portrait, 23.8 s vs 62.2 s landscape — saving
~37 s per settings change. B6 earns its keep; and since 24–27 s of
user-facing wait remains, B7's promotion stands. Two context notes: the
full build now also captures CONT.BIN (wr 8.9–9.7 k blocks vs the replay's
4.1 k), and the July 14.1 s cold build was a ~3× smaller page-count config
— today's ~64 s first open at current settings also strengthens B4's
progressive-open case.

Replay authenticity: both Type Size rebuilds read 2.8–3.8 MB from the
card — far below the 11.7 MB source zip — proving the CONT.BIN path ran,
not the EPUB fallback. Nearly all replay cost is downstream of the capture
point (wrap + section writes), which is what promotes B7.

**Still unmeasured, with availability verdicts (2026-07-25):**

- **#42 on the X4 — permanently unavailable: the owner has no X4.** The
  20 MHz value stands on freeink's fleet evidence; it is a one-constant
  revert if an X4 user ever reports regressions. This applies to every
  "verify on both boards" hardware step in this roadmap: X4 coverage is
  compile/clippy/goldens only (tools/check.sh covers both boards
  host-side); on-device X4 validation happens only if a contributor with
  hardware appears. C2's "test both boards" wake-reliability step reduces
  to X3-only for the same reason.
- **Full-build-vs-replay ratio** — MEASURED same day via the new Library
  cache-clear (file-management slice 1, `.scratch/file-management/`):
  replay 2.4–2.6× faster; numbers in the table above.
- **C2 sleep current** — needs the µA meter (X3).

## Status after round 1 (2026-07-12)

**Landed on main:** A1 (#12), B2+B3 (#10), C1+C3+C4+C5 (#11), D1 (#14, measured
results in its commit message), D3 as per-session runtime PSK (#19 — a stronger
design than the build-time PSK this PRD proposed; see issue 04), E1+E2+E3,
F1–F4+F6 (#13). Adjacent landings this round: upload same-prefix clobber fix +
`upload-store` crate with host fault-injection tests (#15, #18 — not from this
roadmap, but they own the upload write path now), bench harness fixes (#16,
#17), nested-worktree linker fix (#8), agent-contract docs under `docs/agents/`.

**Disproven on hardware — moved to the do-not-re-propose list:** D2 in its
entirety, and D1's "~2× SD bandwidth" framing (real win is 5–10% of cold
builds; the measured evidence now lives in D1's commit message and issue 04).

**Next queue (2026-07-12) — superseded by "Status after round 2" above.**
Of that queue: A2 merged as #24 (landscape frames), B6 merged as #23, B4's
branch is stranded (see round-2 status), B1 was closed by upstream port #37,
and A3/C2/D4/D5/upload-ceiling/B7 carry forward re-ranked.

**docs/OPTIMIZATION_PLAN.md audit (2026-07-12):** that document (a 2026-07-05
brainstorm predating this roadmap) was checked against main, folded in, and
deleted. Its items 0–3 had landed — 8 KB `read_at` chunks with the bounce
buffer gone (`EPUB_READ_AT_CHUNK_BYTES`, `fw/src/reader_cache.rs:39`),
incremental pagination as `PageIndexCursor` (#10), held-open SECTIONS dir
(`with_v2_sections_dir`, `fw/src/reader_cache_files.rs:975`). Its item 6 was
already tracked here as B4. Items 4–5 were the only live remainders, brought
in above as B6/B7 with their full designs inlined in issue 02. One design
note did not survive the audit: the forward-only upload-time build path
(`ZipLocalStream`) has no firmware call site anymore — uploads just store
files (#18) and the cache builds on first open — so B6's capture only needs
the random-access `ZipStream` path.

**Operational context for the next agent (hard-won this round):**

- Verification gates per branch: `cargo fmt --all --check`; host clippy set +
  `tools/cargo.sh clippy -p fw` on BOTH boards (`--features device-x3`);
  release links on both boards (the stack ASSERT is the guard); host tests
  `--workspace --exclude hal-ext --exclude fw`; emulator golden `--check` on
  both boards. Read `docs/agents/` — agent-contract docs were added this round.
- Firmware in nested worktrees links fine now (#8); no RUSTFLAGS workaround.
- Bench harness: captures survive deep-sleep port loss and `report` summarizes
  only the latest run in the log (`--all` pools). `reader-soak` is
  operator-driven — a human works the device while it captures — and menus
  idle-sleep after 3 min (C4), so keep interacting or the device deep-sleeps
  mid-capture.
- Timed upload A/B protocol that produced the D2 verdict: same book/card/
  network/position, `curl -sS -o /dev/null -H 'Expect:' --data-binary @book
  "http://<ip>/upload?name=book.epub" -w '%{time_total}s %{speed_upload} B/s'`,
  3 runs, compare medians. `upload: heap used/free` prints after each upload.
- Measured X3 envelope (2026-07-12, post-round-1 main): Fast flush 415 ms
  (busy 379), FastClean 691 ms (busy 456), prestage 33 ms, reading layout
  19–22 ms, catalog load 31 ms / 15 EPUBs, cold build 14.1 s for an 11.7 MB /
  441-page EPUB, progress write 51 ms, warm reopen (RAM hit) 13–15 ms.
  **Stale as of 2026-07-25:** superseded by the bench-session table in
  "Status after round 2" (re-baselined on X3 the same day).
- Observed once, unexplained (2026-07-11): X3 PON busy wait hit its 1 s
  ceiling (`PON busy_low=false 1000ms`) during sleep-entry Full refresh, then
  behaved normally. First suspect if X3 sleep entry ever misbehaves.

## Goal

Ship the highest-ROI performance, battery, and size improvements across the firmware and web emulator, organized into six workstreams that touch mostly-disjoint files so multiple agents can work concurrently. Every item below cites measured baselines from the repo's own docs, benches, or artifacts — not guesses.

## Measured baselines (2026-07-09)

| Metric | Value | Source |
|---|---|---|
| Page turn press-to-settled | 470–473 ms median (421 ms is panel BUSY — 89%) | docs/IMPLEMENTATION_PLAN.md |
| Layout + framebuffer draw | Reading 20–36 ms; menus 82–90 ms | same |
| Whole-frame RAM stream | 22–24 ms/plane (~10 ms is wire; rest is transform+copy) | same |
| Wake from deep sleep, first paint | ~3.5 s Full refresh (doc claims 1.5 s FastClean — drift, see C1) | app-core/src/lib.rs:182 vs docs/ARCHITECTURE.md:606 |
| Cold V2 cache build | 3.9 s / 117-page EPUB (~70% CPU, 30% SD I/O) | docs/IMPLEMENTATION_PLAN.md |
| Warm reopen | 50–85 ms | same |
| Wi-Fi station join | ~21 s | fw/src/tasks/wifi.rs:36 |
| Upload throughput | never measured; SD writes are 1 CMD24 per 512 B through 64-B SPI chunks | fw/src/sd_session.rs:132 |
| Deep-sleep current | **never measured**; claimed 10–15 µA; SD/EPD pins float in sleep | docs/ARCHITECTURE.md:627 checklist item 6 |
| Firmware image | 3.87 MB (fonts 2.97 MB = 77%); glyph metric tables 797 KB | llvm-size on release ELF |
| Main stack headroom | 39.4 KB X4 (was 45.7 KB on 2026-07-07); X3 ~low-30s vs 27 KB link ASSERT | llvm-nm `_stack_start − _stack_end` |
| Web emulator wasm | 4.9 MB raw / 1.45 MB gz per board; books 1.98 MB, fonts ~3 MB; two boards 99.9% identical data | ls + wasm section dump on _site/ |
| Golden coverage gap | `tools/emulator` tests (incl. 14 reading goldens) run in **no CI workflow** | .github/workflows/ci.yml |

## Priority tiers (ROI = impact × confidence ÷ effort)

### Tier 1 — small effort, large or certain wins (do first)

| # | Item | Workstream | Impact | Effort |
|---|---|---|---|---|
| 1 | Deep-sleep GPIO hold + first-ever sleep-current measurement | C | Possibly months-vs-weeks of standby | S–M |
| 2 | Fix wake refresh: seed planner from deep-sleep wake cause (FastClean, not Full) + drop redundant second `init_panel` | C | ~2 s off every wake | S–M |
| 3 | SD throughput tier: SPI chunk 64→512 B, data clock 20→25 MHz | D | ~2× SD bandwidth; speeds builds, catalog, uploads, reopens | S |
| 4 | Restore radio RX buffers + AMPDU-RX; yield between SD blocks during upload | D | 2–4× upload throughput, compounding with #3 | S |
| 5 | Send `DisplayEvent::Settled` before RED prestage | A | ~20–25 ms per page turn on held-button cadence | S |
| 6 | Custom-font metric cache for non-ASCII glyphs | B | Tens of seconds to minutes off custom-font cold builds | S–M |
| 7 | X3: decimate battery-gauge I2C from 66 Hz to ~0.3 Hz | C | Removes ms-scale input jitter + 0.5–2 mA awake | S |
| 8 | Stack headroom: switch tables to flash + halve DISPLAY_EVENTS | E | ~5 KB stack margin (X3 is nearing the 27 KB floor) | S |
| 9 | Web: fetch books at runtime instead of `include_str!` | F | Initial transfer 1.45 → ~0.80 MB gz (−45%) | M |
| 10 | Web/CI: run `tools/emulator` tests in CI (closes golden coverage hole) + preload wasm + wasm-opt | F | Correctness hole closed; earlier first frame | S |
| 11 | WPA2-PSK the onboarding hotspot (credentials currently plaintext over open RF) | D | Closes a real credential-disclosure hole, zero UX cost | S |
| 12 | Idle timeout 10 min → 3–5 min (or per-view tiers) — land after item 2 | C | ~25–50 mAh/day for typical use; biggest behavioral battery lever | S |

### Tier 2 — medium effort, solid wins

| # | Item | Workstream | Impact | Effort |
|---|---|---|---|---|
| 13 | Glyph metrics 16 → 12 bytes (layout already proven by SD font-pack format) | E | ~195 KB flash + 1.2 KB RAM | S–M |
| 14 | Byte-run rasterizer fast paths (fill_rect, glyph row blits) | A | Reading layout ~20–36 → ~10–15 ms; menus well under half | M |
| 15 | Zero-init `ReaderStore` so `SD_LIBRARY` (47 KB) moves .data → .bss | E | ~46 KB flash + skips 47 KB boot-time copy | M |
| 16 | Catalog scan: O(C+N) orphan sweep, fewer FAT re-walks, title in catalog record | B | Hundreds of ms → seconds on large libraries; snappier Library scroll | M |
| 17 | Directed Wi-Fi join (persist channel/BSSID in WIFI.BIN) | D | Join ~21 s → ~3–6 s on repeat sessions | M |
| 18 | Portal → station handoff in one session (kill the "run sync twice" reset) | D | ~40–60 s + 3 user steps off first-time onboarding | M |
| 19 | Panel-native framebuffer byte order (flush becomes a pure stream) | A | ~10–13 ms per turn and per prestage; frees 8 KB TX_BAND | M–L |
| 20 | Incremental pagination cursor during builds | B | ~100–300 ms per build, scales with book length | S–M |

### Tier 3 — large or hardware-risky (schedule deliberately)

| # | Item | Workstream | Impact | Effort / risk |
|---|---|---|---|---|
| 21 | Progressive first open: publish target section early, finish build in background | B | First open 3.9 s → ~1 s; minutes → seconds on huge books | L |
| 22 | Multi-block SD CMD18/CMD25 (patch pinned embedded-sdmmc) | D | Plausibly halves remaining SD time; 3–5× uploads with #3+#4 | L (fork maintenance) |
| 23 | Skip RED-plane write when CTRL1 bypasses it (verify on hardware first) | A | ~23 ms off Full/FastClean flushes | S code, medium hw risk |
| 24 | Temperature-override "hot" LUT for Fast refresh, opt-in via RefreshPolicy | A | 60–120 ms per turn — the only lever below the 421 ms panel floor | M, high hw risk |
| 25 | X3: power off UC8253 charge pump after ~20–30 s static page | C | 1–3 mA whenever an X3 shows a static page | M; X3 path still hw-unverified |
| 26 | Ship Merriweather/SemiBold as SD font packs instead of in-flash | E | Up to 1.8 MB flash; only when headroom is wanted | L, product tradeoff |
| 27 | Web: shared `fonts.bin` across board builds | F | Board switch 1.45 MB → ~70 KB | L, after item 9 |

## Workstreams

Each workstream is one issue file under `issues/`, owns a distinct set of files, and can be assigned to a separate agent. File-overlap hazards are listed in "Coordination" below.

- **WS-A — Display render path** (`issues/01-display-render-path.md`): items 5, 14, 19, 23, 24. Files: `display/`, `fw/src/display_flush/`, `fw/src/tasks/display.rs` (flush/prestage region), `hal-ext/src/spi_dma.rs`.
- **WS-B — Book pipeline** (`issues/02-book-pipeline.md`): items 6, 16, 20, 21. Files: `fw/src/reader_cache*.rs`, `fw/src/custom_font.rs`, `fw/src/library_sd.rs`, `fw/src/reader_layout.rs`, `ui/src/reading.rs`.
- **WS-C — Power & boot** (`issues/03-power-boot.md`): items 1, 2, 7, 12, 25. Files: `fw/src/tasks/power.rs`, `fw/src/tasks/input.rs`, `hal-ext/src/rtc.rs`, `app-core/src/lib.rs` (planner seed), boot region of `fw/src/tasks/display.rs`.
- **WS-D — Storage & Wi-Fi throughput** (`issues/04-storage-wifi-throughput.md`): items 3, 4, 11, 17, 18, 22. Files: `fw/src/sd_session.rs`, `fw/src/tasks/wifi.rs`, `fw/src/upload.rs`, `fw/src/sync_mem.rs`, vendored `embedded-sdmmc`.
- **WS-E — Flash & RAM budget** (`issues/05-flash-ram-budget.md`): items 8, 13, 15, 26. Files: `.cargo/config.toml`, `display/src/font.rs` (struct only), `fw/src/reader_store.rs`, generated font tables.
- **WS-F — Web emulator & CI** (`issues/06-web-emulator-ci.md`): items 9, 10, 27. Files: `web/`, `tools/web-emulator/`, `tools/build-web.sh`, `.github/workflows/`. Fully disjoint from firmware workstreams.

## Coordination hazards (read before starting parallel work)

1. **`fw/src/tasks/display.rs`** is touched by WS-A (flush/prestage/Settled ordering), WS-C (boot double-init, OTA probe skip), and eventually WS-B item 21 (storage-command continuation). WS-A and WS-C touch disjoint regions of the file; rebase carefully. Sequence item 21 after both.
2. **`display/src/font.rs` + `fw/src/custom_font.rs`**: WS-E item 13 (12-byte metric struct) and WS-B item 6 (metric cache) both touch `custom_font.rs`, and WS-A item 14 touches `font.rs::draw_glyph`. Recommended order: land item 13 first (it shrinks the cache entries item 6 will store), then 6 and 14 independently.
3. **`fw/src/sd_session.rs`** is owned by WS-D. WS-B benefits from its changes but must not modify it — coordinate through WS-D's issue.
4. **Stack/RAM budget is shared currency.** WS-E frees ~5 KB (+1.2 KB from item 13); WS-D spends ~448 B DRAM (512-B chunk) plus loaned-heap for radio buffers; WS-A item 19 frees 8 KB (TX_BAND). Every .bss change must re-check the link-time stack ASSERT on **both** X4 and X3 builds (X3 is the tight one). WS-A item 19 supersedes the alternative DMA-overlap design (which would *spend* 8 KB) — do not implement both.
5. **Golden frames**: WS-A items 14/19 and anything WS-F touches in render paths are gated on `fixtures/golden` + `tools/emulator/tests/reading_golden.rs` per `docs/agents/visual-verification.md`. Item 14 must pass goldens unchanged; item 19 re-blesses deliberately.
6. **Hardware access**: items 1, 23, 24, 25 require a device and (for item 1) a µA meter. Code can be prepared by an agent, but sign-off is a hardware measurement — mark those PRs as needing on-device validation.

## Verification

- Firmware timing: `tools/bench/bench.py` suites (`page-turn`, `storage-cache --cold --warm --strict`, `sleep-sync`, `channel-stress --host`, `reader-soak`) per `docs/agents/bench.md`. Budgets live in `tools/bench/benches.toml`.
- Visual: emulator scenario runner + golden frames on both X4 and X3 per `docs/agents/visual-verification.md`.
- Size/stack: `llvm-size -A` and `llvm-nm` on `_stack_start`/`_stack_end`; the fw link-time stack ASSERT is the guard on stable.
- Upload throughput: timed `curl --data-binary @book.epub http://<ip>/upload?name=...` A/B plus `sd_stats` counters (`write_calls` vs `write_blocks` proves batching).
- Power: bench-supervised runs with an external µA/mA meter (bench.py has no power channel today).

## Already considered / rejected — do NOT re-propose

- **D2 (radio RX buffers 8/24 + AMPDU-RX + SD writes paced in 512-B slices
  with yields) — rejected on hardware measurement 2026-07-11.** Timed upload
  A/B, X3, 3.2 MB EPUB: main 19.3 s median; D1+D2 21.1 s; D1+buffers-only
  ~20.2 s. The pacing cost ~1 s/upload; the buffers bought nothing and spent
  ~6.6 KB of loaned heap at join. Upload throughput sits near 160 KB/s
  regardless — the bottleneck is neither radio RX nor SD write stalls (main's
  blocking 4 KB writes demonstrably don't stall TCP). Code comments at the
  radio config and the upload write loop record the verdict; only the
  per-upload heap log survived. Any future upload work starts with the
  upload-ceiling investigation (issue 04), not by re-trying these.
- **Build-time portal PSK** (this PRD's original D3 shape) — a committed PSK
  is public in a public repo, and even a CI-minted one is extractable from
  released firmware.bin. Shipped instead as a per-session runtime PSK with
  on-device QR encoding (#19).

- Partial-window panel refresh — deliberately shelved, twice (docs/ARCHITECTURE.md, IMPLEMENTATION_PLAN.md).
- SPI above 40 MHz for the panel (rated ceiling); `MIRROR_Y=true` (tested, wrong).
- 80 MHz CPU clock — 160 MHz race-to-idle is an explicit decision (fw/src/main.rs:156).
- ilp32e ABI, frame-pointer removal (stack brainstorm rejections).
- Re-donating dram2 to the radio heap (removed on purpose to restore stack).
- kosync progress sync (implemented, shipped unused, removed).
- Software optimization of the Full-refresh waveform ("noise" per IMPLEMENTATION_PLAN).
- Dependency-dedup and panic/fmt shrinking — measured at <25 KB combined; poor ROI.
- `.eh_frame` — non-alloc, not in the flash image; nothing to reclaim.

## Doc-drift fixes to fold into whichever PR touches the area

- `docs/ARCHITECTURE.md:606` claims 1.5 s wake — false until WS-C item 2 lands.
- `docs/ARCHITECTURE.md:131-133` still describes the removed 16 KB dram2 radio-heap claim.
- `hal_ext::rtc::enter_light_sleep_timer` is documented as used but has zero call sites.
