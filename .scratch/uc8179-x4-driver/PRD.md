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
- **Carries the deep-sleep RST hold with it, and this is the controller the drain was reported on.** Upstream's field report is a UC8179 pack dead in ~36 h: the active-low RESET pin floats high-Z in deep sleep, drifts, and restarts the controller's explicitly BTST-programmed DC-DC booster, which then drains through "off". The SSD1677 tolerates the same floating pin — no external booster, and its deep sleep actively discharges — which is why this has never bitten our X4 units and why PR #70 closing unmerged carried no consequence. Shipping this driver changes that: it must land with a RST hold. Note that #70's `rst.set_high()` is not the mechanism, since a C3 pad goes high-Z in deep sleep whatever its output level; upstream `0425477` arms a pad hold before sleep and releases it in bus init before the wake reset pulse, and both halves are required — an un-released hold makes the reset pulse bounce off the latch.

### Notes

- No UC8179 hardware available for testing.
- Driver is ported from freeink C++ reference (MIT).
- Requires bench verification on a UC8179 unit before shipping.
- The 800×600 addressing recorded above (600 gates, 480 visible) is upstream `0252c62`, and the `MIRROR_*`/`REVERSE_BITS` transforms are `92303ba`. Both are hardware-found corrections rather than datasheet readings, so treat them as pinned constants and do not "simplify" them during the port.
- Upstream has since added a partial-refresh path (`84f6bab`) and a two-phase reset-then-set grey waveform (`e52d480`) for this controller. Neither is in scope: the PTIN/PTOUT fast path above already covers what our refresh planner asks for, and grey levels have no consumer here.

## Done when

- `display/src/epd/uc8179.rs` contains the full command set, init sequence, constants, and pixel transforms matching the freeink reference.
- `fw/src/display_flush/uc8179.rs` implements the async flush backend following the existing `FlushStep` pattern.
- The module compiles in X4 builds without warnings.
- `tools/check.sh fast` passes.
- Bench-verified on a UC8179 X4 unit (blocks shipping, not development).
- If the driver ships, the deep-sleep RST hold ships with it: armed before sleep, released before the wake reset pulse, and confirmed by a standby-draw measurement rather than by inspection.

## Comments

**2026-08-06** — Reviewed against the crosspoint/freeink upstream sweep (freeink
`e62f6c1..8b8337b`). The init and refresh sequences here are current; the UC8179
commits in that window were the 800×600 addressing fix, the row-flip transform, a
partial-refresh path and a grey-waveform reset phase, and the first two are
already reflected in Context. Added the provenance note so those constants are not
mistaken for arbitrary, and scoped the latter two out. The substantive addition is
the deep-sleep RST hold: this is the controller upstream's ~36 h battery-drain
field report names, so the dependency belongs on this PRD even though PR #70 was
closed unmerged for the SSD1677 units we actually ship.

**2026-08-07** — The dispatch dependency is satisfied:
`panel-controller-detection` is implemented on
`feature/runtime-display-controller-detection` (58f5fa0). `fw::display_flush`
routes the four panel operations through a `DetectedController` read from the
boot probe, and the arm this backend plugs into is marked in the source. Until
it lands, a confirmed UC8179 runs the SSD1677 backend — unchanged behaviour,
not a dark panel.

The probe was bench-validated on an X3 only; no X4 hardware exists here, so the
SSD1677 side of the negative path is host-verified rather than measured. Two
details from that work matter to this port. The X4 build deliberately does
*not* compile the 50 ms reset escalation (the UC8179 is bench-proven upstream
to answer the 1 ms screening pulse), so if a real UC8179 ever fails to be
detected, that asymmetry is the first thing to revisit. And `VER` byte 2
(`LUT_VER`) is captured in the probe's diagnostics and written to
`/XTEINK/PROBE.TXT`: it is what separates a UC8179 (`0x01`) from a UC8279 in
X4 clothing (`0x02`/`0x68`/`0x69`), which is the discriminator this PRD will
need if X4 units turn out to carry either.
