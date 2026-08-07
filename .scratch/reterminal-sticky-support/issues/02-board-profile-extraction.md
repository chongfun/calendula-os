# 02 — Board-profile extraction

Status: ready-for-agent

Part of [reTerminal Sticky support](../PRD.md). Depends on
[01](01-multi-mcu-build-foundation.md).

## Problem

`fw/src/main.rs` *is* the XTeink board definition. It names GPIO21 as EPD CS,
GPIO4 DC, GPIO5 RST, GPIO6 BUSY, GPIO12 SD CS, GPIO3 power, GPIO8/10/7 as
SCK/MOSI/MISO, and GPIO0/1/2 as the ADC channels, with the X3 fuel gauge on
I2C0 (SCL GPIO0 / SDA GPIO20) inline. `fw/src/tasks/input.rs` and
`fw/src/tasks/power.rs` carry the same assumptions further: `InputPins` is typed
on `GPIO0`/`GPIO1`/`GPIO2`, and the deep-sleep handoff protocol
(`WAKE_PIN_REQUESTS` / `WAKE_PIN_HANDOFF` / `GPIO3::steal()`) is written against
GPIO3 by number, with the `steal()` soundness argument stated in terms of that
pin.

Without a board seam, `#[cfg(device_sticky)]` spreads through the application.
This is the most valuable architectural piece of the port.

## Context

**Do not build a runtime `BoardConfig` struct of GPIO numbers.** FreeInk's C++
`BoardProfile` works because it is a C++ codebase where a multi-device binary is
the point; `esp-hal`'s strongly typed peripherals make that abstraction awkward
in Rust, and X3/X4 versus Sticky are separate binaries anyway. Use cfg-selected
board constructors instead.

Each board module consumes `Peripherals` and returns the resources Calendula
actually needs, keeping concrete GPIO/SPI/I²C types private:

```text
fw/src/board/
    mod.rs      // BoardHardware, capability traits/consts, cfg selection
    xteink.rs   // X3/X4 — mechanical extraction of today's main.rs
    sticky.rs   // added in milestone 03
```

```text
BoardHardware
  display_bus      // EpdBus, already generic over SPI/CS/DC/BUSY/RST
  display_control  // rails/enables the board needs before first panel use
  sd_cs
  input            // the board's input backend
  battery          // gauge or ADC, behind a capability
  power            // wake source + latch/rails policy
```

`hal_ext::spi_dma::EpdBus` is already fully generic over its five pin/bus types,
so it needs no change — this is mostly about who constructs it.

**Two capability decisions belong here, not in milestone 3:**

- **Battery.** Replace `#[cfg(feature = "device-x3")]` around
  `hal-ext/src/bq27220.rs`, `tasks::input::battery_run`, `CACHED_GAUGE` and
  `read_power` with a "board has a BQ27220" capability. The X4's ADC divider
  becomes the other arm. Today's X3 behavior — the 30 s thread-executor sampling
  task, the `GAUGE_VALID` boot placeholder, the raised I²C bus timeout for the
  gauge's clock stretching — must survive the move byte-for-byte.
- **Power/wake.** The GPIO3 handoff protocol becomes board-provided: the board
  module owns which pin is the wake source and how it is re-materialised, and
  `tasks/power.rs` asks the board for a wake source rather than naming GPIO3.
  Keep the `steal()` `// SAFETY:` argument intact and re-state it in terms of the
  board's invariant. `hal_ext::rtc`'s C3-specific comments get split from the
  wake mechanism at the same time.

**This is a refactor, not a redesign.** No pin number, timing constant, task
structure, or channel changes. `xteink.rs` starts as a mechanical move. That is
what makes the change low-risk and what makes an X3 hardware smoke test a
sufficient regression gate.

Leave `fw/src/tasks/input.rs`'s ADC ladder where it is, behind the board's input
backend — the GT911 arm lands in milestone 4. Note for that milestone:
`InputEvent::Sample`'s `aux_raw`/`nav_raw`/`page_raw` are ADC-shaped debug fields
that a touch board has no values for. Do **not** solve that here; just make sure
the seam does not make it harder.

## Scope

### Files

- **[NEW]** `fw/src/board/mod.rs` — `BoardHardware`, capability constants,
  cfg selection
- **[NEW]** `fw/src/board/xteink.rs` — today's construction, moved verbatim
- **[MODIFY]** `fw/src/main.rs` — reduced to: chip init, wake-cause read, board
  construction, task spawning
- **[MODIFY]** `fw/src/tasks/input.rs` — input backend behind the board seam;
  battery selection by capability, not by `device-x3`
- **[MODIFY]** `fw/src/tasks/power.rs` — board-provided wake source in place of
  the hardcoded `GPIO3`
- **[MODIFY]** `hal-ext/src/rtc.rs` — separate the C3 wake mechanism from the
  semantics the firmware relies on
- **[MODIFY]** `docs/ARCHITECTURE.md` — the board layer in the module map and
  the bring-up checklist

### Dependencies

- Depends on: `01-multi-mcu-build-foundation`
- Blocks: `03-sticky-hardware-bringup`

### Notes

- Behavior-preserving by construction: if a pin number or a timing constant
  changes in this branch, that is a defect, not a decision.
- The recovery-combo ADC sampling in `main.rs` runs before any task owns the ADC
  and must keep that ordering after extraction.
- `AGENTS.md`: every `.bss` change re-checks the link-time stack `ASSERT` and
  `tools/check.sh stack-frames` on both X4 and X3. A refactor that moves statics
  around still counts.

## Done when

- `fw/src/main.rs` contains no board-specific GPIO number.
- `fw/src/board/xteink.rs` constructs the display bus, SD CS, input, battery and
  power resources with the same pins, configs, and ordering as today.
- Battery support is selected by a board capability rather than `device-x3`, with
  X3 gauge behavior (sampling cadence, `GAUGE_VALID` seeding, bus timeout)
  unchanged.
- The deep-sleep wake pin is board-provided; the handoff protocol and its safety
  argument are preserved.
- `tools/check.sh all` passes for X4 and X3.
- Golden frames pass unblessed on both boards.
- Stack-frame and link-time stack checks pass on both boards, with the numbers
  compared against pre-refactor values.
- An X3 hardware smoke test — boot, open a book, page turn, sleep, wake, battery
  reading — shows no regression.
