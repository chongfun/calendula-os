# PRD: Idle light sleep & static-page power management

Status: ready-for-agent

## Summary

Port the power-management techniques from CrossPoint PR #2525 to CalendulaOS without compromising:

- Embassy timer correctness and task scheduling,
- button responsiveness and debounce reliability,
- SPI/SD/display bus safety and single-writer ownership,
- USB Serial JTAG stability and bench telemetry,
- hard-power cut and deep-sleep behavior,
- `#![no_std]` reading path without heap allocation,
- and X3/X4 stack and memory budgets.

CrossPoint measured a substantial reduction in static-page idle current on Xteink X3 (falling from 9.68 mA to 2.78 mA) by placing the ESP32-C3 into light sleep while sitting idle on a book page.

CalendulaOS cannot independently measure current consumption with external bench instrumentation (e.g. Nordic PPK2). The goal is therefore not to reproduce CrossPoint's exact battery-life or current numbers.

The implementation target is:

> **Safely place the ESP32-C3 into light sleep for most of the time that CalendulaOS is sitting idle on a static page, while preserving all existing functional and timing invariants.**

Success is established through runtime correctness, hardware stability, sleep/wake cycles, input responsiveness, and measured **light-sleep residency** from firmware telemetry (`elapsed_sleep_ms / eligible_idle_ms`).

EPD BUSY sleeping and CPU downclocking are explicitly outside this initial implementation.

---

## Context & Motivation

CalendulaOS currently runs the ESP32-C3 at a constant 160 MHz. When resting on a reading page:
1. `input_task` polls the ADC resistor ladder every 15 ms via `Timer::after_millis(POLL_MS)`.
2. Between polls, `esp_rtos` runs `wait_for_interrupt()` (`wfi`), keeping the 160 MHz PLL and APB peripherals energized (drawing ~10–20 mA).
3. The power task's idle leash is 10 minutes in Reading before terminal deep sleep. Slow readers therefore spend over 98% of active reading time idling in 160 MHz WFI.

In CrossPoint PR #2525, entering light sleep between input polls when idle dropped average static-page draw by ~70%. However, CalendulaOS is built on Embassy, `esp-rtos`, and `esp-hal` `#![no_std]`, requiring an architecture suited to cooperative async executors rather than FreeRTOS loops.

---

## Scope

### In Scope

#### Phase 0 — Runtime and Hardware Proof
Prove that timer-driven light sleep is compatible with:
- the current pinned `esp-hal` / `esp-rtos` / Embassy runtime,
- CalendulaOS timing and `Instant` monotonic progression,
- GPIO13 battery-latch behavior on X3,
- hard shutdown / deep sleep power-off,
- and USB Serial JTAG stability.

#### Phase 1 — Production Static-Page Idle Sleep
Add production static-page idle light sleep with:
- `power_task` as the sole sleep caller,
- an RAII global sleep-inhibit guard mechanism (`SleepInhibitGuard`),
- tri-state USB-aware admission (`Unknown`, `Present`, `Absent`),
- debounce-aware admission,
- short bounded sleep intervals (e.g. 50 ms slices),
- and sleep-residency telemetry.

### Out of Scope / Deferred

- **EPD BUSY GPIO wake (Phase 2):** In CalendulaOS, BUSY waits already await GPIO level/edge events cooperatively. Adding light sleep during refresh requires BUSY wake-pin lifecycle management and is deferred.
- **CPU downclocking (Phase 3):** Race-to-sleep at 160 MHz completes ADC polling in <0.3 ms and returns to light sleep, consuming less total energy than stretching execution at 10 MHz. Downclocking adds clock-tree complexity without local instrumentation to justify it.
- **External battery-life / current claims:** No unverified current or battery-life multipliers will be claimed without independent electrical measurement.
- **X4 hardware validation:** X3 is the primary hardware target; X4 will maintain build and clippy coverage, with light sleep feature-gated until verified on X4 hardware.

---

## Architecture & Design Decisions

### 1. `power_task` is the sole light-sleep owner

`input_task`, `display_task`, storage code, and HAL bus helpers must not independently call `Rtc::sleep_light()`.

`power_task` already owns the RTC and runs in normal task context. All system-wide light sleep is initiated there. Other subsystems publish activity state, acquire sleep inhibitors, or expose debounce status, but never suspend the chip themselves.

This provides a single place responsible for:
- RTC ownership
- Sleep admission
- Wake-source setup and cleanup
- USB policy
- Latch policy
- Telemetry reporting

```text
┌───────────────────────────────────────────────────────────┐
│ input_task — InterruptExecutor                            │
│                                                           │
│ Poll input as today                                       │
│ Publishes:                                                │
│   - last input activity timestamp                         │
│   - debounce pending flag                                 │
│                                                           │
│ Never enters light sleep                                  │
└──────────────────────────┬────────────────────────────────┘
                           │
                           ▼
┌───────────────────────────────────────────────────────────┐
│ Sleep coordination (fw::power_state)                      │
│                                                           │
│ SLEEP_INHIBITORS (AtomicU32)                              │
│ USB state (Unknown | Present | Absent)                    │
│ input activity                                           │
│ debounce state                                           │
│ device capability                                       │
└──────────────────────────┬────────────────────────────────┘
                           │
                           ▼
┌───────────────────────────────────────────────────────────┐
│ power_task                                                │
│                                                           │
│ sole RTC / light-sleep caller                             │
│                                                           │
│ if:                                                       │
│   idle >= 1000 ms                                         │
│   inhibitors == 0                                         │
│   USB == Absent                                           │
│   no debounce pending                                     │
│   light sleep supported                                   │
│                                                           │
│ then:                                                     │
│   sleep for bounded timer slice (50 ms)                   │
│                                                           │
│ wake:                                                     │
│   record telemetry                                        │
│   reevaluate everything                                   │
└───────────────────────────────────────────────────────────┘
```

---

### 2. Global RAII sleep-inhibit mechanism

Light sleep suspends the entire chip. The safety invariant is:
> **No subsystem requiring continuous CPU execution or active bus clocks may be active when sleep begins.**

Introduce a global inhibitor count with RAII guards:

```rust
static SLEEP_INHIBITORS: AtomicU32 = AtomicU32::new(0);

#[must_use]
pub struct SleepInhibitGuard {
    _private: (),
}

pub fn inhibit_light_sleep() -> SleepInhibitGuard;
pub fn light_sleep_inhibited() -> bool;
```

Requirements:
- Allocation-free, zero-sized guard.
- Bounded, underflow-protected (`fetch_sub` with saturation/assertion).
- RAII-driven: creating increments, dropping decrements.

---

### 3. Race-safe sleep admission

The final transition must be:
```text
check sleep eligibility
→ no scheduler yield / no .await
→ enter light sleep
```
There must be no `.await` between the final safety check and `Rtc::sleep_light()`. If inhibitor acquisition can occur from an interrupt context, protect the final admission check with an appropriate critical section.

---

### 4. Do not directly mutate SYSTIMER

Do not implement direct RTC-to-SYSTIMER counter mutation (`SystemTimer::set_unit_value`). The current HAL/runtime interfaces do not make this a safe firmware-level operation while Embassy alarms are armed.

Phase 0 will empirically establish how the current runtime behaves across light sleep. If compensation is required, it must be solved through a supported runtime time-driver mechanism.

---

### 5. Preserve GPIO13 hard-power semantics

On Xteink boards, GPIO13 controls the battery latch MOSFET gate:
- **Awake:** GPIO13 keeps battery power asserted.
- **Light sleep:** GPIO13 remains asserted (not pulled low by flash workarounds).
- **Hard shutdown / deep sleep:** Any sleep-related hold is released, and existing power-cut behavior still functions.

Do not permanently hold GPIO13 HIGH from startup unless testing proves that this does not interfere with deep-sleep shutdown. Treat light sleep and hard shutdown as separate hardware states.

---

### 6. Tri-state USB host detection

USB host detection uses:
```rust
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UsbHostState {
    Unknown,
    Absent,
    Present,
}
```
At boot:
```text
Unknown
→ read SOF frame index from USB_DEVICE
→ remain awake >= 10 ms
→ read again
→ Absent (if unchanged) or Present (if changed)
```

Rules:
- `Unknown` => sleep prohibited.
- `Present` => sleep prohibited.
- `Absent` => sleep may be considered.

Sample during normal operation so hot-plug / hot-unplug is detected. USB activity prohibits light sleep to protect USB Serial JTAG and bench telemetry.

---

### 7. Normal CPU frequency maintained

Keep normal 160 MHz CPU frequency while awake. Race-to-sleep: wake, complete required polling or processing quickly, and return to light sleep. No 10 MHz downclocking.

---

## Phased Implementation Plan

### Phase 0 — Runtime and Hardware Proof

Phase 0 adds no automatic idle-sleep policy. Its purpose is to verify that the runtime and hardware can safely support Phase 1.

#### 0.1 Controlled timer-only light sleep primitive
In `hal-ext/src/rtc.rs`, expose a timer-only light-sleep operation:
- Receives `&mut Rtc<'_>`,
- Arms `TimerWakeupSource`,
- Enters `rtc.sleep_light(&[&wakeup])`,
- Returns after wake with no SYSTIMER mutation and no unsafe code.

#### 0.2 Establish Embassy time semantics across sleep
Create a hardware test exercising:
- **Case A (Elapsed time):** Sleep ~50 ms. Record whether `after - before` advances or pauses.
- **Case B (Future deadline):** Arm `Timer::after(200 ms)`, sleep 50 ms. Verify the timer fires at a sensible wall-clock time after wake.
- **Case C (Expired deadline):** Arm `Timer::after(20 ms)`, sleep 50 ms. Verify the timer fires immediately upon wake with no hang, clock reversal, or alarm corruption.
- **Case D (Multi-thousand cycle soak):** Repeatedly sleep and verify `Instant` remains strictly monotonic with no deadlocks.

#### 0.3 GPIO13 battery latch verification on X3
On X3 hardware:
1. Boot on battery power.
2. Enter timer-only light sleep.
3. Wake and repeat 100+ times.
4. Verify no spontaneous power cuts.
5. Invoke normal deep-sleep shutdown (`enter_deep_sleep_button`).
6. Verify the device powers off completely.

#### 0.4 USB host detection
Implement `usb_detect` using `USB_DEVICE::regs().fram_num().read().sof_frame_index().bits()`:
- Verify boot attached vs detached.
- Verify hot-plug and hot-unplug transitions.
- Verify counter rollover.

#### 0.5 Sleep telemetry proof
Add initial telemetry counters:
```rust
pub struct LightSleepStats {
    pub entries: u64,
    pub requested_ms: u64,
    pub elapsed_ms: u64,
}
```
Prove that the MCU enters light sleep and remains asleep for approximately the requested duration.

#### Phase 0 Exit Criteria
- [ ] Timer-only light sleep executes repeatedly without panics.
- [ ] Embassy timing semantics are characterized and proven safe.
- [ ] Armed timers fire deterministically without alarm corruption.
- [ ] No direct SYSTIMER mutation is required.
- [ ] GPIO13 remains asserted through light sleep on X3.
- [ ] Normal hard shutdown cuts power cleanly.
- [ ] USB host detection reliably distinguishes `Present` from `Absent`.
- [ ] Telemetry proves light-sleep intervals match requested durations.

---

### Phase 1 — Production Static-Page Idle Sleep

#### 1.1 Sleep coordination (`fw/src/power_state.rs`)
Expose:
```rust
pub fn inhibit_light_sleep() -> SleepInhibitGuard;
pub fn light_sleep_inhibited() -> bool;
pub fn set_usb_state(state: UsbHostState);
pub fn usb_state() -> UsbHostState;
```

#### 1.2 Instrument sleep-sensitive operations
Acquire `SleepInhibitGuard` around operations requiring continuous CPU or bus activity:
- SD transactions and FAT operations (`sd_session.rs`).
- Active SPI DMA display transfers (`display_flush`).
- Wi-Fi sync session (`tasks/wifi.rs`).
- Long book build / decompression loops (`book_build.rs`).

#### 1.3 Input activity publishing (`fw/src/tasks/input.rs`)
Expose:
- `last_activity_time(): Instant`
- `is_debounce_pending(): bool`
Do not change input polling cadence in this step; maintain 15 ms polling.

#### 1.4 Production admission in `power_task`
In `tasks/power.rs`:
```rust
const IDLE_THRESHOLD: Duration = Duration::from_secs(1);
const LIGHT_SLEEP_SLICE: Duration = Duration::from_millis(50);

if idle_duration >= IDLE_THRESHOLD
    && !power_state::light_sleep_inhibited()
    && power_state::usb_state() == UsbHostState::Absent
    && !input_state::is_debounce_pending()
    && power_state::device_supports_light_sleep()
{
    // Enter 50 ms light sleep slice
    rtc.sleep_light(&[&timer_wake]);
    stats.record_sleep(...);
}
```
After wake: immediately re-evaluate USB, input, inhibitors, and idle deadlines.

#### 1.5 Sleep-residency telemetry
Expose telemetry counters:
```rust
pub struct LightSleepStats {
    pub entered: u64,
    pub requested_ms: u64,
    pub elapsed_ms: u64,
    pub timer_wakes: u64,
    pub gpio_wakes: u64,
    pub refused_inhibited: u64,
    pub refused_usb_unknown: u64,
    pub refused_usb_present: u64,
    pub refused_debounce: u64,
    pub refused_not_idle: u64,
}
```
Report residency: `residency = elapsed_sleep_ms / eligible_idle_ms`.

---

## Verification & Acceptance

### Functional Verification Checklist
1. **Static reading page (60 s):** Leave book open. Telemetry confirms high sleep residency.
2. **Static library page:** Verify menu idle enters sleep without stranded inhibitors.
3. **First key after idle:** Press next/prev page. Verify press is not dropped, duplicated, or subject to excessive latency.
4. **Repeated idle/active cycles:** Navigate rapidly, rest, navigate again. Verify clean transitions.
5. **Storage inhibitor:** Verify light sleep is blocked during SD reads/writes.
6. **Display inhibitor:** Verify light sleep is blocked during SPI DMA transfers.
7. **Wi-Fi inhibitor:** Verify light sleep is blocked during sync sessions.
8. **USB transitions:** Test boot attached, boot detached, hot-plug, hot-unplug.
9. **Hard shutdown:** Verify deep sleep shuts down completely after extensive light-sleep cycles.
10. **Multi-thousand cycle soak:** Verify no panics, lockups, clock reversals, or timer stalls.

### Automated Checks
```sh
tools/check.sh fmt
tools/check.sh fast
tools/check.sh emulator
tools/check.sh firmware

# Dual-device X3 checks
tools/cargo.sh clippy --target riscv32imc-unknown-none-elf -p fw --features device-x3 -- -D warnings
tools/cargo.sh build --target riscv32imc-unknown-none-elf --release -p fw --features device-x3
```

---

## Claims Allowed by This Work

The implementation may claim:
> CalendulaOS enters ESP32-C3 light sleep while idle on static pages.

It may claim:
> Firmware telemetry demonstrates that the device spends most eligible static-page idle time in light sleep.

It may reference CrossPoint as motivation:
> CrossPoint measured substantial idle-current savings from a similar technique on X3 hardware.

It must **not** claim unverified current (mA) reductions or battery-life multipliers without independent physical measurement.

---

## Done When

- [ ] `power_task` is the only production caller of light sleep.
- [ ] No direct SYSTIMER counter mutation is used.
- [ ] Embassy timer behavior across light sleep is characterized and verified safe.
- [ ] Multi-thousand cycle soak completes without timing corruption.
- [ ] Global `SleepInhibitGuard` is implemented and instrumented across SD, display DMA, and Wi-Fi.
- [ ] Final check-to-sleep transition is race-free.
- [ ] Tri-state USB detection prevents light sleep when attached or unsettled.
- [ ] Button debounce state prevents premature sleep.
- [ ] GPIO13 remains asserted during light sleep on X3.
- [ ] Normal hard shutdown cuts power cleanly.
- [ ] No button presses are lost or duplicated after idle.
- [ ] Static reading page telemetry shows high light-sleep residency.
- [ ] Dual-device build checks (X4 and X3) pass cleanly.
- [ ] The PR description records residency and soak-test telemetry evidence without unverified current claims.

---

## Comments

**2026-10-13** — Opened on branch `optimization-roadmap`. Formulated from analysis of CrossPoint PR #2525 and adapted to CalendulaOS's Embassy/`esp-rtos` single-writer architecture. Defers EPD BUSY sleep and CPU downclocking to prioritize static-page idle sleep with measurable residency telemetry.
