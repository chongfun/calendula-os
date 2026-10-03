//! A pick's rescan reports its progress so the Library note can show a
//! percentage, and the reports change nothing about the scan.
//!
//! The harness card's sink borrows the volume manager for every report, as
//! the firmware's does to lend the bus to the panel, so a report made inside
//! a card operation panics here. See `support`.

mod support;

use app_core::{AppView, Button, LibraryEvent};
use display::font::FontSize;
use proto::progress::JobProgress;
use storage::progress::{MAX_REPORTED_PERCENT, REPORT_INTERVAL_MS};
use support::{epub, epub_shaped, Card, Device};

const BOOK: &str = "86 - Volume 02.epub";
const HOME: &str = "BOOKS/86/86 - Volume 02.epub";
const MOVED: &str = "BOOKS/86/MOVED/86 - Volume 02.epub";

/// A book read to page 5 and then moved on a computer while the device was
/// off: picking it from its new folder rescans and proves the move by hash.
fn moved_book_card() -> Card {
    let card = Card::blank();
    card.put(HOME, &epub("86 Volume 2", 6, 2));
    card.put("BOOKS/86/86 - Volume 01.epub", &epub("86 Volume 1", 6, 1));
    let mut device = Device::wake(&card);
    device.app.font_size = FontSize::Large;
    device.open_library();
    device.choose("86");
    device.choose(BOOK);
    device.turn(5);
    device.sleep();
    card.rename(HOME, MOVED);
    card
}

/// Boot over `card`, watch progress on a clock stepping `step_ms` per read
/// (or not at all), and pick the moved book.
fn pick_moved_book(card: &Card, step_ms: Option<u64>) -> Device {
    let mut device = Device::wake(card);
    device.open_library();
    device.choose("86");
    device.choose("MOVED");
    device.card.progress.step_ms = step_ms;
    device.choose(BOOK);
    device
}

/// Rising, at most 99, and spaced by the interval on the sink's clock.
fn assert_well_formed(reports: &[(u64, u8)]) {
    for pair in reports.windows(2) {
        let [(was_at, was), (at, percent)] = pair else {
            unreachable!("windows of two");
        };
        assert!(percent > was, "rising: {reports:?}");
        assert!(at - was_at >= REPORT_INTERVAL_MS, "spaced: {reports:?}");
    }
    assert!(
        reports
            .iter()
            .all(|&(_, percent)| percent <= MAX_REPORTED_PERCENT),
        "below 100 until the scan is over: {reports:?}"
    );
}

/// The move's rescan reports as it hashes and carries, every report at a
/// point where the card is idle, and the percentage only rises.
#[test]
fn a_rescan_that_proves_a_move_reports_rising_progress() {
    let card = moved_book_card();
    let device = pick_moved_book(&card, Some(900));
    assert!(
        device.saw(|event| matches!(event, LibraryEvent::Rescanning { .. })),
        "the pick rescanned: {:?}",
        device.log
    );
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    assert_eq!(device.app.page, 5, "the move was proved and carried");

    let sessions = &device.card.progress.sessions;
    assert_eq!(
        sessions
            .iter()
            .filter(|reports| !reports.is_empty())
            .count(),
        1,
        "only the scan reports: {sessions:?}"
    );
    let reports = device.card.progress.reports();
    assert_well_formed(&reports);
    assert!(reports.len() >= 2, "{reports:?}");
    assert!(
        reports.last().is_some_and(|&(_, percent)| percent >= 90),
        "the carry and the ledger write bring it near the end: {reports:?}"
    );
}

/// The reports are spaced by the clock: one that hardly moves is asked to
/// show nothing, however much the scan earns.
#[test]
fn a_rescan_on_a_slow_clock_reports_nothing() {
    let card = moved_book_card();
    let device = pick_moved_book(&card, Some(1));
    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!(device.card.progress.reports(), vec![]);
}

/// Watching changes nothing: the same pick on the same card, watched and
/// not, sends the same events and lands on the same page in the same rows.
#[test]
fn reporting_progress_leaves_the_scan_as_it_was() {
    let watched = pick_moved_book(&moved_book_card(), Some(900));
    let unwatched = pick_moved_book(&moved_book_card(), None);
    assert!(!watched.card.progress.reports().is_empty());
    assert!(unwatched.card.progress.sessions.is_empty());

    assert_eq!(watched.log, unwatched.log);
    assert_eq!(watched.rows(), unwatched.rows());
    assert_eq!(
        (
            watched.app.view,
            watched.app.page,
            watched.app.sd_page_count
        ),
        (
            unwatched.app.view,
            unwatched.app.page,
            unwatched.app.sd_page_count
        )
    );
}

/// A book added on a computer proves nothing by hash, and the phases alone
/// carry the percentage.
#[test]
fn a_rescan_with_nothing_to_prove_reports_by_phase() {
    let card = Card::blank();
    card.put("BOOKS/Alpha.epub", &epub("Alpha", 4, 1));
    Device::wake(&card).sleep();
    card.put("BOOKS/Added.epub", &epub("Added", 4, 3));

    let mut device = Device::wake(&card);
    device.open_library();
    device.card.progress.step_ms = Some(5_000);
    device.choose("Added.epub");
    assert!(device.saw(|event| matches!(event, LibraryEvent::Rescanning { .. })));
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    let reports = device.card.progress.reports();
    assert_well_formed(&reports);
    assert!(!reports.is_empty(), "{reports:?}");
}

/// A chapter jump answered from the cache carries a suspended walk through,
/// and its progress with it. Steps then move the progress on, and the walk
/// finishing clears it.
#[test]
fn a_carried_walk_keeps_its_progress_until_it_finishes() {
    let card = Card::blank();
    card.put(HOME, &epub("86 Volume 2", 6, 2));
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("86");
    device.point_at(BOOK);
    // Opened with no background step, so the walk is where the first open left it.
    device.press_only(Button::Confirm);
    device.run_queued();
    let book = device.app.book_id;
    let opened = device.task.build_progress(book);
    assert_eq!(
        opened,
        Some(JobProgress::new(1, 6)),
        "first open suspended at spine 1 of 6"
    );

    // To the chapter list and back to the chapter on the glass: the jump's
    // load is a fast hit on the published index, which leaves the walk alone.
    // Portrait: the first press shows the key sheet, the second acts on it.
    device.press_only(Button::Confirm);
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Chapters);
    let before = device.log.len();
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Reading);
    assert!(
        device.log[before..]
            .iter()
            .any(|event| matches!(event, LibraryEvent::Loaded { .. })),
        "the jump loaded its section: {:?}",
        &device.log[before..]
    );
    assert_eq!(device.task.build_progress(book), opened, "carried through");

    device.step_background();
    if let Some(stepped) = device.task.build_progress(book) {
        assert!(stepped.done > 1 && stepped.total == 6, "{stepped:?}");
    }
    device.settle();
    assert_eq!(device.task.build_progress(book), None);
    assert!(device.task.background_build.is_none());
}

/// Front matter the walk skips is not progress: a book whose text starts after
/// three front-matter items reads one item of six on its first open, not four
/// of nine.
#[test]
fn build_progress_counts_only_the_items_the_walk_builds() {
    let card = Card::blank();
    card.put(HOME, &epub_shaped("86 Volume 2", 3, 6, false, 2));
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("86");
    device.point_at(BOOK);
    device.press_only(Button::Confirm);
    device.run_queued();

    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!(
        device.task.build_progress(device.app.book_id),
        Some(JobProgress::new(1, 6)),
        "first open suspended after the first chapter"
    );
    device.step_background();
    if let Some(stepped) = device.task.build_progress(device.app.book_id) {
        assert_eq!(stepped.total, 6, "{stepped:?}");
    }
}

/// A navigation item after the last chapter is nothing to build. A book of one
/// chapter and a trailing nav finishes on its first open, rather than leaving
/// a walk with nothing to do that shows a full rule while it claims to run.
#[test]
fn a_trailing_navigation_item_leaves_no_walk_behind() {
    let card = Card::blank();
    card.put(HOME, &epub_shaped("86 Volume 2", 0, 1, true, 2));
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("86");
    device.point_at(BOOK);
    device.press_only(Button::Confirm);
    device.run_queued();

    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!(device.task.build_progress(device.app.book_id), None);
    assert!(device.task.background_build.is_none(), "no walk left");
}

/// A book put to sleep before its first build finished wakes to Home with no
/// page total, not the total the abandoned build had reached. That total
/// covers only the chapters the build walked, so the Home rule measured
/// against it puts the reader much further into the book than they are.
#[test]
fn a_build_cut_by_sleep_gives_home_no_page_total() {
    let card = Card::blank();
    card.put(HOME, &epub("86 Volume 2", 6, 2));
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("86");
    device.point_at(BOOK);
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Reading);
    let partial = device.app.sd_page_count;
    assert_eq!(
        device.task.build_progress(device.app.book_id),
        Some(JobProgress::new(1, 6)),
        "the walk is suspended, so the index on the card is unfinished"
    );
    device.press_only(Button::Next);
    device.run_queued();
    device.sleep();

    let restored = |device: &Device| {
        device.log.iter().find_map(|event| match event {
            LibraryEvent::Restored { page_count, .. } => Some(*page_count),
            _ => None,
        })
    };
    let mut device = Device::wake(&card);
    assert_eq!(
        restored(&device),
        Some(0),
        "the abandoned build had reached {partial} pages"
    );

    // Once a build finishes, the total it wrote is the book's and comes back.
    device.open_library();
    device.choose("86");
    device.point_at(BOOK);
    device.press_only(Button::Confirm);
    device.run_queued();
    device.settle();
    assert!(device.task.background_build.is_none(), "the build finished");
    let whole = device.app.sd_page_count;
    assert!(whole > partial, "{whole} pages against {partial}");
    device.sleep();
    assert_eq!(restored(&Device::wake(&card)), Some(whole));
}
