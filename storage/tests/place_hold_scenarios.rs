//! A restore's hold on the stored place.
//!
//! An open that cannot reach the reader's place yet lands them on a
//! provisional page and holds the place: a save at that page is not the
//! reader choosing it, so it must not overwrite the place on the card. The
//! hold ends when the reader turns away from the landing. These scenarios
//! cover the ways the wait for the place can end without the reader doing
//! that, and check the place survives each.

mod support;

use app_core::{AppView, Button};
use proto::identity::BookId;
use reader_cache::files::{read_place, PlaceRead};
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
