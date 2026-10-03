//! A computer adds a book while the device is off, and the rescan that finds
//! it renumbers the catalog under the book being read.
//!
//! Tests run the real storage task over a FAT image and the real reducer over
//! its events, through power cycles that keep only the card. See `support`.

mod support;

use app_core::storage_loop::SleepRefusal;
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

/// The rescan a stale pick owes takes seconds on a large library, so the
/// Library says so first. The note is announced before the scan, is on the
/// frame the firmware paints while it still has the bus, and is gone once the
/// scan lands. A pick the catalog already knows announces nothing.
#[test]
fn a_pick_that_rescans_says_so_before_the_scan_and_clears_after() {
    let (_card, mut device) = second_book_read_with_a_book_added();
    to_library_root(&mut device);
    let before = device.log.len();
    device.choose(ADDED);
    let events = &device.log[before..];
    let at = |wanted: fn(&LibraryEvent) -> bool| {
        events
            .iter()
            .position(wanted)
            .unwrap_or_else(|| panic!("missing event: {events:?}"))
    };
    let rescanning = at(|event| matches!(event, LibraryEvent::Rescanning { .. }));
    let scanned = at(|event| matches!(event, LibraryEvent::Scanned { .. }));
    let answered = at(|event| matches!(event, LibraryEvent::RowIsBook { .. }));
    assert!(
        rescanning < scanned && scanned < answered,
        "announced, scanned, answered: {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, LibraryEvent::Rescanning { .. }))
            .count(),
        1,
        "{events:?}"
    );

    let [Some(frame)] = device.before_rescan[..] else {
        panic!("one frame before one rescan: {:?}", device.before_rescan);
    };
    assert_eq!(frame.view, AppView::Library);
    assert!(
        frame.library_rescanning,
        "the note is on screen as the scan starts"
    );
    assert!(frame.library_move_pending, "and the list is held");

    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    assert!(!device.app.library_browse.rescanning(), "the note is gone");

    // The catalog knows every book now, so a pick goes straight to its book.
    to_library_root(&mut device);
    let before = device.log.len();
    device.choose(ADDED);
    assert_eq!(device.app.view, AppView::Reading);
    assert!(
        !device.log[before..]
            .iter()
            .any(|event| matches!(event, LibraryEvent::Rescanning { .. })),
        "{:?}",
        &device.log[before..]
    );
    assert_eq!(device.before_rescan.len(), 1);
}

/// The note's refresh lets the app run before the scan, and Back is the one
/// press a waiting pick takes. The reader presses it, then Continue on Home,
/// which asks for the book being read by the row it holds in this catalog,
/// unfenced. The scan would put the added book ahead of it, so that open
/// would land on the first book under the second's number. Instead the pick
/// walked away from is refused without a scan, Continue opens the same book
/// at its page, and the next pick the reader waits for scans.
#[test]
fn back_during_the_note_skips_the_scan_so_continue_opens_the_same_book() {
    let (card, mut device) = second_book_read_with_a_book_added();
    let row_before = device.app.book_id;
    let identity_before = device
        .store
        .loaded_book_identity(row_before)
        .expect("the second book is loaded");
    to_library_root(&mut device);
    device.point_at(ADDED);
    let before = device.log.len();
    device.press_only(Button::Confirm);
    device.handle_one_with(|device| {
        assert!(device.app.library_browse.rescanning(), "the note is up");
        device.press_only(Button::Back);
        assert_eq!(device.app.view, AppView::Home);
        assert!(device.app.library_browse.is_idle());
        device.press_only(Button::Confirm);
        assert_eq!(device.app.view, AppView::Reading, "continue reading");
    });
    device.settle();
    let events = &device.log[before..];
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, LibraryEvent::Scanned { .. })),
        "no scan renumbered the catalog under the open: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, LibraryEvent::RowFailed { .. })),
        "the pick walked away from is still answered: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, LibraryEvent::BookOpenFailed { .. })),
        "{events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            LibraryEvent::Loaded { book_id, .. } if *book_id == row_before
        )),
        "continue opened the book by its row: {events:?}"
    );
    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!(device.app.book_id, row_before);
    assert_eq!(
        device.store.loaded_book_identity(row_before),
        Some(identity_before),
        "and that row still named the same book"
    );
    assert_eq!(device.app.page, 9);
    assert!(
        device.before_rescan.len() == 1,
        "{:?}",
        device.before_rescan
    );

    // A pick the reader waits for scans as before.
    to_library_root(&mut device);
    let before = device.log.len();
    device.choose(ADDED);
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    assert!(device.log[before..]
        .iter()
        .any(|event| matches!(event, LibraryEvent::Scanned { .. })));
    device.sleep();

    // That scan did renumber: the second book no longer holds its old row.
    let device = open_after_boot(&card, "Shelf", SECOND);
    assert_ne!(device.app.book_id, row_before);
    assert_eq!(device.app.page, 9, "the second book kept its page");
}

/// A press the waiting pick swallows during the note changes nothing, so the
/// pick is still waited on and its scan runs.
#[test]
fn a_press_the_pick_swallows_during_the_note_leaves_its_scan_to_run() {
    let (_card, mut device) = second_book_read_with_a_book_added();
    to_library_root(&mut device);
    device.point_at(ADDED);
    let before = device.log.len();
    device.press_only(Button::Confirm);
    device.handle_one_with(|device| {
        device.press_only(Button::Next);
        assert!(
            device.app.library_browse.rescanning(),
            "the press was swallowed"
        );
    });
    device.settle();
    assert!(
        device.log[before..]
            .iter()
            .any(|event| matches!(event, LibraryEvent::Scanned { .. })),
        "{:?}",
        &device.log[before..]
    );
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    assert_eq!(device.app.page, 0, "the added book opened at its start");
}

/// Back during the note and a fresh pick of the same row: the first pick is
/// refused unheard, and the second, which the reader waits for, scans once
/// and opens the book.
#[test]
fn a_newer_pick_during_the_note_gets_the_scan_in_place_of_the_older() {
    let (_card, mut device) = second_book_read_with_a_book_added();
    to_library_root(&mut device);
    device.point_at(ADDED);
    let row = device.app.selection;
    let before = device.log.len();
    device.press_only(Button::Confirm);
    let first_pick = device
        .app
        .library_browse
        .request_id()
        .expect("a pick in flight");
    device.handle_one_with(|device| {
        device.press_only(Button::Back);
        device.press_only(Button::Back);
        assert_eq!(device.app.view, AppView::Library);
        for _ in 0..row {
            device.press_only(Button::Next);
        }
        assert_eq!(device.app.selection, row);
        device.press_only(Button::Confirm);
        assert_ne!(device.app.library_browse.request_id(), Some(first_pick));
    });
    device.settle();
    let events = &device.log[before..];
    assert!(
        events.iter().any(|event| matches!(
            event,
            LibraryEvent::RowFailed { request_id } if *request_id == first_pick
        )),
        "{events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, LibraryEvent::Scanned { .. }))
            .count(),
        1,
        "one scan, for the pick still waited on: {events:?}"
    );
    assert_eq!(
        device.before_rescan.len(),
        2,
        "a note before each pick's rescan"
    );
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    assert_eq!(device.app.page, 0, "the added book opened at its start");
}

/// Back pressed while the note is painting, seen from the loop. The note's
/// flush yields, so the app has the Home frame queued by the time it settles,
/// and the loop paints that frame before it reaches the scan it still owes:
/// the frame on screen as the pick's second half runs is Home, not the note.
/// Storage stands down in between, so nothing Back queued runs ahead of it.
/// The second half then finds the pick walked away from and refuses it
/// without a scan; the next pick the reader waits for scans.
#[test]
fn back_during_the_note_is_painted_before_the_loop_runs_the_owed_rescan() {
    let (_card, mut device) = second_book_read_with_a_book_added();
    to_library_root(&mut device);
    let books = device.store.catalog_count();
    device.point_at(ADDED);
    device.press_only(Button::Confirm);
    device.run_queued();
    assert!(device.rescan_owed(), "the pick left its scan to the loop");
    assert!(device.app.library_browse.rescanning());
    let note = device.frame().expect("a frame");
    assert_eq!(note.view, AppView::Library);
    assert!(note.library_rescanning, "the note is on the glass");

    // Back lands while the plate settles: the app reduces it and queues Home.
    device.press_only(Button::Back);
    assert_eq!(device.app.view, AppView::Home);
    assert!(device.app.library_browse.is_idle(), "the wait went with it");
    device.run_queued();
    assert!(
        device.rescan_owed(),
        "storage stands down until the pick's second half has run"
    );

    let before = device.log.len();
    device.rescan();
    let frame = device
        .before_rescan
        .last()
        .copied()
        .flatten()
        .expect("a frame");
    assert_eq!(
        frame.view,
        AppView::Home,
        "Home was painted before the scan"
    );
    assert!(!frame.library_rescanning);
    let events = &device.log[before..];
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, LibraryEvent::Scanned { .. })),
        "no scan for a pick walked away from: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, LibraryEvent::RowFailed { .. })),
        "which is still answered: {events:?}"
    );
    assert_eq!(device.app.view, AppView::Home, "{events:?}");
    assert_eq!(
        device.store.catalog_count(),
        books,
        "the catalog is untouched"
    );
    device.settle();
    assert_eq!(device.app.view, AppView::Home);

    // A pick the reader waits for scans, with the note on the glass.
    device.open_library();
    let before = device.log.len();
    device.choose(ADDED);
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    assert!(device.log[before..]
        .iter()
        .any(|event| matches!(event, LibraryEvent::Scanned { .. })));
    let [_, Some(frame)] = device.before_rescan[..] else {
        panic!("two second halves: {:?}", device.before_rescan);
    };
    assert!(frame.library_rescanning);
}

/// Power pressed with the pick still queued, ahead of the storage task. The
/// drain before sleep handles it, and the rescan it would owe is refused the
/// same way: no scan, the catalog untouched, and the next boot's pick scans.
#[test]
fn power_with_the_pick_still_queued_refuses_it_in_the_drain() {
    let (card, mut device) = second_book_read_with_a_book_added();
    to_library_root(&mut device);
    let books = device.store.catalog_count();
    device.point_at(ADDED);
    device.press_only(Button::Confirm);
    let pick = device
        .app
        .library_browse
        .request_id()
        .expect("a pick waits");
    assert!(!device.rescan_owed(), "the pick is still queued");

    let before = device.log.len();
    assert_eq!(device.power_pressed(), None, "the sleep proceeds");
    let events = &device.log[before..];
    assert!(
        events.contains(&LibraryEvent::RowFailed { request_id: pick }),
        "the pick is refused: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, LibraryEvent::Scanned { .. })),
        "no scan: {events:?}"
    );
    assert!(!device.rescan_owed());
    assert_eq!(
        device.store.catalog_count(),
        books,
        "the catalog is untouched"
    );
    assert!(device.app.library_browse.is_idle());
    assert!(device.before_rescan.is_empty());

    let mut device = Device::wake(&card);
    device.open_library();
    let before = device.log.len();
    device.choose(ADDED);
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    assert!(
        device.log[before..]
            .iter()
            .any(|event| matches!(event, LibraryEvent::Scanned { .. })),
        "the next boot's pick rescans: {:?}",
        &device.log[before..]
    );
}

/// An upload request queued ahead of the stale pick is not the drain's to
/// answer: it goes back on the queue and the sleep is refused, with the pick
/// behind it untouched, neither scanned nor refused. The upload session owns
/// the sleep from there.
#[test]
fn an_upload_queued_before_the_pick_refuses_the_sleep_and_leaves_the_pick_queued() {
    let (_card, mut device) = second_book_read_with_a_book_added();
    to_library_root(&mut device);
    let books = device.store.catalog_count();
    device.point_at(ADDED);
    device.send(StorageCommand::ReceiveUpload);
    device.press_only(Button::Confirm);
    let pick = device
        .app
        .library_browse
        .request_id()
        .expect("a pick waits");

    let before = device.log.len();
    assert_eq!(
        device.power_pressed(),
        Some(SleepRefusal::UploadQueued),
        "the upload request is put back and the sleep refused"
    );
    let events = &device.log[before..];
    assert!(
        !events.iter().any(|event| matches!(
            event,
            LibraryEvent::RowFailed { .. } | LibraryEvent::Scanned { .. }
        )),
        "the pick behind the upload is neither refused nor scanned: {events:?}"
    );
    assert_eq!(device.app.library_browse.request_id(), Some(pick));
    assert_eq!(device.store.catalog_count(), books);
    assert!(!device.rescan_owed());
}

/// Power pressed while the note is painting. Sleep is terminal, so the pick
/// is refused instead of scanned: no scan holds the note on the panel ahead
/// of the sleep image, and a sleep a late press abandons finds the Library
/// with nothing waiting. The catalog still lacks the book, so the next boot's
/// pick rescans.
#[test]
fn power_during_the_note_refuses_the_pick_instead_of_scanning() {
    let (card, mut device) = second_book_read_with_a_book_added();
    to_library_root(&mut device);
    let books = device.store.catalog_count();
    device.point_at(ADDED);
    device.press_only(Button::Confirm);
    device.run_queued();
    assert!(device.rescan_owed());
    let pick = device
        .app
        .library_browse
        .request_id()
        .expect("a pick waits");

    let before = device.log.len();
    assert_eq!(device.power_pressed(), None, "the sleep proceeds");
    let events = &device.log[before..];
    assert!(
        events.contains(&LibraryEvent::RowFailed { request_id: pick }),
        "the pick is refused: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, LibraryEvent::Scanned { .. })),
        "no scan: {events:?}"
    );
    assert!(!device.rescan_owed());
    assert_eq!(
        device.store.catalog_count(),
        books,
        "the catalog is untouched"
    );
    // An abandoned sleep returns here: the Library, with nothing waiting.
    assert_eq!(device.app.view, AppView::Library);
    assert!(device.app.library_browse.is_idle());
    assert!(!device.frame().expect("a frame").library_rescanning);
    assert!(device.before_rescan.is_empty());

    let mut device = Device::wake(&card);
    device.open_library();
    let before = device.log.len();
    device.choose(ADDED);
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    assert!(
        device.log[before..]
            .iter()
            .any(|event| matches!(event, LibraryEvent::Rescanning { .. })),
        "the pick refused before sleep still owes its scan: {:?}",
        &device.log[before..]
    );
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
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, LibraryEvent::Rescanning { .. })),
        "no note promised a scan that did not run: {events:?}"
    );
    assert!(device.before_rescan.is_empty());
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
    // The loop runs the scan the pick left owed, after its wait.
    device.rescan();
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
/// The reader stays on the catalog it had, reloaded from the card under a new
/// epoch, and opens its books without another scan.
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
    assert_eq!(books, 2);
    card.disk.refuse_next_writes(u32::MAX);

    let before = device.log.len();
    device.press_only(Button::Confirm);
    device.run_queued();
    // The pick paints its note and leaves the scan to the loop.
    device.rescan();
    assert!(
        device.log[before..]
            .iter()
            .any(|event| matches!(event, LibraryEvent::RowFailed { .. })),
        "{:?}",
        &device.log[before..]
    );
    assert_eq!(device.store.catalog_count(), books, "the old catalog stays");
    assert_eq!(
        device.app.catalog_epoch,
        device.store.catalog_epoch(),
        "the app is on the rows reloaded from the card"
    );

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
    assert_eq!(books, 2);
    card.disk.refuse_next_writes(u32::MAX);
    device.send(StorageCommand::RefreshCatalog);
    device.run_queued();
    assert_eq!(device.store.catalog_count(), books, "the old catalog stays");
    assert_eq!(
        device.app.catalog_epoch,
        device.store.catalog_epoch(),
        "the app is on the rows reloaded from the card"
    );

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
    let (mut kept, mut cleared, mut finished) = (0, 0, false);
    for landed in 0..200 {
        let card = Card::blank();
        card.put(&format!("BOOKS/Shelf/{FIRST}"), &epub("Alpha", 6, 1));
        card.put(&format!("BOOKS/Shelf/{SECOND}"), &epub("Beta", 6, 2));
        Device::wake(&card).sleep();
        card.put(ADDED, &epub("Added", 4, 3));

        let mut device = Device::wake(&card);
        assert_eq!(device.store.catalog_count(), 2);
        card.disk.fail_after_writes(landed);
        device.send(StorageCommand::RefreshCatalog);
        device.run_queued();
        match device.store.catalog_count() {
            3 => {
                finished = true;
                break;
            }
            2 => {
                kept += 1;
                assert_eq!(
                    device.app.catalog_epoch,
                    device.store.catalog_epoch(),
                    "after {landed} writes"
                );
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
    assert!(
        finished,
        "the refresh made more writes than the sweep covers"
    );
    assert!(kept > 0, "some failures came before the truncation");
    assert!(cleared > 0, "and some after it");
}

/// A refresh over a card that lands each write in turn and then reports it
/// failed, after a computer replaced one book with another so the count is
/// unchanged. Whatever the failure leaves in CATALOG.BIN, the resident rows
/// are that file's rows, and an epoch the app agrees on.
#[test]
fn a_write_that_lands_and_fails_leaves_the_rows_the_file_holds() {
    let (mut landed_new, mut finished) = (0, false);
    for landed in 0..200 {
        let card = Card::blank();
        card.put(&format!("BOOKS/Shelf/{FIRST}"), &epub("Alpha", 6, 1));
        card.put(&format!("BOOKS/Shelf/{SECOND}"), &epub("Beta", 6, 2));
        Device::wake(&card).sleep();
        card.delete(&format!("BOOKS/Shelf/{SECOND}"));
        card.put("BOOKS/Shelf/Gamma.epub", &epub("Gamma", 6, 4));

        let mut device = Device::wake(&card);
        let before: Vec<_> = (0..device.store.catalog_count())
            .map(|i| catalog_identity(&device, i))
            .collect();
        card.disk.land_then_fail_after(landed);
        device.send(StorageCommand::RefreshCatalog);
        device.run_queued();
        if !card.disk.failed() {
            finished = true;
            break;
        }
        let on_card: Vec<_> = (0..)
            .map_while(|i| {
                card.session(|root| storage::library_sd::read_catalog_record_at(root, i))
                    .map(|record| (record.source_hash, record.byte_size))
            })
            .collect();
        let resident: Vec<_> = (0..device.store.catalog_count())
            .map(|i| catalog_identity(&device, i))
            .collect();
        assert_eq!(resident, on_card, "after {landed} writes");
        assert_eq!(
            device.app.catalog_epoch,
            device.store.catalog_epoch(),
            "after {landed} writes"
        );
        if !on_card.is_empty() && on_card != before {
            landed_new += 1;
        }
    }
    assert!(
        finished,
        "the refresh made more writes than the sweep covers"
    );
    assert!(
        landed_new > 0,
        "some failures left the new catalog on the card"
    );
}

fn catalog_identity(device: &Device, index: usize) -> (u32, u32) {
    let entry = device.store.catalog_entry(index).expect("a resident row");
    (entry.source_hash, entry.byte_size)
}

/// A write lands and reports failure, and then one read of the reload that
/// follows is refused. A catalog that did not load in full is no catalog:
/// either every resident row is there and matches the file, or none is.
#[test]
fn a_reload_cut_short_after_a_failed_write_leaves_no_partial_catalog() {
    let (mut cut_reloads, mut swept) = (0, false);
    'writes: for landed in 0..200 {
        let mut finished = false;
        for reads in 0..200 {
            let card = Card::blank();
            card.put(&format!("BOOKS/Shelf/{FIRST}"), &epub("Alpha", 6, 1));
            card.put(&format!("BOOKS/Shelf/{SECOND}"), &epub("Beta", 6, 2));
            Device::wake(&card).sleep();
            card.delete(&format!("BOOKS/Shelf/{SECOND}"));
            card.put("BOOKS/Shelf/Gamma.epub", &epub("Gamma", 6, 4));

            let mut device = Device::wake(&card);
            card.disk.land_then_fail_after(landed);
            card.disk.refuse_read_after_the_failed_write(reads);
            device.send(StorageCommand::RefreshCatalog);
            device.run_queued();
            if !card.disk.failed() {
                swept = true;
                break 'writes;
            }
            if !card.disk.read_failed() {
                finished = true;
                break;
            }
            let count = device.store.catalog_count();
            let window = device.store.catalog_window().len();
            if count > 0 {
                assert_eq!(
                    window,
                    count.min(16),
                    "after {landed} writes, {reads} reads"
                );
                let on_card: Vec<_> = (0..count)
                    .map(|i| {
                        card.session(|root| storage::library_sd::read_catalog_record_at(root, i))
                            .map(|record| (record.source_hash, record.byte_size))
                    })
                    .collect();
                let resident: Vec<_> = (0..count)
                    .map(|i| Some(catalog_identity(&device, i)))
                    .collect();
                assert_eq!(resident, on_card, "after {landed} writes, {reads} reads");
            } else {
                cut_reloads += 1;
            }
            assert_eq!(device.app.catalog_epoch, device.store.catalog_epoch());
        }
        assert!(finished, "after {landed} writes the reads ran out first");
    }
    assert!(swept, "the refresh made more writes than the sweep covers");
    assert!(cut_reloads > 0, "some reload was cut short");
}

/// A pick deep in a folder whose book the catalog lacks: the rescan lands,
/// the book opens, and the listing that follows has the cursor on that book.
/// The storage task does not see the scrolling that reached it, only the
/// pick. The app takes the cursor only while the Library is up, so here,
/// with the book open, the row is in the listing and the store, not in the
/// reader's selection, which the chapter list owns in Reading.
#[test]
fn a_pick_that_rescans_deep_in_a_folder_keeps_the_cursor_on_its_book() {
    const PICKED: &str = "B30a.epub";
    let card = Card::blank();
    for n in 0..40u32 {
        card.put(
            &format!("BOOKS/Shelf/B{n:02}.epub"),
            &epub(&format!("B{n:02}"), 2, n),
        );
    }
    Device::wake(&card).sleep();
    card.put(&format!("BOOKS/Shelf/{PICKED}"), &epub("B30a", 2, 99));

    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Shelf");
    for press in 0.. {
        if device.rows().iter().any(|row| row == PICKED) {
            break;
        }
        assert!(
            press < 60,
            "selection={} start={} rows={:?}",
            device.app.selection,
            device.store.folder_start(),
            device.rows()
        );
        device.press(Button::Next);
        // The firmware refills the page before each Library paint.
        let portrait = app_core::is_portrait(device.app.orientation);
        let selection = device.app.selection;
        storage::library_sd::ensure_folder_page(
            &mut device.card,
            &mut device.store,
            selection,
            portrait,
        );
    }
    device.point_at(PICKED);
    let row = device.app.selection;
    device.press(Button::Confirm);
    assert!(
        device.saw(|event| matches!(event, LibraryEvent::Scanned { .. })),
        "{:?}",
        device.log
    );
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    // The pick opened the book at its start, with the chapter cursor on
    // chapter 0. The listing that followed is for the Library, and must not
    // move that cursor to the picked row.
    assert_eq!(
        device.app.selection, 0,
        "the listing must not replace the Reading selection: {:?}",
        device.log
    );
    assert!(
        device.saw(|event| matches!(
            event,
            LibraryEvent::FolderListed {
                request_id: None,
                selection,
                ..
            } if *selection == row
        )),
        "the listing put the cursor on the picked book: {:?}",
        device.log
    );
    assert_eq!(
        device
            .store
            .folder_row(usize::from(row))
            .map(|resident| resident.name.as_str()),
        Some(PICKED),
        "the page read is the one around it"
    );
}
