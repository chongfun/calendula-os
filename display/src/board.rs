//! The board description: panel geometry, board identity, and bus
//! parameters. One module per supported board, gated by a same-named
//! Cargo feature; exactly one board feature is enabled at a time (see
//! Cargo.toml), and the crate root re-exports its geometry so shared code
//! stays board-blind.

#[cfg(feature = "board-x4")]
mod x4;

#[cfg(feature = "board-x4")]
pub use x4::*;

#[cfg(not(feature = "board-x4"))]
compile_error!("enable exactly one board-* feature, e.g. board-x4");
