//! Moving through the library's folders when the reader does not wait for
//! the answer. Back while a move is out takes the reader Home, but the
//! storage task still carries the move out, so the app has to end up in the
//! folder storage is in.

mod support;

use app_core::{AppView, Button};
use support::{epub, Card, Device};

fn shelf() -> Card {
    let card = Card::blank();
    card.put("BOOKS/Shelf/Alpha.epub", &epub("Alpha", 2, 1));
    card.put("BOOKS/Shelf/Beta.epub", &epub("Beta", 2, 2));
    card.put("BOOKS/Shelf/Gamma.epub", &epub("Gamma", 2, 3));
    card
}

/// The app and the storage task agree on where browsing is.
fn assert_in_step(device: &Device) {
    assert_eq!(
        device.app.library_depth == 0,
        device.store.browse().is_root(),
        "app depth {} against storage at {:?}",
        device.app.library_depth,
        device.store.browse().path()
    );
    assert_eq!(device.app.library_count, device.store.browse().count());
}

/// Back, and Back again before the Leave is answered. Storage leaves the
/// folder; the Library reopens at the root with it, and Back there goes Home
/// instead of asking storage to leave a folder it is not in.
#[test]
fn a_leave_walked_away_from_leaves_the_app_at_the_root_too() {
    let card = shelf();
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Shelf");
    assert_eq!(device.app.library_depth, 1);

    device.press_only(Button::Back);
    device.press_only(Button::Back);
    assert_eq!(device.app.view, AppView::Home);
    device.settle();

    device.open_library();
    assert_in_step(&device);
    device.press(Button::Back);
    assert_eq!(device.app.view, AppView::Home, "{:?}", device.log);
}

/// Confirm on the folder, then Back before the Enter is answered. Storage
/// goes into the folder; the Library reopens there with its rows, and Back
/// leaves it.
#[test]
fn an_enter_walked_away_from_leaves_the_app_in_the_folder_too() {
    let card = shelf();
    let mut device = Device::wake(&card);
    device.open_library();
    device.point_at("Shelf");
    device.press_only(Button::Confirm);
    device.press_only(Button::Back);
    assert_eq!(device.app.view, AppView::Home);
    device.settle();

    device.open_library();
    assert_in_step(&device);
    assert_eq!(device.app.library_depth, 1);
    device.press(Button::Back);
    assert_eq!(device.app.view, AppView::Library, "{:?}", device.log);
    assert_eq!(device.app.library_depth, 0);
    assert_in_step(&device);
}
