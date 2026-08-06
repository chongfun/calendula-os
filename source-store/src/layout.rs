//! The managed on-card namespace: where authoritative records and managed
//! EPUB slots live, and what they are called.
//!
//! Physical names stay internal to this module — the HTTP contract exposes
//! only logical-book tokens, and no other module builds a file name by
//! hand. Everything lives in one directory (`SRC`, under the existing
//! `XTEINK` root the firmware already owns), which is the PRD's
//! managed-slot provenance namespace: unmanaged discovery scans `/books`
//! and the card root for `.epub`/`.epu` files and never descends here, so
//! a candidate EPUB parked in a managed slot cannot be mistaken for a
//! direct-SD book no matter what state its metadata is in. The `.EPB`
//! extension is a second fence for the same invariant.
//!
//! Names are 8.3, uppercase, fixed shape:
//!
//! ```text
//! S<NN>.EPB            managed EPUB slot NN (00..=63)
//! M<NN>A.BIN / B.BIN   slot NN's source-metadata A/B record pair
//! T<NN>A.BIN / B.BIN   tombstone slot NN's A/B record pair (00..=15)
//! IDEMA.BIN / IDEMB.BIN  idempotency-state record pair
//! MARKA.BIN / MARKB.BIN  managed-upload staging-marker record pair
//! ```

use heapless::String;

use crate::bodies::{SOURCE_METADATA_MAGIC, STAGING_MARKER_MAGIC, TOMBSTONE_MAGIC};
use crate::publish::SlotPair;
use crate::receipts::IDEMPOTENCY_MAGIC;
use crate::select::MAX_SOURCE_SLOTS;

/// Directory holding every managed source slot and authoritative record,
/// created under the firmware's existing `XTEINK` root.
pub const SOURCE_DIR: &str = "SRC";

/// Tombstone slots. Eight books can sit deleted-but-uncleaned at once;
/// cleanup reclaims slots long before a ninth accumulates, and a full
/// table rejects further deletions with a stable retryable error rather
/// than dropping replay safety. Half of `MAX_SOURCE_SLOTS`, the same
/// 1:2 ratio the 32-slot layout carried — shrunk with the other
/// capacities when the owner image outgrew the X3's session heap
/// (2026-08-06); each tombstone also rides in the resident workspace.
pub const MAX_TOMBSTONE_SLOTS: usize = 8;

/// An owned A/B file-name pair, built once and borrowed as a
/// [`SlotPair`] for the publish layer. 12 bytes covers every 8.3 name.
pub struct PairNames {
    names: [String<12>; 2],
    magic: [u8; 4],
}

impl PairNames {
    pub fn pair(&self) -> SlotPair<'_> {
        SlotPair {
            names: [self.names[0].as_str(), self.names[1].as_str()],
            magic: self.magic,
        }
    }
}

/// Two decimal digits, zero-padded. Slots are bounded well below 100 by
/// [`MAX_SOURCE_SLOTS`] and [`MAX_TOMBSTONE_SLOTS`].
fn push_two_digits(out: &mut String<12>, value: u8) {
    let _ = out.push((b'0' + value / 10) as char);
    let _ = out.push((b'0' + value % 10) as char);
}

fn record_pair(prefix: char, slot: u8, magic: [u8; 4]) -> PairNames {
    let mut a = String::new();
    let _ = a.push(prefix);
    push_two_digits(&mut a, slot);
    let mut b = a.clone();
    let _ = a.push_str("A.BIN");
    let _ = b.push_str("B.BIN");
    PairNames {
        names: [a, b],
        magic,
    }
}

/// The managed EPUB file for a source slot, e.g. `S07.EPB`. `None` for an
/// out-of-range slot — no caller-provided index reaches the filesystem
/// unvalidated.
pub fn source_slot_name(slot: u8) -> Option<String<12>> {
    if usize::from(slot) >= MAX_SOURCE_SLOTS {
        return None;
    }
    let mut name = String::new();
    let _ = name.push('S');
    push_two_digits(&mut name, slot);
    let _ = name.push_str(".EPB");
    Some(name)
}

/// Source-metadata record pair for a source slot.
pub fn metadata_pair(slot: u8) -> Option<PairNames> {
    if usize::from(slot) >= MAX_SOURCE_SLOTS {
        return None;
    }
    Some(record_pair('M', slot, SOURCE_METADATA_MAGIC))
}

/// Tombstone record pair for a tombstone slot (unrelated to source slots).
pub fn tombstone_pair(slot: u8) -> Option<PairNames> {
    if usize::from(slot) >= MAX_TOMBSTONE_SLOTS {
        return None;
    }
    Some(record_pair('T', slot, TOMBSTONE_MAGIC))
}

/// A literal name known to fit the 12-byte capacity.
fn fixed_name(name: &str) -> String<12> {
    let mut out = String::new();
    let _ = out.push_str(name);
    out
}

/// The single idempotency-state record pair.
pub fn idempotency_pair() -> PairNames {
    PairNames {
        names: [fixed_name("IDEMA.BIN"), fixed_name("IDEMB.BIN")],
        magic: IDEMPOTENCY_MAGIC,
    }
}

/// The single staging-marker record pair.
pub fn marker_pair() -> PairNames {
    PairNames {
        names: [fixed_name("MARKA.BIN"), fixed_name("MARKB.BIN")],
        magic: STAGING_MARKER_MAGIC,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    #[test]
    fn names_are_expected_shape() {
        assert_eq!(source_slot_name(0).unwrap().as_str(), "S00.EPB");
        let last = MAX_SOURCE_SLOTS as u8 - 1;
        assert_eq!(
            source_slot_name(last).unwrap().as_str(),
            std::format!("S{last:02}.EPB")
        );
        assert!(source_slot_name(MAX_SOURCE_SLOTS as u8).is_none());
        let pair = metadata_pair(7).unwrap();
        assert_eq!(pair.pair().names, ["M07A.BIN", "M07B.BIN"]);
        let last_stone = MAX_TOMBSTONE_SLOTS as u8 - 1;
        let pair = tombstone_pair(last_stone).unwrap();
        assert_eq!(
            pair.pair().names,
            [
                std::format!("T{last_stone:02}A.BIN").as_str(),
                std::format!("T{last_stone:02}B.BIN").as_str()
            ]
        );
        assert!(tombstone_pair(MAX_TOMBSTONE_SLOTS as u8).is_none());
        assert_eq!(idempotency_pair().pair().names, ["IDEMA.BIN", "IDEMB.BIN"]);
        assert_eq!(marker_pair().pair().names, ["MARKA.BIN", "MARKB.BIN"]);
    }

    #[test]
    fn all_names_are_valid_8_3() {
        // Every generated name: at most 8 chars before one dot, at most 3
        // after, all uppercase-or-digit — what embedded-sdmmc accepts.
        let mut names: std::vec::Vec<std::string::String> = std::vec::Vec::new();
        for slot in 0..MAX_SOURCE_SLOTS as u8 {
            names.push(source_slot_name(slot).unwrap().as_str().into());
            let pair = metadata_pair(slot).unwrap();
            names.push(pair.pair().names[0].into());
            names.push(pair.pair().names[1].into());
        }
        for slot in 0..MAX_TOMBSTONE_SLOTS as u8 {
            let pair = tombstone_pair(slot).unwrap();
            names.push(pair.pair().names[0].into());
            names.push(pair.pair().names[1].into());
        }
        for name in names {
            let (stem, ext) = name.split_once('.').expect("one dot");
            assert!(!stem.is_empty() && stem.len() <= 8, "{name}");
            assert!(!ext.is_empty() && ext.len() <= 3, "{name}");
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '.'),
                "{name}"
            );
        }
    }
}
