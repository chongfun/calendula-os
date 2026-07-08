# esp-hal 1.x / Stable-Rust Migration Plan

Status: **in progress** (updated 2026-07-07 evening). The dependency
migration itself has landed (f47eb88 plus the stack-fix follow-ups
below, carried on `development`); the tree builds and passes the host
net on stable 1.96.1 and the nightly pin is gone from
rust-toolchain.toml. What remains is hardware verification (reader,
sync, OTA) and CI/toolchain hygiene — see "Remaining phases".

Original goal: retire the pinned nightly (`nightly-2025-10-01`) by
migrating the esp-rs stack from the 0.23-era crates to the 1.x-era
crates, all of which build on **stable Rust** (highest MSRV in the set:
1.88, esp-radio). Chosen over a nightly re-pin deliberately: the only
nightly feature the tree used was `#![feature(impl_trait_in_assoc_type)]`
(fw/src/main.rs:3), demanded by embassy-executor 0.7's `nightly` feature,
and esp-hal 0.23 is an unsupported dead branch.

## State as of 2026-07-07

Landed, and what it cost:

- f47eb88 migrated the dependency set per the map below (esp-hal 1.1.1,
  esp-rtos 0.3.0, esp-radio 1.0.0-beta.0, embassy-executor 0.10, stable
  toolchain, `#![feature]` line removed).
- **The landing was not clean.** The new stack grew `.data` plus the IRAM
  shadow (`.rwdata_dummy`) by ~13 KB, squeezing the main stack — wedged
  between `_bss_end` and the dram2_seg boundary — from 27,380 to
  14,496 bytes. The reader's ~28 KB deep path overflowed into the top of
  .bss: first corrupting embassy channel state (BorrowMutError panic in
  embassy-sync channel.rs while reading), then, after a rebuild shuffled
  the layout, the SPI DMA descriptors (silent lockup six spines into a
  V2 cache build). Fixed by:
  - 3e48f87 — dram2 rebalance: `fw/build.rs` emits `ram-layout.x`, which
    pins the prev-frame framebuffer to the top of dram2_seg and raises
    `_stack_start` over the radio's former dram2 heap share; a link-time
    ASSERT now fails any build whose stack drops under 27 KB.
  - 33bd8cb — SPI RX DMA buffer 8000 → 64 bytes (the SD bounce chunk;
    the EPD side is write-only), freeing another 7.9 KB into the stack.
  - Stacks now: **X3 36,504 B, X4 45,696 B**, inside the 30-43 KB
    EPUB-chain budget documented in the workspace Cargo.toml.
  - cd67219 reverted an interim vendored embassy-sync "deferred wake"
    patch that had only masked the symptom by moving the corruption
    target.
- The `ESP_WIFI_CONFIG_*` env tuning was silently dead — esp-radio
  dropped those compile-time options. 5a8ef74 rebuilt the trims as a
  runtime `ControllerConfig` (static RX 4, dynamic RX/TX 8/8, AMPDU off)
  in the wifi task, which also offsets the sync heap's loss of the dram2
  share.
- **App descriptor:** the hand-rolled `EspAppDesc` at `.rodata_desc` /
  image offset 0x20 was kept as-is; `esp-bootloader-esp-idf` was *not*
  adopted and the runner still passes `--ignore-app-descriptor`. Stock
  bootloader compatibility of new-toolchain images is therefore still
  unverified (phase R3).
- Old risk 4 (embassy task-arena sizing) is moot: embassy-executor 0.10
  on stable allocates tasks as per-task statics; there is no arena.
- The `--cfg portable_atomic_unsafe_assume_single_core` rustflag question
  (see "Unaffected" below) is still open: the flag coexists with the
  crate feature, redundant but harmless. Cleanup candidate for R4.
- The exact-pin advice in the dependency map was not followed: esp-hal /
  esp-rtos / esp-radio use caret requirements in fw and hal-ext. R4.

Verified on stable (2026-07-07):

- Phase 1: both feature builds plus `test_dma` compile. Clippy is *not*
  `-D warnings` clean — one warning (`manual_is_multiple_of`,
  fw/src/tasks/input.rs:246). R4.
- Phase 2 host net: green — 119 host tests (app-core 35 / proto 73 /
  ui 11), emulator golden frames match for both panels, and the wasm
  web-emulator builds.

Adjacent open issue (not migration-caused, but it shapes R1 testing):
the EPUB 3 nav-TOC filtering commits (238780d, de61873) cost Waybound
its 47-chapter TOC ("toc unavailable", 0 items) and invalidated existing
book caches, so first opens after flashing rebuild the cache — which
conveniently exercises the cold path.

## Ecosystem facts (checked 2026-07-07)

- esp-hal 1.0.0 released 2025-10-30; current is **1.1.1** (2026-05-07). Builds
  on stable; peripherals beyond the stabilized core (DMA, ADC, RTC control —
  all used here) sit behind the `unstable` *cargo feature*, which is an API
  stability marker, not a nightly-rustc requirement.
- **esp-wifi is replaced by esp-radio** (0.18.0, 2026-04-16; a 1.0.0-beta.0
  exists, 2026-06-03). MSRV 1.88, stable. Requires esp-hal's `unstable`
  feature.
- **esp-hal-embassy is replaced by esp-rtos** (0.3.x): the scheduler esp-radio
  requires, plus the embassy executor/time-driver integration, in one crate.
- espflash v4 pairs with the 1.x line; app images are expected to carry an
  app descriptor emitted via the new `esp-bootloader-esp-idf` crate's
  `esp_app_desc!()`.

## Dependency map (fw/Cargo.toml)

| Now | Target | Notes |
| --- | --- | --- |
| esp-hal 0.23.1 | 1.1.x, features `esp32c3`, `unstable` | Pin exactly (`=1.1.1`): APIs under `unstable` may move between minors. |
| esp-hal-embassy 0.6 | **removed** → esp-rtos 0.3.x (`esp32c3`, embassy feature) | `esp_hal_embassy::init` (fw/src/main.rs:150) becomes the esp-rtos start call; it needs a timer and must run before radio init. |
| esp-wifi 0.12 | **removed** → esp-radio (0.18.x or 1.0.0-beta.x — pick whichever pairs with the esp-hal pin) | fw/src/tasks/wifi.rs:31-114: `esp_wifi::init`, `EspWifiController`, `new_with_mode` STA/AP all change shape. |
| embassy-executor 0.7 + `nightly` | same crate, `nightly` dropped (version per esp-rtos) | Task storage moves from TAIT statics to the arena. 7 task definitions, 5 spawned concurrently from main.rs — size the arena from measured task sizes plus margin, and account for it in the RAM budget. |
| embassy-time 0.4 / embassy-sync 0.6 / embassy-net 0.6 | whatever esp-radio/esp-rtos pair with | embassy-net drives the kosync portal (tasks/wifi.rs). |
| esp-println 0.13 / esp-backtrace 0.15 / esp-alloc 0.6 / esp-storage 0.3.1 | contemporaries of esp-hal 1.1 | esp-storage backs OTA flash writes (fw/src/ota_update.rs:123+). |
| — | **new**: esp-bootloader-esp-idf | `esp_app_desc!()`; reconcile with the existing descriptor-at-0x20 arrangement and the `--ignore-app-descriptor` runner flag in `.cargo/config.toml`. |
| rust-toolchain.toml nightly-2025-10-01 | pinned current **stable** (≥1.88), riscv32imc + wasm32 targets | Also update mise.local.toml and both workflow files (the nightly date string is hardcoded in pages.yml and release.yml). Remove the `#![feature]` line. |

Unaffected: embedded-sdmmc (git pin), heapless, static_cell, portable-atomic
(but check whether esp-hal 1.x still wants the
`--cfg portable_atomic_unsafe_assume_single_core` rustflag or now owns that
setup), and every host-side crate (app-core, proto, ui, display, emulator
tools) — none of them touch esp-hal, so goldens and host tests are the
regression net that must stay green untouched.

## Repo-specific hot spots (where the churn lands)

1. **fw/src/main.rs** (272 lines): `esp_hal::init`, clock config,
   `dma_buffers!(8000)` + `DmaRxBuf`/`DmaTxBuf` (SD SPI), executor init,
   GPIO/peripheral handoff to tasks.
2. **hal-ext/src/spi_dma.rs** (136 lines) + **fw/src/display_flush/**: the
   custom display SPI DMA path. The DMA and SPI type-state APIs were rewritten
   across 0.23→1.0. Both panel drivers (SSD1677 X4, UC8253 X3 at 16 MHz) ride
   on this; panel timing regressions are the thing to watch.
3. **fw/src/tasks/wifi.rs** (868 lines) + **fw/src/sync_mem.rs**: the deepest
   risk. The sync session donates dismantled reader-scratch statics plus dram2
   to the esp-alloc heap as the radio's memory (`SyncLoan`,
   `donate_heap`), sized against esp-wifi 0.12's demands and the
   `ESP_WIFI_CONFIG_*` env tuning in `.cargo/config.toml`. esp-radio + esp-rtos
   have a different memory model (esp-rtos brings its own task/scheduler
   storage) and renamed config env vars. **Re-audit the whole loan accounting**;
   the dram2 heap was already trimmed 16→13 KB for the X3 framebuffer, so
   there is little slack.
4. **fw/src/tasks/power.rs** + **hal-ext/src/rtc.rs**: rtc_cntl deep
   sleep/wake (`peripherals.LPWR`).
5. **fw/src/tasks/input.rs**: ADC battery read (X4 aux ADC path), async GPIO.
6. **fw/src/ota_update.rs**: esp-storage `FlashStorage` API, plus the image
   format question. This firmware writes the inactive OTA slot and flips
   otadata under the **stock vendor bootloader** (dual-OTA layout,
   partitions.csv). The new app-descriptor arrangement must produce images the
   stock bootloader still boots, for both the espflash dev path and the SD
   `FWUPDATE.BIN`/`FWUPDX3.BIN` update path. Treat as brick-risk;
   USB reflash via the stock bootloader is the recovery (docs/FLASHING.md).
7. **fw/src/bin/test_dma.rs**: port or delete.

## Remaining phases

Constraint: no pre-migration timing logs exist, and flashing an old build
to collect a baseline is out of scope. R1 therefore checks **absolute**
sanity of the numbers, not before/after deltas.

R1. **X3 bring-up on current HEAD** (the daily device; original phases
    3 and 6, merged and narrowed).
    - ~~Cold open of a large EPUB (Waybound, 48 spines, 580 KB): the V2
      cache build must run through every spine and land on the page —
      this is exactly the path that hung at part0009 pre-fix.~~
      **Verified on device 2026-07-07**: cold open succeeded, all
      spines loaded. Warm reopen via `TryV2BookIndexFast` still to
      confirm.
    - Absolute guardrails from the serial bench lines, no baseline
      needed: Fast page-turn flush (incl. the UC8253 DRF busy wait) in
      the sub-second band the logs already show (~450 ms); layout in the
      tens of ms; zero `display: SPI transfer failed` lines. Wildly
      outside those bands, or any SPI error, implicates the SPI/DMA port
      or the 64-byte RX buffer change.
    - Visual pass: full refresh clean; fast refresh free of *new*
      ghosting or banding — the prestage/prev-fb compare path moved to
      the top of dram2, and banding artifacts would implicate that move.
    - Buttons and battery: paging responsive once the build settles
      (input shares the thread executor on X3, so button deafness
      *during* a cold build is the current design, not a regression);
      gauge percentage plausible with no BQ27220 I2C timeout spam.
    - Auto-sleep after the idle timeout; button wake.
R2. **Wi-Fi sync session on X3.** The top remaining functional risk:
    the donated heap lost its 13 KB dram2 share (offset by the runtime
    trims, but the watermark has never been measured under
    esp-radio/esp-rtos, whose wifi-task stack also comes out of that
    heap). Portal AP mode, STA join, kosync exchange; watch serial for
    esp-alloc allocation failures. If it OOMs: re-tune the trims and
    `SyncLoan` sizes; worst case the sync budget gets redesigned around
    esp-rtos (original risk 1).
R3. **OTA — brick-risk.** `ota-selftest` build on device; confirm the
    stock vendor bootloader boots **both** slots with stable-toolchain
    images (the hand-rolled descriptor is unchanged, but every other
    byte of the image is new); then the SD `FWUPDATE.BIN`/`FWUPDX3.BIN`
    path end-to-end. USB reflash via the stock bootloader is the
    recovery (docs/FLASHING.md).
R4. **CI + toolchain hygiene.**
    - pages.yml and release.yml still install and build with
      `nightly-2025-10-01`; release artifacts currently target the dead
      toolchain and may not even build against the refreshed lock.
      Point both at the stable pin.
    - Fix the one clippy warning (input.rs:246) so phase 1's
      `-D warnings` bar actually holds, and turn it on in CI.
    - Pin esp-hal / esp-rtos / esp-radio exactly (`=1.1.1` etc.) in fw
      and hal-ext, per the dependency-map note — `unstable`-feature APIs
      can move between minors.
    - Optional: drop the redundant
      `--cfg portable_atomic_unsafe_assume_single_core` rustflag (the
      cargo feature covers it) and verify with a scratch build.
R5. **X4 hardware pass**, hardware permitting: the full original
    checklist on the other board — SSD1677 panel timing, aux-ADC battery
    path, buttons, sleep/wake, and its SD-update trigger (also still
    untested on X3).

## Risks, re-ranked (remaining)

1. Sync-session heap watermark under the esp-radio/esp-rtos memory model
   with the smaller donation (R2).
2. OTA image compatibility with the stock bootloader (R3).
3. Release CI shipping from the dead nightly until R4 lands.
4. Display timing: mostly retired — daily X3 use shows sane Fast-refresh
   numbers — fully retired after R1's full-refresh and visual checks.
5. Future churn: `unstable`-feature APIs can move between esp-hal 1.x
   minors; retire by pinning exactly in R4.

Realized risk, for the record: the RAM-budget squeeze (a sharper version
of old risk 4) shipped as silent .bss corruption. The guard against
recurrence is structural — the link-time stack ASSERT from 3e48f87 —
plus `docs/brainstorms/2026-07-07-stack-headroom-options.md` for the
next ~2-4 KB if the budget tightens again.
