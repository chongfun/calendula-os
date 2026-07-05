//! Xteink X4: 800x480 panel on an SSD1677 controller.

pub const WIDTH: usize = 800;
pub const HEIGHT: usize = 480;
pub const BAND_ROWS: usize = 80;

pub const BOARD_NAME: &str = "Xteink X4";
pub const PORTAL_SSID: &str = "XTEINK-X4";

pub const DISPLAY_SPI_MHZ: u32 = 40;

/// Panel byte transforms for the SSD1677's scan direction and bit order.
pub const MIRROR_X: bool = true;
pub const MIRROR_Y: bool = false;
pub const REVERSE_BITS: bool = true;

/// The SSD1677's BUSY line idles low and asserts high during a refresh.
pub const BUSY_ACTIVE_HIGH: bool = true;

const _: () = assert!(crate::FB_BYTES == 48_000);
const _: () = assert!(crate::BAND_BYTES == 8_000);
