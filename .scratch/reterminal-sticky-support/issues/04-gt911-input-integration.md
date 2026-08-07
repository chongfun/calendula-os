# 04 — GT911 input integration

Status: ready-for-human

Part of [reTerminal Sticky support](../PRD.md). Depends on
[03](03-sticky-hardware-bringup.md).

## Problem

The Sticky's primary input is a GT911 capacitive touch panel. Calendula's input
path is two ADC resistor ladders. Adding touch must not turn a hardware port into
a UI rewrite.

## Context

**The semantic seam already exists.** `app_core::Button` is
`Power | Back | Confirm | Previous | Next | PagePrevious | PageNext` — already
actions, not physical keys — and the physical-to-semantic map already lives in
`fw/src/tasks/input.rs::map_hardware`. So the work is a second input backend that
emits the same `Button` values, not a new event model:

```text
physical input                       semantic actions
  XTeink ADC ladders  ──┐             Previous / Next
  Sticky GPIO 5/6/4   ──┼──────────►  PagePrevious / PageNext
  Sticky GT911        ──┘             Confirm / Back / Power
```

Everything above that seam — reducers, screens, the reader — stays untouched.

**What is genuinely ADC-shaped** is `InputEvent::Sample`'s `aux_raw`, `nav_raw`
and `page_raw` debug fields, which a touch board has no values for. They are read
by the diagnostics view, the web emulator, and the `bench: input` line. Pick one
of: neutral values from the touch backend (what `fw/src/tasks/display.rs` already
does when it synthesises a sample), or widening the event. Prefer the former —
widening a struct carried in a channel is a RAM decision under `AGENTS.md`.

**The battery reading rides on every input sample.** The touch backend must keep
pushing `battery_mv` / `battery_percent`, including the boot seed that waits for
`GAUGE_VALID`, or the first paint shows a flat placeholder forever.

### Driver

Check first whether a maintained `embedded-hal` GT911 crate covers what is
needed; `AGENTS.md` prefers crates already in the tree and requires a new one to
be `no_std`, default-features-trimmed, and weighed for flash and RAM. Otherwise
implement the small subset Calendula needs — the protocol is not large, and
FreeInk's `InputManager.cpp` is a known-working behavioural reference:

- **Rail then reset then probe.** Drive `TOUCH_EN` (GPIO42, active-high) to ON —
  with a `gpio_hold_dis()` first, because the sleep path holds it LOW and the
  hold survives the wake reset — settle ~50 ms, then the reset/address dance:
  RST low, INT driven to the select level, ~10 ms, RST high, ~10 ms, re-drive
  INT, ~50 ms, INT back to input, ~50 ms. The INT level as RST rises selects the
  I²C address. Probe 0x5D, then 0x14; if neither ACKs, repeat the dance with the
  opposite INT level before declaring touch absent.
- **Polling.** Read status at `0x814E`; bit 0x80 means a frame is ready, the low
  nibble is the contact count. Read 8 bytes at `0x8150`. **Write 0 back to
  `0x814E` after every read** — the controller will not produce another frame
  otherwise.
- **Frame layout.** FreeInk's Sticky profile sets `gt911CoordsAtByte0 = true`
  (coords at byte 0, no track-id), confirmed by raw point dumps during their
  bring-up. Verify on the unit rather than trusting the datasheet default.
- **Coordinates.** FreeInk's Sticky profile applies `swapXY` plus both flips: a
  portrait digitizer on a landscape panel, with the raw range equal to the panel
  size (0–799 × 0–479) because the GT911 reports pixel coordinates. Order
  matters: swap first, then map against the post-swap axis ranges, then flip.
  Confirm with corner taps on hardware — this and the panel orientation from
  milestone 03 must agree.

### Mapping

Map gestures and regions onto existing actions. Reader taps become
previous/next page; swipes and region taps supply the navigation actions menus
need. The Sticky's digital up/down/confirm buttons remain live, so touch is
filling in Back/Previous/Next rather than carrying everything.

**Tune the interaction mapping on hardware; this PRD deliberately does not
specify it.** Keep whatever policy emerges in a host-testable module beside
`app_core::buttons`, so tap-versus-swipe classification, slop, and hold
thresholds get regression tests the way the ADC classifier does.

**Touch must feed the idle timer.** `PowerEvent::Activity(view)` is what pushes
the deep-sleep deadline out; a touch-only interaction that never emits it puts
the device to sleep under the reader's finger.

### Deliberately later

First-class pointer/tap events and UI hit testing. CrossPoint has gone
further — real coordinate transformation and hit testing rather than treating a
touchscreen as a set of buttons — and that is the right destination. It does not
block initial Sticky support, and doing it now would mean redesigning every
screen during a hardware port.

## Scope

### Files

- **[NEW]** `hal-ext/src/gt911.rs` (or a vetted crate dependency) — reset/address
  sequence, status/point reads, status clear
- **[NEW]** touch-to-action policy module beside `app-core/src/buttons.rs`, host
  tested
- **[MODIFY]** `fw/src/board/sticky.rs` — touch rail, I²C bus, INT/RST pins
- **[MODIFY]** `fw/src/tasks/input.rs` — GT911 backend arm emitting `Button`
  events with battery and neutral raw fields
- **[MODIFY]** `docs/ARCHITECTURE.md` — the input seam and its two backends

### Dependencies

- Depends on: `03-sticky-hardware-bringup`

### Notes

- The GT911 shares nothing with the fuel gauge bus — separate I²C controller
  (touch on SDA3/SCL2, gauge on SDA1/SCL0), so no arbitration is needed between
  them.
- Cutting the touch rail for deep sleep forfeits touch-to-wake. That is the right
  trade for a reader whose power button wakes it; note it rather than
  rediscovering it.
- Poll cadence: the ADC loop runs at 15 ms on an interrupt-priority executor.
  I²C reads have no place at interrupt priority — the X3 gauge was moved off that
  executor for exactly this reason. Decide where the GT911 poll lives and say
  why.

## Done when

- The GT911 is detected reliably across cold boots and deep-sleep wakes,
  including after a sleep that held its rail off.
- Taps and swipes produce the intended `Button` actions with no stuck or repeated
  events.
- Coordinates are correct at all four corners and agree with the panel
  orientation chosen in milestone 03.
- Every existing screen — home, library, chapters, settings, reader, wireless —
  is navigable through touch.
- Representative EPUBs open and page forward/back; image-heavy books and
  settings/library transitions behave.
- Touch input resets the idle/sleep timeout.
- Battery readings continue to flow through touch-originated samples, including
  the boot seed.
- The touch-to-action policy has host tests.
- `tools/check.sh all` passes, plus the Sticky build/clippy entry point.
- 50+ sleep → power-button-wake cycles pass with touch working after each wake,
  with genuine wakeups distinguished from resets and brownouts.
