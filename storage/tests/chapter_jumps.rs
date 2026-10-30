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

/// The Contents screen numbers a chapter's first page from 1, whichever
/// store holds the list. The on-card list passed the 0-based start page into
/// the row, so every number read one low and the first chapter showed none.
#[test]
fn contents_rows_number_pages_from_one() {
    let card = Card::blank();
    card.put(HOME, &epub("86 Volume 2", 6, 2));
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("86");
    device.point_at(BOOK);
    device.press(Button::Confirm);
    device.settle();
    let resident: Vec<u32> = (0..device.store.toc_count())
        .map(|i| device.store.toc_item(i).expect("resident row").page)
        .collect();
    assert_eq!(resident.first(), Some(&1), "{resident:?}");

    // Portrait: the first press shows the key sheet, the second opens Contents.
    device.press(Button::Confirm);
    device.press(Button::Confirm);
    device.settle();
    assert_eq!(device.app.view, AppView::Chapters);
    assert!(device.store.text_holds_toc(), "the list came off the card");
    let listed: Vec<u32> = (0..device.store.overview_chapter_count())
        .map(|i| device.store.overview_row_page(i))
        .collect();
    assert_eq!(listed, resident);
}

/// The chapter count announced by a Loaded event.
fn loaded_chapters(events: &[LibraryEvent]) -> Option<u16> {
    events.iter().rev().find_map(|event| match event {
        LibraryEvent::Loaded { chapters, .. } => Some(*chapters),
        _ => None,
    })
}

/// A jump reloads the book, and with it the resident chapter list, which
/// holds 128. The count from the Contents visit stays, so a longer book is
/// not announced short. Another book still reports its own count.
#[test]
fn a_chapter_jump_keeps_the_full_count_past_the_resident_cap() {
    let card = Card::blank();
    card.put("BOOKS/Shelf/Long.epub", &epub("Long", 140, 3));
    card.put("BOOKS/Shelf/Short.epub", &epub("Short", 6, 2));
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Shelf");
    device.choose("Long.epub");
    device.settle();
    // Portrait: the first press shows the key sheet, the second opens Contents.
    device.press(Button::Confirm);
    device.press(Button::Confirm);
    device.settle();
    assert_eq!(device.app.view, AppView::Chapters);
    assert_eq!(device.store.overview_chapter_count(), 140);

    let events = jump_to_last_chapter(&mut device);
    assert_eq!(loaded_chapters(&events), Some(140), "{events:?}");

    // Portrait: the first press shows the key sheet, the second goes Home.
    // The library reopens in the book's folder.
    device.press(Button::Back);
    device.press(Button::Back);
    device.open_library();
    let before = device.log.len();
    device.choose("Short.epub");
    device.settle();
    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!(loaded_chapters(&device.log[before..]), Some(6));
}

/// Crossing a section boundary in the same book preserves resident chapter
/// tracking without re-reading the entire chapter map from TOC.BIN.
#[test]
fn section_crossing_in_same_book_does_not_reload_chapter_map() {
    let card = Card::blank();
    card.put(HOME, &epub("86 Volume 2", 6, 2));
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("86");
    device.choose(BOOK);
    device.settle();
    assert_eq!(device.app.view, AppView::Reading);

    assert!(device.store.chapter_start_ready());
    let token = device.store.chapter_start_token();
    assert_ne!(token, (0, 0, 0, 0));

    let sec0_pages = device
        .store
        .book_sections
        .iter()
        .next()
        .expect("has section")
        .page_count as u32;
    assert!(sec0_pages > 0);
    assert_eq!(device.store.current_section_start_page, 0);

    // Delete TOC.BIN so an attempt to reload the full TOC chapter map would fail
    // and clear chapter_start_ready.
    let dir = cache_dir(&card);
    card.delete(&format!("{dir}/TOC.BIN"));

    device.turn(sec0_pages);
    device.settle();

    assert_eq!(device.store.current_section_start_page, sec0_pages);
    assert!(device.store.chapter_start_ready());
    assert_eq!(device.store.chapter_start_token(), token);
}
