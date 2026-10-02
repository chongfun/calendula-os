//! A computer adds a book while the device is off, and the rescan that finds
//! it renumbers the catalog under the book being read.
//!
//! Tests run the real storage task over a FAT image and the real reducer over
//! its events, through power cycles that keep only the card. See `support`.

mod support;

use app_core::{AppView, Button, LibraryEvent, StorageCommand};
use support::{epub, Card, Device};

const FIRST: &str = "Alpha.epub";
const SECOND: &str = "Beta.epub";
const ADDED: &str = "Added.epub";

/// From a book, back to the library root.
fn to_library_root(device: &mut Device) {
    // Portrait: the first Back shows the key sheet, the second acts on it.
    device.press(Button::Back);
    device.press(Button::Back);
    device.open_library();
    while device.app.library_depth > 0 {
        device.press(Button::Back);
    }
    assert_eq!(device.app.view, AppView::Library);
}

/// Open `name` from `folder` after a fresh boot, and return the device on it.
fn open_after_boot(card: &Card, folder: &str, name: &str) -> Device {
    let mut device = Device::wake(card);
    device.open_library();
    device.choose(folder);
    device.choose(name);
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    device
}

/// A folder of two books, then a book added at the card root while the
/// device is off. The reader opens the second folder book and reads to page
/// 9, which is still coalesced: the card holds an older page.
fn second_book_read_with_a_book_added() -> (Card, Device) {
    let card = Card::blank();
    card.put(&format!("BOOKS/Shelf/{FIRST}"), &epub("Alpha", 6, 1));
    card.put(&format!("BOOKS/Shelf/{SECOND}"), &epub("Beta", 6, 2));
    Device::wake(&card).sleep();
    card.put(ADDED, &epub("Added", 4, 3));

    let mut device = open_after_boot(&card, "Shelf", SECOND);
    device.turn(9);
    assert_eq!(device.app.page, 9);
    assert_eq!(
        device.task.pending_progress.map(|record| record.screen),
        Some(9),
        "page 9 is still coalesced when the reader leaves"
    );
    (card, device)
}

/// The recipe owed by the departing-book close-out on the X3. A computer adds
/// a book at the card root while the device is off, so the boot keeps a
/// catalog that lacks it. The reader opens the second book of a folder and
/// turns pages, then picks the added book. That pick rescans, and card-root
/// books number first, so the second book's row now names the first. The
/// open then closes the second book out against the renumbered catalog. Its
/// page has to reach it, not the book now holding its old row.
#[test]
fn a_rescan_under_the_open_book_saves_its_page_to_it() {
    let (card, mut device) = second_book_read_with_a_book_added();
    let row_before = device.app.book_id;
    to_library_root(&mut device);
    device.choose(ADDED);
    assert!(
        device.saw(|event| matches!(event, LibraryEvent::Scanned { .. })),
        "the pick found the catalog behind the card and rescanned: {:?}",
        device.log
    );
    assert!(
        !device.saw(|event| matches!(event, LibraryEvent::BookOpenFailed { .. })),
        "closing out the departing book did not refuse the open: {:?}",
        device.log
    );
    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!(device.app.page, 0, "the added book opens at its start");
    device.sleep();

    let device = open_after_boot(&card, "Shelf", FIRST);
    assert_eq!(
        device.app.book_id, row_before,
        "the first book now holds the second's old row, so this tests the renumbering"
    );
    assert_eq!(
        device.app.page, 0,
        "the book now at the second's old row was not given its page"
    );
    device.sleep();

    let device = open_after_boot(&card, "Shelf", SECOND);
    assert_eq!(device.app.page, 9, "the second book kept its page");
}

/// The write before the rescan is refused. The scan would drop the pages that
/// turn page 9 into a place, so the pick is refused instead, and the next pick
/// writes the page before it scans.
#[test]
fn a_refused_write_before_the_rescan_refuses_the_pick() {
    let (card, mut device) = second_book_read_with_a_book_added();
    to_library_root(&mut device);
    device.point_at(ADDED);
    let before = device.log.len();
    card.disk.refuse_next_writes(1);
    device.press(Button::Confirm);
    let events = &device.log[before..];
    assert!(
        events
            .iter()
            .any(|event| matches!(event, LibraryEvent::RowFailed { .. })),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, LibraryEvent::Scanned { .. })),
        "no scan dropped the pages: {events:?}"
    );
    assert_eq!(device.app.view, AppView::Library);
    assert_eq!(
        device.task.pending_progress.map(|record| record.screen),
        Some(9),
        "page 9 is still owed"
    );

    device.choose(ADDED);
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    device.sleep();

    let device = open_after_boot(&card, "Shelf", SECOND);
    assert_eq!(device.app.page, 9, "the second book kept its page");
}

/// A catalog refresh while the card keeps refusing the position write. Each
/// refusal leaves the refresh owed to a background slice, which the firmware
/// runs only after a backoff, instead of putting it straight back on the
/// queue. Once the card takes writes, the refresh scans and the page is kept.
#[test]
fn a_refresh_over_a_refusing_card_waits_for_a_background_slice() {
    let (card, mut device) = second_book_read_with_a_book_added();
    let before = device.log.len();
    card.disk.refuse_next_writes(3);
    device.send(StorageCommand::RefreshCatalog);
    device.run_queued();
    let scanned = |device: &Device| {
        device.log[before..]
            .iter()
            .any(|event| matches!(event, LibraryEvent::Scanned { .. }))
    };
    assert!(
        !scanned(&device),
        "no scan dropped the pages: {:?}",
        device.log
    );
    assert!(
        device.host.requeued.is_empty(),
        "not requeued straight away"
    );
    assert!(device.task.catalog_refresh.owed);
    assert_eq!(device.task.background_attempts(), 1, "the slice backs off");
    assert_eq!(
        device.task.pending_progress.map(|record| record.screen),
        Some(9),
        "page 9 is still owed"
    );

    device.settle();
    assert!(
        scanned(&device),
        "the refresh ran once the card took writes: {:?}",
        device.log
    );
    assert!(!device.task.catalog_refresh.owed);
    assert_eq!(device.task.catalog_refresh.refusals, 0);
    assert_eq!(device.task.pending_progress, None);
    device.sleep();

    let device = open_after_boot(&card, "Shelf", SECOND);
    assert_eq!(device.app.page, 9, "the second book kept its page");
}
