//! How emphasis in a book's markup reaches the lines the reader draws.

mod support;

use app_core::{AppView, Button};
use display::font::FontStyle;
use support::{epub_with_body, Card, Device};
use ui::reading::ReadingBlocks;

const BOOK: &str = "Emphasis.epub";

/// Each line's text and the style its drawing starts from, as the store holds
/// them for the page on the glass.
fn lines(device: &Device) -> Vec<(String, FontStyle)> {
    let store = &*device.store;
    (0..store.block_count())
        .map(|i| (store.block_text(i).to_string(), store.block_style(i)))
        .collect()
}

fn open_book(device: &mut Device) {
    device.open_library();
    device.point_at(BOOK);
    device.press_only(Button::Confirm);
    device.run_queued();
    device.settle();
    assert_eq!(device.app.view, AppView::Reading);
}

/// A line that opens in plain text starts its drawing in Regular, though it
/// goes on into emphasis. Taking the first marker anywhere in the line drew
/// the plain opening words in the style of the later run, and a bold run made
/// them wider than the line was measured at.
#[test]
fn a_plain_opening_is_drawn_plain_before_later_emphasis() {
    let card = Card::blank();
    card.put(
        &format!("BOOKS/{BOOK}"),
        &epub_with_body(
            "Emphasis",
            "<p>Plain words <em>stressed</em> after.</p>\
             <p>Plain again <strong>heavy</strong> after.</p>\
             <p><em>Stressed</em> opening, then plain.</p>",
            7,
        ),
    );
    let mut device = Device::wake(&card);
    open_book(&mut device);
    let expected = [
        ("Plain words", FontStyle::Regular),
        ("Plain again", FontStyle::Regular),
        ("Stressed", FontStyle::Italic),
    ];
    let check = |built: Vec<(String, FontStyle)>, when: &str| {
        assert_eq!(built.len(), expected.len(), "{when}: {built:?}");
        for ((text, style), (words, want)) in built.iter().zip(expected) {
            assert!(text.contains(words), "{when}: {built:?}");
            assert_eq!(*style, want, "{when}: {text:?}");
        }
    };
    check(lines(&device), "as built");

    // Read back from the card, not the build's RAM.
    device.sleep();
    let mut device = Device::wake(&card);
    open_book(&mut device);
    check(lines(&device), "loaded from the cache");
}
