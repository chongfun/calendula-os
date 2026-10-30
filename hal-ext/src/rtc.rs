//! Thin wrappers around `esp_hal::rtc_cntl::Rtc` for the sleep modes the
//! firmware uses. Deep sleep is *terminal* — the chip reboots on wake and
//! re-enters `main()`, so that path returns `!`. Light sleep returns: the
//! power task takes it in short timer-bounded slices while a static page is
//! idle (see `fw::tasks::power` and `app_core::light_sleep`).

use core::time::Duration;
use esp_hal::gpio::RtcPinWithResistors;
use esp_hal::rtc_cntl::sleep::{RtcioWakeupSource, TimerWakeupSource, WakeupLevel};
use esp_hal::rtc_cntl::Rtc;
use esp_hal::system::SleepSource;

/// Whether this boot is a deep-sleep wake triggered by the armed RTC GPIO —
/// on this firmware, the Power button `enter_deep_sleep_button` arms.
///
/// Deep sleep is terminal (the chip resets into `main`), so the wake cause
/// is only readable here, at boot. The check is strict on purpose:
/// `wakeup_cause()` reports `Gpio` only when the reset reason is the
/// deep-sleep reset *and* the GPIO trigger fired, so battery pulls, crashes,
/// watchdog resets, and software resets (OTA staging) all report other
/// causes and read as `false`. Callers use this to trust panel contents the
/// sleep path drew before powering down.
pub fn woke_from_deep_sleep_gpio() -> bool {
    // SleepSource carries no PartialEq upstream; match instead of `==`.
    matches!(esp_hal::rtc_cntl::wakeup_cause(), SleepSource::Gpio)
}

/// Whether this boot is the RTC timer wake a bench scenario armed.
///
/// The shipped rule stays GPIO-only on purpose, so nothing but a real button
/// vouches for the panel contents. A bench build arms the timer as well, and
/// its wake is just as good a witness that the sleep frame is still on the
/// panel: the chip powered down through the same handshake either way. Left
/// out, a timer wake reads as a cold boot, the display pays the full
/// waveform instead of the fast one, and the suite measures a path the
/// shipped firmware does not take.
#[cfg(feature = "bench-selftest")]
pub fn woke_from_deep_sleep_timer() -> bool {
    matches!(esp_hal::rtc_cntl::wakeup_cause(), SleepSource::Timer)
}

/// Enters deep sleep with `wake_pin` (the active-low Power button) as the wake
/// source. The chip draws ~10–15 µA until the button is pressed, then resets
/// and reboots from `main`. Returns `!` because waking is a fresh boot, not a
/// resume — the executor and all in-RAM state are gone.
///
/// `WakeupLevel::Low` matches the button's wiring: the pin idles high through
/// its pull-up and is driven low while pressed. The C3 wake path re-enables
/// that pull-up internally, so a released button can't spuriously hold the line.
///
/// The caller owns the pin so this crate stays `forbid(unsafe_code)`; on the C3
/// the wake source must be one of the RTC GPIOs (0–5), which the Power button's
/// GPIO3 satisfies.
pub fn enter_deep_sleep_button(rtc: &mut Rtc<'_>, wake_pin: &mut dyn RtcPinWithResistors) -> ! {
    let mut wake_pins: [(&mut dyn RtcPinWithResistors, WakeupLevel); 1] =
        [(wake_pin, WakeupLevel::Low)];
    let wakeup = RtcioWakeupSource::new(&mut wake_pins);
    rtc.sleep_deep(&[&wakeup])
}

/// Deep sleep that wakes on the button **or** an RTC timer, for the bench
/// scenarios that have to sleep and come back with nobody present.
///
/// A sleeping device cannot be told to wake: the radio is down, the USB
/// device is gone, and the only host-side control left is a reset, which is
/// a reboot rather than a wake and so proves nothing about the wake path.
/// Arming the timer beside the button lets `sleep-sync` and `reader-soak`
/// run themselves.
///
/// Deliberately not on the shipped path, and behind `bench-selftest`
/// rather than an option: a reader that wakes itself on a timer would spend
/// the battery this firmware is careful with.
#[cfg(feature = "bench-selftest")]
pub fn enter_deep_sleep_button_or_timer(
    rtc: &mut Rtc<'_>,
    wake_pin: &mut dyn RtcPinWithResistors,
    after: Duration,
) -> ! {
    let mut wake_pins: [(&mut dyn RtcPinWithResistors, WakeupLevel); 1] =
        [(wake_pin, WakeupLevel::Low)];
    let rtcio = RtcioWakeupSource::new(&mut wake_pins);
    let timer = TimerWakeupSource::new(after);
    rtc.sleep_deep(&[&rtcio, &timer])
}

/// What one timer-only light-sleep slice measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LightSleepSlice {
    /// The slice the caller asked for.
    pub requested: Duration,
    /// RTC wall-clock time between the call and the return, in microseconds.
    /// The RTC slow clock keeps counting through light sleep, so this is the
    /// one clock that sees the whole slice; SYSTIMER (the Embassy clock)
    /// stops with the crystal.
    pub rtc_elapsed_us: u64,
}

impl LightSleepSlice {
    pub const fn rtc_elapsed_ms(&self) -> u64 {
        self.rtc_elapsed_us / 1_000
    }
}

/// RTC wall-clock since power-up, in microseconds.
///
/// `Rtc::time_since_power_up` reads the RTC main timer, which runs on the
/// slow clock and counts through every sleep mode; it is the reference the
/// light-sleep telemetry and the idle-leash correction are measured against.
pub fn rtc_now_us(rtc: &Rtc<'_>) -> u64 {
    rtc.time_since_power_up().as_micros()
}

/// One timer-only light-sleep slice: arm the RTC main timer for `duration`,
/// enter light sleep, return when it fires.
///
/// Light sleep keeps the digital domain powered (register state, SRAM and
/// pad outputs survive) and stops its clocks: SYSTIMER, TIMG0 and the USB
/// Serial/JTAG clock all pause until the wake, which is why the power task
/// admits a slice only when no bus transfer, SD session, radio session or
/// USB host would notice. The RTC timer is the only wake source armed here;
/// the ADC button ladders cannot wake the chip, so the caller keeps the
/// slice short and polls on return. Nothing here touches SYSTIMER or any
/// pad hold: GPIO13 (the Xteink battery latch / SD rail) is left exactly as
/// the running firmware had it, and esp-hal carries no flash-leakage
/// workaround that would pull it low. Pure safe code; the crate stays
/// `forbid(unsafe_code)`.
///
/// Takes the RTC by reference so the same handle serves the terminal deep
/// sleep afterwards; the earlier by-value helper consumed it.
///
/// A slice that returns well short of `duration` was rejected by the sleep
/// controller (`light_slp_reject`): a wake source was already asserted. The
/// caller reads `rtc_elapsed_us` rather than assuming it slept.
pub fn light_sleep_timer(rtc: &mut Rtc<'_>, duration: Duration) -> LightSleepSlice {
    let before = rtc_now_us(rtc);
    let wakeup = TimerWakeupSource::new(duration);
    rtc.sleep_light(&[&wakeup]);
    let after = rtc_now_us(rtc);
    LightSleepSlice {
        requested: duration,
        rtc_elapsed_us: after.saturating_sub(before),
    }
}
