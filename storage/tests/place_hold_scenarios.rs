//! A restore's hold on the stored place.
//!
//! An open that cannot reach the reader's place yet lands them on a
//! provisional page and holds the place: a save at that page is not the
//! reader choosing it, so it must not overwrite the place on the card. The
//! hold ends when the reader turns away from the landing. These scenarios
//! cover the ways the wait for the place can end without the reader doing
//! that, and check the place survives each.

mod support;

use app_core::{AppView, Button, RefreshPolicy, StorageCommand};
use display::font::FontSize;
use proto::anchor::ContentAnchor;
use proto::identity::BookId;
use reader_cache::files::{read_place, read_position_file, read_state_file, PlaceRead};
use support::{epub, Card, Device};

const FIRST: &str = "Alpha.epub";
const SECOND: &str = "Beta.epub";

fn open(device: &mut Device, name: &str) {
    to_home(device);
    device.open_library();
    if device.app.library_depth == 0 {
        device.choose("Shelf");
    }
    device.choose(name);
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
}

/// Home, answering each press's commands as it goes but running no
/// background slice, so a walk the open left suspended is still where it was.
/// Portrait: the first Back in Reading shows the key sheet, the second acts.
fn to_home(device: &mut Device) {
    if device.app.view == AppView::Reading {
        device.press_only(Button::Back);
        device.press_only(Button::Back);
        device.run_queued();
    }
    while device.app.view != AppView::Home {
        device.press_only(Button::Back);
        device.run_queued();
    }
}

fn shelf() -> Card {
    let card = Card::blank();
    card.put(&format!("BOOKS/Shelf/{FIRST}"), &epub("Alpha", 6, 1));
    card.put(&format!("BOOKS/Shelf/{SECOND}"), &epub("Beta", 6, 2));
    card
}

/// Read the first book deep into a later chapter and put the device to sleep,
/// so the card holds its place. Returns the page.
fn read_deep_and_sleep(card: &Card) -> u32 {
    let mut device = Device::wake(card);
    open(&mut device, FIRST);
    let pages = device.app.sd_page_count;
    device.turn(pages * 3 / 4);
    let deep = device.app.page;
    assert!(
        device.app.chapter >= 3,
        "deep in the book: {:?}",
        device.app.chapter
    );
    device.sleep();
    deep
}

/// The book's cache cleared from the Library's actions sheet, which leaves
/// its place on the card, so the next open builds again from the start.
fn clear_first_books_cache(device: &mut Device) {
    device.open_library();
    if device.app.library_depth == 0 {
        device.choose("Shelf");
    }
    device.point_at(FIRST);
    device.press(Button::PagePrevious);
    device.press(Button::Confirm);
    device.settle();
    to_home(device);
    device.settle();
}

/// Waiting on the walk, the reader goes Home. The slices carry on and the
/// walk finishes while nobody is reading the book, so the place cannot
/// fire there. Picking the book again closes it out at the landing page
/// first, which the hold has to refuse, and the reopen lands on the place.
#[test]
fn leaving_reading_while_the_place_waits_keeps_it() {
    let card = shelf();
    let deep = read_deep_and_sleep(&card);

    let mut device = Device::wake(&card);
    clear_first_books_cache(&mut device);
    device.open_library();
    if device.app.library_depth == 0 {
        device.choose("Shelf");
    }
    device.point_at(FIRST);
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Reading);
    assert!(
        device.task.pending_place.is_some(),
        "the open is waiting on the walk for the place: page {}",
        device.app.page
    );
    let landed = device.app.page;
    assert_ne!(landed, deep);

    to_home(&mut device);
    device.settle();

    open(&mut device, FIRST);
    device.settle();
    assert_eq!(device.app.page, deep, "{:?}", device.log);
}

fn copy_id(device: &Device) -> BookId {
    device
        .store
        .loaded_book_snapshot()
        .and_then(|loaded| loaded.copy_id)
        .expect("the copy has an id")
}

fn stored_place(card: &Card, id: BookId) -> PlaceRead {
    card.session(|root| read_place(root, id))
}

/// Where the loaded book's position file lives, kept past the session.
struct PositionFile {
    key: String,
    root: proto::library_path::BookRoot,
    locator: String,
}

impl PositionFile {
    fn of(device: &Device) -> Self {
        let loaded = device
            .store
            .loaded_book_snapshot()
            .expect("the book is loaded");
        Self {
            key: proto::cache::cache_key_from(loaded.identity.0)
                .as_str()
                .to_string(),
            root: loaded.root,
            locator: loaded.path.to_string(),
        }
    }

    /// The page it holds: the place an open falls back to, which a held save
    /// must leave alone as it does the anchor.
    fn page(&self, card: &Card) -> Option<u32> {
        let owner = proto::cache::CacheOwner {
            key: &self.key,
            root: self.root,
            locator: &self.locator,
        };
        card.session(|root| read_position_file(root, &owner))
            .map(|(_, page)| page)
    }
}

/// The first book read deep into a later chapter and left for the second, so
/// every record holds the deep page, all in one session: a wake would restore
/// the book at its chapter's start, and leaving it writes that page. Then its
/// cache is cleared and it is picked again, which lands short of the place and
/// waits on the walk. Returns the device, the deep page, and the place.
fn waiting_in_one_session(card: &Card) -> (Device, u32, PositionFile, BookId) {
    let mut device = Device::wake(card);
    open(&mut device, FIRST);
    let pages = device.app.sd_page_count;
    device.turn(pages * 3 / 4);
    let deep = device.app.page;
    let position = PositionFile::of(&device);
    let id = copy_id(&device);
    open(&mut device, SECOND);
    assert_eq!(position.page(card), Some(deep), "the switch saved the page");

    to_home(&mut device);
    clear_first_books_cache(&mut device);
    device.open_library();
    if device.app.library_depth == 0 {
        device.choose("Shelf");
    }
    device.point_at(FIRST);
    device.press_only(Button::Confirm);
    device.run_queued();
    assert!(device.task.pending_place.is_some(), "{:?}", device.log);
    assert_ne!(device.app.page, deep);
    (device, deep, position, id)
}

/// Waiting on the walk, the reader picks the other book. The close-out is at
/// the landing page, so it may write neither the place nor the position there.
#[test]
fn switching_while_the_place_waits_keeps_both_records() {
    let card = shelf();
    let (mut device, deep, position, id) = waiting_in_one_session(&card);
    let PlaceRead::Found(place) = stored_place(&card, id) else {
        panic!("the place is on the card");
    };

    open(&mut device, SECOND);
    let PlaceRead::Found(after) = stored_place(&card, id) else {
        panic!("the place is gone");
    };
    assert_eq!(after.anchor, place.anchor, "the anchor kept the place");
    assert_eq!(
        position.page(&card),
        Some(deep),
        "the position kept the page"
    );
}

/// Waiting on the walk, the reader changes a setting, then the device sleeps.
/// Both saves are at the landing page, so they may write neither per-book
/// record there. The setting still lands.
#[test]
fn a_settings_save_while_the_place_waits_keeps_both_records() {
    let card = shelf();
    let (mut device, deep, position, id) = waiting_in_one_session(&card);
    let PlaceRead::Found(place) = stored_place(&card, id) else {
        panic!("the place is on the card");
    };

    device.app.refresh_policy = RefreshPolicy::FastOnly;
    let saved = device.app.persisted();
    device.send(StorageCommand::StoreProgress(saved));
    device.run_queued();
    device.sleep();
    let mirror = card.session(read_state_file).expect("a saved state");
    assert_eq!(
        mirror.refresh_policy, saved.refresh_policy,
        "the setting landed"
    );
    let PlaceRead::Found(after) = stored_place(&card, id) else {
        panic!("the place is gone");
    };
    assert_eq!(after.anchor, place.anchor, "the anchor kept the place");
    assert_eq!(
        position.page(&card),
        Some(deep),
        "the position kept the page"
    );
}

/// The walk the place waits on ends without finishing, here on a refused
/// read. The wait ends, but the reader is still on the landing page, so the
/// hold has to outlast it or switching books writes that page over the place.
/// Each refusal point is tried on a fresh card.
#[test]
fn a_walk_that_ends_early_keeps_the_place_it_was_reaching_for() {
    let mut exercised = false;
    for refusal in 0..400 {
        let card = shelf();
        read_deep_and_sleep(&card);
        let mut device = Device::wake(&card);
        clear_first_books_cache(&mut device);
        device.open_library();
        if device.app.library_depth == 0 {
            device.choose("Shelf");
        }
        device.point_at(FIRST);
        device.press_only(Button::Confirm);
        device.run_queued();
        assert!(device.task.pending_place.is_some(), "refusal {refusal}");
        let id = copy_id(&device);
        let PlaceRead::Found(place) = stored_place(&card, id) else {
            panic!("refusal {refusal}: the place is on the card");
        };

        card.disk.refuse_read_in(Some(refusal));
        device.step_background();
        let fired = !card.disk.read_refusal_armed();
        card.disk.refuse_read_in(None);
        if !fired {
            break;
        }
        let ended_early =
            device.task.background_build.is_none() && device.store.book_index_is_partial();
        if !ended_early {
            continue;
        }
        exercised = true;

        open(&mut device, SECOND);
        let PlaceRead::Found(after) = stored_place(&card, id) else {
            panic!("refusal {refusal}: the place is gone");
        };
        assert_eq!(
            after.anchor, place.anchor,
            "refusal {refusal}: the switch kept the place the reader left"
        );
    }
    assert!(exercised, "some refused read ended the walk early");
}

/// The first book read deep at the default text size and put to sleep.
/// Returns the page, the copy, and the place the card holds for it.
fn deep_place_at_default_size(card: &Card) -> (u32, BookId, ContentAnchor) {
    let mut device = Device::wake(card);
    open(&mut device, FIRST);
    let pages = device.app.sd_page_count;
    device.turn(pages * 3 / 4);
    let deep = device.app.page;
    let id = copy_id(&device);
    device.sleep();
    let PlaceRead::Found(place) = stored_place(card, id) else {
        panic!("the sleep saved the place");
    };
    (deep, id, place.anchor)
}

/// The reader is on the page a restore under a larger text size put them on.
/// That page starts before the place, so saving it would move the place back.
/// A settings save and the sleep's flush both land on it, and the place has to
/// survive them: back at the old size, the book opens on the page they left.
fn saves_on_the_restored_page_keep_the_place(
    mut device: Device,
    card: &Card,
    deep: u32,
    id: BookId,
    anchor: ContentAnchor,
) {
    let starts_at = device
        .store
        .anchor_for_global_page(device.app.page)
        .expect("the restored page is resident");
    assert!(
        starts_at < anchor,
        "the restored page starts before the place, or this proves nothing: {starts_at:?} {anchor:?}"
    );

    let saved = device.app.persisted();
    device.send(StorageCommand::StoreProgress(saved));
    device.run_queued();
    device.sleep();
    let PlaceRead::Found(after) = stored_place(card, id) else {
        panic!("the place is gone");
    };
    assert_eq!(after.anchor, anchor, "the saves kept the place");

    let mut device = Device::wake(card);
    device.app.font_size = FontSize::Medium;
    open(&mut device, FIRST);
    device.settle();
    assert_eq!(device.app.page, deep, "{:?}", device.log);
}

/// A larger text size, and the open resolves the place straight away.
#[test]
fn a_restore_under_a_new_text_size_keeps_the_exact_place() {
    let card = shelf();
    let (deep, id, anchor) = deep_place_at_default_size(&card);
    let mut device = Device::wake(&card);
    device.app.font_size = FontSize::Large;
    open(&mut device, FIRST);
    device.settle();
    saves_on_the_restored_page_keep_the_place(device, &card, deep, id, anchor);
}

/// A larger text size and no cache, so the open lands short of the place and
/// the walk carries the reader there.
#[test]
fn a_place_the_walk_reaches_under_a_new_text_size_keeps_the_exact_place() {
    let card = shelf();
    let (deep, id, anchor) = deep_place_at_default_size(&card);
    let mut device = Device::wake(&card);
    device.app.font_size = FontSize::Large;
    clear_first_books_cache(&mut device);
    device.open_library();
    if device.app.library_depth == 0 {
        device.choose("Shelf");
    }
    device.point_at(FIRST);
    device.press_only(Button::Confirm);
    device.run_queued();
    assert!(
        device.task.pending_place.is_some(),
        "the open is waiting on the walk for the place: {:?}",
        device.log
    );
    let landed = device.app.page;
    device.settle();
    assert_ne!(device.app.page, landed, "the walk reached the place");
    saves_on_the_restored_page_keep_the_place(device, &card, deep, id, anchor);
}
