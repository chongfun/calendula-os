# UC8279 display driver for X3 (newer production panels)

Status: ready-for-human

## Problem

The UC8279d replaces UC8253 on newer X3 production units. Same 792×528 glass and pinout, but different init, LUT format (43 bytes command-prefixed vs UC8253's 42), and some units ship with blank MTP requiring external LUTs (PSR REG=1).

External LUTs are **mandatory, not an optimisation**. These modules ship a blank MTP — address 0x000 is not 0xA5, so there are no factory command defaults and no per-temperature waveforms — and the host must therefore drive everything: PSR, the drive voltages (PWR/VDCS), booster, PLL, the full-panel PTL window in place of TRES, and every waveform bank. An OTP-mode driver runs the panel with no drive rails and leaves it **completely dark**, which is the failure upstream saw on the first UC8279d field units.

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

The whole register recipe and every waveform bank were reverse-engineered from the stock X3 firmware (`update.bin`), not read off the datasheet. Upstream's comments cite the stock function addresses for each path, which is why the sequencing rules below are stated as facts about the panel rather than as style preferences.

### Load-bearing sequencing rules

Four details in the upstream driver are not derivable from the command set, and each has a failure mode that does not look like its cause:

1. **PTOUT closes *after* the plane write, not before the refresh.** Both plane writes must happen inside the same PTIN window that init's PTL established. DTM2 in the refresh path is windowed to 792×528 addressing, so the DTM1 sync that follows the refresh must use that same window or the two planes misalign and the DU diff drives garbage — the new frame simply never appears. A full-frame GC hides the bug completely, because it flashes every pixel regardless of the diff, so this presents as "fast refresh is broken, full refresh is fine" (upstream `468e341`).

2. **GC vs DU is *only* a waveform-bank choice.** Both modes diff the new frame against the real previous frame held in DTM1; neither seeds a white baseline. Forcing DTM1 white to get a "clean" full refresh is the intuitive move and it is wrong: a pixel that is black in the splash and white in home then reads old == new == white, which selects the WW transition, which applies no drive — and the splash ghosts through. Use GC for Full/Half, the first paint, and a forced resync; DU only for a Fast request that has a valid baseline.

3. **Spend a small initial-full budget after boot.** Upstream forces the first two content paints after boot to GC, because the splash and then home are both painted with Fast, and a DU differential over the splash lets it ghost through. Our own refresh planner already owns this class of decision, so the port is a planner input, not a second mechanism.

4. **E0/E5 belong to the AA pre-conditioning pass only.** CCSET (0xE0 = 0x02) and TSSET (0xE5 = 0x5A) must **not** be written in either B/W refresh path. Upstream shipped them in the DU path and removed them in `b1523d2` as incorrect, having traced the stock firmware: the plain GC and DU paths write CDI and then the waveform bank, and nothing else. Note that the driver's own header comment still says "DU adds E0=02, E5=5A" — that comment is stale relative to the code, and a port that follows it reintroduces the bug.

### If a grayscale path is ever wanted

Not in scope here — grayscale was assessed and rejected for this firmware (no 2-bpp consumer, and the RAM budget does not have room) — but the constraint is worth recording while it is in front of us. Upstream's 4-level path encodes gray in dual 1 bpp planes (LSB → DTM1, MSB → DTM2) against an external XTF_AA bank loaded as raw 49-byte tables, and every grayscale plane write and refresh must first re-enter the 792×528 partial window so rows stay 99 bytes; the controller's default 800×600 frame uses 100-byte rows and the planes will not align otherwise (upstream `cef5c04`). Upstream marks its own AA path PENDING HARDWARE VALIDATION.

### Do not port from upstream's prose doc

`docs/xteink-x3-uc8279-support.md` in freeink-sdk is **stale relative to its own driver**. It still describes a v1 that uses factory OTP waveforms with `PSR REG=0`, claims the LUT format is 7-byte groups so "the X3's six tuned banks cannot be copied over", and says there is no grayscale. The code has since moved to external LUTs, the 43-byte command-prefixed format recorded above, and a captured AA bank. Port from `Uc8279Driver.{h,cpp}` and `lut/Uc8279X3Luts.h`; treat the doc as historical.

## Scope

### Files

- **[NEW]** `display/src/epd/uc8279.rs` — command set, init, LUT banks (43 bytes each), constants
- **[NEW]** `fw/src/display_flush/uc8279.rs` — async flush backend
- **[MODIFY]** `display/src/epd/mod.rs` — add `uc8279` module (compiled for X3 builds)

### Dependencies

- Depends on: `panel-controller-detection` (dispatch layer must exist to route to this backend)
- **Carries the deep-sleep RST hold with it.** PR #70 (hold RST high through deep sleep) was closed unmerged, which is defensible while the installed base is SSD1677 and UC8253 — upstream's ~36 h-to-dead field report names the UC8179 specifically, and calls the SSD1677 tolerant of a floating RST because it has no external booster and its deep sleep actively discharges. A UC8279 has host-programmed BTST rails, which puts it in the same class as the UC8179 rather than the SSD1677. If this driver ships, re-open that question as part of it, and note that #70's `rst.set_high()` was not the mechanism: a C3 pad goes high-Z in deep sleep whatever its output level, so it needs a pad hold armed before sleep and released before the wake reset pulse (upstream `0425477` does exactly this pairing).

### Notes

- No UC8279 hardware available for testing.
- Upstream has none either: `docs/xteink-x3-uc8279-support.md` still states that no UC8279 X3 unit has been on its bench, and the AA bank is marked PENDING HARDWARE VALIDATION. The evidence has improved since that sentence was written, though — the recipe is reverse-engineered from stock firmware rather than read off a datasheet, and the dark-panel OTP failure is a field report from real units — so this is unvalidated rather than speculative.
- Requires bench verification on a UC8279 X3 unit before shipping.

## Done when

- `display/src/epd/uc8279.rs` contains the full command set, init, LUT banks (43-byte format), and constants matching the freeink reference.
- `fw/src/display_flush/uc8279.rs` implements the async flush backend.
- Both plane writes happen inside one PTIN window and PTOUT closes only after the trailing DTM1 sync, per rule 1. A comment at the call site states the failure mode, because the code reads as if PTOUT could move earlier.
- Neither B/W path writes E0/E5, per rule 4.
- No path seeds DTM1 white to force a clean full refresh, per rule 2.
- The module compiles in X3 builds without warnings.
- `tools/check.sh fast` passes.
- Bench-verified on a UC8279 X3 unit (blocks shipping, not development). Fast/DU refresh is checked specifically, and not only full refresh — rule 1's failure mode is invisible under GC.

## Comments

**2026-08-06** — Enriched from the crosspoint/freeink upstream sweep (freeink
`e62f6c1..8b8337b`). The init recipe and 43-byte LUT format this PRD already
carried are current; what was missing was the sequencing that makes them work.
Added the four load-bearing rules, each of which upstream arrived at by fixing a
bug: `468e341` (PTOUT after the plane write — DU silently dead, GC fine),
`b1523d2` (E0/E5 are AA-only, and the driver's own header comment is now stale
against it), plus the never-seed-DTM1-white and initial-full-budget findings from
the `displayStart` commentary. Also recorded that upstream's prose doc for this
controller has fallen behind its code and describes the abandoned OTP-mode v1, so
a porter following the doc would build the version that leaves the panel dark.
The RST-hold cross-reference is here because this controller's host-programmed
BTST rails put it in the UC8179's class, not the SSD1677's, which is what made
closing PR #70 safe for today's hardware.
