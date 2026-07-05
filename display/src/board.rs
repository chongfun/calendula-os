//! The board description: panel geometry, board identity, and bus
//! parameters. One module per supported board, gated by a same-named
//! Cargo feature; exactly one board feature is enabled at a time (see
//! Cargo.toml), and the crate root re-exports its geometry so shared code
//! stays board-blind.

#[cfg(feature = "board-x4")]
mod x4;
#[cfg(feature = "board-x4")]
pub use x4::*;

#[cfg(feature = "board-x3")]
mod x3;
#[cfg(feature = "board-x3")]
pub use x3::*;

#[cfg(all(feature = "board-x4", feature = "board-x3"))]
compile_error!("`board-x4` and `board-x3` are mutually exclusive");
#[cfg(not(any(feature = "board-x4", feature = "board-x3")))]
compile_error!("enable exactly one board-* feature, e.g. board-x4 or board-x3");
