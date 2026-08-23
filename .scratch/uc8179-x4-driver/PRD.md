# UC8179 display driver for X4 (newer production panels)

Status: needs-hardware

**Restatused 2026-08-13, and the reason is worth more than the label.** This
read `ready-for-human`, the same status as the UC8279 X3 PRD, and the two are
not the same kind of thing. That PRD's hardware caveat is a *schedule* risk — a
UC8279d X3 unit can arrive on this bench, so "blocks shipping, not development"
is a gate that will one day close. **This one can never close.** The owner has
no X4 and no path to one, so identical wording implied the difference was
timing when it is a difference in kind.

Two honest deliverables, and this PRD should pick one rather than leave it
implicit:

1. **Reframe it** to what is achievable here — a compile-clean, host-tested
   port whose acceptance is "matches the freeink reference and passes host
   tests", with device validation handed to someone with X4 hardware (upstream
   has benched UC8179 units).
2. **Park it.** It costs nothing today: `fw/src/display_flush/mod.rs` already
   routes a confirmed UC8179 to the SSD1677 backend — unchanged behaviour, not
   a dark panel. This is the asymmetry with the X3 case, where an undetected
   UC8279d on the UC8253 backend risks a dark panel on units that actually
   ship. **No user harm accrues while this sits.**

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

- Depends on: the runtime controller probe, shipped as #76 (`display/src/epd/probe.rs`, `hal-ext/src/epd_probe.rs`, dispatch in `fw/src/display_flush/mod.rs`). The dispatch layer exists and routes a confirmed sibling to the default backend until one of these drivers lands; `active_backend()` is where the second arm goes.
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
The controller probe merged as #76 (`27ea614`); its PRD has been deleted now
that the code and `docs/ARCHITECTURE.md:383-446` carry the reasoning, and is
recoverable at `733d27c` if needed. `fw::display_flush`
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

**2026-08-13 upstream sweep — four corrections. The first two mean the current
spec would build the wrong thing.**

1. **The BUSY model in Problem is now wrong.** freeink `c60987a` replaced
   `BusyPolarity::X3TwoPhase` with a new `UcIdleHigh` for both UC8179 and
   UC8279 X4: delay one RTOS tick, then poll until BUSY_N is HIGH — **no LOW
   edge observed, and deliberately no millisecond timeout**, because *"issuing
   the next command while BUSY_N is still LOW can make the UC controller
   discard plane or LUT writes."* `waitRefreshComplete()` now routes
   `UcIdleHigh` back through `waitBusy()` rather than the ISR/semaphore path,
   *"so a missed assertion edge can never make the caller write RAM while the
   waveform is still busy."* This PRD still specifies the edge-qualified
   two-phase wait upstream just removed — which can miss the edge on a fast
   operation and return immediately.
2. **A missing init register.** `41f2a7f` appends PWS (0xE3) = 0x22 after GATE
   — "VCOM 2 lines, source 2 × 660 ns", from GxEPD2, for dithered-bitmap
   stability. Upstream keeps it configurable (0 = skip) because the X4 Pro uses
   different glass and a 600-gate scan, so port it as a zeroable constant
   rather than a blind copy.
3. **The RST hold refinement, which bites harder here** because this PRD makes
   the hold a ship blocker. Per freeink `61f0b2b`: the hold *level* is
   board-conditional (X4 lands on HIGH **because** its panel rail is never
   gated, not by default), and "releases it in bus init … and both halves are
   required" is now **three** sites — the third being controller detection,
   which runs before bus init. On our tree that is `fw/src/main.rs:488`
   borrowing GPIO5 ahead of `Output::new` at `:498`.
4. **This PRD ports one of two drivers the probe can now select.** `41f2a7f`
   plus `625a496` — the latter sitting in the *previous* sweep window and
   missed on 2026-08-06 — mean upstream ships a distinct `Uc8279X4Driver`, and
   `BoardConfig.h` enables both `FREEINK_DRIVER_UC8179` and
   `FREEINK_DRIVER_UC8279_X4` for X4 **and** X4 Pro, so it is not X4-Pro-only.
   Upstream's own reference doc says to keep three drivers apart. This PRD
   still frames it as "if X4 units turn out to carry either" — upstream has
   settled that they do, and a UC8279-in-X4-clothing unit would fall through to
   SSD1677 under this scope as written.

**Do not "correct" the PSR values from upstream's commit message.** `c60987a`'s
message claims *"UC8179 now uses PSR 0x3B and software byte/bit reversal
instead of SHL."* The diff does not do that: `uc8179DefaultConfig()` still
ships `psr0 = 0x3F` (0x3B + SHL), mirror-X is still the hardware SHL bit, and
only comments were reworded. This PRD's pinned transforms and PSR values are
**current and correct** — verified against the shipped code, not the message.
Same failure mode as the stale-header warning above, and it earns the same
explicit treatment.

*Adjacent, out of scope but worth not losing:* `c60987a` also changed the
**SSD1677** — a 10 ms wait after SWRESET, and a documented FAST/`0xFC`
`turnOff=true` shutdown (`0x3C=0x80`, `0x22=0x03`, `0x20`, 200 ms) with async
updates deferring it until refresh completion. That is the backend our X4
builds ship *today*.

**Rescued from an abandoned PRD, because the last copy was about to be garbage
collected.** An `ssd1677-production-fixes` PRD was written on 2026-08-05 and
dropped once it turned out to be X4-only — SSD1677 is `cfg`'d out of X3 builds,
so none of it was reachable on the only hardware this project has. That was the
right call, and it is not being reopened here. But the document survived solely
in a dangling commit (`bc96b25`) on no branch, and one of its findings is still
live in shipped X4 builds:

- **We apply the fast-DU `0x1C` shortcut unconditionally**
  (`display/src/epd/ssd1677.rs:143`, the `RefreshMode::Fast` arm). Upstream made
  it **opt-in after ghosting and blotching reports**. Note the neighbouring
  `FastClean` arm carries a "deliberately" comment explaining its bit choice and
  the `Fast` arm carries none, so the unconditional `0x1C` currently reads as
  considered when it is inherited.
- Its other fix — the booster soft-start 5th byte `0x80` — **already landed**
  as #69 and is at `display/src/epd/ssd1677.rs:61`. Nothing to do.

The implementation is not lost: `origin/feature/fast-du-setting` (tip
`7463c78`) makes the shortcut a setting, and `origin/fix-rst-pin-hold`
(`d34a733`) carries the RST work. Both are live refs. It is only the *reasoning*
that was dangling, which is why it is written here rather than left as a branch
nobody would think to read. Anyone picking up X4 display work should start from
those two branches rather than from scratch.

**2026-08-22 upstream sweep (freeink `f4441d2`, window `61f0b2b..f4441d2`).**
One commit on SDK main since the last sweep. It adds a **field-validated
`Uc8279X4Driver`** (tested 2026-08-19) alongside the existing `Uc8179Driver`.
Implementation details not previously recorded:

- **Post-PON PSR latch:** PSR (`0x37, 0x4D`) must be written *after* `0x04 PON`
  because PON reloads MTP defaults — a protocol difference from both UC8179 and
  UC8279d X3.
- **120-gate offset:** the active 480 rows start at gate 120 within the 600-gate
  scan; rows 0..119 and 600+ are padded `0xFF`.
- **PLL:** `0x0E` (vs X3's `0x0F`).
- **AA waveform variants:** selects between `kXtfAa02` and `kXtfAa68` based on
  the probed LUT_VER byte.
- A `MurphyM4` board profile was also added: ESP32-S3R8, SSD1677, FT6336U
  capacitive touch, dual frontlight channels, 4-bit SDMMC — a second S3
  reference implementation alongside Sticky.

All scoped out: this PRD covers UC8179 only, and the X4 Pro is a separate
controller requiring its own PRD if it is ever targeted. The details are
recorded here because this PRD's comment 4 already tracked the driver's
existence and this is where someone looking for it would start.

**Submodule pin check resolved:** crosspoint-reader pins freeink-sdk at
`fa06239`, which diverged from our `f4441d2` at common ancestor `e62f6c1`.
The delta is 6 FreeInkUI styling headers (corner radii, delete icon, capsule
slider props). All `libs/hardware`, `libs/display`, `libs/book`, and
`libs/network` modules are byte-identical. No driver, power, or hardware
changes affect the baselines recorded in this PRD.
