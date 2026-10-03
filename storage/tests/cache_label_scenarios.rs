//! The Library label a scan reads out of a book's cache. `BOOK.BIN` is on
//! the card, so its header is input like any other: the counts the title's
//! offset is summed from have to be held to the bounds the index loaders
//! hold them to, or a corrupt header overflows the sum.

mod support;

use app_core::AppView;
use support::{epub, Card, Device};

const BOOK: &str = "Alpha.epub";
const HOME: &str = "BOOKS/Alpha.epub";

/// Open the one book so its cache, title included, is on the card, and
/// return the card with the reader asleep.
fn read_and_sleep() -> Card {
    let card = Card::blank();
    card.put(HOME, &epub("Alpha Title", 3, 1));
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose(BOOK);
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    device.sleep();
    card
}

/// The title the last scan stored in the one book's catalog record: the
/// cached EPUB title, or empty when the scan found none and the label falls
/// back to the file stem.
fn scanned_title(card: &Card) -> String {
    card.session(|root| storage::library_sd::read_catalog_record_at(root, 0))
        .expect("the book's catalog record")
        .title
        .to_string()
}

/// The one book's `BOOK.BIN`, as `READER/CACHE2/<key>/BOOK.BIN`.
fn book_index_path(card: &Card) -> String {
    let keys = card.list("READER/CACHE2");
    assert_eq!(keys.len(), 1, "one book, one cache directory: {keys:?}");
    format!("READER/CACHE2/{}/BOOK.BIN", keys[0])
}

/// A `BOOK.BIN` whose header claims more TOC text than any build writes:
/// the title's offset summed from it would overflow a `u32`. The scan reads
/// no cached title, so the label falls back to the file stem, rather than
/// panicking on the sum or reading a wrapped offset inside the file as the
/// title. The book still opens.
#[test]
fn a_scan_refuses_a_cached_title_behind_a_header_past_its_bounds() {
    let card = read_and_sleep();
    // Over the intact cache, a fresh scan labels the book with its title.
    card.delete("READER/CATALOG.BIN");
    Device::wake(&card).sleep();
    assert_eq!(scanned_title(&card), "Alpha Title");

    let path = book_index_path(&card);
    let mut index = card.read(&path).expect("the book index is cached");
    let mut header = proto::cache::decode_book_v2_header(&index).expect("a v2 header");
    // Sized so the title's offset sums to exactly 2^32: wrapped, it lands on
    // the header's own magic, which a reader trusting the count would store
    // as the title.
    let before_text = proto::cache::BOOK_V2_HEADER_BYTES as u32
        + u32::from(header.section_count) * proto::cache::BOOK_V2_SECTION_RECORD_BYTES as u32
        + u32::from(header.toc_count) * proto::cache::TOC_RECORD_BYTES as u32;
    header.toc_text_bytes = before_text.wrapping_neg();
    proto::cache::encode_book_v2_header(header, &mut index).expect("rewrite the header");
    card.delete(&path);
    card.put_short(&path, &index);
    // No catalog, so the boot scans and resolves every label afresh.
    card.delete("READER/CATALOG.BIN");

    let mut device = Device::wake(&card);
    assert_eq!(scanned_title(&card), "", "{:?}", device.log);
    device.open_library();
    assert_eq!(device.rows(), vec![BOOK.to_owned()], "{:?}", device.log);
    device.choose(BOOK);
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
}
