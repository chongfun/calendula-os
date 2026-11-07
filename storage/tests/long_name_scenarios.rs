//! Books and folders whose long names do not fit a locator component.
//!
//! A component holds at most `MAX_COMPONENT_BYTES` (128) bytes, and FAT
//! allows long names up to 255 UTF-16 units. Such an entry is addressed by
//! its 8.3 alias: browsing lists it under the alias, the catalog records the
//! same locator, and the resolver finds it again.
//!
//! Tests run the real storage task over a FAT image and the real reducer over
//! its events. See `support`.

mod support;

use app_core::{AppView, Button, LibraryEvent};
use proto::library_path::MAX_COMPONENT_BYTES;
use support::{epub, Card, Device};

/// A name of exactly `bytes` bytes, ending `.epub`.
fn name_of(bytes: usize) -> String {
    let stem = "Tearmoon Empire Short Story Collection ";
    let mut name: String = stem.chars().cycle().take(bytes - 5).collect();
    name.push_str(".epub");
    assert_eq!(name.len(), bytes);
    name
}

/// A folder name of exactly `bytes` bytes.
fn folder_of(bytes: usize) -> String {
    "Side Stories ".chars().cycle().take(bytes).collect()
}

/// The one row that is none of `known`, which is the alias a long-named
/// entry lists under.
fn alias_row(device: &Device, known: &[&str]) -> String {
    let rows = device.rows();
    let others: Vec<&String> = rows
        .iter()
        .filter(|row| !known.contains(&row.as_str()))
        .collect();
    assert_eq!(others.len(), 1, "one alias row in {rows:?}");
    let alias = others[0].clone();
    assert!(
        alias.contains('~') && alias.len() <= 12,
        "an 8.3 alias: {alias:?}"
    );
    alias
}

/// Pick `row` and return the events the pick produced.
fn pick(device: &mut Device, row: &str) -> Vec<LibraryEvent> {
    let before = device.log.len();
    device.choose(row);
    device.log[before..].to_vec()
}

fn rescanned(events: &[LibraryEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, LibraryEvent::Scanned { .. }))
}

/// A book named past the component limit lists under its alias, shows its
/// title once the probe reads it, and opens without the pick rescanning, so
/// browsing and the catalog agree on its address. Its Mac sidecar, longer
/// still, does not list.
#[test]
fn a_long_named_book_lists_under_its_alias_and_opens() {
    let long = name_of(200);
    let card = Card::blank();
    card.put("BOOKS/Good.epub", &epub("Good", 3, 1));
    card.put(&format!("BOOKS/{long}"), &epub("Long Named", 3, 2));
    card.put(&format!("BOOKS/._{long}"), b"AppleDouble sidecar");
    let mut device = Device::wake(&card);
    device.open_library();
    device.settle();

    let alias = alias_row(&device, &["Good.epub"]);
    assert_eq!(
        device.rows().len(),
        2,
        "no sidecar row: {:?}",
        device.rows()
    );
    assert!(
        device.labels().iter().any(|label| label == "Long Named"),
        "the probe titled the alias row: {:?}",
        device.labels()
    );

    let events = pick(&mut device, &alias);
    assert_eq!(device.app.view, AppView::Reading, "{events:?}");
    assert!(!device.app.book_unreadable(), "{events:?}");
    assert!(
        !rescanned(&events),
        "the catalog had the same locator: {events:?}"
    );
    device.press(Button::PageNext);
    assert_eq!(device.app.page, 1);
    device.sleep();

    // The restore finds the book at its saved locator, so it lands before
    // the boot's scan rather than waiting on it.
    let device = Device::wake(&card);
    let restored = device
        .log
        .iter()
        .position(|event| matches!(event, LibraryEvent::Restored { page: 1, .. }));
    let scanned = device
        .log
        .iter()
        .position(|event| matches!(event, LibraryEvent::Scanned { .. }));
    assert!(
        restored.is_some() && restored < scanned,
        "restored ahead of the scan: {:?}",
        device.log
    );
    assert!(!device.app.book_unreadable());
}

/// A folder named past the component limit lists under its alias, and the
/// books inside it list and open by their own names.
#[test]
fn a_long_named_folder_lists_under_its_alias_and_its_books_open() {
    let folder = folder_of(150);
    let card = Card::blank();
    card.put(
        &format!("BOOKS/{folder}/Inside.epub"),
        &epub("Inside", 3, 3),
    );
    let mut device = Device::wake(&card);
    device.open_library();

    let alias = alias_row(&device, &[]);
    device.choose(&alias);
    assert_eq!(device.rows(), ["Inside.epub"]);
    let events = pick(&mut device, "Inside.epub");
    assert_eq!(device.app.view, AppView::Reading, "{events:?}");
    assert!(!device.app.book_unreadable(), "{events:?}");
    assert!(!rescanned(&events), "{events:?}");
}

/// A loose book at the card root goes through the card-root scan rather
/// than the library walk, and follows the same rule.
#[test]
fn a_long_named_book_at_the_card_root_opens() {
    let long = name_of(200);
    let card = Card::blank();
    card.put("BOOKS/Good.epub", &epub("Good", 3, 1));
    card.put(&long, &epub("Loose Long", 3, 4));
    let mut device = Device::wake(&card);
    device.open_library();

    let alias = alias_row(&device, &["Good.epub"]);
    let events = pick(&mut device, &alias);
    assert_eq!(device.app.view, AppView::Reading, "{events:?}");
    assert!(!device.app.book_unreadable(), "{events:?}");
    assert!(!rescanned(&events), "{events:?}");
}

/// The boundary: a name of exactly the component limit keeps its long name,
/// and one byte more lists under the alias.
#[test]
fn a_name_at_the_component_limit_keeps_its_long_name() {
    let fits = name_of(MAX_COMPONENT_BYTES);
    let over = name_of(MAX_COMPONENT_BYTES + 1).replacen('T', "U", 1);
    let card = Card::blank();
    card.put(&format!("BOOKS/{fits}"), &epub("Fits", 3, 5));
    card.put(&format!("BOOKS/{over}"), &epub("Over", 3, 6));
    let mut device = Device::wake(&card);
    device.open_library();

    alias_row(&device, &[fits.as_str()]);
    let events = pick(&mut device, &fits);
    assert_eq!(device.app.view, AppView::Reading, "{events:?}");
    assert!(!device.app.book_unreadable(), "{events:?}");
}
