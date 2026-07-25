# PRD: CalendulaOS optimization roadmap

Status: round 2 landed — see "Status after round 2" below for the current queue
Date: 2026-07-09 (status updated 2026-07-25)
Author: research pass over six parallel code-survey agents (display, book pipeline, power/boot, flash/RAM, storage/Wi-Fi, web emulator), each scoped to a mostly-disjoint code region so implementation can proceed in parallel.

## Status after round 2 (2026-07-25)

**Landed on main since round 1:**

- **A2** — byte-run rasterizer fast paths, landscape frames (#24). Goldens
  unchanged; the on-device `page-turn` layout_ms A/B is still pending.
  Portrait deliberately kept per-pixel — see the ranking note below.
- **B6** — settings-independent content cache, CONT.BIN (#23). Landed with
  review hardening (`BookPublishOutcome` distinguishes index-write failure
  from section read-back miss). The on-device A/B (Type Size change on the
  11.7 MB baseline book) is still pending — it also decides B7.
- **B1's goal arrived as an upstream port** (#37, from crosspoint `8da2d42`):
  16-slot non-ASCII metric ring, run measurement through one open pack
  handle, whole-glyph bitmap reads. B1 leaves the queue; its remaining levers
  (General-Punctuation slot runs, a walk-held pack handle) matter only if
  custom-font cold builds still measure slow.
- **Portrait is now the default orientation** (#5): `ReaderState::boot()`
  starts in PortraitButtonsLeft and the shell was already portrait-pinned.
  Fixtures inverted (landscape-* scenarios now cycle out of a portrait boot).
- **Display SPI at the datasheet 20 MHz on both panels** (#42, ported from
  freeink `9bd931e`): X3 plane writes ~25% faster; X4 refreshes pay
  ~17–20 ms to move in spec. On-device check pending; each value is a
  one-constant revert.
- **Adjacent robustness wave (#25–#36, #38, #41 — not from this roadmap, but
  it moved roadmap ground):** durable two-generation STATE/POS/WIFI.BIN
  persistence (#29 — WIFI.BIN is now a `proto::durable` A/B-generation
  record; D4's new fields ride that framing), book-open folded into one
  storage-owned transaction (#41 — restructures the open flow B4 modifies),
  portal credential verify + nearby-network list (#35 — rewrites D5's
  `run_portal` surface), upload session interruptible by sleep/Exit (#31),
  X3 input at interrupt priority with the gauge on its own 30 s task (#36),
  coordinated GPIO3 wake-button handoff (#27 — supersedes the
  `steal_wake_button` pattern C2 cites), catalog-header durability (#32),
  u16 chapter indices (#33), FAT cluster reclaim on delete (#26), UTF-8
  catalog labels (#25), OTA partition-layout fix (#38), web-flasher erase
  guard (#34).

**B4 is implemented but stranded:** `origin/opt/b4-progressive-open`
(5 commits, now 19 behind main) stacks on B6's pre-review commit and
predates both #41's single-transaction open and #29's persistence change.
Treat the branch as a design reference, not a rebase candidate — rework the
design against the storage-owned open transaction before re-implementing
(issue 02).

**Upstream sweep checkpoint (2026-07-25, crosspoint `d0359edf` / freeink
`e62f6c1`):** #37 and #42 above were the ports taken. Still open from the
sweep: strongest-AP join for duplicate SSIDs (crosspoint `e03aa163`) —
folded into D4's scope in issue 04. The UC8279d X3 panel-variant port is
deliberately deferred until upstream hardware-validates its driver
(hardware enablement, not an optimization item — tracked outside this
roadmap). Grayscale LUTs were assessed and rejected (no 2-bpp consumer, RAM
budget, UC8279 can't use them).

**Ranking directive (owner, 2026-07-25): prioritize the portrait reading
experience.** Portrait is the default reading orientation and the shell is
portrait-pinned, so nearly every frame the device draws goes through the
per-pixel portrait path — the #24 fast paths only fire when a user manually
holds landscape. That inverts A-item priorities: the portrait rasterizer is
now the top of the queue.

**Next queue (re-ranked 2026-07-25):**

1. **NEW: A2-P** — portrait byte-run/strided fast paths (issue 01). Extend
   `fill_span`/`blit_row` past their per-pixel Portrait arm
   (`display/src/fb.rs:132,181`). Portrait rows are native columns, so the
   design differs from #24 — candidate shapes in issue 01. Goldens must
   pass unchanged; the equivalence-test harness already enumerates the
   Portrait frame. The single highest-leverage portrait item.
2. **Device bench session** — cheap, unblocks several verdicts at once:
   portrait `page-turn` baseline (`layout_ms` has never been measured in
   portrait; the 19–22 ms envelope below is landscape), A2 landscape A/B
   (#24), B6 replay A/B (#23 — decides B7), #42 SPI check on both panels.
   Run before or alongside A2-P; A2-P's win is measured against this
   baseline.
3. **A3** — panel-native framebuffer byte order (issue 01). Its "coordinate
   with the portrait work" blocker is resolved; design the map-folding
   together with A2-P so the index math is written once, land after it
   (A2-P is goldens-unchanged, A3 re-blesses deliberately). ~10–13 ms per
   turn and prestage on the default path; frees 8 KB.
4. **B4 rework** — progressive first open (issue 02). Time-to-first-page on
   the default reading flow. Redesign against #41's storage-owned open
   transaction; the stranded branch is reference only.
5. **C2** — deep-sleep GPIO hold + first-ever sleep-current measurement
   (issue 03). Top non-reading item, unchanged. #27 reworked the
   wake-button handoff the plan cites — follow the new pattern.
6. **D4** — directed Wi-Fi join (issue 04), now also carrying strongest-AP
   join for duplicate SSIDs (crosspoint `e03aa163`). New WIFI.BIN fields
   ride the `proto::durable` framing from #29.
7. **Upload-ceiling investigation** (issue 04) — unchanged; #31's
   stop-channel session lifecycle is new context.
8. **D5** — portal → station handoff (issue 04). #35 rewrote the portal
   surface — rebase the design on it; verify-after-save must survive the
   handoff.
9. **B7** — per-config section caches (issue 02), still conditional: decide
   from B6's device A/B, don't schedule independently.

Tier 3 unchanged: A4, A5, C6, E4, F5, D6 (read its evidence file in issue
04 first), B7 (conditional, above).

**Pending on-device measurements (single bench session covers the first
four):** portrait `page-turn` baseline; A2 landscape `layout_ms` A/B (#24);
B6 settings-change replay A/B + corrupt-CONT.BIN fallback (#23); #42 SPI
clocks on both panels; portrait wake-restore fallback (reachable only from
a corrupt orientation byte — flagged untested in #5). C2 additionally needs
the µA meter.

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
  **Stale as of 2026-07-25:** these numbers predate #42 (display SPI 20 MHz
  — X3 plane writes ~25% faster) and the portrait default (#5) — the layout
  figure is landscape; portrait `layout_ms` has never been measured. The
  round-2 bench session re-baselines.
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
