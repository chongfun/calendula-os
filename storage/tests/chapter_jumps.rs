//! Choosing a chapter from the list, when its text will not come off the card.
//!
//! Tests run the real storage task over a FAT image and the real reducer over
//! its events. See `support`.

mod support;

use app_core::{AppView, Button, LibraryEvent};
use support::{epub, Card, Device};

const BOOK: &str = "86 - Volume 02.epub";
const HOME: &str = "BOOKS/86/86 - Volume 02.epub";

/// A book open on its first page, fully built, with the chapter list showing.
fn on_the_chapter_list() -> (Card, Device) {
    let card = Card::blank();
    card.put(HOME, &epub("86 Volume 2", 6, 2));
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("86");
    device.choose(BOOK);
    assert_eq!(device.app.view, AppView::Reading);
    // Portrait: the first press shows the key sheet, the second acts on it.
    device.press(Button::Confirm);
    device.press(Button::Confirm);
    assert_eq!(device.app.view, AppView::Chapters);
    (card, device)
}

/// The one book's cache directory, as `READER/CACHE2/<key>`.
fn cache_dir(card: &Card) -> String {
    let keys = card.list("READER/CACHE2");
    assert_eq!(keys.len(), 1, "one book, one cache directory: {keys:?}");
    format!("READER/CACHE2/{}", keys[0])
}

/// Pick the last chapter, and return the events its jump produced.
fn jump_to_last_chapter(device: &mut Device) -> Vec<LibraryEvent> {
    device.press(Button::Previous);
    let before = device.log.len();
    device.press(Button::Confirm);
    device.log[before..].to_vec()
}

/// With the book and its index gone, no page of it loads. The jump says the
/// book is unreadable instead of announcing a page nothing loaded.
#[test]
fn a_chapter_jump_with_nothing_to_load_marks_the_book_unreadable() {
    let (card, mut device) = on_the_chapter_list();
    let book_id = device.app.book_id;
    let dir = cache_dir(&card);
    card.delete(HOME);
    card.delete(&format!("{dir}/BOOK.BIN"));

    let events = jump_to_last_chapter(&mut device);

    assert!(
        events.contains(&LibraryEvent::BookOpenUnreadable { book_id }),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, LibraryEvent::Loaded { .. })),
        "no page was announced: {events:?}"
    );
    assert!(device.app.book_unreadable());
    assert_eq!(device.app.view, AppView::Reading, "the book stays selected");
}

/// With the last chapter's section, the book and its content stream gone, the
/// jump falls back to the first page, which still loads from the cache.
#[test]
fn a_chapter_jump_whose_section_will_not_load_lands_on_the_first_page() {
    let (card, mut device) = on_the_chapter_list();
    let book_id = device.app.book_id;
    let dir = cache_dir(&card);
    let sections = format!("{dir}/SECTIONS");
    let mut names = card.list(&sections);
    names.sort();
    assert!(
        names.len() > 1,
        "the book spans several sections: {names:?}"
    );
    // The content stream would rebuild the section without the book.
    card.delete(HOME);
    card.delete(&format!("{dir}/CONT.BIN"));
    card.delete(&format!("{sections}/{}", names.last().unwrap()));

    let events = jump_to_last_chapter(&mut device);

    let landed = events.iter().find_map(|e| match *e {
        LibraryEvent::Loaded {
            book_id: id,
            position,
            ..
        } if id == book_id => Some(position),
        _ => None,
    });
    assert_eq!(landed, Some(Some(0)), "{events:?}");
    assert!(!device.app.book_unreadable());
    assert_eq!(device.app.page, 0);
}
