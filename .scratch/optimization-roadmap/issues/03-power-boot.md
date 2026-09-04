# WS-C: Power & boot — standby current, wake latency, battery

Status (2026-07-30): C1, C3, C4, C5 are on `main`. **C2 is no longer blocked
on an instrument** — the X3's fuel gauge can integrate a long deep sleep and
answer the question it has been parked on for four rounds, for one register
read and one `println!`. C6 is blocked on the X3 display path being
hardware-verified. C7 (serial logging) is new and affects both the device and
every measurement taken from it.

Owns: `fw/src/tasks/power.rs`, `fw/src/tasks/input.rs`, `hal-ext/src/rtc.rs`,
`hal-ext/src/bq27220.rs`, the planner-seed surface of `app-core/src/lib.rs`,
the boot-init region of `fw/src/tasks/display.rs`.
Do not touch: the flush/prestage region of the display task (WS-A), the wifi
task (WS-D).

Baseline facts: deep sleep is terminal — a wake is a cold boot
(`hal-ext/src/rtc.rs:22-27`); the radio is genuinely off until a session;
160 MHz race-to-idle is an explicit decision (`fw/src/main.rs:156-159`).

New on the boot path since #75 (2026-08-13): upload recovery replays any
standing install journal **before** the library is scanned or a cached catalog
is trusted. In the ordinary case that is one small read that finds nothing.
The case to know about when boot-to-first-paint is finally measured is the one
after an interrupted upload: recovery does real work and then retires the
catalog snapshot (`catalog_load` result `reclaimed`), so that boot pays a full
rescan. It is correct and it is rare, but it is a slow boot with no code change
behind it — do not let one land in a baseline capture unnoticed.

## Open

Order: C7 (affects every other measurement in the roadmap) → C2 (the gauge
run, which needs no instrument and settles four items at once) → C9 (one line)
→ C8 → C10 (only if C2's awake-idle number says the SoC dominates).

### C7 (S): serial logging is unconditional in release, and blocking whenever no USB host is attached

**New 2026-07-30, and it is both an optimization and a measurement-validity
problem.** `esp-println` is pulled in with default features, so the `auto`
printer decides its transport **at every call** by reading the USB SOF flag.
A host sends SOF every 1 ms, so with `espflash monitor` attached a print is a
few register writes into a 64-byte FIFO — nearly free. **Untethered on
battery there is no SOF, so every print falls through to the UART printer:**
ROM `uart_tx_one_char` per byte plus a `uart_tx_flush` every 32 bytes, at the
bootloader's 115200 baud, all inside `esp_sync::RawMutex::lock` — i.e. with
**global interrupts disabled**.

At 86.8 µs/byte, one X3 Fast page turn emits six lines totalling ≈418 B
≈ **36 ms — 8.6% of the 424 ms turn.** Boot-to-first-paint emits ~35–45 lines
≈ 1.4 KB ≈ **120 ms per cold boot and per wake.** Cold builds are worse:
`book_build.rs` alone has 59 print sites. `bench: input` runs on the
**interrupt-priority** executor, so it stalls SPI-DMA completion IRQs too.
*(Arithmetic from constants; the mechanism was read from the crate source.)*

A second survey found the same cost from the other side: the **tethered** path
busy-waits up to 50,000 iterations on a full FIFO, also inside a critical
section, and the SD session alone emits six lines per call on a path used by
every page turn. So logging is material in **both** regimes.

**The measurement consequence is the bigger half.** Every latency number this
project owns — the 424 ms turn, the 13 ms layout, every bench capture — was
taken with a monitor attached, which is a materially different regime from the
shipped device. The baselines are not wrong, but they are *tethered*
baselines, and nothing in the harness says so.

- Fix: a `serial-log` cargo feature (default on) gating the `bench:` and
  per-refresh chatter, or `esp-println/no-op` in release. Keep error and
  boot-identity lines. Effort S.
- **The one measurement:** print `USB_DEVICE_INT_RAW & 0b10` once at boot (one
  volatile read) to confirm which transport is live untethered, then stopwatch
  20 deliberate page turns on battery with no USB against the tethered
  424 ms median. 36 ms × 20 = 0.72 s, comfortably stopwatch-visible.
  **Kills it:** untethered turns match 424 ms.

### C2 (S–M code + hardware sign-off): hold GPIO states in deep sleep, and measure sleep current at last

`enter_deep_sleep_button` arms GPIO3 and sleeps with **no GPIO hold or
isolation**: SD CS (GPIO12), the shared SPI pins (GPIO8/10/7), and EPD
CS/DC/RST (GPIO21/4/5) all float. The SD card is hardware-powered with no
power switch, and a powered card with floating CS/CLK commonly leaks
100 µA–1 mA; a floating RST can pop the panel out of its ~1 µA sleep.

Fix: before `sleep_deep`, latch CS and RST high — RTC-domain pulls for
GPIO0–5, GPIO hold for the digital pins — leaving GPIO3's wake configuration
untouched. Then **measure**, which is the part that has never been done.

- Impact: potentially the largest standby win available. Deep-sleep current is
  *claimed* 10–15 µA and has **never been measured** (open checklist item 6,
  `docs/ARCHITECTURE.md`). The difference between ~15 µA and several hundred
  is months versus a week or two of shelf life. It may already be fine —
  nothing proves it either way, and that is the point.
- **Upstream now has a reference figure: 12.8 µA deep sleep, measured on an X3
  with a PPK2 at the battery terminals** (crosspoint `70faa29d`, 2026-08-12).
  **Corrected 2026-08-13 — that number is a *post-fix* figure, not a baseline,
  and the first version of this bullet got it wrong.** At the same commit,
  `startDeepSleep` already drives GPIO13 low and `gpio_hold_en`s it before
  sleeping (`lib/hal/HalPowerManager.cpp:89-90`, with the hold surviving via
  `esp_sleep_config_gpio_isolate` + `gpio_deep_sleep_hold_en`). So 12.8 µA is
  what an X3 draws **with the SD rail explicitly cut and latched** — which is
  precisely the configuration we do not have. It bounds what the hardware can
  reach; it says nothing about what it draws with GPIO13 left alone. That makes
  the rail a **stronger** suspect than "a high reading is our pin
  configuration" implied, not a co-equal one.
- **GPIO13 is the C3's flash SPIWP pad**, not a floating or pulled-up line
  (crosspoint's own comment, `HalPowerManager.cpp:17`: "unused in this board's
  DIO flash mode, rewired to the battery-latch MOSFET gate"). That is a third
  possibility this roadmap and the `x3-sd-rail-sleep-power` PRD both missed —
  the card powers up without us touching the pin because it is muxed to the
  flash controller at boot, not because something holds it high. Claiming it is
  the same class of move as `fw/src/main.rs:469`, which already claims GPIO12
  (SPIHD) as `sd_cs` on the same DIO-mode premise and works.
- **A larger suspect, and the only part of C2 investigable with no hardware:
  our deep sleep never runs esp-hal's digital-pad isolation pass at all.**
  `isolate_digital_gpio()` returns immediately unless the deep-sleep pad hold
  is enabled (`esp-hal-1.1.1/src/rtc_cntl/sleep/esp32c3.rs:147-158`), and
  esp-hal only ever **reads** `dg_pad_autohold_en` — the bit is never written
  anywhere in the crate, so the guard is always false for us. The loop being
  skipped carries esp-hal's own comment: *"make pad work as gpio (otherwise,
  deep_sleep bottom current will rise)"* (`:177`). crosspoint and freeink both
  enable the hold through ESP-IDF, so their isolation runs and ours does not.
  This affects **every unheld digital pad**, not just the SD rail, and it is a
  concrete named difference between the 12.8 µA firmware and ours. Check it
  before spending a 72-hour gauge run on the rail alone.
  - Blocker to be honest about: esp-hal exposes no public API for this, so
    enabling it means raw PAC writes against `hal-ext`'s
    `#![forbid(unsafe_code)]` and `fw`'s `#![deny(unsafe_code)]`. That is a
    design decision, not a patch.
- **The GPIO13 suspect is now confirmed board-conditional upstream.**
  crosspoint `9b1fb712` ("Guard GPIO13 power control for Xteink C3 boards
  only") restricts the GPIO13 SD-rail control to C3 boards, because X4 uses the
  same pin for `power.latch0`. Anything we write here must be board-gated the
  same way — driving GPIO13 unconditionally would toggle a power latch on the
  board we cannot test on.
- Risk: wake reliability. Pin maps differ between boards (GPIO0 is the ADC
  divider on X4 and I2C SCL to the BQ27220 on X3). The terminal-path pin
  handling changed in #27: the wake button now arrives through a cooperative
  `Option` handoff from the input task rather than a steal over a live handle
  — latch the other pins inside that flow.
- Risk: wake reliability must be re-checked after any pin latching —
  `bench.py sleep-sync --cycles 20` with no missed wakes. **X3 only**: the
  owner has no X4.

#### Measure it with the fuel gauge first — no meter, no disassembly

**The X3 carries a BQ27220 on the battery, and a fuel gauge is an integrating
meter that stays powered while the SoC is in deep sleep.** That answers C2's
actual question without the instrument this item has been parked on for four
rounds.

**Not** via the pogo cable, which is the obvious wrong turn: the 4-pin
magnetic connector is USB (`docs/FLASHING.md` — the 2-pin variant is
charge-only and will not enumerate), so a meter in series with it sits on the
wrong side of the charger, reads charge current rather than system draw, and
— worse — the act of plugging in changes the state under test, since VBUS
present means the charger is active and USB-Serial-JTAG is enumerated. Deep
sleep on battery is by definition the unplugged state. Floating CS/RST leakage
is a battery-rail phenomenon and is invisible from VBUS.

Procedure:

1. Charge, unplug, boot, record the gauge's accumulated-charge register.
2. Deep sleep, untouched, 24–72 h.
3. Wake, record again. ΔmAh ÷ elapsed hours = average standby current.

**The arithmetic is decisively in our favour**, because the question is "is
this fine or is something leaking?", not "is it 14 µA or 17 µA". Over 48 h,
15 µA is **0.72 mAh** against 300 µA's **14.4 mAh** — unmistakable even at
1 mAh gauge resolution. **If Δ is lost in the noise, that is the answer:**
nothing is leaking, and the GPIO-hold code above is unnecessary. This is the
rare case where the cheap experiment and the decisive one are the same.

Two preconditions:

- **The driver cannot do this yet.** `hal-ext/src/bq27220.rs` reads only
  voltage (`REG_VOLTAGE`), state of charge (`REG_STATE_OF_CHARGE`) and current
  (`REG_CURRENT`) — and it discards the current *magnitude*, using it solely
  for a sign test in `charging()`. At the gauge's 1 mA resolution the
  instantaneous register is useless here regardless; this needs an
  accumulated-charge / remaining-capacity register, which the driver does not
  have. One constant, one read, one `println!` at boot. Check the exact
  standard-command map against the BQ27220 datasheet rather than assuming —
  published register maps for this part disagree, and the existing
  `REG_CURRENT = 0x14` choice should be re-confirmed while in there.
- **Calibrate the technique before trusting a 72-hour delta.** Run it over
  30 minutes in a known state — screen on, idle, order 15–20 mA — and check
  the gauge's ΔmAh agrees. That tests whether its configured pack capacity is
  sane, which is what the whole method rests on.

**Honest limitation:** this measures *system* standby, including the gauge's
and the charger's own quiescent draw. For shelf life that is arguably the
number we want, but it cannot isolate SoC pin leakage from the rest. **If it
comes back high, that is when the series meter earns its place** — and only
then, since by that point there is a known problem worth opening the case for.

#### The meter session, once something indicts it

Demoted to follow-up, but the shape stands. Insertion point is the **battery
lead**, not the cable — meter in series between cell and board. Note the
instrument class matters: a handheld DMM on its µA range has enough burden
voltage that a wake spike (a panel refresh is tens of mA) can brown the device
out or reset it mid-measurement, and the span needed is ~15 µA to ~100 mA. A
PPK2- or Joulescope-class instrument handles that; a DMM generally will not.

**Then take four readings, not one.** This project knows *nothing measured*
about its own power draw at any operating point — not deep sleep, not awake
idle on a static page, not the panel refresh, not the SD card's standby draw
on a rail with no power switch. `bench.py` has no power channel, and C6's
"1–3 mA" and C10's whole premise rest on datasheet-typical SoC figures that
exclude the board, on a board where the peripherals are plausibly a majority
of the load. **Deep sleep, awake idle on a static page, awake idle immediately
after a hand-issued `CMD_POWER_OFF`, and the plateau during a page turn** —
that resolves C2, C6, C10 and the premise underneath C4 together, in under an
hour of bench time.

**SSD1677 shutdown sequence, surfaced 2026-08-22 from sibling-repo sweep.**
freeink-sdk `c60987a` added two changes to the SSD1677 backend our X4 builds
ship: a **10 ms wait after SWRESET**, and a documented **FAST/`0xFC`
`turnOff=true` shutdown path** (`0x3C=0x80`, `0x22=0x03`, `0x20`, 200 ms wait)
with async updates deferring shutdown until refresh completion. Neither exists
in our sleep-entry path. The SWRESET wait is cheap to try; the shutdown
sequence is a candidate for the C2 measurement session — if the panel's
internal charge pump is being left in a state that leaks during deep sleep,
this is where it would show up. Record, do not implement without the
measurement.

### C8 (M, hardware sign-off): sleep entry pays a second full-panel pass the next boot throws away

**New 2026-07-30.** The X3 sleep-screen flush runs `FULL_POWERED_STEPS`. The
sleep image lands at the **first** `DisplayRefresh`; everything after it —
`DelayMs(200)` → `WritePlane(Old)` → `DataStop` → `LoadBank(Fast)` →
`WritePlane(New)` → `DisplayRefresh(Fast)` → `WritePlane(Old)` → `DataStop` —
exists only to leave the controller staged for a next fast turn. **There is no
next turn:** the power task deep-sleeps, the wake is a cold boot, and
`init_panel` hardware-resets and re-whitens both DTM planes before anything
else. Every byte staged after the sleep image is discarded by construction.

≈ **657 ms of the ~4.1 s sleep entry (16%)**: 200 ms settle + 4 plane writes
at ~26 ms + one Fast `DisplayRefresh` at 379 ms. *Arithmetic from measured
constants.* Energy is negligible (~0.09 mAh/day) — **the win is the 0.65 s a
user waits after pressing Power**, every time. Fix: give `RefreshMode::PowerDown`
its own step list ending after the first `DisplayRefresh` + settle. The variant
already exists and already threads through `bank_for`, whose comment
("PowerDown never reaches a bank load") is currently false.

- **The refuting measurement costs nothing — the data is already on disk.**
  Count `bench: refresh` lines per sleep in any existing `sleep-sync` capture.
  **Two** per sleep entry (one long, one ~379 ms) confirms; one refutes.
- **Risk medium-high, and it gates the item:** the trailing Fast pass may be
  doing real anti-ghost settling on the final image. Do not land this while
  sleep-screen artifacts are unexplained — it is a prime new suspect if they
  recur. Needs X3 sign-off with photographs.

### C9 (S): the recovery combo runs on every deep-sleep wake and cannot possibly fire there

**New 2026-07-30.** `recovery_combo_confirmed` blocks for up to 7 gaps × 4 ms
= **28 ms** plus **48 blocking ADC conversions** before anything else happens
at boot. On a deep-sleep wake it can never succeed: the only armed wake source
is GPIO3, and anyone wanting recovery uses a reset or a battery pull, both of
which read `woke_from_deep_sleep_gpio() == false`. `woke_by_button` is already
computed one line above. Gating the combo on `!woke_by_button` removes 28 ms
and 48 ADC conversions from every wake and cannot lock anyone out of the
escape hatch. *Derived from constants; certain.*

*(Adjacent, flagged as a candidate only: `init_panel` on X3 costs ~144 ms on
the wake path, **92 ms of it fixed delay** — 20 ms high + 2 ms low + 20 ms
high reset, then a 50 ms settle — none of it attributed to any datasheet
figure in the code, where UC8253-class parts typically spec ~10 ms. Worth a
documented bisect on device, not a speculative edit: a short reset on a cold
panel is a bring-up failure mode and X4 cannot be regression-tested.)*

### C10 (L, blocked upstream): light sleep between page turns — costed honestly, not yet buildable

**Upstream measured this on an X3, 2026-08-12 (crosspoint `70faa29d`, "light-sleep
idle + refresh downclock — ~3.2x active reading time"). It answers C10's gating
question and does not lift C10's blocker — those are two different things, and
the entry below is edited on that basis rather than rewritten.**

Their method is one this roadmap has wanted for four rounds: a Nordic PPK2 at
the battery terminals, slim build, USB cable removed, 3.8 V. Their numbers, on
their firmware:

| Metric | Before | After |
|---|---|---|
| Idle on a static page | 9.68 mA | **2.78 mA** |
| Session average @ 30 s/page | ~11.6 mA | **~3.6 mA** |
| Post-turn 160 MHz tail | 21.2 mA × 3 s | eliminated |
| Deep sleep | 12.8 µA | unchanged |

**What that settles.** C10 says below that C2's awake-idle reading decides it:
≥15 mA means the SoC dominates and this is worth an L, ≤8 mA means the board
rails dominate and C10 should be closed. Upstream took the same board family
from 9.68 to 2.78 mA, so **~6.9 mA of a 9.68 mA idle was SoC and clock, not
rails** — the SoC dominates, decisively, and the "close it" branch is dead.
Note also that our 15 mA estimate for 160 MHz WFI is datasheet-typical and
upstream's whole-board idle came in *below* it; treat the arithmetic below as
an upper bound.

**What it does not settle, and must not be read as settling.** This is a
different firmware on a different runtime — an Arduino/ESP-IDF main loop
calling `esp_light_sleep_start()` directly. **Our TIMG0 blocker is untouched by
it**: they have no embassy time driver to desynchronize. The implementation
does not port. Their "before" also already had #852's flat 3 s full-speed
window, so their delta is not measured against a design like ours.

**Three things worth taking regardless of C10's fate:**

1. **Their guard list is design input we would otherwise buy with a bug.** Never
   light-sleep while a render lock is held, while Wi-Fi is up, or while USB is
   connected — light sleep kills the CDC link.
2. **That last guard collides with Tier 0d.** If light sleep is disabled
   whenever USB is attached, then a tethered capture cannot see this item at
   all — not "measures it slightly wrong", *cannot see it*. Any C10 verification
   plan has to be untethered from the start, which means the gauge method, not
   the serial log.
3. **The post-turn full-speed tail is a separate, unblocked item.** See C11.

Here is the costing, so nobody spends a week discovering the blocker:

**The optimistic arithmetic** (datasheet-typical, SoC only): idle at 160 MHz
in WFI ≈15 mA; C3 light sleep ≈130 µA; at a 15 ms cadence with ~1.2 ms
entry+exit the duty works out to ≈1.7 mA — **~9× off the SoC's idle draw**,
and a reader at 30 s/page is >98% idle. Largest theoretical awake lever here.

**Why it is blocked.** `esp_rtos::start(timg0.timer0, …)` makes TIMG0 both the
scheduler tick and embassy's clock, and TIMG0 is in the digital domain — it
does not survive C3 light sleep. Every `Timer::at` deadline, including the idle
leash and the 15 ms input tick, would drift by the whole sleep duration, and
`esp-rtos` 0.3 has no light-sleep-aware time driver. That is upstream. Also
the helper as written takes `Rtc` **by value**, consuming the handle the power
task needs afterwards for deep sleep, so any repeated tier needs that signature
changed regardless.

**And the board may swamp it:** if SD + charge pump + LDO quiescent are ~5 mA
of the ~20 mA, the win is 2×, not 9×. **C2's awake-idle reading decides it** —
a steady-state idle draw ≥15 mA means the SoC dominates and this is worth an
L; ≤8 mA means the board rails dominate, C6 and SD standby are the real items,
and this should be closed. Note the gauge method reaches this one too, and
faster than the sleep case: at ~15–20 mA a 30-minute screen-on idle window is
7.5–10 mAh, far above the gauge's resolution — so this is answerable in half
an hour without an instrument, and it doubles as the calibration run C2 needs
before trusting a 72-hour delta.

*(Recorded negative result, so it is not re-litigated: decimating the 15 ms
input tick is worth ~0.06 mA of ~15–20 mA. One tick is two ADC reads plus a
GPIO read and debounce — order 150–250 µs, ~1.0–1.7% duty — and the other 98%
is already WFI. The standing rejection below is correct and now has a number.
The only periodic waker with a static page on screen is that tick; the gauge is
30 s and every other task is parked on a channel or an infinite `pending()`.
The seam for any deeper tier is `esp_rtos::start_with_idle_hook`, which nothing
references today.)*

### C11 (S–M, unblocked): scale the CPU clock down when nothing needs 160 MHz

**New 2026-08-13, from crosspoint `70faa29d`.** We set the CPU clock once —
`esp_hal::Config::default().with_cpu_clock(CpuClock::_160MHz)` at
`fw/src/main.rs:367` — and never vary it again. Upstream's measurement puts a
number on what that costs: eliminating a 3 s post-turn tail at 160 MHz was
worth 21.2 mA × 3 s per page turn on its own, separately from their light-sleep
work.

**This is not C10 and is not blocked by C10's blocker.** Changing the CPU
frequency does not stop TIMG0, does not desynchronize embassy's clock, and does
not consume the `Rtc` handle. It is orthogonal to the light-sleep tier and can
land without it.

**Be honest about the two places it applies, because they are not equal:**

- **During a refresh's BUSY wait (379 ms of every 405 ms flush).** Upstream's
  version of this recovers a genuine spin — their `pollBusy()` polls the pin in
  a loop. **Ours does not spin**: `hal-ext/src/spi_dma.rs:99-134` awaits
  `busy.wait_for_low()` / `wait_for_high()` on a GPIO edge, so the CPU is
  already in WFI for the whole wait. The lever here is therefore only the
  difference between WFI at 160 MHz and WFI at 10 MHz — the clock tree and PLL,
  not the core. Real, but a fraction of what upstream measured, and **anyone
  quoting their 3.2× for this item is quoting the wrong number.**
- **During idle with a static page on screen**, where the same clock tree is up
  for a reader doing nothing at 30 s/page. This is the larger share and it does
  not need light sleep to collect.

- Impact: unknown until measured, and deliberately not estimated here — the
  point of the item is that it is cheap to try. Bracket it with the gauge
  method (C2), not with a tethered capture.
- Risk: the SPI bus clock divider and any timing derived from the CPU clock
  must be re-derived or re-asserted across a frequency change; a downclock
  taken while a DMA transfer is in flight is the obvious way to corrupt a
  flush. Raise the clock on the same lock that guards a render, as upstream
  does, rather than sprinkling calls.
- **Risk specific to light sleep, if C11 ever grows into C10: the SD rail and
  the flash-leakage workaround collide.** GPIO13 is the C3's flash SPIWP pad
  (see C2), and ESP-IDF's `CONFIG_ESP_SLEEP_FLASH_LEAKAGE_WORKAROUND` pulls the
  DIO-unused SPIWP pad low on light-sleep entry — which on the X3 is the SD
  power rail. crosspoint carries a bespoke `lightSleep()` for exactly this
  reason. esp-hal has no equivalent workaround today, so nothing bites us yet;
  the coupling is recorded because a future esp-hal that gains one would cut
  the card mid-session, and the symptom would look like SD corruption rather
  than a sleep bug.
- Verify: gauge-integrated idle window at each clock, plus the existing
  `bench: refresh` timings to prove no refresh got slower.

### C6 (M; blocked on the X3 display path being hardware-verified): power off the UC8253 charge pump on static pages

X3 leaves the booster on between turns (`SCREEN_POWERED` is cleared only by
`sleep_panel`) — roughly 1–3 mA while a static page is displayed. E-ink holds
an image at zero power, and `flush_plan` already models the powered-off state,
so a POF is transparently recovered by the next flush's PowerOn (~30–100 ms).
Add a ~20–30 s no-render timer in the display task's select loop that sends
`CMD_POWER_OFF` after the prestage settles.

- Risk: the whole UC8253 path is flagged UNVERIFIED on hardware
  (`uc8253.rs:12-14`); PON/POF sequencing and the `prev_staged` interaction
  must be preserved. X4 is unaffected — the SSD1677 threads `screen_on` per
  refresh.
- Verify: X3 `page-turn` (PON busy in the logs, latency budget), current on a
  held page, `thermal-run`.

## Done

- **C1** (#11) — the wake refresh took the Full waveform on every real wake,
  because deep sleep reboots the chip and `panel_shows_sleep_screen` was only
  ever set by a running session. Boot now reads the RTC wake cause plus an
  RTC-RAM marker the sleep handshake writes after the sleep frame settles, and
  seeds the planner with the sleep screen it knows the panel holds. A battery
  pull, crash, or a sleep whose final flush failed still pays Full, correctly.

  **Re-sized 2026-07-30: the "~3.5 s Full / ~2 s saved" figures were wrong for
  this board.** Measured on X3, n=16: **Full BUSY is 928 ms**, FastClean 455 —
  so C1 saves about **474 ms** of BUSY per wake, not 2 s. C1 is still correct
  and still worth having; only its advertised size changes. The 3.5 s figure
  traces to a June 10 2026 capture on an unnamed board. Note also that wake
  latency is *not* just the refresh — see C7 and the boot-attribution item in
  WS-F: nobody has measured wake-to-readable, and the app's own comment puts
  the restored book at "~1.5 s later", which suggests 2.5–3 s end to end.
- **C3** (#11, extended by #36) — the X3 battery gauge was polled at 66 Hz over
  clock-stretching I2C, at the top of every 15 ms input tick and *before* the
  nav ADC reads. #36 moved it to its own thread-executor task sampling every
  30 s; the gauge is fully off the input tick, and input runs at interrupt
  priority on both boards.
- **C4** (#11) — the flat 600 s idle timeout is now tiered: 10 min in Reading,
  3 min on menus, 10 min in Wireless. The biggest behavioural battery lever in
  the codebase (~25–50 mAh/day for ~10 walk-aways), and nearly free once C1
  made a wake fast.
- **C5** (#11) — the redundant second `init_panel` on every boot's first
  render is gone. On X3 that was a reset plus a 50 ms settle plus whitening
  both ~52 KB DTM planes.

## Do not re-propose

- **80 MHz CPU clock** — 160 MHz race-to-idle is an explicit decision.
- **Slower input polling** — the 15 ms tick is a deliberate latency trade. C3
  decimated only the battery channel, which is the part that was free.
- **Radio-off work** — already optimal; the radio is genuinely off until a
  session opens.
- **Light sleep** — `enter_light_sleep_timer` is a dead helper with zero call
  sites. A plausible future tier, but C4's tiering came first and took most of
  the win. Either wire it up or delete it; do not leave it as documentation.

Cross-cutting: `bench.py` has no power channel, and adding one is not the
prerequisite it looks like — **C2's gauge method turns the device into its own
power instrument**, and its output is two numbers printed over the serial link
the harness already reads. C6 still needs an external meter (it is a
milliamp-scale question on a rail the gauge sees only in aggregate); C1, C4
and C5 were verified from existing serial telemetry.

**Breadcrumb, observed once and never explained (2026-07-11):** an X3 PON busy
wait hit its 1 s ceiling (`PON busy_low=false 1000ms`) during a sleep-entry
Full refresh, then behaved normally afterwards. First suspect if X3 sleep
entry ever misbehaves, and worth watching for while doing C2 or C6 — both
touch that sequence.
