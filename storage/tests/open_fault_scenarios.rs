//! What the reader is told when a book will not open.
//!
//! Tests run the real storage task over a FAT image and the real reducer over
//! its events. See `support`.

mod support;

use app_core::{AppView, Button, LibraryEvent};
use reader_cache::store::ReaderFault;
use support::{epub, Card, Device};

/// Returns `bytes` as a text conversion leaves them: every byte that is not
/// part of valid UTF-8 becomes U+FFFD (EF BF BD).
fn through_a_text_conversion(bytes: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(bytes).into_owned().into_bytes()
}

/// Returns `bytes` with the end record pointing the central directory at
/// 0xBFEF0000, the offset a real converted book on a card claimed.
fn with_directory_far_past_the_end(bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    let eocd = out
        .windows(4)
        .rposition(|w| w == [0x50, 0x4b, 0x05, 0x06])
        .expect("an end record");
    out[eocd + 16..eocd + 20].copy_from_slice(&0xBFEF_0000u32.to_le_bytes());
    out
}

/// Read one book, then go back to the Library and pick `broken`. Returns
/// the events that pick produced.
fn read_then_pick(card: &Card, broken: &str) -> (Device, Vec<LibraryEvent>) {
    let mut device = Device::wake(card);
    device.open_library();
    device.choose("Good.epub");
    assert_eq!(device.app.view, AppView::Reading);
    while device.app.view != AppView::Library {
        device.press(Button::Back);
    }
    let before = device.log.len();
    device.choose(broken);
    let events = device.log[before..].to_vec();
    (device, events)
}

/// A damaged book keeps the reader on its error page, which names the
/// damage.
#[test]
fn a_damaged_book_says_its_file_is_damaged() {
    for (name, bytes) in [
        (
            "Converted.epub",
            through_a_text_conversion(&epub("Converted", 3, 2)),
        ),
        (
            "Far.epub",
            with_directory_far_past_the_end(&epub("Far", 3, 3)),
        ),
    ] {
        let card = Card::blank();
        card.put("BOOKS/Good.epub", &epub("Good", 3, 1));
        card.put(&format!("BOOKS/{name}"), &bytes);

        let (device, events) = read_then_pick(&card, name);

        let book_id = device.app.book_id;
        assert!(
            events.contains(&LibraryEvent::BookOpenUnreadable { book_id }),
            "{name}: {events:?}"
        );
        assert_eq!(device.app.view, AppView::Reading, "{name}");
        assert!(device.app.book_unreadable(), "{name}");
        assert_eq!(device.store.reader_fault(), ReaderFault::Damaged, "{name}");
    }
}

/// A book that opens fine leaves no fault behind from the one before it.
#[test]
fn opening_a_good_book_clears_the_last_fault() {
    let card = Card::blank();
    card.put("BOOKS/Good.epub", &epub("Good", 3, 1));
    card.put(
        "BOOKS/Converted.epub",
        &through_a_text_conversion(&epub("Converted", 3, 2)),
    );
    let (mut device, _) = read_then_pick(&card, "Converted.epub");
    assert_eq!(device.store.reader_fault(), ReaderFault::Damaged);

    while device.app.view != AppView::Library {
        device.press(Button::Back);
    }
    device.choose("Good.epub");

    assert!(!device.app.book_unreadable());
    assert_eq!(device.store.reader_fault(), ReaderFault::Unknown);
}

/// A card read refused anywhere in a first open, while reading the row's
/// catalog record, finding the book's folder or reading the book, is a card
/// fault. The book is on the card the whole time, so no message tells the
/// user it is missing or damaged.
#[test]
fn a_refused_read_while_opening_a_book_is_a_card_fault() {
    let mut unreadable = 0usize;
    for probe in 0..4_000 {
        let card = Card::blank();
        card.put("BOOKS/Shelf/Good.epub", &epub("Good", 3, 1));
        let mut device = Device::wake(&card);
        device.open_library();
        device.choose("Shelf");
        device.point_at("Good.epub");
        card.disk.refuse_read_in(Some(probe));
        device.press_only(Button::Confirm);
        device.run_queued();
        let fired = !card.disk.read_refusal_armed();
        card.disk.refuse_read_in(None);
        if !fired {
            break;
        }
        if device.app.book_unreadable() {
            unreadable += 1;
            assert_eq!(
                device.store.reader_fault(),
                ReaderFault::Card,
                "probe {probe}: {:?}",
                device.log
            );
        }
    }
    assert!(unreadable > 20, "the sweep reached the open: {unreadable}");
}
