//! A book whose background walk has built further than the index on the card.
//!
//! The walk rewrites the card's index only every `INDEX_PUBLISH_SECTIONS`
//! sections, so between rewrites the reader is told about pages the card's
//! index does not list yet. A move to one of them reloads that index, and has
//! to land on the page all the same.
//!
//! Tests run the real storage task over a FAT image and the real reducer over
//! its events. See `support`.

mod support;

use app_core::{AppView, Button};
use support::{epub, epub_without_nav, Card, Device};

const BOOK: &str = "Long.epub";
const HOME: &str = "BOOKS/Long.epub";

/// Background steps after the first open. Each chapter is too large to share
/// a step, so this builds that many chapters past the first, well short of
/// the sixteen sections that would rewrite the card's index.
const STEPS: usize = 5;

/// A long book's first open, with the walk `STEPS` chapters past the first
/// and its index on the card still the one the open wrote.
fn reading_ahead_of_the_card_index(card: &Card) -> Device {
    reading_ahead_with(card, &epub("Long", 24, 7))
}

fn reading_ahead_with(card: &Card, bytes: &[u8]) -> Device {
    card.put(HOME, bytes);
    let mut device = Device::wake(card);
    device.open_library();
    device.point_at(BOOK);
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Reading);
    assert!(
        device.task.background_build.is_some(),
        "the walk is suspended"
    );
    let first = device.store.advertised_page_count();
    for _ in 0..STEPS {
        device.step_background();
    }
    assert!(
        device.store.book_index_is_partial(),
        "the walk is unfinished"
    );
    assert!(
        device.store.advertised_page_count() > first,
        "the walk grew"
    );
    device
}

/// The fifth chapter was built by a step after the open, so only the walk's
/// records list it. The jump lands on its first page and keeps the walk's
/// page count, rather than falling back to the first page of the shorter
/// index on the card.
#[test]
fn a_jump_to_a_chapter_built_since_the_index_was_written_lands_on_it() {
    let card = Card::blank();
    let mut device = reading_ahead_of_the_card_index(&card);
    // The spine is the chapters; the navigation document is not in it.
    let fifth = device
        .store
        .first_page_of_spine(4)
        .expect("the walk built the fifth chapter");
    let pages = device.store.advertised_page_count();
    // Portrait: the first press shows the key sheet, the second acts on it.
    device.press_only(Button::Confirm);
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Chapters);
    for _ in 0..4 {
        device.press_only(Button::Next);
    }
    assert_eq!(device.app.selection, 4, "the fifth chapter");
    device.press_only(Button::Confirm);
    device.run_queued();

    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!(device.app.page, fifth, "{:?}", device.log.last());
    assert_eq!(device.app.chapter, 4);
    assert_eq!(device.store.advertised_page_count(), pages);
    assert!(
        device.task.background_build.is_some(),
        "the walk carries on"
    );

    device.settle();
    assert!(!device.store.book_index_is_partial(), "the walk finished");
    assert_eq!(device.app.page, fifth);
}

/// The same reload on an ordinary page turn: the first page of the second
/// chapter is the first one past the open's index.
#[test]
fn a_page_turn_past_the_index_on_the_card_reaches_the_next_chapter() {
    let card = Card::blank();
    let mut device = reading_ahead_of_the_card_index(&card);
    let second = device
        .store
        .first_page_of_spine(1)
        .expect("the walk built the second chapter");
    let pages = device.store.advertised_page_count();
    for _ in 0..second {
        device.press_only(Button::PageNext);
        device.run_queued();
    }

    assert_eq!(device.app.page, second, "{:?}", device.log.last());
    assert_eq!(device.app.chapter, 1);
    assert_eq!(device.store.advertised_page_count(), pages);
    assert!(
        device.task.background_build.is_some(),
        "the walk carries on"
    );
}

/// In a book with no navigation document, the walk collects chapter titles
/// from `<h1>` headings into the resident TOC as each spine item is built, and
/// `BOOK.BIN` on the card still holds only the first chapter's heading until
/// the next index publish. Reloading `BOOK.BIN` across a section boundary
/// mid-walk must keep the headings built since that write so the finished book
/// has all 24 chapters.
#[test]
fn a_move_during_the_walk_keeps_headings_built_since_the_index_was_written() {
    let card = Card::blank();
    let mut device = reading_ahead_with(&card, &epub_without_nav("Long", 24, 7));
    assert_eq!(device.store.toc_count(), 1 + STEPS);
    let second = device
        .store
        .first_page_of_spine(1)
        .expect("the walk built the second chapter");
    for _ in 0..second {
        device.press_only(Button::PageNext);
        device.run_queued();
    }

    assert_eq!(device.app.page, second);
    assert_eq!(
        device.store.toc_count(),
        1 + STEPS,
        "reloading BOOK.BIN kept the headings built since the open"
    );
    for chapter in 0..=STEPS {
        let item = device.store.toc_item(chapter).expect("heading kept");
        assert_eq!(item.title, format!("Chapter {}", chapter + 1));
    }

    device.settle();
    assert!(!device.store.book_index_is_partial(), "the walk finished");
    assert_eq!(device.store.toc_count(), 24);
    for chapter in 0..24 {
        let item = device.store.toc_item(chapter).expect("heading present");
        assert_eq!(item.title, format!("Chapter {}", chapter + 1));
    }

    // Reopening from the card's finished BOOK.BIN also sees all 24 headings.
    device.sleep();
    let mut device = Device::wake(&card);
    device.press(Button::Confirm);
    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!(device.store.toc_count(), 24);
    for chapter in 0..24 {
        let item = device.store.toc_item(chapter).expect("heading persisted");
        assert_eq!(item.title, format!("Chapter {}", chapter + 1));
    }
}
