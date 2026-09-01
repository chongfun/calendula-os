//! On-card CATALOG.BIN format: the header and fixed-size book records the
//! firmware's library scan writes and every list/lookup path reads back.
//! Lives here (not in firmware) so the encode/decode round-trip, the title
//! field layout, and the orphan-sweep identity staging are host-testable.

use crate::library_path::BookRoot;
use heapless::String;

pub const CATALOG_MAGIC: &[u8; 4] = b"X4CT";
/// v3 widened the on-disk book count from a single byte to a `u16` at
/// `header[5..7]`. v4 rebuilt stale records written before long filenames
/// were safely bounded. v5 appends a 64-byte title field to every record so
/// the Library list reads labels straight from the catalog instead of
/// probing each book's cache per window crossing; the version check makes
/// an older catalog fail to load, and a fresh scan rebuilds it -- no
/// migration code needed.
///
/// v6 changes no bytes at all: the record layout is identical to v5, and the
/// bump exists to retire the *contents* of every v5 catalog. Scans before it
/// catalogued macOS AppleDouble sidecars (`._<book>.epub`) as books, so a
/// card written by one holds a phantom, unopenable duplicate of every book
/// copied there in Finder. The scan filter alone would not reach those
/// cards: boot loads `CATALOG.BIN` and only queues a scan when the load
/// fails, so a v5 snapshot would keep serving its sidecar records until the
/// user forced a rescan. Bumping the version is how this format retires a
/// snapshot; see `is_hidden_entry` in `proto::storage` for the rule the
/// rebuild applies.
///
/// v7 replaces the 8.3 alias and the "which of the two directories" flag
/// with a root and a locator, because a book can now live at any depth
/// under the library root and neither field can say where. The root stays a
/// field rather than a prefix on the locator: a locator is library-root
/// relative by contract, so spelling the library directory into it would
/// give one type two coordinate systems. Records grow by the locator's
/// width, so a scan stages fewer of them per walk; the walk count is what
/// pays for nesting until a derived index earns its place.
///
/// v8 changes no bytes. It retires v7 snapshots whose `source_hash` was
/// derived from the 64-byte display path rather than from the root plus the
/// full locator. A nested locator can spend the whole display budget before
/// the filename, so two books could share a truncated label, and with sizes
/// equal they shared an identity and a cache. Loading a v7 snapshot under
/// the new derivation would be worse than a stale label: rebuilt caches
/// carry new hashes the old records cannot vouch for, and the orphan sweep
/// would reclaim them on the next scan. Rejecting v7 forces the rescan that
/// rewrites records and caches under one derivation.
pub const CATALOG_VERSION: u8 = 8;
pub const CATALOG_HEADER_BYTES: usize = 8;
pub const CATALOG_RECORD_BYTES: usize = 419;
/// Byte range of the title field inside a record, exposed so the firmware
/// can rewrite just the title in place when a book open learns the real
/// EPUB title.
pub const CATALOG_RECORD_TITLE_OFFSET: usize = 76;
pub const CATALOG_TITLE_BYTES: usize = 64;
/// Byte range of the locator, which is `LibraryPath` text.
const CATALOG_RECORD_PATH_OFFSET: usize = 140;
const CATALOG_PATH_BYTES: usize = crate::library_path::MAX_PATH_BYTES;
/// Byte range of the 8.3 alias an uploaded book landed under.
const CATALOG_RECORD_ALIAS_OFFSET: usize = 396;
const CATALOG_ALIAS_BYTES: usize = crate::storage::MAX_ALIAS_UTF8_BYTES;

/// One catalog record decoded into owned fields, so it outlives the file
/// handle it was read through.
pub struct CatalogRecord {
    /// The label a row falls back to when no title is known. Presentation
    /// only: identity and the cache key derive from `root`, `path`, and
    /// `byte_size` (see `proto::cache::source_hash_at`), because this field
    /// truncates at 64 bytes and a truncated label can collide where the
    /// locator does not.
    pub display_name: String<64>,
    /// Which directory `path` is relative to. `None` is a byte this build
    /// does not recognize, which makes the record unopenable rather than
    /// resolvable against the wrong root, where it could name a real but
    /// different file.
    pub root: Option<BookRoot>,
    /// Where the book is, as `LibraryPath` text. A locator, not identity:
    /// moving the file on a computer changes this and nothing else.
    pub path: String<{ crate::library_path::MAX_PATH_BYTES }>,
    /// The EPUB title learned when the book was last opened (or the upload
    /// label stashed at upload). Empty when unknown; readers fall back to a
    /// label derived from the file stem.
    pub title: String<64>,
    /// The 8.3 alias the book landed under, which is what names its upload
    /// label sidecar. Not how the book is opened: that is `path`. Wide enough
    /// for an alias of accented characters, which takes two bytes each.
    pub upload_alias: String<{ crate::storage::MAX_ALIAS_UTF8_BYTES }>,
    pub byte_size: u32,
    pub source_hash: u32,
}

/// The library root is 0, so the common book costs a zero byte and a record
/// whose root byte was lost reads as unopenable rather than as a card-root
/// book that is not there.
const ROOT_LIBRARY: u8 = 0;
const ROOT_CARD: u8 = 1;

const fn root_byte(root: BookRoot) -> u8 {
    match root {
        BookRoot::Library => ROOT_LIBRARY,
        BookRoot::CardRoot => ROOT_CARD,
    }
}

const fn book_root(byte: u8) -> Option<BookRoot> {
    match byte {
        ROOT_LIBRARY => Some(BookRoot::Library),
        ROOT_CARD => Some(BookRoot::CardRoot),
        _ => None,
    }
}

pub fn encode_catalog_header(count: u16, out: &mut [u8; CATALOG_HEADER_BYTES]) {
    out.fill(0);
    out[..4].copy_from_slice(CATALOG_MAGIC);
    out[4] = CATALOG_VERSION;
    out[5..7].copy_from_slice(&count.to_le_bytes());
}

/// A deliberately invalid header, written first and left in place while
/// records are still landing: version 0 is never accepted by readers, and
/// for a non-empty partial file a count of 0 cannot agree with the file
/// length either. The writer commits the real header by seeking back only
/// after every record is durable, so a scan interrupted mid-write leaves a
/// catalog that fails to load (and triggers a fresh scan) instead of one
/// that quietly truncates the library — or worse, drives the orphan sweep
/// into reclaiming caches of books that are still on the card.
pub fn encode_catalog_placeholder_header(out: &mut [u8; CATALOG_HEADER_BYTES]) {
    out.fill(0);
    out[..4].copy_from_slice(CATALOG_MAGIC);
}

/// The exact byte length of a catalog holding `count` records; readers
/// reject files whose length disagrees with their committed header.
pub fn catalog_file_len(count: u16) -> usize {
    CATALOG_HEADER_BYTES + count as usize * CATALOG_RECORD_BYTES
}

/// Why a header did not decode, for callers that report rather than just
/// rebuild. Both lead to the same fresh scan; they differ only in whether
/// anything went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogHeaderFault {
    /// Right magic, a version this build does not read, and not the
    /// placeholder: a catalog written by other firmware. Bumping
    /// `CATALOG_VERSION` *is* how this format migrates — the old snapshot
    /// stops loading and a scan rebuilds it — so this is the designed first
    /// boot after an upgrade, not damage.
    Stale,
    /// Wrong magic, or the version-0 placeholder a scan leaves in place while
    /// its records are still landing. Either means this is not a catalog the
    /// reader may trust, and the second means a scan was interrupted.
    Invalid,
}

/// The book count, or why the header was rejected. Callers that only need to
/// know whether to rescan can use [`decode_catalog_header`].
pub fn classify_catalog_header(
    header: &[u8; CATALOG_HEADER_BYTES],
) -> Result<u16, CatalogHeaderFault> {
    if &header[..4] != CATALOG_MAGIC {
        return Err(CatalogHeaderFault::Invalid);
    }
    let version = header[4];
    if version == CATALOG_VERSION {
        return Ok(u16::from_le_bytes([header[5], header[6]]));
    }
    // Version 0 is the placeholder, so it is a torn write rather than an
    // older format, however much the two look alike from here.
    Err(if version == 0 {
        CatalogHeaderFault::Invalid
    } else {
        CatalogHeaderFault::Stale
    })
}

/// The book count, or `None` when the magic or version doesn't match (the
/// caller then runs a fresh scan).
pub fn decode_catalog_header(header: &[u8; CATALOG_HEADER_BYTES]) -> Option<u16> {
    classify_catalog_header(header).ok()
}

#[allow(clippy::too_many_arguments)]
pub fn encode_catalog_record(
    out: &mut [u8; CATALOG_RECORD_BYTES],
    display_name: &str,
    root: BookRoot,
    path: &str,
    title: &str,
    upload_alias: &str,
    byte_size: u32,
    source_hash: u32,
) {
    out.fill(0);
    out[0] = root_byte(root);
    out[4..8].copy_from_slice(&byte_size.to_le_bytes());
    out[8..12].copy_from_slice(&source_hash.to_le_bytes());
    copy_fixed(display_name.as_bytes(), &mut out[12..76]);
    copy_fixed(
        title.as_bytes(),
        &mut out[CATALOG_RECORD_TITLE_OFFSET..CATALOG_RECORD_TITLE_OFFSET + CATALOG_TITLE_BYTES],
    );
    copy_fixed(
        path.as_bytes(),
        &mut out[CATALOG_RECORD_PATH_OFFSET..CATALOG_RECORD_PATH_OFFSET + CATALOG_PATH_BYTES],
    );
    copy_fixed(
        upload_alias.as_bytes(),
        &mut out[CATALOG_RECORD_ALIAS_OFFSET..CATALOG_RECORD_ALIAS_OFFSET + CATALOG_ALIAS_BYTES],
    );
}

pub fn decode_catalog_record(record: &[u8; CATALOG_RECORD_BYTES]) -> CatalogRecord {
    let mut display_name = String::<64>::new();
    let _ = display_name.push_str(fixed_str(&record[12..76]));
    let mut title = String::<64>::new();
    let _ = title.push_str(fixed_str(
        &record[CATALOG_RECORD_TITLE_OFFSET..CATALOG_RECORD_TITLE_OFFSET + CATALOG_TITLE_BYTES],
    ));
    let root = book_root(record[0]);
    let mut path = String::new();
    let _ = path.push_str(fixed_str(
        &record[CATALOG_RECORD_PATH_OFFSET..CATALOG_RECORD_PATH_OFFSET + CATALOG_PATH_BYTES],
    ));
    let mut upload_alias = String::new();
    let _ = upload_alias.push_str(fixed_str(
        &record[CATALOG_RECORD_ALIAS_OFFSET..CATALOG_RECORD_ALIAS_OFFSET + CATALOG_ALIAS_BYTES],
    ));
    CatalogRecord {
        display_name,
        root,
        path,
        title,
        upload_alias,
        byte_size: u32::from_le_bytes([record[4], record[5], record[6], record[7]]),
        source_hash: u32::from_le_bytes([record[8], record[9], record[10], record[11]]),
    }
}

/// The `(source_hash, byte_size)` identity of an encoded record.
pub fn catalog_record_identity(record: &[u8; CATALOG_RECORD_BYTES]) -> (u32, u32) {
    (
        u32::from_le_bytes([record[8], record[9], record[10], record[11]]),
        u32::from_le_bytes([record[4], record[5], record[6], record[7]]),
    )
}

/// The `(source_hash, byte_size)` identity pre-v8 firmware would have given
/// this record's book, when its display shape is one that firmware could
/// address; `None` for nested books, which have no pre-v8 identity to
/// answer to. Derived from the stored display name through the frozen rule
/// in [`crate::cache::legacy_source_hash`].
pub fn catalog_record_legacy_identity(record: &[u8; CATALOG_RECORD_BYTES]) -> Option<(u32, u32)> {
    let byte_size = u32::from_le_bytes([record[4], record[5], record[6], record[7]]);
    let display_name = fixed_str(&record[12..76]);
    Some((
        crate::cache::legacy_source_hash(display_name, byte_size)?,
        byte_size,
    ))
}

/// Resolves a current identity against records, offered one at a time in
/// catalog order, with the same one-match discipline as
/// [`LegacyIdentityScan`]: exactly one record may answer.
///
/// The identity is a 32-bit hash of the root, locator, and size, and two
/// legal locators can share it; a verified pair exists in the tests below.
/// Accepting the first or the hinted match would open one book as the
/// other, and the next save would persist that choice. Zero matches is a
/// book no longer on the card; two or more is a question the identity
/// cannot answer, so resolution refuses rather than guesses.
pub struct IdentityScan {
    wanted: (u32, u32),
    found: Option<u16>,
    matches: usize,
}

impl IdentityScan {
    pub fn new(source_hash: u32, byte_size: u32) -> Self {
        Self {
            wanted: (source_hash, byte_size),
            found: None,
            matches: 0,
        }
    }

    pub fn offer(&mut self, index: u16, record: &[u8; CATALOG_RECORD_BYTES]) {
        if catalog_record_identity(record) == self.wanted {
            self.matches += 1;
            if self.found.is_none() {
                self.found = Some(index);
            }
        }
    }

    pub fn finish(self) -> Option<u16> {
        if self.matches == 1 {
            self.found
        } else {
            None
        }
    }
}

/// Resolves a pre-v8 saved identity against v8 records, offered one at a
/// time in catalog order.
///
/// The global state record written by pre-v8 firmware still decodes after
/// the upgrade, carrying the identity that firmware derived, while the
/// rebuilt catalog holds only v8 identities. Restoring the active book
/// therefore needs this second reading. Exactly one record may answer:
/// zero is a book no longer on the card, and two or more is a guess
/// between real books, which restoration refuses rather than opens.
pub struct LegacyIdentityScan {
    wanted: (u32, u32),
    found: Option<u16>,
    matches: usize,
}

impl LegacyIdentityScan {
    pub fn new(source_hash: u32, byte_size: u32) -> Self {
        Self {
            wanted: (source_hash, byte_size),
            found: None,
            matches: 0,
        }
    }

    pub fn offer(&mut self, index: u16, record: &[u8; CATALOG_RECORD_BYTES]) {
        if catalog_record_legacy_identity(record) == Some(self.wanted) {
            self.matches += 1;
            if self.found.is_none() {
                self.found = Some(index);
            }
        }
    }

    pub fn finish(self) -> Option<u16> {
        if self.matches == 1 {
            self.found
        } else {
            None
        }
    }
}

/// Encode `title` into a standalone 64-byte title field, for rewriting the
/// field in place at `CATALOG_RECORD_TITLE_OFFSET` within a record.
pub fn encode_catalog_title(title: &str, out: &mut [u8; CATALOG_TITLE_BYTES]) {
    out.fill(0);
    copy_fixed(title.as_bytes(), out);
}

/// Bytes one staged `(source_hash, byte_size)` identity occupies in the
/// orphan sweep's scratch region.
pub const CATALOG_IDENTITY_BYTES: usize = 8;

/// Stage identity `index` into `scratch` for the orphan sweep's in-RAM
/// membership checks. Returns false (staging nothing) past capacity.
pub fn stage_catalog_identity(scratch: &mut [u8], index: usize, hash: u32, size: u32) -> bool {
    let at = index * CATALOG_IDENTITY_BYTES;
    let Some(slot) = scratch.get_mut(at..at + CATALOG_IDENTITY_BYTES) else {
        return false;
    };
    slot[..4].copy_from_slice(&hash.to_le_bytes());
    slot[4..].copy_from_slice(&size.to_le_bytes());
    true
}

/// Sort the first `count` staged identities so `catalog_identity_staged`
/// can binary-search them. Heapsort over the 8-byte entries: in place, no
/// allocation, no recursion -- O(N log N) with N bounded by the scratch
/// capacity (~2,048 identities in the 16 KB arena).
pub fn sort_catalog_identities(scratch: &mut [u8], count: usize) {
    let count = count.min(scratch.len() / CATALOG_IDENTITY_BYTES);
    for parent in (0..count / 2).rev() {
        sift_down_identity(scratch, parent, count);
    }
    for end in (1..count).rev() {
        swap_identities(scratch, 0, end);
        sift_down_identity(scratch, 0, end);
    }
}

/// Whether `(hash, size)` is among the first `count` staged identities,
/// which must already be ordered by `sort_catalog_identities` -- the check
/// is a binary search, O(log N) per cache dir instead of a linear scan. A
/// zero identity never matches, mirroring the streamed catalog lookup that
/// refuses to resolve `(0, 0)`.
pub fn catalog_identity_staged(scratch: &[u8], count: usize, hash: u32, size: u32) -> bool {
    if hash == 0 && size == 0 {
        return false;
    }
    if count > scratch.len() / CATALOG_IDENTITY_BYTES {
        return false;
    }
    let key = identity_key(hash, size);
    let (mut lo, mut hi) = (0usize, count);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match staged_key(scratch, mid).cmp(&key) {
            core::cmp::Ordering::Less => lo = mid + 1,
            core::cmp::Ordering::Greater => hi = mid,
            core::cmp::Ordering::Equal => return true,
        }
    }
    false
}

/// The sort/search key for one identity: the staged slot's little-endian
/// bytes as a `u64`, so `identity_key(hash, size)` and `staged_key` agree
/// byte for byte with what `stage_catalog_identity` wrote.
fn identity_key(hash: u32, size: u32) -> u64 {
    u64::from(hash) | (u64::from(size) << 32)
}

fn staged_key(scratch: &[u8], index: usize) -> u64 {
    let at = index * CATALOG_IDENTITY_BYTES;
    let mut bytes = [0u8; CATALOG_IDENTITY_BYTES];
    bytes.copy_from_slice(&scratch[at..at + CATALOG_IDENTITY_BYTES]);
    u64::from_le_bytes(bytes)
}

fn swap_identities(scratch: &mut [u8], a: usize, b: usize) {
    for offset in 0..CATALOG_IDENTITY_BYTES {
        scratch.swap(
            a * CATALOG_IDENTITY_BYTES + offset,
            b * CATALOG_IDENTITY_BYTES + offset,
        );
    }
}

fn sift_down_identity(scratch: &mut [u8], mut parent: usize, end: usize) {
    loop {
        let mut child = parent * 2 + 1;
        if child >= end {
            return;
        }
        if child + 1 < end && staged_key(scratch, child) < staged_key(scratch, child + 1) {
            child += 1;
        }
        if staged_key(scratch, parent) >= staged_key(scratch, child) {
            return;
        }
        swap_identities(scratch, parent, child);
        parent = child;
    }
}

fn copy_fixed(src: &[u8], dst: &mut [u8]) {
    let len = src.len().min(dst.len());
    dst[..len].copy_from_slice(&src[..len]);
}

fn fixed_str(bytes: &[u8]) -> &str {
    let len = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    core::str::from_utf8(&bytes[..len]).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrips_and_rejects_other_versions() {
        let mut header = [0u8; CATALOG_HEADER_BYTES];
        encode_catalog_header(1234, &mut header);
        assert_eq!(decode_catalog_header(&header), Some(1234));

        // The version byte is the migration mechanism: an old catalog fails
        // the decode and the caller rescans.
        let mut stale = header;
        stale[4] = CATALOG_VERSION - 1;
        assert_eq!(decode_catalog_header(&stale), None);

        let mut wrong_magic = header;
        wrong_magic[0] = b'Y';
        assert_eq!(decode_catalog_header(&wrong_magic), None);
    }

    #[test]
    fn placeholder_header_never_decodes() {
        let mut header = [0u8; CATALOG_HEADER_BYTES];
        encode_catalog_placeholder_header(&mut header);
        assert_eq!(decode_catalog_header(&header), None);
    }

    #[test]
    fn header_faults_separate_another_version_from_damage() {
        // All four rescan; only the version cases are the format doing its
        // job. A reader that reports must not call the designed migration a
        // fault, or the first boot after every CATALOG_VERSION bump fails a
        // strict capture.
        let mut header = [0u8; CATALOG_HEADER_BYTES];
        encode_catalog_header(1234, &mut header);
        assert_eq!(classify_catalog_header(&header), Ok(1234));

        let mut stale = header;
        stale[4] = CATALOG_VERSION - 1;
        assert_eq!(
            classify_catalog_header(&stale),
            Err(CatalogHeaderFault::Stale)
        );

        // A version this build has never seen is still just a version.
        let mut newer = header;
        newer[4] = CATALOG_VERSION + 1;
        assert_eq!(
            classify_catalog_header(&newer),
            Err(CatalogHeaderFault::Stale)
        );

        // The placeholder shares the magic and means an interrupted scan.
        let mut placeholder = [0u8; CATALOG_HEADER_BYTES];
        encode_catalog_placeholder_header(&mut placeholder);
        assert_eq!(
            classify_catalog_header(&placeholder),
            Err(CatalogHeaderFault::Invalid)
        );

        let mut wrong_magic = header;
        wrong_magic[0] = b'Y';
        assert_eq!(
            classify_catalog_header(&wrong_magic),
            Err(CatalogHeaderFault::Invalid)
        );
    }

    #[test]
    fn file_len_matches_header_plus_records() {
        assert_eq!(catalog_file_len(0), CATALOG_HEADER_BYTES);
        assert_eq!(
            catalog_file_len(3),
            CATALOG_HEADER_BYTES + 3 * CATALOG_RECORD_BYTES
        );
    }

    #[test]
    fn record_roundtrips_all_fields_including_title() {
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut record,
            "/books/wuthering-heights.epub",
            BookRoot::Library,
            "Bront\u{eb}/wuthering-heights.epub",
            "Wuthering Heights",
            "WUTHE~01.EPU",
            123_456,
            0xdead_beef,
        );
        let decoded = decode_catalog_record(&record);
        assert_eq!(
            decoded.display_name.as_str(),
            "/books/wuthering-heights.epub"
        );
        // Library-root relative: the library directory's own name is not a
        // component, so the record and a listing of it agree.
        assert_eq!(decoded.root, Some(BookRoot::Library));
        assert_eq!(decoded.path.as_str(), "Bront\u{eb}/wuthering-heights.epub");
        assert_eq!(decoded.title.as_str(), "Wuthering Heights");
        assert_eq!(decoded.upload_alias.as_str(), "WUTHE~01.EPU");
        assert_eq!(decoded.byte_size, 123_456);
        assert_eq!(decoded.source_hash, 0xdead_beef);
        assert_eq!(catalog_record_identity(&record), (0xdead_beef, 123_456));
    }

    #[test]
    fn empty_title_decodes_empty_for_the_stem_fallback() {
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut record,
            "/plain.epub",
            BookRoot::CardRoot,
            "plain.epub",
            "",
            "PLAIN.EPU",
            9,
            7,
        );
        let decoded = decode_catalog_record(&record);
        assert!(decoded.title.is_empty());
        assert_eq!(decoded.root, Some(BookRoot::CardRoot));
        assert_eq!(decoded.path.as_str(), "plain.epub");
    }

    #[test]
    fn overlong_fields_truncate_to_their_budgets() {
        // Over every field budget, including the widest.
        let long = "x".repeat(CATALOG_PATH_BYTES + 10);
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut record,
            &long,
            BookRoot::Library,
            &long,
            &long,
            &long,
            1,
            2,
        );
        let decoded = decode_catalog_record(&record);
        assert_eq!(decoded.display_name.len(), 64);
        assert_eq!(decoded.title.len(), 64);
        assert_eq!(decoded.path.len(), CATALOG_PATH_BYTES);
        assert_eq!(decoded.upload_alias.len(), CATALOG_ALIAS_BYTES);
    }

    #[test]
    fn an_unknown_root_makes_a_record_unopenable_rather_than_misplaced() {
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut record,
            "/books/d.epub",
            BookRoot::Library,
            "d.epub",
            "",
            "D.EPU",
            1,
            2,
        );
        record[0] = 0x7f;

        // Resolving "d.epub" against the wrong root can name a real file that
        // is not this book, so an unreadable root answers nothing at all.
        assert_eq!(decode_catalog_record(&record).root, None);
    }

    #[test]
    fn title_field_rewrite_in_place_matches_a_full_reencode() {
        // The book-open path patches only the 64-byte title field; it must
        // land exactly where a from-scratch encode puts the title.
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut record,
            "/b.epub",
            BookRoot::Library,
            "b.epub",
            "",
            "B.EPU",
            10,
            20,
        );
        let mut field = [0u8; CATALOG_TITLE_BYTES];
        encode_catalog_title("Bleak House", &mut field);
        record[CATALOG_RECORD_TITLE_OFFSET..CATALOG_RECORD_TITLE_OFFSET + CATALOG_TITLE_BYTES]
            .copy_from_slice(&field);

        let mut expected = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut expected,
            "/b.epub",
            BookRoot::Library,
            "b.epub",
            "Bleak House",
            "B.EPU",
            10,
            20,
        );
        assert_eq!(record, expected);
        assert_eq!(decode_catalog_record(&record).title.as_str(), "Bleak House");
    }

    #[test]
    fn staged_identities_answer_membership_like_a_catalog_walk() {
        // Staged deliberately out of key order: membership must come from
        // the sort, not the staging order.
        let mut scratch = [0u8; 64];
        assert!(stage_catalog_identity(&mut scratch, 0, 0xcccc, 300));
        assert!(stage_catalog_identity(&mut scratch, 1, 0xaaaa, 100));
        assert!(stage_catalog_identity(&mut scratch, 2, 0xbbbb, 200));
        sort_catalog_identities(&mut scratch, 3);

        assert!(catalog_identity_staged(&scratch, 3, 0xbbbb, 200));
        assert!(!catalog_identity_staged(&scratch, 3, 0xbbbb, 201));
        assert!(
            !catalog_identity_staged(&scratch, 1, 0xbbbb, 200),
            "past count"
        );
        // The zero identity never matches: an unreadable cache header must
        // not accidentally resolve to a zeroed record.
        assert!(stage_catalog_identity(&mut scratch, 3, 0, 0));
        sort_catalog_identities(&mut scratch, 4);
        assert!(!catalog_identity_staged(&scratch, 4, 0, 0));
    }

    #[test]
    fn sorted_lookup_matches_a_linear_scan_over_many_identities() {
        // Enough entries (descending, with duplicates) that the binary
        // search crosses several pivot levels on both hit and miss paths.
        const N: usize = 33;
        // Adjacent staged indices share one key, so every key appears twice
        // (bar one) and the search must still find it.
        fn identity_for(index: usize) -> (u32, u32) {
            let v = (N as u32 - index as u32).div_ceil(2);
            (v * 3, v % 5)
        }
        let mut scratch = [0u8; N * CATALOG_IDENTITY_BYTES];
        for index in 0..N {
            let (hash, size) = identity_for(index);
            assert!(stage_catalog_identity(&mut scratch, index, hash, size));
        }
        sort_catalog_identities(&mut scratch, N);

        for index in 0..N {
            let (hash, size) = identity_for(index);
            assert!(
                catalog_identity_staged(&scratch, N, hash, size),
                "staged identity {index} must be found after sorting"
            );
            // Same hash, different size: the full 8-byte key must match.
            assert!(!catalog_identity_staged(&scratch, N, hash, size + 7));
        }
        assert!(!catalog_identity_staged(&scratch, N, 1, 1));
    }

    #[test]
    fn staging_past_capacity_reports_the_overflow() {
        let mut scratch = [0u8; CATALOG_IDENTITY_BYTES * 2];
        assert!(stage_catalog_identity(&mut scratch, 0, 1, 1));
        assert!(stage_catalog_identity(&mut scratch, 1, 2, 2));
        assert!(!stage_catalog_identity(&mut scratch, 2, 3, 3));
    }

    /// A v8 record for a book pre-v8 firmware could address, alongside the
    /// identity that firmware would have saved for it.
    fn flat_record(display: &str, size: u32) -> ([u8; CATALOG_RECORD_BYTES], u32) {
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        let name = display.rsplit('/').next().unwrap_or(display);
        encode_catalog_record(
            &mut record,
            display,
            BookRoot::Library,
            name,
            "",
            "",
            size,
            crate::cache::source_hash_at(BookRoot::Library, name, size),
        );
        let old_hash =
            crate::cache::legacy_source_hash(display, size).expect("flat shape has a legacy hash");
        (record, old_hash)
    }

    /// A record's pre-v8 identity is reconstructible exactly when its
    /// display shape is one old firmware could have produced.
    #[test]
    fn a_records_legacy_identity_follows_its_display_shape() {
        let (record, old_hash) = flat_record("/books/Book.epub", 12_345);
        assert_eq!(
            catalog_record_legacy_identity(&record),
            Some((old_hash, 12_345)),
        );

        let mut nested = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut nested,
            "/books/fiction/dune.epub",
            BookRoot::Library,
            "fiction/dune.epub",
            "",
            "",
            12_345,
            crate::cache::source_hash_at(BookRoot::Library, "fiction/dune.epub", 12_345),
        );
        assert_eq!(
            catalog_record_legacy_identity(&nested),
            None,
            "a nested book has no pre-v8 identity to answer to",
        );
    }

    /// The reviewer's upgrade sequence: a pre-v8 state record's identity
    /// against a v8 catalog holding the same flat book under its new
    /// identity must resolve to that book, and only that shape may answer.
    #[test]
    fn a_pre_v8_saved_identity_resolves_against_the_v8_catalog() {
        let (dune, old_dune) = flat_record("/books/dune.epub", 4_096);
        let (other, _) = flat_record("/books/other.epub", 9_000);
        // A nested book whose display text and size mirror the flat one as
        // closely as v8 allows; it must not be eligible.
        let mut nested = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut nested,
            "/books/shelf/dune.epub",
            BookRoot::Library,
            "shelf/dune.epub",
            "",
            "",
            4_096,
            crate::cache::source_hash_at(BookRoot::Library, "shelf/dune.epub", 4_096),
        );

        let mut scan = LegacyIdentityScan::new(old_dune, 4_096);
        scan.offer(0, &other);
        scan.offer(1, &nested);
        scan.offer(2, &dune);
        assert_eq!(scan.finish(), Some(2), "the flat book resolves");

        // Two records answering the same legacy identity is a guess between
        // real books, which restoration refuses.
        let mut ambiguous = LegacyIdentityScan::new(old_dune, 4_096);
        ambiguous.offer(0, &dune);
        ambiguous.offer(1, &dune);
        assert_eq!(ambiguous.finish(), None);

        // An identity naming nothing on the card stays a miss.
        let mut missing = LegacyIdentityScan::new(old_dune ^ 1, 4_096);
        missing.offer(0, &dune);
        assert_eq!(missing.finish(), None);
    }

    /// The current rule collides with itself too: this pair of legal
    /// library locators shares one 32-bit identity at one size. A lookup
    /// that trusted a hint or the first match could resolve a saved
    /// identity to the other book after the catalog was rebuilt in a
    /// different order, and the next save would persist the swap. One
    /// match resolves; two refuse.
    #[test]
    fn a_same_domain_hash_collision_refuses_rather_than_guesses() {
        let size = 1_234_567;
        let hash_a =
            crate::cache::source_hash_at(BookRoot::Library, "Fiction/zIx6RBhQEK.epub", size);
        let hash_b =
            crate::cache::source_hash_at(BookRoot::Library, "Fiction/nTfOyBwYzX.epub", size);
        assert_eq!(hash_a, hash_b, "the collision this test exists for");

        let mut twin_a = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut twin_a,
            "/books/fiction/zix6rbhqek.epub",
            BookRoot::Library,
            "Fiction/zIx6RBhQEK.epub",
            "",
            "",
            size,
            hash_a,
        );
        let mut twin_b = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut twin_b,
            "/books/fiction/ntfoybwyzx.epub",
            BookRoot::Library,
            "Fiction/nTfOyBwYzX.epub",
            "",
            "",
            size,
            hash_b,
        );

        let mut alone = IdentityScan::new(hash_a, size);
        alone.offer(0, &twin_a);
        assert_eq!(alone.finish(), Some(0), "one match resolves");

        let mut twins = IdentityScan::new(hash_a, size);
        twins.offer(0, &twin_a);
        twins.offer(1, &twin_b);
        assert_eq!(
            twins.finish(),
            None,
            "two rows answering one identity is a guess between real books",
        );
    }

    /// The two hash domains genuinely collide: this pair of legal books
    /// shares one 32-bit value across the pre-v8 and v8 rules. An
    /// unversioned saved identity read under the current rule first would
    /// restore the unrelated nested book and then persist that mistake at
    /// the next save. The record's version byte says which rule wrote the
    /// identity, and the legacy reading cannot see the nested book at all.
    #[test]
    fn a_cross_domain_hash_collision_cannot_steal_a_restoration() {
        let size = 1_234_567;
        let legacy_a = crate::cache::legacy_source_hash("/books/6HawKebl.epub", size)
            .expect("flat shape has a legacy identity");
        let current_b =
            crate::cache::source_hash_at(BookRoot::Library, "Fiction/XpipGrkt.epub", size);
        assert_eq!(legacy_a, current_b, "the collision this test exists for");

        let (a, old_a) = flat_record("/books/6HawKebl.epub", size);
        assert_eq!(old_a, legacy_a);
        let mut b = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut b,
            "/books/fiction/xpipgrkt.epub",
            BookRoot::Library,
            "Fiction/XpipGrkt.epub",
            "",
            "",
            size,
            current_b,
        );
        // The nested book owns the colliding value as its current identity.
        assert_eq!(catalog_record_identity(&b), (legacy_a, size));

        // Read under the rule that wrote the saved identity, only A answers.
        let mut scan = LegacyIdentityScan::new(legacy_a, size);
        scan.offer(0, &b);
        scan.offer(1, &a);
        assert_eq!(
            scan.finish(),
            Some(1),
            "the legacy reading finds the flat book, not the collider",
        );
    }
}
