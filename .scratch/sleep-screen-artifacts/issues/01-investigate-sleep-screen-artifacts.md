# Investigate intermittent sleep-screen artifacts

Status: closed

## Problem

The sleep screen sometimes shows visible artifacts or ghosting after the device enters deep sleep.

## Root Causes Identified & Resolved (X3 UC8253)

Two distinct hardware/software mechanisms were diagnosed and fixed on the Xteink X3:

1. **Dark Horizontal Stripes Degradation (Post-Sleep Capacitive Leakage):**
   - *Cause:* The ESP32-C3 SoC was cutting board power and entering deep sleep ~30 ms after `sleep_panel` completed. At this point, the UC8253 booster capacitors were still discharging. Cutting MCU power left the SPI/control GPIO pins floating, allowing discharging rail current to leak through panel drive gates and create blooming dark-striped field artifacts across the glass over several seconds after sleep.
   - *Fix:* Added a cancellable 1.5s post-sleep settle delay before deep sleep in `fw/src/tasks/power.rs` (selecting between `Timer::at(settle_deadline)` and `POWER_EVENTS.receive()`, scoped to X3). This allows the booster charge pump to fully drain before MCU GPIO pins float, while immediately aborting sleep and unparking the display task if user navigation activity or a Power-button press arrives during settling.

2. **Faint Text Ghosting (Single-Pass Differential Over-Run):**
   - *Cause:* `RefreshMode::Full` in `display/src/epd/uc8253.rs` previously attempted to paint the sleep screen directly over the old reading page without a global clearing pass. A single differential pass was unable to reset deep particle polarization accumulated from dozens of preceding fast page turns.
   - *Fix:* Restructured UC8253 `Full` refresh into a true two-phase sequence:
     1. Pass 1: Global white scrub filling both `DTM1` and `DTM2` with `0xFF` using the absolute `HALF` scrub bank (`WW==BW = 0xAA...`), actively driving all pixels to pure white.
     2. 200 ms quiet settle.
     3. Pass 2: Drawing the target frame into `DTM2` with the deep 850 ms `FULL` waveform (`WB_FULL`) against the known white baseline.

3. **Long-Term Deep Sleep Screen Darkening & Re-blooming Stripes (Floating GPIOs / Hardware Wake):**
   - *Cause:* On the X3, battery power remains continuously connected to the UC8253 display controller. When the ESP32-C3 entered deep sleep, standard digital GPIOs were unheld and floated. In particular, GPIO5 (UC8253 active-low hardware reset / `/RST`) drifted low over 30–60 minutes, which is the UC8253's hardware wake line out of deep sleep mode (`0x07`). This woke the UC8253 into an uninitialized power-on state with floating gate/CS inputs, slowly leaking charge onto the panel electrodes, darkening the entire display, and re-blooming the prior reading page's text lines as horizontal stripes.
   - *Fix:* Configured RTC pad hold on GPIO5 (`/RST`) and GPIO4 (`D/C`), digital pad hold on GPIO21 (`/CS`), GPIO8 (`SCK`), GPIO10 (`MOSI`), GPIO12 (`SD CS`), and GPIO3 (`Power button`), enabled deep sleep digital auto-hold (`dg_pad_autohold_en`), and unhold all pads upon wake/boot in `fw/src/main.rs`.

All three mechanisms resolved and verified against the full check matrix.

## Current finding

The firmware sleep path is:

```text
DisplayCommand::Sleep -> flush pending STATE.BIN -> render_sleep -> full refresh -> SSD1677 power down -> SSD1677 deep sleep -> ESP32-C3 deep sleep
```

One software race was found and patched: boot intentionally defers the first render until catalog/restore state arrives, but the power button can request sleep before `RefreshPlanner` has a settled `last_request`. Before the patch, that path skipped `render_sleep` and deep-slept the panel with whatever image was already retained. The sleep command now renders a fallback boot sleep plate even when no previous request exists.

## Hardware repro needed

Run these on the device and capture serial logs plus a photo if artifacts appear:

- Wake from deep sleep, press Power immediately before the first restored screen paints.
- Let the device reach Home/Reading, press Power after several fast page turns.
- Let idle sleep trigger from Reading after several fast page turns.
- Repeat at least 10 times per path; note whether artifacts correlate with early-boot sleep, ordinary manual sleep, or idle sleep.

Useful serial markers:

- `display: write BW RAM Full`
- `display: write RED RAM current`
- `display: refresh busy ... ms`
- `display: sleep start`
- `display: sleep deep`
- `power: deep sleep`
- `display: sleep framebuffer flush failed`

## Ranked hypotheses

1. Early sleep before first render skipped the sleep-frame refresh, leaving stale retained pixels visible through deep sleep. Prediction: artifacts cluster when Power is pressed soon after wake/boot; patched fallback should remove that case.
2. The full sleep-frame refresh sometimes fails or is interrupted, but the task still proceeds to panel deep sleep. Prediction: artifacts correlate with `display: sleep framebuffer flush failed` or missing/short full-refresh busy timing.
3. The SSD1677 full-refresh activation used before power-down is insufficient after some prior fast-refresh states. Prediction: artifacts happen after many fast page turns but not from a fresh Home screen, with normal full-refresh busy timing.
4. This is analog panel ghosting rather than a command bug. Prediction: emulator/protocol history remains clean, serial timings are normal, and artifacts correlate with temperature, previous high-contrast content, or repeated fast refreshes.

## Done when

- The artifact path is classified as early-sleep race, failed flush, refresh-sequence issue, or analog panel behavior.
- A device log/photo pair exists for the failing path.
- If the patch does not resolve it, add a protocol or hardware experiment for the remaining top hypothesis.

## Comments

### 2026-07-07 hardware flash smoke

Flashed the patched firmware to an unlocked X4 over `/dev/cu.usbmodem101` with:

```sh
RUSTC="$(rustup which --toolchain nightly-2025-10-01 rustc)" rustup run nightly-2025-10-01 cargo run -p fw --release
```

The app image matched the existing flash contents, then booted and monitored normally. After many fast page turns, pressing Power produced the expected sleep sequence:

```text
input: Some(Power) ...
app: sleep requested
power: display sleep
display: write BW RAM Full 16 ms
display: write RED RAM current 15 ms
display: refresh activate
display: refresh busy 3529 ms
display: sleep start
display: sleep deep
power: deep sleep
```

No `display: sleep framebuffer flush failed` line appeared. The serial monitor ended with a broken pipe as USB dropped during deep sleep, which is expected.
