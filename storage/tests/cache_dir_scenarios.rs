//! The shape of a book's cache directory on the card. The firmware owns it,
//! so an entry there of a kind nothing of ours writes is cleared for the
//! cache to be remade, not read as a book that cannot be opened.

mod support;

use app_core::{AppView, LibraryEvent};
use support::{epub, Card, Device};

const BOOK: &str = "Alpha.epub";
const HOME: &str = "BOOKS/Alpha.epub";

/// Open the one book so its cache is on the card, and return the card with
/// the reader asleep.
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

/// The one book's cache directory, as `READER/CACHE2/<key>`.
fn cache_dir(card: &Card) -> String {
    let keys = card.list("READER/CACHE2");
    assert_eq!(keys.len(), 1, "one book, one cache directory: {keys:?}");
    format!("READER/CACHE2/{}", keys[0])
}

/// A file where the `SECTIONS` directory should be. No section can be read
/// or written under it, so the cache is rebuilt, and the build clears the
/// file for its directory: the book opens, and its sections are back.
#[test]
fn a_file_at_sections_is_cleared_for_the_directory_and_the_book_opens() {
    let card = read_and_sleep();
    let dir = cache_dir(&card);
    let sections = format!("{dir}/SECTIONS");
    for name in card.list(&sections) {
        card.delete(&format!("{sections}/{name}"));
    }
    card.remove_folder(&sections);
    card.put_short(&sections, b"not a directory");

    let mut device = Device::wake(&card);
    device.open_library();
    device.choose(BOOK);
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    assert!(
        device.saw(|event| matches!(event, LibraryEvent::Loaded { .. })),
        "{:?}",
        device.log
    );
    assert!(
        !device.saw(|event| matches!(event, LibraryEvent::BookOpenUnreadable { .. })),
        "{:?}",
        device.log
    );
    device.sleep();
    assert!(
        !card.list(&sections).is_empty(),
        "the sections are written under the directory again"
    );
}
