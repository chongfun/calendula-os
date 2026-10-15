//! A chapter jump into a book still being built.
//!
//! The chapter list is the whole TOC from the first step on, so it offers
//! chapters the progressive walk has not reached. A jump to one cannot load
//! the chapter's page yet; it has to land somewhere honest and follow the
//! walk there, not send the reader to the start of the book.

mod support;

use app_core::{AppView, Button, LibraryEvent};
use support::{epub, Card, Device};

const BOOK: &str = "86 - Volume 02.epub";
const HOME: &str = "BOOKS/86/86 - Volume 02.epub";

/// First open, so the walk is suspended after the first chapter, then
/// `turned` pages read and the chapter list, with no background step in
/// between.
fn on_the_chapter_list_of_a_book_being_built(card: &Card, turned: u32) -> Device {
    card.put(HOME, &epub("86 Volume 2", 6, 2));
    let mut device = Device::wake(card);
    device.open_library();
    device.choose("86");
    device.point_at(BOOK);
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Reading);
    assert!(
        device.task.background_build.is_some(),
        "the walk is suspended"
    );
    for _ in 0..turned {
        device.press_only(Button::PageNext);
        device.run_queued();
    }
    assert_eq!(device.app.page, turned);
    // Portrait: the first press shows the key sheet, the second acts on it.
    device.press_only(Button::Confirm);
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Chapters);
    device
}

#[test]
fn a_jump_past_the_walk_follows_it_to_the_chapter() {
    let card = Card::blank();
    let mut device = on_the_chapter_list_of_a_book_being_built(&card, 0);
    for _ in 0..4 {
        device.press_only(Button::Next);
    }
    assert_eq!(device.app.selection, 4, "the fifth chapter");
    let before = device.log.len();
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Reading);
    let landed_at_start = device.log[before..].iter().any(|event| {
        matches!(
            event,
            LibraryEvent::Loaded {
                position: Some(0),
                ..
            }
        )
    });
    assert!(
        !landed_at_start,
        "the jump did not send the reader to the start: {:?}",
        &device.log[before..]
    );

    device.settle();
    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!(device.app.chapter, 4, "{:?}", &device.log[before..]);
    assert!(device.app.page > 0);
    // The fifth spine item: the spine is the six chapters, the navigation
    // document is not in it.
    let built = device.store.first_page_of_spine(4);
    assert_eq!(Some(device.app.page), built);
}

/// The landing page is not one the reader chose, so a sleep before the walk
/// arrives keeps the page they were reading, even one the coalescer was still
/// holding when they jumped.
#[test]
fn a_sleep_before_the_walk_arrives_keeps_the_page_the_reader_chose() {
    let card = Card::blank();
    let mut device = on_the_chapter_list_of_a_book_being_built(&card, 1);
    for _ in 0..4 {
        device.press_only(Button::Next);
    }
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Reading);
    assert!(
        device.app.page > 1,
        "landed at the frontier, past the page read"
    );
    device.sleep();

    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("86");
    device.choose(BOOK);
    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!(device.app.page, 1);
}

/// Once the walk reaches the chapter, the reader is on a page they chose, and
/// a sleep with no page turn after it keeps that page, not the one they
/// jumped from.
#[test]
fn a_sleep_after_the_walk_arrives_keeps_the_chapter_the_reader_chose() {
    let card = Card::blank();
    let mut device = on_the_chapter_list_of_a_book_being_built(&card, 1);
    for _ in 0..4 {
        device.press_only(Button::Next);
    }
    device.press_only(Button::Confirm);
    device.run_queued();
    device.settle();
    assert_eq!(device.app.chapter, 4);
    let chosen = device.app.page;
    assert_eq!(Some(chosen), device.store.first_page_of_spine(4));
    device.sleep();

    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("86");
    device.choose(BOOK);
    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!(device.app.page, chosen);
}

/// A card that will not take the page the reader was on refuses the jump
/// with it: landing at the frontier would replace that page in the
/// coalescer, and it would be gone before the next flush could retry it.
#[test]
fn a_jump_whose_page_will_not_save_leaves_the_reader_on_that_page() {
    let card = Card::blank();
    let mut device = on_the_chapter_list_of_a_book_being_built(&card, 1);
    for _ in 0..4 {
        device.press_only(Button::Next);
    }
    assert_eq!(
        device.task.pending_progress.map(|record| record.screen),
        Some(1),
        "the page read is still in the coalescer"
    );
    card.disk.refuse_next_writes(u32::MAX);
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!(device.app.page, 1, "back on the page the reader was on");
    assert_eq!(device.app.chapter, 0);
    // The card comes back in time for the sleep's flush to retry it.
    card.disk.refuse_next_writes(0);
    device.sleep();

    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("86");
    device.choose(BOOK);
    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!(device.app.page, 1);
}
