//! A computer moves a book while the device is off, and the reader comes back
//! to it. Each scenario here is a defect that reached the device in September
//! 2026 and was caught only by a person with the device in hand: the storage
//! task, the library scan and the app's reducer were each tested on their
//! own, and the way they fit together was not tested at all.
//!
//! Every test runs the real storage task over a FAT image and the real
//! reducer over the events it sends, through power cycles that keep only the
//! card. See `support`.

mod support;

use app_core::{AppView, LibraryEvent};
use display::font::FontSize;
use support::{epub, Card, Device};

const BOOK: &str = "86 - Volume 02.epub";
const HOME: &str = "BOOKS/86/86 - Volume 02.epub";
const MOVED: &str = "BOOKS/86/MOVED/86 - Volume 02.epub";

/// A card with two books in one folder, and a session that read one of them
/// at a non-default font size to page `page`, then slept. Returns the card and
/// the book's page count, as the finished build put it.
fn read_and_sleep(page: u32) -> (Card, u32) {
    let card = Card::blank();
    card.put(HOME, &epub("86 Volume 2", 6, 2));
    card.put("BOOKS/86/86 - Volume 01.epub", &epub("86 Volume 1", 6, 1));
    let mut device = Device::wake(&card);
    // Chosen in Settings at some point before; the app persists it with every
    // record it saves.
    device.app.font_size = FontSize::Large;
    device.open_library();
    device.choose("86");
    device.choose(BOOK);
    assert_eq!(device.app.view, AppView::Reading);
    device.turn(page);
    assert_eq!(device.app.page, page, "the reader got to the page");
    let pages = device.app.sd_page_count;
    device.sleep();
    (card, pages)
}

/// The first `Loaded` after the reader picked the moved book: what the open
/// put on screen before any background slice grew it.
fn first_open(device: &Device) -> LibraryEvent {
    let picked = device
        .log
        .iter()
        .position(|event| matches!(event, LibraryEvent::RowIsBook { .. }))
        .expect("the pick was answered with a book");
    *device.log[picked..]
        .iter()
        .find(|event| matches!(event, LibraryEvent::Loaded { .. }))
        .expect("the book opened")
}

/// The boot, the library, a folder and a book, on a card nobody has edited.
#[test]
fn the_harness_boots_a_card_and_opens_a_book() {
    let card = Card::blank();
    card.put(HOME, &epub("86 Volume 2", 6, 2));
    let mut device = Device::wake(&card);
    device.open_library();
    assert_eq!(device.rows(), vec!["86".to_owned()]);
    device.choose("86");
    device.choose(BOOK);
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    device.turn(5);
    assert_eq!(device.app.page, 5);
    assert!(
        device.app.sd_page_count > 20,
        "the background walk finished the book"
    );
}

/// Picking the moved book opens it, on the page the reader left, from the
/// pagination the move carried: no second press, no refused open, no
/// rebuild from the cover. Afterwards the library is still in the folder the
/// book was picked from.
#[test]
fn a_moved_book_opens_from_its_new_folder_on_the_page_it_was_left() {
    let (card, pages) = read_and_sleep(12);
    card.rename(HOME, MOVED);

    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("86");
    device.choose("MOVED");
    device.choose(BOOK);

    assert_eq!(
        device.app.view,
        AppView::Reading,
        "one press opens the book after the rescan, rather than leaving the reader \
         in the library to find it again: {:?}",
        device.log
    );
    assert!(
        !device.saw(|event| matches!(event, LibraryEvent::BookOpenFailed { .. })),
        "the open was not refused over the book's old identity: {:?}",
        device.log
    );
    match first_open(&device) {
        LibraryEvent::Loaded {
            pages: opened,
            position,
            ..
        } => {
            assert_eq!(
                opened, pages,
                "the open found the whole book's pagination, carried with it, \
                 rather than a rebuild's first pages"
            );
            assert_eq!(position, Some(12), "and put the reader on their page");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(device.app.page, 12);
    assert_eq!(
        device.app.library_depth, 2,
        "browsing stayed in the folder the book was picked from"
    );
    assert!(
        device.rows().contains(&BOOK.to_owned()),
        "{:?}",
        device.rows()
    );
}

/// The saved state names the book by its place, and carries the reading
/// settings. After a move it is carried to the new place, so the settings
/// come back and the book opens at the layout its pagination was built for.
#[test]
fn a_moved_book_opens_with_the_settings_it_was_read_at() {
    let (card, pages) = read_and_sleep(9);
    card.rename(HOME, MOVED);

    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("86");
    device.choose("MOVED");
    device.choose(BOOK);

    assert_eq!(
        device.app.font_size,
        FontSize::Large,
        "the settings came back"
    );
    assert_eq!(device.app.view, AppView::Reading);
    match first_open(&device) {
        LibraryEvent::Loaded { pages: opened, .. } => assert_eq!(
            opened, pages,
            "at the layout the carried pagination was built for, so it was used"
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(device.app.page, 9);

    // And the next power-on finds the book where it now is.
    device.sleep();
    let device = Device::wake(&card);
    assert_eq!(device.app.font_size, FontSize::Large);
}

/// A saved state can name a book the catalog no longer holds: deleted on a
/// computer, or left naming a place by an older firmware. The settings are
/// the reader's whatever became of the book, so they come back without it.
#[test]
fn the_reading_settings_come_back_when_the_saved_book_is_gone() {
    let (card, _) = read_and_sleep(4);
    // Rewrite the saved state as if its book had been somewhere that is not
    // on the card now.
    card.session(|root| {
        let mut record = reader_cache::files::read_state_file(root).expect("a saved state");
        record.source_hash ^= 0x5a5a_5a5a;
        reader_cache::files::write_state_file(root, record).expect("rewrite it");
    });

    let device = Device::wake(&card);
    assert_eq!(
        device.app.font_size,
        FontSize::Large,
        "the defaults would be written over the reader's settings at the next save: {:?}",
        device.log
    );
}

/// The device's own sequence: moved into a folder, then back out again,
/// opening it each time. Each move carries the pagination, the place and the
/// state, and each open lands on the page from the cache.
#[test]
fn a_book_moved_there_and_back_opens_on_its_page_each_time() {
    let (card, pages) = read_and_sleep(7);
    card.rename(HOME, MOVED);

    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("86");
    device.choose("MOVED");
    device.choose(BOOK);
    assert_eq!((device.app.view, device.app.page), (AppView::Reading, 7));
    device.turn(3);
    device.sleep();

    card.rename(MOVED, HOME);
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("86");
    device.choose(BOOK);
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    assert!(!device.saw(|event| matches!(event, LibraryEvent::BookOpenFailed { .. })));
    match first_open(&device) {
        LibraryEvent::Loaded {
            pages: opened,
            position,
            ..
        } => {
            assert_eq!(opened, pages, "from the cache, carried twice");
            assert_eq!(position, Some(10), "on the page read to in the moved copy");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(device.app.font_size, FontSize::Large);
}

/// The catalog snapshot can be gone at boot: an upload session invalidates
/// it, and so can an interrupted scan. Then the boot rescans before it
/// restores, and the saved state has to name the book where the move left it,
/// or the restore finds nothing and the reader's book is no longer the one
/// Home continues.
#[test]
fn a_moved_book_is_still_the_one_home_continues_after_a_boot_that_rescans() {
    let (card, pages) = read_and_sleep(6);
    card.rename(HOME, MOVED);
    card.delete("READER/CATALOG.BIN");

    let mut device = Device::wake(&card);
    assert_eq!(device.app.view, AppView::Home);
    device.press(app_core::Button::Confirm);
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    assert!(!device.saw(|event| matches!(event, LibraryEvent::BookOpenFailed { .. })));
    let opened = device
        .log
        .iter()
        .find(|event| matches!(event, LibraryEvent::Loaded { .. }))
        .copied()
        .expect("Home continued a book");
    match opened {
        LibraryEvent::Loaded {
            pages: opened,
            position,
            ..
        } => {
            assert_eq!(opened, pages, "the moved book, from its carried pagination");
            assert_eq!(position, Some(6), "on the reader's page");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(device.app.font_size, FontSize::Large);
}
