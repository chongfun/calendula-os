//! The panel flush path. The shared `Epd` bus type lives here; the
//! controller-specific init/flush/sleep sequences live in one module per
//! board, and exactly one is active.

use esp_hal::gpio::{Input, Output};
use esp_hal::spi::master::SpiDmaBus;
use esp_hal::Async;

#[cfg(feature = "board-x4")]
mod x4;
#[cfg(feature = "board-x4")]
pub(crate) use x4::*;

#[cfg(feature = "board-x3")]
mod x3;
#[cfg(feature = "board-x3")]
pub(crate) use x3::*;

#[cfg(all(feature = "board-x4", feature = "board-x3"))]
compile_error!("`board-x4` and `board-x3` are mutually exclusive");
#[cfg(not(any(feature = "board-x4", feature = "board-x3")))]
compile_error!("enable exactly one board-* feature, e.g. board-x4 or board-x3");

pub(crate) type Epd = hal_ext::spi_dma::EpdBus<
    SpiDmaBus<'static, Async>,
    Output<'static>,
    Output<'static>,
    Input<'static>,
    Output<'static>,
>;
