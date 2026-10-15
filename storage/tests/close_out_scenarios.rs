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
use reader_cache::files::{
    read_place, read_position_file, write_place, write_position_file, PlaceRead,
};
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

/// The card refuses one read during the save, and every read the save makes is
/// refused in turn. A refusal says nothing about page 9's anchor, so each save
/// either lands with that anchor or stays owed with a place still on the card.
/// What must not happen is a refused look taken for no anchor at all: the
/// place removed for one the section file holds, and the save reported done.
#[test]
fn a_refused_read_while_saving_keeps_the_place_and_the_save_owed() {
    let Read {
        card,
        mut device,
        id,
        page_nine,
    } = read_to_page_nine_then_drop_the_pages();
    let older = match stored_place(&card, id) {
        PlaceRead::Found(place) => place,
        _ => panic!("the first turn wrote a place"),
    };
    let record = device.task.pending_progress.expect("page 9 is owed");

    let mut owed = 0usize;
    let mut finished = false;
    for probe in 0..2_000 {
        card.disk.refuse_read_in(Some(probe));
        let saved = device
            .task
            .flush_pending_progress(&mut device.card, &mut device.store);
        let refused = !card.disk.read_refusal_armed();
        card.disk.refuse_read_in(None);

        let place = match stored_place(&card, id) {
            PlaceRead::Found(place) => place.anchor,
            PlaceRead::Absent => panic!("probe {probe}: the save removed the place"),
            PlaceRead::Fault => panic!("probe {probe}: the place does not read back"),
        };
        if saved {
            assert_eq!(place, page_nine, "probe {probe}: a save reported done");
        } else {
            owed += 1;
            assert!(
                place == older.anchor || place == page_nine,
                "probe {probe}: a refused save left a place it was not given",
            );
            assert_eq!(
                device.task.pending_progress,
                Some(record),
                "probe {probe}: and stays owed",
            );
        }
        if !refused {
            assert!(saved, "a save the card answered in full lands");
            finished = true;
            break;
        }

        // Back to where the save started.
        device.task.pending_progress = Some(record);
        card.session(|root| write_place(root, id, older.anchor, older.source, older.progression))
            .expect("the older place goes back");
    }
    assert!(finished, "the save makes fewer reads than the probes cover");
    assert!(owed > 0, "no read in the save could be refused");
}

/// Where the open book's section files live on the card.
fn sections_path(device: &Device) -> String {
    let identity = device
        .store
        .loaded_book_snapshot()
        .expect("the book is loaded")
        .identity;
    format!(
        "READER/CACHE2/{}/SECTIONS",
        proto::cache::cache_key_from(identity.0)
    )
}

/// The book's section files, which hold page 9's anchor, taken off the card.
fn delete_the_section_files(card: &Card, sections: &str) -> Vec<String> {
    let names = card.list(sections);
    assert!(!names.is_empty(), "the book has section files");
    for name in &names {
        card.delete(&format!("{sections}/{name}"));
    }
    names
}

/// Switch books with no anchor for page 9 to be had. The switch still goes
/// through, the older place is removed, and the page-keyed position holds
/// page 9 instead.
fn switch_away_on_the_position(card: &Card, device: &mut Device, id: BookId) {
    let loaded = device
        .store
        .loaded_book_snapshot()
        .expect("the book is loaded");
    let key = proto::cache::cache_key_from(loaded.identity.0);
    let (root, locator) = (loaded.root, loaded.path.to_string());

    open(device, FIRST);
    assert!(
        !device.saw(|event| matches!(event, LibraryEvent::BookOpenFailed { .. })),
        "the close-out did not refuse the open: {:?}",
        device.log
    );
    assert_eq!(device.app.page, 0, "the other book opens at its start");
    assert!(
        matches!(stored_place(card, id), PlaceRead::Absent),
        "the older place is gone"
    );
    let owner = proto::cache::CacheOwner {
        key: key.as_str(),
        root,
        locator: &locator,
    };
    assert_eq!(
        card.session(|root| read_position_file(root, &owner))
            .map(|(_, page)| page),
        Some(9),
        "the position holds the page"
    );
}

/// The section file for page 9 is gone too, so no anchor can be had.
#[test]
fn a_switch_with_no_anchor_to_be_had_removes_the_older_place() {
    let Read {
        card,
        mut device,
        id,
        ..
    } = read_to_page_nine_then_drop_the_pages();
    let sections = sections_path(&device);
    delete_the_section_files(&card, &sections);
    switch_away_on_the_position(&card, &mut device, id);
    device.sleep();

    let mut device = open_after_boot(&card, SECOND);
    assert_eq!(device.app.page, 9, "the reopen lands on the newer page");
    // The next save writes a place again, from the pages now resident.
    device.turn(1);
    device.sleep();
    assert!(matches!(stored_place(&card, id), PlaceRead::Found(_)));
}

/// A file stands where the sections folder belongs, an entry the card read
/// back, so no section file can be under it. No reopen follows: the open
/// refuses a cache folder it cannot write sections into, as it did before.
#[test]
fn a_file_in_place_of_the_sections_folder_is_no_anchor() {
    let Read {
        card,
        mut device,
        id,
        ..
    } = read_to_page_nine_then_drop_the_pages();
    let sections = sections_path(&device);
    delete_the_section_files(&card, &sections);
    card.remove_folder(&sections);
    card.put_short(&sections, b"not a folder");
    let book = sections.trim_end_matches("/SECTIONS");
    assert!(card.list(book).iter().any(|name| name == "SECTIONS"));
    assert_eq!(card.read(&sections).as_deref(), Some(&b"not a folder"[..]));
    switch_away_on_the_position(&card, &mut device, id);
}

/// A folder stands where each section file belongs, the same reading from the
/// other side.
#[test]
fn a_folder_in_place_of_a_section_file_is_no_anchor() {
    let Read {
        card,
        mut device,
        id,
        ..
    } = read_to_page_nine_then_drop_the_pages();
    let sections = sections_path(&device);
    let names = delete_the_section_files(&card, &sections);
    for name in &names {
        card.make_short_folder(&format!("{sections}/{name}"));
    }
    assert_eq!(
        card.list(&sections),
        names,
        "folders under the files' names"
    );
    switch_away_on_the_position(&card, &mut device, id);
}

/// A page-keyed position past the end of the book, as a card from older
/// firmware or a pagination that has since shrunk leaves it, opens on the
/// book's last page. It opened on the first: the page would not load, and
/// the fallback went to page 0.
#[test]
fn a_saved_page_past_the_end_opens_on_the_last_page() {
    let card = Card::blank();
    card.put(&format!("BOOKS/Shelf/{FIRST}"), &epub("Alpha", 6, 1));
    card.put(&format!("BOOKS/Shelf/{SECOND}"), &epub("Beta", 6, 2));
    let mut device = open_after_boot(&card, SECOND);
    device.turn(3);
    device.settle();
    let total = device.app.sd_page_count;
    let loaded = device
        .store
        .loaded_book_snapshot()
        .expect("the book is loaded");
    let key = proto::cache::cache_key_from(loaded.identity.0);
    let (root, locator) = (loaded.root, loaded.path.to_string());
    device.sleep();

    // No place, only a page this pagination does not reach.
    for dir in card.list("READER/PLACES") {
        for name in card.list(&format!("READER/PLACES/{dir}")) {
            card.delete(&format!("READER/PLACES/{dir}/{name}"));
        }
    }
    let owner = proto::cache::CacheOwner {
        key: key.as_str(),
        root,
        locator: &locator,
    };
    card.session(|root| write_position_file(root, &owner, 0, total + 5))
        .expect("the position is written");

    let mut device = open_after_boot(&card, SECOND);
    device.settle();
    assert_eq!(device.app.page, total - 1, "of {total} pages");
}

/// When a saved position lies past the end of the book and the last section
/// itself will not load, the open still falls back to page 0 rather than
/// declaring the entire book unreadable.
#[test]
fn a_saved_page_past_the_end_falls_back_to_zero_when_last_page_fails() {
    let card = Card::blank();
    card.put(&format!("BOOKS/Shelf/{FIRST}"), &epub("Alpha", 6, 1));
    card.put(&format!("BOOKS/Shelf/{SECOND}"), &epub("Beta", 6, 2));
    let mut device = open_after_boot(&card, SECOND);
    device.turn(3);
    device.settle();
    let total = device.app.sd_page_count;
    let sections = sections_path(&device);
    let loaded = device
        .store
        .loaded_book_snapshot()
        .expect("the book is loaded");
    let key = proto::cache::cache_key_from(loaded.identity.0);
    let (root, locator) = (loaded.root, loaded.path.to_string());
    device.sleep();

    for dir in card.list("READER/PLACES") {
        for name in card.list(&format!("READER/PLACES/{dir}")) {
            card.delete(&format!("READER/PLACES/{dir}/{name}"));
        }
    }
    let mut names = card.list(&sections);
    names.sort();
    let last = names.last().expect("the book has section files");
    card.delete(&format!("{sections}/{last}"));
    card.make_short_folder(&format!("{sections}/{last}"));

    let owner = proto::cache::CacheOwner {
        key: key.as_str(),
        root,
        locator: &locator,
    };
    card.session(|root| write_position_file(root, &owner, 0, total + 5))
        .expect("the position is written");

    let mut device = open_after_boot(&card, SECOND);
    device.settle();
    assert_eq!(device.app.view, AppView::Reading);
    // An unreadable book also stays selected on page 0, so only this tells
    // the fallback that landed from the open that gave up.
    assert!(!device.app.book_unreadable(), "page 0 loaded");
    assert_eq!(device.app.page, 0);
}

/// Home after a wake shows the page and chapter the reader left, before the
/// book is opened. The place names a spine item and nothing about pages, and
/// taking it as (chapter, page) gave Home page 0 of that spine item's number:
/// an empty progress rule and, with front matter in the spine, another
/// chapter's name.
#[test]
fn home_after_a_wake_shows_the_page_the_reader_left() {
    let card = Card::blank();
    card.put(
        "BOOKS/Shelf/Beta.epub",
        &support::epub_shaped("Beta", 2, 6, false, 2),
    );
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Shelf");
    device.point_at("Beta.epub");
    device.press(Button::Confirm);
    device.settle();
    device.turn(30);
    device.settle();
    let (page, chapter) = (device.app.page, device.app.chapter);
    assert!(page > 20 && chapter > 0, "page {page} chapter {chapter}");
    device.sleep();

    let mut device = Device::wake(&card);
    assert_eq!(device.app.view, AppView::Home);
    assert_eq!((device.app.page, device.app.chapter), (page, chapter));
    assert_eq!(
        device.store.current_chapter(),
        chapter,
        "the colophon's chapter"
    );

    // Continue still opens on the place, not on the page Home showed.
    device.press(Button::Confirm);
    device.settle();
    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!((device.app.page, device.app.chapter), (page, chapter));
}

/// If a save lands the per-book place before the global state write completes,
/// the global record still carries an older chapter and page while the place
/// holds the latest ones. Home after a wake must show the per-book place's
/// position rather than the lagging global mirror.
#[test]
fn home_after_a_wake_prefers_the_per_book_place_when_the_global_mirror_lags() {
    let card = Card::blank();
    card.put(
        "BOOKS/Shelf/Beta.epub",
        &support::epub_shaped("Beta", 2, 6, false, 2),
    );
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Shelf");
    device.point_at("Beta.epub");
    device.press(Button::Confirm);
    device.settle();
    device.turn(2);
    device.settle();
    let (early_page, early_chapter) = (device.app.page, device.app.chapter);
    device.sleep();

    let stale_global = card
        .session(reader_cache::files::read_state_file)
        .expect("early global state");
    assert_eq!(
        (stale_global.screen, stale_global.chapter),
        (early_page, early_chapter)
    );

    let mut device = Device::wake(&card);
    device.press(Button::Confirm);
    device.settle();
    device.turn(30);
    device.settle();
    let (page, chapter) = (device.app.page, device.app.chapter);
    assert!(
        page > early_page && chapter > early_chapter,
        "page {page} (early {early_page}), chapter {chapter} (early {early_chapter})"
    );
    device.sleep();

    // Restore the older global state record and remove the cache-side page
    // files as if power failed right after the per-copy place write completed.
    let key = proto::cache::cache_key_from(stale_global.source_hash);
    for side in ["POSA.BIN", "POSB.BIN"] {
        card.delete(&format!("READER/CACHE2/{key}/{side}"));
    }
    card.session(|root| reader_cache::files::write_state_file(root, stale_global))
        .expect("the older global state goes back");

    let device = Device::wake(&card);
    assert_eq!(device.app.view, AppView::Home);
    assert_eq!((device.app.page, device.app.chapter), (page, chapter));
    assert_eq!(
        device.store.current_chapter(),
        chapter,
        "the colophon's chapter from the per-book place, not the lagging global mirror"
    );
}

/// The copy's place directory, the only one on a card holding one book.
fn place_dir(card: &Card) -> String {
    let dirs = card.list("READER/PLACES");
    assert_eq!(dirs.len(), 1, "one copy has a place: {dirs:?}");
    format!("READER/PLACES/{}", dirs[0])
}

/// `place` as firmware before display pages wrote it: version 1, 41 bytes,
/// the same fields up to the progression and no page.
fn v1_place_payload(place: proto::nvm::PlaceRecord) -> Vec<u8> {
    let current = proto::nvm::PlaceRecord {
        position: None,
        ..place
    }
    .encode();
    let mut out = current[..proto::nvm::PlaceRecord::V1_ENCODED_LEN].to_vec();
    out[4] = 1;
    out[35] = 0;
    out[36] = 0;
    let sum = proto::durable::durable_checksum(&out[..37]);
    out[37..41].copy_from_slice(&sum.to_le_bytes());
    out
}

/// One side of a durable pair holding `payload` at `generation`.
fn durable_side(payload: &[u8], generation: u32) -> Vec<u8> {
    let mut out = [0u8; proto::durable::DURABLE_MAX_BYTES];
    let len = proto::durable::encode_durable_record(*b"X4PL", generation, payload, &mut out)
        .expect("a place fits a durable record");
    out[..len].to_vec()
}

/// Read Beta into a later chapter and sleep, so the card holds a place, a
/// page-keyed position, and a global record all naming that page.
fn read_beta_into_a_later_chapter(card: &Card) -> (u32, u16, BookId) {
    card.put(
        "BOOKS/Shelf/Beta.epub",
        &support::epub_shaped("Beta", 2, 6, false, 2),
    );
    let mut device = Device::wake(card);
    device.open_library();
    device.choose("Shelf");
    device.point_at("Beta.epub");
    device.press(Button::Confirm);
    device.settle();
    device.turn(30);
    device.settle();
    let (page, chapter) = (device.app.page, device.app.chapter);
    assert!(page > 20 && chapter > 0, "page {page} chapter {chapter}");
    let id = device
        .store
        .loaded_book_snapshot()
        .and_then(|loaded| loaded.copy_id)
        .expect("the copy has an id");
    device.sleep();
    (page, chapter, id)
}

/// Rewind the global record to the book's first page and drop the page-keyed
/// position, so only the place can say where the reader was.
fn leave_only_the_place(card: &Card) {
    let mut global = card
        .session(reader_cache::files::read_state_file)
        .expect("the global state");
    let key = proto::cache::cache_key_from(global.source_hash);
    for side in ["POSA.BIN", "POSB.BIN"] {
        if card.read(&format!("READER/CACHE2/{key}/{side}")).is_some() {
            card.delete(&format!("READER/CACHE2/{key}/{side}"));
        }
    }
    (global.chapter, global.screen) = (0, 0);
    card.session(|root| reader_cache::files::write_state_file(root, global))
        .expect("the rewound global state goes back");
}

/// A card whose places were written before records carried a page still
/// opens on them. Its pair holds two version 1 sides, and the first save
/// after the upgrade writes a current record over one of them, numbered from
/// one because it cannot read the other side's number. The v1 side left
/// behind carries a higher generation and must not win.
#[test]
fn a_place_from_before_the_page_was_stored_keeps_its_anchor_and_upgrades() {
    let card = Card::blank();
    let (page, chapter, id) = read_beta_into_a_later_chapter(&card);
    let place = stored_place(&card, id)
        .found()
        .expect("the save wrote a place");
    assert!(place.position.is_some(), "this build stores the page");

    let dir = place_dir(&card);
    for side in card.list(&dir) {
        card.delete(&format!("{dir}/{side}"));
    }
    let v1 = v1_place_payload(place);
    card.put_short(&format!("{dir}/POSA.BIN"), &durable_side(&v1, 7));
    card.put_short(&format!("{dir}/POSB.BIN"), &durable_side(&v1, 8));
    leave_only_the_place(&card);
    match stored_place(&card, id) {
        PlaceRead::Found(read) => {
            assert_eq!(read.anchor, place.anchor);
            assert_eq!(read.progression, place.progression);
            assert_eq!(read.position, None, "a v1 place has no page");
        }
        _ => panic!("a v1 pair reads as a place"),
    }

    // Continue lands on the anchor, not on the rewound global record.
    let mut device = Device::wake(&card);
    device.press(Button::Confirm);
    device.settle();
    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!((device.app.page, device.app.chapter), (page, chapter));

    // The next save writes a current record, and it wins over the v1 side
    // with the higher generation.
    device.turn(1);
    device.settle();
    let (later_page, later_chapter) = (device.app.page, device.app.chapter);
    device.sleep();
    let upgraded = stored_place(&card, id)
        .found()
        .expect("the save wrote a place");
    assert_ne!(upgraded.anchor, place.anchor, "the later page's place");
    assert_eq!(
        upgraded.position.map(|at| (at.page, at.chapter)),
        Some((later_page, later_chapter))
    );

    leave_only_the_place(&card);
    let mut device = Device::wake(&card);
    device.press(Button::Confirm);
    device.settle();
    assert_eq!(
        (device.app.page, device.app.chapter),
        (later_page, later_chapter),
        "the upgraded place, not the v1 side left behind"
    );
}

/// A place whose page was counted under another pagination, as a card moved
/// from an X3 to an X4 holds: the page means nothing against this book
/// index's total. Home keeps the chapter and places the page by the place's
/// progression through the index it divides by.
#[test]
fn home_after_a_wake_does_not_show_a_page_from_another_pagination() {
    let card = Card::blank();
    let (page, chapter, id) = read_beta_into_a_later_chapter(&card);
    let place = stored_place(&card, id)
        .found()
        .expect("the save wrote a place");
    let saved = place.position.expect("this build stores the page");
    assert_eq!((saved.page, saved.chapter), (page, chapter));

    // The same place, counted on a panel whose pagination runs longer.
    let elsewhere = proto::nvm::PlacePage {
        page: page + 7,
        layout: proto::nvm::PageLayout {
            font_config: saved.layout.font_config ^ (128 << 8),
            ..saved.layout
        },
        ..saved
    };
    card.session(|root| {
        reader_cache::files::write_place_with_position(
            root,
            id,
            place.anchor,
            place.source,
            place.progression,
            Some(elsewhere),
        )
    })
    .expect("the moved card's place goes back");

    let mut device = Device::wake(&card);
    assert_eq!(device.app.view, AppView::Home);
    assert_eq!(
        (device.app.page, device.app.chapter),
        (page, chapter),
        "the progression through this index, not the other panel's page"
    );

    // Continue opens on the anchor as before.
    device.press(Button::Confirm);
    device.settle();
    assert_eq!((device.app.page, device.app.chapter), (page, chapter));
}
