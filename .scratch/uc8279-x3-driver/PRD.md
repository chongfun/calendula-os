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

- Depends on: the runtime controller probe, shipped as #76 (`display/src/epd/probe.rs`, `hal-ext/src/epd_probe.rs`, dispatch in `fw/src/display_flush/mod.rs`). The dispatch layer exists and routes a confirmed sibling to the default backend until one of these drivers lands; `active_backend()` is where the second arm goes.
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

**2026-08-07** — The dispatch dependency is satisfied:
The controller probe merged as #76 (`27ea614`); its PRD has been deleted now
that the code and `docs/ARCHITECTURE.md:383-446` carry the reasoning, and is
recoverable at `733d27c` if needed. `fw::display_flush`
now routes `init_panel`/`flush`/`prestage_previous`/`sleep_panel` through a
`DetectedController`, and the arm this driver plugs into is marked in the
source. Until it lands, a confirmed UC8279d runs the UC8253 backend — no worse
than before the probe existed, and it means shipping this driver is the moment
the probe's verdict starts changing behaviour rather than just being recorded.

**Read this before trusting the probe's verdict on X3.** Bench-validating the
probe turned up something that lands squarely on this PRD: a *shipping UC8253*
answers the fingerprint with `VER = FF FF FF FF FF` and `FLG = 0x13` — byte for
byte the field-UC8279d signature the probe's **rule 3** exists to recover (the
rules renumbered on the way into the code — this was rule 4 in the deleted PRD;
`display/src/epd/probe.rs:38-60` is the live statement of it).
Its status line is genuinely driven; only its VER and MTP come back blank. The
single discriminator between the two controllers this PRD spans is the RMTP
dump opening with the `0xA5` key.

Two consequences for the port. First, "the panel answered `0x71`, so it is a
UC8279" is false on this hardware, and any bring-up shortcut resting on it will
drive UC8253 units with the wrong protocol. Second, the UC8279d units that
motivate this driver are described upstream as sometimes shipping with a *blank
MTP* (hence PSR REG=1 and the external LUTs above) — so it is worth checking on
first real hardware whether a blank-MTP UC8279d can also produce a dump without
the `0xA5` key. If it can, the probe would classify that unit as a UC8253 and
this driver would never be selected for it. That is a safe failure today and a
silent one once this driver exists, so it belongs in this PRD's bring-up list
rather than the probe's.

**Upstream sweep 2026-08-13 — four corrections, none of them cosmetic.**

1. **Rule 3's two-forced-GC budget is wrong, and upstream deleted it as a
   regression.** freeink `b17beee` took `_initialFullSyncsRemaining` from 2 to
   1 on the UC8253 X3 driver, device-validated: a FAST refresh costing 436 ms
   warm was costing 2989 ms, and press-to-home went 7634 → 4569 ms. The load
   bearing part is the commit's own reasoning — *"the UC8279 and SSD1677
   siblings use a one-shot `_needFullClear` instead; this brings the X3 in
   line."* So **the UC8279 driver this PRD ports was always one-shot**; the
   two-paint budget described UC8253 behaviour and has since been removed as a
   ~3 s cost. Porting it would import a fixed bug.
2. **A fifth load-bearing rule, which rule 2 currently hides.** freeink
   `1f0a314` added inverted-content support: on a non-GC refresh with a dark
   background, DTM1 is rewritten as the *complement* of the target
   (`sendPlaneFlippedInverted`), so every pixel classifies as changed and is
   re-driven, and `displayFinish`'s DTM1 sync restores the true baseline. The
   reason is that a DU idles unchanged pixels, so light residue from each
   white→black transition parks in a dark background and accumulates between GC
   passes. This is the sanctioned exception to rule 2 — a porter reading "no
   path seeds DTM1 white" as "never pre-write DTM1" will not build it. Upstream
   flips on the wire specifically because *"the C3 boards have no RAM to spare"*
   for a host-side inverted copy, which is our exact constraint.
3. **The RST hold has a second release site, and on our tree it is the boot
   probe.** freeink `61f0b2b` supersedes `0425477` (cited above): the hold level
   is board-conditional — LOW alongside a gated-off rail, because holding an
   unpowered controller's RESET high back-powers it through its protection
   diode; HIGH only where the rail stays up. X3/X4 have no gated panel rail, so
   we land on HIGH, but *because* our rail is never gated rather than by
   default. And the release must happen in controller detection as well as bus
   init, or *"every `digitalWrite` silently bounces off the retained latch and
   the probe can select the wrong driver."* **This lands on us directly**:
   `fw/src/main.rs:488` borrows GPIO5 for the probe before
   `Output::new(peripherals.GPIO5, …)` at `:498`. Mitigation, not a fix:
   `probe_cache::load()` short-circuits most wakes — but `Inconclusive` is
   deliberately never cached (`fw/src/probe_cache.rs:107-112`), so a *marginal*
   unit re-probes every wake, which is exactly the population a retained hold
   would silently corrupt.
4. **A correct upstream doc now exists.** `docs/display-driver-references.md`
   (freeink `41f2a7f`) agrees with this PRD, carries a change policy — never
   copy waveforms because the controller name matches — and states the UC8279d
   **X3** driver must be kept separate from the UC8279 **X4 Pro** one. Worth
   naming next to the stale `docs/xteink-x3-uc8279-support.md` warning, now that
   two upstream files are called `Uc8279*`.

*Corroboration, no change needed:* freeink `cd2bcc5` removed the UC8253
`factoryP1/P2` banks because they need "a dedicated grayscale panel init (PSR
3F 4A, PWR 43 00 78 78 17, VCOM 0x26 — different rails from the B/W init)".
Those are exactly the bytes this PRD lists as the UC8279d's *B/W* init —
independent confirmation the reverse-engineered recipe is right, and a
rail-level explanation of why UC8253 LUTs cannot carry over.

The host test `a_shipping_uc8253_is_only_told_apart_by_its_mtp` in
`display::epd::probe` pins the observed bytes and is the fastest way to see the
shape of the problem.
