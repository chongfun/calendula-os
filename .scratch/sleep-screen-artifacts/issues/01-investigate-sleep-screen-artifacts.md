# Investigate intermittent sleep-screen artifacts

Status: needs-info

## Problem

The sleep screen sometimes shows visible artifacts or ghosting after the device enters deep sleep.

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
