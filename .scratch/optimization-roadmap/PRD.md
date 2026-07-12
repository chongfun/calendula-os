# PRD: CalendulaOS optimization roadmap

Status: round 1 landed — see "Status after round 1" below for what's done, what's disproven, and the next queue
Date: 2026-07-09 (status updated 2026-07-12)
Author: research pass over six parallel code-survey agents (display, book pipeline, power/boot, flash/RAM, storage/Wi-Fi, web emulator), each scoped to a mostly-disjoint code region so implementation can proceed in parallel.

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

**Next queue (priority order, rationale in the issue files):**

1. **B1** — custom-font metric cache (issue 02). Unblocked: E2 landed, metrics
   are the 12-byte layout; the cache should store encoded 12-B records (the
   in-flash `FONT_METRICS` cache already does exactly that — copy its shape).
2. **A2** — byte-run rasterizer fast paths (issue 01). Unblocked by E2. New
   constraint since 2026-07-09: portrait is under active development (portrait
   reading sheet + icons merged; a portrait-default PR is open) — gate the fast
   paths on Landscape frames and keep per-pixel portrait, exactly as the issue
   already says. Goldens must pass unchanged, both boards.
3. **D4** — directed Wi-Fi join (issue 04). Software-implementable; needs a
   device for join-time A/B. WIFI.BIN gains fields — keep old records readable.
4. **C2** — deep-sleep GPIO hold + first-ever sleep-current measurement
   (issue 03). Needs a device and a µA meter; the largest standby unknown.
5. **A3** — panel-native framebuffer byte order (issue 01). Frees 8 KB; heavy
   golden re-bless; coordinate with the portrait work before starting.
6. **B4** — progressive first open (issue 02). Its sequencing prerequisites
   (B3, D1) have landed. Large; interleaves with display-task storage dispatch.
7. **D5** — portal → station handoff (issue 04). Hardware-validation-heavy.
8. **NEW: upload-ceiling investigation** (issue 04, replaces D2's slot).
   Measure-first: find what actually caps uploads at ~160 KB/s.

Tier 3 unchanged (A4, A5, C6, E4, F5) except **D6**, which now has a complete
evidence file in issue 04 — read it before deciding.

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
