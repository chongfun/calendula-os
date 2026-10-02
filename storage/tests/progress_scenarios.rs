//! A pick's rescan reports its progress so the Library note can show a
//! percentage, and the reports change nothing about the scan.
//!
//! The harness card's sink borrows the volume manager for every report, as
//! the firmware's does to lend the bus to the panel, so a report made inside
//! a card operation panics here. See `support`.

mod support;

use app_core::{AppView, Button, LibraryEvent};
use display::font::FontSize;
use storage::progress::{MAX_REPORTED_PERCENT, REPORT_INTERVAL_MS};
use storage::task::Host;
use support::{epub, Card, Device};

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

/// A background build suspended mid-spine preserves its reported progress when a
/// foreground section load hits the cache (the Carried path), rather than
/// erroneously resetting progress to None on section loads.
#[test]
fn carried_foreground_load_preserves_build_progress() {
    let card = Card::blank();
    card.put(HOME, &epub("86 Volume 2", 6, 2));
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("86");
    device.point_at(BOOK);
    // Open the book running only foreground queued commands, without background steps.
    device.press_only(Button::Confirm);
    device.run_queued();

    assert_eq!(device.app.view, AppView::Reading);
    assert!(device.task.background_owed(&device.store));
    let initial_progress = device.store.background_build_progress(device.app.book_id);
    assert_eq!(
        initial_progress,
        Some(proto::progress::JobProgress::new(1, 6)),
        "first open suspended at spine 1 of 6"
    );

    // Exercise the documented fast-hit / Carried path:
    // With the background build suspended and progress present, perform a foreground
    // book cache load for the published section (chapter 0, page 0).
    let scratch = device.host.ensure_scratch(&mut device.task.epub_scratch);
    let outcome = storage::book_build::build_or_load_book_cache(
        &mut device.card,
        &mut device.store,
        0, // catalog index
        0, // requested chapter
        0, // target pages
        scratch,
        &mut device.metrics,
    );
    assert_eq!(
        outcome,
        storage::book_build::BookBuildOutcome::Carried(initial_progress.unwrap())
    );

    // Re-arm / preserve background build handle via apply_build_outcome.
    storage::task::apply_build_outcome(
        &mut device.task.background_build,
        outcome,
        device.app.book_id,
        &mut device.store,
    );

    // Invariant: background build is still live and progress value is preserved across Carried load.
    assert!(
        device.task.background_owed(&device.store),
        "background build must remain alive"
    );
    assert_eq!(
        device.store.background_build_progress(device.app.book_id),
        initial_progress,
        "build progress must be preserved across Carried foreground load"
    );

    // A step that continues moves the projection forward; one that finishes clears it.
    device.step_background();
    match device.store.background_build_progress(device.app.book_id) {
        Some(stepped) => {
            assert!(device.task.background_owed(&device.store));
            assert!(stepped.done > initial_progress.unwrap().done, "{stepped:?}");
            assert_eq!(stepped.total, 6);
        }
        None => assert!(device.task.background_build.is_none()),
    }

    // When the remaining background steps settle and the build finishes, progress clears.
    device.settle();
    assert!(!device.task.background_owed(&device.store));
    assert_eq!(
        device.store.background_build_progress(device.app.book_id),
        None
    );

    // A subsequent load of the settled cache returns Settled and leaves progress None.
    let scratch = device.host.ensure_scratch(&mut device.task.epub_scratch);
    let settled_outcome = storage::book_build::build_or_load_book_cache(
        &mut device.card,
        &mut device.store,
        0,
        0,
        0,
        scratch,
        &mut device.metrics,
    );
    assert_eq!(
        settled_outcome,
        storage::book_build::BookBuildOutcome::Settled
    );
    assert_eq!(
        device.store.background_build_progress(device.app.book_id),
        None
    );
}
