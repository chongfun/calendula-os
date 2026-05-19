#![no_std]
#![forbid(unsafe_code)]

pub mod fb;
pub mod epd;
pub mod render;

pub const FB_WIDTH:  usize = 800;
pub const FB_HEIGHT: usize = 480;
pub const FB_BYTES:  usize = FB_WIDTH * FB_HEIGHT / 8;        // 48_000
pub const BAND_ROWS: usize = 80;
pub const BAND_BYTES: usize = FB_WIDTH * BAND_ROWS / 8;        // 8_000

const _: () = assert!(FB_BYTES == 48_000);
const _: () = assert!(BAND_BYTES <= 16_384, "band exceeds DMA buffer budget");
