# WS-C: Power & boot — standby current, wake latency, battery

Status (2026-07-30): C1, C3, C4, C5 are on `main`. **C2 is the highest-ranked
open item in the whole roadmap after the double-repaint fix**, and it is
blocked on a device and a µA meter, not on code. C6 is blocked on the X3
display path being hardware-verified.

Owns: `fw/src/tasks/power.rs`, `fw/src/tasks/input.rs`, `hal-ext/src/rtc.rs`,
`hal-ext/src/bq27220.rs`, the planner-seed surface of `app-core/src/lib.rs`,
the boot-init region of `fw/src/tasks/display.rs`.
Do not touch: the flush/prestage region of the display task (WS-A), the wifi
task (WS-D).

Baseline facts: deep sleep is terminal — a wake is a cold boot
(`hal-ext/src/rtc.rs:22-27`); the radio is genuinely off until a session;
160 MHz race-to-idle is an explicit decision (`fw/src/main.rs:156-159`).

## Open

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
- Risk: wake reliability. Pin maps differ between boards (GPIO0 is the ADC
  divider on X4 and I2C SCL to the BQ27220 on X3). The terminal-path pin
  handling changed in #27: the wake button now arrives through a cooperative
  `Option` handoff from the input task rather than a steal over a live handle
  — latch the other pins inside that flow.
- Verify: hardware only — meter plus `bench.py sleep-sync --cycles 20` with no
  missed wakes. **X3 only**: the owner has no X4.

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

- **C1** (#11) — the wake refresh took the ~3.5 s Full waveform on every real
  wake, because deep sleep reboots the chip and `panel_shows_sleep_screen`
  was only ever set by a running session. Boot now reads the RTC wake cause
  plus an RTC-RAM marker the sleep handshake writes after the sleep frame
  settles, and seeds the planner with the sleep screen it knows the panel
  holds. A battery pull, crash, or a sleep whose final flush failed still pays
  the full 3.5 s, correctly.
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

Cross-cutting: `bench.py` has no power channel. C2 and C6 need an external
meter; C1, C4 and C5 verified from existing serial telemetry.

**Breadcrumb, observed once and never explained (2026-07-11):** an X3 PON busy
wait hit its 1 s ceiling (`PON busy_low=false 1000ms`) during a sleep-entry
Full refresh, then behaved normally afterwards. First suspect if X3 sleep
entry ever misbehaves, and worth watching for while doing C2 or C6 — both
touch that sequence.
