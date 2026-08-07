#![no_std]
#![forbid(unsafe_code)]

pub mod bq27220;
// The panel-controller probe bit-bangs the display bus by hand on GPIO numbers
// that only mean what it assumes on the Xteink ESP32-C3 boards. The reTerminal
// Sticky is an Xtensa ESP32-S3 where those pins are something else entirely,
// and there is no evidence it ships more than one controller, so the whole
// module stays out of an Xtensa build rather than relying on nobody calling it.
#[cfg(target_arch = "riscv32")]
pub mod epd_probe;
pub mod rtc;
pub mod spi_dma;
