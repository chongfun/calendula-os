#![no_std]
#![no_main]
#![deny(unsafe_code)]
#![allow(clippy::manual_div_ceil)] // False positive inside esp_hal::dma_buffers!.
#![deny(clippy::large_stack_arrays)]
#![deny(clippy::large_types_passed_by_value)]

#[repr(C)]
pub struct EspAppDesc {
    pub magic_word: u32,
    pub secure_version: u32,
    pub reserv1: [u32; 2],
    pub version: [u8; 32],
    pub project_name: [u8; 32],
    pub time: [u8; 16],
    pub date: [u8; 16],
    pub idf_ver: [u8; 32],
    pub app_elf_sha256: [u8; 32],
    pub min_efuse_blk_rev_full: u16,
    pub max_efuse_blk_rev_full: u16,
    pub mmu_page_size: u8,
    pub spi_flash_mode: u8,
    pub reserv3: [u8; 2],
    pub reserv2: [u32; 18],
}

// Zero-pad a string into a fixed descriptor field. The fields are always
// [u8; 32] in the image, so filling them costs no bytes; overlong input
// fails the const evaluation instead of truncating silently.
const fn desc_field<const N: usize>(s: &str) -> [u8; N] {
    let mut out = [0u8; N];
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        out[i] = bytes[i];
        i += 1;
    }
    out
}

/// The app descriptor's `project_name`, and the identity `ota_update` reads
/// back out of flash to decide whether the slot-0 anchor can apply an update.
///
/// Format: `CalendulaOS <board> u<updater-generation>`.
///
/// The product name alone is not enough to answer that question. The X4 and X3
/// builds are the same product but take different OTA trigger filenames
/// (`FWUPDATE.BIN` vs `FWUPDX3.BIN`) and drive different panels and battery
/// gauges, so bouncing into an anchor built for the other board would boot
/// firmware for the wrong hardware *and* strand the update, since that image
/// never looks for this board's trigger. The board is therefore part of the
/// identity. So is a generation digit, bumped whenever the trigger filename or
/// the update hand-off changes, so an anchor too old to recognise this trigger
/// is refused rather than booted into. The digit counts per board, since the
/// board is already in the string.
///
/// The strings themselves live in [`proto::ota`], beside the comparison that
/// reads them back and the tests that pin them, so the firmware cannot stamp
/// one identity while the updater expects another.
#[cfg(not(feature = "device-x3"))]
pub const PROJECT_NAME: &str = proto::ota::IDENTITY_X4;
#[cfg(feature = "device-x3")]
pub const PROJECT_NAME: &str = proto::ota::IDENTITY_X3;

// The descriptor field is a fixed 32 bytes; `desc_field` would fail const
// evaluation on an overlong identity, but with an index panic rather than a
// reason.
const _: () = assert!(
    PROJECT_NAME.len() <= 32,
    "PROJECT_NAME must fit the app descriptor's project_name field"
);

#[allow(unsafe_code)]
#[link_section = ".rodata_desc"]
#[used]
#[no_mangle]
pub static _ESP_APP_DESC: EspAppDesc = EspAppDesc {
    magic_word: 0xABCD5432,
    secure_version: 0,
    reserv1: [0; 2],
    version: desc_field(env!("CARGO_PKG_VERSION")),
    project_name: desc_field(PROJECT_NAME),
    time: *b"00:00:00\0\0\0\0\0\0\0\0",
    date: *b"2026-05-20\0\0\0\0\0\0",
    idf_ver: [0; 32],
    app_elf_sha256: [0; 32],
    min_efuse_blk_rev_full: 0,
    max_efuse_blk_rev_full: 65535,
    mmu_page_size: 16,
    spi_flash_mode: 2,
    reserv3: [0; 2],
    reserv2: [0; 18],
};

// The only allocator user is the Wi-Fi sync session; reader paths stay
// allocation-free because no region exists until sync_mem donates the
// loaned buffers.
extern crate alloc;

use app_core::buttons::{ComboConfirmer, ComboVerdict};
pub use app_core::{
    AppView, Button, DisplayCommand, DisplayEvent, DisplayOrientation, InputEvent, LibraryEvent,
    PowerEvent, ReaderSource, RefreshPolicy, RenderKind, RenderRequest, StorageCommand,
    SyncCommand, SyncEvent,
};
use core::sync::atomic::AtomicU32;
use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::Instant;
use esp_backtrace as _;
use esp_hal::analog::adc::{
    Adc, AdcCalCurve, AdcCalScheme, AdcChannel, AdcConfig, AdcPin, Attenuation,
};
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::interrupt::Priority;
use esp_hal::peripherals::ADC1;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_rtos::embassy::Executor;
use esp_rtos::embassy::InterruptExecutor;
use static_cell::StaticCell;
use tasks::input::InputPins;

// `#[macro_use]` is textual: `log` must precede the modules that call
// bench_log!.
#[macro_use]
mod log;

mod board_guard;
mod book_build;
pub mod catalog;
mod custom_font;
mod display_flush;
mod library_sd;
mod mmu;
mod ota_update;
mod probe_cache;
mod probe_report;
mod sd_session;
mod sleep_marker;
mod sync_mem;
pub mod tasks;
pub mod upload;
mod views;

pub static INPUT_EVENTS: Channel<CriticalSectionRawMutex, InputEvent, 8> = Channel::new();
pub static LATEST_READER_REQUEST_ID: AtomicU32 = AtomicU32::new(0);
pub static DISPLAY_COMMANDS: Channel<CriticalSectionRawMutex, DisplayCommand, 4> = Channel::new();
// 8 slots (270 B each) is enough for the cache-build burst case, and the
// ~2.1 KB of .bss saved by not going wider widens the main stack region. A
// full queue is not a loss: the queue is left alone and the acknowledgement
// waits in DisplayEventHolder until the app drains a slot (see
// send_required_display_event), so a shorter queue costs only that wait.
pub static DISPLAY_EVENTS: Channel<
    CriticalSectionRawMutex,
    DisplayEvent,
    { app_core::DISPLAY_EVENT_SLOTS },
> = Channel::new();
// Sized from app_core so the eviction walk that makes room in this channel
// (see send_required_library_event) cannot disagree with it about the ring.
pub static LIBRARY_EVENTS: Channel<
    CriticalSectionRawMutex,
    LibraryEvent,
    { app_core::LIBRARY_EVENT_SLOTS },
> = Channel::new();
pub static STORAGE_COMMANDS: Channel<CriticalSectionRawMutex, StorageCommand, 4> = Channel::new();
pub static POWER_EVENTS: Channel<CriticalSectionRawMutex, PowerEvent, 4> = Channel::new();
/// The generation of a sleep handshake the power task gave up on.
///
/// Once the display task has put the panel down it parks until deep sleep
/// takes the chip or this names its own generation. Parking is what keeps a
/// render from repainting over the sleep image: a task that is not running
/// cannot touch the panel, and `enter_deep_sleep_button` never returns, so on
/// the ordinary path the park simply never ends.
///
/// A `Signal` rather than a channel because the latest value is the only one
/// that matters and it must latch: the power task can abandon a handshake
/// before the display task reaches its park, and that resume has to still be
/// waiting when it gets there.
pub static DISPLAY_RESUME: Signal<CriticalSectionRawMutex, u32> = Signal::new();
// Power button (GPIO3) handoff for the terminal deep-sleep path. The input
// task owns the pin and polls it for the whole run; the power task needs it
// back as the RTC wake source, and re-materialising it there is only sound
// while no other handle is live. A request on WAKE_PIN_REQUESTS asks the input
// task to stop polling and surrender its `Input`, which comes back over
// WAKE_PIN_HANDOFF. One request is ever sent -- the branch that sends it does
// not return -- so both stay at a single slot.
//
// SAFETY INVARIANT: WAKE_PIN_HANDOFF must be consumed immediately before
// GPIO3::steal(). No other code may construct GPIO3 directly.
pub static WAKE_PIN_REQUESTS: Channel<CriticalSectionRawMutex, (), 1> = Channel::new();
pub static WAKE_PIN_HANDOFF: Channel<CriticalSectionRawMutex, Input<'static>, 1> = Channel::new();
pub static SYNC_COMMANDS: Channel<CriticalSectionRawMutex, SyncCommand, 2> = Channel::new();
pub static SYNC_EVENTS: Channel<CriticalSectionRawMutex, SyncEvent, 4> = Channel::new();
// Carries Err when the display task refuses the loan (progress flush failed);
// the wifi task reports the failure and returns to waiting for Start, so the
// Wireless screen can offer a retry instead of hanging on a loan that will
// never arrive.
pub static SYNC_LOANS: Channel<
    CriticalSectionRawMutex,
    Result<sync_mem::SyncLoan, app_core::SyncError>,
    1,
> = Channel::new();
pub static UPLOAD_BEGINS: Channel<CriticalSectionRawMutex, upload::UploadBegin, 1> = Channel::new();
pub static UPLOAD_CHUNKS: Channel<CriticalSectionRawMutex, upload::UploadChunk, 2> = Channel::new();
pub static UPLOAD_RETURNS: Channel<CriticalSectionRawMutex, &'static mut [u8], 2> = Channel::new();
pub static UPLOAD_RESULTS: Channel<CriticalSectionRawMutex, bool, 1> = Channel::new();
/// Wireless Exit asks the board I/O task to abort and close the upload
/// session (any in-flight book is aborted and its clusters reclaimed).
pub static UPLOAD_STOP_REQUESTS: Channel<CriticalSectionRawMutex, (), 1> = Channel::new();
/// Acknowledges the stop: every FAT handle is closed and the volume is
/// unmounted, so the session-ending reset cannot race an open writer.
pub static UPLOAD_STOPPED: Channel<CriticalSectionRawMutex, (), 1> = Channel::new();
/// A Sleep exit closed the upload session while the book server may be
/// mid-request. The server consumes this to fail the interrupted request,
/// reclaim the loaned buffers, and restart the session on the next request.
/// Only observable when sleep turns out non-terminal (a late activity event
/// cancels the handshake, or display shutdown fails); a completed deep
/// sleep resets before anything reads it. Wireless Exit doesn't raise it —
/// that path ends in a reset behind the UPLOAD_STOPPED handshake.
pub static UPLOAD_INTERRUPTS: Signal<CriticalSectionRawMutex, ()> = Signal::new();
/// Whether a StoreWifiCredentials write landed AND read back identically
/// through the boot-time read path; the portal waits on this before it
/// may show the success page.
pub static WIFI_STORAGE_RESULTS: Channel<CriticalSectionRawMutex, bool, 1> = Channel::new();

static EXECUTOR: StaticCell<Executor> = StaticCell::new();
static INPUT_EXECUTOR: StaticCell<InterruptExecutor<1>> = StaticCell::new();

type BoardAdc = ADC1<'static>;
type BoardAdcDriver = Adc<'static, BoardAdc, esp_hal::Blocking>;

/// Blocking median-of-three ADC read for the boot-time recovery combo check,
/// before the async input task exists. Median rejects a single noisy sample.
fn median3_adc<P, CS>(adc: &mut BoardAdcDriver, pin: &mut AdcPin<P, BoardAdc, CS>) -> u16
where
    P: AdcChannel,
    CS: AdcCalScheme<BoardAdc>,
{
    let mut v = [0u16; 3];
    for slot in v.iter_mut() {
        *slot = loop {
            match adc.read_oneshot(pin) {
                Ok(mv) => break mv,
                Err(nb::Error::WouldBlock) => {}
                Err(_) => break 0,
            }
        };
    }
    app_core::buttons::median3(v[0], v[1], v[2])
}

/// Poll both ladders until [`ComboConfirmer`] resolves. This is the ADC and the
/// delay around it; the timing rules and what counts as a confirmed hold live
/// in `app_core::buttons`, where they are host-tested.
fn recovery_combo_confirmed(
    adc: &mut BoardAdcDriver,
    nav_pin: &mut AdcPin<esp_hal::peripherals::GPIO1<'static>, BoardAdc, AdcCalCurve<BoardAdc>>,
    page_pin: &mut AdcPin<esp_hal::peripherals::GPIO2<'static>, BoardAdc, AdcCalCurve<BoardAdc>>,
) -> bool {
    let delay = esp_hal::delay::Delay::new();
    let mut confirmer = ComboConfirmer::new();
    loop {
        let nav_mv = median3_adc(adc, nav_pin);
        let page_mv = median3_adc(adc, page_pin);
        match confirmer.push(nav_mv, page_mv) {
            ComboVerdict::Confirmed => {
                esp_println::println!(
                    "recovery: Back+Up confirmed over {} polls (nav={} page={})",
                    confirmer.consecutive(),
                    nav_mv,
                    page_mv
                );
                return true;
            }
            ComboVerdict::GaveUp => return false,
            ComboVerdict::KeepPolling => delay.delay_millis(ComboConfirmer::POLL_MS),
        }
    }
}

/// Work out which panel controller this unit carries, fingerprinting it if
/// this power cycle has not already done so, and hand every pin back.
///
/// This is the board layer's half of `hal_ext::epd_probe`: the probe itself
/// knows nothing about pin numbers, and this function is the only place that
/// says which GPIO is the panel's clock, data, chip select, D/C and reset. If
/// a board-profile layer lands later, it supplies these five and the probe is
/// untouched.
///
/// The probe has to run *here* — after `esp_hal::init` gives out the pin
/// singletons, before `Spi::new` configures SPI2 onto the clock and data lines
/// — because the read is bit-banged: the SPI peripheral cannot turn its own
/// MOSI pin around mid-transfer to let the controller answer. The pins are
/// only borrowed, so their real drivers are built from the same singletons a
/// few lines later, and on the cached path they are not touched at all.
///
/// A live probe costs about 70 ms on the X4 and about 200 ms on the X3
/// (203 ms measured). The X3's UC8279d is not bench-proven to answer the
/// short reset pulse, so it gets `ResetEscalation::OnMiss`, and a UC8253
/// presents the blank-VER shape, so its confirming pass runs at vendor
/// timing too — two 50 ms pulses rather than none (see
/// `hal_ext::epd_probe`). It answers a question
/// about soldered hardware, so `probe_cache` retains the answer for the rest
/// of the power cycle and a deep-sleep wake pays nothing. The extra reset
/// pulse a live probe puts on the panel is harmless — the driver's own init
/// sequence opens with a reset, and e-paper holds its image through both.
fn resolve_panel_controller(
    sclk: esp_hal::peripherals::GPIO8<'_>,
    sda: esp_hal::peripherals::GPIO10<'_>,
    cs: esp_hal::peripherals::GPIO21<'_>,
    dc: esp_hal::peripherals::GPIO4<'_>,
    rst: esp_hal::peripherals::GPIO5<'_>,
) -> hal_ext::epd_probe::ProbeDiag {
    use esp_hal::gpio::Flex;
    use hal_ext::epd_probe::{ProbePins, ResetEscalation};

    if let Some(cached) = probe_cache::load() {
        esp_println::println!("display: controller probe reused from this power-on");
        return cached;
    }

    // The X4's UC8179 answers the cheap screening pulse on every unit benched
    // so far; the X3's UC8279d has no such evidence behind it, so a missed
    // screening pass there is retried at the vendor identification timing
    // rather than written off.
    let escalation = if cfg!(feature = "device-x3") {
        ResetEscalation::OnMiss
    } else {
        ResetEscalation::Off
    };
    let start = Instant::now();
    let diag = hal_ext::epd_probe::probe(
        ProbePins {
            sclk: Flex::new(sclk),
            sda: Flex::new(sda),
            cs: Flex::new(cs),
            dc: Flex::new(dc),
            rst: Flex::new(rst),
        },
        escalation,
    );
    // The tiering in `epd_probe` exists to keep this number small on the units
    // that will never answer, so the number belongs in the telemetry that
    // proves it rather than in a comment claiming it.
    bench_log!(
        "bench: panel_probe verdict={} elapsed_ms={} t_ms={}",
        diag.verdict.as_str(),
        start.elapsed().as_millis(),
        Instant::now().as_millis(),
    );
    probe_cache::store(&diag);
    diag
}

#[esp_hal::main]
fn main() -> ! {
    // Config::default() leaves the ESP32-C3 at 80 MHz; layout, panel byte
    // transforms, and EPUB inflate are all CPU-bound, so run at full speed
    // and rely on race-to-idle for power.
    let config = esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::_160MHz);
    // `mut` for the two boot probes below: both borrow pin singletons and hand
    // them back before the real drivers are built.
    let mut peripherals = esp_hal::init(config);
    esp_println::println!("calendula-os: boot");

    // Deep sleep is terminal, so waking is this cold boot; the RTC wake
    // cause and RTC RAM are its only trace. Fast wake needs both: the
    // Power-button (GPIO) wake cause proves how the chip woke, and the
    // sleep_marker records that the pre-sleep handshake actually settled
    // the sleep frame on the panel — a failed flush still powers down, so
    // the wake cause alone can't vouch for the pixels. Only when both hold
    // does the display task seed its refresh planner for the fast wake
    // waveform. Battery pulls, crashes, and software resets read false.
    // The marker is consumed unconditionally so it never outlives one boot.
    let woke_by_button = hal_ext::rtc::woke_from_deep_sleep_gpio();
    let sleep_image_settled = sleep_marker::take_sleep_image_settled();
    let deep_sleep_wake = woke_by_button && sleep_image_settled;
    esp_println::println!(
        "main: deep_sleep_wake={} (gpio={}, sleep_image={})",
        deep_sleep_wake,
        woke_by_button,
        sleep_image_settled
    );

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_ints = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_ints.software_interrupt0);

    // Two probes run here, and the order between them is deliberate. This one
    // answers "which board is this"; `resolve_panel_controller` below answers
    // Which board is this? An image built for the other one boots happily and
    // then drives the wrong controller at the wrong geometry, and nothing
    // earlier can see it. `board_probe` reads the X3-only I2C parts over the
    // gauge pins and hands both back, so it must run before the ADC claims
    // GPIO0 (X4) or the gauge driver claims both (X3). The verdict is spent
    // further down, once the SPI bus the refusal needs exists.
    //
    // Before `resolve_panel_controller`, deliberately: a wrong-board verdict
    // makes "which controller" moot, and that probe costs ~70 ms on the X4 and
    // ~200 ms on the X3 (its own doc comment has the breakdown), plus reset
    // pulses, on a device about to halt. They share no pins, so this is about
    // wasted work, not correctness.
    //
    // Not on a fast wake. A deep-sleep wake is the same power-on continuing:
    // the board cannot have changed, and the boot that armed the sleep already
    // passed this guard. `deep_sleep_wake` needs *both* the RTC GPIO cause and
    // a settled sleep frame, so everything else — a flash, a battery pull, a
    // crash, a software or USB reset — takes the full probe. Flashing always
    // ends in a reset, never a deep-sleep wake, so a wrong-board image cannot
    // reach the reader by a path that skips this.
    let board_mismatch = if deep_sleep_wake {
        esp_println::println!("board: probe skipped, deep-sleep wake");
        None
    } else {
        let probe_start = Instant::now();
        let board_fingerprint = hal_ext::board_probe::fingerprint(
            peripherals.I2C0.reborrow(),
            /* sda */ peripherals.GPIO20.reborrow(),
            /* scl */ peripherals.GPIO0.reborrow(),
        );
        let board_verdict = board_fingerprint.verdict();
        // Boot-identity output, so not behind `serial-log`: on a device that
        // will not paint, this and the SD diagnostic are the whole story.
        //
        // COMPATIBILITY: web/index.html's "Check my reader over USB" matches
        // /board: probe verdict=(\w+)\s/ against this and maps the captured
        // token by name. It reads the probe rather than the installed
        // descriptor because the probe reports hardware — a mis-flashed reader
        // still tells the truth here. So the wording, and the `BoardVerdict`
        // variant names `{:?}` renders, are load-bearing outside this file, and
        // breaking either fails silently. A variant added here reads as unknown
        // there until taught; change both together. The site resets the reader
        // over DTR, which is a chip reset rather than a deep-sleep wake, so
        // this line is always printed for it.
        esp_println::println!(
            "board: probe verdict={:?} found={:#b}/{:#b} fault={}/{} elapsed_ms={}",
            board_verdict,
            board_fingerprint.first.found,
            board_fingerprint.second.found,
            board_fingerprint.first.faulted,
            board_fingerprint.second.faulted,
            probe_start.elapsed().as_millis()
        );
        for index in 0..hal_ext::board_probe::TARGET_COUNT {
            let bit = 1u8 << index;
            bench_log!(
                "board: {} pass1={} pass2={}",
                hal_ext::board_probe::target_name(index),
                board_fingerprint.first.found & bit != 0,
                board_fingerprint.second.found & bit != 0
            );
        }
        board_guard::evaluate(board_verdict, PROJECT_NAME)
    };

    // Decided above, executed much later: refusing needs the SPI bus and the SD
    // chip select, and those cannot exist yet — the panel probe below has to
    // read SCK and MOSI before `Spi::new` claims them. Deciding early is what
    // lets everything between here and the refusal be skipped.

    // The card's chip select comes up first, and before the panel probe below:
    // that probe bit-bangs the shared SPI bus by hand, and a card left with a
    // floating CS would see every clock it sends.
    let sd_cs = Output::new(peripherals.GPIO12, Level::High, OutputConfig::default());

    // Skipped outright on a confirmed mismatch. The probe is not passive — it
    // drives the panel pins, resets the controller once per pass, and may pay
    // the vendor-timing reset and an MTP read — and on this path its answer is
    // never read, because the refusal never initialises a panel. Running it
    // anyway would cost ~70 ms (X4) or ~200 ms (X3) and two reset pulses to
    // identify a controller nothing will drive.
    //
    // Otherwise: the pins are borrowed, not consumed. Their real drivers are
    // built from the same singletons a few lines down, and SCK/MOSI have to be
    // probed before `Spi::new` claims them, the panel's CS/DC/RST before their
    // `Output`s exist.
    let probe_diag = if board_mismatch.is_none() {
        let diag = resolve_panel_controller(
            peripherals.GPIO8.reborrow(),
            peripherals.GPIO10.reborrow(),
            peripherals.GPIO21.reborrow(),
            peripherals.GPIO4.reborrow(),
            peripherals.GPIO5.reborrow(),
        );
        display_flush::record_probe_verdict(diag.verdict);
        Some(diag)
    } else {
        None
    };

    let epd_cs = Output::new(peripherals.GPIO21, Level::High, OutputConfig::default());
    let epd_dc = Output::new(peripherals.GPIO4, Level::High, OutputConfig::default());
    let epd_rst = Output::new(peripherals.GPIO5, Level::High, OutputConfig::default());
    let epd_busy = Input::new(peripherals.GPIO6, InputConfig::default());
    let power_button = Input::new(
        peripherals.GPIO3,
        InputConfig::default().with_pull(Pull::Up),
    );

    let mut adc_config = AdcConfig::new();
    // GPIO0 is the battery ADC divider on the X4; on the X3 it is I2C SCL
    // (paired with GPIO20 SDA) for the fuel gauge, so it is not an ADC pin.
    #[cfg(not(feature = "device-x3"))]
    let aux_adc = adc_config
        .enable_pin_with_cal::<_, AdcCalCurve<BoardAdc>>(peripherals.GPIO0, Attenuation::_11dB);
    let mut nav_adc = adc_config
        .enable_pin_with_cal::<_, AdcCalCurve<BoardAdc>>(peripherals.GPIO1, Attenuation::_11dB);
    let mut page_adc = adc_config
        .enable_pin_with_cal::<_, AdcCalCurve<BoardAdc>>(peripherals.GPIO2, Attenuation::_11dB);
    let mut adc1 = Adc::new(peripherals.ADC1, adc_config);

    // X3 fuel gauge on I2C0: SCL=GPIO0, SDA=GPIO20, 400 kHz.
    #[cfg(feature = "device-x3")]
    let battery_gauge = {
        let i2c = esp_hal::i2c::master::I2c::new(
            peripherals.I2C0,
            esp_hal::i2c::master::Config::default()
                .with_frequency(Rate::from_khz(400))
                // The BQ27220 clock-stretches for milliseconds while it
                // processes a command; esp-hal's default SCL timeout of 10
                // bus cycles (25 us) aborts every read with Timeout. Allow
                // 2000 cycles (~5 ms at 400 kHz, rounded up to a power of
                // two by the hardware), matching the 4 ms Wire timeout the
                // stock firmware uses.
                .with_timeout(esp_hal::i2c::master::BusTimeout::BusCycles(2000)),
        )
        .expect("I2C0 config")
        .with_scl(peripherals.GPIO0)
        .with_sda(peripherals.GPIO20)
        .into_async();
        hal_ext::bq27220::Bq27220::new(i2c)
    };

    // RecoveryBoot escape hatch: holding Back + Up at reset repoints otadata at
    // the slot-0 anchor and reboots into it — a way back if the update slot's
    // firmware boots but misbehaves. Sampled here, the earliest point, before
    // any task owns the ADC; the stock bootloader can't read buttons, so only
    // the running firmware can honour the combo.
    if recovery_combo_confirmed(&mut adc1, &mut nav_adc, &mut page_adc)
        && ota_update::recover_to_slot0()
    {
        esp_hal::system::software_reset();
    }

    // Not on a refusal. Marking the running slot valid asserts "this image
    // works", and the guard is about to say it does not — blessing a trial
    // image immediately before refusing it would cement the very install the
    // refusal exists to reject, and take automatic rollback with it. Left
    // unblessed, a rollback-enabled bootloader can return the device to the
    // other slot on the next power-up. The recovery combo above stays ahead of
    // this either way.
    if board_mismatch.is_none() {
        ota_update::mark_running_slot_valid();
    } else {
        // Said out loud so a refusing boot's trace does not just go quiet here;
        // FLASHING.md sends people to these lines.
        esp_println::println!("ota: not marking this slot valid; the board guard is refusing");
    }

    // One display band must fit a single TX DMA buffer (X4 fills it
    // exactly; the X3's 99-byte rows leave 80 bytes slack). The RX side
    // only ever carries the SD session's bounce chunk - the EPD is
    // write-only - so it stays at chunk size; every byte saved in .bss
    // is main-stack headroom now (see build.rs).
    const _: () = assert!(display::BAND_BYTES <= 8000);
    let (rx_buffer, rx_descriptors, tx_buffer, tx_descriptors) =
        esp_hal::dma_buffers!(sd_session::SD_SPI_CHUNK_BYTES, 8000);
    let dma_rx = esp_hal::dma::DmaRxBuf::new(rx_descriptors, rx_buffer).unwrap();
    let dma_tx = esp_hal::dma::DmaTxBuf::new(tx_descriptors, tx_buffer).unwrap();
    let epd_spi = Spi::new(
        peripherals.SPI2,
        SpiConfig::default()
            .with_frequency(Rate::from_hz(display::epd::SPI_HZ))
            .with_mode(esp_hal::spi::Mode::_0),
    )
    .expect("SPI2 config")
    .with_sck(peripherals.GPIO8)
    .with_mosi(peripherals.GPIO10)
    .with_miso(peripherals.GPIO7)
    .with_dma(peripherals.DMA_CH0)
    .with_buffers(dma_rx, dma_tx)
    .into_async();
    let epd_bus = hal_ext::spi_dma::EpdBus::new(epd_spi, epd_cs, epd_dc, epd_busy, epd_rst);

    let executor = EXECUTOR.init(Executor::new());

    // Wrong-board refusal, placed at the first point where everything it needs
    // exists (SPI bus, SD chip select) and the last before anything
    // board-specific starts: nothing spawned, no panel init, no geometry work.
    // The recovery combo stays ahead of it, so Back + Up still reaches the
    // slot-0 anchor on a device this would otherwise stop.
    if let Some(mismatch) = board_mismatch {
        executor.run(|spawner: Spawner| {
            spawner.spawn(board_guard::refuse(epd_bus, sd_cs, mismatch).unwrap());
        });
    }

    // Input polls from an interrupt-priority executor on both boards, so
    // button sampling keeps running while the thread executor blocks on
    // SD/EPUB work; a cold cache build no longer deafens the buttons.
    // Channels between the tasks already use CriticalSectionRawMutex, so
    // handoff is unchanged. The X3's fuel gauge no longer rides in this
    // loop: its clock-stretched I2C reads have no place at interrupt
    // priority, so a thread-executor task below samples it instead.
    {
        let input_executor =
            INPUT_EXECUTOR.init(InterruptExecutor::new(sw_ints.software_interrupt1));
        let input_spawner = input_executor.start(Priority::Priority1);
        esp_println::println!("main: spawn input t_ms={}", Instant::now().as_millis());
        input_spawner.spawn(
            tasks::input::run(
                adc1,
                InputPins {
                    power: Some(power_button),
                    #[cfg(not(feature = "device-x3"))]
                    aux_pin: aux_adc,
                    nav_pin: nav_adc,
                    page_pin: page_adc,
                },
            )
            .unwrap(),
        );
    }

    executor.run(|spawner: Spawner| {
        #[cfg(feature = "device-x3")]
        {
            esp_println::println!("main: spawn battery t_ms={}", Instant::now().as_millis());
            spawner.spawn(tasks::input::battery_run(battery_gauge).unwrap());
        }
        esp_println::println!("main: spawn display t_ms={}", Instant::now().as_millis());
        // Some unless the guard refused, and that path diverges above.
        let probe_diag = probe_diag.expect("panel probe runs on every non-refusing boot");
        spawner.spawn(tasks::display::run(epd_bus, sd_cs, deep_sleep_wake, probe_diag).unwrap());
        esp_println::println!("main: spawn power t_ms={}", Instant::now().as_millis());
        spawner.spawn(tasks::power::run(peripherals.LPWR).unwrap());
        esp_println::println!("main: spawn app t_ms={}", Instant::now().as_millis());
        spawner.spawn(tasks::app::run().unwrap());
        esp_println::println!("main: spawn wifi t_ms={}", Instant::now().as_millis());
        spawner.spawn(tasks::wifi::run(spawner, peripherals.WIFI).unwrap());
    })
}
