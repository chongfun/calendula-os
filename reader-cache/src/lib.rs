//! The reader's on-card cache: the store it renders from, the files that back
//! it, and the publish tail that swaps one for the other.
//!
//! Split out of `fw` so it can be tested. The progressive-open work (B4) took
//! six review rounds, and every defect was in the same place — the publish
//! tail's *cleanup ordering*, where a write fails after the store has already
//! been updated: deleting a cache the reader was reading from, returning before
//! restoring the shared text arena, settling for an index nobody was still
//! building. None of it was subtle logic; all of it is the class a
//! fault-injection harness catches on the first run. It had no coverage because
//! this code lived in a `#![no_main]` firmware binary with no host tests.
//!
//! So the boundary is drawn at the seam that needed testing: what decides
//! *which bytes land on the card and in what order* moves here; what needs the
//! target stays in `fw`. Nothing in this crate touches `esp_hal`, the EPD, or
//! the zip/XML walk. The card is an [`embedded_sdmmc::BlockDevice`], which the
//! tests supply as an in-memory FAT image that can fail any chosen read or
//! write — the same harness `upload-store` already uses.
//!
//! Telemetry deliberately does not come along. The publish functions used to
//! take `Instant` and SD-counter snapshots purely to print one bench line;
//! they now return their outcome and `fw` prints it. Storage decisions in the
//! crate, telemetry at the call site.

#![no_std]
#![forbid(unsafe_code)]

#[macro_use]
mod log;

pub mod files;
pub mod layout;
pub mod publish;
pub mod store;

// Working-buffer sizes for the cache layer. They live here rather than beside
// the EPUB walk in `fw` because the on-card format depends on them: a CONT.BIN
// record longer than `READER_XHTML_SCRATCH` could be written but never replayed,
// so the file layer has to enforce the bound it is named for. One constant, used
// by both sides -- two that had to agree is the shape of a bug this crate exists
// to stop.
pub const READER_TAIL_SCRATCH: usize = 4096;
pub const READER_HEADER_SCRATCH: usize = 46;
// All zip reads stream through the shared inflate engine in bounded chunks,
// so this only sets the fetch granularity; SD transfers are 2 KB ops either
// way. Kept small so the freed static RAM widens the stack region.
pub const READER_COMPRESSED_SCRATCH: usize = 8192;
pub const READER_CONTAINER_SCRATCH: usize = 4096;
pub const READER_OPF_SCRATCH: usize = 16_384;
pub const READER_XHTML_SCRATCH: usize = 24_576;
