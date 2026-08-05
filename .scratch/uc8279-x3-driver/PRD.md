# UC8279 display driver for X3 (newer production panels)

Status: ready-for-human

## Problem

The UC8279d replaces UC8253 on newer X3 production units. Same 792×528 glass and pinout, but different init, LUT format (43 bytes command-prefixed vs UC8253's 42), and some units ship with blank MTP requiring external LUTs (PSR REG=1).

## Context

Init sequence from freeink `Uc8279X3Luts.h`:

- PSR 0x3F 0x4A (REG=1 external LUT)
- PTIN 0x91
- PTL 0x90 (792×528 window)
- PFS 0x20
- PWR 0x43 0x00 0x78 0x78 0x17
- VDCS 0x24
- BTST 0x25 0x25 0x3C (3 bytes not 4)
- PLL 0x0F
- GATE 0x02

CDI model: 1 byte (0x97 first refresh, 0xD7 subsequent) instead of UC8253's 2-byte CDI.

LUT banks from `Uc8279X3Luts.h`:

- BwGc (full refresh)
- BwDu (fast refresh)
- XtfPreBwMid (scrub)

Each LUT is 43 bytes (command-prefixed). Same two-phase BUSY as UC8253.

## Scope

### Files

- **[NEW]** `display/src/epd/uc8279.rs` — command set, init, LUT banks (43 bytes each), constants
- **[NEW]** `fw/src/display_flush/uc8279.rs` — async flush backend
- **[MODIFY]** `display/src/epd/mod.rs` — add `uc8279` module (compiled for X3 builds)

### Dependencies

- Depends on: `panel-controller-detection` (dispatch layer must exist to route to this backend)

### Notes

- No UC8279 hardware available for testing.
- freeink-sdk docs note this driver is itself "Pending hardware validation."
- Requires bench verification on a UC8279 X3 unit before shipping.

## Done when

- `display/src/epd/uc8279.rs` contains the full command set, init, LUT banks (43-byte format), and constants matching the freeink reference.
- `fw/src/display_flush/uc8279.rs` implements the async flush backend.
- The module compiles in X3 builds without warnings.
- `tools/check.sh fast` passes.
- Bench-verified on a UC8279 X3 unit (blocks shipping, not development).
