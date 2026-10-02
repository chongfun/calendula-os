//! The storage half of the firmware's display task: the library scan, book
//! open and cache, saved reader state, and the commands that drive them.
//!
//! It lives outside `fw` so it can run on the host: `fw` implements
//! [`card::Card`] over its SPI session, and the tests over an in-memory FAT
//! image. The panel, the radio and the scratch loaned to it stay in `fw`.

#![no_std]
#![forbid(unsafe_code)]

#[macro_use]
mod log;

pub mod book_build;
pub mod card;
pub mod custom_font;
pub mod library_sd;
pub mod platform;
pub mod progress;
pub mod sd_stats;
pub mod task;
