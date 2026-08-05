//! The persistent-authority layer of the M0S source-transaction foundation:
//! commit-sector record framing, the durable publication sequence, and the
//! startup selection rules that decide which records are allowed to matter
//! after a power cut.
//!
//! ## Why this exists as its own crate
//!
//! The image-rendering PRD (`.scratch/on-device-image-rendering/PRD.md`,
//! Part 2) draws a hard line that the existing storage code does not: a record
//! that can be closed, reopened, reread, and checksum-verified is *not*
//! thereby committed. Reread proves structural readability; it proves nothing
//! about surviving a power loss that happens one instruction later. Both
//! `proto::durable` (the two-generation settings records) and
//! `reader-cache::publish` (the cache publish tail) stop at reread-validation,
//! which is the right cost for state whose loss is an inconvenience. Source
//! authority — which EPUB generation a logical book *is*, whether a book is
//! deleted, whether an upload happened — cannot lose a committed state or
//! resurrect an uncommitted one, so it gets the stronger protocol, and the
//! protocol lives here where host tests can drive every failure boundary.
//!
//! Like `reader-cache`, everything is generic over
//! [`embedded_sdmmc::BlockDevice`] so the test suite can supply an in-memory
//! FAT image that fails, cuts, or tears any chosen write.
//!
//! ## The durability contract
//!
//! [`publish::durable_sync`] is the one primitive every authoritative commit
//! goes through. Over the pinned `embedded-sdmmc` it is
//! [`File::flush`][embedded_sdmmc::File::flush], and that is only sufficient
//! because of three properties this crate *documents but cannot enforce*, so
//! they are stated here as the contract the block device must uphold:
//!
//! 1. **Write-through**: every `VolumeManager` mutation writes its cached
//!    block back to the device before returning (true of the pinned rev's
//!    single-block `BlockCache`; data, FAT, and directory writes all go
//!    through it). No dirty state outlives a call.
//! 2. **Completion on return**: `BlockDevice::write` does not return until
//!    the card reports the write complete. The SPI `SdCard` driver busy-waits
//!    (`wait_not_busy`) after every data block, which is the SD SPI-mode
//!    contract for "programming finished".
//! 3. **`flush` covers the metadata**: `flush_file` persists the directory
//!    entry (file length) and FAT info sector, the only mutations the write
//!    path defers.
//!
//! Whether real cards honour property 2 under an actual power cut is exactly
//! what the PRD's hardware power-cut gate exists to verify; nothing in
//! software can substitute for it, and this crate does not claim to.
//!
//! ## Record framing
//!
//! Every authoritative record is one file laid out as:
//!
//! ```text
//! logical body | zero padding to a 512-byte boundary | 512-byte commit sector
//! ```
//!
//! The body carries its own magic, schema, exact length, generation, and a
//! CRC over body-plus-padding. The commit sector is written **only after**
//! the body has been durably synced and reread, and names the exact body it
//! commits (generation, padded length, body CRC). A record whose commit
//! sector is missing, zeroed, torn, or malformed is *prepared* — visible to
//! cleanup, invisible to authority selection. See [`record`] for the exact
//! classification rules and [`publish`] for the write sequence.

#![no_std]
#![forbid(unsafe_code)]

pub mod bodies;
pub mod layout;
pub mod ops;
pub mod publish;
pub mod receipts;
pub mod record;
pub mod select;
