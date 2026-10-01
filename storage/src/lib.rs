//! The storage half of the firmware's display task: the library scan, the
//! book open and its cache, the saved reader state, and the commands that
//! drive them.
//!
//! Split out of `fw` for the reason `reader-cache` was: code in a `no_main`
//! firmware binary cannot run on the host, and the defects that reached the
//! device in September 2026 were all here, in how these pieces fit together
//! across a computer moving a book while the device was off. The card is
//! reached only through [`card::Card`], which `fw` implements over its SPI
//! session and the tests over an in-memory FAT image. What needs the target
//! (the panel, the radio, the scratch loaned to it) stays in `fw`.

#![no_std]
#![forbid(unsafe_code)]

#[macro_use]
mod log;

pub mod book_build;
pub mod card;
pub mod custom_font;
pub mod library_sd;
pub mod platform;
pub mod sd_stats;
pub mod task;
