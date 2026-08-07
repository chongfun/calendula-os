# Runtime display controller detection

Status: implemented on `feature/runtime-display-controller-detection` (58f5fa0),
X3-validated; awaiting review and merge

## Problem

Newer Xteink production units ship different panel controllers (X3: UC8279d instead of UC8253, X4: UC8179 instead of SSD1677). Users cannot externally identify their controller variant, so detection must happen at runtime.

## Context

The UC81xx controllers expose version registers that can be read over the data bus before SPI peripheral init. A GPIO bit-bang probe can read UC81xx VER (0x70), FLG (0x71), and RMTP (0xA2) registers at boot. SSD1677 and UC8253 do not respond to this probe, so a timeout-based fallback classifies them as the default controller for the device.

The probe result is stored in a static `AtomicU8` for the flush module to dispatch on. The dispatch layer in `fw/src/display_flush/mod.rs` routes `init_panel`, `flush`, `prestage_previous`, and `sleep_panel` to the detected controller's backend.

### Probe protocol, from the FreeInk reference

`libs/hardware/XteinkDetect/` implements this in production. Six details there
are load-bearing and are not derivable from the datasheets:

1. **Two passes with agreement, not one read.** Confirm the sibling controller
   only when two independent passes agree. A single stray answer is
   `Inconclusive` and resolves to the device's default controller. A one-shot
   timeout fallback is weaker than this and will misclassify on a marginal bus.
2. **FLG must be a *driven* status, not merely readable.** Reject `0x00` and
   `0xFF` as floating-bus artifacts, and require `BUSY_N` (bit 0) asserted —
   idle. Without this check a floating bus produces plausible-looking bytes and
   the probe confirms a controller that is not there.
3. **Do not gate on BUSY during the probe, and tier the reset pulse.** Which
   controller is present is the unknown, so its BUSY polarity is also unknown —
   the two families are opposite (SSD1677 active-high; UC8253 two-phase, idle
   high). Use a flat delay after the reset pulse instead, sized to cover every
   UC81xx power-up (upstream: 2 ms high, the pulse low, then 30 ms settle).

   The pulse length is **tiered, not a flat 50 ms**. The vendor identification
   path holds RST_N low for 50 ms — far beyond the datasheet's 50 µs minimum,
   because the ID readback is less forgiving than normal operation — but paying
   that on both passes costs ~100 ms on every boot *and every wake* of the
   SSD1677 and UC8253 units that are the entire installed base, which gain
   nothing from it. Upstream `ca93e3d` therefore screens with a **1 ms** pulse on
   pass 1 and spends the 50 ms only as the confirm pass after a UC81xx hit,
   whose VER bytes become the authoritative readback:

   | Pass | Pulse | Condition |
   |---|---|---|
   | 1 (screen) | 1 ms | always |
   | 1 (escalate) | 50 ms | only if the screen failed **and** the board is X3-family |
   | 2 (confirm) | 50 ms if pass 1 matched, else 1 ms | always |

   The escalation is deliberately X3-only: upstream's comment is that "the X4
   family keeps the cheap path — its UC8179 is bench-proven to answer the 1 ms
   pulse", while an X3 sibling that answers only at vendor timing is still
   possible and that board's boot budget tolerates the retry. That asymmetry is a
   bench finding, not a datasheet reading, and it is the whole reason the timing
   is not uniform.
4. **RMTP (0xA2) is the escalation path, not just a third register.** When VER
   reads as floating with `ver[0] == 0xFF` but FLG is driven and both passes
   agree, dump the MTP and confirm on `mtp[0] == 0xA5`, the refresh-enable key.
   This recovers parts that do not answer VER cleanly.
5. **`VER` byte 2 (`LUT_VER`) tells UC8179 from UC8279.** Not needed while the
   device build already implies which sibling is possible (X3 → UC8279d,
   X4 → UC8179), but it is the discriminator if a device ever ships both, and
   worth capturing in diagnostics regardless.
6. **NVS `hw_calib/screenType` is diagnostics only.** The OEM records a panel
   type per unit there, and FreeInk's comment states it can describe the wrong
   panel entirely. The live bus probe is ground truth. Do not dispatch on NVS —
   it is the obvious-looking shortcut and it is wrong.

### Field diagnosis without a serial cable

FreeInk keeps a `XteinkDisplayProbeDiag` snapshot — raw VER bytes, FLG, verdict,
whether the driver was promoted, and the first 48 MTP bytes — explicitly so
firmware can persist it "somewhere a user can retrieve WITHOUT serial access
(locked units): e.g. a file on the SD card."

Calendula should do the same. A user reporting "my screen looks wrong" cannot be
asked for `esp-println` output, and the probe's verdict is the single most useful
fact for answering them. Writing it to the SD card on every boot costs one small
file and makes the failure remotely diagnosable.

### Pin sourcing

Take the probe's pins from the board layer rather than hardcoding them. FreeInk
carries two variants for exactly this reason — one that hardcodes the X3 C3
pinout and a board-agnostic one that reads `BoardConfig::ACTIVE` — because the
hardcoded probe is unsafe on any other board. If `reterminal-sticky-support`
issue 02 (board-profile extraction) lands first, source the pins from
`BoardHardware`; otherwise leave a clear seam so it can move later.

The probe must **not** compile into the ESP32-S3 Sticky build: there is no
evidence the Sticky ships multiple controllers, and these GPIOs mean different
things on that board. See `reterminal-sticky-support` non-goals.

## Scope

### Files

- **[NEW]** `hal-ext/src/epd_probe.rs` — GPIO bit-bang probe, `ProbeVerdict` enum
- **[MODIFY]** `hal-ext/src/lib.rs` — export `epd_probe` module
- **[MODIFY]** `fw/src/main.rs` — run probe after GPIO init, before SPI2 config
- **[MODIFY]** `fw/src/display_flush/mod.rs` — runtime dispatch based on `DetectedController`

### Dependencies

- Blocks: `uc8179-x4-driver`, `uc8279-x3-driver` (those drivers need this dispatch layer)
- Sequenced after `board-identity-guard`, if that lands: it answers "which board", this answers "which controller on that board", and a wrong-board verdict makes this question moot. Both are early-boot probes competing for the same pins-before-peripherals budget.

### Notes

- Can be developed and tested on existing hardware — probe should return `DefaultAssumed` on SSD1677/UC8253 panels.
- No UC8179 or UC8279d hardware is available, so the *confirming* path cannot be exercised locally. That makes the negative path — a correct `DefaultAssumed` on every existing unit — the only thing this can prove, and it makes rules 1 and 2 above the difference between a safe probe and one that promotes a driver on a floating bus.

## Done when

- `epd_probe.rs` implements the GPIO bit-bang read of VER/FLG/RMTP registers and returns a `ProbeVerdict`.
- The verdict is two-pass with agreement; a single stray answer resolves to `DefaultAssumed`.
- FLG is validated as driven (not `0x00`/`0xFF`) with `BUSY_N` asserted before any confirmation.
- The probe does not wait on BUSY; a flat post-reset delay covers power-up for either controller family.
- The reset pulse is tiered per the table in rule 3: a 1 ms screening pass, the 50 ms vendor timing spent only on the confirm pass after a match, and the 50 ms escalation compiled only into X3 builds. Measured against the pre-probe baseline, the added boot cost on an SSD1677 or UC8253 unit is the screening pass plus its settle, not two vendor-timing pulses.
- The RMTP `mtp[0] == 0xA5` escalation is implemented for the floating-VER case.
- The probe runs at boot after GPIO init but before SPI2 peripheral configuration, and releases its pins afterwards.
- The result is stored in a `static AtomicU8` accessible to the flush module.
- `display_flush/mod.rs` dispatches `init_panel`/`flush`/`prestage_previous`/`sleep_panel` based on the detected controller.
- Raw VER/FLG/verdict are persisted somewhere retrievable without a serial cable.
- NVS `hw_calib/screenType`, if read at all, is recorded as diagnostics and never dispatched on.
- The probe is absent from the ESP32-S3 build.
- On existing SSD1677/UC8253 hardware, the probe returns `DefaultAssumed` and the existing driver path is taken with no behavioral change — verified across repeated cold boots, not a single run.
- `tools/check.sh fast` passes.
- `tools/check.sh emulator` passes (emulator path unchanged for default controllers).

## Comments

**2026-08-06** — Enriched from the FreeInk reference (`libs/hardware/XteinkDetect/`,
standalone checkout at `e52d480`) while researching the reTerminal Sticky port.
The original Context described a single-shot probe with a timeout fallback; the
production implementation is two-pass-with-agreement plus a floating-bus
rejection on FLG, and treats RMTP as an escalation path rather than a third
register to read. Added the pin-sourcing and S3-exclusion notes so this probe and
`reterminal-sticky-support` do not collide, and a cross-reference to the new
`board-identity-guard` PRD, which owns the adjacent "which board am I" question.
Scope and intent are unchanged.

**2026-08-06 (later)** — Corrected rule 3 from the upstream sweep. The rule said
a flat 50 ms reset pulse, which was the reference implementation's behaviour up to
freeink `ca93e3d` (2026-08-02) and is now two passes' worth of vendor timing that
commit exists to avoid — ~100 ms on every boot and every wake of exactly the
SSD1677/UC8253 units this probe is supposed to leave untouched. Replaced with the
tiered scheme and its X3-only escalation, and added the matching acceptance line.
Nothing else about the probe changed: two-pass agreement, the FLG driven-status
check, the RMTP escalation, and the NVS-is-diagnostics rule all still hold as
written.

**2026-08-07** — Implemented and bench-validated on an X3. Every acceptance
line above is met except the three named at the end of this entry.

**The bench found the guard that carries this feature.** A shipping UC8253 X3
answers the probe with `VER = FF FF FF FF FF` and `FLG = 0x13` — byte for byte
the signature rule 4 attributes to a *field UC8279d*. Its status line is
genuinely driven, so the driven-FLG check admits it; its VER is blank and both
passes agree, so the blank-VER recovery is fully armed. The only thing between
that unit and a false sibling confirmation is its MTP dump reading all `FF`
instead of opening with `0xA5`.

So rule 4 is not a third register read and not defence in depth. Relax it and
this firmware misidentifies the installed base it was written for, on hardware
we have in hand. Pinned as the host test
`a_shipping_uc8253_is_only_told_apart_by_its_mtp`, built from the observed
bytes. This also invalidates a claim the Context made in passing: the UC8253
does *not* leave the bus floating. It answers `0x71` with a real status and
only its VER and MTP come back blank, which is a much narrower difference than
"does not respond" suggests. Anyone touching this matcher should read that
test first.

**Deviations from Scope, all deliberate.** The decision rules did not stay in
`hal-ext/src/epd_probe.rs`; they live in a new sans-IO `display::epd::probe`,
with `epd_probe` reduced to pins, reset timing and the bit-bang. `hal-ext` has
no host tests, and with no UC8179 or UC8279d hardware in existence the
confirming path's only possible evidence is host tests — so the rules had to go
where they could be exercised. Fourteen of them now cover the two-pass
agreement, the floating-bus rejections, the MTP escalation and the byte
encoding. The cost is a `display` path dependency on `hal-ext`, which is the
one architectural call in this change worth a second opinion.

NVS `hw_calib/screenType` is not read at all. The acceptance line permits this
("if read at all"), and an ESP-IDF NVS parser is a large addition for a value
that must never be dispatched on; `PROBE.TXT` explains what it is instead. The
S3 exclusion is `#[cfg(target_arch = "riscv32")]` on the module, which is
compile-checked but unrunnable until an Xtensa build exists.

**One addition the PRD did not ask for.** The probe answers a question about
soldered hardware, and nothing reaches a soldering iron without disconnecting
the battery, so the scope of one probe is one *power cycle* rather than one
boot. `fw::probe_cache` retains the verdict in RTC fast RAM; a deep-sleep wake,
OTA reset or crash reboot reuses it and pays nothing. RTC RAM specifically, and
for the same reason rule 6 refuses to dispatch on NVS: flash, NVS and the SD
card all survive being copied onto another unit, so a verdict cached there can
outlive the hardware it describes. RTC RAM is zeroed on first power-on, which
is also the earliest moment the panel could have changed. *Bench note:* a
software reset now reuses the verdict, so re-running the probe takes a power
cycle; `main` logs which path each boot took.

**Measured, X3, 2026-08-07.** Twelve consecutive live probes (temporary
cache-bypass build), all `default-assumed -> UC8253`, no `Inconclusive` on any
run. 154 ms against 152 ms computed from the constants, the difference being
the actual bit-banging. The full X3 tier ladder runs — 1 ms screen, 50 ms
escalation on the miss, 1 ms confirm — and the escalation is dead-code
eliminated from the X4 binary outright (three `run_pass` call sites there
against four on X3, confirmed by disassembly), so "compiled only into X3
builds" holds literally. Home renders unchanged at 929 + 379 ms with a 24 ms
prestage. `PROBE.TXT` was read back off the device: 447 bytes, correct.
Deep-sleep retention is witnessed rather than assumed — the linker packs
`sleep_marker` between the cache's two statics in one 64-byte
`.rtc_fast.persistent` region, so a wake rendering as one quick flicker proves
the bytes either side of `SLEEP_IMAGE` were retained too.

RAM +112 B `.bss` and +60 B RTC fast RAM; flash +6.1 KB `.text`, +808 B
`.rodata`. Worst stack frame unchanged. `tools/check.sh all` passes, as do
`ota-selftest` for both devices.

**Not proven, and not provable here.** The confirming path, which has no
hardware to run on. The ESP32-S3 exclusion, until an Xtensa build exists. The
`page-turn` and `sleep-sync` bench suites, which need an operator on the
buttons — the render path is untouched by this change (the dispatch lands on
the same backend) and a normal Home render was observed, but that is an
observation, not those suites.
