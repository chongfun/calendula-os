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

/// The one book's `BOOK.BIN`, as `READER/CACHE2/<key>/BOOK.BIN`.
fn book_index_path(card: &Card) -> String {
    let keys = card.list("READER/CACHE2");
    assert_eq!(keys.len(), 1, "one book, one cache directory: {keys:?}");
    format!("READER/CACHE2/{}/BOOK.BIN", keys[0])
}

/// A `BOOK.BIN` whose header claims more TOC text than any build writes:
/// the title's offset summed from it would overflow a `u32`. The scan reads
/// the label as missing and lists the book under its file name, rather than
/// panicking on the sum or seeking somewhere inside the file.
#[test]
fn a_scan_refuses_a_cached_title_behind_a_header_past_its_bounds() {
    let card = read_and_sleep();
    let path = book_index_path(&card);
    let mut index = card.read(&path).expect("the book index is cached");
    // `toc_text_bytes`, at byte 36 of the v2 header.
    index[36..40].copy_from_slice(&u32::MAX.to_le_bytes());
    card.delete(&path);
    card.put_short(&path, &index);
    // No catalog, so the boot scans and resolves every label afresh.
    card.delete("READER/CATALOG.BIN");

    let mut device = Device::wake(&card);
    device.open_library();
    assert_eq!(device.rows(), vec![BOOK.to_owned()], "{:?}", device.log);
    device.choose(BOOK);
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
}
