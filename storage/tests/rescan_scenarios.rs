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
/// runs only after a backoff that grows with each refusal, instead of putting
/// it straight back on the queue. Once the card takes writes, the refresh
/// scans and the page is kept.
#[test]
fn a_refresh_over_a_refusing_card_waits_for_a_background_slice() {
    let (card, mut device) = second_book_read_with_a_book_added();
    let before = device.log.len();
    card.disk.refuse_next_writes(u32::MAX);
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

    // Each refused retry backs off further than the last.
    for refusals in 2..=3 {
        assert!(device.task.catalog_refresh.owed);
        device.step_background();
        device.run_queued();
        assert_eq!(device.task.background_attempts(), refusals);
    }
    card.disk.refuse_next_writes(0);
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

/// A refresh is owed when a stale-row pick scans first. That scan settles the
/// refresh, so no background slice scans again.
#[test]
fn a_scan_by_a_pick_settles_an_owed_refresh() {
    let (card, mut device) = second_book_read_with_a_book_added();
    to_library_root(&mut device);
    device.point_at(ADDED);
    card.disk.refuse_next_writes(1);
    device.send(StorageCommand::RefreshCatalog);
    device.run_queued();
    assert!(device.task.catalog_refresh.owed);

    let before = device.log.len();
    device.press(Button::Confirm);
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    let scans = device.log[before..]
        .iter()
        .filter(|event| matches!(event, LibraryEvent::Scanned { .. }))
        .count();
    assert_eq!(scans, 1, "one scan, not a second for the owed refresh");
    assert!(!device.task.catalog_refresh.owed);
}

/// A refresh is owed when a stale-row pick scans, and that scan fails on the
/// card. The refresh stays owed, and a later background slice retries it.
#[test]
fn a_failed_scan_by_a_pick_leaves_the_refresh_owed() {
    let (card, mut device) = second_book_read_with_a_book_added();
    to_library_root(&mut device);
    device.point_at(ADDED);
    card.disk.refuse_next_writes(1);
    device.send(StorageCommand::RefreshCatalog);
    device.run_queued();
    assert!(device.task.catalog_refresh.owed);
    // Page 9 lands now, so the pick has nothing to write before its scan,
    // and every write the scan makes is refused.
    assert!(device
        .task
        .flush_pending_progress(&mut device.card, &mut device.store));
    let books = device.store.catalog_count();
    card.disk.refuse_next_writes(u32::MAX);

    let before = device.log.len();
    device.press_only(Button::Confirm);
    device.run_queued();
    assert!(
        device.log[before..]
            .iter()
            .any(|event| matches!(event, LibraryEvent::RowFailed { .. })),
        "{:?}",
        &device.log[before..]
    );
    assert_ne!(
        device.store.catalog_count(),
        books + 1,
        "the scan did not land"
    );
    assert!(
        device.task.catalog_refresh.owed,
        "the refresh is still owed"
    );

    card.disk.refuse_next_writes(0);
    device.settle();
    assert!(!device.task.catalog_refresh.owed);
    assert_eq!(
        device.store.catalog_count(),
        books + 1,
        "the retried refresh catalogued the added book"
    );
}

/// The owed refresh's own retry gets past the write and its scan fails. It
/// settles as any refresh does: no further retry, and no scan once the card
/// answers again.
#[test]
fn a_retried_refresh_whose_scan_fails_settles_like_any_refresh() {
    let (card, mut device) = second_book_read_with_a_book_added();
    card.disk.refuse_next_writes(1);
    device.send(StorageCommand::RefreshCatalog);
    device.run_queued();
    assert!(device.task.catalog_refresh.owed);
    assert!(device
        .task
        .flush_pending_progress(&mut device.card, &mut device.store));
    let books = device.store.catalog_count();
    card.disk.refuse_next_writes(u32::MAX);

    let before = device.log.len();
    device.settle();
    assert!(
        device.log[before..]
            .iter()
            .any(|event| matches!(event, LibraryEvent::Scanned { .. })),
        "the background slice ran the refresh: {:?}",
        &device.log[before..]
    );
    assert_ne!(device.store.catalog_count(), books + 1, "its scan failed");
    assert!(!device.task.catalog_refresh.owed, "and it is settled");
    assert_eq!(
        device.task.catalog_refresh.refusals, 0,
        "with its refusals spent"
    );

    card.disk.refuse_next_writes(0);
    let before = device.log.len();
    device.settle();
    assert!(
        !device.log[before..]
            .iter()
            .any(|event| matches!(event, LibraryEvent::Scanned { .. })),
        "nothing retries it"
    );

    // A later refresh refused once backs off as a first refusal.
    device.turn(1);
    card.disk.refuse_next_writes(1);
    device.send(StorageCommand::RefreshCatalog);
    device.run_queued();
    assert!(device.task.catalog_refresh.owed);
    assert_eq!(device.task.background_attempts(), 1);
}

/// From the library root, open `name` in `folder` once the card takes writes
/// again, and check no scan was needed to find it.
fn opens_from_the_kept_catalog(device: &mut Device, folder: &str, name: &str) {
    let before = device.log.len();
    device.choose(folder);
    device.choose(name);
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    assert!(
        !device.log[before..]
            .iter()
            .any(|event| matches!(event, LibraryEvent::Scanned { .. })),
        "the catalog still named the book: {:?}",
        &device.log[before..]
    );
}

/// A stale-row pick scans, and the card refuses every write the scan makes.
/// The reader stays on the catalog it had, under the same epoch, and opens
/// its books without another scan.
#[test]
fn a_failed_scan_by_a_pick_keeps_the_old_catalog() {
    let (card, mut device) = second_book_read_with_a_book_added();
    to_library_root(&mut device);
    device.point_at(ADDED);
    // Page 9 lands now, so every write refused below is the scan's.
    assert!(device
        .task
        .flush_pending_progress(&mut device.card, &mut device.store));
    let books = device.store.catalog_count();
    let epoch = device.store.catalog_epoch();
    assert_eq!(books, 2);
    card.disk.refuse_next_writes(u32::MAX);

    let before = device.log.len();
    device.press_only(Button::Confirm);
    device.run_queued();
    assert!(
        device.log[before..]
            .iter()
            .any(|event| matches!(event, LibraryEvent::RowFailed { .. })),
        "{:?}",
        &device.log[before..]
    );
    assert_eq!(device.store.catalog_count(), books, "the old catalog stays");
    assert_eq!(device.store.catalog_epoch(), epoch, "and is not renumbered");
    assert_eq!(device.app.catalog_epoch, epoch);

    card.disk.refuse_next_writes(0);
    device.settle();
    while device.app.library_depth > 0 {
        device.press(Button::Back);
    }
    opens_from_the_kept_catalog(&mut device, "Shelf", FIRST);
}

/// A refresh with no position owed, over a card that refuses every write.
/// The scan fails, and the reader keeps the catalog it booted with.
#[test]
fn a_refresh_over_a_refusing_card_keeps_the_old_catalog() {
    let card = Card::blank();
    card.put(&format!("BOOKS/Shelf/{FIRST}"), &epub("Alpha", 6, 1));
    card.put(&format!("BOOKS/Shelf/{SECOND}"), &epub("Beta", 6, 2));
    Device::wake(&card).sleep();

    let mut device = Device::wake(&card);
    let books = device.store.catalog_count();
    let epoch = device.store.catalog_epoch();
    assert_eq!(books, 2);
    card.disk.refuse_next_writes(u32::MAX);
    device.send(StorageCommand::RefreshCatalog);
    device.run_queued();
    assert_eq!(device.store.catalog_count(), books, "the old catalog stays");
    assert_eq!(device.store.catalog_epoch(), epoch, "and is not renumbered");
    assert_eq!(device.app.catalog_epoch, epoch);

    card.disk.refuse_next_writes(0);
    device.settle();
    device.open_library();
    opens_from_the_kept_catalog(&mut device, "Shelf", SECOND);
}

/// A refresh that lands replaces the catalog, so its rows are a new epoch
/// even when it lists as many books as the one it replaced.
#[test]
fn a_landed_refresh_moves_the_epoch_with_the_count_unchanged() {
    let card = Card::blank();
    card.put(&format!("BOOKS/Shelf/{FIRST}"), &epub("Alpha", 6, 1));
    card.put(&format!("BOOKS/Shelf/{SECOND}"), &epub("Beta", 6, 2));
    Device::wake(&card).sleep();

    let mut device = Device::wake(&card);
    let epoch = device.store.catalog_epoch();
    device.send(StorageCommand::RefreshCatalog);
    device.run_queued();
    assert_eq!(device.store.catalog_count(), 2);
    assert_ne!(device.store.catalog_epoch(), epoch);
    assert_eq!(device.app.catalog_epoch, device.store.catalog_epoch());
}

/// A refresh over a card that fails after each number of landed writes in
/// turn. Until the old CATALOG.BIN is truncated the reader keeps it and opens
/// from it; after that the library is empty, never rows the card cannot back.
#[test]
fn a_refresh_failing_partway_keeps_the_old_catalog_only_while_the_card_does() {
    let (mut kept, mut cleared) = (0, 0);
    for landed in 0.. {
        let card = Card::blank();
        card.put(&format!("BOOKS/Shelf/{FIRST}"), &epub("Alpha", 6, 1));
        card.put(&format!("BOOKS/Shelf/{SECOND}"), &epub("Beta", 6, 2));
        Device::wake(&card).sleep();
        card.put(ADDED, &epub("Added", 4, 3));

        let mut device = Device::wake(&card);
        let epoch = device.store.catalog_epoch();
        assert_eq!(device.store.catalog_count(), 2);
        card.disk.fail_after_writes(landed);
        device.send(StorageCommand::RefreshCatalog);
        device.run_queued();
        match device.store.catalog_count() {
            3 => break,
            2 => {
                kept += 1;
                assert_eq!(device.store.catalog_epoch(), epoch, "after {landed} writes");
                card.disk.refuse_next_writes(0);
                device.settle();
                device.open_library();
                opens_from_the_kept_catalog(&mut device, "Shelf", FIRST);
            }
            count => {
                cleared += 1;
                assert_eq!(count, 0, "after {landed} writes");
            }
        }
    }
    assert!(kept > 0, "some failures came before the truncation");
    assert!(cleared > 0, "and some after it");
}
