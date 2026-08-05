# Runtime display controller detection

Status: ready-for-agent

## Problem

Newer Xteink production units ship different panel controllers (X3: UC8279d instead of UC8253, X4: UC8179 instead of SSD1677). Users cannot externally identify their controller variant, so detection must happen at runtime.

## Context

The UC81xx controllers expose version registers that can be read over the data bus before SPI peripheral init. A GPIO bit-bang probe can read UC81xx VER (0x70), FLG (0x71), and RMTP (0xA2) registers at boot. SSD1677 and UC8253 do not respond to this probe, so a timeout-based fallback classifies them as the default controller for the device.

The probe result is stored in a static `AtomicU8` for the flush module to dispatch on. The dispatch layer in `fw/src/display_flush/mod.rs` routes `init_panel`, `flush`, `prestage_previous`, and `sleep_panel` to the detected controller's backend.

## Scope

### Files

- **[NEW]** `hal-ext/src/epd_probe.rs` — GPIO bit-bang probe, `ProbeVerdict` enum
- **[MODIFY]** `hal-ext/src/lib.rs` — export `epd_probe` module
- **[MODIFY]** `fw/src/main.rs` — run probe after GPIO init, before SPI2 config
- **[MODIFY]** `fw/src/display_flush/mod.rs` — runtime dispatch based on `DetectedController`

### Dependencies

- Blocks: `uc8179-x4-driver`, `uc8279-x3-driver` (those drivers need this dispatch layer)

### Notes

- Can be developed and tested on existing hardware — probe should return `DefaultAssumed` on SSD1677/UC8253 panels.

## Done when

- `epd_probe.rs` implements the GPIO bit-bang read of VER/FLG/RMTP registers and returns a `ProbeVerdict`.
- The probe runs at boot after GPIO init but before SPI2 peripheral configuration.
- The result is stored in a `static AtomicU8` accessible to the flush module.
- `display_flush/mod.rs` dispatches `init_panel`/`flush`/`prestage_previous`/`sleep_panel` based on the detected controller.
- On existing SSD1677/UC8253 hardware, the probe returns `DefaultAssumed` and the existing driver path is taken with no behavioral change.
- `tools/check.sh fast` passes.
- `tools/check.sh emulator` passes (emulator path unchanged for default controllers).
