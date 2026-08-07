//! Panel flush backends behind the display task's seam.
//!
//! The display task drives four operations — `init_panel`, `flush`,
//! `prestage_previous`, `sleep_panel` — plus the shared `Epd` bus type. The
//! task never sees command bytes or RAM-plane names. Both controllers keep
//! a previous-frame plane for differential fast refreshes (SSD1677: RED
//! RAM; UC8253: DTM1), which is what `prestage_previous` and `flush`'s
//! `prev_staged` speak to.
//!
//! *Which* controller implements them is two decisions, not one. The device
//! build picks the family — X4 code carries the SSD1677 backend, X3 code the
//! UC8253 one — and that stays a compile-time choice, because the two devices
//! differ in geometry, waveforms and battery hardware, not just in silicon.
//! Within a family, newer production runs swap in an UltraChip sibling
//! (SSD1677 to UC8179, UC8253 to UC8279d) that no external marking reveals, so
//! that half is a runtime choice, taken from the boot probe in
//! `hal_ext::epd_probe` and dispatched on here.

use core::sync::atomic::{AtomicU8, Ordering};
use display::epd::probe::ProbeVerdict;
use display::epd::RefreshMode;
use display::fb::Framebuffer;
use esp_hal::gpio::{Input, Output};
use esp_hal::spi::master::SpiDmaBus;
use esp_hal::Async;

#[cfg(not(feature = "device-x3"))]
mod ssd1677;
#[cfg(feature = "device-x3")]
mod uc8253;

#[cfg(not(feature = "device-x3"))]
use ssd1677 as default_backend;
#[cfg(feature = "device-x3")]
use uc8253 as default_backend;

pub(crate) type Epd = hal_ext::spi_dma::EpdBus<
    SpiDmaBus<'static, Async>,
    Output<'static>,
    Output<'static>,
    Input<'static>,
    Output<'static>,
>;

pub(crate) type SpiError = <SpiDmaBus<'static, Async> as embedded_hal_async::spi::ErrorType>::Error;

/// Which panel controller this unit turned out to carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DetectedController {
    /// The controller this device build was written for: SSD1677 on the X4,
    /// UC8253 on the X3. Every unit shipped so far, and the answer whenever
    /// the probe is not certain of anything else.
    Default,
    /// The UltraChip sibling newer production runs carry in its place:
    /// UC8179 on the X4, UC8279d on the X3.
    UltraChipSibling,
}

impl DetectedController {
    /// The part name, for logs and the on-card probe report.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            #[cfg(not(feature = "device-x3"))]
            Self::Default => "SSD1677",
            #[cfg(feature = "device-x3")]
            Self::Default => "UC8253",
            #[cfg(not(feature = "device-x3"))]
            Self::UltraChipSibling => "UC8179",
            #[cfg(feature = "device-x3")]
            Self::UltraChipSibling => "UC8279d",
        }
    }
}

const CODE_DEFAULT: u8 = 0;
const CODE_SIBLING: u8 = 1;

/// The probe's verdict, reduced to the one bit the four operations below
/// dispatch on. Written once from `main` before any task is spawned and only
/// read afterwards, so `Relaxed` is enough; it is an atomic rather than a
/// plain static because the readers are on another task.
static DETECTED_CONTROLLER: AtomicU8 = AtomicU8::new(CODE_DEFAULT);

/// Record what the boot probe found. `Inconclusive` deliberately lands on
/// `Default`: a verdict the probe itself would not stand behind must never
/// promote a driver (see `hal_ext::epd_probe`, rule 1).
pub(crate) fn record_probe_verdict(verdict: ProbeVerdict) {
    let (controller, code) = match verdict {
        ProbeVerdict::Uc81xxConfirmed => (DetectedController::UltraChipSibling, CODE_SIBLING),
        ProbeVerdict::DefaultAssumed | ProbeVerdict::Inconclusive => {
            (DetectedController::Default, CODE_DEFAULT)
        }
    };
    DETECTED_CONTROLLER.store(code, Ordering::Relaxed);
    // Not behind `bench_log!`: this is boot identity, and it is the first
    // thing to look at when a unit renders wrong.
    esp_println::println!(
        "display: controller probe {} -> {}",
        verdict.as_str(),
        controller.name()
    );
}

pub(crate) fn detected_controller() -> DetectedController {
    match DETECTED_CONTROLLER.load(Ordering::Relaxed) {
        CODE_SIBLING => DetectedController::UltraChipSibling,
        _ => DetectedController::Default,
    }
}

/// Which backend the four operations below actually execute.
///
/// Deliberately not the same question as [`detected_controller`], and the gap
/// is the point. The UltraChip sibling backends are separate work
/// (`.scratch/uc8179-x4-driver`, `.scratch/uc8279-x3-driver`) and both wait on
/// this dispatch layer to have somewhere to plug into, so until one lands a
/// confirmed sibling runs the default sequence. That is the same thing it
/// would get with no probe at all, and better than refusing to drive the
/// panel.
///
/// It is a separate function because `probe_report` must print both. A
/// diagnostics file that names a UC8279d as the driver while the UC8253
/// sequence is running would mislead precisely the person it exists to help —
/// someone whose screen renders wrong on a controller the probe identified
/// correctly.
///
/// When a sibling backend lands, return the detected controller here and give
/// the four `match`es below their second arm; they follow this function.
pub(crate) fn active_backend() -> DetectedController {
    match detected_controller() {
        // No sibling backend to route to yet.
        DetectedController::Default | DetectedController::UltraChipSibling => {
            DetectedController::Default
        }
    }
}

pub(crate) async fn init_panel(epd: &mut Epd) -> Result<(), PanelError> {
    match active_backend() {
        DetectedController::Default | DetectedController::UltraChipSibling => {
            default_backend::init_panel(epd).await
        }
    }
}

pub(crate) async fn flush(
    epd: &mut Epd,
    fb: &Framebuffer,
    prev_fb: &Framebuffer,
    screen_on: bool,
    mode: RefreshMode,
    prev_staged: bool,
) -> Result<(), PanelError> {
    match active_backend() {
        DetectedController::Default | DetectedController::UltraChipSibling => {
            default_backend::flush(epd, fb, prev_fb, screen_on, mode, prev_staged).await
        }
    }
}

pub(crate) async fn prestage_previous(epd: &mut Epd, fb: &Framebuffer) -> Result<(), PanelError> {
    match active_backend() {
        DetectedController::Default | DetectedController::UltraChipSibling => {
            default_backend::prestage_previous(epd, fb).await
        }
    }
}

pub(crate) async fn sleep_panel(epd: &mut Epd) -> Result<(), PanelError> {
    match active_backend() {
        DetectedController::Default | DetectedController::UltraChipSibling => {
            default_backend::sleep_panel(epd).await
        }
    }
}

/// Why a panel operation failed: the SPI transfer itself errored, or the
/// BUSY handshake after a command never completed. Either way the panel's
/// RAM/waveform state is unknown, so callers must not report the frame as
/// settled or the panel as asleep.
// The payloads are read only through the derived Debug in log lines, which
// dead_code does not count as a use; both device builds compile this module
// the same way, so the expectation is fulfilled on X4 and X3 alike.
#[expect(dead_code, reason = "The payloads exist for the Debug log line.")]
#[derive(Debug)]
pub(crate) enum PanelError {
    Spi(SpiError),
    Busy(hal_ext::spi_dma::BusyError),
}

impl From<esp_hal::spi::Error> for PanelError {
    fn from(value: esp_hal::spi::Error) -> Self {
        Self::Spi(value)
    }
}

impl From<hal_ext::spi_dma::BusyError> for PanelError {
    fn from(value: hal_ext::spi_dma::BusyError) -> Self {
        Self::Busy(value)
    }
}
