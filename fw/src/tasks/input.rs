use crate::{Button, InputEvent, INPUT_EVENTS, WAKE_PIN_HANDOFF, WAKE_PIN_REQUESTS};
use embassy_time::{Instant, Timer};
use esp_hal::analog::adc::{Adc, AdcCalCurve, AdcCalScheme, AdcPin};
use esp_hal::gpio::Input;
#[cfg(not(feature = "device-x3"))]
use esp_hal::peripherals::GPIO0;
use esp_hal::peripherals::{ADC1, GPIO1, GPIO2};
#[cfg(feature = "device-x3")]
use portable_atomic::{AtomicU32, Ordering};

type BoardAdc = ADC1<'static>;
type BoardAdcDriver = Adc<'static, BoardAdc, esp_hal::Blocking>;

/// The X3's fuel gauge on the shared I2C bus (GPIO0/GPIO20). The X4 has no
/// gauge; its battery comes from the aux ADC below.
#[cfg(feature = "device-x3")]
pub type BatteryGauge =
    hal_ext::bq27220::Bq27220<esp_hal::i2c::master::I2c<'static, esp_hal::Async>>;

// 15 ms polling puts press-to-event latency at 30-45 ms (two debounce
// ticks) instead of the 80-120 ms a 40 ms poll cost; the tick-based
// constants below are scaled to keep their wall-clock behavior.
const POLL_MS: u64 = 15;
const CALIBRATION_ONLY: bool = false;
const RAW_LOG_ENABLED: bool = false;
const DEBOUNCE_TICKS: u8 = 2;
// ~480 ms between held-button repeats, matching the fast-refresh settle
// cadence so one repeat advances one displayed page.
const REPEAT_COOLDOWN_TICKS: u8 = 32;
const NAV_BACK_MIN_MV: u16 = 2400;
const NAV_BACK_MAX_MV: u16 = 2700;
const RAW_LOG_TICKS: u8 = 67;
// Battery moves over minutes, not ticks: sample the gauge/ADC once per
// ~3 s instead of at the top of every 15 ms tick. On the X3 each sample
// is two clock-stretched BQ27220 write_reads (up to ~5 ms each behind the
// raised bus timeout), awaited ahead of the button ADC reads — per-tick
// sampling cost input jitter and standing I2C traffic for a value the UI
// hysteresis-holds anyway. Buttons still sample every tick.
const BATTERY_SAMPLE_TICKS: u32 = 200;
/// X3 battery telemetry, packed `GAUGE_VALID | percent << 16 | mv`, written
/// by the thread-executor `battery_run` task and read lock-free by the
/// interrupt-priority input loop — the gauge's clock-stretched I2C reads
/// (up to ~5 ms behind the raised bus timeout) have no place at interrupt
/// priority. Starts as a flat 100% so the boot paint never shows a
/// spurious 0%, with `GAUGE_VALID` clear: the input loop starts before the
/// thread executor and must not seed the app from this placeholder.
#[cfg(feature = "device-x3")]
static CACHED_GAUGE: AtomicU32 = AtomicU32::new(100 << 16);
/// Set in [`CACHED_GAUGE`] once `battery_run` has stored a real reading.
/// `percent` occupies bits 16..24, so bit 24 is free.
#[cfg(feature = "device-x3")]
const GAUGE_VALID: u32 = 1 << 24;

#[derive(Clone, Copy)]
struct Band {
    min: u16,
    max: u16,
    button: HardwareButton,
}

pub struct InputPins {
    /// Power button. Held as an `Option` because the deep-sleep path takes it:
    /// the power task re-materialises GPIO3 as the RTC wake source, which is
    /// only sound once this task's handle is gone, so `release_power_button`
    /// hands it over and stops polling. Deep sleep is terminal, so the pin
    /// never comes back.
    pub power: Option<Input<'static>>,
    /// X4 only: battery voltage on the GPIO0 ADC divider. On the X3 GPIO0 is
    /// I2C SCL, so the aux channel does not exist and the `battery_run`
    /// task's cached gauge word replaces it.
    #[cfg(not(feature = "device-x3"))]
    pub aux_pin: AdcPin<GPIO0<'static>, BoardAdc, AdcCalCurve<BoardAdc>>,
    pub nav_pin: AdcPin<GPIO1<'static>, BoardAdc, AdcCalCurve<BoardAdc>>,
    pub page_pin: AdcPin<GPIO2<'static>, BoardAdc, AdcCalCurve<BoardAdc>>,
}

#[derive(Clone, Copy)]
struct RawSample {
    aux: u16,
    nav: u16,
    page: u16,
}

struct StableButton {
    candidate: Option<HardwareButton>,
    current: Option<HardwareButton>,
    ticks: u8,
    cooldown: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HardwareButton {
    Back,
    Confirm,
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
enum FrontLayout {
    BackConfirmLeftRight,
    BackConfirmRightLeft,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
enum SideLayout {
    PrevNext,
    NextPrev,
}

const FRONT_LAYOUT: FrontLayout = FrontLayout::BackConfirmLeftRight;
const SIDE_LAYOUT: SideLayout = SideLayout::PrevNext;

const NAV: &[Band] = &[
    // X4 front-button ladder on GPIO1. These bands scale Adafruit's current
    // 16-bit CircuitPython X4 thresholds to the 12-bit esp-hal ADC reads.
    Band {
        min: NAV_BACK_MIN_MV,
        max: NAV_BACK_MAX_MV,
        button: HardwareButton::Back,
    },
    Band {
        min: 1800,
        max: 2150,
        button: HardwareButton::Confirm,
    },
    Band {
        min: 1000,
        max: 1250,
        button: HardwareButton::Left,
    },
    Band {
        min: 0,
        max: 100,
        button: HardwareButton::Right,
    },
];

const PAGE: &[Band] = &[
    // X4 side-button ladder on GPIO2, scaled from Adafruit's thresholds.
    Band {
        min: 1500,
        max: 1800,
        button: HardwareButton::Up,
    },
    Band {
        min: 0,
        max: 100,
        button: HardwareButton::Down,
    },
];

#[embassy_executor::task]
pub async fn run(mut adc: BoardAdcDriver, mut pins: InputPins) {
    esp_println::println!("input: started");

    let mut last_power = false;
    let mut power_ticks = 0u8;
    let mut nav_stable = StableButton::new();
    let mut page_stable = StableButton::new();
    let mut raw_log_ticks = 0u8;
    let mut reported_percent: Option<u8> = None;
    let mut battery_seeded = false;
    // (mv, percent, aux, valid) from the most recent battery sample; ticks
    // between samples reuse it. Until a valid sample seeds the app, every
    // tick re-reads so the X3 picks up battery_run's first gauge reading
    // within one tick instead of on the ~3 s cadence.
    let mut battery: (u16, u8, u16, bool) = (0, 100, 0, false);
    let mut battery_ticks: u32 = 0;

    loop {
        Timer::after_millis(POLL_MS).await;

        if release_power_button(&mut pins).await {
            return;
        }

        if battery_ticks == 0 || !battery_seeded {
            battery = read_power(&mut adc, &mut pins).await;
        }
        battery_ticks = (battery_ticks + 1) % BATTERY_SAMPLE_TICKS;
        let (battery_mv, raw_percent, aux, battery_valid) = battery;
        let sample = RawSample {
            aux,
            nav: read_adc(&mut adc, &mut pins.nav_pin).await,
            page: read_adc(&mut adc, &mut pins.page_pin).await,
        };
        if RAW_LOG_ENABLED {
            raw_log_ticks = raw_log_ticks.wrapping_add(1);
            if raw_log_ticks >= RAW_LOG_TICKS {
                raw_log_ticks = 0;
                esp_println::println!(
                    "input raw: gpio0={} gpio1={} gpio2={}",
                    sample.aux,
                    sample.nav,
                    sample.page,
                );
            }
        }

        if CALIBRATION_ONLY {
            continue;
        }

        // Invalid samples are the X3 boot placeholder (flat 100%); keep them
        // out of the hysteresis state so a first real reading of 99% is not
        // held at 100% as noise against the placeholder.
        let percent = if battery_valid {
            stable_percent(&mut reported_percent, raw_percent)
        } else {
            raw_percent
        };

        // The app boots with a placeholder 100% battery and only learns the
        // real charge from a Sample, which otherwise rides on a button press.
        // Push one button-less reading now so the first screen after a wake
        // (deep sleep is terminal -- wake is a cold boot) shows the true
        // charge instead of a flat 100%. On the X3 hold the seed until
        // battery_run's first successful gauge read: this loop starts before
        // the thread executor, and seeding from the placeholder cache would
        // pin the first paint at 100% until the next button press.
        if !battery_seeded && battery_valid {
            emit(None, sample, battery_mv, percent);
            battery_seeded = true;
            esp_println::println!("input: battery seeded ({} mV, {}%)", battery_mv, percent);
        }

        let power = pins
            .power
            .as_ref()
            .expect("power button owned by input task");
        let power_pressed = debounce_active_low(power.is_low(), &mut power_ticks);
        if power_pressed && !last_power {
            emit(Some(Button::Power), sample, battery_mv, percent);
            log_input(Some(Button::Power), sample);
        }
        last_power = power_pressed;

        let nav = nav_stable.update(classify(sample.nav, NAV));
        if let Some(nav) = nav {
            let StableEvent::Changed(hardware) = nav;
            let button = map_hardware(hardware);
            emit(Some(button), sample, battery_mv, percent);
            log_input(Some(button), sample);
        }

        let page = page_stable.update(classify(sample.page, PAGE));
        if let Some(page) = page {
            let StableEvent::Changed(hardware) = page;
            let button = map_hardware(hardware);
            emit(Some(button), sample, battery_mv, percent);
            log_input(Some(button), sample);
        }
    }
}

/// Surrenders the Power button to the power task if it has asked for it,
/// leaving this task with nothing on GPIO3. Returns true if ownership was
/// transferred, indicating the task should terminate.
///
/// The power task blocks on the handoff before it arms the wake source and
/// cuts power, so this costs the deep-sleep path about one poll tick, plus
/// any in-progress sample. The send never blocks in practice: the request
/// is only sent once per boot, so the single handoff slot is always free.
async fn release_power_button(pins: &mut InputPins) -> bool {
    if WAKE_PIN_REQUESTS.try_receive().is_err() {
        return false;
    }
    let power = pins
        .power
        .take()
        .expect("wake pin requested after ownership transfer");
    esp_println::println!("input: released power button for deep sleep");
    WAKE_PIN_HANDOFF.send(power).await;
    true
}

/// Read the battery, returning `(millivolts, percent, aux_raw, valid)`.
/// `aux_raw` is the debug value reported as the aux channel: the raw ADC
/// reading on the X4, the gauge voltage on the X3 (which has no aux ADC).
/// `valid` is false on the X3 until `battery_run` caches its first real
/// gauge reading; the X4's direct ADC read is always valid. On an I2C error
/// the X3 keeps reporting the last cached reading rather than a spurious 0%.
#[cfg(not(feature = "device-x3"))]
async fn read_power(adc: &mut BoardAdcDriver, pins: &mut InputPins) -> (u16, u8, u16, bool) {
    let aux = read_adc(adc, &mut pins.aux_pin).await;
    (battery_mv(aux), battery_percent(aux), aux, true)
}

#[cfg(feature = "device-x3")]
async fn read_power(_adc: &mut BoardAdcDriver, _pins: &mut InputPins) -> (u16, u8, u16, bool) {
    let packed = CACHED_GAUGE.load(Ordering::Relaxed);
    let mv = packed as u16;
    let percent = (packed >> 16) as u8;
    (mv, percent, mv, packed & GAUGE_VALID != 0)
}

/// X3 battery telemetry, deliberately independent of the 15 ms button
/// scanner now that input runs at interrupt priority: the slow-moving
/// gauge is sampled every 30 seconds on the thread executor, and the
/// input loop reads the cached word without touching I2C. On errors the
/// last good reading stays displayed; until the first success the word
/// keeps the boot placeholder (flat 100%) with `GAUGE_VALID` clear, so the
/// input task's boot seed waits for a real reading.
#[cfg(feature = "device-x3")]
#[embassy_executor::task]
pub async fn battery_run(mut gauge: BatteryGauge) {
    let mut failures = 0u32;
    loop {
        match gauge.read().await {
            Ok((mv, percent)) => {
                if failures > 0 {
                    esp_println::println!("battery: gauge recovered after {} failures", failures);
                    failures = 0;
                }
                CACHED_GAUGE.store(
                    GAUGE_VALID | u32::from(mv) | (u32::from(percent) << 16),
                    Ordering::Relaxed,
                );
            }
            Err(error) => {
                failures = failures.saturating_add(1);
                esp_println::println!("battery: gauge read failed ({:?})", error);
            }
        }
        Timer::after_secs(30).await;
    }
}

/// ADC noise straddling a percent boundary makes the displayed battery
/// flip between adjacent values on every refresh. Hold the reported
/// percent until the raw reading moves at least two points.
fn stable_percent(reported: &mut Option<u8>, raw: u8) -> u8 {
    match reported {
        Some(current) if raw.abs_diff(*current) < 2 => *current,
        _ => {
            *reported = Some(raw);
            raw
        }
    }
}

fn emit(button: Option<Button>, sample: RawSample, battery_mv: u16, battery_percent: u8) {
    let event = InputEvent::Sample {
        button,
        aux_raw: sample.aux,
        nav_raw: sample.nav,
        page_raw: sample.page,
        battery_mv,
        battery_percent,
    };
    if INPUT_EVENTS.try_send(event).is_ok() {
        return;
    }
    let _ = INPUT_EVENTS.try_receive();
    if INPUT_EVENTS.try_send(event).is_err() {
        esp_println::println!("input: event queue full");
    }
}

/// The X4 senses battery through a 2x divider on the aux ADC. The X3 reads
/// true voltage from its gauge, so these live only in the X4 build.
#[cfg(not(feature = "device-x3"))]
fn battery_mv(aux_mv: u16) -> u16 {
    aux_mv.saturating_mul(2)
}

#[cfg(not(feature = "device-x3"))]
fn battery_percent(aux_mv: u16) -> u8 {
    let mv = battery_mv(aux_mv).clamp(3300, 4200);
    (((mv - 3300) as u32 * 100) / 900) as u8
}

fn log_input(button: Option<Button>, sample: RawSample) {
    esp_println::println!(
        "bench: input button={:?} aux={} nav={} page_raw={} t_ms={}",
        button,
        sample.aux,
        sample.nav,
        sample.page,
        Instant::now().as_millis(),
    );
}

enum StableEvent {
    Changed(HardwareButton),
}

impl StableButton {
    const fn new() -> Self {
        Self {
            candidate: None,
            current: None,
            ticks: 0,
            cooldown: 0,
        }
    }

    fn update(&mut self, next: Option<HardwareButton>) -> Option<StableEvent> {
        self.cooldown = self.cooldown.saturating_sub(1);
        if next == self.candidate {
            self.ticks = self.ticks.saturating_add(1);
        } else {
            self.candidate = next;
            self.ticks = 1;
        }

        if self.ticks < DEBOUNCE_TICKS {
            return None;
        }

        if self.candidate != self.current {
            self.current = self.candidate;
            if let Some(current) = self.current {
                self.cooldown = REPEAT_COOLDOWN_TICKS;
                return Some(StableEvent::Changed(current));
            }
            self.cooldown = 0;
            return None;
        }

        let current = self.current?;
        if self.cooldown > 0 || !is_repeatable(current) {
            return None;
        }
        self.cooldown = REPEAT_COOLDOWN_TICKS;
        Some(StableEvent::Changed(current))
    }
}

fn is_repeatable(button: HardwareButton) -> bool {
    matches!(
        button,
        HardwareButton::Left | HardwareButton::Right | HardwareButton::Up | HardwareButton::Down
    )
}

fn debounce_active_low(raw_pressed: bool, ticks: &mut u8) -> bool {
    if raw_pressed {
        *ticks = ticks.saturating_add(1).min(DEBOUNCE_TICKS);
    } else {
        *ticks = ticks.saturating_sub(1);
    }
    *ticks == DEBOUNCE_TICKS
}

fn classify(value: u16, table: &[Band]) -> Option<HardwareButton> {
    for band in table {
        if value >= band.min && value <= band.max {
            return Some(band.button);
        }
    }
    None
}

fn map_hardware(button: HardwareButton) -> Button {
    match button {
        HardwareButton::Back => Button::Back,
        HardwareButton::Confirm => Button::Confirm,
        HardwareButton::Left => match FRONT_LAYOUT {
            FrontLayout::BackConfirmLeftRight => Button::Previous,
            FrontLayout::BackConfirmRightLeft => Button::Next,
        },
        HardwareButton::Right => match FRONT_LAYOUT {
            FrontLayout::BackConfirmLeftRight => Button::Next,
            FrontLayout::BackConfirmRightLeft => Button::Previous,
        },
        HardwareButton::Up => match SIDE_LAYOUT {
            SideLayout::PrevNext => Button::PagePrevious,
            SideLayout::NextPrev => Button::PageNext,
        },
        HardwareButton::Down => match SIDE_LAYOUT {
            SideLayout::PrevNext => Button::PageNext,
            SideLayout::NextPrev => Button::PagePrevious,
        },
    }
}

async fn read_adc<P, CS>(adc: &mut BoardAdcDriver, pin: &mut AdcPin<P, BoardAdc, CS>) -> u16
where
    P: esp_hal::analog::adc::AdcChannel,
    CS: AdcCalScheme<BoardAdc>,
{
    loop {
        match adc.read_oneshot(pin) {
            Ok(value) => return value,
            Err(nb::Error::WouldBlock) => Timer::after_micros(50).await,
            Err(_) => return 0,
        }
    }
}
