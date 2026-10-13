//! The global state record a settings change saves, and what it may write
//! beside it.

mod support;

use app_core::{AppView, ReaderSource, StorageCommand};
use reader_cache::files::read_position_file;
use support::{epub, Card, Device};

/// A boot that restored nothing still names a book: the scan defaults the
/// reader to the first catalog row, which nobody has opened. A settings
/// change saves the state then, with that book at page 0. The settings have
/// to land, but the book's own position on the card is not the app's to
/// overwrite: its page 0 is a default, and the card's page is the reader's.
#[test]
fn a_settings_save_before_any_open_leaves_the_books_position_alone() {
    let card = Card::blank();
    card.put("BOOKS/Shelf/Alpha.epub", &epub("Alpha", 6, 1));
    card.put("BOOKS/Shelf/Beta.epub", &epub("Beta", 6, 2));

    // Read the first catalog row to page 9 and sleep, so the card holds its
    // position. Then lose the state file, as a card from a firmware that kept
    // none, or a fresh one, does.
    let mut device = Device::wake(&card);
    let name = device
        .store
        .catalog_entry(0)
        .expect("a first row")
        .display_name
        .rsplit('/')
        .next()
        .expect("a file name")
        .to_string();
    device.open_library();
    device.choose("Shelf");
    device.choose(&name);
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    assert_eq!(device.app.book_id, ReaderSource::sd(0).book_id());
    device.turn(9);
    let loaded = device.store.loaded_book_snapshot().expect("loaded");
    let (root, locator) = (loaded.root, loaded.path.to_string());
    let key = proto::cache::cache_key_from(loaded.identity.0);
    device.sleep();
    // Written as A/B generations, with a legacy single file read as well.
    for name in ["STATEA.BIN", "STATEB.BIN", "STATE.BIN"] {
        let path = format!("READER/{name}");
        if card.read(&path).is_some() {
            card.delete(&path);
        }
    }
    let owner = proto::cache::CacheOwner {
        key: key.as_str(),
        root,
        locator: &locator,
    };
    let position = || card.session(|root| read_position_file(root, &owner));
    assert_eq!(position().map(|(_, page)| page), Some(9));

    // Nothing restored, so the reader is on the scan's default.
    let mut device = Device::wake(&card);
    assert_eq!(device.app.book_id, ReaderSource::sd(0).book_id());
    assert!(!device.store.holds_book(device.app.book_id));

    // What a settings change dispatches.
    let record = device.app.persisted();
    device.send(StorageCommand::StoreProgress(record));
    device.settle();
    device.sleep();

    assert_eq!(
        position().map(|(_, page)| page),
        Some(9),
        "the book's position is the reader's"
    );
    assert!(
        ["STATEA.BIN", "STATEB.BIN"]
            .iter()
            .any(|name| card.read(&format!("READER/{name}")).is_some()),
        "the settings still landed"
    );
}
