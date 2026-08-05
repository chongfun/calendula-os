//! Commit-sector record framing: layout, sealing, and classification.
//!
//! Everything here is pure over byte slices — no I/O, no allocation — so the
//! rules that decide what is *committed* can be unit-tested exhaustively and
//! reused unchanged by the publish path, the startup selector, and host
//! tools. The I/O side (writing, syncing, rereading) lives in
//! [`crate::publish`].
//!
//! A record file is:
//!
//! ```text
//! +--------------------------+  offset 0
//! | logical body             |  starts with the common prefix below,
//! |   ..type-specific fields |  ends with body_crc32
//! +--------------------------+  offset logical_len
//! | zero padding             |  to the next 512-byte boundary
//! +--------------------------+  offset padded_len (multiple of 512)
//! | commit sector            |  exactly 512 bytes, 512-aligned
//! +--------------------------+  offset padded_len + 512 == file length
//! ```
//!
//! The dedicated commit sector means the one write that flips a record from
//! prepared to committed never shares a logical sector with authoritative
//! body bytes: a torn commit write can tear only the commit sector, and the
//! sector's own CRC catches the tear.
//!
//! Common body prefix (little-endian):
//!
//! ```text
//! 0..4   magic            record-type-specific four bytes
//! 4..6   schema_version   u16
//! 6..8   reserved         u16, zero
//! 8..12  logical_len      u32, exact logical body length including prefix+CRC
//! 12..20 generation       u64, >= 1, monotonic per record slot pair
//! ```
//!
//! The body CRC is the last four bytes of the logical body and covers the
//! *padded* body (prefix, fields, padding) with the CRC field itself zeroed.
//! Covering the padding is deliberate: canonical zero padding is part of the
//! format, and a record whose padding carries stray bytes must not classify
//! as valid no matter how it got that way.

use core::ops::Range;

/// Size of the commit sector, and the alignment it must start at. One
/// 512-byte logical sector on every card protocol v1 supports.
pub const COMMIT_FOOTER_BYTES: usize = 512;
pub const COMMIT_ALIGNMENT: usize = 512;

/// Bytes of the common prefix every logical body starts with.
pub const BODY_PREFIX_BYTES: usize = 20;
/// The body CRC trailing every logical body.
pub const BODY_CRC_BYTES: usize = 4;
/// The smallest logical body the framing accepts: prefix plus CRC and
/// nothing else. Real record types are all larger.
pub const MIN_LOGICAL_BODY_BYTES: usize = BODY_PREFIX_BYTES + BODY_CRC_BYTES;

/// Every commit sector starts with these bytes regardless of record type;
/// the record type is named by the *body* magic. One commit format for all
/// record classes keeps the classifier singular.
pub const COMMIT_MAGIC: [u8; 4] = *b"XTCM";
pub const COMMIT_SCHEMA_VERSION: u16 = 1;

// Commit-sector field offsets (little-endian).
const CS_MAGIC: Range<usize> = 0..4;
const CS_SCHEMA: Range<usize> = 4..6;
const CS_RESERVED_HEAD: Range<usize> = 6..8;
const CS_GENERATION: Range<usize> = 8..16;
const CS_PADDED_LEN: Range<usize> = 16..24;
const CS_BODY_CRC: Range<usize> = 24..28;
const CS_NONCE: Range<usize> = 28..36;
const CS_RESERVED_TAIL: Range<usize> = 36..508;
const CS_CRC: Range<usize> = 508..512;

/// CRC-32/ISO-HDLC — the ubiquitous "crc32". Chosen over rolling our own so
/// the algorithm identity is nameable in the capabilities response
/// (`payload-checksum algorithm`) and reproducible in the browser.
const CRC32: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);

/// CRC over `bytes` with the four bytes at `hole` treated as zero — how both
/// the body CRC (field inside the body) and the commit CRC (field inside the
/// sector) are defined, so neither covers itself.
fn crc32_with_zeroed_field(bytes: &[u8], hole: Range<usize>) -> Option<u32> {
    if hole.end.checked_sub(hole.start)? != BODY_CRC_BYTES || hole.end > bytes.len() {
        return None;
    }
    let mut digest = CRC32.digest();
    digest.update(bytes.get(..hole.start)?);
    digest.update(&[0u8; BODY_CRC_BYTES]);
    digest.update(bytes.get(hole.end..)?);
    Some(digest.finalize())
}

/// `logical_len` rounded up to the next 512-byte boundary; `None` on
/// overflow or a length below the framing minimum. `const` so workspace
/// buffers can be sized from record layouts at compile time.
pub const fn padded_body_len(logical_len: usize) -> Option<usize> {
    if logical_len < MIN_LOGICAL_BODY_BYTES {
        return None;
    }
    match logical_len.checked_add(COMMIT_ALIGNMENT - 1) {
        None => None,
        Some(up) => Some(up & !(COMMIT_ALIGNMENT - 1)),
    }
}

/// Total record file length for a logical body: padded body plus commit
/// sector.
pub const fn record_file_len(logical_len: usize) -> Option<usize> {
    match padded_body_len(logical_len) {
        None => None,
        Some(padded) => padded.checked_add(COMMIT_FOOTER_BYTES),
    }
}

/// What [`seal_body`] produced: everything the publish path needs to build
/// the matching commit sector without re-deriving it from bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SealedBody {
    pub generation: u64,
    pub logical_len: usize,
    pub padded_len: usize,
    pub body_crc32: u32,
}

/// Stamp the common prefix, canonical padding, and body CRC onto a logical
/// body whose type-specific fields the caller has already written into
/// `buf[BODY_PREFIX_BYTES..logical_len - BODY_CRC_BYTES]`.
///
/// `buf` must hold at least the padded body; the padding bytes are zeroed
/// here rather than trusted, so a reused scratch buffer cannot leak a stale
/// record's bytes into the new record's covered padding.
pub fn seal_body(
    magic: [u8; 4],
    schema_version: u16,
    generation: u64,
    logical_len: usize,
    buf: &mut [u8],
) -> Option<SealedBody> {
    let padded_len = padded_body_len(logical_len)?;
    if generation == 0 || buf.len() < padded_len {
        return None;
    }
    buf[0..4].copy_from_slice(&magic);
    buf[4..6].copy_from_slice(&schema_version.to_le_bytes());
    buf[6..8].fill(0);
    let len32 = u32::try_from(logical_len).ok()?;
    buf[8..12].copy_from_slice(&len32.to_le_bytes());
    buf[12..20].copy_from_slice(&generation.to_le_bytes());
    buf[logical_len..padded_len].fill(0);
    let crc_field = logical_len - BODY_CRC_BYTES..logical_len;
    let crc = crc32_with_zeroed_field(&buf[..padded_len], crc_field.clone())?;
    buf[crc_field].copy_from_slice(&crc.to_le_bytes());
    Some(SealedBody {
        generation,
        logical_len,
        padded_len,
        body_crc32: crc,
    })
}

/// Build the commit sector naming one sealed body. Written to the record's
/// final 512 bytes only after the body's durable sync — see
/// [`crate::publish`].
pub fn encode_commit_sector(sealed: &SealedBody, nonce: u64) -> Option<[u8; COMMIT_FOOTER_BYTES]> {
    let mut sector = [0u8; COMMIT_FOOTER_BYTES];
    sector[CS_MAGIC].copy_from_slice(&COMMIT_MAGIC);
    sector[CS_SCHEMA].copy_from_slice(&COMMIT_SCHEMA_VERSION.to_le_bytes());
    sector[CS_GENERATION].copy_from_slice(&sealed.generation.to_le_bytes());
    let padded = u64::try_from(sealed.padded_len).ok()?;
    sector[CS_PADDED_LEN].copy_from_slice(&padded.to_le_bytes());
    sector[CS_BODY_CRC].copy_from_slice(&sealed.body_crc32.to_le_bytes());
    sector[CS_NONCE].copy_from_slice(&nonce.to_le_bytes());
    let crc = crc32_with_zeroed_field(&sector, CS_CRC)?;
    sector[CS_CRC].copy_from_slice(&crc.to_le_bytes());
    Some(sector)
}

/// A structurally valid logical body, borrowed out of the record file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordView<'a> {
    pub schema_version: u16,
    pub generation: u64,
    /// The exact logical body, prefix and CRC included; typed decoders in
    /// [`crate::bodies`] take this.
    pub logical_body: &'a [u8],
}

/// The four states of the PRD's record protocol. Only [`Committed`]
/// participates in authority selection; the others differ only in what
/// cleanup may do with them.
///
/// [`Committed`]: RecordState::Committed
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordState<'a> {
    /// No file, or a zero-length file: never written, or reclaimed.
    Absent,
    /// Valid body, but the commit sector is zeroed, malformed, torn, or of
    /// an unknown commit schema. The normal state between the two durable
    /// syncs of a publication; also what an unknown *newer* commit format
    /// classifies as on old firmware, which errs on the side of not
    /// treating bytes it cannot interpret as authority.
    Prepared(RecordView<'a>),
    /// Valid body and a CRC-valid commit sector naming exactly this body.
    Committed(RecordView<'a>),
    /// Framing, length, padding, or checksum inconsistencies — including a
    /// CRC-valid commit sector that names a *different* body. That last case
    /// can never result from a cleanly interrupted publication (the sector
    /// is only ever written after its body's durable sync, from the
    /// validated body's own values), so it is flagged as corruption
    /// requiring reclamation rather than quietly demoted to prepared.
    Corrupt,
}

impl RecordState<'_> {
    /// The committed generation, if this record is committed. What the
    /// selector compares; prepared generations deliberately have no getter
    /// so no caller can rank by them.
    pub fn committed_generation(&self) -> Option<u64> {
        match self {
            RecordState::Committed(view) => Some(view.generation),
            _ => None,
        }
    }
}

/// Classify one record file image against the PRD's state rules.
///
/// `file` is the complete file as read from disk; the caller maps a missing
/// file to an empty slice. `expected_magic` is the record type's body magic —
/// a valid record of the *wrong* type classifies as corrupt, because a typed
/// slot containing some other record's bytes is exactly the cross-linked
/// state the classifier exists to catch.
pub fn classify_record<'a>(file: &'a [u8], expected_magic: [u8; 4]) -> RecordState<'a> {
    if file.is_empty() {
        return RecordState::Absent;
    }
    // Structure: multiple of 512, at least one padded-body sector plus the
    // commit sector.
    if !file.len().is_multiple_of(COMMIT_ALIGNMENT)
        || file.len() < COMMIT_ALIGNMENT + COMMIT_FOOTER_BYTES
    {
        return RecordState::Corrupt;
    }
    let padded_len = file.len() - COMMIT_FOOTER_BYTES;

    // Body prefix.
    if file[CS_MAGIC] != expected_magic || file[6..8] != [0, 0] {
        return RecordState::Corrupt;
    }
    let schema_version = u16::from_le_bytes([file[4], file[5]]);
    let logical_len = u32::from_le_bytes([file[8], file[9], file[10], file[11]]) as usize;
    let generation = u64::from_le_bytes([
        file[12], file[13], file[14], file[15], file[16], file[17], file[18], file[19],
    ]);
    if generation == 0
        || logical_len < MIN_LOGICAL_BODY_BYTES
        || padded_body_len(logical_len) != Some(padded_len)
    {
        return RecordState::Corrupt;
    }

    // Canonical zero padding.
    if file[logical_len..padded_len].iter().any(|byte| *byte != 0) {
        return RecordState::Corrupt;
    }

    // Body CRC over the padded body with the CRC field zeroed.
    let crc_field = logical_len - BODY_CRC_BYTES..logical_len;
    let stored_crc = u32::from_le_bytes([
        file[crc_field.start],
        file[crc_field.start + 1],
        file[crc_field.start + 2],
        file[crc_field.start + 3],
    ]);
    let Some(computed) = crc32_with_zeroed_field(&file[..padded_len], crc_field) else {
        return RecordState::Corrupt;
    };
    if computed != stored_crc {
        return RecordState::Corrupt;
    }

    let view = RecordView {
        schema_version,
        generation,
        logical_body: &file[..logical_len],
    };

    // Commit sector: the final 512 bytes.
    let sector = &file[padded_len..];
    if sector.iter().all(|byte| *byte == 0) {
        return RecordState::Prepared(view);
    }
    let torn_or_malformed = sector[CS_MAGIC] != COMMIT_MAGIC
        || crc32_with_zeroed_field(sector, CS_CRC)
            != Some(u32::from_le_bytes([
                sector[CS_CRC.start],
                sector[CS_CRC.start + 1],
                sector[CS_CRC.start + 2],
                sector[CS_CRC.start + 3],
            ]));
    if torn_or_malformed {
        return RecordState::Prepared(view);
    }
    // An unknown commit schema is never committed; reserved bytes must be
    // zero for *this* schema, and a nonzero one under schema 1 is malformed.
    let commit_schema = u16::from_le_bytes([sector[CS_SCHEMA.start], sector[CS_SCHEMA.start + 1]]);
    if commit_schema != COMMIT_SCHEMA_VERSION {
        return RecordState::Prepared(view);
    }
    if sector[CS_RESERVED_HEAD].iter().any(|byte| *byte != 0)
        || sector[CS_RESERVED_TAIL].iter().any(|byte| *byte != 0)
    {
        return RecordState::Prepared(view);
    }

    // A CRC-valid schema-1 sector must name exactly this body.
    let cs_generation = u64::from_le_bytes([
        sector[8], sector[9], sector[10], sector[11], sector[12], sector[13], sector[14],
        sector[15],
    ]);
    let cs_padded_len = u64::from_le_bytes([
        sector[16], sector[17], sector[18], sector[19], sector[20], sector[21], sector[22],
        sector[23],
    ]);
    let cs_body_crc = u32::from_le_bytes([sector[24], sector[25], sector[26], sector[27]]);
    let matches_body = cs_generation == generation
        && u64::try_from(padded_len) == Ok(cs_padded_len)
        && cs_body_crc == stored_crc;
    if matches_body {
        RecordState::Committed(view)
    } else {
        RecordState::Corrupt
    }
}

/// Startup selection over one record's A/B slot pair, applying the PRD
/// rules: only committed records participate, the higher committed
/// generation wins, and equal committed generations are ambiguous authority
/// — corruption to report, never a coin flip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    /// Neither slot is committed.
    None,
    /// Exactly one authoritative slot.
    Selected { slot: usize, generation: u64 },
    /// Both slots committed with the same generation. A/B publication can
    /// never legitimately produce this.
    Ambiguous,
}

pub fn select_slot(a: &RecordState<'_>, b: &RecordState<'_>) -> Selection {
    select_generations(a.committed_generation(), b.committed_generation())
}

/// The selection rule itself, over the only inputs it may consider:
/// committed generations. Shared by [`select_slot`] and the publish path's
/// summary-based selection so the two cannot drift.
pub fn select_generations(a: Option<u64>, b: Option<u64>) -> Selection {
    match (a, b) {
        (None, None) => Selection::None,
        (Some(generation), None) => Selection::Selected {
            slot: 0,
            generation,
        },
        (None, Some(generation)) => Selection::Selected {
            slot: 1,
            generation,
        },
        (Some(gen_a), Some(gen_b)) => {
            if gen_a == gen_b {
                Selection::Ambiguous
            } else if gen_a > gen_b {
                Selection::Selected {
                    slot: 0,
                    generation: gen_a,
                }
            } else {
                Selection::Selected {
                    slot: 1,
                    generation: gen_b,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec;
    use std::vec::Vec;

    const MAGIC: [u8; 4] = *b"TSTR";
    const SCHEMA: u16 = 1;

    /// A sealed record file image: body of `extra` type-specific bytes, all
    /// 0xAB, plus a valid commit sector when `committed`.
    fn build_record(generation: u64, extra: usize, committed: bool) -> Vec<u8> {
        let logical_len = BODY_PREFIX_BYTES + extra + BODY_CRC_BYTES;
        let file_len = record_file_len(logical_len).unwrap();
        let mut file = vec![0u8; file_len];
        file[BODY_PREFIX_BYTES..BODY_PREFIX_BYTES + extra].fill(0xAB);
        let sealed = seal_body(MAGIC, SCHEMA, generation, logical_len, &mut file).unwrap();
        if committed {
            let sector = encode_commit_sector(&sealed, 0).unwrap();
            let at = sealed.padded_len;
            file[at..at + COMMIT_FOOTER_BYTES].copy_from_slice(&sector);
        }
        file
    }

    #[test]
    fn empty_is_absent() {
        assert_eq!(classify_record(&[], MAGIC), RecordState::Absent);
    }

    #[test]
    fn prepared_then_committed() {
        let prepared = build_record(7, 100, false);
        match classify_record(&prepared, MAGIC) {
            RecordState::Prepared(view) => {
                assert_eq!(view.generation, 7);
                assert_eq!(view.schema_version, SCHEMA);
                assert_eq!(
                    view.logical_body.len(),
                    BODY_PREFIX_BYTES + 100 + BODY_CRC_BYTES
                );
            }
            state => panic!("expected prepared, got {state:?}"),
        }
        let committed = build_record(7, 100, true);
        assert_eq!(
            classify_record(&committed, MAGIC).committed_generation(),
            Some(7)
        );
    }

    #[test]
    fn wrong_magic_is_corrupt() {
        let file = build_record(1, 8, true);
        assert_eq!(classify_record(&file, *b"OTHR"), RecordState::Corrupt);
    }

    #[test]
    fn zero_generation_is_rejected_at_seal() {
        let logical_len = MIN_LOGICAL_BODY_BYTES;
        let mut file = vec![0u8; record_file_len(logical_len).unwrap()];
        assert!(seal_body(MAGIC, SCHEMA, 0, logical_len, &mut file).is_none());
    }

    #[test]
    fn nonzero_padding_is_corrupt() {
        let mut file = build_record(3, 10, true);
        // Body is 34 bytes -> padding spans 34..512.
        file[100] = 1;
        assert_eq!(classify_record(&file, MAGIC), RecordState::Corrupt);
    }

    #[test]
    fn body_flip_is_corrupt() {
        let mut file = build_record(3, 10, true);
        file[BODY_PREFIX_BYTES] ^= 0xFF;
        assert_eq!(classify_record(&file, MAGIC), RecordState::Corrupt);
    }

    #[test]
    fn every_torn_commit_prefix_stays_prepared() {
        // Simulate a torn commit-sector write: for every split point, the
        // first k bytes are the new sector and the rest is still zero. No
        // split may classify as committed.
        let prepared = build_record(9, 40, false);
        let committed = build_record(9, 40, true);
        let sector_at = committed.len() - COMMIT_FOOTER_BYTES;
        for split in 0..COMMIT_FOOTER_BYTES {
            let mut torn = prepared.clone();
            torn[sector_at..sector_at + split]
                .copy_from_slice(&committed[sector_at..sector_at + split]);
            match classify_record(&torn, MAGIC) {
                RecordState::Prepared(_) => {}
                state => panic!("split {split}: expected prepared, got {state:?}"),
            }
        }
    }

    #[test]
    fn commit_sector_naming_wrong_body_is_corrupt() {
        // Committed record whose body is then replaced by a *valid* body of
        // a different generation: the sector no longer names the body.
        let committed = build_record(5, 40, true);
        let other = build_record(6, 40, false);
        let sector_at = committed.len() - COMMIT_FOOTER_BYTES;
        let mut crossed = other;
        crossed[sector_at..].copy_from_slice(&committed[sector_at..]);
        assert_eq!(classify_record(&crossed, MAGIC), RecordState::Corrupt);
    }

    #[test]
    fn unknown_commit_schema_is_not_committed() {
        let mut file = build_record(4, 16, true);
        let sector_at = file.len() - COMMIT_FOOTER_BYTES;
        // Bump the commit schema and re-CRC the sector so only the version
        // is unfamiliar.
        file[sector_at + 4] = 2;
        let crc_at = sector_at + CS_CRC.start;
        file[crc_at..crc_at + 4].fill(0);
        let crc = {
            let sector = &file[sector_at..];
            crc32_with_zeroed_field(sector, CS_CRC).unwrap()
        };
        file[crc_at..crc_at + 4].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            classify_record(&file, MAGIC),
            RecordState::Prepared(_)
        ));
    }

    #[test]
    fn selection_rules() {
        let old = build_record(3, 8, true);
        let new = build_record(4, 8, true);
        let prepared = build_record(9, 8, false);
        let a = classify_record(&old, MAGIC);
        let b = classify_record(&new, MAGIC);
        assert_eq!(
            select_slot(&a, &b),
            Selection::Selected {
                slot: 1,
                generation: 4
            }
        );
        // A higher prepared generation never outranks a lower committed one.
        let p = classify_record(&prepared, MAGIC);
        assert_eq!(
            select_slot(&p, &a),
            Selection::Selected {
                slot: 1,
                generation: 3
            }
        );
        assert_eq!(select_slot(&p, &p), Selection::None);
        // Equal committed generations are ambiguous, not first-wins.
        let dup = classify_record(&new, MAGIC);
        assert_eq!(select_slot(&b, &dup), Selection::Ambiguous);
    }

    #[test]
    fn truncated_file_is_corrupt() {
        let file = build_record(2, 700, true);
        for cut in [1, 511, 512, 513, file.len() - 1] {
            assert_eq!(
                classify_record(&file[..file.len() - cut], MAGIC),
                RecordState::Corrupt,
                "cut {cut}"
            );
        }
    }
}
