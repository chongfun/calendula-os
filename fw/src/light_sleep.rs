//! ESP32-C3 RTOS-idle light sleep between page turns (adapted from Marigold PR #34).
//!
//! A reader spends nearly all of its awake time waiting for the next press.
//! Waiting used to be a `wfi` at 160 MHz with every clock running; this
//! module replaces esp-rtos's idle hook so that, when no task needs the
//! chip, it light-sleeps instead: RAM, registers and pad states stay put,
//! the clocks and the PLL stop, and the chip resumes in place when the next
//! timer is due or a wake button goes low.
//!
//! Tasks that are mid-way through hardware work hold the chip awake (see
//! [`Hold`]). The idle hook only runs when every thread is blocked, so
//! "nothing ready and no holds" means the chip has nothing to do until the
//! next timer or the next press.
//!
//! Light sleep stops the systimer behind `Instant::now()` and the TIMG0
//! alarm esp-rtos wakes the executor with. Each sleep therefore runs to the
//! deadline TIMG0 is armed for, then moves the systimer forward by the time
//! the RTC clock saw pass and fires the alarm at once, so every embassy
//! timer (the input poll, the idle timeout, the progress-write window)
//! keeps wall-clock time.

use core::time::Duration;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use esp_hal::peripherals::{GPIO, INTERRUPT_CORE0, IO_MUX, LPWR, SYSTIMER, TIMG0, USB_DEVICE};
use esp_hal::rtc_cntl::sleep::{GpioWakeupSource, TimerWakeupSource};
use esp_hal::rtc_cntl::Rtc;
use esp_hal::timer::systimer::{SystemTimer, Unit};
use portable_atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};

// These guards are acquired by SD, display, input, gauge and Wi-Fi paths.
// The idle hook does not run until *all* tasks block, but some tasks are
// blocked on hardware completion and cannot tolerate gated clocks.
static INHIBITORS: AtomicU32 = AtomicU32::new(0);

#[must_use = "dropping the guard releases the light-sleep inhibitor"]
pub struct SleepInhibitGuard {
    _private: (),
}

pub fn inhibit_light_sleep() -> SleepInhibitGuard {
    // Never wrap to zero while any guard is held.
    let _ = INHIBITORS.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
        Some(n.saturating_add(1))
    });
    SleepInhibitGuard { _private: () }
}

impl Drop for SleepInhibitGuard {
    fn drop(&mut self) {
        let _ = INHIBITORS.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            Some(n.saturating_sub(1))
        });
    }
}

/// A reason the chip must stay awake. Each holder sets its bit while it
/// has hardware work in flight that light sleep would stall (SPI DMA, a
/// panel BUSY wait, an ADC conversion, the radio).
#[derive(Clone, Copy)]
#[repr(u8)]
pub enum Hold {
    /// The display task, everywhere except its waits for the next command.
    Display = 1 << 0,
    /// The input task while it samples the ADC.
    Input = 1 << 1,
}

/// Boot starts held: the display initialises the panel and the input task
/// takes its first samples before either lets go.
static HOLDS: AtomicU8 = AtomicU8::new(Hold::Display as u8 | Hold::Input as u8);

pub fn hold(holder: Hold) {
    HOLDS.fetch_or(holder as u8, Ordering::SeqCst);
}

pub fn release(holder: Hold) {
    HOLDS.fetch_and(!(holder as u8), Ordering::SeqCst);
}

/// Raised after a sleep that a button ended, so the input task samples at
/// once instead of at its next poll.
pub static BUTTON_WAKE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Wall time spent in light sleep and the number of sleeps, for telemetry.
static SLEPT_US: AtomicU64 = AtomicU64::new(0);
static SLEEPS: AtomicU32 = AtomicU32::new(0);

/// `(microseconds slept, sleeps)` since boot.
#[cfg(feature = "serial-log")]
pub fn stats() -> (u64, u32) {
    (
        SLEPT_US.load(Ordering::Relaxed),
        SLEEPS.load(Ordering::Relaxed),
    )
}

/// A sleep shorter than this costs more to enter and leave than it saves;
/// the hook waits with `wfi` instead.
const MIN_SLEEP_US: u64 = 2_000;
/// Longest single sleep. Nothing needs it shorter (TIMG0 bounds every
/// sleep by the next deadline); this only caps the damage if the alarm
/// read were ever wrong.
const MAX_SLEEP_US: u64 = 60_000_000;

/// Pins that end a light sleep when they read low.
///
/// The Power button (GPIO3) and the two button ladders (GPIO1 front, GPIO2
/// side): Next on either ladder pulls its line to 0 V, so a page turn wakes
/// the chip the moment it is pressed; ladder buttons that sit higher are
/// caught by the input task's next poll.
///
/// USB D+ (GPIO19) idles high, held up by the chip's own pull-up whether
/// the cable is out or in a charger. A computer starts enumeration with a
/// bus reset that holds D+ low for at least 10 ms, so plugging into one
/// wakes the chip, which then stays awake long enough to enumerate.
const BUTTON_PINS: [usize; 3] = [1, 2, 3];
const USB_DP_PIN: usize = 19;
const WAKE_PINS: [usize; 4] = [1, 2, 3, USB_DP_PIN];
/// How long a D+ wake keeps the chip up for the host to enumerate it; once
/// it does, its start-of-frame traffic keeps the chip awake.
const USB_ENUMERATION_US: u64 = 3_000_000;
const GPIO_INTR_LOW_LEVEL: u8 = 4;

/// A USB host means the reader is on USB power, where sleep saves nothing
/// and would drop the serial console and the flashing link, so the chip
/// stays awake while one is attached.
fn usb_keeps_awake(usb_host: bool) -> bool {
    usb_host
        || embassy_time::Instant::now().as_micros()
            < USB_ENUMERATING_UNTIL_US.load(Ordering::Relaxed)
}

/// Systimer microseconds until which a D+ wake keeps the chip awake.
static USB_ENUMERATING_UNTIL_US: AtomicU64 = AtomicU64::new(0);

/// esp-rtos idle hook: runs whenever no thread is ready. esp-rtos does not
/// save the idle context, so every idle period re-enters this function from
/// the top; anything that must outlive one idle period lives in a static.
pub extern "C" fn idle_hook() -> ! {
    // SAFETY: the RTC block is shared with the power task's `Rtc`, which
    // only uses it for the terminal deep sleep. Light sleep runs in a
    // critical section from this hook, so the two never overlap.
    #[allow(unsafe_code)]
    let mut rtc = Rtc::new(unsafe { LPWR::steal() });
    loop {
        let slept = HOLDS.load(Ordering::SeqCst) == 0
            && INHIBITORS.load(Ordering::SeqCst) == 0
            && !usb_keeps_awake(usb_host_attached())
            && critical_section::with(|_| sleep_once(&mut rtc));
        if !slept {
            esp_hal::interrupt::wait_for_interrupt();
        }
    }
}

/// The USB frame number last seen, and when it last changed (systimer µs).
static USB_FRAME: AtomicU16 = AtomicU16::new(0);
static USB_FRAME_CHANGED_US: AtomicU64 = AtomicU64::new(0);

/// Whether a USB host is talking to the reader. A host sends a
/// start-of-frame every millisecond, so the frame number keeps moving;
/// one that has not moved for a few milliseconds means no host (unplugged,
/// or a charger that does not enumerate).
fn usb_host_attached() -> bool {
    let now = embassy_time::Instant::now().as_micros();
    let frame = usb_frame();
    if USB_FRAME.swap(frame, Ordering::Relaxed) != frame {
        USB_FRAME_CHANGED_US.store(now, Ordering::Relaxed);
    }
    now.saturating_sub(USB_FRAME_CHANGED_US.load(Ordering::Relaxed)) < 3_000
}

fn active_wake_pins() -> &'static [usize] {
    &WAKE_PINS
}

/// One light sleep, run with interrupts masked. Returns false, without
/// sleeping, when something is about to need the CPU anyway.
fn sleep_once(rtc: &mut Rtc<'_>) -> bool {
    // A hold taken, or an interrupt raised, between the check in the
    // hook and the critical section: let it run.
    if HOLDS.load(Ordering::SeqCst) != 0
        || INHIBITORS.load(Ordering::SeqCst) != 0
        || INTERRUPT_CORE0::regs().cpu_int_eip_status().read().bits() != 0
    {
        return false;
    }
    let budget = alarm_remaining_us().unwrap_or(MAX_SLEEP_US);
    if budget < MIN_SLEEP_US {
        return false;
    }
    let saved = arm_wake_pins();
    // A wake pin already low (a button held, or a ladder release still
    // chattering) would end the sleep straight away; leave it to input.
    if wake_pin_low() {
        disarm_wake_pins(saved);
        return false;
    }

    let rtc_before = rtc_now_us(rtc);
    let ticks_before = SystemTimer::unit_value(Unit::Unit0);
    let timer = TimerWakeupSource::new(Duration::from_micros(budget.min(MAX_SLEEP_US)));
    let gpio = GpioWakeupSource::new();
    // esp-hal's light-sleep defaults drop the chip's memories to their
    // retention voltage. RAM keeps its contents; the flash cache does not,
    // so a press that ends a sleep refetches the app's code and starts the
    // panel's waveform after ~10.2 ms instead of ~6.6 ms awake (measured
    // November 2026; 5.8 ms with the memories held at full voltage). Full
    // voltage costs sleep current on every sleep, and 3.6 ms is invisible
    // next to the ~300 ms waveform, so the defaults stay.
    rtc.sleep_light(&[&timer, &gpio]);
    let rtc_after = rtc_now_us(rtc);
    let ticks_after = SystemTimer::unit_value(Unit::Unit0);

    let button = pins_low(&BUTTON_PINS);
    let usb_bus = active_wake_pins().contains(&USB_DP_PIN)
        && (GPIO::regs().status().read().bits() & (1 << USB_DP_PIN) != 0
            || pins_low(&[USB_DP_PIN]));
    disarm_wake_pins(saved);

    // Move the systimer to wall time: the RTC saw `slept` pass, the
    // systimer counted only the part of it the chip was awake for.
    let slept = rtc_after.saturating_sub(rtc_before);
    let ticks_per_us = SystemTimer::ticks_per_second() / 1_000_000;
    let counted = ticks_after.wrapping_sub(ticks_before) / ticks_per_us;
    if slept > counted {
        advance_systimer((slept - counted) * ticks_per_us);
    }
    // TIMG0 stopped too, so its alarm still lies a whole sleep away. Fire
    // it now; esp-rtos's handler runs every timer that came due and
    // re-arms for the next one against the corrected clock.
    fire_alarm_now();

    if usb_bus {
        USB_ENUMERATING_UNTIL_US.store(
            embassy_time::Instant::now().as_micros() + USB_ENUMERATION_US,
            Ordering::Relaxed,
        );
    }
    SLEPT_US.fetch_add(slept, Ordering::Relaxed);
    SLEEPS.fetch_add(1, Ordering::Relaxed);
    if button {
        BUTTON_WAKE.signal(());
    }
    true
}

/// The RTC timer in microseconds, fresh. On the C3 a time update latches
/// on the next slow-clock edge, but esp-hal reads straight after asking,
/// so on its own it returns the value latched by the previous request.
/// Asking first and waiting two slow-clock periods (~15 µs at 136 kHz)
/// makes both this read and the one inside `TimerWakeupSource` current.
fn rtc_now_us(rtc: &Rtc<'_>) -> u64 {
    LPWR::regs()
        .time_update()
        .write(|w| w.time_update().set_bit());
    esp_hal::delay::Delay::new().delay_micros(20);
    rtc.time_since_power_up().as_micros()
}

fn usb_frame() -> u16 {
    USB_DEVICE::regs()
        .fram_num()
        .read()
        .sof_frame_index()
        .bits()
}

/// Microseconds until TIMG0 timer 0, esp-rtos's time base, raises its
/// alarm; `None` when no alarm is armed.
fn alarm_remaining_us() -> Option<u64> {
    let t = TIMG0::regs().t(0);
    let config = t.config().read();
    if config.alarm_en().bit_is_clear() {
        return None;
    }
    t.update().write(|w| w.update().set_bit());
    while t.update().read().update().bit_is_set() {}
    let now = (u64::from(t.hi().read().bits()) << 32) | u64::from(t.lo().read().bits());
    let alarm = (u64::from(t.alarmhi().read().bits()) << 32) | u64::from(t.alarmlo().read().bits());
    // The counter runs from APB (80 MHz with the CPU at 160 MHz) or XTAL
    // (40 MHz) through the divider; see esp-hal's `Timer::divider`.
    let source_mhz = if config.use_xtal().bit_is_set() {
        40
    } else {
        80
    };
    let divider = match config.divider().bits() {
        0 => 65_536,
        1 | 2 => 2,
        n => u64::from(n),
    };
    Some(alarm.saturating_sub(now) * divider / source_mhz)
}

/// Restarts TIMG0 timer 0 from zero with its alarm one tick away, the same
/// sequence esp-hal's one-shot `schedule` uses, so the alarm interrupt is
/// pending as soon as interrupts unmask.
fn fire_alarm_now() {
    let t = TIMG0::regs().t(0);
    t.config()
        .modify(|_, w| w.en().clear_bit().alarm_en().clear_bit());
    #[allow(unsafe_code)]
    // SAFETY: plain counter/alarm value writes on the timer esp-rtos owns;
    // its handler re-arms the timer from scratch on every alarm.
    unsafe {
        t.loadlo().write(|w| w.load_lo().bits(0));
        t.loadhi().write(|w| w.load_hi().bits(0));
        t.load().write(|w| w.load().bits(1));
        t.alarmlo().write(|w| w.alarm_lo().bits(1));
        t.alarmhi().write(|w| w.alarm_hi().bits(0));
    }
    t.config()
        .modify(|_, w| w.increase().set_bit().en().set_bit().alarm_en().set_bit());
}

/// Moves systimer unit 0, the clock behind `Instant::now()`, forward by
/// `ticks`. (esp-hal's own `set_count` shifts the high word the wrong way.)
fn advance_systimer(ticks: u64) {
    let systimer = SYSTIMER::regs();
    let value = SystemTimer::unit_value(Unit::Unit0).wrapping_add(ticks);
    #[allow(unsafe_code)]
    // SAFETY: unit 0 only feeds `Instant::now()`; no systimer alarm is in
    // use, so no comparator can be skipped by the jump.
    unsafe {
        let load = systimer.unitload(0);
        load.hi()
            .write(|w| w.load_hi().bits((value >> 32) as u32 & 0xF_FFFF));
        load.lo().write(|w| w.load_lo().bits(value as u32));
    }
    systimer.unit_load(0).write(|w| w.load().set_bit());
}

/// What `arm_wake_pins` changed, for `disarm_wake_pins` to put back.
#[derive(Clone, Copy)]
struct SavedPins {
    input_enabled: [bool; WAKE_PINS.len()],
}

/// Makes each wake pin end a light sleep when it reads low. The ladder
/// pins are ADC inputs with their digital input buffer off; it is turned
/// on only for the sleep.
fn arm_wake_pins() -> SavedPins {
    let mut saved = SavedPins {
        input_enabled: [false; WAKE_PINS.len()],
    };
    for (slot, &pin) in active_wake_pins().iter().enumerate() {
        let pad = IO_MUX::regs().gpio(pin);
        saved.input_enabled[slot] = pad.read().fun_ie().bit_is_set();
        pad.modify(|_, w| w.fun_ie().set_bit());
        #[allow(unsafe_code)]
        // SAFETY: int_type is a 3-bit field; 4 is "low level". The pin's
        // CPU interrupt enable stays as it was (off), so this only arms
        // the light-sleep wake.
        GPIO::regs().pin(pin).modify(|_, w| unsafe {
            w.int_type()
                .bits(GPIO_INTR_LOW_LEVEL)
                .wakeup_enable()
                .set_bit()
        });
    }
    saved
}

fn disarm_wake_pins(saved: SavedPins) {
    for (slot, &pin) in active_wake_pins().iter().enumerate() {
        #[allow(unsafe_code)]
        // SAFETY: back to "no interrupt type", the state these pins run in.
        GPIO::regs()
            .pin(pin)
            .modify(|_, w| unsafe { w.int_type().bits(0).wakeup_enable().clear_bit() });
        IO_MUX::regs()
            .gpio(pin)
            .modify(|_, w| w.fun_ie().bit(saved.input_enabled[slot]));
    }
    // The level trigger latched status bits while armed; clear them so the
    // GPIO interrupt handler (the panel BUSY wait) never sees them.
    let mask = WAKE_PINS.iter().fold(0u32, |mask, &pin| mask | 1 << pin);
    #[allow(unsafe_code)]
    // SAFETY: write-1-to-clear of this module's own pins' status bits.
    GPIO::regs()
        .status_w1tc()
        .write(|w| unsafe { w.bits(mask) });
}

fn wake_pin_low() -> bool {
    pins_low(active_wake_pins())
}

fn pins_low(pins: &[usize]) -> bool {
    let levels = GPIO::regs().in_().read().bits();
    pins.iter().any(|&pin| levels & (1 << pin) == 0)
}
