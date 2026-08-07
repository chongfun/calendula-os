# 03 — Sticky hardware bring-up

Status: ready-for-human

Part of [reTerminal Sticky support](../PRD.md). Depends on
[02](02-board-profile-extraction.md).

## Problem

With a chip-neutral build and a board seam in place, bring up the Sticky's
peripherals: power latch and rails, SSD1677 display, shared-bus MicroSD, BQ27220,
S3 deep sleep and wake, and Wi-Fi. Touch is deliberately excluded — the Sticky
has digital up/down/confirm buttons, so it is navigable without GT911, and that
is what makes this milestone shippable on its own.

## Context

### Pinout

From FreeInk's `STICKY` profile (triple-sourced: V01 schematic 2026-06-05,
porting spec, Seeed demo `pin_config.h`):

| Function | GPIO |
|---|---|
| EPD SCK / MOSI / CS / DC / RST / BUSY | 13 / 14 / 15 / 16 / 17 / 18 |
| EPD rail enable (`EP_PWR_EN`) | 47 |
| SD SCK / MISO / MOSI / CS | 13 / 12 / 14 / 8 (shares the display bus) |
| SD rail enable (`SD_PWR_EN`) | 10 |
| Buttons: up / down / confirm+power (shared) | 5 / 6 / 4, active-low |
| Power latch `PWR_HOLD` / `PWR_LOCK` | 45 / 46 |
| BQ27220 (0x55) SDA / SCL, 400 kHz | 1 / 0 |
| Charge status (BQ25616) | 40 |
| GT911 SDA / SCL / INT / RST, rail enable | 3 / 2 / 21 / 41, 42 *(milestone 04)* |

Panel: 800×480 SSD1677, same controller and geometry as the X4. FreeInk ships
`NO_FLIP` with orientation explicitly pending hardware validation — determine the
transform on a unit, do not guess it.

### Power latch, first

The Sticky uses a 74AHC1G79 latch arrangement: the firmware must drive
`PWR_HOLD` (GPIO45) and `PWR_LOCK` (GPIO46) HIGH or the board powers itself off
when the user releases the power button. This must happen **as close to `main()`
entry as possible** — before SD, display, or network setup. CrossPoint does the
same thing conceptually: hold the rails, then initialise.

FreeInk's `holdPowerRails()` calls `gpio_hold_dis()` before writing HIGH,
because a previous power-off may have latched the pin LOW with `gpio_hold_en()`,
and that hold survives a reset and a USB-powered deep-sleep wake — silently
defeating the write. Reproduce that release-then-drive ordering; it is the
difference between a board that boots and one that appears dead after a software
power-off.

### Three switched rails Calendula has no concept of

X3/X4 have no gated peripheral rails; the Sticky has three (EPD 47, SD 10, touch
42). Two consequences:

- **At boot:** FreeInk's `releaseSdRail()` exists because an unpowered SD card
  clamps the shared SCLK/MOSI lines, so the panel never hears a command. Power
  the SD rail and deassert its CS **before** first display use, even in a build
  that never mounts the card.
- **At sleep:** FreeInk's `powerDownRailsForSleep()` drives each rail to its OFF
  level and latches it with `gpio_hold_en()` (plus `gpio_deep_sleep_hold_en()`),
  because otherwise the gated peripherals stay powered through deep sleep —
  milliamps of standby drain. This is the main reason step 11 of the validation
  ladder exists.

### Display: reuse the driver, add a waveform seam

Reuse `display/src/epd/ssd1677.rs` and `fw/src/display_flush/ssd1677.rs` — but
not verbatim. Calendula hardcodes `RefreshMode::Fast => value | 0x1C`, the
incremental differential-update path. FreeInk's `ssd1677StickyConfig()` records
that on this panel `0x1C` does not select the partial/DU waveform; it promotes to
the full OTP waveform at roughly 1.7 s per refresh ("UI unusably slow"). Seeed's
own driver uses absolute sequences instead:

| | Sticky | X4 (today) |
|---|---|---|
| `0x22` full | `0xF7` | `0xF7` |
| `0x22` fast | `0xFF` (vendor partial/DU) | `\|0x1C` incremental |
| `0x22` half | falls back to full | `0xD7` |
| `0x3C` border init / full / fast | `0x01` / `0x01` / `0x80` | `0x80` / `0xC0` / `0xC0` |
| booster `0x0C` | `AE C7 C3 C0 80` | `AE C7 C3 C0 80` |

The border value must be re-written per refresh in the override path, or a
partial refresh leaves the panel's edge driven dark — FreeInk calls it "Sticky's
black ring". FreeInk also sets `grayPowerUpFirst` for this board because the
vendor sequences power the rails down after every refresh; Calendula has no
grayscale LUT path today, so that is informational rather than actionable, but it
is the same underlying fact: **the Sticky's panel does not stay powered between
fast refreshes the way the X4's does.** Expect that to matter for prestage and
the `Settled` timing that WS-A tuned on the X4/X3.

Do **not** wire in the runtime UC81xx controller probe. That solves XTeink batch
variation.

Measure fast-refresh time on hardware and report it. A working-looking display
that takes ~1.7 s per page is the exact failure this section exists to prevent.

### MicroSD

Port the pinout, don't redesign storage. `fw/src/sd_session.rs` already
implements the architecture the Sticky needs: one DMA SPI bus shared by display
and card, per-device CS, and a retune between the 400 kHz identification clock
and the data clock, restored to the display frequency afterwards. FreeInk notes
the Sticky's SD bus-sharing is *inferred* — Seeed's demo does not exercise the
card — so CS arbitration is the thing to prove on hardware, not to assume.

Display SPI clock: Calendula uses 20 MHz today; FreeInk defaults this controller
to 40 MHz and Seeed's demo uses a conservative 10 MHz. Start at Calendula's
20 MHz, and drop to 10 MHz if the shared bus proves flaky rather than debugging
two variables at once.

### Battery

The board capability added in milestone 02 selects BQ27220; the Sticky's gauge is
on its own bus (SDA GPIO1 / SCL GPIO0, 0x55, 400 kHz). **GPIO0 is an ESP32-S3
strapping pin.** The board init must not leave a pull state that corrupts boot
mode — this is why validation step 6 pairs gauge reads with repeated cold boots
rather than just checking that a reading looks plausible.

### Sleep and wake

Preserve today's semantics, on the S3 mechanism:

- **Normal sleep:** keep the power latch asserted, drive the peripheral rails off
  and hold them, enter S3 deep sleep armed on the power button via RTC `ext1`
  (`esp_sleep_enable_ext1_wakeup` / `ESP_EXT1_WAKEUP_ANY_LOW`), wake by reboot
  into `main()`, and trust retained e-paper contents under the existing
  `sleep_marker` + wake-cause conjunction.
- **Hard power-off (if Calendula ever wants one):** release the latch.

`hal_ext::rtc::woke_from_deep_sleep_gpio()` matches `SleepSource::Gpio`; the S3's
`ext1` wake reports a different cause, so the wake-cause check needs an S3 arm or
`deep_sleep_wake` silently reads false forever and every wake takes the full
waveform. The wake pin must be RTC-capable on `ext1` parts — confirm GPIO4
qualifies on the S3 before relying on it.

`fw/src/sleep_marker.rs` uses `#[esp_hal::ram(unstable(rtc_fast, persistent))]`;
confirm the same attribute and retention behaviour on the S3.

### Shared confirm/power button

GPIO4 is **both** confirm and power: a click confirms, a hold sleeps. Calendula's
model treats any power-button press as `Button::Power` → `SleepNow`, and the
input task owns the pin exclusively until it hands it to the power task for deep
sleep. The Sticky needs press-duration discrimination on that pin before it emits
either action, and the handoff protocol still has to work afterwards. Keep the
discrimination logic host-testable in `app_core::buttons` alongside
`ComboConfirmer`, where the existing timing rules live.

### Wi-Fi

`fw/src/sync_mem.rs`'s heap loan is tied to the C3 DRAM2 arrangement (see the
`fw/build.rs` comment about the radio's dram2 share and the runtime compensation
in `tasks/wifi.rs`). On the S3 the loan needs its own region sourcing. Keep the
one-way-loan-then-reset ownership model — that is an architectural rule, not a
C3 artifact.

## Scope

### Files

- **[NEW]** `fw/src/board/sticky.rs` — peripherals, rails, latch, wake source
- **[MODIFY]** `fw/src/main.rs` — power-latch assertion at the earliest point
- **[MODIFY]** `display/src/epd/ssd1677.rs` — per-board waveform/border config;
  X4 values unchanged as the default
- **[MODIFY]** `fw/src/display_flush/ssd1677.rs` — honour the config in the
  refresh sequences
- **[MODIFY]** `hal-ext/src/rtc.rs` — S3 `ext1` arm + wake-cause read
- **[MODIFY]** `fw/src/sleep_marker.rs` — confirm/adjust RTC-RAM retention on S3
- **[MODIFY]** `fw/src/sync_mem.rs`, `fw/src/tasks/wifi.rs` — S3 heap sourcing
- **[MODIFY]** `app-core/src/buttons.rs` — host-tested click-vs-hold rule for the
  shared confirm/power button

### Dependencies

- Depends on: `02-board-profile-extraction`
- Blocks: `04-gt911-input-integration`

### Notes

- Bring up in ladder order: latch → serial → display → SD → battery → sleep/wake
  → Wi-Fi. Each step's failure mode is much easier to read in isolation.
- Orientation is unknown until a unit says so. Ship whatever the hardware proves
  and record it.
- `AGENTS.md`'s no-heap and single-writer-owner rules apply unchanged: the board
  I/O task stays the only toucher of the EPD bus, SD CS, and framebuffer.

## Done when

- Cold boot from battery with USB disconnected keeps the board powered; the latch
  survives a software power-off and a subsequent boot.
- Serial diagnostics are reliable across repeated resets.
- Full and fast refreshes render correctly with the right orientation, and the
  **measured** fast-refresh time confirms the DU waveform rather than a promoted
  full waveform.
- The SD card mounts and reads known files.
- Hundreds of alternating SD-read / display-flush cycles pass with no
  cross-talk — neither device responds while the other's CS is asserted.
- BQ27220 readings are plausible and repeatable, and cold boots stay reliable
  with GPIO0 configured for I²C.
- Wi-Fi connects and completes the same sync path X3/X4 use.
- Deep sleep and power-button wake succeed repeatedly; the wake cause is read
  correctly, so a woken boot takes the fast waveform.
- The switched rails measurably stay off through deep sleep.
- The device is navigable end to end on its digital buttons: library, settings,
  open a book, page turn, back.
- X4/X3 display output is byte-identical to before the waveform-config change
  (goldens pass unblessed).
- `tools/check.sh all` passes, plus the Sticky build/clippy entry point.
