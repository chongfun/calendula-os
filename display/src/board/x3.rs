//! Xteink X3: 792x528 panel on a UC8253 controller.

pub const WIDTH: usize = 792;
pub const HEIGHT: usize = 528;
pub const BAND_ROWS: usize = 40;

pub const BOARD_NAME: &str = "Xteink X3";
pub const PORTAL_SSID: &str = "XTEINK-X3";

pub const DISPLAY_SPI_MHZ: u32 = 16;

/// Panel byte transforms for the UC8253's scan direction and bit order.
pub const MIRROR_X: bool = true;
pub const MIRROR_Y: bool = true;
pub const REVERSE_BITS: bool = true;

/// The UC8253's BUSY line idles high and asserts low during a refresh.
pub const BUSY_ACTIVE_HIGH: bool = false;

const _: () = assert!(crate::FB_BYTES == 52_272);
const _: () = assert!(crate::BAND_BYTES == 3_960);
