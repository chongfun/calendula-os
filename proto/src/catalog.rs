//! On-card CATALOG.BIN format: the header and fixed-size book records the
//! firmware's library scan writes and every list/lookup path reads back.
//! Lives here (not in firmware) so the encode/decode round-trip, the title
//! field layout, and the orphan-sweep identity staging are host-testable.

use heapless::String;

pub const CATALOG_MAGIC: &[u8; 4] = b"X4CT";
/// v3 widened the on-disk book count from a single byte to a `u16` at
/// `header[5..7]`. v4 rebuilt stale records written before long filenames
/// were safely bounded. v5 appends a 64-byte title field to every record so
/// the Library list reads labels straight from the catalog instead of
/// probing each book's cache per window crossing; the version check makes
/// an older catalog fail to load, and a fresh scan rebuilds it -- no
/// migration code needed.
pub const CATALOG_VERSION: u8 = 5;
pub const CATALOG_HEADER_BYTES: usize = 8;
pub const CATALOG_RECORD_BYTES: usize = 156;
/// Byte range of the title field inside a record, exposed so the firmware
/// can rewrite just the title in place when a book open learns the real
/// EPUB title.
pub const CATALOG_RECORD_TITLE_OFFSET: usize = 92;
pub const CATALOG_TITLE_BYTES: usize = 64;

/// One catalog record decoded into owned fields, so it outlives the file
/// handle it was read through.
pub struct CatalogRecord {
    pub display_name: String<64>,
    pub open_name: String<16>,
    /// The EPUB title learned when the book was last opened (or the upload
    /// label stashed at upload). Empty when unknown; readers fall back to a
    /// label derived from the file stem.
    pub title: String<64>,
    pub in_books_dir: bool,
    pub byte_size: u32,
    pub source_hash: u32,
}

pub fn encode_catalog_header(count: u16, out: &mut [u8; CATALOG_HEADER_BYTES]) {
    out.fill(0);
    out[..4].copy_from_slice(CATALOG_MAGIC);
    out[4] = CATALOG_VERSION;
    out[5..7].copy_from_slice(&count.to_le_bytes());
}

/// The book count, or `None` when the magic or version doesn't match (the
/// caller then runs a fresh scan).
pub fn decode_catalog_header(header: &[u8; CATALOG_HEADER_BYTES]) -> Option<u16> {
    if &header[..4] != CATALOG_MAGIC || header[4] != CATALOG_VERSION {
        return None;
    }
    Some(u16::from_le_bytes([header[5], header[6]]))
}

#[allow(clippy::too_many_arguments)]
pub fn encode_catalog_record(
    out: &mut [u8; CATALOG_RECORD_BYTES],
    display_name: &str,
    open_name: &str,
    title: &str,
    in_books_dir: bool,
    byte_size: u32,
    source_hash: u32,
) {
    out.fill(0);
    out[0] = in_books_dir as u8;
    out[4..8].copy_from_slice(&byte_size.to_le_bytes());
    out[8..12].copy_from_slice(&source_hash.to_le_bytes());
    copy_fixed(display_name.as_bytes(), &mut out[12..76]);
    copy_fixed(open_name.as_bytes(), &mut out[76..92]);
    copy_fixed(
        title.as_bytes(),
        &mut out[CATALOG_RECORD_TITLE_OFFSET..CATALOG_RECORD_TITLE_OFFSET + CATALOG_TITLE_BYTES],
    );
}

pub fn decode_catalog_record(record: &[u8; CATALOG_RECORD_BYTES]) -> CatalogRecord {
    let mut display_name = String::<64>::new();
    let _ = display_name.push_str(fixed_str(&record[12..76]));
    let mut open_name = String::<16>::new();
    let _ = open_name.push_str(fixed_str(&record[76..92]));
    let mut title = String::<64>::new();
    let _ = title.push_str(fixed_str(
        &record[CATALOG_RECORD_TITLE_OFFSET..CATALOG_RECORD_TITLE_OFFSET + CATALOG_TITLE_BYTES],
    ));
    CatalogRecord {
        display_name,
        open_name,
        title,
        in_books_dir: record[0] != 0,
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

/// Whether `(hash, size)` is among the first `count` staged identities. A
/// zero identity never matches, mirroring the streamed catalog lookup that
/// refuses to resolve `(0, 0)`.
pub fn catalog_identity_staged(scratch: &[u8], count: usize, hash: u32, size: u32) -> bool {
    if hash == 0 && size == 0 {
        return false;
    }
    for index in 0..count {
        let at = index * CATALOG_IDENTITY_BYTES;
        let Some(slot) = scratch.get(at..at + CATALOG_IDENTITY_BYTES) else {
            return false;
        };
        let staged_hash = u32::from_le_bytes([slot[0], slot[1], slot[2], slot[3]]);
        let staged_size = u32::from_le_bytes([slot[4], slot[5], slot[6], slot[7]]);
        if staged_hash == hash && staged_size == size {
            return true;
        }
    }
    false
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
    fn record_roundtrips_all_fields_including_title() {
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut record,
            "/books/wuthering-heights.epub",
            "WUTHE~01.EPU",
            "Wuthering Heights",
            true,
            123_456,
            0xdead_beef,
        );
        let decoded = decode_catalog_record(&record);
        assert_eq!(
            decoded.display_name.as_str(),
            "/books/wuthering-heights.epub"
        );
        assert_eq!(decoded.open_name.as_str(), "WUTHE~01.EPU");
        assert_eq!(decoded.title.as_str(), "Wuthering Heights");
        assert!(decoded.in_books_dir);
        assert_eq!(decoded.byte_size, 123_456);
        assert_eq!(decoded.source_hash, 0xdead_beef);
        assert_eq!(catalog_record_identity(&record), (0xdead_beef, 123_456));
    }

    #[test]
    fn empty_title_decodes_empty_for_the_stem_fallback() {
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(&mut record, "/plain.epub", "PLAIN.EPU", "", false, 9, 7);
        let decoded = decode_catalog_record(&record);
        assert!(decoded.title.is_empty());
        assert!(!decoded.in_books_dir);
    }

    #[test]
    fn overlong_fields_truncate_to_their_budgets() {
        // 100 bytes: over every field budget (64/16/64).
        let long = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\
                    xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(&mut record, long, long, long, false, 1, 2);
        let decoded = decode_catalog_record(&record);
        assert_eq!(decoded.display_name.len(), 64);
        assert_eq!(decoded.open_name.len(), 16);
        assert_eq!(decoded.title.len(), 64);
    }

    #[test]
    fn title_field_rewrite_in_place_matches_a_full_reencode() {
        // The book-open path patches only the 64-byte title field; it must
        // land exactly where a from-scratch encode puts the title.
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(&mut record, "/b.epub", "B.EPU", "", true, 10, 20);
        let mut field = [0u8; CATALOG_TITLE_BYTES];
        encode_catalog_title("Bleak House", &mut field);
        record[CATALOG_RECORD_TITLE_OFFSET..CATALOG_RECORD_TITLE_OFFSET + CATALOG_TITLE_BYTES]
            .copy_from_slice(&field);

        let mut expected = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut expected,
            "/b.epub",
            "B.EPU",
            "Bleak House",
            true,
            10,
            20,
        );
        assert_eq!(record, expected);
        assert_eq!(decode_catalog_record(&record).title.as_str(), "Bleak House");
    }

    #[test]
    fn staged_identities_answer_membership_like_a_catalog_walk() {
        let mut scratch = [0u8; 64];
        assert!(stage_catalog_identity(&mut scratch, 0, 0xaaaa, 100));
        assert!(stage_catalog_identity(&mut scratch, 1, 0xbbbb, 200));
        assert!(stage_catalog_identity(&mut scratch, 2, 0xcccc, 300));

        assert!(catalog_identity_staged(&scratch, 3, 0xbbbb, 200));
        assert!(!catalog_identity_staged(&scratch, 3, 0xbbbb, 201));
        assert!(
            !catalog_identity_staged(&scratch, 1, 0xbbbb, 200),
            "past count"
        );
        // The zero identity never matches: an unreadable cache header must
        // not accidentally resolve to a zeroed record.
        assert!(stage_catalog_identity(&mut scratch, 3, 0, 0));
        assert!(!catalog_identity_staged(&scratch, 4, 0, 0));
    }

    #[test]
    fn staging_past_capacity_reports_the_overflow() {
        let mut scratch = [0u8; CATALOG_IDENTITY_BYTES * 2];
        assert!(stage_catalog_identity(&mut scratch, 0, 1, 1));
        assert!(stage_catalog_identity(&mut scratch, 1, 2, 2));
        assert!(!stage_catalog_identity(&mut scratch, 2, 3, 3));
    }
}
