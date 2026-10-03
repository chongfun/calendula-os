//! Storage while the Wi-Fi session holds the reader's memory.
//!
//! The loan is one way: the EPUB scratch goes to the radio and only the
//! session's reset brings it back. Until then storage refuses anything that
//! needs it. The reader can still leave the Wireless screen while a join is
//! running, so whatever the app is left waiting on has to be told no.

mod support;

use app_core::{AppView, Button, LibraryEvent};
use support::{epub, Card, Device};

fn shelf() -> Card {
    let card = Card::blank();
    card.put("BOOKS/Shelf/Alpha.epub", &epub("Alpha", 2, 1));
    card.put("BOOKS/Shelf/Beta.epub", &epub("Beta", 2, 2));
    card
}

/// A Library pick under the loan is refused out loud, and the Library is
/// usable again rather than holding its rail for a minute.
#[test]
fn a_pick_during_the_loan_is_refused_not_dropped() {
    let card = shelf();
    let mut device = Device::wake(&card);
    device.open_library();
    device.sync.loan_granted();
    let before = device.log.len();
    device.point_at("Shelf");
    device.press(Button::Confirm);
    assert!(
        device.log[before..]
            .iter()
            .any(|event| matches!(event, LibraryEvent::RowFailed { .. })),
        "{:?}",
        &device.log[before..]
    );
    assert!(device.app.library_browse.is_idle(), "the wait is over");
    assert_eq!(device.app.view, AppView::Library);
}

/// Continuing a book from Home sends an open. Under the loan it is answered
/// as failed, which in the firmware clears the open gate and rolls the app
/// back to Home (`fw/src/tasks/app.rs`, not modeled here), instead of
/// leaving "opening" on screen with input held.
#[test]
fn an_open_during_the_loan_is_answered_as_failed() {
    let card = shelf();
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Shelf");
    device.choose("Alpha.epub");
    assert_eq!(device.app.view, AppView::Reading);
    // Portrait: the first Back shows the key sheet, the second acts on it.
    device.press(Button::Back);
    device.press(Button::Back);
    assert_eq!(device.app.view, AppView::Home);

    device.sync.loan_granted();
    let before = device.log.len();
    device.press(Button::Confirm);
    let book_id = device.app.book_id;
    let events = &device.log[before..];
    assert!(
        events.contains(&LibraryEvent::BookOpenFailed { book_id }),
        "{events:?}"
    );
}
