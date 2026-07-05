//! Board-specific firmware configuration: pin wiring, the ADC button-ladder
//! tables, the boot-recovery combo windows, and the dram2 radio-heap budget.
//! One module per supported board, gated by a same-named Cargo feature;
//! exactly one board feature is enabled at a time (see Cargo.toml).

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

/// One rung of an ADC resistor-ladder button table: the millivolt band
/// that reads as a given hardware button.
#[derive(Clone, Copy)]
pub(crate) struct Band {
    pub(crate) min: u16,
    pub(crate) max: u16,
    pub(crate) button: HardwareButton,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HardwareButton {
    Back,
    Confirm,
    Left,
    Right,
    Up,
    Down,
}
