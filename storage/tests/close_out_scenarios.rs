//! A save closes the book out after something dropped its resident pages.
//!
//! The place a save writes is the anchor of the page on screen, read from the
//! resident pages. A catalog scan borrows that arena, and PR #107 flushes the
//! coalesced page before one, but any other path that drops the pages first
//! reaches the close-out with no anchor in RAM. The card holds an older place
//! by then, and the open reads the place before the page-keyed position.
//!
//! Tests run the real storage task over a FAT image and the real reducer over
//! its events, through power cycles that keep only the card. See `support`.

mod support;

use app_core::{AppView, Button, LibraryEvent};
use proto::anchor::ContentAnchor;
use proto::identity::BookId;
use reader_cache::files::{read_place, PlaceRead};
use support::{epub, Card, Device};

const FIRST: &str = "Alpha.epub";
const SECOND: &str = "Beta.epub";

fn open(device: &mut Device, name: &str) {
    if device.app.view == AppView::Reading {
        // Portrait: the first Back shows the key sheet, the second acts on it.
        device.press(Button::Back);
        device.press(Button::Back);
    }
    device.open_library();
    if device.app.library_depth == 0 {
        device.choose("Shelf");
    }
    device.choose(name);
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
}

fn open_after_boot(card: &Card, name: &str) -> Device {
    let mut device = Device::wake(card);
    open(&mut device, name);
    device
}

fn stored_place(card: &Card, id: BookId) -> PlaceRead {
    card.session(|root| read_place(root, id))
}

struct Read {
    card: Card,
    device: Device,
    id: BookId,
    /// Where page 9 starts, taken while its page was resident.
    page_nine: ContentAnchor,
}

/// The second book of a folder read to page 9. The first turn wrote a place
/// and the rest coalesced, so the card's place is older than page 9. Then the
/// resident pages are dropped, as a scan borrowing the arena drops them.
fn read_to_page_nine_then_drop_the_pages() -> Read {
    let card = Card::blank();
    card.put(&format!("BOOKS/Shelf/{FIRST}"), &epub("Alpha", 6, 1));
    card.put(&format!("BOOKS/Shelf/{SECOND}"), &epub("Beta", 6, 2));
    let mut device = open_after_boot(&card, SECOND);
    device.turn(9);
    assert_eq!(device.app.page, 9);
    assert_eq!(
        device.task.pending_progress.map(|record| record.screen),
        Some(9),
        "page 9 is still coalesced"
    );
    let id = device
        .store
        .loaded_book_snapshot()
        .and_then(|loaded| loaded.copy_id)
        .expect("the copy has an id");
    let page_nine = device
        .store
        .anchor_for_global_page(9)
        .expect("page 9 is resident");
    match stored_place(&card, id) {
        PlaceRead::Found(place) => assert!(place.anchor < page_nine, "the card's place is older"),
        _ => panic!("the first turn wrote a place"),
    }
    device.store.clear_lines();
    assert_eq!(device.store.anchor_for_global_page(9), None);
    Read {
        card,
        device,
        id,
        page_nine,
    }
}

/// The flush before sleep closes the book out with its pages gone. The anchor
/// comes back from the section file on the card.
#[test]
fn a_save_after_the_pages_drop_writes_the_newer_place() {
    let Read {
        card,
        device,
        id,
        page_nine,
    } = read_to_page_nine_then_drop_the_pages();
    device.sleep();
    match stored_place(&card, id) {
        PlaceRead::Found(place) => assert_eq!(place.anchor, page_nine),
        _ => panic!("the place survives the save"),
    }

    let device = open_after_boot(&card, SECOND);
    assert_eq!(device.app.page, 9, "the reopen lands on the newer page");
}

/// Picking another book closes the departing one out with its pages gone. The
/// switch goes through, and the departing book keeps its page.
#[test]
fn a_switch_after_the_pages_drop_keeps_the_newer_page() {
    let Read {
        card,
        mut device,
        id,
        page_nine,
    } = read_to_page_nine_then_drop_the_pages();
    open(&mut device, FIRST);
    assert!(
        !device.saw(|event| matches!(event, LibraryEvent::BookOpenFailed { .. })),
        "the close-out did not refuse the open: {:?}",
        device.log
    );
    assert_eq!(device.app.page, 0, "the other book opens at its start");
    device.sleep();
    match stored_place(&card, id) {
        PlaceRead::Found(place) => assert_eq!(place.anchor, page_nine),
        _ => panic!("the place survives the switch"),
    }

    let device = open_after_boot(&card, SECOND);
    assert_eq!(device.app.page, 9, "the reopen lands on the newer page");
}

/// The section file for page 9 is gone too, so no anchor can be had. The
/// switch still goes through, and the older place is removed so the page-keyed
/// position, which holds page 9, answers the reopen.
#[test]
fn a_switch_with_no_anchor_to_be_had_removes_the_older_place() {
    let Read {
        card,
        mut device,
        id,
        ..
    } = read_to_page_nine_then_drop_the_pages();
    let identity = device
        .store
        .loaded_book_snapshot()
        .expect("the book is loaded")
        .identity;
    let sections = format!(
        "READER/CACHE2/{}/SECTIONS",
        proto::cache::cache_key_from(identity.0)
    );
    let names = card.list(&sections);
    assert!(!names.is_empty(), "the book has section files");
    for name in names {
        card.delete(&format!("{sections}/{name}"));
    }

    open(&mut device, FIRST);
    assert!(
        !device.saw(|event| matches!(event, LibraryEvent::BookOpenFailed { .. })),
        "the close-out did not refuse the open: {:?}",
        device.log
    );
    assert_eq!(device.app.page, 0, "the other book opens at its start");
    device.sleep();
    assert!(
        matches!(stored_place(&card, id), PlaceRead::Absent),
        "the older place is gone"
    );

    let mut device = open_after_boot(&card, SECOND);
    assert_eq!(device.app.page, 9, "the reopen lands on the newer page");
    // The next save writes a place again, from the pages now resident.
    device.turn(1);
    device.sleep();
    assert!(matches!(stored_place(&card, id), PlaceRead::Found(_)));
}
