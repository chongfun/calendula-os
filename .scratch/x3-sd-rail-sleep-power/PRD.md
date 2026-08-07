# X3 SD rail power-down for deep sleep (GPIO13)

Status: needs-info

## Problem

The X3's SD card rail is switched by GPIO13, and this firmware never drives that
pin. Nothing cuts the card's power when the device enters deep sleep, so if the
rail is live at that moment it stays live for the whole sleep — which on a reader
that sleeps between every reading session is nearly all of its battery life.

Whether that is actually costing anything here is unmeasured. That is what makes
this a `needs-info` PRD rather than a fix: the mechanism is confirmed upstream,
the symptom is not confirmed on our hardware, and the measurement that would
settle it is already ranked as Tier 1 item 4 (C2) in the optimization roadmap.

## Context

### What upstream established

FreeInk `2da0700` declares GPIO13 as the X3's SD-rail power switch and states the
provenance plainly: **confirmed by X3 factory-firmware reverse engineering** —
stock `setup()` does `digitalWrite(13, HIGH)`, and every stock deep-sleep path
does `digitalWrite(13, LOW)`. It is active-high: HIGH powers the card. FreeInk's
comment is explicit about the consequence of leaving it undeclared: with no SD
enable pin in the board profile, `powerDownRailsForSleep()` has nothing to cut,
"the card stays powered through sleep -> battery drain".

CrossPoint `b19503bf` is the corresponding consumer-side fix ("Properly power
down SD power rails on x3"), and like every other CrossPoint hardware commit in
this window it is a one-line freeink-sdk submodule bump — the substance is all in
`2da0700`.

The same commit carries a second fact that constrains the port: **X4 uses GPIO13
too, as `power.latch0`**, and upstream deliberately leaves the X4 profile
unchanged. Any change here must therefore be X3-only and gated on `device-x3`.
Driving GPIO13 in an X4 build would be pulling on a power latch.

### Why the pin map is trustworthy for our board

FreeInk's X3 SD group is MISO 7, CS 12, powerEnable 13, sharing the display SPI
bus on SCLK 8 / MOSI 10. Our own pins agree where they overlap — `fw/src/main.rs`
takes SD CS on GPIO12 and MOSI on GPIO10 — and FreeInk's UC8279 variant profile
declares the identical SD group, describing the newer X3 run as unchanged in
"pinout, ADC ladder input, BQ27220/DS3231/QMI8658 peripherals, SD wiring". So the
GPIO13 claim is about the board we actually have, not a sibling.

### The open question, and why it must be answered before any change

**Our SD card works today, and we never drive GPIO13 HIGH.** If the rail truly
needed an explicit enable, the card would not mount at all. So one of these is
true, and they lead to different work:

1. The rail defaults on (pull-up, or a hard-wired supply GPIO13 only gates in one
   direction). Then driving it LOW for sleep is a real saving and driving it HIGH
   on wake is the necessary counterpart.
2. GPIO13 is not the only enable on this unit, or our unit's revision differs.
   Then driving it is at best a no-op and at worst fights something.

A bench check answers this before a line of firmware changes: read the pin's
resting level at boot, then drive it LOW with a card mounted and confirm the card
drops off the bus. That is also the cheapest possible confirmation that GPIO13 is
the SD rail on *this* unit rather than on upstream's.

### Relationship to the other standby-drain suspect

This is the second of two candidate sleep drains found in the same upstream
window. The other — holding the panel's RESET line through deep sleep
(freeink `0425477`) — was investigated here and closed unmerged as PR #70. The
two are independent: one is the card rail, one is the display controller's
booster restarting through a floating RST. If C2 comes back high, both are live
suspects again and the fuel-gauge experiment cannot tell them apart on its own;
cutting the SD rail is the easier of the two to isolate, because the card can be
removed entirely for a control run.

## Scope

### Files

- **[MODIFY]** `fw/src/main.rs` — claim GPIO13 as an output in X3 builds and
  drive it to the powered level at boot, beside the existing SD CS setup
- **[MODIFY]** `fw/src/tasks/power.rs` or the display task's sleep path — drive
  GPIO13 to the unpowered level as part of the sleep handshake, after the SD
  session owner has finished with the card
- **[MODIFY]** `docs/ARCHITECTURE.md` — record the X3 SD rail in the power model
  if the change lands

### Dependencies

- **Gated on** optimization-roadmap Tier 1 item 4 (C2), the BQ27220 standby-draw
  measurement. A null result there closes this PRD as `wontfix` rather than
  motivating a fix.
- **Gated on** the bench check above resolving which of the two GPIO13 cases
  holds on our unit.
- Interacts with `reterminal-sticky-support`: the Sticky has its own switched SD
  rail (`SD_PWR_EN` on GPIO10 per issue 03) and already documents the
  release-on-wake requirement. If board-profile extraction lands first, this pin
  belongs in `BoardHardware` rather than hardcoded in `main.rs`.

### Non-goals

- Touching GPIO13 in X4 builds. It is `power.latch0` there.
- Any change before C2 has produced a number. A rail that is not draining does
  not need switching, and driving an unverified power pin is a way to create a
  fault rather than fix one.

### Notes

- Single-writer ownership applies: the card rail must not be cut while the SD
  session owner still believes it holds the card. The sleep handshake already
  sequences the display task and the power task, so the rail change belongs
  inside that existing ordering, not beside it.
- Cutting power to a mounted FAT volume is the same hazard as a power cut. The
  existing staging-marker and integrity-gate work already covers torn writes, so
  the requirement here is ordering (cut after the session is closed), not a new
  durability mechanism.

## Done when

- The bench check has answered which GPIO13 case holds on our X3, and the answer
  is written into this file's Comments.
- **If C2 indicts the rail:** GPIO13 is driven to the powered level at boot in
  X3 builds only, driven to the unpowered level inside the sleep handshake after
  the card session is closed, and left untouched in X4 builds.
- A second C2-style fuel-gauge run over the same window shows a measurably lower
  standby draw than the pre-change baseline, with both numbers recorded here.
- `tools/check.sh fast` and `tools/check.sh firmware` pass, and the X3 build is
  checked as well as the X4 per the repository's dual-device rule.
- **If C2 comes back null:** this PRD is closed `wontfix` with the measurement
  recorded, so the next person who reads freeink `2da0700` does not re-open it
  from first principles.

## Comments

**2026-08-06** — Opened from the crosspoint/freeink upstream sweep (freeink
`e62f6c1..8b8337b`). Filed as `needs-info` rather than `ready-for-*` on purpose:
freeink's GPIO13 provenance is strong (factory-firmware RE, not inference), but
it explains a drain we have never measured on a unit whose card mounts fine
without the enable we supposedly need. Both of those have to resolve before the
work is specifiable, and one of them (C2) is already ranked in the roadmap.
