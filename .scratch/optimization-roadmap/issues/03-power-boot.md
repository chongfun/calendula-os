# WS-C: Power & boot — standby current, wake latency, battery

Status: C1+C3+C4+C5 DONE (#11; wake-cause gating logs 'main: deep_sleep_wake=', idle timeout tiered Reading 10 min / menus 3 min / Wireless 10 min). Next: C2 (device + µA meter). C6 still blocked on X3 display-path hardware verification. See PRD status for a once-observed X3 PON quirk. (items C1, C3, C4, C5 are code-verifiable; C2 and C6 need a device + meter for sign-off)

Owns: `fw/src/tasks/power.rs`, `fw/src/tasks/input.rs`, `hal-ext/src/rtc.rs`, `hal-ext/src/bq27220.rs`, planner-seed surface of `app-core/src/lib.rs`, boot-init region of `fw/src/tasks/display.rs`.
Do not touch: flush/prestage region of the display task (WS-A), wifi task (WS-D).

Baseline facts: deep sleep is terminal — wake is a cold boot (`hal-ext/src/rtc.rs:22-27`); radio genuinely off until a session (`fw/src/tasks/wifi.rs:62-107`); 160 MHz race-to-idle is an explicit decision (`fw/src/main.rs:156-159`); deep-sleep current claimed 10–15 µA but **never measured** (open checklist item 6, `docs/ARCHITECTURE.md:627`).

## C1 (Tier 1, S–M): Wake takes the 3.5 s Full waveform — the FastClean branch is dead code on hardware

`RefreshPlanner::mode_for` picks the ~1.5 s FastClean wake only when `panel_shows_sleep_screen` (`app-core/src/lib.rs:182-192`), which is set only by `record_sleep()` in a running session (`:234-239`). Deep sleep reboots the chip → fresh `RefreshPlanner::new()` (`fw/src/tasks/display.rs:66`) → flag false → every real wake pays `RefreshMode::Full` (~3.5 s). Nothing reads the wake cause (zero grep hits). ARCHITECTURE.md:606 promises 1.5 s — doc/code drift. Fix: read the RTC wakeup cause at boot; on deep-sleep GPIO wake, seed the planner with `panel_shows_sleep_screen = true` (the only deep-sleep entry path draws the sleep screen and waits for `DisplayAsleep`, `fw/src/tasks/power.rs:48-63` — panel content known by construction). Optionally skip the boot OTA pending-update SD probe (`display.rs:92-104`) on deep-sleep wake (updates stage via wifi session, which exits by software reset, never deep sleep).

- Impact: wake-to-readable ~2 s faster; +100–300 ms if the OTA probe is skipped; less energy per wake.
- Risk: if panel image was lost (battery pull, crash mid-sleep) FastClean can leave artifacts — gate strictly on the deep-sleep wake cause. X3's UC8253 analog exists but that path is hw-unverified.
- Verify: `bench.py sleep-sync --cycles 10` — wake `bench: refresh mode=... busy_ms` 3500 → ~1500; extend app-core planner unit tests. Fix ARCHITECTURE.md:606 in the same PR.

## C2 (Tier 1, S–M code + hardware sign-off): Hold GPIO states in deep sleep; measure sleep current at last

`enter_deep_sleep_button` arms GPIO3 and sleeps (`hal-ext/src/rtc.rs:22-27`) with no GPIO hold/isolation: SD CS (GPIO12), shared SPI (GPIO8/10/7), EPD CS/DC/RST (GPIO21/4/5) float. The SD card is hardware-powered with no power switch — a powered card with floating CS/CLK commonly leaks 100 µA–1 mA; a floating RST can pop the panel out of its ~1 µA sleep. Fix: before `sleep_deep`, latch CS/RST high (RTC-domain pulls for GPIO0–5, GPIO hold for digital pins), leaving GPIO3's wake config untouched. Then measure (µA meter in series across `sleep-sync` cycles).

- Impact: potentially the largest standby win — the difference between ~15 µA and several hundred µA is months vs ~1–2 weeks of shelf life. High uncertainty until measured; may already be fine, but nothing proves it.
- Risk: wake reliability — test both boards; pin maps differ (GPIO0 is ADC divider on X4, I2C SCL to BQ27220 on X3, `fw/src/main.rs:178-209`). The `steal_wake_button` pattern in `power.rs:48-73` shows how to re-materialize pins on the terminal path.
- Verify: hardware only — meter + `bench.py sleep-sync --cycles 20` (no missed wakes).

## C3 (Tier 1, S): X3 battery gauge polled at 66 Hz over clock-stretching I2C

`read_power` runs at the top of every 15 ms input tick (`fw/src/tasks/input.rs:148-157`). On X3 that's two BQ27220 `write_read`s (`hal-ext/src/bq27220.rs:55-59`), each clock-stretched "for milliseconds" (bus timeout raised to ~5 ms for it, `fw/src/main.rs:196-202`) — up to 20–40% of every tick, awaited *before* the nav/page ADC reads, adding input jitter. On X4 it's a wasted ADC oneshot per tick. Fix: decimate battery sampling to once per ~2–5 s (tick counter; keep the first-tick seed read feeding `battery_seeded`, `input.rs:178-186`); sample buttons every tick as today. UI already hysteresis-holds percent (`input.rs:261-269`).

- Impact: >99% gauge-traffic reduction on X3 (~0.5–2 mA + jitter removal); small-but-free on X4.
- Verify: `page-turn --turns 50` (input→render latency unchanged/better), serial `bench: input` tick regularity on X3, ammeter idle delta.

## C4 (Tier 1, S; land after C1): Shrink the 600 s fully-awake idle tail

`IDLE_TIMEOUT` is a flat 10 min (`fw/src/tasks/power.rs:8`); until it fires the device idles at 160 MHz with 66 Hz polling (order 10–20 mA) — ~2–5 mAh per walk-away. Once C1 makes wake ~1.5 s straight into the restored book, an aggressive timeout is nearly free. Drop to 3–5 min, or tier per view (keep 10 min in `AppView::Reading` for slow readers; 2–3 min on Home/Library/Settings — view context rides on `PowerEvent::Activity`, `fw/src/tasks/app.rs:104`).

- Impact: ~25–50 mAh/day for ~10 walk-aways — the biggest behavioral battery lever in the codebase.
- Verify: `sleep-sync` idle-timeout path, `reader-soak --minutes 30` (no mid-reading surprise sleeps). Progress is already flushed before display sleep — no state-loss risk.

## C5 (Tier 1, S): Redundant second `init_panel` on every boot's first render

`init_panel` runs at display-task start (`fw/src/tasks/display.rs:84-86`) AND again via the wake-init guard `!screen_on() && last_request().is_none()` — also true at boot (`:172-177`). X4: redundant reset+init (~tens of ms); X3: reset + 50 ms settle + whitening both ~52 KB DTM planes (~100–300 ms, `fw/src/display_flush/uc8253.rs:36-62`). Fix: drop the task-start init and let the guard own first init, or add a `panel_initialized` flag. Keep the aborted-sleep re-init path working (Activity during sleep handshake, `power.rs:52-62` / `record_sleep` clears `last_request`).

- Verify: serial timestamps between `display: init` and `display: wake init` lines in a `sleep-sync` run; X3 first-paint correctness.
- Coordination: same file as WS-A's A1 but a different region (boot vs flush loop); rebase carefully.

## C6 (Tier 3, M; blocked on X3 display path being hardware-verified): Power off the UC8253 charge pump on static pages

X3 leaves the booster on between turns (`SCREEN_POWERED` cleared only by `sleep_panel`, `fw/src/display_flush/uc8253.rs:31-34,93-113`) — ~1–3 mA while a static page displays. E-ink holds images at zero power; `flush_plan` already models the powered-off state, so a POF is transparently recovered by the next flush's PowerOn (~30–100 ms). Add a ~20–30 s no-render timer in the display task select loop sending `CMD_POWER_OFF` after prestage settles.

- Risk: whole UC8253 path is flagged UNVERIFIED on hardware (`uc8253.rs:12-14`); PON/POF sequencing and `prev_staged` interaction must be preserved. X4 unaffected (SSD1677 threads `screen_on` per refresh).
- Verify: X3 `page-turn` (PON busy in logs, latency budget), current on a held page, `thermal-run`.

## Do not re-propose

80 MHz clock (rejected — race-to-idle), slower input polling (15 ms was a deliberate latency trade; C3 decimates only the battery channel), radio-off work (already optimal), light sleep (dead helper `enter_light_sleep_timer` — plausible future tier but C4 first; also fix its stale doc mention).

Cross-cutting: bench.py has no power channel — C2/C3/C6 need an external meter; C1/C4/C5 verify from existing serial telemetry.

Suggested order: C2 (measure + hold) → C1 + C5 (wake latency) → C3 → C4 → C6.
