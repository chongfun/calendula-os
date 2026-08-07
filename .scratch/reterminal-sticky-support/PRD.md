# reTerminal Sticky (ESP32-S3) support

Status: ready-for-human

## Problem

CalendulaOS only targets the ESP32-C3 Xteink X3/X4. The Seeed reTerminal Sticky
uses an ESP32-S3 and a different board architecture, so it cannot be supported by
adding another panel driver or pin table — the approach the UC8179/UC8279 PRDs
take, which works precisely because those variants keep the same C3 board and
change only the controller. The firmware build, toolchain, linker layout, atomics
assumption, board initialization, input model, OTA identity, and sleep path all
contain C3/Xteink assumptions.

The Sticky otherwise matches several existing Calendula primitives: its 800×480
panel uses SSD1677, its MicroSD shares the display SPI bus, and its battery gauge
is a BQ27220. FreeInk SDK and CrossPoint v1.5 both ship working reference
implementations for the board.

## Approach

Add ESP32-S3 as a second compile-time MCU target, then move device-specific
peripheral construction behind a board module. Keep X3/X4 on the existing
ESP32-C3 binary and build Sticky as a separate ESP32-S3 binary; the two never
share one image.

Reuse the existing SSD1677 driver, the shared EPD/SD SPI bus, the reader and
application code, and the BQ27220 driver. Add a GT911 input backend that feeds
the existing `app_core::Button` actions.

Deliver as four dependent, individually mergeable milestones so each hard
dimension is isolated. If the S3 linker explodes you are not also debugging
GT911; if touch feels wrong, display/SD/power are already settled.

## Context

**What the Sticky shares with Calendula today.** 800×480 SSD1677 (the same
controller and geometry as the X4, and FreeInk drives it with the same driver);
MicroSD over SPI on the display's bus, which is the architecture
`fw/src/sd_session.rs` already implements (one DMA bus, per-device CS, SD
retunes the clock between identification and data speed); a BQ27220 fuel gauge,
already supported for the X3 in `hal-ext/src/bq27220.rs`.

**What it does not share.** ESP32-S3/Xtensa instead of ESP32-C3/RISC-V, which
changes the Rust target, the toolchain, the linker layout, the deep-sleep wake
mechanism, the panic/backtrace path, and the atomics story. GT911 capacitive
touch instead of the two ADC resistor ladders. Two digital nav buttons plus a
**shared OK/power button on one GPIO** — click confirms, hold sleeps. Three
switched peripheral power rails (EPD, SD, touch) that X3/X4 do not have. A
PWR_HOLD/PWR_LOCK power latch the firmware must assert at boot or the board cuts
its own power when the button is released. A different pinout throughout.

**Four C3 assumptions that are not obvious from a pin table**, each verified in
this tree:

1. `fw/build.rs` packs the previous-frame framebuffer into `dram2_seg` and
   rewrites `_stack_start`/`_stack_start_cpu0` using the C3 linker layout, with a
   27 KB minimum-stack `ASSERT`. That is an optimization worth keeping on X3/X4
   and meaningless on the S3.
2. `portable-atomic`'s `unsafe-assume-single-core` is enabled in
   `fw/Cargo.toml`. The C3 is single-core; the S3 is not.
3. `fw/src/mmu.rs` and `proto::ota` are C3-specific in named constants —
   `MMU_TABLE = 0x600C_5000`, `MMU_ENTRY_COUNT = 128`, and
   `EXPECTED_CHIP_ID = 5` (the C3's ESP image chip id; the S3's is 9). OTA slot
   detection and image validation cannot be carried over untested.
4. `hal-ext/src/rtc.rs` uses `RtcioWakeupSource` and documents the C3's
   RTC-capable GPIO 0–5 constraint. The S3 wakes from deep sleep through RTC
   `ext1`, which FreeInk's `consumer-mcu-portability.md` calls out as exactly
   this C3→S3 problem.

**One correction to "reuse SSD1677 unchanged."** Calendula's
`display/src/epd/ssd1677.rs` hardcodes `RefreshMode::Fast => value | 0x1C`, the
incremental differential-update path. FreeInk's `ssd1677StickyConfig()` states
that on this panel `0x1C` does *not* select the partial/DU waveform — it silently
promotes to the full OTP waveform, ~1.7 s per refresh, which its comment calls
"UI unusably slow." The Sticky needs Seeed's absolute sequences (`0x22` = `0xF7`
full, `0xFF` fast) and a per-refresh border waveform re-write (init/full `0x01`,
fast `0x80`) or partial refreshes leave a black edge ring. This is a small
per-board waveform config seam in the existing driver, not a new driver — but
"unchanged" is wrong, and getting it wrong looks like working-but-slow hardware
rather than a failure.

**One simplification.** `app_core::Button` is already semantic —
`Power`/`Back`/`Confirm`/`Previous`/`Next`/`PagePrevious`/`PageNext` — and the
physical-to-semantic map already lives in `fw/src/tasks/input.rs::map_hardware`.
The semantic input seam this port needs mostly exists. What is ADC-shaped is
`InputEvent::Sample`'s `aux_raw`/`nav_raw`/`page_raw` debug fields and the
battery reading that rides on every sample. So touch integration is smaller than
a UI rewrite: a GT911 backend that emits `Button` values and keeps pushing
battery.

**And the Sticky is not touch-only.** It has digital up (GPIO5) and down (GPIO6)
buttons plus the shared confirm/power on GPIO4. That is enough to navigate before
GT911 exists, which is what makes milestone 3 shippable on its own.

## Scope

- Add ESP32-S3 build support on the Xtensa (`espup`) toolchain, selected by
  target triple. The Sticky is 800×480 — the default geometry — so it needs no
  new geometry feature, and board identity is derived from the target rather
  than made independently selectable.
- Preserve the existing ESP32-C3 toolchain, entry points, and X3/X4 binaries
  unchanged; the normal C3 workflow must not start requiring `espup`.
- Give each of `esp-hal`, `esp-rtos`, `esp-radio`, `esp-storage`, `esp-backtrace`
  and `esp-println` exactly one chip feature per build — in every crate that
  depends on them, `hal-ext` included — and make an incompatible device selection
  a compile error.
- Make the C3 DRAM2 framebuffer/stack linker layout conditional on the target
  architecture; start Sticky on the standard S3 internal-RAM layout.
- Remove the S3 build from the `portable-atomic` single-core assumption.
- Extract XTeink peripheral/pin construction from `fw/src/main.rs` into a
  cfg-selected board module returning a common `BoardHardware`, with no
  behavior change on X3/X4.
- Add a Sticky board module using the FreeInk/CrossPoint pinout.
- Assert PWR_HOLD/PWR_LOCK as close to `main()` entry as possible, and manage the
  three switched peripheral rails (EPD/SD/touch).
- Give the SSD1677 driver a per-board waveform/border config and supply Sticky's.
- Reuse the existing shared EPD/SD SPI bus and `sd_session` retuning.
- Generalize BQ27220 support from `#[cfg(feature = "device-x3")]` to a board
  capability.
- Add an S3 `ext1` power-button deep-sleep/wake path preserving today's
  semantics.
- Add a GT911 driver and map touch onto existing `Button` actions.
- Build and exercise Wi-Fi sync on the S3.

## Non-goals

- PDM microphone, PCF8563 RTC, SHT40 environmental sensor, LSM6DS3TR-C IMU,
  buzzer — each becomes an independent capability PRD later.
- PSRAM. FreeInk deliberately keeps the Sticky framebuffer in internal DRAM even
  though the module has PSRAM: the buffers fit and internal RAM is faster. Get a
  boring, correct S3 build first.
- OTA on the Sticky. `mmu.rs` and `proto::ota` are C3-validated; the Sticky is a
  USB-flashed development device for now. Exclude the path at compile time rather
  than shipping an untested one.
- Redesigning Calendula's UI around direct touch hit-testing. CrossPoint has gone
  further — coordinate transformation plus real UI hit testing — and that is the
  right destination, but it must not block initial Sticky support.
- The runtime UC81xx controller probe. That solves XTeink batch variation; there
  is no evidence the Sticky ships multiple controllers.
- Xteink X4 Pro (also ESP32-S3 + SSD1677 + GT911, with a FreeInk profile). The
  board seam should make it cheap later; it is not in this work.

## Implementation order

Four dependent milestones, one branch each:

1. **[Multi-MCU build foundation](issues/01-multi-mcu-build-foundation.md)** —
   S3 dependency selection by target table, toolchain/check entry points,
   target-conditional linker strategy, atomics audit, chip-neutral compilation
   fixes. No Sticky pins, no peripheral functionality.
2. **[Board-profile extraction](issues/02-board-profile-extraction.md)** —
   `fw/src/board/`, mechanical XTeink extraction, `BoardHardware` seam, board
   capabilities for battery and power/wake. No behavior change on X3/X4.
3. **[Sticky hardware bring-up](issues/03-sticky-hardware-bringup.md)** —
   `board/sticky.rs`, power latch and rails, SSD1677 waveform config, shared-bus
   MicroSD, BQ27220, S3 sleep/wake, Wi-Fi. Usable on its digital buttons.
4. **[GT911 input integration](issues/04-gt911-input-integration.md)** — GT911
   driver, touch-to-action mapping, coordinate/orientation validation, idle-timer
   integration.

Hardware soak and sleep/wake validation runs against milestones 3 and 4 and
gates the umbrella, not the individual merges.

## Validation ladder

Hardware-driven, unlike the UC8179/UC8279 PRDs — the owner has a Sticky. Do not
advance a stage until its predecessors pass.

1. Cold boot from battery with USB disconnected; the power latch holds.
2. Flash/serial diagnostic loop reliable across repeated resets.
3. Full black/white/checkerboard refresh, plus existing fast/partial refreshes,
   with correct orientation — and a **measured** fast-refresh time, to prove the
   panel took the DU waveform rather than promoting to full.
4. Mount SD and read known files.
5. Hundreds of alternating SD-read / display-flush cycles; neither peripheral
   ever responds while the other's CS is asserted.
6. Repeated plausible BQ27220 readings, and cold boots that stay reliable despite
   GPIO0's strapping role.
7. Wi-Fi connects and runs the same sync path as X3/X4.
8. Every existing screen navigable — on digital buttons after milestone 3, and
   through GT911 after milestone 4.
9. Representative EPUBs open; page forward/back; image-heavy books;
   settings/library transitions.
10. At least 50 sleep → power-button-wake cycles, distinguishing genuine wakeups
    from resets and brownouts.
11. Unplugged run long enough to catch an S3 standby-power regression, including
    the switched rails actually being off in deep sleep.
12. All normal X3/X4 checks re-run, plus an X3 hardware smoke test, so the board
    extraction is demonstrated not to have regressed existing hardware.

Record memory high-water marks and stack-frame checks **separately for C3 and
S3**. The existing C3 numbers — `fw/build.rs`'s 27 KB `MIN_STACK_BYTES` floor and
`tools/check.sh stack-frames`' 24 KB per-frame bound — are derived from the C3
layout and stop meaning anything once the memory architecture changes.

## Risks, in order

1. **S3 linker/memory transition.** Isolated deliberately into milestone 1.
2. **The single-core assumption.** Silent and memory-corrupting if carried over.
3. **S3 sleep/wake and the power latch.** A latch mistake powers the board off
   mid-session; a wake mistake makes sleep terminal in the wrong sense.
4. **GT911 and UI integration.** Tunable on hardware, and last.

SSD1677 and SD are comparatively low-risk given the waveform caveat above.
CrossPoint v1.5 already runs a full reader stack on an S3 Sticky, so little
hardware discovery remains; this work is mostly about separating Calendula's C3
assumptions cleanly enough to exploit known-good hardware.

## References

- FreeInk SDK `libs/hardware/BoardConfig/include/BoardConfig.h` — the `STICKY`
  board profile (pins triple-sourced from the V01 schematic, the porting spec,
  and Seeed's demo `pin_config.h`), `holdPowerRails()`, `releaseSdRail()`.
- FreeInk SDK `libs/display/FreeInkDisplay/src/driver/Ssd1677Driver.{h,cpp}` —
  `ssd1677StickyConfig()` and the `Ssd1677Config` seam.
- FreeInk SDK `libs/hardware/InputManager/src/InputManager.cpp` —
  `beginGt911()` reset/address dance, `pollGt911()` frame handling.
- FreeInk SDK `libs/hardware/PowerManager/` — the `ext1` vs `gpio` wake branch
  and `powerDownRailsForSleep()`.
- FreeInk SDK `docs/consumer-mcu-portability.md` — the three C3→S3 patterns.
- CrossPoint Reader v1.5 `platformio.ini` `[env:sticky]`.
- Seeed reTerminal Sticky schematic/pinout (V01, 2026-06-05).
