//! Phase 0 of the idle light-sleep work: prove the primitive on the device
//! (`light-sleep-selftest` feature only, and kept out of release builds the
//! way `powercut-selftest` is).
//!
//! Light sleep stops the USB-Serial-JTAG clock, so a host watching the
//! serial port sees the device drop on every slice and the policy refuses
//! to sleep under a host. The proof therefore runs untethered: the device
//! waits for the cable to go, takes its slices with nobody watching,
//! keeps the measurements in RAM, and prints them the moment a host is back
//! on the bus. Boot attached, unplug, wait, plug back in, read the report.
//!
//! What it measures, each as one `bench: light_sleep_selftest case=...`
//! line with its own `pass=`:
//!
//! - `A`: twenty 50 ms slices; how far the Embassy clock and the RTC each
//!   moved across one. The Embassy clock is SYSTIMER, which stops with the
//!   crystal, so the expected shape is RTC ~50 ms and Embassy ~0 ms. The
//!   policy does not depend on which it is; this line records it.
//! - `B`: `Timer::after(200 ms)` armed, one slice taken, the timer awaited.
//!   It must fire, once, at a sensible time on both clocks.
//! - `C`: `Timer::after(20 ms)` armed, a 50 ms slice taken over it. It must
//!   fire promptly on the wake with no hang and no clock reversal.
//! - `D`: a thousand slices back to back on battery: `Instant` strictly
//!   monotonic, every slice near its request, and the device still alive,
//!   which is also the GPIO13 battery-latch check the PRD asks for.
//! - `clock`: a fixed spin timed by SYSTIMER before and after the slices, so
//!   a CPU left on the crystal instead of the PLL after a wake shows as a
//!   ratio, not a feeling.
//! - `usb`: the host verdict at boot, when the cable left, when it returned.
//!
//! Runs inside the power task before its normal loop, because that task is
//! the RTC's only owner. Presses during the run are drained and discarded,
//! and the Power button does nothing until the report has printed; the
//! normal loop then takes over, which is how the recipe's deep-sleep
//! shutdown check is reached.

use app_core::light_sleep::{UsbHostState, SLICE_MS};
use embassy_time::{Duration, Instant, Timer, WithTimeout};
use esp_hal::rtc_cntl::Rtc;

use crate::power_state;
use crate::POWER_EVENTS;

/// Slices in case A.
const CASE_A_SLICES: u32 = 20;
/// Slices in case D. At 50 ms each plus the wake gap, about a minute.
const CASE_D_SLICES: u32 = 1_000;
/// Case B arms this far ahead and sleeps one slice under it.
const CASE_B_ARM_MS: u64 = 200;
/// Case C arms this far ahead, inside the slice taken over it.
const CASE_C_ARM_MS: u64 = 20;
/// A slice is "near its request" inside this band of RTC milliseconds. The
/// RTC slow clock is the calibrated internal RC oscillator, a few percent
/// out at worst; the floor also catches a rejected sleep (returns at once).
const SLICE_OK_MIN_MS: u64 = 40;
const SLICE_OK_MAX_MS: u64 = 100;
/// How long one slice may wait for the inhibitors to clear before it is
/// counted as skipped rather than taken.
const INHIBIT_WAIT_MS: u64 = 2_000;
/// Iterations of the CPU-clock probe spin. Order 4 ms at 160 MHz.
const SPIN_ITERATIONS: u32 = 200_000;
/// Case B and C wait this long for their timer before calling it a hang.
const TIMER_HANG_MS: u64 = 5_000;

#[derive(Default)]
struct Results {
    usb_at_start: u8,
    unplugged_rtc_ms: u64,
    replugged_rtc_ms: u64,
    spin_before_us: u64,
    spin_after_us: u64,
    a_taken: u32,
    a_skipped: u32,
    a_embassy_min_ms: u64,
    a_embassy_max_ms: u64,
    a_rtc_min_ms: u64,
    a_rtc_max_ms: u64,
    b_fired: bool,
    b_taken: bool,
    b_embassy_ms: u64,
    b_rtc_ms: u64,
    c_fired: bool,
    c_taken: bool,
    c_embassy_ms: u64,
    c_rtc_ms: u64,
    c_after_wake_rtc_ms: u64,
    d_taken: u32,
    d_skipped: u32,
    d_reversals: u32,
    d_short: u32,
    d_long: u32,
    d_rtc_total_ms: u64,
    d_embassy_total_ms: u64,
    d_max_awake_gap_ms: u64,
}

/// One slice through the same primitive and the same final check the
/// policy uses. `None` when the inhibitors would not clear in time: the
/// device was busy (an SD session, a flush, the gauge) and the slice is
/// skipped rather than forced over it.
async fn slice(rtc: &mut Rtc<'_>) -> Option<(hal_ext::rtc::LightSleepSlice, u64)> {
    let give_up = Instant::now() + Duration::from_millis(INHIBIT_WAIT_MS);
    loop {
        drain_power_events();
        let outcome = critical_section::with(|_| {
            if power_state::light_sleep_inhibited() {
                return None;
            }
            let before = Instant::now();
            let slice =
                hal_ext::rtc::light_sleep_timer(rtc, core::time::Duration::from_millis(SLICE_MS));
            let after = Instant::now();
            Some((slice, after.saturating_duration_since(before).as_millis()))
        });
        if let Some(taken) = outcome {
            // One poll per slice, as the policy does.
            power_state::POLL_NUDGE.signal(());
            return Some(taken);
        }
        if Instant::now() >= give_up {
            return None;
        }
        Timer::after_millis(5).await;
    }
}

/// Presses and view changes arriving during the run would fill the power
/// channel and stall the app task on its send; they are dropped here.
fn drain_power_events() {
    while POWER_EVENTS.try_receive().is_ok() {}
}

async fn wait_for_usb(state: UsbHostState) {
    loop {
        drain_power_events();
        if power_state::usb_state() == state {
            return;
        }
        Timer::after_millis(250).await;
    }
}

/// A fixed amount of CPU work timed by SYSTIMER, which runs off the crystal
/// whatever the CPU clock does. Same before and after the slices means the
/// PLL came back; a probe four times slower means the CPU woke on the
/// 40 MHz crystal.
fn spin_probe_us() -> u64 {
    let start = Instant::now();
    let mut acc = 0u32;
    for i in 0..SPIN_ITERATIONS {
        acc = core::hint::black_box(acc.wrapping_add(i));
    }
    core::hint::black_box(acc);
    start.elapsed().as_micros()
}

fn in_band(ms: u64) -> bool {
    (SLICE_OK_MIN_MS..=SLICE_OK_MAX_MS).contains(&ms)
}

async fn case_a(rtc: &mut Rtc<'_>, r: &mut Results) {
    r.a_embassy_min_ms = u64::MAX;
    r.a_rtc_min_ms = u64::MAX;
    for _ in 0..CASE_A_SLICES {
        let Some((taken, embassy_ms)) = slice(rtc).await else {
            r.a_skipped += 1;
            continue;
        };
        let rtc_ms = taken.rtc_elapsed_ms();
        r.a_taken += 1;
        r.a_embassy_min_ms = r.a_embassy_min_ms.min(embassy_ms);
        r.a_embassy_max_ms = r.a_embassy_max_ms.max(embassy_ms);
        r.a_rtc_min_ms = r.a_rtc_min_ms.min(rtc_ms);
        r.a_rtc_max_ms = r.a_rtc_max_ms.max(rtc_ms);
        // Awake between slices, so the input task's nudged poll lands.
        Timer::after_millis(5).await;
    }
    if r.a_taken == 0 {
        r.a_embassy_min_ms = 0;
        r.a_rtc_min_ms = 0;
    }
}

fn case_a_pass(r: &Results) -> bool {
    r.a_taken == CASE_A_SLICES
        && in_band(r.a_rtc_min_ms)
        && in_band(r.a_rtc_max_ms)
        // The Embassy clock cannot see more than the RTC did.
        && r.a_embassy_max_ms <= r.a_rtc_max_ms + 2
}

async fn case_b(rtc: &mut Rtc<'_>, r: &mut Results) {
    let armed = Instant::now();
    let armed_rtc = hal_ext::rtc::rtc_now_us(rtc);
    let timer = Timer::after_millis(CASE_B_ARM_MS);
    r.b_taken = slice(rtc).await.is_some();
    r.b_fired = timer
        .with_timeout(Duration::from_millis(TIMER_HANG_MS))
        .await
        .is_ok();
    r.b_embassy_ms = armed.elapsed().as_millis();
    r.b_rtc_ms = hal_ext::rtc::rtc_now_us(rtc).saturating_sub(armed_rtc) / 1_000;
}

fn case_b_pass(r: &Results) -> bool {
    // Fired, after at least its own length on both clocks, and not so late
    // that the alarm was lost and only the hang guard brought it home: the
    // slice plus the arm plus a generous wake gap bounds a paused clock.
    r.b_taken
        && r.b_fired
        && r.b_embassy_ms >= CASE_B_ARM_MS
        && r.b_rtc_ms >= CASE_B_ARM_MS
        && r.b_rtc_ms <= CASE_B_ARM_MS + SLICE_MS + 100
}

async fn case_c(rtc: &mut Rtc<'_>, r: &mut Results) {
    let armed = Instant::now();
    let armed_rtc = hal_ext::rtc::rtc_now_us(rtc);
    let timer = Timer::after_millis(CASE_C_ARM_MS);
    r.c_taken = slice(rtc).await.is_some();
    let woke_rtc = hal_ext::rtc::rtc_now_us(rtc);
    r.c_fired = timer
        .with_timeout(Duration::from_millis(TIMER_HANG_MS))
        .await
        .is_ok();
    let fired_rtc = hal_ext::rtc::rtc_now_us(rtc);
    r.c_embassy_ms = armed.elapsed().as_millis();
    r.c_rtc_ms = fired_rtc.saturating_sub(armed_rtc) / 1_000;
    r.c_after_wake_rtc_ms = fired_rtc.saturating_sub(woke_rtc) / 1_000;
}

fn case_c_pass(r: &Results) -> bool {
    // A deadline that expired under the slice fires on the wake: within one
    // arm length if the clock paused, at once if it ran. Either is prompt.
    r.c_taken && r.c_fired && r.c_after_wake_rtc_ms <= CASE_C_ARM_MS + 50
}

async fn case_d(rtc: &mut Rtc<'_>, r: &mut Results) {
    let mut last_wake_rtc: Option<u64> = None;
    let start = Instant::now();
    let start_rtc = hal_ext::rtc::rtc_now_us(rtc);
    for _ in 0..CASE_D_SLICES {
        let before = Instant::now();
        let before_rtc = hal_ext::rtc::rtc_now_us(rtc);
        if let Some(last) = last_wake_rtc {
            r.d_max_awake_gap_ms = r
                .d_max_awake_gap_ms
                .max(before_rtc.saturating_sub(last) / 1_000);
        }
        let Some((taken, _)) = slice(rtc).await else {
            r.d_skipped += 1;
            // The gap that just elapsed was a busy device, not a wake gap.
            last_wake_rtc = None;
            continue;
        };
        let after = Instant::now();
        if after < before {
            r.d_reversals += 1;
        }
        let rtc_ms = taken.rtc_elapsed_ms();
        if rtc_ms < SLICE_OK_MIN_MS {
            r.d_short += 1;
        }
        if rtc_ms > SLICE_OK_MAX_MS {
            r.d_long += 1;
        }
        r.d_taken += 1;
        last_wake_rtc = Some(hal_ext::rtc::rtc_now_us(rtc));
    }
    r.d_embassy_total_ms = start.elapsed().as_millis();
    r.d_rtc_total_ms = hal_ext::rtc::rtc_now_us(rtc).saturating_sub(start_rtc) / 1_000;
}

fn case_d_pass(r: &Results) -> bool {
    r.d_taken == CASE_D_SLICES && r.d_reversals == 0 && r.d_short == 0 && r.d_long == 0
}

/// The spin after the slices within a quarter of the one before: the PLL
/// came back and the CPU is at the same clock. A crystal-clocked wake is
/// four times slower and fails this at once.
fn clock_pass(r: &Results) -> bool {
    let before = r.spin_before_us.max(1);
    let after = r.spin_after_us;
    after * 4 <= before * 5 && before * 4 <= after * 5
}

fn report(r: &Results) {
    let a = case_a_pass(r);
    let b = case_b_pass(r);
    let c = case_c_pass(r);
    let d = case_d_pass(r);
    let clock = clock_pass(r);
    esp_println::println!(
        "bench: light_sleep_selftest case=A slices={} taken={} skipped={} req_ms={} embassy_min_ms={} embassy_max_ms={} rtc_min_ms={} rtc_max_ms={} embassy_paused={} pass={}",
        CASE_A_SLICES,
        r.a_taken,
        r.a_skipped,
        SLICE_MS,
        r.a_embassy_min_ms,
        r.a_embassy_max_ms,
        r.a_rtc_min_ms,
        r.a_rtc_max_ms,
        r.a_embassy_max_ms < SLICE_OK_MIN_MS / 2,
        a
    );
    esp_println::println!(
        "bench: light_sleep_selftest case=B armed_ms={} taken={} fired={} embassy_ms={} rtc_ms={} pass={}",
        CASE_B_ARM_MS,
        r.b_taken,
        r.b_fired,
        r.b_embassy_ms,
        r.b_rtc_ms,
        b
    );
    esp_println::println!(
        "bench: light_sleep_selftest case=C armed_ms={} taken={} fired={} embassy_ms={} rtc_ms={} after_wake_rtc_ms={} pass={}",
        CASE_C_ARM_MS,
        r.c_taken,
        r.c_fired,
        r.c_embassy_ms,
        r.c_rtc_ms,
        r.c_after_wake_rtc_ms,
        c
    );
    esp_println::println!(
        "bench: light_sleep_selftest case=D cycles={} taken={} skipped={} reversals={} short={} long={} rtc_total_ms={} embassy_total_ms={} max_awake_gap_ms={} pass={}",
        CASE_D_SLICES,
        r.d_taken,
        r.d_skipped,
        r.d_reversals,
        r.d_short,
        r.d_long,
        r.d_rtc_total_ms,
        r.d_embassy_total_ms,
        r.d_max_awake_gap_ms,
        d
    );
    esp_println::println!(
        "bench: light_sleep_selftest case=clock spin_before_us={} spin_after_us={} pass={}",
        r.spin_before_us,
        r.spin_after_us,
        clock
    );
    // Printing at all is the latch verdict: the device ran every slice on
    // battery and is still up. A cut would have rebooted it, and the boot
    // banner would be on the port instead of this line.
    esp_println::println!(
        "bench: light_sleep_selftest case=latch cycles={} pass=true",
        r.a_taken + r.d_taken + u32::from(r.b_taken) + u32::from(r.c_taken)
    );
    esp_println::println!(
        "bench: light_sleep_selftest case=usb at_start={} unplugged_rtc_ms={} replugged_rtc_ms={} pass=true",
        UsbHostState::from_code(r.usb_at_start).as_str(),
        r.unplugged_rtc_ms,
        r.replugged_rtc_ms
    );
    let all = a && b && c && d && clock;
    esp_println::println!(
        "light-sleep-selftest: result={}",
        if all { "pass" } else { "fail" }
    );
}

/// The whole proof. Returns once the report has printed, handing the RTC
/// back to the power task's normal loop.
pub async fn run(rtc: &mut Rtc<'_>) {
    let mut r = Results::default();
    // The input task needs two spaced samples before the verdict settles.
    Timer::after_millis(100).await;
    r.usb_at_start = power_state::usb_state().code();
    esp_println::println!(
        "light-sleep-selftest: usb={}; unplug the USB cable to begin, plug it back in for the report",
        UsbHostState::from_code(r.usb_at_start).as_str()
    );
    wait_for_usb(UsbHostState::Absent).await;
    r.unplugged_rtc_ms = hal_ext::rtc::rtc_now_us(rtc) / 1_000;
    // Let the hand and the cable settle, and the boot paint finish.
    Timer::after_secs(3).await;

    r.spin_before_us = spin_probe_us();
    case_a(rtc, &mut r).await;
    case_b(rtc, &mut r).await;
    case_c(rtc, &mut r).await;
    case_d(rtc, &mut r).await;
    r.spin_after_us = spin_probe_us();

    wait_for_usb(UsbHostState::Present).await;
    r.replugged_rtc_ms = hal_ext::rtc::rtc_now_us(rtc) / 1_000;
    // Enumeration and the monitor's reconnect want a moment.
    Timer::after_secs(2).await;
    report(&r);
    esp_println::println!("light-sleep-selftest: done; the power task runs normally from here");
}
