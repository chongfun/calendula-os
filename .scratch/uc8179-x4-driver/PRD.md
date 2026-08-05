# UC8179 display driver for X4 (newer production panels)

Status: ready-for-human

## Problem

The UC8179 (UltraChip) replaces SSD1677 on newer X4 production units. Same 800×480 glass and pinout, completely different protocol: DTM1/DTM2 RAM model (not BW/RED), CDI+TSSET refresh control (not CTRL2), two-phase BUSY (not single-phase), addressed as 800×600 (600 gates, 480 visible). Uses OTP waveforms with TSSET frame-rate lever (0x1E full, 0x5A fast).

## Context

Init sequence from freeink `Uc8179Driver.cpp`:

- PSR 0x3F 0x0A
- TRES 800×600
- GSST zeros
- PFS 0x20
- BTST 0x25 0x25 0x3C 0x25
- GATE 0x02

Refresh sequence:

- CDI 0x29/0x07 during refresh, 0xA9/0x07 at idle
- CCSET 0x02
- TSSET 0x1E (full) or 0x5A (fast)
- PSR 0x1F (OTP mode)
- PON → PTIN (fast) → DRF → wait → PTOUT (fast)

Deep sleep: 0x07 + 0xA5 check byte.

Transform constants: `MIRROR_X=true`, `MIRROR_Y=true`, `REVERSE_BITS=true`.

## Scope

### Files

- **[NEW]** `display/src/epd/uc8179.rs` — command set, init sequence, constants, transforms
- **[NEW]** `fw/src/display_flush/uc8179.rs` — async flush backend with `FlushStep` pattern
- **[MODIFY]** `display/src/epd/mod.rs` — add `uc8179` module (compiled for X4 builds)

### Dependencies

- Depends on: `panel-controller-detection` (dispatch layer must exist to route to this backend)

### Notes

- No UC8179 hardware available for testing.
- Driver is ported from freeink C++ reference (MIT).
- Requires bench verification on a UC8179 unit before shipping.

## Done when

- `display/src/epd/uc8179.rs` contains the full command set, init sequence, constants, and pixel transforms matching the freeink reference.
- `fw/src/display_flush/uc8179.rs` implements the async flush backend following the existing `FlushStep` pattern.
- The module compiles in X4 builds without warnings.
- `tools/check.sh fast` passes.
- Bench-verified on a UC8179 X4 unit (blocks shipping, not development).
