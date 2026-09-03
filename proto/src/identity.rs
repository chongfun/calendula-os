//! Persistent identity for one library copy of a book, and the on-card
//! ledger format that makes it durable.
//!
//! Three facts about a book are easy to run together, and this crate keeps
//! them apart. Where its file is, which is a locator
//! ([`crate::library_path::LibraryPath`]). Which bytes it holds, which is a
//! [`crate::source::SourceDigest`]. And which user-visible copy it is, which
//! is a [`BookId`]: random, opaque, minted once when a physical copy is
//! adopted into the library, and kept across renames and moves. Two
//! byte-identical files are two ids with independent user state, and moving
//! a file changes its locator and nothing else.
//!
//! A `BookId` is deliberately derived from nothing. A path is where a file
//! was. A digest says which book, and a card can hold the same book twice.
//! A FAT cluster is recycled. So the id cannot be rebuilt from the card, and
//! the record binding it to a copy is durable user state rather than a
//! cache. That is the ledger: fixed-size records that each carry their own
//! checksum, under a header that is committed last, in two alternating files
//! so that an interrupted rewrite loses only the generation it was writing.
//! The I/O lives in `upload-store`; this module is the bytes.
//!
//! `CATALOG.BIN` caches the id beside each row so nothing on the reading path
//! has to read the ledger. The catalog is rebuildable and the ledger is not:
//! a rebuild reads the ledger once, joins it to the fresh rows by place, and
//! mints ids only for rows no record names.

use crate::catalog::{book_root, root_byte};
use crate::library_path::{BookRoot, MAX_PATH_BYTES};

/// Bytes in a [`BookId`].
pub const BOOK_ID_BYTES: usize = 16;

/// The identity of one library copy.
///
/// Ordered so that ids can be sorted and searched; the order means nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BookId([u8; BOOK_ID_BYTES]);

impl BookId {
    /// Mint a fresh id.
    ///
    /// `random` must be a hardware or otherwise unpredictable source: two
    /// copies handed the same id are one book to everything above this
    /// layer, and nothing here can tell. The all-zero id encodes "no id" on
    /// the card, so a draw that lands on it is drawn again.
    pub fn mint(random: &mut impl FnMut() -> u32) -> Self {
        loop {
            let mut bytes = [0u8; BOOK_ID_BYTES];
            let mut at = 0;
            while at < BOOK_ID_BYTES {
                bytes[at..at + 4].copy_from_slice(&random().to_le_bytes());
                at += 4;
            }
            if let Some(id) = Self::from_bytes(bytes) {
                return id;
            }
        }
    }

    /// An id read back off the card. `None` for the all-zero bytes, which is
    /// how a row or record says it has none.
    pub const fn from_bytes(bytes: [u8; BOOK_ID_BYTES]) -> Option<Self> {
        let mut at = 0;
        while at < BOOK_ID_BYTES {
            if bytes[at] != 0 {
                return Some(Self(bytes));
            }
            at += 1;
        }
        None
    }

    pub const fn to_bytes(self) -> [u8; BOOK_ID_BYTES] {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Ledger format
// ---------------------------------------------------------------------------

pub const LEDGER_MAGIC: [u8; 4] = *b"X4LG";
/// A ledger of a version this build does not read is refused, not rebuilt:
/// unlike the catalog, its contents cannot come back from the card. So a
/// change to this format is a migration, a reader for the old layout that
/// writes the new one, and a bump alone is not one.
///
/// Version 1 is the exception, and the only one there will be. It was the
/// layout of one pre-merge commit, without the `misses` byte, and no build
/// that wrote it ever hung user state from an id, so it was retired without
/// a reader. A card written by that build refuses here until its two ledger
/// files are removed, after which the next scan adopts every book afresh.
pub const LEDGER_VERSION: u8 = 2;
pub const LEDGER_HEADER_BYTES: usize = 16;
pub const LEDGER_RECORD_BYTES: usize = 284;

// Header: magic | version | reserved (zero) | count u16 | generation u32 |
// checksum u32 over everything before it.
const HEADER_VERSION: usize = 4;
const HEADER_RESERVED: usize = 5;
const HEADER_COUNT: usize = 6;
const HEADER_GENERATION: usize = 8;
const HEADER_CHECKSUM: usize = 12;
const _: () = assert!(HEADER_CHECKSUM + 4 == LEDGER_HEADER_BYTES);

// Record: id | root | misses | locator length u16 | locator, zero padded |
// byte size at adoption u32 | checksum u32 over everything before it.
const RECORD_ID: usize = 0;
const RECORD_ROOT: usize = BOOK_ID_BYTES;
const RECORD_MISSES: usize = RECORD_ROOT + 1;
const RECORD_LOCATOR_LEN: usize = RECORD_MISSES + 1;
const RECORD_LOCATOR: usize = RECORD_LOCATOR_LEN + 2;
const RECORD_SIZE: usize = RECORD_LOCATOR + MAX_PATH_BYTES;
const RECORD_CHECKSUM: usize = RECORD_SIZE + 4;
const _: () = assert!(RECORD_CHECKSUM + 4 == LEDGER_RECORD_BYTES);

/// One adopted copy: which id it carries, the place and size it had when
/// the record was written, and how long it has been missing from there.
///
/// The size is the cheap evidence a rebuild has that the file at a known
/// locator is still the file that was adopted there. It is a filter and
/// not proof: a same-sized replacement passes it, and only a source digest
/// can say otherwise. It is enough to keep a different book dropped at a
/// known path by a computer from inheriting the id of the one that was
/// there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LedgerRecord<'a> {
    pub id: BookId,
    pub root: BookRoot,
    /// Root-relative, exactly as the catalog stores it.
    pub locator: &'a str,
    pub byte_size: u32,
    /// Consecutive scans in which no row on the card was this place. Zero
    /// for a copy that is there. A record is carried while this stays
    /// within [`MISSING_SCANS_RETAINED`] and left behind once it does not,
    /// which is what keeps the ledger near the size of the live library
    /// rather than the size of every book that ever passed through it.
    pub misses: u8,
}

/// How many consecutive scans a copy may be missing before its record is
/// left out of the next generation.
///
/// A missing record is what lets a book that was taken off the card and put
/// back, or moved while the card was in a computer, be recognised as the
/// copy it was rather than adopted afresh; the reconciliation that does the
/// recognising is later work, and this keeps its evidence for it. The bound
/// is what the identity design asks for and leaves to the implementation:
/// a scan runs once per change to the card, so this is eight card edits
/// during which the book stayed away. Missing records are also the first to
/// go when a generation would not fit; see [`carry_missing`].
pub const MISSING_SCANS_RETAINED: u8 = 8;

/// Whether a record that named no row this scan is carried into the next
/// generation, and with what `misses`.
///
/// `room` is how many more records the generation can hold once every live
/// copy and every newly adopted one is in it. Live and new copies are what
/// the library is, so they are counted first and a missing record only
/// takes a slot that is left. Without that, a card that once held the most
/// books a header can count and then lost them all would carry every stale
/// record forever, and the first book added afterwards could not be
/// adopted at all.
pub fn carry_missing(misses: u8, room: usize) -> Option<u8> {
    let aged = misses.saturating_add(1);
    (aged <= MISSING_SCANS_RETAINED && room > 0).then_some(aged)
}

/// Encode one record. `None` when the locator does not fit, which a legal
/// [`crate::library_path::LibraryPath`] cannot reach.
pub fn encode_ledger_record(
    record: &LedgerRecord<'_>,
    out: &mut [u8; LEDGER_RECORD_BYTES],
) -> Option<()> {
    let locator = record.locator.as_bytes();
    if locator.len() > MAX_PATH_BYTES {
        return None;
    }
    out.fill(0);
    out[RECORD_ID..RECORD_ROOT].copy_from_slice(&record.id.to_bytes());
    out[RECORD_ROOT] = root_byte(record.root);
    out[RECORD_MISSES] = record.misses;
    out[RECORD_LOCATOR_LEN..RECORD_LOCATOR].copy_from_slice(&(locator.len() as u16).to_le_bytes());
    out[RECORD_LOCATOR..RECORD_LOCATOR + locator.len()].copy_from_slice(locator);
    out[RECORD_SIZE..RECORD_CHECKSUM].copy_from_slice(&record.byte_size.to_le_bytes());
    let sum = fnv1a(&out[..RECORD_CHECKSUM]);
    out[RECORD_CHECKSUM..].copy_from_slice(&sum.to_le_bytes());
    Some(())
}

/// Decode one record, its locator borrowed from the stored bytes. `None` for
/// anything that is not a record this build wrote: a failed checksum, an id
/// of zero, a root byte it does not know, a locator length past the field,
/// or locator bytes that are not UTF-8.
pub fn decode_ledger_record(bytes: &[u8; LEDGER_RECORD_BYTES]) -> Option<LedgerRecord<'_>> {
    let stored = u32::from_le_bytes(bytes[RECORD_CHECKSUM..].try_into().ok()?);
    if fnv1a(&bytes[..RECORD_CHECKSUM]) != stored {
        return None;
    }
    let id = BookId::from_bytes(bytes[RECORD_ID..RECORD_ROOT].try_into().ok()?)?;
    let root = book_root(bytes[RECORD_ROOT])?;
    let len =
        u16::from_le_bytes([bytes[RECORD_LOCATOR_LEN], bytes[RECORD_LOCATOR_LEN + 1]]) as usize;
    if len > MAX_PATH_BYTES {
        return None;
    }
    let locator = core::str::from_utf8(&bytes[RECORD_LOCATOR..RECORD_LOCATOR + len]).ok()?;
    let byte_size = u32::from_le_bytes(bytes[RECORD_SIZE..RECORD_CHECKSUM].try_into().ok()?);
    Some(LedgerRecord {
        id,
        root,
        locator,
        byte_size,
        misses: bytes[RECORD_MISSES],
    })
}

/// What a committed generation says about itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LedgerHeader {
    /// Compared with [`crate::durable::generation_is_newer`], so the counter
    /// survives wrapping.
    pub generation: u32,
    pub count: u16,
}

pub fn encode_ledger_header(header: LedgerHeader, out: &mut [u8; LEDGER_HEADER_BYTES]) {
    out.fill(0);
    out[..4].copy_from_slice(&LEDGER_MAGIC);
    out[HEADER_VERSION] = LEDGER_VERSION;
    out[HEADER_COUNT..HEADER_GENERATION].copy_from_slice(&header.count.to_le_bytes());
    out[HEADER_GENERATION..HEADER_CHECKSUM].copy_from_slice(&header.generation.to_le_bytes());
    let sum = fnv1a(&out[..HEADER_CHECKSUM]);
    out[HEADER_CHECKSUM..].copy_from_slice(&sum.to_le_bytes());
}

/// The header a writer puts down first and replaces last: all zero, so that a
/// generation whose write was interrupted reads as one that was never
/// committed rather than as a shorter library.
///
/// It is the only header that means that. A header block is written whole,
/// so a torn commit leaves this placeholder and a landed commit leaves a
/// header that decodes; bytes that are neither are a header that landed and
/// was damaged since.
pub fn encode_ledger_placeholder_header(out: &mut [u8; LEDGER_HEADER_BYTES]) {
    out.fill(0);
}

/// What the sixteen bytes at the start of a ledger file say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerHeaderReading {
    /// The all-zero placeholder: a generation that was never committed.
    Placeholder,
    /// A committed generation this build reads.
    Committed(LedgerHeader),
    /// The ledger's magic under a version this build does not read: another
    /// build's generation, whose ids cannot be rebuilt from the card.
    UnknownVersion(u8),
    /// None of the above: a committed header damaged after it landed, or a
    /// file that was never a ledger. Either way not a reason to trust the
    /// other side instead.
    Damaged,
}

/// Read a header without guessing. A version this build does not read is
/// reported before the checksum is consulted, since another version may
/// frame its header differently.
pub fn classify_ledger_header(bytes: &[u8; LEDGER_HEADER_BYTES]) -> LedgerHeaderReading {
    if bytes.iter().all(|byte| *byte == 0) {
        return LedgerHeaderReading::Placeholder;
    }
    if bytes[..4] != LEDGER_MAGIC {
        return LedgerHeaderReading::Damaged;
    }
    if bytes[HEADER_VERSION] != LEDGER_VERSION {
        return LedgerHeaderReading::UnknownVersion(bytes[HEADER_VERSION]);
    }
    let stored = u32::from_le_bytes([
        bytes[HEADER_CHECKSUM],
        bytes[HEADER_CHECKSUM + 1],
        bytes[HEADER_CHECKSUM + 2],
        bytes[HEADER_CHECKSUM + 3],
    ]);
    if bytes[HEADER_RESERVED] != 0 || fnv1a(&bytes[..HEADER_CHECKSUM]) != stored {
        return LedgerHeaderReading::Damaged;
    }
    LedgerHeaderReading::Committed(LedgerHeader {
        generation: u32::from_le_bytes([
            bytes[HEADER_GENERATION],
            bytes[HEADER_GENERATION + 1],
            bytes[HEADER_GENERATION + 2],
            bytes[HEADER_GENERATION + 3],
        ]),
        count: u16::from_le_bytes([bytes[HEADER_COUNT], bytes[HEADER_COUNT + 1]]),
    })
}

/// The exact length of a committed generation holding `count` records.
pub fn ledger_file_len(count: u16) -> usize {
    LEDGER_HEADER_BYTES + count as usize * LEDGER_RECORD_BYTES
}

fn fnv1a(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5u32, |hash, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
    })
}

// ---------------------------------------------------------------------------
// Row keys: the in-RAM side of the join between a fresh catalog and the ledger
// ---------------------------------------------------------------------------

/// Bytes one staged `(source_hash, row)` key occupies in the caller's
/// scratch: 2,730 rows in the 16 KB arena.
pub const ROW_KEY_BYTES: usize = 6;

/// Stage the key of catalog row `row`, whose place hashes to `hash`, at
/// `index` in `scratch`. Returns false (staging nothing) past capacity.
///
/// The join reads the ledger once per slice of rows and needs, per record,
/// the rows whose place could be the record's. The 32-bit place hash is a
/// lossy projection and two locators can share one, so what comes back
/// from [`rows_with_hash`] is candidates to compare in full, not answers.
pub fn stage_row_key(scratch: &mut [u8], index: usize, hash: u32, row: u16) -> bool {
    let at = index * ROW_KEY_BYTES;
    let Some(slot) = scratch.get_mut(at..at + ROW_KEY_BYTES) else {
        return false;
    };
    slot[..4].copy_from_slice(&hash.to_le_bytes());
    slot[4..].copy_from_slice(&row.to_le_bytes());
    true
}

/// Order the first `count` staged keys by hash, then row, so
/// [`rows_with_hash`] can binary-search them. Heapsort: in place, no
/// allocation, no recursion.
pub fn sort_row_keys(scratch: &mut [u8], count: usize) {
    let count = count.min(scratch.len() / ROW_KEY_BYTES);
    for parent in (0..count / 2).rev() {
        sift_down_row_key(scratch, parent, count);
    }
    for end in (1..count).rev() {
        swap_row_keys(scratch, 0, end);
        sift_down_row_key(scratch, 0, end);
    }
}

/// Every staged row whose place hashes to `hash`, in row order. The keys
/// must have been through [`sort_row_keys`].
pub fn rows_with_hash(scratch: &[u8], count: usize, hash: u32) -> RowsWithHash<'_> {
    let count = count.min(scratch.len() / ROW_KEY_BYTES);
    // Lower bound of the hash: the first key at or above `(hash, row 0)`.
    let wanted = row_key(hash, 0);
    let (mut lo, mut hi) = (0usize, count);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if staged_row_key(scratch, mid) < wanted {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    RowsWithHash {
        scratch,
        count,
        hash,
        next: lo,
    }
}

/// See [`rows_with_hash`].
pub struct RowsWithHash<'a> {
    scratch: &'a [u8],
    count: usize,
    hash: u32,
    next: usize,
}

impl Iterator for RowsWithHash<'_> {
    type Item = u16;

    fn next(&mut self) -> Option<u16> {
        if self.next >= self.count {
            return None;
        }
        let key = staged_row_key(self.scratch, self.next);
        if (key >> 16) as u32 != self.hash {
            return None;
        }
        self.next += 1;
        Some(key as u16)
    }
}

/// The sort key: hash in the high bits so equal hashes are adjacent, row
/// below so the walk out of [`rows_with_hash`] comes back in row order.
fn row_key(hash: u32, row: u16) -> u64 {
    (u64::from(hash) << 16) | u64::from(row)
}

fn staged_row_key(scratch: &[u8], index: usize) -> u64 {
    let at = index * ROW_KEY_BYTES;
    let hash = u32::from_le_bytes([
        scratch[at],
        scratch[at + 1],
        scratch[at + 2],
        scratch[at + 3],
    ]);
    let row = u16::from_le_bytes([scratch[at + 4], scratch[at + 5]]);
    row_key(hash, row)
}

fn swap_row_keys(scratch: &mut [u8], a: usize, b: usize) {
    for offset in 0..ROW_KEY_BYTES {
        scratch.swap(a * ROW_KEY_BYTES + offset, b * ROW_KEY_BYTES + offset);
    }
}

fn sift_down_row_key(scratch: &mut [u8], mut parent: usize, end: usize) {
    loop {
        let mut child = parent * 2 + 1;
        if child >= end {
            return;
        }
        if child + 1 < end && staged_row_key(scratch, child) < staged_row_key(scratch, child + 1) {
            child += 1;
        }
        if staged_row_key(scratch, parent) >= staged_row_key(scratch, child) {
            return;
        }
        swap_row_keys(scratch, parent, child);
        parent = child;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic "random" source for tests: hands out the words it
    /// was given, in order.
    fn words(seq: &'static [u32]) -> impl FnMut() -> u32 {
        let mut at = 0;
        move || {
            let word = seq[at];
            at += 1;
            word
        }
    }

    #[test]
    fn minting_redraws_the_zero_id_rather_than_handing_it_out() {
        let mut random = words(&[0, 0, 0, 0, 1, 2, 3, 4]);
        let id = BookId::mint(&mut random);
        let mut expected = [0u8; BOOK_ID_BYTES];
        expected[0] = 1;
        expected[4] = 2;
        expected[8] = 3;
        expected[12] = 4;
        assert_eq!(id.to_bytes(), expected);
        assert_eq!(BookId::from_bytes([0u8; BOOK_ID_BYTES]), None);
        assert_eq!(BookId::from_bytes(expected), Some(id));
    }

    #[test]
    fn two_mints_are_two_ids() {
        let mut random = words(&[9, 9, 9, 9, 9, 9, 9, 8]);
        let a = BookId::mint(&mut random);
        let b = BookId::mint(&mut random);
        assert_ne!(a, b);
    }

    fn sample() -> LedgerRecord<'static> {
        LedgerRecord {
            id: BookId::from_bytes([7u8; BOOK_ID_BYTES]).unwrap(),
            root: BookRoot::Library,
            locator: "History/Rome/SPQR.epub",
            byte_size: 1_234_567,
            misses: 0,
        }
    }

    #[test]
    fn a_record_round_trips() {
        let record = sample();
        let mut bytes = [0u8; LEDGER_RECORD_BYTES];
        encode_ledger_record(&record, &mut bytes).unwrap();
        assert_eq!(decode_ledger_record(&bytes), Some(record));

        let loose = LedgerRecord {
            root: BookRoot::CardRoot,
            locator: "Dune.epub",
            misses: 3,
            ..record
        };
        encode_ledger_record(&loose, &mut bytes).unwrap();
        assert_eq!(decode_ledger_record(&bytes), Some(loose));
    }

    #[test]
    fn a_missing_record_ages_until_the_bound_and_yields_to_a_full_generation() {
        assert_eq!(carry_missing(0, 1), Some(1));
        assert_eq!(
            carry_missing(MISSING_SCANS_RETAINED - 1, 1),
            Some(MISSING_SCANS_RETAINED)
        );
        assert_eq!(carry_missing(MISSING_SCANS_RETAINED, 1), None);
        assert_eq!(carry_missing(u8::MAX, usize::MAX), None);
        assert_eq!(carry_missing(0, 0), None, "live and new copies come first");
    }

    #[test]
    fn a_torn_or_foreign_record_decodes_as_nothing() {
        let mut bytes = [0u8; LEDGER_RECORD_BYTES];
        encode_ledger_record(&sample(), &mut bytes).unwrap();
        for at in [
            0,
            RECORD_ROOT,
            RECORD_MISSES,
            RECORD_LOCATOR_LEN,
            RECORD_LOCATOR + 3,
            RECORD_SIZE,
            RECORD_CHECKSUM,
        ] {
            let mut torn = bytes;
            torn[at] ^= 0x40;
            assert_eq!(decode_ledger_record(&torn), None, "flipped byte {at}");
        }
        assert_eq!(decode_ledger_record(&[0u8; LEDGER_RECORD_BYTES]), None);
        // A checksum that happens to agree does not make an unknown root or
        // a zero id readable.
        let mut alien = bytes;
        alien[RECORD_ROOT] = 9;
        let sum = fnv1a(&alien[..RECORD_CHECKSUM]);
        alien[RECORD_CHECKSUM..].copy_from_slice(&sum.to_le_bytes());
        assert_eq!(decode_ledger_record(&alien), None);
        let mut anonymous = bytes;
        anonymous[RECORD_ID..RECORD_ROOT].fill(0);
        let sum = fnv1a(&anonymous[..RECORD_CHECKSUM]);
        anonymous[RECORD_CHECKSUM..].copy_from_slice(&sum.to_le_bytes());
        assert_eq!(decode_ledger_record(&anonymous), None);
    }

    #[test]
    fn a_locator_past_the_field_is_refused_rather_than_cut() {
        let long = core::str::from_utf8(&[b'a'; MAX_PATH_BYTES + 1]).unwrap();
        let record = LedgerRecord {
            locator: long,
            ..sample()
        };
        let mut bytes = [0u8; LEDGER_RECORD_BYTES];
        assert_eq!(encode_ledger_record(&record, &mut bytes), None);
        let full = core::str::from_utf8(&[b'a'; MAX_PATH_BYTES]).unwrap();
        let record = LedgerRecord {
            locator: full,
            ..sample()
        };
        encode_ledger_record(&record, &mut bytes).unwrap();
        assert_eq!(decode_ledger_record(&bytes).unwrap().locator, full);
    }

    #[test]
    fn a_header_reads_as_committed_placeholder_unknown_or_damaged() {
        let header = LedgerHeader {
            generation: 0xFFFF_FFFE,
            count: 1129,
        };
        let mut bytes = [0u8; LEDGER_HEADER_BYTES];
        encode_ledger_header(header, &mut bytes);
        assert_eq!(
            classify_ledger_header(&bytes),
            LedgerHeaderReading::Committed(header)
        );
        let mut placeholder = [0xAAu8; LEDGER_HEADER_BYTES];
        encode_ledger_placeholder_header(&mut placeholder);
        assert_eq!(
            classify_ledger_header(&placeholder),
            LedgerHeaderReading::Placeholder
        );
        // Only the exact placeholder means "never committed". Every other
        // byte flip is damage to a header that had landed, except the
        // version, which is another build's ledger.
        for at in 0..LEDGER_HEADER_BYTES {
            let mut torn = bytes;
            torn[at] ^= 0x01;
            let expected = if at == HEADER_VERSION {
                LedgerHeaderReading::UnknownVersion(LEDGER_VERSION ^ 0x01)
            } else {
                LedgerHeaderReading::Damaged
            };
            assert_eq!(classify_ledger_header(&torn), expected, "flipped byte {at}");
        }
        let mut nearly_blank = placeholder;
        nearly_blank[LEDGER_HEADER_BYTES - 1] = 1;
        assert_eq!(
            classify_ledger_header(&nearly_blank),
            LedgerHeaderReading::Damaged
        );
        // The retired pre-merge layout is a version this build does not
        // read, whatever its checksum says.
        let mut retired = bytes;
        retired[HEADER_VERSION] = 1;
        let sum = fnv1a(&retired[..HEADER_CHECKSUM]);
        retired[HEADER_CHECKSUM..].copy_from_slice(&sum.to_le_bytes());
        assert_eq!(
            classify_ledger_header(&retired),
            LedgerHeaderReading::UnknownVersion(1)
        );
        assert_eq!(
            ledger_file_len(header.count),
            LEDGER_HEADER_BYTES + 1129 * LEDGER_RECORD_BYTES
        );
    }

    #[test]
    fn row_keys_sort_and_answer_by_hash_with_duplicates_in_row_order() {
        let mut scratch = [0u8; 6 * ROW_KEY_BYTES];
        let staged = [(0x30u32, 5u16), (0x10, 9), (0x30, 2), (0x20, 0), (0x30, 7)];
        for (index, (hash, row)) in staged.iter().enumerate() {
            assert!(stage_row_key(&mut scratch, index, *hash, *row));
        }
        assert!(!stage_row_key(&mut scratch, 6, 1, 1), "past capacity");
        sort_row_keys(&mut scratch, staged.len());
        let rows: heapless::Vec<u16, 8> = rows_with_hash(&scratch, staged.len(), 0x30).collect();
        assert_eq!(rows.as_slice(), &[2, 5, 7]);
        let rows: heapless::Vec<u16, 8> = rows_with_hash(&scratch, staged.len(), 0x10).collect();
        assert_eq!(rows.as_slice(), &[9]);
        assert_eq!(rows_with_hash(&scratch, staged.len(), 0x25).count(), 0);
        assert_eq!(rows_with_hash(&scratch, staged.len(), 0x40).count(), 0);
        assert_eq!(rows_with_hash(&scratch, staged.len(), 0).count(), 0);
        // A count past what the scratch holds is clamped, not read past.
        assert_eq!(rows_with_hash(&scratch, 99, 0x20).count(), 1);
    }

    #[test]
    fn the_extreme_hash_values_sort_and_search() {
        let mut scratch = [0u8; 3 * ROW_KEY_BYTES];
        stage_row_key(&mut scratch, 0, u32::MAX, 1);
        stage_row_key(&mut scratch, 1, 0, 2);
        stage_row_key(&mut scratch, 2, u32::MAX, 0);
        sort_row_keys(&mut scratch, 3);
        let rows: heapless::Vec<u16, 4> = rows_with_hash(&scratch, 3, u32::MAX).collect();
        assert_eq!(rows.as_slice(), &[0, 1]);
        let rows: heapless::Vec<u16, 4> = rows_with_hash(&scratch, 3, 0).collect();
        assert_eq!(rows.as_slice(), &[2]);
    }
}
